# Changelog

All notable changes to this project are documented here, newest first.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `firstlight-svbony`: a backend over SVBONY's own SDK, covering the SV305
  series, which does not speak the Touptek protocol. The SDK and, on macOS,
  libusb are fetched or built by the build script against pinned hashes, so
  there is nothing to install by hand. Verified against a real SV305C Pro.
- Backends can explain why they see nothing (`Backend::unavailable_reason`),
  which the GUI and CLI both surface.

### Fixed

- Sliders stuttered while dragging, because a status snapshot carrying the
  camera's older value could arrive mid-drag and overwrite the slider.
- The Touptek backend was documented as covering the SVBONY SV305C Pro. It
  does not; that camera uses SVBONY's own SDK.
- `firstlight-cli info` printed the enumeration record rather than the opened
  camera's, showing a 0x0 sensor with no bit depths.
- Vendor SDK libraries had no rpath recorded in the binaries, so a build with
  a hardware backend aborted at startup.

## [0.1.0] — 2026-08-19

First working version. Nothing here has been run against a real camera yet;
see "What is verified, and what is not" in the README.

### Added

- `firstlight-core`: the `Camera` and `Backend` traits, frame and control
  types, an event channel for device-lost and reconnect, and a bounded
  drop-oldest frame ring that carries the reason a stream ended.
- `firstlight-core::worker`: a camera IO thread that talks in commands and
  status, records to SER, saves stills to FITS, and restores the user's
  settings after an unplug.
- Dependency-free SER v3 and FITS writers, and a display path (nearest
  neighbour debayer, percentile auto-stretch, gamma) that operates only on
  copies.
- `firstlight-core::simulator`: a synthetic star field with fault injection
  for unplug, USB stall, frame freeze, busy device and slow control calls.
- `firstlight-touptek`: Touptek SDK backend (Touptek and its rebadges)
  behind the `sdk` feature, with a
  `mock-sdk` feature that compiles and exercises the FFI layer against a small
  C camera so the unsafe code is covered in CI.
- `firstlight-cli`: `list`, `info`, `capture`, `snap` and `watch`.
- `firstlight-view`: the egui application, with headless tests that assert the
  window keeps drawing while the camera is unplugged or stalled.

[Unreleased]: https://github.com/zrbecker/firstlight/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/zrbecker/firstlight/releases/tag/v0.1.0
