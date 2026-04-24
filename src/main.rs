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
        MonoFont, MonoTextStyle,
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
static CPU_TASK_US: AtomicU32 = AtomicU32::new(0);
static FLUSH_TASK_US: AtomicU32 = AtomicU32::new(0);
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

struct UiState {
    screen_mode: Option<ScreenMode>,
    last_flush_at: Instant,
    x_text: String<32>,
    y_text: String<32>,
    z_text: String<32>,
    refresh_text: String<16>,
    led_text: String<24>,
    sensor_text: String<24>,
    cpu_text: String<24>,
    flush_text: String<24>,
    loop_text: String<24>,
    error_message: String<32>,
    error_detail: String<32>,
}

impl UiState {
    fn new(now: Instant) -> Self {
        Self {
            screen_mode: None,
            last_flush_at: now,
            x_text: String::new(),
            y_text: String::new(),
            z_text: String::new(),
            refresh_text: String::new(),
            led_text: String::new(),
            sensor_text: String::new(),
            cpu_text: String::new(),
            flush_text: String::new(),
            loop_text: String::new(),
            error_message: String::new(),
            error_detail: String::new(),
        }
    }

    fn invalidate_dynamic_fields(&mut self) {
        self.x_text.clear();
        self.y_text.clear();
        self.z_text.clear();
        self.refresh_text.clear();
        self.led_text.clear();
        self.sensor_text.clear();
        self.cpu_text.clear();
        self.flush_text.clear();
        self.loop_text.clear();
        self.error_message.clear();
        self.error_detail.clear();
    }
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
    let mut ui_state = UiState::new(Instant::now());

    display.enable();
    display.set_clear_state(BinaryColor::On);
    show_startup_splash(&mut display).await;

    if let Err(err) = sensor.initialize().await {
        loop {
            refresh_count = refresh_count.wrapping_add(1);
            let loop_started = Instant::now();

            if ui_state.screen_mode != Some(ScreenMode::Error) {
                prepare_error_screen(&mut display, &mut ui_state);
            }

            let cpu_started = Instant::now();
            let needs_flush = draw_sensor_error(&mut display, &mut ui_state, err, refresh_count);
            let cpu_us = elapsed_micros_u32(cpu_started);
            let flush_us = flush_or_keepalive(&mut display, &mut ui_state, needs_flush);
            let loop_us = elapsed_micros_u32(loop_started);

            CPU_TASK_US.store(cpu_us, Ordering::Relaxed);
            FLUSH_TASK_US.store(flush_us, Ordering::Relaxed);
            LOOP_TASK_US.store(loop_us, Ordering::Relaxed);

            Timer::after(Duration::from_millis(250)).await;
        }
    }

    loop {
        refresh_count = refresh_count.wrapping_add(1);
        let loop_started = Instant::now();

        let sensor_started = Instant::now();
        let sensor_result = sensor.read_angles();
        let sensor_us = elapsed_micros_u32(sensor_started);

        let cpu_started = Instant::now();
        let needs_flush = match sensor_result {
            Ok(readings) => {
                if ui_state.screen_mode != Some(ScreenMode::Angles) {
                    prepare_angle_screen(&mut display, &mut ui_state);
                }
                draw_angle_screen(&mut display, &mut ui_state, readings, refresh_count)
            }
            Err(err) => {
                if ui_state.screen_mode != Some(ScreenMode::Error) {
                    prepare_error_screen(&mut display, &mut ui_state);
                }
                draw_sensor_error(&mut display, &mut ui_state, err, refresh_count)
            }
        };
        let cpu_us = elapsed_micros_u32(cpu_started);
        let flush_us = flush_or_keepalive(&mut display, &mut ui_state, needs_flush);
        let loop_us = elapsed_micros_u32(loop_started);

        SENSOR_TASK_US.store(sensor_us, Ordering::Relaxed);
        CPU_TASK_US.store(cpu_us, Ordering::Relaxed);
        FLUSH_TASK_US.store(flush_us, Ordering::Relaxed);
        LOOP_TASK_US.store(loop_us, Ordering::Relaxed);

        Timer::after(Duration::from_millis(100)).await;
    }
}

async fn show_startup_splash(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
) {
    display.clear_buffer();

    draw_scaled_centered_text(display, "Ruddy", 5, 25);
    draw_scaled_centered_text(display, "Subsea", 5, 125);

    display.flush_buffer();
    Timer::after(Duration::from_secs(5)).await;
}

fn draw_scaled_centered_text(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    text: &str,
    scale: u32,
    top_y: i32,
) {
    let width = scaled_text_width(&FONT_10X20, text, scale) as i32;
    let start_x = ((400 - width) / 2).max(0);

    for (index, ch) in text.chars().enumerate() {
        let glyph_advance =
            (FONT_10X20.character_size.width + FONT_10X20.character_spacing) * scale;
        let glyph_x = start_x + (index as i32 * glyph_advance as i32);
        draw_scaled_glyph(display, &FONT_10X20, ch, scale, Point::new(glyph_x, top_y));
    }
}

fn scaled_text_width(font: &MonoFont<'_>, text: &str, scale: u32) -> u32 {
    let char_count = text.chars().count() as u32;
    if char_count == 0 {
        0
    } else {
        ((font.character_size.width + font.character_spacing) * char_count - font.character_spacing) * scale
    }
}

fn draw_scaled_glyph(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    font: &MonoFont<'_>,
    ch: char,
    scale: u32,
    top_left: Point,
) {
    let glyphs_per_row = font.image.size().width / font.character_size.width;
    let glyph_index = font.glyph_mapping.index(ch) as u32;
    let row = glyph_index / glyphs_per_row;
    let char_x = (glyph_index - (row * glyphs_per_row)) * font.character_size.width;
    let char_y = row * font.character_size.height;
    let glyph_area = Rectangle::new(
        Point::new(char_x as i32, char_y as i32),
        font.character_size,
    );

    let mut scaled_target = ScaledDrawTarget {
        display,
        offset: top_left,
        scale,
    };
    let _ = font.image.draw_sub_image(&mut scaled_target, &glyph_area);
}

struct ScaledDrawTarget<'a, 'b> {
    display: &'a mut MemoryDisplay<DisplaySpi<'b>, Output<'static>, TiedHighDisplayPin>,
    offset: Point,
    scale: u32,
}

impl OriginDimensions for ScaledDrawTarget<'_, '_> {
    fn size(&self) -> Size {
        Size::new(400, 240)
    }
}

impl DrawTarget for ScaledDrawTarget<'_, '_> {
    type Color = BinaryColor;
    type Error = SpimError;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels {
            let scaled_top_left = Point::new(
                self.offset.x + coord.x * self.scale as i32,
                self.offset.y + coord.y * self.scale as i32,
            );
            let style = PrimitiveStyle::with_fill(color);
            let rect = Rectangle::new(
                scaled_top_left,
                Size::new(self.scale, self.scale),
            );
            let _ = rect.into_styled(style).draw(self.display);
        }

        Ok(())
    }
}

fn prepare_angle_screen(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    ui_state: &mut UiState,
) {
    display.clear_buffer();
    ui_state.screen_mode = Some(ScreenMode::Angles);
    ui_state.invalidate_dynamic_fields();

    let title_style = MonoTextStyle::new(&FONT_10X20, BinaryColor::Off);
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let _ = Text::new("SCL3300", Point::new(12, 20), title_style).draw(display);
    let _ = Text::new("angles in deg", Point::new(12, 108), body_style).draw(display);
}

fn draw_angle_screen(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    ui_state: &mut UiState,
    readings: AngleReadings,
    refresh_count: u32,
) -> bool {
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let x_line = format_axis_line('X', readings.x_raw);
    let y_line = format_axis_line('Y', readings.y_raw);
    let z_line = format_axis_line('Z', readings.z_raw);

    let mut needs_flush = false;
    needs_flush |= update_text_field(
        display,
        &mut ui_state.x_text,
        &x_line,
        12,
        44,
        140,
        12,
        12,
        52,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.y_text,
        &y_line,
        12,
        60,
        140,
        12,
        12,
        68,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.z_text,
        &z_line,
        12,
        76,
        140,
        12,
        12,
        84,
        body_style,
    );
    needs_flush |= draw_status_overlay(display, ui_state, refresh_count);

    needs_flush
}

fn prepare_error_screen(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    ui_state: &mut UiState,
) {
    display.clear_buffer();
    ui_state.screen_mode = Some(ScreenMode::Error);
    ui_state.invalidate_dynamic_fields();

    let title_style = MonoTextStyle::new(&FONT_10X20, BinaryColor::Off);
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let _ = Text::new("SCL3300", Point::new(12, 20), title_style).draw(display);
    let _ = Text::new("sensor error", Point::new(12, 44), body_style).draw(display);
}

fn draw_sensor_error(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    ui_state: &mut UiState,
    error: SensorError,
    refresh_count: u32,
) -> bool {
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let (message, detail) = format_sensor_error(error);

    let mut needs_flush = false;
    needs_flush |= update_text_field(
        display,
        &mut ui_state.error_message,
        message,
        12,
        52,
        200,
        12,
        12,
        60,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.error_detail,
        detail.as_str(),
        12,
        68,
        200,
        12,
        12,
        76,
        body_style,
    );
    needs_flush |= draw_status_overlay(display, ui_state, refresh_count);

    needs_flush
}

fn draw_status_overlay(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    ui_state: &mut UiState,
    refresh_count: u32,
) -> bool {
    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

    let mut refresh_text: String<16> = String::new();
    let _ = write!(&mut refresh_text, "R{:05}", refresh_count % 100_000);

    let mut led_text: String<24> = String::new();
    let _ = write!(&mut led_text, "LED {:>7}us", LED_TASK_US.load(Ordering::Relaxed));

    let mut sensor_text: String<24> = String::new();
    let _ = write!(&mut sensor_text, "SNS {:>7}us", SENSOR_TASK_US.load(Ordering::Relaxed));

    let mut cpu_text: String<24> = String::new();
    let _ = write!(&mut cpu_text, "CPU {:>7}us", CPU_TASK_US.load(Ordering::Relaxed));

    let mut flush_text: String<24> = String::new();
    let _ = write!(&mut flush_text, "FLU {:>7}us", FLUSH_TASK_US.load(Ordering::Relaxed));

    let mut loop_text: String<24> = String::new();
    let _ = write!(&mut loop_text, "LOP {:>7}us", LOOP_TASK_US.load(Ordering::Relaxed));

    let mut needs_flush = false;
    needs_flush |= update_text_field(
        display,
        &mut ui_state.refresh_text,
        refresh_text.as_str(),
        320,
        0,
        80,
        14,
        324,
        10,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.led_text,
        led_text.as_str(),
        220,
        182,
        180,
        12,
        220,
        190,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.sensor_text,
        sensor_text.as_str(),
        220,
        194,
        180,
        12,
        220,
        202,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.cpu_text,
        cpu_text.as_str(),
        220,
        206,
        180,
        12,
        220,
        214,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.flush_text,
        flush_text.as_str(),
        220,
        218,
        180,
        12,
        220,
        226,
        body_style,
    );
    needs_flush |= update_text_field(
        display,
        &mut ui_state.loop_text,
        loop_text.as_str(),
        220,
        230,
        180,
        10,
        220,
        238,
        body_style,
    );

    needs_flush
}

fn update_text_field<const N: usize>(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    cache: &mut String<N>,
    new_text: &str,
    clear_x: i32,
    clear_y: i32,
    clear_width: u32,
    clear_height: u32,
    text_x: i32,
    text_y: i32,
    style: MonoTextStyle<'_, BinaryColor>,
) -> bool {
    if cache.as_str() == new_text {
        return false;
    }

    clear_rect(display, clear_x, clear_y, clear_width, clear_height);
    if !new_text.is_empty() {
        let _ = Text::new(new_text, Point::new(text_x, text_y), style).draw(display);
    }

    cache.clear();
    let _ = cache.push_str(new_text);
    true
}

fn clear_rect(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) {
    let clear_style = PrimitiveStyle::with_fill(BinaryColor::On);
    let _ = Rectangle::new(Point::new(x, y), Size::new(width, height))
        .into_styled(clear_style)
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

fn format_sensor_error(error: SensorError) -> (&'static str, String<32>) {
    let mut detail: String<32> = String::new();

    let message = match error {
        SensorError::Spi => "SPI transfer failed",
        SensorError::Crc => "CRC mismatch",
        SensorError::UnexpectedResponse => "off-frame mismatch",
        SensorError::StartupStatus(0) => "startup in progress",
        SensorError::StartupStatus(1) => "status says ok",
        SensorError::StartupStatus(3) => "status flag set",
        SensorError::StartupStatus(_) => "status reserved",
        SensorError::WhoAmI(value) => {
            let _ = write!(&mut detail, "whoami=0x{:04X}", value);
            "WHOAMI mismatch"
        }
    };

    (message, detail)
}

fn elapsed_micros_u32(started: Instant) -> u32 {
    started.elapsed().as_micros().min(u32::MAX as u64) as u32
}

fn flush_or_keepalive(
    display: &mut MemoryDisplay<DisplaySpi<'_>, Output<'static>, TiedHighDisplayPin>,
    ui_state: &mut UiState,
    needs_flush: bool,
) -> u32 {
    if needs_flush {
        let flush_started = Instant::now();
        display.flush_buffer();
        ui_state.last_flush_at = Instant::now();
        elapsed_micros_u32(flush_started)
    } else if ui_state.last_flush_at.elapsed() >= Duration::from_millis(500) {
        display.display_mode();
        ui_state.last_flush_at = Instant::now();
        0
    } else {
        0
    }
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
