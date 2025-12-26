extern crate alloc;

use embassy_sync::watch::DynSender as WatchDynSender;
use embassy_time::{Duration as EmbassyDuration, Ticker, Timer};
use esp_radio::wifi::{
    AuthMethod,
    ClientConfig,
    Interfaces,
    ModeConfig,
    ScanConfig,
    WifiController,
    WifiStaState,
    sta_state,
};
use log::{info, warn};

pub const WIFI_SSIDS: [&str; 2] = ["COMMUNITIES.WIN", "324GUEST"];
pub const WIFI_PASSWORD: &str = "72427040";
pub const WIFI_MAX_ATTEMPTS: u8 = 3;

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WifiConnState {
    Disconnected = 0,
    Connecting = 1,
    Connected = 2,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WifiUiState {
    pub state: WifiConnState,
    /// 0 when not attempting; otherwise 1..=WIFI_MAX_ATTEMPTS.
    pub attempt: u8,
    pub blink_on: bool,
    /// Index into WIFI_SSIDS.
    pub ssid_idx: u8,
}

impl WifiUiState {
    pub const fn disconnected() -> Self {
        Self { state: WifiConnState::Disconnected, attempt: 0, blink_on: false, ssid_idx: 0 }
    }
}

#[embassy_executor::task]
pub async fn wifi_connect_task(
    mut controller: WifiController<'static>,
    _ifaces: Interfaces<'static>,
    tx: WatchDynSender<'static, WifiUiState>,
) {
    tx.send(WifiUiState::disconnected());

    // The Wi-Fi driver needs a mode configured before start().
    // Set an initial client config; we'll update it per attempt.
    let initial_client_config = ClientConfig::default()
        .with_ssid(WIFI_SSIDS[0].into())
        .with_password(WIFI_PASSWORD.into())
        .with_auth_method(AuthMethod::WpaWpa2Personal);
    let initial_mode_config = ModeConfig::Client(initial_client_config);
    if let Err(e) = controller.set_config(&initial_mode_config) {
        warn!("wifi: initial set_config failed: {e:?}");
        return;
    }

    if let Err(e) = controller.start() {
        warn!("wifi: start failed: {e:?}");
        return;
    }

    // Optional: scan once after start so we can log visibility/auth/channel.
    // Helpful for diagnosing 5GHz-only SSIDs (ESP32-S3 is 2.4GHz) and enterprise auth.
    match controller.scan_with_config(ScanConfig::default()) {
        Ok(aps) => {
            info!("wifi: scan found {} AP(s)", aps.len());

            // Log strongest APs so we can see if signal is too weak.
            let mut aps = aps;
            aps.sort_by(|a, b| b.signal_strength.cmp(&a.signal_strength));

            const MAX_LOG: usize = 15;
            for (idx, ap) in aps.iter().take(MAX_LOG).enumerate() {
                info!(
                    "wifi: ap[{}] ssid='{}' rssi={} channel={} auth={:?}",
                    idx,
                    ap.ssid,
                    ap.signal_strength,
                    ap.channel,
                    ap.auth_method
                );
            }

            for ssid in WIFI_SSIDS {
                if let Some(ap) = aps.iter().find(|ap| ap.ssid == ssid) {
                    info!(
                        "wifi: target visible ssid='{}' rssi={} channel={} auth={:?}",
                        ap.ssid,
                        ap.signal_strength,
                        ap.channel,
                        ap.auth_method
                    );
                } else {
                    warn!("wifi: SSID not seen in scan: {ssid}");
                }
            }
        }
        Err(e) => {
            warn!("wifi: scan failed: {e:?}");
        }
    }

    // Preserve the original UX contract: stop after WIFI_MAX_ATTEMPTS attempts.
    // But on each attempt, choose the best (strongest) visible SSID among our candidates.
    for attempt in 1..=WIFI_MAX_ATTEMPTS {
        let (ssid_idx, ssid) = match controller.scan_with_config(ScanConfig::default()) {
            Ok(aps) => {
                // Choose the strongest candidate by RSSI.
                let mut best: Option<(u8, &str, i8)> = None;
                for (idx, candidate) in WIFI_SSIDS.iter().enumerate() {
                    let idx_u8 = idx as u8;
                    if let Some(ap) = aps.iter().filter(|ap| ap.ssid == *candidate).max_by_key(|ap| ap.signal_strength) {
                        let rssi = ap.signal_strength;
                        info!("wifi: candidate ssid='{}' visible rssi={} channel={} auth={:?}", ap.ssid, rssi, ap.channel, ap.auth_method);
                        match best {
                            None => best = Some((idx_u8, *candidate, rssi)),
                            Some((_, _, best_rssi)) if rssi > best_rssi => best = Some((idx_u8, *candidate, rssi)),
                            _ => {}
                        }
                    } else {
                        info!("wifi: candidate ssid='{}' not visible", candidate);
                    }
                }

                if let Some((idx, ssid, rssi)) = best {
                    info!("wifi: selecting ssid='{}' (best rssi={})", ssid, rssi);
                    (idx, ssid)
                } else {
                    warn!("wifi: none of the candidate SSIDs are visible; defaulting to '{}'", WIFI_SSIDS[0]);
                    (0, WIFI_SSIDS[0])
                }
            }
            Err(e) => {
                warn!("wifi: scan failed during attempt selection: {e:?}; defaulting to '{}'", WIFI_SSIDS[0]);
                (0, WIFI_SSIDS[0])
            }
        };

        info!("wifi: connecting to {} (attempt {}/{})...", ssid, attempt, WIFI_MAX_ATTEMPTS);

        tx.send(WifiUiState { state: WifiConnState::Disconnected, attempt: 0, blink_on: false, ssid_idx });

        let client_config = ClientConfig::default()
            .with_ssid(ssid.into())
            .with_password(WIFI_PASSWORD.into())
            // User requested WPA; allow WPA or WPA2 personal.
            .with_auth_method(AuthMethod::WpaWpa2Personal);
        let mode_config = ModeConfig::Client(client_config);

        if let Err(e) = controller.set_config(&mode_config) {
            warn!("wifi: set_config failed for ssid='{ssid}': {e:?}");
            Timer::after(EmbassyDuration::from_secs(1)).await;
            continue;
        }

        // Ensure we start from a clean state.
        let _ = controller.disconnect();

        if let Err(e) = controller.connect() {
            warn!("wifi: connect() failed (ssid='{ssid}') attempt {attempt}: {e:?}");
            tx.send(WifiUiState { state: WifiConnState::Disconnected, attempt, blink_on: false, ssid_idx });
            Timer::after(EmbassyDuration::from_secs(1)).await;
            continue;
        }

        // Wait for the STA state to become connected, while blinking.
        let mut blink = false;
        let mut ticker = Ticker::every(EmbassyDuration::from_millis(500));
        let deadline_ticks: u8 = 60; // ~30s total

        let mut last_state_dbg = alloc::format!("{:?}", sta_state());

        for _ in 0..deadline_ticks {
            let state = sta_state();
            let state_dbg = alloc::format!("{:?}", state);
            if state_dbg != last_state_dbg {
                info!("wifi: sta_state (ssid='{ssid}') = {state_dbg}");
                last_state_dbg = state_dbg;
            }

            if matches!(state, WifiStaState::Connected) {
                info!("wifi: connected to {}", ssid);
                tx.send(WifiUiState { state: WifiConnState::Connected, attempt, blink_on: false, ssid_idx });
                return;
            }

            blink = !blink;
            tx.send(WifiUiState { state: WifiConnState::Connecting, attempt, blink_on: blink, ssid_idx });
            ticker.next().await;
        }

        warn!("wifi: timeout waiting for connection (ssid='{ssid}', attempt {attempt})");
        tx.send(WifiUiState { state: WifiConnState::Disconnected, attempt, blink_on: false, ssid_idx });
        Timer::after(EmbassyDuration::from_secs(1)).await;
    }

    warn!("wifi: giving up after {} attempts", WIFI_MAX_ATTEMPTS);
    tx.send(WifiUiState { state: WifiConnState::Disconnected, attempt: WIFI_MAX_ATTEMPTS, blink_on: false, ssid_idx: 0 });
}
