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

pub mod demo {
	use embassy_time::{Duration, Timer};
	use embedded_graphics::{
		pixelcolor::Rgb565,
		prelude::*,
		primitives::{PrimitiveStyle, Rectangle},
	};

	pub fn draw_test_bars<D>(display: &mut D) -> Result<(), D::Error>
	where
		D: DrawTarget<Color = Rgb565> + OriginDimensions,
	{
		display.clear(Rgb565::BLACK)?;

		let size = display.size();
		let bar_h = size.height / 3;

		Rectangle::new(Point::new(0, 0), Size::new(size.width, bar_h))
			.into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
			.draw(display)?;

		Rectangle::new(Point::new(0, bar_h as i32), Size::new(size.width, bar_h))
			.into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN))
			.draw(display)?;

		Rectangle::new(
			Point::new(0, (bar_h * 2) as i32),
			Size::new(size.width, size.height - bar_h * 2),
		)
		.into_styled(PrimitiveStyle::with_fill(Rgb565::BLUE))
		.draw(display)?;

		Ok(())
	}

	pub async fn cycle_hues<D>(display: &mut D) -> !
	where
		D: DrawTarget<Color = Rgb565>,
	{
		let mut hue: u8 = 0;
		loop {
			let _ = display.clear(super::tft::color_wheel(hue));
			Timer::after(Duration::from_millis(150)).await;
			let _ = display.clear(Rgb565::BLACK);
			Timer::after(Duration::from_millis(50)).await;

			hue = hue.wrapping_add(2);
		}
	}
}
