# AGENTS.md

This file gives coding agents the minimum project context needed to work safely in this repository.

## Project Summary

- Target hardware: Adafruit Feather nRF52840 Express
- Display: Adafruit Sharp Memory Display breakout using the `sharp-memory-display` crate with the `ls027b7dh01` feature
- Debug/flash hardware: Black Magic Probe for GDB debugging, plus `probe-rs` configured as the default Cargo runner
- Language/runtime: Rust `no_std` firmware using Embassy async on `thumbv7em-none-eabihf`

The current firmware lives entirely in `src/main.rs`. It:

- configures `TWISPI0` as a TX-only SPI bus
- drives the Sharp display over SPI
- toggles the onboard LED in a periodic Embassy task
- renders `"Ruddy Subsea"` once, then refreshes the display periodically

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

- `P0_14`: SPI SCK to Sharp display
- `P0_13`: SPI MOSI / display DI
- `P0_03`: display chip select
- `P1_15`: onboard LED

Display-specific note:

- the display `DISP` line is not software-controlled right now
- firmware uses a dummy `TiedHighDisplayPin`
- the code assumes `DISP` is physically tied high on the hardware

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
