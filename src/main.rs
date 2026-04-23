#![no_std]
#![no_main]

use core::convert::Infallible;
use core::future::pending;

use embassy_executor::Spawner;
use embassy_nrf::bind_interrupts;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::spim::{self, Config as SpimConfig, Frequency, MODE_0, Spim};
use embassy_time::{Duration, Timer};
use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_6X10, ascii::FONT_10X20},
    pixelcolor::BinaryColor,
    prelude::*,
    text::Text,
};
use embedded_hal::digital::v2::OutputPin as HalOutputPin;
use panic_halt as _;

use sharp_memory_display::MemoryDisplay;

bind_interrupts!(struct Irqs {
    TWISPI0 => spim::InterruptHandler<embassy_nrf::peripherals::TWISPI0>;
});

struct TiedHighDisplayPin;

impl HalOutputPin for TiedHighDisplayPin {
    type Error = Infallible;

    fn set_low(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[embassy_executor::task]
async fn led_task(mut led: Output<'static>) {
    loop {
        led.toggle();
        Timer::after(Duration::from_millis(250)).await;
    }
}

#[embassy_executor::task]
async fn display_task(
    mut display: MemoryDisplay<Spim<'static>, Output<'static>, TiedHighDisplayPin>,
) {
    display.enable();
    display.set_clear_state(BinaryColor::On);
    display.clear_buffer();

    let title_style = MonoTextStyle::new(&FONT_10X20, BinaryColor::Off);
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let _ = Text::new("Hello!", Point::new(16, 32), title_style).draw(&mut display);
    let _ = Text::new("nRF52840 Display", Point::new(16, 52), body_style).draw(&mut display);
    display.flush_buffer();

    loop {
        display.display_mode();
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let mut config = SpimConfig::default();
    config.frequency = Frequency::M1;
    config.mode = MODE_0;

    let spi = Spim::new_txonly(
        p.TWISPI0, Irqs, p.P0_14, // SCK
        p.P0_13, // MOSI / display DI
        config,
    );

    let cs = Output::new(p.P0_03, Level::High, OutputDrive::Standard); // A5 -> CS
    let disp = TiedHighDisplayPin; // DISP is physically held high.
    let led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard); // onboard LED

    let display = MemoryDisplay::new(spi, cs, disp);

    spawner.spawn(led_task(led)).unwrap();
    spawner.spawn(display_task(display)).unwrap();

    pending::<()>().await;
}
