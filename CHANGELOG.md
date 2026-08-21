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
- Capture writes a numbered run of FITS files rather than a SER video. The
  name of the first file sets where the numbering starts and how wide it is,
  so `light_0001.fits` is followed by `light_0002.fits`; existing files are
  skipped, never overwritten. The number of frames can be left open, and a
  delay between exposures spaces the run out — a one second exposure with a
  two second delay keeps one frame every three seconds. SER is still
  available from the CLI with `--format ser`.
- The live view says when it is stopped. A still picture could mean stopped,
  stalled, lost or connecting, and all four looked the same. The frame itself
  is left untouched — it is usually the reason the view was stopped — and a
  "Clear image" button removes it when it is no longer wanted.
- A preview white balance, on by default: each colour channel is stretched
  against its own histogram so the live view stays neutral wherever the camera
  is pointed, which is what other capture software does and what makes a live
  view usable for framing. It never touches recordings, the gains it applies
  are shown beside the toggle, and the live view is marked while it is on.
- FITS stills record the camera's white balance gains (`WB_R`, `WB_G`,
  `WB_B`). These cameras apply the gains to the raw data before any software
  sees it and store nothing to say so, which leaves a file that cannot be
  un-balanced in processing. SharpCap solves the same problem by writing a
  settings file beside each capture.
- Automatic white balance measures the picture and corrects the camera's own
  gains, so captures come out balanced too. SVBONY's own "white balance once"
  call is not used: it writes the same fixed triple whatever the camera is
  pointed at.

### Notes

- The SVBONY SDK remembers exposure, gain and white balance in a file in the
  working directory, and the camera itself remembers nothing — it powers up at
  about 1 ms exposure with a factory white balance. That mechanism is left
  enabled, warts and all, because it is the only persistence there is. The
  files it writes are gitignored.

### Fixed

- Frames could be saved with a header that did not describe them. A frame
  already integrating when a setting changed finishes under the old value,
  and every frame was stamped with the settings read back afterwards — so a
  file could say `OFFSET=50` over pixels exposed at 0. Observed on an SV305C
  Pro, and it cost an evening: it made a working offset control look broken.
  Frames now carry whether they describe themselves, the live view shows them
  regardless, and anything that writes a file waits for one that does.

- Control sliders showed each control's default instead of what the camera
  actually held: only exposure, gain and offset were ever read back. A camera
  keeps white balance, gamma and the rest between sessions, so the UI could
  claim neutral white balance while the frames carried a heavy colour cast —
  which is exactly what happened on an SV305C Pro carrying gains left by
  other software. Every control's value now comes from the camera. Each one
  also gets a button to put it back to its default, plus a "Reset all".
- A colour cast can be cancelled for the preview ("Neutralise colour"), but
  it is off by default: a preview that quietly corrects the picture stops you
  noticing that the camera needs setting up. The SVBONY backend warns on
  connect when a camera carries non-default white balance.
- Frame conversion ran on the UI thread, so a 1920x1080 camera made the
  window crawl. It now happens on a renderer thread, sized to the panel.
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
