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
static COUNTER_VALUE: Watch<CriticalSectionRawMutex, i32, 1> = Watch::new_with(0);

// Another piece of shared UI state: a simple heartbeat counter.
static HEARTBEAT_VALUE: Watch<CriticalSectionRawMutex, i32, 1> = Watch::new_with(0);

// Enable/disable frame pacing when Slint has active animations.
static FRAME_ENABLE: Watch<CriticalSectionRawMutex, bool, 1> = Watch::new_with(false);

// Wake the UI loop without queueing an event (coalesces automatically).
static UI_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// Wi-Fi connection status for the UI.
static WIFI_UI: Watch<CriticalSectionRawMutex, WifiUiState, 1> = Watch::new_with(WifiUiState::disconnected());

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
    let mut ticker = Ticker::every(EmbassyDuration::from_millis(500));
    loop {
        ticker.next().await;

        value += dir;
        if value >= 100 {
            value = 100;
            dir = -1;
        } else if value <= 0 {
            value = 0;
            dir = 1;
        }

        // Latest-value semantics: overwrite previous value.
        tx.send(value);
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
    let mut ticker = Ticker::every(EmbassyDuration::from_millis(50));
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
    ui.show().unwrap();

    // Force at least one frame to render.
    window.request_redraw();

    let rx_hi = UI_EVENTS_HI.receiver();
    let mut counter_rx = COUNTER_VALUE.dyn_receiver().unwrap();
    let mut heartbeat_rx = HEARTBEAT_VALUE.dyn_receiver().unwrap();
    let mut wifi_rx = WIFI_UI.dyn_receiver().unwrap();
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
                        ifaces,
                        WIFI_UI.dyn_sender(),
                    ));
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
        drain_ui_events(&rx_hi, &mut counter_rx, &mut heartbeat_rx, &mut wifi_rx, &ui, &window);

        // 2) Render if needed.
        window.draw_if_needed(|renderer| {
            renderer.render_by_line(&mut buffer_provider);
        });

        // Coalesce any pending updates that happened during render.
        drain_ui_events(&rx_hi, &mut counter_rx, &mut heartbeat_rx, &mut wifi_rx, &ui, &window);

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
            drain_ui_events(&rx_hi, &mut counter_rx, &mut heartbeat_rx, &mut wifi_rx, &ui, &window);
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
        drain_ui_events(&rx_hi, &mut counter_rx, &mut heartbeat_rx, &mut wifi_rx, &ui, &window);
    }
}
