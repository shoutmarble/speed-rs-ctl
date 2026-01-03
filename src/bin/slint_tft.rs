#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::priority_channel::{Max, PriorityChannel, Receiver as PrioReceiver, Sender as PrioSender};
use embassy_sync::signal::Signal;
use embassy_sync::watch::{DynReceiver as WatchDynReceiver, DynSender as WatchDynSender, Watch};
use embassy_time::{Duration as EmbassyDuration, Timer, Ticker};
use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::prelude::*;
use embedded_hal::digital::OutputPin;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::efuse::Efuse;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::{Instant, Rate};
use esp_hal::timer::timg::TimerGroup;
use log::info;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation, Rotation};
use mipidsi::Builder;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use speed::wifi::{WifiConnState, WifiUiState, WIFI_SSIDS};
use static_cell::StaticCell;
use embassy_net::{Config, Stack, StackResources};
use embassy_net::tcp::{Error as TcpError, TcpSocket};
use embassy_net::udp::UdpSocket;
use embedded_io_async::Write;
use serde::Deserialize;

use rust_mqtt::buffer::AllocBuffer;
use rust_mqtt::client::event::Event as MqttEvent;
use rust_mqtt::client::options::{ConnectOptions, PublicationOptions, RetainHandling, SubscriptionOptions};
use rust_mqtt::client::Client as MqttClient;
use rust_mqtt::config::{KeepAlive, SessionExpiryInterval};
use rust_mqtt::types::{MqttString, QoS, TopicFilter, TopicName};
use rust_mqtt::Bytes as MqttBytes;

esp_bootloader_esp_idf::esp_app_desc!();

slint::include_modules!();

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum UiEvent {
    Redraw,
    Frame,
    Housekeeping,
}

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum UiEventPrio {
    Low = 0,
    Normal = 1,
    High = 2,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct UiEventItem {
    prio: UiEventPrio,
    seq: u32,
    event: UiEvent,
}

static UI_EVENT_SEQ: AtomicU32 = AtomicU32::new(0);

static HOUSEKEEPING_SENT: AtomicU32 = AtomicU32::new(0);
static HOUSEKEEPING_DROPPED: AtomicU32 = AtomicU32::new(0);
static HOUSEKEEPING_HANDLED: AtomicU32 = AtomicU32::new(0);

impl UiEventItem {
    fn new(prio: UiEventPrio, event: UiEvent) -> Self {
        let seq = UI_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
        Self { prio, seq, event }
    }
}

// High-priority discrete events (input, redraw requests, etc.).
// Max-heap: larger (prio, seq, ..) is received first.
static UI_EVENTS_HI: PriorityChannel<CriticalSectionRawMutex, UiEventItem, Max, 4> = PriorityChannel::new();

// Latest counter value as state (not a queue). This ensures the UI always sees the newest value.
// We need two receivers: one for the UI loop and one for the network post task.
static COUNTER_VALUE: Watch<CriticalSectionRawMutex, i32, 2> = Watch::new_with(0);

// External counter override requests (e.g. from web_gui). The counter task applies these
// to its internal state so the up/down loop continues from the new value.
static COUNTER_OVERRIDE: Signal<CriticalSectionRawMutex, i32> = Signal::new();

// Another piece of shared UI state: a simple heartbeat counter.
static HEARTBEAT_VALUE: Watch<CriticalSectionRawMutex, i32, 1> = Watch::new_with(0);

// Enable/disable frame pacing when Slint has active animations.
static FRAME_ENABLE: Watch<CriticalSectionRawMutex, bool, 1> = Watch::new_with(false);

// Wake the UI loop without queueing an event (coalesces automatically).
static UI_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// Wi-Fi connection status for the UI.
static WIFI_UI: Watch<CriticalSectionRawMutex, WifiUiState, 1> = Watch::new_with(WifiUiState::disconnected());

// Discovered desktop web_gui endpoint (IPv4) via UDP broadcast.
static WEB_GUI_IPV4: Watch<CriticalSectionRawMutex, Option<embassy_net::Ipv4Address>, 1> = Watch::new_with(None);
// Discovered desktop MQTT port (typically 1883).
static WEB_GUI_MQTT_PORT: Watch<CriticalSectionRawMutex, Option<u16>, 1> = Watch::new_with(None);

// Request that the MQTT client tears down and reconnects.
static MQTT_RECONNECT: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// Our own network identity (for diagnostics/UI).
static ESP_IPV4: Watch<CriticalSectionRawMutex, Option<embassy_net::Ipv4Address>, 1> = Watch::new_with(None);
static ESP_MAC: Watch<CriticalSectionRawMutex, Option<[u8; 6]>, 1> = Watch::new_with(None);

// Network stack resources.
// Must accommodate DHCPv4 plus our long-lived UDP discovery socket and TCP sockets.
static STACK_RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();

// The esp-radio controller must live for 'static so Wi-Fi tasks can be spawned.
static RADIO_INIT: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();

fn apply_event(item: UiEventItem, _ui: &CounterWindow, window: &Rc<MinimalSoftwareWindow>) {
    match item.event {
        UiEvent::Redraw | UiEvent::Frame => {
            window.request_redraw();
        }
        UiEvent::Housekeeping => {
            let handled = HOUSEKEEPING_HANDLED.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            // Log occasionally to avoid spamming serial.
            if handled.is_multiple_of(128) {
                let sent = HOUSEKEEPING_SENT.load(Ordering::Relaxed);
                let dropped = HOUSEKEEPING_DROPPED.load(Ordering::Relaxed);
                info!("housekeeping: handled={handled} sent={sent} dropped={dropped}");
            }
        }
    }
}

fn drain_ui_events(
    rx_hi: &PrioReceiver<'static, CriticalSectionRawMutex, UiEventItem, Max, 4>,
    counter_rx: &mut WatchDynReceiver<'static, i32>,
    heartbeat_rx: &mut WatchDynReceiver<'static, i32>,
    wifi_rx: &mut WatchDynReceiver<'static, WifiUiState>,
    esp_ipv4_rx: &mut WatchDynReceiver<'static, Option<embassy_net::Ipv4Address>>,
    esp_mac_rx: &mut WatchDynReceiver<'static, Option<[u8; 6]>>,
    ui: &CounterWindow,
    window: &Rc<MinimalSoftwareWindow>,
) {
    let mut redraw = false;

    while let Ok(item) = rx_hi.try_receive() {
        if matches!(item.event, UiEvent::Redraw | UiEvent::Frame) {
            redraw = true;
        }

        if item.event == UiEvent::Housekeeping {
            let _ = HOUSEKEEPING_HANDLED.fetch_add(1, Ordering::Relaxed);
        }
    }

    if let Some(value) = counter_rx.try_changed() {
        ui.set_counter(value);
        redraw = true;
    }

    if let Some(value) = heartbeat_rx.try_changed() {
        ui.set_heartbeat(value);
        redraw = true;
    }

    if let Some(value) = wifi_rx.try_changed() {
        let idx = (value.ssid_idx as usize).min(WIFI_SSIDS.len().saturating_sub(1));
        ui.set_wifi_ssid(WIFI_SSIDS[idx].into());
        ui.set_wifi_state(value.state as i32);
        ui.set_wifi_attempt(value.attempt as i32);
        ui.set_wifi_blink(value.blink_on);
        redraw = true;
    }

    if let Some(ip) = esp_ipv4_rx.try_changed() {
        let txt: slint::SharedString = match ip {
            Some(ip) => alloc::format!("{ip}").into(),
            None => "".into(),
        };
        ui.set_wifi_ip(txt);
        redraw = true;
    }

    if let Some(mac) = esp_mac_rx.try_changed() {
        let txt: slint::SharedString = match mac {
            Some([a, b, c, d, e, f]) => alloc::format!(
                "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}"
            )
            .into(),
            None => "".into(),
        };
        ui.set_wifi_mac(txt);
        redraw = true;
    }

    if redraw {
        window.request_redraw();
    }
}

struct EspBackend {
    window: RefCell<Option<Rc<MinimalSoftwareWindow>>>,
}

impl Default for EspBackend {
    fn default() -> Self {
        Self { window: RefCell::new(None) }
    }
}

impl Platform for EspBackend {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        if let Some(window) = self.window.borrow().as_ref() {
            return Ok(window.clone());
        }

        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        Duration::from_millis(Instant::now().duration_since_epoch().as_millis())
    }
}

#[embassy_executor::task]
async fn counter_task(tx: WatchDynSender<'static, i32>) {
    let mut value: i32 = 0;
    let mut dir: i32 = 1;
    let mut ticker = Ticker::every(EmbassyDuration::from_secs(1));
    loop {
        match select(ticker.next(), COUNTER_OVERRIDE.wait()).await {
            Either::First(()) => {
                value += dir;
                if value >= 100 {
                    value = 100;
                    dir = -1;
                } else if value <= 0 {
                    value = 0;
                    dir = 1;
                }
                tx.send(value);
            }
            Either::Second(new_value) => {
                value = new_value.clamp(0, 100);
                tx.send(value);
            }
        }
    }
}

#[embassy_executor::task]
async fn heartbeat_task(tx: WatchDynSender<'static, i32>) {
    let mut value: i32 = 0;
    let mut ticker = Ticker::every(EmbassyDuration::from_millis(1_000));
    loop {
        ticker.next().await;
        value = value.wrapping_add(1);
        tx.send(value);
    }
}

#[embassy_executor::task]
async fn netinfo_task(
    stack: Stack<'static>,
    ipv4_tx: WatchDynSender<'static, Option<embassy_net::Ipv4Address>>,
    mac_tx: WatchDynSender<'static, Option<[u8; 6]>>,
) {
    // MAC is available immediately (from eFuse). Publish once.
    let mac = Efuse::mac_address();
    mac_tx.send(Some(mac));
    log::info!(
        "net: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    UI_WAKE.signal(());

    let mut last_ip: Option<embassy_net::Ipv4Address> = None;

    loop {
        stack.wait_link_up().await;
        stack.wait_config_up().await;

        if let Some(cfg) = stack.config_v4() {
            let ip = cfg.address.address();
            if last_ip != Some(ip) {
                last_ip = Some(ip);
                ipv4_tx.send(Some(ip));
                log::info!("net: IPv4={ip}");
                UI_WAKE.signal(());
            }
        }

        Timer::after(EmbassyDuration::from_secs(1)).await;
    }
}

#[embassy_executor::task]
async fn button_task(
    mut button: Input<'static>,
    tx: PrioSender<'static, CriticalSectionRawMutex, UiEventItem, Max, 4>,
) {
    loop {
        // BOOT is typically active-low on ESP32-S3 boards.
        button.wait_for_falling_edge().await;
        let _ = tx.try_send(UiEventItem::new(UiEventPrio::High, UiEvent::Redraw));
        UI_WAKE.signal(());

        // Basic debounce.
        Timer::after(EmbassyDuration::from_millis(30)).await;
        button.wait_for_rising_edge().await;
    }
}

#[embassy_executor::task]
async fn frame_pacer_task(
    mut enabled_rx: WatchDynReceiver<'static, bool>,
    tx: PrioSender<'static, CriticalSectionRawMutex, UiEventItem, Max, 4>,
) {
    let mut enabled = enabled_rx.try_get().unwrap_or(false);
    let mut ticker = Ticker::every(EmbassyDuration::from_millis(16));

    loop {
        if enabled {
            ticker.next().await;
            let _ = tx.try_send(UiEventItem::new(UiEventPrio::Low, UiEvent::Frame));
            continue;
        }

        enabled = enabled_rx.changed().await;
    }
}

#[embassy_executor::task]
async fn housekeeping_task(tx: PrioSender<'static, CriticalSectionRawMutex, UiEventItem, Max, 4>) {
    // Periodic background events to exercise PriorityChannel backpressure.
    // Keep this relatively slow so it doesn't add noise or steal cycles from Wi-Fi/DHCP.
    let mut ticker = Ticker::every(EmbassyDuration::from_millis(250));
    loop {
        ticker.next().await;
        HOUSEKEEPING_SENT.fetch_add(1, Ordering::Relaxed);
        if tx
            .try_send(UiEventItem::new(UiEventPrio::Low, UiEvent::Housekeeping))
            .is_err()
        {
            HOUSEKEEPING_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}


#[embassy_executor::task]
async fn mqtt_counter_task(
    stack: Stack<'static>,
    mut counter_rx: WatchDynReceiver<'static, i32>,
    mut web_gui_rx: WatchDynReceiver<'static, Option<embassy_net::Ipv4Address>>,
    mut web_gui_port_rx: WatchDynReceiver<'static, Option<u16>>,
) {
    const TOPIC_ESP_COUNTER: &str = "speed/counter/esp";
    const TOPIC_SET_COUNTER: &str = "speed/counter/set";

    // embassy-net's `TcpSocket` implements `embedded-io-async` v0.6, but `rust-mqtt`
    // expects v0.7. Implement a small adapter that forwards v0.7 calls into the
    // v0.6 trait impl, mapping errors into an `embedded-io` v0.7-compatible type.
    #[derive(Debug)]
    struct MqttIoError(TcpError);

    impl core::fmt::Display for MqttIoError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }

    impl core::error::Error for MqttIoError {}

    impl embedded_io::Error for MqttIoError {
        fn kind(&self) -> embedded_io::ErrorKind {
            embedded_io::ErrorKind::Other
        }
    }

    struct TcpSocketV7<'a>(TcpSocket<'a>);

    impl<'a> embedded_io_async07::ErrorType for TcpSocketV7<'a> {
        type Error = MqttIoError;
    }

    impl<'a> embedded_io_async07::Read for TcpSocketV7<'a> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            embedded_io_async::Read::read(&mut self.0, buf)
                .await
                .map_err(MqttIoError)
        }
    }

    impl<'a> embedded_io_async07::Write for TcpSocketV7<'a> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            embedded_io_async::Write::write(&mut self.0, buf)
                .await
                .map_err(MqttIoError)
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            embedded_io_async::Write::flush(&mut self.0)
                .await
                .map_err(MqttIoError)
        }
    }

    let mut logged_config = false;

    // Pre-build MQTT topic name (static) for publishes. (Filters are re-created per reconnect,
    // because `subscribe()` takes ownership of the filter.)
    let topic_esp_name = unsafe {
        TopicName::new_unchecked(MqttString::try_from(TOPIC_ESP_COUNTER).unwrap())
    };

    let connect_opts = ConnectOptions {
        clean_start: true,
        // Larger keepalive reduces broker-side disconnects on slightly lossy Wi-Fi.
        keep_alive: KeepAlive::Seconds(120),
        session_expiry_interval: SessionExpiryInterval::EndOnDisconnect,
        user_name: None,
        password: None,
        will: None,
    };

    let sub_opts = SubscriptionOptions {
        retain_handling: RetainHandling::NeverSend,
        retain_as_published: false,
        no_local: false,
        qos: QoS::AtMostOnce,
    };

    let pub_opts = PublicationOptions {
        retain: false,
        topic: topic_esp_name,
        qos: QoS::AtMostOnce,
    };

    // Reconnect backoff to avoid thrashing on flaky Wi-Fi or blocked SSID/LAN rules.
    let mut backoff_secs: u64 = 2;

    loop {
        stack.wait_link_up().await;
        stack.wait_config_up().await;

        if !logged_config {
            logged_config = true;
            log::info!("mqtt_counter: IPv4 config up: {:?}", stack.config_v4());
        }

        let addr = match web_gui_rx.try_get().unwrap_or(None) {
            Some(ip) => ip,
            None => {
                log::warn!("mqtt_counter: no web_gui discovered yet (waiting for UDP announce)");
                Timer::after(EmbassyDuration::from_secs(2)).await;
                continue;
            }
        };

        let port = web_gui_port_rx.try_get().unwrap_or(None).unwrap_or(1883);

        let mut rx_buffer = [0u8; 4096];
        let mut tx_buffer = [0u8; 4096];
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        // Keep this >= MQTT keepalive so the socket doesn't time out mid-packet and
        // trigger reconnect loops.
        socket.set_timeout(Some(EmbassyDuration::from_secs(120)));

        log::info!("mqtt_counter: connecting to mqtt://{}:{}", addr, port);
        // Use an explicit, shorter timeout for the TCP handshake so we don't stall
        // for a long time when the network/SSID blocks client->LAN TCP.
        let connect_timeout = EmbassyDuration::from_secs(10);
        match select(socket.connect((addr, port)), Timer::after(connect_timeout)).await {
            Either::First(connect_res) => {
                if let Err(e) = connect_res {
                    log::warn!("mqtt_counter: connect to {}:{} failed: {:?}", addr, port, e);
                    Timer::after(EmbassyDuration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs.saturating_mul(2)).min(30);
                    continue;
                }
            }
            Either::Second(()) => {
                log::warn!(
                    "mqtt_counter: connect to {}:{} timed out after {:?} (likely blocked by SSID/LAN rules)",
                    addr,
                    port,
                    connect_timeout
                );
                Timer::after(EmbassyDuration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs.saturating_mul(2)).min(30);
                continue;
            }
        }

        let mut buffer = AllocBuffer;
        let mut client: MqttClient<'_, _, _, 4, 8, 8> = MqttClient::new(&mut buffer);

        let client_id = MqttString::try_from("esp32tft").ok();
        if let Err(e) = client
            .connect(TcpSocketV7(socket), &connect_opts, client_id)
            .await
        {
            log::warn!("mqtt_counter: MQTT connect failed: {:?}", e);
            client.abort().await;
            Timer::after(EmbassyDuration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs.saturating_mul(2)).min(30);
            continue;
        }

        let topic_set_filter = unsafe {
            TopicFilter::new_unchecked(MqttString::try_from(TOPIC_SET_COUNTER).unwrap())
        };
        if let Err(e) = client.subscribe(topic_set_filter, sub_opts).await {
            log::warn!("mqtt_counter: subscribe failed: {:?}", e);
            client.abort().await;
            Timer::after(EmbassyDuration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs.saturating_mul(2)).min(30);
            continue;
        }

        log::info!("mqtt_counter: connected to mqtt://{}:{}", addr, port);

        // Connection succeeded; reset backoff.
        backoff_secs = 2;

        // Publish the counter periodically. This also provides regular MQTT traffic
        // so the broker sees activity even if there are no inbound messages.
        let mut send_ticker = Ticker::every(EmbassyDuration::from_secs(5));

        let mut errored = false;

        'connected: loop {
            match select3(send_ticker.next(), client.poll_header(), MQTT_RECONNECT.wait()).await {
                Either3::First(()) => {
                    let value = counter_rx.try_get().unwrap_or(0).clamp(0, 100);
                    let payload = alloc::format!("{}", value);
                    let message: MqttBytes<'_> = payload.as_str().into();
                    if let Err(e) = client.publish(&pub_opts, message).await {
                        log::warn!("mqtt_counter: publish failed: {:?}", e);
                        client.abort().await;
                        errored = true;
                        break 'connected;
                    }
                }
                Either3::Second(header_res) => {
                    let header = match header_res {
                        Ok(h) => h,
                        Err(e) => {
                            log::warn!("mqtt_counter: poll header failed: {:?}", e);
                            client.abort().await;
                            errored = true;
                            break 'connected;
                        }
                    };

                    let event = match client.poll_body(header).await {
                        Ok(ev) => ev,
                        Err(e) => {
                            log::warn!("mqtt_counter: poll body failed: {:?}", e);
                            client.abort().await;
                            errored = true;
                            break 'connected;
                        }
                    };

                    if let MqttEvent::Publish(p) = event
                        && p.topic.as_ref() == TOPIC_SET_COUNTER
                    {
                        let payload = core::str::from_utf8(p.message.as_ref()).unwrap_or("");
                        if let Ok(v) = payload.trim().parse::<i32>() {
                            let v = v.clamp(0, 100);
                            COUNTER_OVERRIDE.signal(v);
                        }
                    }
                }
                Either3::Third(()) => {
                    log::info!("mqtt_counter: reconnect requested");
                    client.abort().await;
                    break 'connected;
                }
            }
        }

        if errored {
            Timer::after(EmbassyDuration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs.saturating_mul(2)).min(30);
        } else {
            // Manual reconnect request: keep it quick.
            Timer::after(EmbassyDuration::from_secs(1)).await;
            backoff_secs = 2;
        }
    }
}

#[embassy_executor::task]
async fn discovery_task(stack: Stack<'static>) {
    #[derive(Debug, Deserialize)]
    struct DiscoveryMsg {
        #[serde(default)]
        service: Option<alloc::string::String>,

        #[serde(default)]
        reconnect: Option<bool>,

        #[serde(default)]
        ip: Option<alloc::string::String>,

        #[serde(default)]
        mqtt_port: Option<u16>,
    }

    loop {
        stack.wait_link_up().await;
        stack.wait_config_up().await;

        let mut rx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 4];
        let mut tx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 4];
        let mut rx_buf = [0u8; 512];
        let mut tx_buf = [0u8; 256];
        let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);

        // Listen on a fixed UDP port so peers can broadcast to it.
        const DISCOVERY_PORT: u16 = 53530;
        if let Err(e) = socket.bind(DISCOVERY_PORT) {
            log::warn!("discovery: bind failed: {:?}", e);
            Timer::after(EmbassyDuration::from_secs(2)).await;
            continue;
        }

        // Broadcast announce. (255.255.255.255)
        let broadcast = stack
            .config_v4()
            .and_then(|cfg| cfg.address.broadcast())
            .unwrap_or(embassy_net::Ipv4Address::new(255, 255, 255, 255));
        log::info!("discovery: broadcasting to {}:{}", broadcast, DISCOVERY_PORT);
        let announce = b"{\"service\":\"esp32tft\",\"http_port\":8080}";

        // Discovery doesn't need to be ultra-frequent; slower reduces serial/log spam
        // and avoids wasting airtime on crowded 2.4GHz.
        let mut ticker = Ticker::every(EmbassyDuration::from_secs(5));
        let mut buf = [0u8; 512];
        let mut last_broker: Option<(embassy_net::Ipv4Address, u16)> = None;

        loop {
            ticker.next().await;

            // announce ourselves
            let _ = socket.send_to(announce, (broadcast, DISCOVERY_PORT)).await;

            // opportunistically receive any peer announcements
            if let Either::First(Ok((n, from))) = select(
                socket.recv_from(&mut buf),
                Timer::after(EmbassyDuration::from_millis(250)),
            )
            .await
                && let Ok(msg) = serde_json::from_slice::<DiscoveryMsg>(&buf[..n])
                && msg.service.as_deref() == Some("web_gui")
            {
                let embassy_net::IpAddress::Ipv4(ip) = from.endpoint.addr;

                // Prefer explicit broker IP from payload (more robust with multiple NICs/VPNs).
                let broker_ip = msg.ip.as_deref().and_then(|s| {
                    let v = s.parse::<core::net::Ipv4Addr>().ok()?;
                    let [a, b, c, d] = v.octets();
                    Some(embassy_net::Ipv4Address::new(a, b, c, d))
                });

                let broker_ip = broker_ip.unwrap_or(ip);

                let broker_port = msg.mqtt_port.unwrap_or(1883);

                let broker = (broker_ip, broker_port);
                if last_broker != Some(broker) {
                    last_broker = Some(broker);
                    log::info!(
                        "discovery: web_gui announce from {} (broker {}:{})",
                        ip,
                        broker_ip,
                        broker_port
                    );
                    WEB_GUI_IPV4.dyn_sender().send(Some(broker_ip));
                    WEB_GUI_MQTT_PORT.dyn_sender().send(Some(broker_port));
                }

                if msg.reconnect.unwrap_or(false) {
                    MQTT_RECONNECT.signal(());
                }
            }
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        if let Some(rest) = line.strip_prefix("Content-Length:")
            && let Ok(v) = rest.trim().parse::<usize>()
        {
            return v;
        }
        if let Some(rest) = line.strip_prefix("content-length:")
            && let Ok(v) = rest.trim().parse::<usize>()
        {
            return v;
        }
    }
    0
}

#[derive(Debug, Deserialize)]
struct EspCounterBody {
    #[serde(default)]
    counter: Option<i32>,
    #[serde(default)]
    count: Option<i32>,
}

#[embassy_executor::task]
async fn esp_counter_server_task(stack: Stack<'static>) {
    let mut logged_config = false;

    loop {
        stack.wait_link_up().await;
        stack.wait_config_up().await;
        if !logged_config {
            logged_config = true;
            log::info!("esp_counter_server: IPv4 config up: {:?}", stack.config_v4());
            log::info!("esp_counter_server: listening on :8080/esp_counter");
        }

        // Serve sequentially (single socket) to keep resource usage low.
        let mut rx_buffer = [0; 2048];
        let mut tx_buffer = [0; 1024];
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(EmbassyDuration::from_secs(15)));

        if let Err(e) = socket.accept(8080).await {
            log::warn!("esp_counter_server: accept failed: {:?}", e);
            Timer::after(EmbassyDuration::from_millis(250)).await;
            continue;
        }

        // Fallback discovery: learn the desktop IP from the TCP connection source.
        if let Some(remote) = socket.remote_endpoint() {
            let embassy_net::IpAddress::Ipv4(ip) = remote.addr;
            WEB_GUI_IPV4.dyn_sender().send(Some(ip));
            log::info!("esp_counter_server: learned web_gui IP from request: {}", ip);
        }

        let mut req = alloc::vec::Vec::<u8>::new();
        let mut tmp = [0u8; 512];
        let mut header_end: Option<usize> = None;
        let mut body_len: Option<usize> = None;
        let max_req = 4096usize;

        loop {
            match socket.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    if req.len() + n > max_req {
                        break;
                    }
                    req.extend_from_slice(&tmp[..n]);

                    if header_end.is_none() && let Some(idx) = find_subslice(&req, b"\r\n\r\n") {
                        header_end = Some(idx);
                        body_len = Some(
                            core::str::from_utf8(&req[..idx])
                                .map(parse_content_length)
                                .unwrap_or(0),
                        );
                    }

                    if let (Some(h_end), Some(b_len)) = (header_end, body_len) {
                        let needed = h_end + 4 + b_len;
                        if req.len() >= needed {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }

        let mut status = "400 Bad Request";
        let mut response_body = "{\"error\":\"bad_request\"}";

        if let Some(h_end) = header_end
            && let Ok(headers_str) = core::str::from_utf8(&req[..h_end])
        {
            let mut lines = headers_str.lines();
            if let Some(start_line) = lines.next() {
                let mut parts = start_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");

                    let b_len = body_len.unwrap_or(0);
                    let body_start = h_end + 4;
                    let body_end = core::cmp::min(req.len(), body_start + b_len);
                    let body_bytes = if body_start <= req.len() { &req[body_start..body_end] } else { &[] };

                if method == "POST" && path == "/esp_counter" {
                        match serde_json::from_slice::<EspCounterBody>(body_bytes) {
                            Ok(body) => {
                                if let Some(v) = body.counter.or(body.count) {
                                    let clamped = v.clamp(0, 100);
                                    COUNTER_OVERRIDE.signal(clamped);
                                    status = "200 OK";
                                    response_body = "{\"status\":\"ok\"}";
                                } else {
                                    status = "400 Bad Request";
                                    response_body = "{\"error\":\"missing_counter\"}";
                                }
                            }
                            Err(_) => {
                                status = "400 Bad Request";
                                response_body = "{\"error\":\"invalid_json\"}";
                            }
                        }
                } else {
                    status = "404 Not Found";
                    response_body = "{\"error\":\"not_found\"}";
                }
            }
        }

        let resp = alloc::format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = socket.write_all(resp.as_bytes()).await;
        socket.close();
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::WifiDevice<'static>>) -> ! {
    runner.run().await
}

/// Provides a draw buffer for the MinimalSoftwareWindow renderer.
struct DrawBuffer<'a, Display> {
    display: Display,
    buffer: &'a mut [Rgb565Pixel],
}

impl<
    DI: mipidsi::interface::Interface<Word = u8>,
    RST: OutputPin<Error = core::convert::Infallible>,
> slint::platform::software_renderer::LineBufferProvider
    for &mut DrawBuffer<'_, mipidsi::Display<DI, mipidsi::models::ST7789, RST>>
{
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Rgb565Pixel]),
    ) {
        let buffer = &mut self.buffer[range.clone()];
        render_fn(buffer);

        // mipidsi's set_pixels uses inclusive end coordinates; Range::end is exclusive.
        // Passing an out-of-range end coordinate is undefined behavior.
        let x_end_inclusive = (range.end - 1) as u16;
        self.display
            .set_pixels(
                range.start as u16,
                line as u16,
                x_end_inclusive,
                line as u16,
                buffer.iter().map(|x| RawU16::new(x.0).into()),
            )
            .unwrap();
    }
}

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    // Force INFO so wifi scan/selection logs are always visible.
    esp_println::logger::init_logger(log::LevelFilter::Info);
    esp_println::println!("boot: logger initialized (INFO)");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Embassy time needs a timer.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // Wi-Fi init requires internal heap (PSRAM-only allocations can fail with ESP_ERR_NO_MEM).
    // Mirror the setup that works in the main example.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    // COEX/Wi-Fi needs more RAM.
    esp_alloc::heap_allocator!(size: 64 * 1024);

    info!("Starting Slint ST7789 init (Feather TFT pinout)...");

    // Feather TFT control pins.
    let mut lcd_pwr = Output::new(peripherals.GPIO21, Level::Low, OutputConfig::default());
    let mut backlight = Output::new(peripherals.GPIO45, Level::Low, OutputConfig::default());
    let dc = Output::new(peripherals.GPIO39, Level::Low, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO40, Level::High, OutputConfig::default());

    // BOOT button (GPIO0). Optional event source for the UI loop: it just requests redraw.
    let button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

    // Ensure the panel is powered.
    lcd_pwr.set_high();

    // Turn on the backlight early so failures before display init don't leave it dark.
    backlight.set_high();

    // SPI2 on the Feather TFT pins.
    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO36)
    .with_mosi(peripherals.GPIO35)
    .with_miso(peripherals.GPIO37);

    // Chip select.
    let cs = Output::new(peripherals.GPIO7, Level::High, OutputConfig::default());

    // Wrap SPI into a bus.
    let spi_delay = Delay::new();
    let spi_device = ExclusiveDevice::new(spi, cs, spi_delay).unwrap();

    // mipidsi requires a pixel batching buffer that must live as long as the display driver.
    let mut di_buffer = [0u8; 512];
    let di = SpiInterface::new(spi_device, dc, &mut di_buffer);

    // ST7789 config for the Adafruit 240x135 panel.
    let mut delay = Delay::new();
    let display = Builder::new(ST7789, di)
        .reset_pin(reset)
        .color_order(ColorOrder::Bgr)
        .invert_colors(ColorInversion::Inverted)
        .display_size(135, 240)
        .display_offset(52, 40)
        .orientation(Orientation::new().rotate(Rotation::Deg90))
        .init(&mut delay)
        .unwrap();

    // Prepare a draw buffer for the Slint software renderer (one line).
    let mut buffer_provider = DrawBuffer {
        display,
        buffer: &mut [Rgb565Pixel(0); 240],
    };

    // Set up Slint platform + window.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    {
        let size = buffer_provider.display.size();
        window.set_size(slint::PhysicalSize::new(size.width, size.height));
    }
    // Install the pre-created window into the backend so Slint uses it.
    slint::platform::set_platform(Box::new(EspBackend { window: RefCell::new(Some(window.clone())) }))
        .expect("backend already initialized");

    let ui = CounterWindow::new().unwrap();
    ui.set_wifi_ssid(WIFI_SSIDS[0].into());
    ui.set_wifi_state(WifiConnState::Disconnected as i32);
    ui.set_wifi_attempt(0);
    ui.set_wifi_blink(false);
    ui.set_wifi_ip("".into());
    ui.set_wifi_mac("".into());
    ui.show().unwrap();

    // Force at least one frame to render.
    window.request_redraw();

    let rx_hi = UI_EVENTS_HI.receiver();
    let mut counter_rx = COUNTER_VALUE.dyn_receiver().unwrap();
    let mut heartbeat_rx = HEARTBEAT_VALUE.dyn_receiver().unwrap();
    let mut wifi_rx = WIFI_UI.dyn_receiver().unwrap();
    let mut esp_ipv4_rx = ESP_IPV4.dyn_receiver().unwrap();
    let mut esp_mac_rx = ESP_MAC.dyn_receiver().unwrap();
    let frame_enable_tx = FRAME_ENABLE.dyn_sender();
    let frame_enable_rx = FRAME_ENABLE.dyn_receiver().unwrap();

    // Start a dedicated Embassy task for the counter updates.
    // Slint stays single-threaded: the task only updates a Watch (latest state).
    _spawner.must_spawn(counter_task(COUNTER_VALUE.dyn_sender()));
    _spawner.must_spawn(heartbeat_task(HEARTBEAT_VALUE.dyn_sender()));
    _spawner.must_spawn(button_task(button, UI_EVENTS_HI.sender()));
    _spawner.must_spawn(frame_pacer_task(frame_enable_rx, UI_EVENTS_HI.sender()));
    _spawner.must_spawn(housekeeping_task(UI_EVENTS_HI.sender()));
    // Set up Wi-Fi controller and spawn the connect task.
    match esp_radio::init() {
        Ok(controller) => {
            let radio_init = RADIO_INIT.init(controller);
            match esp_radio::wifi::new(radio_init, peripherals.WIFI, Default::default()) {
                Ok((wifi_controller, ifaces)) => {
                    _spawner.must_spawn(speed::wifi::wifi_connect_task(
                        wifi_controller,
                        WIFI_UI.dyn_sender(),
                    ));
                    // Set up network stack
                    let config = Config::dhcpv4(Default::default());
                    let (stack, runner) = embassy_net::new(
                        ifaces.sta,
                        config,
                        STACK_RESOURCES.init(StackResources::new()),
                        123456789,
                    );

                    _spawner.must_spawn(net_task(runner));
                    _spawner.must_spawn(netinfo_task(
                        stack,
                        ESP_IPV4.dyn_sender(),
                        ESP_MAC.dyn_sender(),
                    ));
                    // Keep a single long-lived MQTT connection to the embedded desktop broker.
                    let counter_rx_mqtt = COUNTER_VALUE.dyn_receiver().unwrap();
                    let web_gui_rx_mqtt = WEB_GUI_IPV4.dyn_receiver().unwrap();
                    let web_gui_port_rx_mqtt = WEB_GUI_MQTT_PORT.dyn_receiver().unwrap();
                    _spawner.must_spawn(mqtt_counter_task(
                        stack,
                        counter_rx_mqtt,
                        web_gui_rx_mqtt,
                        web_gui_port_rx_mqtt,
                    ));
                    // Spawn server task to accept counter pushes from web_gui.
                    _spawner.must_spawn(esp_counter_server_task(stack));
                    // UDP discovery/announce to learn web_gui IP automatically.
                    _spawner.must_spawn(discovery_task(stack));
                }
                Err(e) => {
                    log::warn!("wifi: init controller failed: {e:?}");
                }
            }
        }
        Err(e) => {
            log::warn!("wifi: esp_radio::init failed: {e:?}");
        }
    }

    info!("Display initialized, entering async Slint render loop...");

    let mut last_anim_enabled = false;

    loop {
        // 1) Let Slint update its timers/animations.
        slint::platform::update_timers_and_animations();

        // Coalesce any pending updates before rendering.
        drain_ui_events(
            &rx_hi,
            &mut counter_rx,
            &mut heartbeat_rx,
            &mut wifi_rx,
            &mut esp_ipv4_rx,
            &mut esp_mac_rx,
            &ui,
            &window,
        );

        // 2) Render if needed.
        window.draw_if_needed(|renderer| {
            renderer.render_by_line(&mut buffer_provider);
        });

        // Coalesce any pending updates that happened during render.
        drain_ui_events(
            &rx_hi,
            &mut counter_rx,
            &mut heartbeat_rx,
            &mut wifi_rx,
            &mut esp_ipv4_rx,
            &mut esp_mac_rx,
            &ui,
            &window,
        );

        // If Slint has active animations, keep driving frames while still reacting to input.
        let anim_enabled = window.window().has_active_animations();
        if anim_enabled != last_anim_enabled {
            last_anim_enabled = anim_enabled;
            frame_enable_tx.send(anim_enabled);
        }

        if anim_enabled {
            window.request_redraw();

            let state_changed = select3(counter_rx.changed(), heartbeat_rx.changed(), wifi_rx.changed());
            match select(
                select3(
                    rx_hi.receive(),
                    state_changed,
                    UI_WAKE.wait(),
                ),
                Timer::after(EmbassyDuration::from_millis(250)),
            )
            .await {
                Either::First(result) => match result {
                    Either3::First(item) => apply_event(item, &ui, &window),
                    Either3::Second(Either3::First(value)) => {
                        ui.set_counter(value);
                        window.request_redraw();
                    }
                    Either3::Second(Either3::Second(value)) => {
                        ui.set_heartbeat(value);
                        window.request_redraw();
                    }
                    Either3::Second(Either3::Third(value)) => {
                        let idx = (value.ssid_idx as usize).min(WIFI_SSIDS.len().saturating_sub(1));
                        ui.set_wifi_ssid(WIFI_SSIDS[idx].into());
                        ui.set_wifi_state(value.state as i32);
                        ui.set_wifi_attempt(value.attempt as i32);
                        ui.set_wifi_blink(value.blink_on);
                        window.request_redraw();
                    }
                    Either3::Third(()) => {
                        // Explicit wake; keep animations moving.
                        window.request_redraw();
                    }
                },
                Either::Second(()) => {
                    // Safety net: ensure we keep servicing timers.
                    window.request_redraw();
                }
            }
            drain_ui_events(
                &rx_hi,
                &mut counter_rx,
                &mut heartbeat_rx,
                &mut wifi_rx,
                &mut esp_ipv4_rx,
                &mut esp_mac_rx,
                &ui,
                &window,
            );
            continue;
        }

        // 3) Sleep until something requires another iteration.
        // - Otherwise, sleep until the next Slint timer needs servicing,
        //   but also wake early if we get a counter update or an explicit redraw request.
        let wait = slint::platform::duration_until_next_timer_update()
            .and_then(|d| EmbassyDuration::try_from(d).ok());

        if let Some(wait) = wait {
            match select(
                select3(
                    rx_hi.receive(),
                    select3(counter_rx.changed(), heartbeat_rx.changed(), wifi_rx.changed()),
                    UI_WAKE.wait(),
                ),
                Timer::after(wait),
            )
            .await {
                Either::First(result) => match result {
                    Either3::First(item) => apply_event(item, &ui, &window),
                    Either3::Second(Either3::First(value)) => {
                        ui.set_counter(value);
                        window.request_redraw();
                    }
                    Either3::Second(Either3::Second(value)) => {
                        ui.set_heartbeat(value);
                        window.request_redraw();
                    }
                    Either3::Second(Either3::Third(value)) => {
                        let idx = (value.ssid_idx as usize).min(WIFI_SSIDS.len().saturating_sub(1));
                        ui.set_wifi_ssid(WIFI_SSIDS[idx].into());
                        ui.set_wifi_state(value.state as i32);
                        ui.set_wifi_attempt(value.attempt as i32);
                        ui.set_wifi_blink(value.blink_on);
                        window.request_redraw();
                    }
                    Either3::Third(()) => {
                        window.request_redraw();
                    }
                },
                Either::Second(()) => {
                    // Time to service Slint timers/animations in next iteration.
                }
            }
        } else {
            // No timers pending: block until an event arrives (HI has priority).
            match select(
                select(rx_hi.receive(), select3(counter_rx.changed(), heartbeat_rx.changed(), wifi_rx.changed())),
                UI_WAKE.wait(),
            )
            .await {
                Either::First(result) => match result {
                    Either::First(item) => apply_event(item, &ui, &window),
                    Either::Second(Either3::First(value)) => {
                        ui.set_counter(value);
                        window.request_redraw();
                    }
                    Either::Second(Either3::Second(value)) => {
                        ui.set_heartbeat(value);
                        window.request_redraw();
                    }
                    Either::Second(Either3::Third(value)) => {
                        let idx = (value.ssid_idx as usize).min(WIFI_SSIDS.len().saturating_sub(1));
                        ui.set_wifi_ssid(WIFI_SSIDS[idx].into());
                        ui.set_wifi_state(value.state as i32);
                        ui.set_wifi_attempt(value.attempt as i32);
                        ui.set_wifi_blink(value.blink_on);
                        window.request_redraw();
                    }
                },
                Either::Second(()) => {
                    window.request_redraw();
                }
            }
        }

        // Also apply any queued events that may have raced with the wait.
        drain_ui_events(
            &rx_hi,
            &mut counter_rx,
            &mut heartbeat_rx,
            &mut wifi_rx,
            &mut esp_ipv4_rx,
            &mut esp_mac_rx,
            &ui,
            &window,
        );
    }
}
