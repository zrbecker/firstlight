//! Tests that need a real camera plugged in.
//!
//! Ignored by default, because most machines have no camera. Run them with
//! one attached:
//!
//! ```sh
//! cargo test -p firstlight-svbony --features sdk -- --ignored --test-threads=1
//! ```
//!
//! These exist for the things a mock cannot answer: whether the hardware
//! actually honours a call, and whether it honours it at the moment the
//! application makes it.

#![cfg(feature = "sdk")]

use std::time::Duration;

use firstlight_core::camera::{Backend, Camera};
use firstlight_core::control::ControlId;
use firstlight_core::frame::{Frame, PixelFormat};
use firstlight_svbony::SvbonyBackend;

const TIMEOUT: Duration = Duration::from_secs(10);

fn open() -> Box<dyn Camera> {
    let backend = SvbonyBackend::new();
    let cameras = backend.enumerate().expect("enumeration");
    let info = cameras.first().expect("a camera to be plugged in");
    backend.open(&info.id).expect("opening the camera")
}

/// Mean of each Bayer channel, using the frame's own reported phase.
fn channel_means(frame: &Frame) -> [f64; 3] {
    let Some(pattern) = frame.meta.format.bayer() else {
        panic!("expected a Bayer frame, got {}", frame.meta.format);
    };
    let mut sums = [0f64; 3];
    let mut counts = [0f64; 3];
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let channel = pattern.channel_at(x, y);
            sums[channel] += f64::from(frame.sample(x, y, 0).unwrap_or(0));
            counts[channel] += 1.0;
        }
    }
    [
        sums[0] / counts[0].max(1.0),
        sums[1] / counts[1].max(1.0),
        sums[2] / counts[2].max(1.0),
    ]
}

/// Grab a frame, discarding ones that were already in flight before the last
/// change was made.
fn settled_frame(camera: &mut dyn Camera) -> Frame {
    for _ in 0..4 {
        let _ = camera.next_frame(TIMEOUT);
    }
    camera.next_frame(TIMEOUT).expect("a frame")
}

#[test]
#[ignore = "needs a camera attached"]
fn white_balance_applies_while_the_stream_is_running() {
    // The case the GUI is always in and the CLI never was: the CLI applies
    // settings and then starts streaming, so a control that only takes effect
    // between streams would look like it worked there and be dead in the app.
    let mut camera = open();
    assert!(
        camera.info().pixel_format != PixelFormat::Mono,
        "this test needs a colour camera"
    );
    camera.set_exposure_us(20_000).unwrap();
    camera.start_streaming().unwrap();

    camera.set_control(ControlId::WbRed, 40).unwrap();
    let low = channel_means(&settled_frame(camera.as_mut()));

    camera.set_control(ControlId::WbRed, 400).unwrap();
    let high = channel_means(&settled_frame(camera.as_mut()));

    let ratio_low = low[0] / low[1].max(1.0);
    let ratio_high = high[0] / high[1].max(1.0);
    println!("R/G at wb_red=40: {ratio_low:.2}, at wb_red=400: {ratio_high:.2}");
    assert!(
        ratio_high > ratio_low * 2.0,
        "white balance did not take effect mid-stream: R/G went from \
         {ratio_low:.2} to {ratio_high:.2}"
    );

    // And the camera reports what it was told, so the UI can show it.
    assert_eq!(camera.control(ControlId::WbRed).unwrap(), 400);
}

#[test]
#[ignore = "needs a camera attached"]
fn gain_applies_while_the_stream_is_running() {
    let mut camera = open();
    camera.set_exposure_us(20_000).unwrap();
    camera.set_gain(0).unwrap();
    camera.start_streaming().unwrap();

    let dark = channel_means(&settled_frame(camera.as_mut()));
    camera.set_gain(300).unwrap();
    let bright = channel_means(&settled_frame(camera.as_mut()));

    let (dark, bright) = (dark[1], bright[1]);
    println!("green mean at gain 0: {dark:.0}, at gain 300: {bright:.0}");
    assert!(
        bright > dark * 1.5,
        "gain did not take effect mid-stream: {dark:.0} -> {bright:.0}"
    );
}

#[test]
#[ignore = "needs a camera attached"]
fn automatic_white_balance_balances_the_frames() {
    let mut camera = open();
    camera.set_exposure_us(20_000).unwrap();
    camera.start_streaming().unwrap();

    camera.auto_white_balance().unwrap();
    let means = channel_means(&settled_frame(camera.as_mut()));
    let (r, g, b) = (means[0], means[1], means[2]);
    println!("after auto white balance: R={r:.0} G={g:.0} B={b:.0}");
    // Whatever it is pointed at, the three channels should end up in the same
    // ballpark; a cast is what this call exists to remove.
    assert!(
        r / g > 0.5 && r / g < 2.0 && b / g > 0.5 && b / g < 2.0,
        "still unbalanced: R/G={:.2} B/G={:.2}",
        r / g,
        b / g
    );
}
