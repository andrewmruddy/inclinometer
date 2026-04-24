#![no_std]
#![no_main]

use core::cell::RefCell;
use core::convert::Infallible;
use core::fmt::Write as _;
use core::future::pending;
use core::sync::atomic::{AtomicU32, Ordering};

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::bind_interrupts;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::spim::{self, Config as SpimConfig, Error as SpimError, Frequency, MODE_0, Spim};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_6X10},
        MonoTextStyle,
    },
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::Text,
};
use embedded_hal::blocking::spi::Write as SpiWrite;
use embedded_hal::digital::v2::OutputPin as HalOutputPin;
use heapless::String;
use panic_probe as _;
use sharp_memory_display::MemoryDisplay;

bind_interrupts!(struct Irqs {
    TWISPI0 => spim::InterruptHandler<embassy_nrf::peripherals::TWISPI0>;
});

const SCL3300_READ_ANG_X: u32 = 0x2400_00C7;
const SCL3300_READ_ANG_Y: u32 = 0x2800_00CD;
const SCL3300_READ_ANG_Z: u32 = 0x2C00_00CB;
const SCL3300_READ_STATUS: u32 = 0x1800_00E5;
const SCL3300_READ_WHOAMI: u32 = 0x4000_0091;
const SCL3300_ENABLE_ANGLE_OUTPUTS: u32 = 0xB000_1F6F;
const SCL3300_MODE_1: u32 = 0xB400_001F;
const SCL3300_SW_RESET: u32 = 0xB400_2098;

static LED_TASK_US: AtomicU32 = AtomicU32::new(0);
static SENSOR_TASK_US: AtomicU32 = AtomicU32::new(0);
static DRAW_TASK_US: AtomicU32 = AtomicU32::new(0);
static LOOP_TASK_US: AtomicU32 = AtomicU32::new(0);

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

struct SharedBus {
    spi: RefCell<Spim<'static>>,
}

impl SharedBus {
    fn new(spi: Spim<'static>) -> Self {
        Self {
            spi: RefCell::new(spi),
        }
    }
}

struct DisplaySpi<'a> {
    bus: &'a SharedBus,
}

impl SpiWrite<u8> for DisplaySpi<'_> {
    type Error = SpimError;

    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        self.bus.spi.borrow_mut().blocking_write(words)
    }
}

#[derive(Clone, Copy)]
struct AngleReadings {
    x_raw: i16,
    y_raw: i16,
    z_raw: i16,
}

#[derive(Clone, Copy)]
enum SensorError {
    Spi,
    Crc,
    UnexpectedResponse,
    StartupStatus(u8),
    WhoAmI(u16),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScreenMode {
    Angles,
    Error,
}

struct Scl3300<'a> {
    bus: &'a SharedBus,
    cs: Output<'static>,
}

impl<'a> Scl3300<'a> {
    fn new(bus: &'a SharedBus, cs: Output<'static>) -> Self {
        Self { bus, cs }
    }

    async fn initialize(&mut self) -> Result<(), SensorError> {
        self.transfer_frame(SCL3300_SW_RESET)?;
        Timer::after_millis(1).await;

        self.transfer_frame(SCL3300_MODE_1)?;
        self.transfer_frame(SCL3300_ENABLE_ANGLE_OUTPUTS)?;
        Timer::after_millis(25).await;

        self.transfer_frame(SCL3300_READ_STATUS)?;
        self.transfer_frame(SCL3300_READ_STATUS)?;
        let status_response = self.transfer_frame(SCL3300_READ_STATUS)?;
        let rs = return_status(status_response);
        if rs != 0b01 {
            return Err(SensorError::StartupStatus(rs));
        }

        self.transfer_frame(SCL3300_READ_WHOAMI)?;
        let whoami_response = self.transfer_frame(SCL3300_READ_WHOAMI)?;
        let whoami = response_data(whoami_response);
        if (whoami & 0x00FF) != 0x00C1 {
            return Err(SensorError::WhoAmI(whoami));
        }

        Ok(())
    }

    fn read_angles(&mut self) -> Result<AngleReadings, SensorError> {
        self.transfer_frame(SCL3300_READ_ANG_X)?;
        let x_response = self.transfer_frame(SCL3300_READ_ANG_Y)?;
        let y_response = self.transfer_frame(SCL3300_READ_ANG_Z)?;
        let z_response = self.transfer_frame(SCL3300_READ_STATUS)?;

        Ok(AngleReadings {
            x_raw: self.parse_read_response(x_response, SCL3300_READ_ANG_X)?,
            y_raw: self.parse_read_response(y_response, SCL3300_READ_ANG_Y)?,
            z_raw: self.parse_read_response(z_response, SCL3300_READ_ANG_Z)?,
        })
    }

    fn parse_read_response(&self, response: u32, expected_command: u32) -> Result<i16, SensorError> {
        let response_opcode = ((response >> 26) & 0x3F) as u8;
        let expected_opcode = ((expected_command >> 26) & 0x3F) as u8;

        if response_opcode != expected_opcode {
            return Err(SensorError::UnexpectedResponse);
        }

        if return_status(response) != 0b01 {
            return Err(SensorError::StartupStatus(return_status(response)));
        }

        Ok(response_data(response) as i16)
    }

    fn transfer_frame(&mut self, frame: u32) -> Result<u32, SensorError> {
        let mut bytes = frame.to_be_bytes();

        self.cs.set_low();
        self.bus
            .spi
            .borrow_mut()
            .blocking_transfer_in_place(&mut bytes)
            .map_err(|_| SensorError::Spi)?;
        self.cs.set_high();

        let response = u32::from_be_bytes(bytes);
        if crc8(response >> 8) != (response as u8) {
            return Err(SensorError::Crc);
        }

        Ok(response)
    }
}

#[embassy_executor::task]
async fn led_task(mut led: Output<'static>) {
    loop {
        let started = Instant::now();
        led.toggle();
        LED_TASK_US.store(elapsed_micros_u32(started), Ordering::Relaxed);
        Timer::after(Duration::from_millis(250)).await;
    }
}

#[embassy_executor::task]
async fn sensor_display_task(
    spi: Spim<'static>,
    display_cs: Output<'static>,
    sensor_cs: Output<'static>,
) {
    let bus = SharedBus::new(spi);
    let display_spi = DisplaySpi { bus: &bus };
    let display_disp = TiedHighDisplayPin;
    let mut display = MemoryDisplay::new(display_spi, display_cs, display_disp);
    let mut sensor = Scl3300::new(&bus, sensor_cs);
    let mut refresh_count: u32 = 0;
    let mut screen_mode = ScreenMode::Angles;

    display.enable();
    display.set_clear_state(BinaryColor::On);
    initialize_angle_screen(&mut display, refresh_count);

    if let Err(err) = sensor.initialize().await {
        draw_sensor_error(&mut display, err, refresh_count);
        loop {
            let started = Instant::now();
            refresh_count = refresh_count.wrapping_add(1);
            draw_sensor_error(&mut display, err, refresh_count);
            DRAW_TASK_US.store(elapsed_micros_u32(started), Ordering::Relaxed);
            LOOP_TASK_US.store(elapsed_micros_u32(started), Ordering::Relaxed);
            display.display_mode();
            Timer::after(Duration::from_millis(250)).await;
        }
    }

    loop {
        let started = Instant::now();
        refresh_count = refresh_count.wrapping_add(1);

        let sensor_started = Instant::now();
        let sensor_result = sensor.read_angles();
        SENSOR_TASK_US.store(elapsed_micros_u32(sensor_started), Ordering::Relaxed);

        let draw_started = Instant::now();
        match sensor_result {
            Ok(readings) => {
                if screen_mode != ScreenMode::Angles {
                    initialize_angle_screen(&mut display, refresh_count);
                    screen_mode = ScreenMode::Angles;
                }
                draw_angle_screen(&mut display, readings, refresh_count);
            }
            Err(err) => {
                screen_mode = ScreenMode::Error;
                draw_sensor_error(&mut display, err, refresh_count);
            }
        }
        DRAW_TASK_US.store(elapsed_micros_u32(draw_started), Ordering::Relaxed);

        LOOP_TASK_US.store(elapsed_micros_u32(started), Ordering::Relaxed);
        display.display_mode();
        Timer::after(Duration::from_millis(100)).await;
    }
}

fn initialize_angle_screen(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    refresh_count: u32,
) {
    display.clear_buffer();

    let title_style = MonoTextStyle::new(&FONT_10X20, BinaryColor::Off);
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let _ = Text::new("SCL3300", Point::new(12, 20), title_style).draw(display);
    let _ = Text::new("angles in deg", Point::new(12, 108), body_style).draw(display);
    draw_status_overlay(display, refresh_count);
    display.flush_buffer();
}

fn draw_angle_screen(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    readings: AngleReadings,
    refresh_count: u32,
) {
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let clear_style = PrimitiveStyle::with_fill(BinaryColor::On);

    let x_line = format_axis_line('X', readings.x_raw);
    let y_line = format_axis_line('Y', readings.y_raw);
    let z_line = format_axis_line('Z', readings.z_raw);

    clear_line(display, 12, 44, 140, 12, clear_style);
    clear_line(display, 12, 60, 140, 12, clear_style);
    clear_line(display, 12, 76, 140, 12, clear_style);

    let _ = Text::new(&x_line, Point::new(12, 52), body_style).draw(display);
    let _ = Text::new(&y_line, Point::new(12, 68), body_style).draw(display);
    let _ = Text::new(&z_line, Point::new(12, 84), body_style).draw(display);
    draw_status_overlay(display, refresh_count);

    display.flush_buffer();
}

fn draw_sensor_error(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    error: SensorError,
    refresh_count: u32,
) {
    display.clear_buffer();

    let title_style = MonoTextStyle::new(&FONT_10X20, BinaryColor::Off);
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let _ = Text::new("SCL3300", Point::new(12, 20), title_style).draw(display);
    let _ = Text::new("sensor error", Point::new(12, 44), body_style).draw(display);

    let message = match error {
        SensorError::Spi => "SPI transfer failed",
        SensorError::Crc => "CRC mismatch",
        SensorError::UnexpectedResponse => "off-frame mismatch",
        SensorError::StartupStatus(0) => "startup in progress",
        SensorError::StartupStatus(1) => "status says ok",
        SensorError::StartupStatus(3) => "status flag set",
        SensorError::StartupStatus(_) => "status reserved",
        SensorError::WhoAmI(_) => "WHOAMI mismatch",
    };
    let _ = Text::new(message, Point::new(12, 60), body_style).draw(display);

    if let SensorError::WhoAmI(value) = error {
        let mut detail: String<32> = String::new();
        let _ = write!(&mut detail, "whoami=0x{:04X}", value);
        let _ = Text::new(&detail, Point::new(12, 76), body_style).draw(display);
    }

    draw_status_overlay(display, refresh_count);

    display.flush_buffer();
}

fn draw_status_overlay(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    refresh_count: u32,
) {
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let clear_style = PrimitiveStyle::with_fill(BinaryColor::On);

    clear_line(display, 320, 0, 80, 14, clear_style);
    clear_line(display, 232, 194, 168, 42, clear_style);

    let mut refresh_text: String<16> = String::new();
    let _ = write!(&mut refresh_text, "R{:05}", refresh_count % 100_000);
    let _ = Text::new(&refresh_text, Point::new(324, 10), body_style).draw(display);

    let led_us = LED_TASK_US.load(Ordering::Relaxed);
    let sens_us = SENSOR_TASK_US.load(Ordering::Relaxed);
    let draw_us = DRAW_TASK_US.load(Ordering::Relaxed);
    let loop_us = LOOP_TASK_US.load(Ordering::Relaxed);

    let mut led_text: String<24> = String::new();
    let _ = write!(&mut led_text, "LED {:>5}us", led_us);
    let _ = Text::new(&led_text, Point::new(232, 202), body_style).draw(display);

    let mut sens_text: String<24> = String::new();
    let _ = write!(&mut sens_text, "SNS {:>5}us", sens_us);
    let _ = Text::new(&sens_text, Point::new(232, 214), body_style).draw(display);

    let mut draw_text: String<24> = String::new();
    let _ = write!(&mut draw_text, "DRW {:>5}us", draw_us);
    let _ = Text::new(&draw_text, Point::new(232, 226), body_style).draw(display);

    let mut loop_text: String<24> = String::new();
    let _ = write!(&mut loop_text, "LOP {:>5}us", loop_us);
    let _ = Text::new(&loop_text, Point::new(232, 238), body_style).draw(display);
}

fn clear_line(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    style: PrimitiveStyle<BinaryColor>,
) {
    let _ = Rectangle::new(Point::new(x, y), Size::new(width, height))
        .into_styled(style)
        .draw(display);
}

fn format_axis_line(axis: char, raw: i16) -> String<32> {
    let mut line: String<32> = String::new();
    let centi_degrees = (raw as i32 * 9000) / 16384;
    let sign = if centi_degrees < 0 { '-' } else { '+' };
    let magnitude = centi_degrees.abs();

    let _ = write!(
        &mut line,
        "{}: {}{}.{:02}",
        axis,
        sign,
        magnitude / 100,
        magnitude % 100
    );

    line
}

fn response_data(response: u32) -> u16 {
    ((response >> 8) & 0xFFFF) as u16
}

fn return_status(response: u32) -> u8 {
    ((response >> 24) & 0x03) as u8
}

fn crc8(data_24: u32) -> u8 {
    let mut crc = 0xFFu8;

    for bit_index in (0..24).rev() {
        let bit = ((data_24 >> bit_index) & 0x01) as u8;
        let mut top = crc & 0x80;
        if bit == 1 {
            top ^= 0x80;
        }

        crc <<= 1;
        if top != 0 {
            crc ^= 0x1D;
        }
    }

    !crc
}

fn elapsed_micros_u32(started: Instant) -> u32 {
    started.elapsed().as_micros().min(u32::MAX as u64) as u32
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let mut spi_config = SpimConfig::default();
    spi_config.frequency = Frequency::M2;
    spi_config.mode = MODE_0;

    let spi = Spim::new(
        p.TWISPI0, Irqs, p.P0_14, // shared SCK
        p.P0_15, // shared MISO, used by SCL3300
        p.P0_13, // shared MOSI / Sharp display DI
        spi_config,
    );

    let display_cs = Output::new(p.P0_03, Level::High, OutputDrive::Standard); // A5 -> Sharp CS
    let sensor_cs = Output::new(p.P0_28, Level::High, OutputDrive::Standard); // A3 -> SCL3300 CSB
    let led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard); // onboard LED

    spawner.spawn(led_task(led)).unwrap();
    spawner.spawn(sensor_display_task(spi, display_cs, sensor_cs)).unwrap();

    pending::<()>().await;
}
