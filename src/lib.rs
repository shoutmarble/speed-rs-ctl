#![no_std]

use esp_backtrace as _;

pub mod tft {
	use embedded_graphics::pixelcolor::Rgb565;

	pub fn color_wheel(hue: u8) -> Rgb565 {
		// Classic “wheel” mapping used by NeoPixel examples.
		// 0..=255 transitions: red -> blue -> green -> red.
		let hue = 255u16 - hue as u16;

		let (r, g, b) = if hue < 85 {
			// Red -> Blue
			let r = (255 - (hue * 3)) as u8;
			let g = 0u8;
			let b = (hue * 3) as u8;
			(r, g, b)
		} else if hue < 170 {
			// Blue -> Green
			let hue = hue - 85;
			let r = 0u8;
			let g = (hue * 3) as u8;
			let b = (255 - (hue * 3)) as u8;
			(r, g, b)
		} else {
			// Green -> Red
			let hue = hue - 170;
			let r = (hue * 3) as u8;
			let g = (255 - (hue * 3)) as u8;
			let b = 0u8;
			(r, g, b)
		};

		Rgb565::new(r, g, b)
	}
}
