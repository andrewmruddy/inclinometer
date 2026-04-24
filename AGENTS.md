# AGENTS.md

This file gives coding agents the minimum project context needed to work safely in this repository.

## Project Summary

- Target hardware: Adafruit Feather nRF52840 Express
- Display: Adafruit Sharp Memory Display breakout using the `sharp-memory-display` crate with the `ls027b7dh01` feature
- Debug/flash hardware: Black Magic Probe for GDB debugging, plus `probe-rs` configured as the default Cargo runner
- Language/runtime: Rust `no_std` firmware using Embassy async on `thumbv7em-none-eabihf`

The current firmware lives entirely in `src/main.rs`. It currently:

- initializes Embassy on the Feather nRF52840
- toggles the onboard LED in one Embassy task
- uses a second Embassy task that owns the shared SPI bus
- reads the Murata SCL3300 and updates the Sharp display from that same bus-owning task
- renders live SCL3300 X/Y/Z angle values on the display

## Important Files

- `src/main.rs`: firmware entry point, Embassy tasks, SPI/display setup, pin assignments
- `Cargo.toml`: dependencies and target profile settings
- `.cargo/config.toml`: default target and `probe-rs` runner
- `memory.x`: linker memory layout for the nRF52840
- `build.rs`: adds the linker search path and rebuild trigger for `memory.x`
- `debug.md`: Black Magic Probe GDB attach flow

## Build And Check

Common commands:

- `cargo check`
- `cargo build`
- `cargo run`

Notes:

- `cargo run` uses the runner configured in `.cargo/config.toml`:
  `probe-rs run --chip nRF52840_xxAA --protocol swd`
- The default target is already set to `thumbv7em-none-eabihf`
- This repo currently has no dedicated test suite; `cargo check` is the fastest validation step

## Debugging

There are two debugging paths in the repo:

1. Default flash/run path via `probe-rs` and `.cargo/config.toml`
2. Black Magic Probe GDB flow documented in `debug.md`

Current `debug.md` flow:

- open `arm-none-eabi-gdb target/thumbv7em-none-eabihf/debug/nRF52840`
- connect to `/dev/cu.usbmodemC40BA8F31`
- run `monitor auto_scan`
- `attach 1`

If the BMP serial device changes, update the device path before relying on those commands.

## Hardware Notes

Current pin usage from `src/main.rs`:

- `TWISPI0`: shared SPI peripheral for Sharp display and SCL3300
- `P0_14`: shared SPI SCK
- `P0_13`: shared SPI MOSI / Sharp display DI / SCL3300 MOSI
- `P0_15`: shared SPI MISO, used by the SCL3300
- `P0_03`: Sharp display chip select (`A5`)
- `P0_28`: SCL3300 CSB (`A3`)
- `P1_15`: onboard LED

Display and sensor wiring notes:

- Sharp display `CS` is on `A5`
- Sharp display `EMD` / `EXTMODE` is wired to `A4`
- Sharp display `DISP` is held high in hardware
- Sharp display `EIN` / `EXTCOMIN` is held low in hardware
- SCL3300 `CSB` is on `A3`
- current code assumes the SCL3300 shares `SCK` and `MOSI` with the display and uses `P0_15` as the shared `MISO` line

nRF52840 Feather note:

- avoid `SPIM3` for external SPI devices if the board may boot without USB/VBUS power
- sharing `SPIM0/TWISPI0` between the display and SCL3300 is acceptable because both devices use SPI mode 0 and separate chip-select lines
- a mutex would be appropriate if multiple independent tasks touched the shared bus, but the current implementation keeps sensor and display traffic inside one bus-owning task because the display driver performs multi-transfer transactions under one chip-select window

When changing wiring or board definitions, update both the firmware comments and this file.

## Code Guidance

- Keep the firmware `no_std` and Embassy-based unless the user asks for a broader refactor
- Prefer small, explicit async tasks instead of adding blocking loops
- Be careful with nRF52840 pin remaps; this project currently documents pin intent inline in `src/main.rs`
- Preserve the existing `defmt`/panic-related dependencies unless there is a clear reason to remove or reconfigure them
- If you add display drawing code, prefer `embedded-graphics` primitives and keep framebuffer flush behavior explicit

## Agent Workflow Expectations

Before making changes:

- read `src/main.rs`, `Cargo.toml`, and `.cargo/config.toml`
- check whether the user is asking for `probe-rs` flashing or Black Magic Probe debugging, because the repo supports both

After making changes:

- run `cargo check`
- if startup, pin mapping, or flashing/debug behavior changed, update this file and `debug.md` when applicable

## Things To Avoid

- Do not assume a generic nRF52840 dev board pinout; use the current Feather wiring in the source
- Do not remove the target configuration from `.cargo/config.toml`
- Do not silently change the flashing/debugging workflow; call it out clearly if you switch between `probe-rs` and Black Magic Probe expectations
