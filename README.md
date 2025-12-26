> How to build 
> > cargo esp-build-all-release

> How to flash adafruit esp32s3 tft heather  
> > espflash flash   target\xtensa-esp32s3-none-elf\release\slint_tft  
  
> > espflash flash   target\xtensa-esp32s3-none-elf\release\speed

> > espflash flash target\xtensa-esp32s3-none-elf\release\tft

> PRE-INSTALL TOOLS

> >  cargo install espup  
> >  espup install  

> >  cargo install esp-generate  

> >  cargo install espflash  

> >  cargo install esp-config  
> >  cargo install esp-config --features=tui --locked  


> >  irm https://github.com/probe-rs/probe-rs/releases/download/v0.30.0/probe-rs-tools-installer.ps1
 
> >  esp-generate --chip=esp32s3 speed-rs-ctl  