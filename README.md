> how to build 
> > cargo esp-build-all-release

> how to flash adafruit esp32s3 tft heather
> > espflash flash target\xtensa-esp32s3-none-elf\release\slint_tft
> > espflash flash target\xtensa-esp32s3-none-elf\release\speed
> > espflash flash target\xtensa-esp32s3-none-elf\release\tft


>   1 cargo install espup
>   2 espup install
>   5 cargo install esp-generate
>   8 cargo install espflash
>  11 cargo install esp-config
>  12 cargo install esp-config --features=tui --locked


>  15 irm https://github.com/probe-rs/probe-rs/releases/latest/download/probe-rs-tools-ins...
 
>  18 esp-generate --chip=esp32s3 speed-rs-ctl