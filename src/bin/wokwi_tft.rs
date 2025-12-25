#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use speed::demo;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode,
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::*,
};
use embedded_hal_bus::spi::ExclusiveDevice;
use log::info;
use mipidsi::{
    interface::SpiInterface,
    models::ILI9341Rgb565,
    options::{ColorInversion, ColorOrder},
    Builder,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// Wokwi note:
/// - `diagram.json` uses `wokwi-ili9341` (240x320).
/// - The simulated ILI9341 ignores RST and LED pins, so this demo does not depend on them.
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Embassy time needs a timer.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    info!("Starting Wokwi TFT demo (ILI9341)...");

    // Pins match your diagram.json wiring.
    let dc = Output::new(peripherals.GPIO39, Level::Low, OutputConfig::default());

    // SPI2
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

    let cs = Output::new(peripherals.GPIO7, Level::High, OutputConfig::default());

    let delay = Delay::new();
    let spi_device = ExclusiveDevice::new(spi, cs, delay).unwrap();

    // mipidsi 0.9 requires a batching buffer.
    let mut di_buffer = [0u8; 512];
    let di = SpiInterface::new(spi_device, dc, &mut di_buffer);

    let mut delay = Delay::new();
    let mut display = Builder::new(ILI9341Rgb565, di)
        .display_size(240, 320)
        .color_order(ColorOrder::Rgb)
        .invert_colors(ColorInversion::Normal)
        .init(&mut delay)
        .unwrap();

    demo::draw_test_bars(&mut display).unwrap();

    info!("Wokwi TFT demo running.");

    demo::cycle_hues(&mut display).await
}
