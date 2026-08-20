//! Exercises the FFI layer against the mock camera in `mock/`.
//!
//! Run with `cargo test -p firstlight-touptek --features mock-sdk`.
//!
//! What this proves: the unsafe code compiles, the vendor callback reaches
//! our channel, pulls produce correctly shaped frames, HRESULTs map to the
//! right errors, and teardown does not use the callback context after it is
//! freed. What it cannot prove: that the vendor's real signatures and event
//! semantics match the mock. Only a build against the real SDK, on real
//! hardware, does that.

#![cfg(feature = "mock-sdk")]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use firstlight_core::camera::{Backend, Camera, CameraId};
use firstlight_core::control::{Binning, BitDepth, ControlId, Roi};
use firstlight_core::{BayerPattern, CameraEvent, Error, PixelFormat};
use firstlight_touptek::{TouptekBackend, mock};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The mock is one global camera, so tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> (MutexGuard<'static, ()>, TouptekBackend) {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    mock::reset();
    (guard, TouptekBackend::new())
}

fn open(backend: &TouptekBackend) -> Box<dyn Camera> {
    backend
        .open(&CameraId::new("mock-0"))
        .unwrap_or_else(|e| panic!("opening the mock camera: {e}"))
}

fn next_frame(camera: &mut dyn Camera) -> firstlight_core::Result<firstlight_core::Frame> {
    camera.next_frame(TIMEOUT)
}

#[test]
fn enumeration_reads_the_model_record() {
    let (_guard, backend) = setup();
    let cameras = backend.enumerate().unwrap();
    assert_eq!(cameras.len(), 1);

    let camera = &cameras[0];
    assert_eq!(camera.id.as_str(), "mock-0", "wide string decoding");
    assert_eq!(camera.display_name, "Mock Camera");
    assert_eq!(camera.model, "Mock Camera", "model name via pointer");
    assert_eq!((camera.max_width, camera.max_height), (64, 48));
    assert!((camera.pixel_size_um - 2.9).abs() < 0.01);
    // The model flags say RAW8/12/16 and no MONO bit.
    assert_eq!(
        camera.bit_depths,
        vec![BitDepth::EIGHT, BitDepth::TWELVE, BitDepth::SIXTEEN]
    );
    assert!(camera.pixel_format.is_colour());
    assert!(!camera.has_cooler);
}

#[test]
fn opening_fills_in_the_serial_and_the_control_ranges() {
    let (_guard, backend) = setup();
    let camera = open(&backend);

    assert_eq!(camera.info().serial, "MOCK-SERIAL-0001");
    // The Bayer phase comes from get_RawFormat, not from enumeration.
    assert_eq!(
        camera.pixel_format().unwrap(),
        PixelFormat::Bayer(BayerPattern::Rggb)
    );

    let controls = camera.controls().unwrap();
    let exposure = controls
        .iter()
        .find(|c| c.id == ControlId::ExposureUs)
        .expect("exposure control");
    assert_eq!((exposure.min, exposure.max), (32, 60_000_000));
    let gain = controls
        .iter()
        .find(|c| c.id == ControlId::Gain)
        .expect("gain control");
    assert_eq!((gain.min, gain.max), (100, 1000));
}

#[test]
fn a_second_open_is_reported_as_busy() {
    let (_guard, backend) = setup();
    let _first = open(&backend);
    let second = backend.open(&CameraId::new("mock-0")).map(|_| ());
    assert!(matches!(second, Err(Error::Busy(_))), "got {second:?}");
}

#[test]
fn controls_round_trip_through_the_sdk() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);

    camera.set_exposure_us(12_345).unwrap();
    assert_eq!(camera.exposure_us().unwrap(), 12_345);
    camera.set_gain(400).unwrap();
    assert_eq!(camera.gain().unwrap(), 400);
    camera.set_offset(7).unwrap();
    assert_eq!(camera.offset().unwrap(), 7);

    // Out of range never reaches the SDK.
    assert!(matches!(
        camera.set_gain(100_000),
        Err(Error::OutOfRange { .. })
    ));
    assert!((camera.temperature_c().unwrap() - 21.5).abs() < 0.01);
}

#[test]
fn the_callback_and_pull_path_delivers_frames() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.set_bit_depth(BitDepth::SIXTEEN).unwrap();
    camera.start_streaming().unwrap();

    let first = next_frame(camera.as_mut()).unwrap();
    assert_eq!((first.width(), first.height()), (64, 48));
    assert_eq!(first.data.len(), 64 * 48 * 2);
    assert!(first.to_u16().iter().any(|&v| v > 0));

    let second = next_frame(camera.as_mut()).unwrap();
    assert!(second.meta.sequence > first.meta.sequence);

    camera.stop_streaming().unwrap();
    // After stopping, waiting is an error rather than a hang.
    let started = Instant::now();
    assert!(matches!(
        camera.next_frame(Duration::from_secs(10)),
        Err(Error::NotStreaming)
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn eight_bit_mode_changes_the_frame_size() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.set_bit_depth(BitDepth::EIGHT).unwrap();
    camera.start_streaming().unwrap();

    let frame = next_frame(camera.as_mut()).unwrap();
    assert_eq!(frame.meta.bit_depth, BitDepth::EIGHT);
    assert_eq!(frame.data.len(), 64 * 48);
}

#[test]
fn roi_and_binning_reach_the_sdk_and_come_back_in_the_frames() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);

    camera.set_roi(Roi::new(8, 8, 32, 16)).unwrap();
    assert_eq!(camera.roi().unwrap(), Roi::new(8, 8, 32, 16));
    camera.start_streaming().unwrap();
    let frame = next_frame(camera.as_mut()).unwrap();
    assert_eq!((frame.width(), frame.height()), (32, 16));

    // An ROI off the end of the sensor is refused before the stream restarts.
    assert!(matches!(
        camera.set_roi(Roi::new(0, 0, 4096, 4096)),
        Err(Error::InvalidGeometry(_))
    ));
    assert!(
        camera.is_streaming(),
        "a rejected ROI must not kill the stream"
    );

    camera.set_binning(Binning(2)).unwrap();
    let binned = next_frame(camera.as_mut()).unwrap();
    assert_eq!((binned.width(), binned.height()), (32, 24));
    assert_eq!(binned.meta.binning, Binning(2));
}

#[test]
fn an_unplug_reaches_both_the_frame_path_and_the_event_channel() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    let events = camera.events();
    camera.start_streaming().unwrap();
    next_frame(camera.as_mut()).unwrap();

    mock::unplug();

    let mut saw_loss = false;
    for _ in 0..20 {
        match camera.next_frame(Duration::from_millis(500)) {
            Err(Error::DeviceLost(_)) => {
                saw_loss = true;
                break;
            }
            Err(e) => panic!("expected DeviceLost, got {e:?}"),
            Ok(_) => continue,
        }
    }
    assert!(saw_loss, "the frame path never reported the unplug");

    let mut events_seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        events_seen.push(event);
    }
    assert!(
        events_seen
            .iter()
            .any(|e| matches!(e, CameraEvent::DeviceLost { .. })),
        "saw {events_seen:?}"
    );
    assert!(!camera.is_connected());

    // Cleanup on a dead device must still work, and must not use the
    // callback context after it is freed.
    camera.stop_streaming().unwrap();
    camera.disconnect().unwrap();
    assert!(backend.enumerate().unwrap().is_empty());

    // And the camera can be opened again once it comes back.
    mock::replug();
    let opens_before = mock::open_count();
    let mut reopened = open(&backend);
    assert_eq!(mock::open_count(), opens_before + 1);
    reopened.start_streaming().unwrap();
    assert!(next_frame(reopened.as_mut()).is_ok());
}

#[test]
fn a_stalled_pipe_is_reported_as_a_stall() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    let events = camera.events();
    camera.start_streaming().unwrap();
    next_frame(camera.as_mut()).unwrap();

    mock::stall();

    let mut saw_stall = false;
    for _ in 0..20 {
        match camera.next_frame(Duration::from_millis(500)) {
            Err(Error::UsbStall(_)) => {
                saw_stall = true;
                break;
            }
            Err(e) => panic!("expected UsbStall, got {e:?}"),
            Ok(_) => continue,
        }
    }
    assert!(saw_stall, "the stall was never reported");
    let mut events_seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        events_seen.push(event);
    }
    assert!(
        events_seen
            .iter()
            .any(|e| matches!(e, CameraEvent::UsbStall { .. })),
        "saw {events_seen:?}"
    );
}

#[test]
fn a_camera_that_stops_delivering_produces_timeouts_not_hangs() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.start_streaming().unwrap();
    next_frame(camera.as_mut()).unwrap();

    mock::freeze(true);
    while camera.next_frame(Duration::from_millis(200)).is_ok() {}
    let started = Instant::now();
    let result = camera.next_frame(Duration::from_millis(400));
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));

    mock::freeze(false);
    assert!(next_frame(camera.as_mut()).is_ok(), "frames should resume");
}

#[test]
fn a_failing_sdk_call_is_mapped_rather_than_swallowed() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);

    // E_INVALIDARG from put_Option: a rejected value, not a dead device.
    mock::fail_next_option(0x8007_0057u32 as i32);
    let result = camera.set_control(ControlId::Offset, 5);
    assert!(
        matches!(result, Err(Error::InvalidGeometry(_))),
        "got {result:?}"
    );
    assert!(
        camera.is_connected(),
        "a bad argument must not kill the handle"
    );

    // ERROR_GEN_FAILURE is a stalled pipe and must be reported as one.
    mock::fail_next_option(0x8007_001Fu32 as i32);
    let result = camera.set_control(ControlId::Offset, 5);
    match result {
        Err(e @ Error::UsbStall(_)) => assert!(e.is_fatal()),
        other => panic!("expected UsbStall, got {other:?}"),
    }
}

#[test]
fn dropping_a_streaming_camera_shuts_the_sdk_down_cleanly() {
    let (_guard, backend) = setup();
    {
        let mut camera = open(&backend);
        camera.start_streaming().unwrap();
        next_frame(camera.as_mut()).unwrap();
        // Dropped while streaming: the SDK must be stopped and closed before
        // the callback context goes away, or this is a use-after-free.
    }
    // If teardown were wrong we would have crashed above; prove the device is
    // usable again afterwards.
    let mut again = open(&backend);
    again.start_streaming().unwrap();
    assert!(next_frame(again.as_mut()).is_ok());
}
