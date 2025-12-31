# speed-rs-ctl

ESP32-S3 firmware project using Embassy + Rust.

## Demo

![ESP32-S3 TFT running the UI](assets/device.jpg)


## Prerequisites

- Rust toolchain (installed via `rustup`)
- Windows PowerShell (examples below use PowerShell)
- A connected ESP32-S3 board (e.g. Adafruit ESP32-S3 TFT Feather)

## Install tools

Install the ESP Rust toolchain helper and set up the `esp` toolchain:

```powershell
cargo install espup
espup install
```

Install useful helpers:

```powershell
cargo install esp-generate
cargo install espflash
```

Optional: install `esp-config` (TUI):

```powershell
cargo install esp-config --features=tui --locked
```

Optional: Probe-rs tools installer script (PowerShell):

```powershell
irm https://github.com/probe-rs/probe-rs/releases/download/v0.30.0/probe-rs-tools-installer.ps1
```

Optional: create a new project (example):

```powershell
esp-generate --chip=esp32s3 speed-rs-ctl
```

## Build

This repo defines Cargo aliases in `.cargo/config.toml`.

Build all ESP32-S3 binaries (release):

```powershell
cargo esp-build-all-release
```

Build just the Slint TFT app (release):

```powershell
cargo esp-build-slint-tft-release
```

## Flash

You can flash a built binary using `espflash`.

If you don’t specify `--port`, `espflash` will try to auto-detect. On Windows, it’s often easiest to specify it explicitly (e.g. `COM3`).

Flash the Slint TFT firmware:

```powershell
espflash flash --port COM3 -M target\xtensa-esp32s3-none-elf\release\slint_tft
```

Flash the main firmware (`speed`):

```powershell
espflash flash --port COM3 -M target\xtensa-esp32s3-none-elf\release\speed
```

Flash the TFT test binary (`tft`):

```powershell
espflash flash --port COM3 -M target\xtensa-esp32s3-none-elf\release\tft
```

## Troubleshooting

- **No serial ports detected**: try a different USB cable (data-capable), different USB port, and confirm the board enumerates in Device Manager.
- **Wrong COM port**: use the COM port shown by `espflash` or Device Manager.
- **Rebuild issues**: run the matching `cargo esp-build-*` alias again (these include the required `-Z build-std=core,alloc` flags).