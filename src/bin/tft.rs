#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use speed::demo;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use embedded_hal_bus::spi::ExclusiveDevice;
use log::info;
use mipidsi::{
    Builder,
    interface::SpiInterface,
    models::ST7789,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
};

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Embassy time needs a timer.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    info!("Starting TFT init (real Feather pinout)...");

    // Feather TFT control pins.
    let mut lcd_pwr = Output::new(peripherals.GPIO21, Level::Low, OutputConfig::default());
    let mut backlight = Output::new(peripherals.GPIO45, Level::Low, OutputConfig::default());
    let dc = Output::new(peripherals.GPIO39, Level::Low, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO40, Level::High, OutputConfig::default());

    // Ensure the panel is powered.
    lcd_pwr.set_high();

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

    // Build a SpiDevice so higher-level display drivers can own CS handling.
    let delay = Delay::new();
    let spi_device = ExclusiveDevice::new(spi, cs, delay).unwrap();

    // mipidsi 0.9 uses its own SPI display interface, which requires a pixel batching buffer.
    // The buffer must live as long as the display driver.
    let mut di_buffer = [0u8; 512];
    let di = SpiInterface::new(spi_device, dc, &mut di_buffer);

    // ST7789 config for the Adafruit 240x135 panel.
    // This panel is an odd-size window within the controller's RAM, so offsets matter.
    // If you see clipping / partial updates, the offset is the first thing to fix.
    let mut delay = Delay::new();
    let mut display = Builder::new(ST7789, di)
        .reset_pin(reset)
        .color_order(ColorOrder::Bgr)
        .invert_colors(ColorInversion::Inverted)
        // Typical offsets for the 1.14" 240x135 ST7789 window.
        // NOTE: Some panels are off-by-one on the long edge due to the odd 135px dimension;
        // if you see a 1px line not updating, try 52 vs 53 here.
        .display_size(135, 240)
        .display_offset(52, 40)
        // Rotate content into a landscape-friendly orientation.
        .orientation(Orientation::new().rotate(Rotation::Deg90))
        .init(&mut delay)
        .unwrap();

    backlight.set_high();

    demo::draw_test_bars(&mut display).unwrap();

    info!("TFT init done. (Color cycling removed.)");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
