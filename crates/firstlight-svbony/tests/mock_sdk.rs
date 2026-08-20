//! Exercises the FFI layer against the mock camera in `mock/`.
//!
//! Run with `cargo test -p firstlight-svbony --features mock-sdk`.
//!
//! The mock compiles against the vendor's own header, so these tests check
//! our Rust against the genuine declarations: struct layouts, enum widths,
//! call order, teardown. They cannot check that a real camera behaves the way
//! the mock does.

#![cfg(feature = "mock-sdk")]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use firstlight_core::camera::{Backend, Camera};
use firstlight_core::control::{Binning, BitDepth, ControlId, Roi};
use firstlight_core::{BayerPattern, CameraEvent, Error, PixelFormat};
use firstlight_svbony::{SvbonyBackend, mock};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The mock is one global camera, so tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> (MutexGuard<'static, ()>, SvbonyBackend) {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    mock::reset();
    (guard, SvbonyBackend::new())
}

fn open(backend: &SvbonyBackend) -> Box<dyn Camera> {
    let id = backend.enumerate().unwrap()[0].id.clone();
    backend
        .open(&id)
        .unwrap_or_else(|e| panic!("opening the mock camera: {e}"))
}

#[test]
fn enumeration_uses_the_serial_as_a_stable_id() {
    let (_guard, backend) = setup();
    let cameras = backend.enumerate().unwrap();
    assert_eq!(cameras.len(), 1);
    let camera = &cameras[0];
    // The SDK's numeric id is a slot that moves; the serial is not.
    assert_eq!(camera.id.as_str(), "MOCK-SN-0001");
    assert_eq!(camera.display_name, "MOCK SV305C PRO");
    assert_eq!(camera.backend, "svbony");
    assert_eq!(camera.reconnect_key(), "MOCK-SN-0001");
}

#[test]
fn connecting_reads_the_sensor_properties() {
    let (_guard, backend) = setup();
    let camera = open(&backend);
    let info = camera.info();

    assert_eq!((info.max_width, info.max_height), (1920, 1080));
    assert_eq!(info.pixel_format, PixelFormat::Bayer(BayerPattern::Grbg));
    assert!((info.pixel_size_um - 2.9).abs() < 0.01);
    assert_eq!(info.binnings, vec![Binning(1), Binning(2)]);
    // 8-bit, and 16-bit: the SDK left-aligns the sensor's 12 bits so the
    // samples fill the whole 16-bit range.
    assert_eq!(info.bit_depths, vec![BitDepth::EIGHT, BitDepth::SIXTEEN]);
    assert!(!info.has_cooler, "the SV305C Pro has no cooler");
    assert_eq!(camera.pixel_format().unwrap(), info.pixel_format);
}

#[test]
fn the_control_table_comes_from_the_camera_including_its_own_labels() {
    let (_guard, backend) = setup();
    let camera = open(&backend);
    let controls = camera.controls().unwrap();

    let gain = controls
        .iter()
        .find(|c| c.id == ControlId::Gain)
        .expect("gain");
    assert_eq!((gain.min, gain.max, gain.default), (0, 450, 10));
    assert_eq!(gain.label, "Gain");

    let exposure = controls
        .iter()
        .find(|c| c.id == ControlId::ExposureUs)
        .expect("exposure");
    assert_eq!((exposure.min, exposure.max), (8, 2_000_000_000));
    assert!(exposure.logarithmic, "six decades needs a log slider");
    assert_eq!(exposure.unit, "us");

    // A control the portable API has no name for still reaches the UI, with
    // the label the camera supplied.
    let speed = controls
        .iter()
        .find(|c| c.label == "Frame speed")
        .expect("frame speed");
    assert!(matches!(speed.id, ControlId::Vendor(_)));
    assert_eq!((speed.min, speed.max), (0, 2));

    // Read-only controls are marked, not hidden.
    let temperature = controls
        .iter()
        .find(|c| c.label == "Current temperature")
        .expect("temperature");
    assert!(temperature.read_only);
}

#[test]
fn controls_round_trip_and_out_of_range_values_never_reach_the_sdk() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);

    camera.set_exposure_us(12_345).unwrap();
    assert_eq!(camera.exposure_us().unwrap(), 12_345);
    camera.set_gain(300).unwrap();
    assert_eq!(camera.gain().unwrap(), 300);
    camera.set_offset(20).unwrap();
    assert_eq!(camera.offset().unwrap(), 20);

    match camera.set_gain(10_000) {
        Err(Error::OutOfRange { max, .. }) => assert_eq!(max, 450),
        other => panic!("expected OutOfRange with the camera's own range, got {other:?}"),
    }
    assert!((camera.temperature_c().unwrap() - 21.5).abs() < 0.01);
}

#[test]
fn usb_bandwidth_is_refused_with_a_pointer_at_the_real_control() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    // This camera family has no bandwidth limit. Silently mapping it onto
    // something else would be worse than saying so.
    let error = camera.set_control(ControlId::UsbBandwidth, 80).unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "got {error:?}");
    assert!(error.to_string().contains("Frame speed"), "{error}");
}

#[test]
fn streaming_delivers_frames_of_the_declared_shape() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.start_streaming().unwrap();

    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (1920, 1080));
    assert_eq!(frame.meta.bit_depth, BitDepth::SIXTEEN);
    assert_eq!(frame.data.len(), 1920 * 1080 * 2);
    assert_eq!(frame.meta.format, PixelFormat::Bayer(BayerPattern::Grbg));
    // Left-aligned: samples reach beyond 12 bits and the low nibble is clear,
    // which is what makes reporting 16 rather than 12 the honest description.
    let samples = frame.to_u16();
    assert!(samples.iter().any(|&v| v > 0x0fff));
    assert!(samples.iter().all(|&v| v & 0xf == 0));

    let second = camera.next_frame(TIMEOUT).unwrap();
    assert!(second.meta.sequence > frame.meta.sequence);

    camera.stop_streaming().unwrap();
    let started = Instant::now();
    assert!(matches!(
        camera.next_frame(Duration::from_secs(10)),
        Err(Error::NotStreaming)
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn eight_bit_mode_halves_the_frame() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.set_bit_depth(BitDepth::EIGHT).unwrap();
    camera.start_streaming().unwrap();

    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!(frame.meta.bit_depth, BitDepth::EIGHT);
    assert_eq!(frame.data.len(), 1920 * 1080);
}

#[test]
fn the_roi_alignment_rule_is_enforced_before_the_sdk_sees_it() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);

    // Width must be a multiple of 8; the SDK would only say "invalid size".
    let error = camera.set_roi(Roi::new(0, 0, 100, 100)).unwrap_err();
    assert!(matches!(error, Error::InvalidGeometry(_)));
    assert!(error.to_string().contains("multiple of 8"), "{error}");
    // Odd height.
    assert!(camera.set_roi(Roi::new(0, 0, 800, 601)).is_err());
    // Off the sensor.
    assert!(camera.set_roi(Roi::new(0, 0, 4096, 4096)).is_err());

    camera.set_roi(Roi::new(8, 8, 800, 600)).unwrap();
    assert_eq!(camera.roi().unwrap(), Roi::new(8, 8, 800, 600));
    camera.start_streaming().unwrap();
    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (800, 600));
}

#[test]
fn geometry_changes_stop_and_restart_the_stream_for_you() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

    // The SDK refuses this outright while video is running; the backend is
    // expected to sequence it rather than pass the error on.
    camera.set_roi(Roi::new(0, 0, 640, 480)).unwrap();
    assert!(camera.is_streaming());
    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (640, 480));
}

#[test]
fn binning_rescales_and_drops_the_colour_mosaic() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.set_binning(Binning(2)).unwrap();
    assert_eq!(camera.pixel_format().unwrap(), PixelFormat::Mono);

    camera.start_streaming().unwrap();
    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (960, 540));
    assert_eq!(frame.meta.binning, Binning(2));

    assert!(matches!(
        camera.set_binning(Binning(3)),
        Err(Error::InvalidGeometry(_))
    ));
}

#[test]
fn an_unplug_reaches_the_frame_path_and_the_event_channel() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    let events = camera.events();
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

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
    assert!(!camera.is_connected());

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    assert!(
        seen.iter()
            .any(|e| matches!(e, CameraEvent::DeviceLost { .. })),
        "saw {seen:?}"
    );

    // Cleanup must work on a device that is already gone.
    camera.stop_streaming().unwrap();
    camera.disconnect().unwrap();
    assert!(backend.enumerate().unwrap().is_empty());

    mock::replug();
    let mut reopened = open(&backend);
    reopened.start_streaming().unwrap();
    assert!(reopened.next_frame(TIMEOUT).is_ok());
}

#[test]
fn a_camera_that_stops_delivering_produces_timeouts_not_hangs() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

    mock::freeze(true);
    while camera.next_frame(Duration::from_millis(300)).is_ok() {}
    let started = Instant::now();
    let result = camera.next_frame(Duration::from_millis(400));
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));

    mock::freeze(false);
    assert!(camera.next_frame(TIMEOUT).is_ok(), "frames should resume");
}

#[test]
fn a_rejected_control_write_is_reported_without_killing_the_handle() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);

    mock::fail_next_control();
    let error = camera.set_gain(200).unwrap_err();
    // SVB_ERROR_GENERAL_ERROR carries no detail, so the message has to supply
    // the usual cause.
    assert!(error.to_string().contains("outside the range"), "{error}");
    assert!(!error.is_fatal());
    assert!(camera.is_connected());

    camera.set_gain(200).unwrap();
    assert_eq!(camera.gain().unwrap(), 200);
}

#[test]
fn dropping_a_streaming_camera_shuts_the_sdk_down_cleanly() {
    let (_guard, backend) = setup();
    {
        let mut camera = open(&backend);
        camera.start_streaming().unwrap();
        camera.next_frame(TIMEOUT).unwrap();
    }
    // The device must be closed, or this second open fails.
    let mut again = open(&backend);
    again.start_streaming().unwrap();
    assert!(again.next_frame(TIMEOUT).is_ok());
}

#[test]
fn the_camera_can_measure_its_own_white_balance() {
    let (_guard, backend) = setup();
    let mut camera = open(&backend);
    assert!(
        camera.info().has_auto_white_balance,
        "a colour camera should offer it"
    );

    let before = camera.white_balance().unwrap();
    camera.auto_white_balance().unwrap();
    let after = camera.white_balance().unwrap();

    assert_ne!(before, after, "the gains should have been rewritten");
    // The point of doing it in the camera rather than in the display: the
    // values are readable afterwards, so the UI can show what it chose, and
    // captures come out balanced too.
    assert_eq!(camera.control(ControlId::WbRed).unwrap(), after.red);
    assert!(after.red > 128 && after.blue > 128);
}

#[test]
fn the_sdk_is_told_not_to_write_parameter_files() {
    // Left on, the SDK writes <model>-AST_Cfg_*.bin into the process's
    // working directory and reloads it next time a camera is opened there.
    // Observed on a real SV305C Pro: the same camera reported different gain,
    // offset and white balance depending on which directory the program ran
    // from, and dropped .bin files into a git working tree. The camera's own
    // state should be the only state.
    let (_guard, backend) = setup();
    assert!(
        mock::auto_save_enabled(),
        "the SDK default is on, or this test proves nothing"
    );
    let _camera = open(&backend);
    assert!(
        !mock::auto_save_enabled(),
        "connect should have turned it off"
    );
}
