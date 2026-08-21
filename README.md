# FirstLight

[![CI](https://github.com/zrbecker/firstlight/actions/workflows/ci.yml/badge.svg)](https://github.com/zrbecker/firstlight/actions/workflows/ci.yml)

A cross-platform (macOS-first) capture library and desktop application for
astronomy cameras, in Rust.

> **Status: early.** Everything builds and is tested on macOS and Linux, but
> the Touptek backend has not yet run against a real camera. See
> [What is verified, and what is not](#what-is-verified-and-what-is-not).

The design goal is boring reliability at 3am: **nothing blocks forever and
nothing fails silently.** Every waiting call takes a deadline, every failure
mode a USB camera can produce has its own error variant, and the GUI keeps
running when the hardware does not.

```
firstlight/
├── crates/firstlight-core      the Camera trait, frames, SER/FITS writers, worker thread, simulator
├── crates/firstlight-svbony    SVBONY SDK backend (SV305 series and relatives)
├── crates/firstlight-touptek   Touptek SDK backend (Altair, Omegon, RisingCam, Bresser, ...)
├── crates/firstlight-cli       command line harness: list, capture, snap, watch
└── crates/firstlight-view      egui desktop app; the binary is called `firstlight`
```

## Quick start

No camera and no vendor SDK are needed — a simulated camera is built in by
default:

```sh
cargo run -p firstlight-cli -- list
cargo run -p firstlight-cli -- capture -e 5ms -n 200 -o run/light_0001.fits
cargo run -p firstlight-cli -- capture -e 1s --delay 2s -o run/light_0001.fits
cargo run -p firstlight-cli -- snap  -e 2s --bits 16 -o m42.fits
cargo run -p firstlight-view          # the GUI, binary name `firstlight`
```

With real hardware, turn on the backend for your camera. **SVBONY needs
nothing installed** — the SDK is downloaded and checked automatically the
first time you build:

```sh
cargo run -p firstlight-view --features svbony
cargo run -p firstlight-cli --features svbony -- list
```

Touptek's SDK has no stable download URL to pin, so that one is still a manual
drop; see [`crates/firstlight-touptek/vendor/README.md`](crates/firstlight-touptek/vendor/README.md).

```sh
FIRSTLIGHT_TOUPTEK_SDK_DIR=~/sdk/toupcam cargo run -p firstlight-view --features touptek
```

**Which backend?** SVBONY sells cameras of both kinds. The SV305 series
enumerates under SVBONY's own USB vendor id and needs `--features svbony`;
their Touptek-based models need `--features touptek`. Build with both if you
are not sure — a backend that sees nothing says so instead of failing.

## Architecture

### The `Camera` trait

`firstlight-core` defines one trait per open device and one per SDK:

```rust
pub trait Backend: Send + Sync {
    fn enumerate(&self) -> Result<Vec<CameraInfo>>;
    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>>;
}

pub trait Camera: Send {
    fn connect(&mut self) -> Result<()>;
    fn set_control(&mut self, id: ControlId, value: i64) -> Result<()>;
    fn set_roi(&mut self, roi: Roi) -> Result<()>;
    fn start_streaming(&mut self) -> Result<()>;
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame>;
    fn events(&self) -> Receiver<CameraEvent>;
    // ...
}
```

Three decisions shape everything else:

* **Geometry is separate from the control table.** ROI, binning and bit depth
  get their own methods because every SDK models them differently, while
  exposure/gain/offset/white-balance are numeric controls with ranges the
  camera advertises. A GUI builds its sliders from `controls()` without
  knowing which backend it is talking to.
* **Every blocking call takes a deadline.** `next_frame(timeout)` returning
  `Error::Timeout` is a normal outcome during a long exposure, not a failure.
* **Asynchronous trouble arrives on a channel.** Unplug, USB stall, dropped
  frames and the SDK's own watchdogs come through `events()` rather than being
  discovered by a call that never returns.

Adding a ZWO (ASI) or QHY backend means implementing those two traits and
registering the backend; nothing in the CLI or the GUI changes.

### Frame path

Every backend has the same problem: the SDK delivers frames on its own thread
and the consumer may be slower. Blocking the producer inside a vendor callback
stalls the USB pipe; an unbounded queue turns a slow consumer into an
out-of-memory crash. So:

```
SDK callback ──▶ channel ──▶ pump thread ──▶ FrameRing (bounded, drop-oldest) ──▶ next_frame
```

`FrameRing` never blocks its producer, discards the *oldest* frame when full
so the live view stays current, counts every loss, and carries the reason a
stream ended — so a consumer blocked in `recv_timeout` finds out about a
device loss immediately instead of waiting out its timeout.

### The worker thread

`firstlight-core::worker::WorkerHandle` owns a camera on its own thread and
talks in commands and status. The GUI never touches a camera directly, which
is why an unplug, a stalled pipe or a 300 ms control write cannot freeze the
window. Two channels leave the worker, and the split matters:

* status and events go out on an unbounded queue — losing an error report is
  unacceptable and the messages are tiny;
* frames go into a one-deep ring — a live view wants the *newest* frame, and a
  backlog of stale ones is worse than useless.

Recording writes happen *before* the display hand-off, so a slow or hidden UI
can never punch holes in a capture. The status distinguishes the two kinds of
loss: `camera_dropped` (the backend outran the worker) and `display_dropped`
(the worker outran the UI; those frames were still recorded).

### File formats

Both writers take frames exactly as the camera produced them. Nothing
debayers, stretches or rescales on the way to a file — that belongs to the
display path only.

* **SER v3** for video, written incrementally. The frame count is patched in
  on `finish()`, and the writer finalises itself on drop, so an interrupted
  capture is still a readable file.
* **FITS** for stills, via a small dependency-free writer: one primary HDU, 8
  or 16 bit big-endian data with `BZERO`, and the acquisition keywords
  stacking software looks for (`DATE-OBS`, `EXPTIME`, `GAIN`, `OFFSET`,
  `XBINNING`, `BAYERPAT`, `ROWORDER`, …). Avoiding `fitsio` keeps cfitsio out
  of the build, which matters for shipping a macOS app bundle.

### Display path

`firstlight-core::display` does nearest-neighbour debayering, percentile
auto-stretch and gamma on a *copy*. The signature (`&Frame` in, RGBA buffer
out) makes it structurally impossible for display processing to reach a file.

## The GUI

* Left panel: camera picker, connect, control sliders built from the camera's
  own control table, ROI/binning/bit-depth dropdowns, display options,
  statistics.
* Centre: live view as an egui texture, with fps and dropped-frame counters
  and a loud banner when the device is lost, reconnecting, or has stopped
  delivering frames.
* Bottom: an event log with timestamps.
* Snap-to-FITS and record-to-SER buttons; recording is independent of the
  display path.
* Auto-stretch toggle, gamma, and a debayer toggle (off shows the raw mosaic,
  which makes a wrong Bayer phase obvious).

Slider drags are coalesced: control writes go over USB, so values are sent at
most every 120 ms during a drag and always once more when it ends.

## Failure handling

| What happens | What you get |
| --- | --- |
| No frame within the deadline | `Error::Timeout`, stream still alive |
| Camera unplugged | `Error::DeviceLost` + `CameraEvent::DeviceLost`, handle closed, auto-reconnect starts |
| USB pipe stalls | `Error::UsbStall` (distinct from a timeout — it needs a re-open) |
| Camera already open elsewhere | `Error::Busy`, with a note about permissions |
| Value the sensor rejects | `Error::OutOfRange` with the range, before it reaches USB |
| Consumer too slow | Oldest frames dropped and counted, never queued unboundedly |
| Camera stops delivering | Reported as stalled; the UI says so instead of showing a frozen image |

After a reconnect the worker restores bit depth, binning, ROI and every
control the user had set, and restarts the stream if it was running — so a
replug is invisible apart from the gap.

## Vendor SDKs

Neither vendor SDK carries a licence. Not "a restrictive licence" — no
copyright notice in the header, no `LICENSE` in the archive, nothing on either
vendor's site. The default position for software with no written grant is all
rights reserved, so **this repository redistributes neither of them**, however
common it is elsewhere.

That does not have to mean manual steps. `firstlight-svbony`'s build script
fetches what it needs:

| Piece | Source | Checked |
| --- | --- | --- |
| `SVBCameraSDK.h` | INDIGO's vendored copy, pinned commit | SHA-256 |
| `libSVBCameraSDK.dylib` (macOS, universal) | INDIGO's vendored copy, pinned commit | SHA-256 |
| `libSVBCameraSDK.so` (Linux x86-64 / arm64) | indi-3rdparty, pinned commit | SHA-256 |
| libusb (macOS only) | system copy if present, else built from the pinned upstream release | SHA-256 |

Everything is cached under the user's cache directory, so it happens once per
machine. `FIRSTLIGHT_SVBONY_SDK_DIR` points the build at a local SDK instead,
`FIRSTLIGHT_SDK_CACHE` moves the cache, and `FIRSTLIGHT_OFFLINE=1` refuses to
download and says what to fetch by hand.

macOS has no system libusb and the SVBONY library needs one, so the build
takes a Homebrew or `/usr/local` copy if there is one and otherwise compiles
libusb 1.0.27 from source — it ships an `Xcode/config.h` for exactly this, so
it is a nine-file compile rather than a build system. libusb is LGPL-2.1 and
its licence text is written next to the built library.

## Testing

```sh
cargo test --workspace                                  # 84 tests, no hardware needed
cargo test -p firstlight-touptek --features mock-sdk    # 12 more, exercising the FFI layer
cargo clippy --workspace --all-targets
```

The simulator backend (`firstlight-core::simulator`) is a synthetic star field
with a fault-injection handle: `unplug()`, `replug()`, `stall_usb()`,
`freeze_frames()`, `set_busy()`, `set_control_latency()`. That is what makes
the interesting paths testable — the ones that otherwise need somebody
standing by the telescope pulling a USB cable.

The GUI is tested headlessly: egui runs a frame with no window, so
`crates/firstlight-view/tests/headless.rs` drives the real widgets against a
simulated camera and asserts, among other things, that a UI frame still
completes in under 100 ms while the camera is unplugged, stalled, or in the
middle of a slow control write.

### What is verified, and what is not

Everything above runs in CI without hardware. The Touptek backend is the
exception worth being explicit about:

* Its error mapping, event mapping and unit conversions are unit-tested.
* Its FFI layer — callback, pull path, teardown ordering, HRESULT mapping —
  compiles and runs against a mock camera written in C (`crates/firstlight-touptek/mock/`)
  under `--features mock-sdk`.

The SVBONY backend is in better shape: its mock compiles against the vendor's
own header, and it has been run against a real SV305C Pro — enumeration,
control table, ROI, binning, 8- and 16-bit capture, SER recording and FITS
stills all verified on the hardware. One thing that measurement changed: the
SDK left-aligns its 16-bit output, so frames are reported as 16-bit rather
than the sensor's 12, which is what stops a linear display clipping to white.
* **It has not been built against the real vendor header, or run against real
  hardware.** The mock is written from the vendor's published API and is only
  as accurate as that reading. Every symbol it calls does exist in a shipping
  `libtoupcam.dylib` (checked against the copy INDIGO distributes), but that
  says nothing about behaviour. A first run should be treated as exactly that.

## Platform notes

* **macOS** — the primary target. The build script adds rpaths for
  `@loader_path` and `@executable_path/../Frameworks`, so `libtoupcam.dylib`
  can ship inside an app bundle.
* **Linux** — the SDK needs its udev rule installed before a non-root user can
  open a camera; without it you get `Error::Busy` mentioning permissions.
* **Windows** — the FFI accounts for 16-bit `wchar_t` and the `__stdcall`
  callback ABI, but is otherwise untested.

## Licence

MIT. See [LICENSE](LICENSE).
