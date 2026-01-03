use iced::widget::{
    button, checkbox, column, container, progress_bar, row, scrollable, slider, table, text,
    text_input,
};
use iced::{Element, Length, Theme};
use serde::Deserialize;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
enum Message {
    InputChanged(String),
    SliderChanged(i32),
    Submit,
    SelectArp(String, bool),
    RefreshArp,
    Tick,
}

#[derive(Debug, Clone)]
struct ArpNeighbor {
    ip: String,
    mac: String,
    kind: String,
    label: Option<String>,
}

#[derive(Debug, Clone)]
enum ArpScanResult {
    Ok(Vec<ArpNeighbor>),
    Err(String),
}

#[derive(Debug, Clone)]
struct ArpRow {
    id: usize,
    ip_text: String,
    mac: String,
    kind: String,
    key: String,
}

struct WebGui {
    esp_counter: Arc<Mutex<i32>>,
    last_esp_ip: Arc<Mutex<Option<IpAddr>>>,
    esp_last_seen: Arc<Mutex<Option<std::time::Instant>>>,
    mqtt_set_tx: Arc<Mutex<Option<mpsc::UnboundedSender<i32>>>>,
    edit_text: String,
    edit_value: i32,
    local_macs: Vec<String>,
    arp_neighbors: Vec<ArpNeighbor>,
    selected_arp_key: Option<String>,
    arp_in_flight: bool,
    arp_refresh_anim: f32,
    arp_tx: std::sync::mpsc::Sender<ArpScanResult>,
    arp_rx: std::sync::mpsc::Receiver<ArpScanResult>,
}

fn new() -> WebGui {
    let local_macs = get_local_mac_addresses();
    let (arp_tx, arp_rx) = std::sync::mpsc::channel::<ArpScanResult>();
    let mut state = WebGui {
        esp_counter: Arc::new(Mutex::new(0)),
        last_esp_ip: Arc::new(Mutex::new(None)),
        esp_last_seen: Arc::new(Mutex::new(None)),
        mqtt_set_tx: Arc::new(Mutex::new(None)),
        edit_text: "0".to_string(),
        edit_value: 0,
        local_macs,
        arp_neighbors: Vec::new(),
        selected_arp_key: None,
        arp_in_flight: false,
        arp_refresh_anim: 0.0,
        arp_tx,
        arp_rx,
    };

    // Start the server automatically on launch.
    start_server(&mut state);

    // Single ARP scan at startup (no periodic polling).
    state.arp_in_flight = true;
    let tx = state.arp_tx.clone();
    std::thread::spawn(move || {
        let result = match scan_arp_neighbors_windows(None) {
            Ok(list) => ArpScanResult::Ok(list),
            Err(e) => ArpScanResult::Err(e),
        };
        let _ = tx.send(result);
    });

    state
}

fn parse_arp_key_to_ip(key: &str) -> Option<IpAddr> {
    let (ip, _mac) = key.split_once('|')?;
    ip.parse::<IpAddr>().ok()
}

fn parse_arp_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('|')
}

fn get_local_ipv4() -> Option<std::net::Ipv4Addr> {
    // Uses the default route selection to infer the local IP.
    let sock = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    let _ = sock.connect((std::net::Ipv4Addr::new(8, 8, 8, 8), 80));
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

fn udp_unicast_announce_to_esp(ip: IpAddr, reconnect: bool) {
    // Send a discovery announce directly to the ESP. The ESP uses the UDP packet source
    // address as the web_gui IP to connect to the embedded MQTT broker.
    // Also populates Windows ARP cache for the ESP IP.
    std::thread::spawn(move || {
        const DISCOVERY_PORT: u16 = 53530;
        let local_ip = get_local_ipv4().map(|v| v.to_string()).unwrap_or_else(|| "".to_string());
        let announce = if reconnect {
            format!(
                "{{\"service\":\"web_gui\",\"mqtt_port\":1883,\"ip\":\"{}\",\"reconnect\":true}}",
                local_ip
            )
        } else {
            format!(
                "{{\"service\":\"web_gui\",\"mqtt_port\":1883,\"ip\":\"{}\"}}",
                local_ip
            )
        };

        let sock = match ip {
            IpAddr::V4(_) => std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)),
            IpAddr::V6(_) => std::net::UdpSocket::bind((std::net::Ipv6Addr::UNSPECIFIED, 0)),
        };

        let Ok(sock) = sock else {
            return;
        };

        let _ = sock.send_to(announce.as_bytes(), (ip, DISCOVERY_PORT));
        println!("web_gui: discovery unicast sent to {ip}:{DISCOVERY_PORT}");
    });
}

fn get_local_mac_addresses() -> Vec<String> {
    // `mac_address` primarily exposes the MAC address of the default interface.
    // Router DHCP lease lists generally require router-specific APIs; this is the
    // most reliable thing we can show from the desktop app without credentials.
    match mac_address::get_mac_address() {
        Ok(Some(mac)) => vec![mac.to_string()],
        Ok(None) => vec!["(unknown)".to_string()],
        Err(e) => vec![format!("(error: {e})")],
    }
}

#[cfg(windows)]
fn scan_arp_neighbors_windows(esp_ip: Option<IpAddr>) -> Result<Vec<ArpNeighbor>, String> {
    let output = std::process::Command::new("arp")
        .arg("-a")
        .output()
        .map_err(|e| format!("failed to run arp -a: {e}"))?;

    if !output.status.success() {
        return Err(format!("arp -a exited with status {}", output.status));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut out: Vec<ArpNeighbor> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Typical line:
        //  192.168.0.1           01-23-45-67-89-ab     dynamic
        if !line.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let ip = parts[0];
        let mac = parts[1];
        let kind = parts[2];

        if !mac.contains('-') && !mac.contains(':') {
            continue;
        }

        let mac = mac.replace('-', ":").to_ascii_lowercase();
        let label = esp_ip
            .as_ref()
            .and_then(|x| if x.to_string() == ip { Some("esp32tft".to_string()) } else { None });

        out.push(ArpNeighbor {
            ip: ip.to_string(),
            mac,
            kind: kind.to_string(),
            label,
        });
    }

    Ok(out)
}

#[cfg(not(windows))]
fn scan_arp_neighbors_windows(_esp_ip: Option<IpAddr>) -> Result<Vec<ArpNeighbor>, String> {
    Err("ARP scan is currently implemented for Windows only".to_string())
}

fn start_server(state: &mut WebGui) {
    let counter = Arc::clone(&state.esp_counter);
    let esp_last_seen = Arc::clone(&state.esp_last_seen);
    let last_esp_ip = Arc::clone(&state.last_esp_ip);
    let mqtt_set_tx_slot = Arc::clone(&state.mqtt_set_tx);
    let (mqtt_set_tx, mut mqtt_set_rx) = mpsc::unbounded_channel::<i32>();
    *mqtt_set_tx_slot.lock().unwrap() = Some(mqtt_set_tx);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use rumqttd::{Broker, Config, ConnectionSettings, Notification, RouterConfig, ServerSettings};
            use std::collections::HashMap;

            #[derive(Debug, Deserialize)]
            struct DiscoveryMsg {
                #[serde(default)]
                service: Option<String>,
            }

            // rumqttd logs via `tracing`, so enable a subscriber (once) to surface
            // bind errors and connect/disconnect events.
            let env_filter = if std::env::var("RUST_LOG").is_ok() {
                tracing_subscriber::EnvFilter::from_default_env()
            } else {
                // Keep rumqttd logs readable by default. Set RUST_LOG=rumqttd=debug to deep-dive.
                tracing_subscriber::EnvFilter::new("rumqttd=info")
            };
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(true)
                .compact()
                .try_init();

            println!("web_gui: starting MQTT broker on mqtt://0.0.0.0:1883");
            println!("web_gui: starting UDP discovery on :53530");

            const TOPIC_ESP_COUNTER: &str = "speed/counter/esp";
            const TOPIC_SET_COUNTER: &str = "speed/counter/set";

            // Embedded MQTT broker (rumqttd). The web GUI uses a local link to subscribe
            // and publish without needing a separate MQTT client.
            let counter_mqtt = Arc::clone(&counter);
            let esp_last_seen_mqtt = Arc::clone(&esp_last_seen);
            let mqtt_task = async move {
                // Minimal single-node broker config with a single MQTT v5 listener on 1883.
                let mut config = Config {
                    id: 0,
                    router: RouterConfig {
                        max_connections: 16,
                        // These defaults can end up as 0 with our minimal config and cause
                        // panics inside rumqttd's router log/segment initialization.
                        // Keep them explicit and sane.
                        max_outgoing_packet_count: 1024,
                        // rumqttd requires a non-zero segment size (>= 1KB). Leaving this
                        // at 0 causes a panic in the router thread.
                        max_segment_size: 1024 * 1024,
                        // rumqttd also expects at least one segment to exist.
                        max_segment_count: 4,
                        ..Default::default()
                    },
                    ..Default::default()
                };

                let mut v5 = HashMap::new();
                v5.insert(
                    "default".to_string(),
                    ServerSettings {
                        name: "v5".to_string(),
                        listen: (std::net::Ipv4Addr::UNSPECIFIED, 1883).into(),
                        tls: None,
                        next_connection_delay_ms: 0,
                        connections: ConnectionSettings {
                            connection_timeout_ms: 30_000,
                            max_payload_size: 64 * 1024,
                            max_inflight_count: 32,
                            auth: None,
                            external_auth: None,
                            dynamic_filters: true,
                        },
                    },
                );
                config.v5 = Some(v5);
                // The ESP no_std client only supports MQTT v5.
                config.v4 = None;

                // Pre-flight: check if TCP :1883 is available.
                // This avoids the previous "connect-to-self" probe, which caused rumqttd to log
                // an error because the probe closes without sending a MQTT CONNECT packet.
                match std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 1883)) {
                    Ok(listener) => drop(listener),
                    Err(e) => {
                        eprintln!("web_gui: cannot bind 0.0.0.0:1883 (already in use / blocked): {e}");
                        return;
                    }
                }

                // IMPORTANT: `rumqttd::Broker::start()` blocks forever (it joins server threads).
                // Run it on its own OS thread so we can keep servicing the local link.
                let mut broker = Broker::new(config);

                let (mut link_tx, mut link_rx) = match broker.link("web_gui") {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("web_gui: mqtt broker link failed: {e}");
                        return;
                    }
                };

                std::thread::spawn(move || {
                    if let Err(e) = broker.start() {
                        eprintln!("web_gui: mqtt broker start failed: {e}");
                    }
                });

                println!("web_gui: mqtt broker thread started (expect 'Listening for remote connections' from rumqttd)");

                if let Err(e) = link_tx.try_subscribe(TOPIC_ESP_COUNTER) {
                    eprintln!("web_gui: mqtt subscribe failed: {e}");
                }

                loop {
                    tokio::select! {
                        maybe_set = mqtt_set_rx.recv() => {
                            let Some(value) = maybe_set else { break; };
                            let value = value.clamp(0, 100);
                            let payload = value.to_string();
                            if let Err(e) = link_tx.try_publish(TOPIC_SET_COUNTER, payload) {
                                eprintln!("web_gui: mqtt publish(set) failed: {e}");
                            } else {
                                println!("web_gui: submitted counter={value} via MQTT topic {TOPIC_SET_COUNTER}");
                            }
                        }
                        notif = link_rx.next() => {
                            match notif {
                                Ok(Some(Notification::Forward(forward))) => {
                                    let topic = std::str::from_utf8(forward.publish.topic.as_ref()).unwrap_or("");
                                    if topic == TOPIC_ESP_COUNTER {
                                        let payload = std::str::from_utf8(forward.publish.payload.as_ref()).unwrap_or("");
                                        if let Ok(v) = payload.trim().parse::<i32>() {
                                            let v = v.clamp(0, 100);
                                            *counter_mqtt.lock().unwrap() = v;
                                            *esp_last_seen_mqtt.lock().unwrap() = Some(std::time::Instant::now());
                                            println!("web_gui: mqtt esp counter={v}");
                                        }
                                    }
                                }
                                Ok(Some(other)) => {
                                    // Useful for diagnosing connect/disconnect issues.
                                    println!("web_gui: mqtt notif={other:?}");
                                }
                                Ok(None) => {
                                    // Router asked us to unschedule; keep going.
                                }
                                Err(e) => {
                                    eprintln!("web_gui: mqtt link error: {e}");
                                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                                }
                            }
                        }
                    }
                }
            };

            // UDP discovery/announce (LAN broadcast). This avoids hardcoded IPs.
            // - We announce ourselves as {service:"web_gui", mqtt_port:1883}
            // - We listen for {service:"esp32tft", http_port:8080} and record sender IP
            let last_esp_ip_udp = Arc::clone(&last_esp_ip);
            let udp_task = async move {
                const DISCOVERY_PORT: u16 = 53530;
                let socket = match tokio::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("web_gui: UDP bind failed on {DISCOVERY_PORT}: {e}");
                        return;
                    }
                };
                if let Err(e) = socket.set_broadcast(true) {
                    eprintln!("web_gui: UDP set_broadcast failed: {e}");
                }

                let local_ip = get_local_ipv4().map(|v| v.to_string()).unwrap_or_else(|| "".to_string());
                let announce = format!(
                    "{{\"service\":\"web_gui\",\"mqtt_port\":1883,\"ip\":\"{}\"}}",
                    local_ip
                );
                let broadcast_ip = std::env::var("SPEED_DISCOVERY_BROADCAST")
                    .ok()
                    .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
                    .unwrap_or(std::net::Ipv4Addr::BROADCAST);
                let broadcast_addr = (broadcast_ip, DISCOVERY_PORT);
                let esp_unicast = std::env::var("SPEED_ESP_HOST")
                    .ok()
                    .and_then(|s| s.parse::<std::net::IpAddr>().ok())
                    .map(|ip| (ip, DISCOVERY_PORT));
                let mut buf = [0u8; 512];
                let mut last_logged: Option<std::net::IpAddr> = None;
                let mut last_seen_esp: Option<(std::net::IpAddr, tokio::time::Instant)> = None;

                // Announce at a modest cadence to reduce unnecessary LAN chatter.
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            // announce
                            let _ = socket.send_to(announce.as_bytes(), broadcast_addr).await;
                            if let Some(addr) = esp_unicast {
                                let _ = socket.send_to(announce.as_bytes(), addr).await;
                            }

                            // If we've seen an ESP, refresh it occasionally (helps some flaky broadcast cases)
                            // but keep this very low frequency.
                            if let Some((ip, when)) = last_seen_esp {
                                if when.elapsed() > std::time::Duration::from_secs(30) {
                                    let _ = socket.send_to(announce.as_bytes(), (ip, DISCOVERY_PORT)).await;
                                    last_seen_esp = Some((ip, tokio::time::Instant::now()));
                                }
                            }
                        }

                        recv = socket.recv_from(&mut buf) => {
                            let Ok((n, from)) = recv else { continue; };
                            let Ok(msg) = serde_json::from_slice::<DiscoveryMsg>(&buf[..n]) else { continue; };
                            if msg.service.as_deref() != Some("esp32tft") { continue; }

                            let ip = from.ip();
                            *last_esp_ip_udp.lock().unwrap() = Some(ip);
                            last_seen_esp = Some((ip, tokio::time::Instant::now()));
                            if last_logged != Some(ip) {
                                last_logged = Some(ip);
                                println!("web_gui: discovered esp32tft at {ip}");

                                // Send a unicast announce back to the ESP so it learns our desktop
                                // IP (source address) even when broadcasts are flaky. Also
                                // populates Windows ARP cache for the ESP IP.
                                let _ = socket.send_to(announce.as_bytes(), (ip, DISCOVERY_PORT)).await;
                            }
                        }
                    }
                }
            };

            // Run broker + discovery forever.
            tokio::spawn(udp_task);
            tokio::spawn(mqtt_task);
            futures_util::future::pending::<()>().await;
        });
    });
}

fn update(state: &mut WebGui, message: Message) {
    match message {
        Message::InputChanged(raw) => {
            let filtered: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
            if filtered.is_empty() {
                state.edit_value = 0;
                state.edit_text.clear();
            } else {
                let parsed = filtered.parse::<i32>().unwrap_or(0);
                state.edit_value = parsed.clamp(0, 100);
                state.edit_text = state.edit_value.to_string();
            }
        }
        Message::SliderChanged(v) => {
            state.edit_value = v.clamp(0, 100);
            state.edit_text = state.edit_value.to_string();
        }
        Message::Submit => {
            // MQTT only.
            if let Some(tx) = state.mqtt_set_tx.lock().unwrap().clone() {
                let value = state.edit_value.clamp(0, 100);
                if tx.send(value).is_ok() {
                    println!("web_gui: submitted counter={value} via MQTT topic speed/counter/set");
                }
            } else {
                eprintln!("web_gui: mqtt broker not ready yet");
            }
        }
        Message::SelectArp(key, selected) => {
            if selected {
                state.selected_arp_key = Some(key.clone());

                if let Some(ip) = parse_arp_key_to_ip(&key) {
                    *state.last_esp_ip.lock().unwrap() = Some(ip);
                    // Manual connect: send unicast discovery to chosen IP so the ESP learns
                    // our desktop IP and connects to the MQTT broker.
                    udp_unicast_announce_to_esp(ip, true);
                }
            } else if state.selected_arp_key.as_deref() == Some(&key) {
                state.selected_arp_key = None;
            }
        }
        Message::RefreshArp => {
            // Force an immediate ARP rescan.
            // Note: on Windows, devices typically only show up after the ARP cache has an entry.
            if state.arp_in_flight {
                return;
            }

            let esp_ip = *state.last_esp_ip.lock().unwrap();
            if let Some(ip) = esp_ip {
                // Generate a packet to populate ARP cache.
                udp_unicast_announce_to_esp(ip, false);
            }

            state.arp_in_flight = true;
            state.arp_refresh_anim = 0.0;

            let tx = state.arp_tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(350));
                let result = match scan_arp_neighbors_windows(esp_ip) {
                    Ok(list) => ArpScanResult::Ok(list),
                    Err(e) => ArpScanResult::Err(e),
                };
                let _ = tx.send(result);
            });
        }
        Message::Tick => {
            if state.arp_in_flight {
                // Simple indeterminate animation for the Refresh UI.
                state.arp_refresh_anim = (state.arp_refresh_anim + 0.06) % 1.0;
            }

            while let Ok(result) = state.arp_rx.try_recv() {
                state.arp_in_flight = false;
                state.arp_refresh_anim = 0.0;
                match result {
                    ArpScanResult::Ok(list) => {
                        state.arp_neighbors = list;

                        if let Some(sel) = state.selected_arp_key.as_ref() {
                            let still_present = state.arp_neighbors.iter().any(|n| {
                                let k = format!("{}|{}", n.ip, n.mac);
                                &k == sel
                            });
                            if !still_present {
                                state.selected_arp_key = None;
                            }
                        }
                    }
                    ArpScanResult::Err(e) => {
                        state.arp_neighbors = vec![ArpNeighbor {
                            ip: "(error)".to_string(),
                            mac: e,
                            kind: "".to_string(),
                            label: None,
                        }];
                    }
                }
            }
        }
    }
}

fn view(state: &WebGui) -> Element<'_, Message> {
    let esp_counter_text = text(format!("Counter (from slint_tft): {}", *state.esp_counter.lock().unwrap()));
    let edit_label = text("Set counter (0-100):");
    let slider = slider(0..=100, state.edit_value, Message::SliderChanged)
        .width(Length::FillPortion(2));
    let input = text_input("0-100", &state.edit_text)
        .on_input(Message::InputChanged)
        .width(Length::Fixed(90.0));
    let controls = row![slider, input, button("Push").on_press(Message::Submit)]
        .spacing(10)
        .align_y(iced::Alignment::Center);

    let mqtt_alive = state
        .esp_last_seen
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|t| t.elapsed() < Duration::from_secs(25));

    // Single-row "summary" table: local host <-> selected MQTT device.
    #[derive(Debug, Clone)]
    struct NetSummaryRow {
        local_ip: String,
        local_mac: String,
        device_ip: String,
        device_mac: String,
    }

    let local_ip = get_local_ipv4().map(|ip| ip.to_string()).unwrap_or("(unknown)".to_string());
    let local_mac = state
        .local_macs
        .first()
        .cloned()
        .unwrap_or_else(|| "(unknown)".to_string());

    let (device_ip, device_mac) = state
        .selected_arp_key
        .as_deref()
        .and_then(parse_arp_key)
        .map(|(ip, mac)| (ip.to_string(), mac.to_string()))
        .or_else(|| {
            let ip = (*state.last_esp_ip.lock().unwrap())?.to_string();
            let mac = state
                .arp_neighbors
                .iter()
                .find(|n| n.ip == ip)
                .map(|n| n.mac.clone())
                .unwrap_or_else(|| "(unknown)".to_string());
            Some((ip, mac))
        })
        .unwrap_or_else(|| ("(none)".to_string(), "(none)".to_string()));

    let summary_rows = vec![NetSummaryRow {
        local_ip,
        local_mac,
        device_ip,
        device_mac,
    }];

    let summary_columns = vec![
        table::column(text("Local IP"), |r: NetSummaryRow| text(r.local_ip))
            .width(Length::FillPortion(2)),
        table::column(text("Local MAC"), |r: NetSummaryRow| text(r.local_mac))
            .width(Length::FillPortion(3)),
        table::column(text("MQTT Device IP"), |r: NetSummaryRow| text(r.device_ip))
            .width(Length::FillPortion(2)),
        table::column(text("MQTT Device MAC"), |r: NetSummaryRow| text(r.device_mac))
            .width(Length::FillPortion(3)),
    ];

    let summary_table = table::table(summary_columns, summary_rows)
        .width(Length::Fill)
        .padding(4)
        .separator(1);
    let summary_table = container(summary_table)
        .padding(6)
        .width(Length::Fill)
        .style(if mqtt_alive {
            container::success
        } else {
            container::danger
        });

    let refresh_button = if state.arp_in_flight {
        button(text("Refreshing..."))
    } else {
        button(text("Refresh")).on_press(Message::RefreshArp)
    };

    let refresh_indicator = if state.arp_in_flight {
        container(progress_bar(0.0..=1.0, state.arp_refresh_anim)).width(Length::Fixed(140.0))
    } else {
        container(progress_bar(0.0..=1.0, 0.0))
            .width(Length::Fixed(140.0))
            .height(Length::Fixed(0.0))
    };

    let arp_header = row![text("LAN neighbors (ARP cache):"), refresh_button, refresh_indicator]
        .spacing(10)
        .align_y(iced::Alignment::Center);

    let selected_key = state.selected_arp_key.clone();
    let rows: Vec<ArpRow> = state
        .arp_neighbors
        .iter()
        .enumerate()
        .map(|(idx, n)| {
            let mut ip_text = n.ip.clone();
            if let Some(label) = &n.label {
                ip_text.push_str(" (");
                ip_text.push_str(label);
                ip_text.push(')');
            }

            let key = format!("{}|{}", n.ip, n.mac);
            ArpRow {
                id: idx + 1,
                ip_text,
                mac: n.mac.clone(),
                kind: n.kind.clone(),
                key,
            }
        })
        .collect();

    let columns = vec![
        table::column(text(""), move |r: ArpRow| {
            let checked = selected_key.as_deref() == Some(r.key.as_str());
            let key = r.key.clone();
            checkbox(checked).on_toggle(move |v| Message::SelectArp(key.clone(), v))
        })
        .width(Length::Fixed(36.0)),
        table::column(text("ID"), |r: ArpRow| text(r.id.to_string())).width(Length::Fixed(48.0)),
        table::column(text("IP"), |r: ArpRow| text(r.ip_text)).width(Length::FillPortion(2)),
        table::column(text("MAC"), |r: ArpRow| text(r.mac)).width(Length::FillPortion(2)),
        table::column(text("Type"), |r: ArpRow| text(r.kind)).width(Length::Fixed(90.0)),
    ];

    let arp_table = table::table(columns, rows)
        .width(Length::Fill)
        .padding(4)
        .separator(1);

    // Show about 5 rows, then allow scrolling.
    let arp_table = scrollable(arp_table).height(Length::Fixed(8.0 * 24.0));
    let arp_table = container(arp_table)
        .padding(6)
        .width(Length::Fill)
        .style(container::bordered_box);

    column![
        esp_counter_text,
        edit_label,
        controls,
        summary_table,
        arp_header,
        arp_table
    ]
    .into()
}

fn main() -> iced::Result {
    iced::application(new, update, view)
        .title("Web GUI")
        .theme(Theme::Dark)
        .subscription(|_| {
            iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::Tick)
        })
        .window(iced::window::Settings {
            size: iced::Size::new(1024.0 / 5.0, 768.0 / 5.0),
            ..Default::default()
        })
        .run()
}