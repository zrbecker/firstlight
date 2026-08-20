//! Exercises the whole `Camera` contract, including the failure paths that
//! otherwise need somebody standing by the telescope pulling the USB cable.

use std::time::{Duration, Instant};

use firstlight_core::simulator::SimulatorBackend;
use firstlight_core::{
    Backend, BayerPattern, Binning, BitDepth, CameraId, ControlId, Error, PixelFormat, Roi,
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn small_colour_backend() -> SimulatorBackend {
    SimulatorBackend::single(64, 48, PixelFormat::Bayer(BayerPattern::Rggb))
}

#[test]
fn enumerate_reports_both_default_cameras() {
    let backend = SimulatorBackend::new();
    let cameras = backend.enumerate().unwrap();
    assert_eq!(cameras.len(), 2);
    assert!(cameras[0].pixel_format.is_colour());
    assert_eq!(cameras[1].pixel_format, PixelFormat::Mono);
    assert!(cameras.iter().all(|c| c.backend == "simulator"));
}

#[test]
fn opening_an_unknown_camera_says_so() {
    let backend = SimulatorBackend::new();
    let result = backend.open(&CameraId::new("no-such-camera")).map(|_| ());
    assert!(matches!(result, Err(Error::NotFound(_))), "got {result:?}");
}

#[test]
fn a_camera_can_only_be_opened_once() {
    let backend = SimulatorBackend::new();
    let id = backend.enumerate().unwrap()[0].id.clone();
    let _first = backend.open(&id).unwrap();
    let second = backend.open(&id).map(|_| ());
    assert!(matches!(second, Err(Error::Busy(_))), "got {second:?}");
}

#[test]
fn streaming_delivers_frames_matching_the_requested_geometry() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    camera.set_exposure_us(5_000).unwrap();
    camera.set_gain(200).unwrap();
    camera.start_streaming().unwrap();

    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (64, 48));
    assert_eq!(frame.meta.bit_depth, BitDepth::SIXTEEN);
    assert_eq!(frame.meta.exposure_us, 5_000);
    assert_eq!(frame.meta.gain, 200);
    assert_eq!(frame.data.len(), 64 * 48 * 2);
    assert!(
        frame.to_u16().iter().any(|&v| v > 0),
        "the synthetic star field should not be uniformly black"
    );

    let second = camera.next_frame(TIMEOUT).unwrap();
    assert!(second.meta.sequence > frame.meta.sequence);
    camera.stop_streaming().unwrap();
}

#[test]
fn next_frame_before_the_stream_starts_is_an_error_not_a_hang() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    let started = Instant::now();
    let result = camera.next_frame(Duration::from_secs(10));
    assert!(matches!(result, Err(Error::NotStreaming)), "got {result:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn out_of_range_control_values_are_rejected_with_the_range() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    match camera.set_control(ControlId::Gain, 1_000_000) {
        Err(Error::OutOfRange {
            control, min, max, ..
        }) => {
            assert_eq!(control, "gain");
            assert!(min < max);
        }
        other => panic!("expected OutOfRange, got {other:?}"),
    }
    assert!(matches!(
        camera.set_control(ControlId::Vendor(42), 1),
        Err(Error::UnknownControl(_))
    ));
}

#[test]
fn roi_must_fit_the_sensor_and_stay_even_on_a_bayer_camera() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();

    assert!(matches!(
        camera.set_roi(Roi::new(0, 0, 4096, 4096)),
        Err(Error::InvalidGeometry(_))
    ));
    assert!(matches!(
        camera.set_roi(Roi::new(1, 0, 16, 16)),
        Err(Error::InvalidGeometry(_))
    ));

    camera.set_roi(Roi::new(8, 8, 16, 16)).unwrap();
    camera.start_streaming().unwrap();
    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (16, 16));
    assert_eq!(frame.meta.roi, Roi::new(8, 8, 16, 16));
}

#[test]
fn binning_rescales_the_frame_and_drops_the_colour_mosaic() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    camera.set_binning(Binning(2)).unwrap();
    assert_eq!(camera.pixel_format().unwrap(), PixelFormat::Mono);

    camera.start_streaming().unwrap();
    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (32, 24));
    assert_eq!(frame.meta.binning, Binning(2));

    assert!(matches!(
        camera.set_binning(Binning(3)),
        Err(Error::InvalidGeometry(_))
    ));
}

#[test]
fn eight_bit_mode_halves_the_frame_size() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    camera.set_bit_depth(BitDepth::EIGHT).unwrap();
    camera.start_streaming().unwrap();

    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!(frame.meta.bit_depth, BitDepth::EIGHT);
    assert_eq!(frame.data.len(), 64 * 48);
    assert!(frame.to_u16().iter().all(|&v| v <= 255));
}

#[test]
fn geometry_changes_survive_being_made_mid_stream() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

    camera.set_roi(Roi::new(0, 0, 32, 32)).unwrap();
    assert!(
        camera.is_streaming(),
        "the stream should have been restarted"
    );
    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert_eq!((frame.width(), frame.height()), (32, 32));
}

#[test]
fn a_frozen_camera_produces_timeouts_and_stays_usable() {
    let backend = small_colour_backend();
    let handle = backend.handle(0).unwrap();
    let mut camera = backend.open_first().unwrap();
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

    handle.freeze_frames(true);
    // Drain whatever was already queued before the freeze took effect.
    while camera.next_frame(Duration::from_millis(200)).is_ok() {}
    let result = camera.next_frame(Duration::from_millis(300));
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");

    handle.freeze_frames(false);
    assert!(camera.next_frame(TIMEOUT).is_ok(), "frames should resume");
}

#[test]
fn unplugging_reports_device_lost_on_every_path() {
    let backend = small_colour_backend();
    let handle = backend.handle(0).unwrap();
    let mut camera = backend.open_first().unwrap();
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

    handle.unplug();

    // The waiting consumer finds out promptly, not after its full timeout.
    let started = Instant::now();
    let mut saw_loss = false;
    for _ in 0..10 {
        match camera.next_frame(Duration::from_secs(2)) {
            Err(Error::DeviceLost(_)) => {
                saw_loss = true;
                break;
            }
            Err(e) => panic!("expected DeviceLost, got {e:?}"),
            Ok(_) => continue, // frames queued before the unplug
        }
    }
    assert!(saw_loss, "next_frame never reported the loss");
    assert!(started.elapsed() < Duration::from_secs(10));

    assert!(!camera.is_connected());
    // Control writes to a dead handle must fail immediately.
    assert!(matches!(camera.set_gain(300), Err(Error::DeviceLost(_))));
    // Cleanup has to work on a dead device, or the app cannot recover.
    camera.stop_streaming().unwrap();
    camera.disconnect().unwrap();

    assert!(backend.enumerate().unwrap().is_empty());
    handle.replug();
    assert_eq!(backend.enumerate().unwrap().len(), 1);
    let mut reopened = backend.open_first().unwrap();
    reopened.start_streaming().unwrap();
    assert!(reopened.next_frame(TIMEOUT).is_ok());
}

#[test]
fn a_usb_stall_is_reported_as_a_stall_not_a_timeout() {
    let backend = small_colour_backend();
    let handle = backend.handle(0).unwrap();
    let mut camera = backend.open_first().unwrap();
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();

    handle.stall_usb();
    let mut saw_stall = false;
    for _ in 0..10 {
        match camera.next_frame(Duration::from_secs(2)) {
            Err(Error::UsbStall(_)) => {
                saw_stall = true;
                break;
            }
            Err(e) => panic!("expected UsbStall, got {e:?}"),
            Ok(_) => continue,
        }
    }
    assert!(saw_stall, "next_frame never reported the stall");
}

#[test]
fn the_event_channel_reports_lifecycle_and_loss() {
    use firstlight_core::CameraEvent;

    let backend = small_colour_backend();
    let handle = backend.handle(0).unwrap();
    let mut camera = backend.open_first().unwrap();
    let events = camera.events();
    camera.start_streaming().unwrap();
    camera.next_frame(TIMEOUT).unwrap();
    handle.unplug();
    let _ = camera.next_frame(Duration::from_secs(2));

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                let fatal = event.is_fatal();
                seen.push(event);
                if fatal {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    assert!(seen.contains(&CameraEvent::Connected), "saw {seen:?}");
    assert!(seen.contains(&CameraEvent::StreamStarted), "saw {seen:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, CameraEvent::DeviceLost { .. })),
        "saw {seen:?}"
    );
}

#[test]
fn dropped_frames_are_counted_when_nobody_reads_them() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    camera.set_exposure_us(1_000).unwrap();
    camera.start_streaming().unwrap();

    // Deliberately do not read: the ring is three deep, so everything after
    // that must be counted as dropped rather than queued or lost silently.
    std::thread::sleep(Duration::from_millis(400));
    let dropped = camera.dropped_frames();
    assert!(dropped > 0, "expected drops, got {dropped}");

    let frame = camera.next_frame(TIMEOUT).unwrap();
    assert!(frame.meta.dropped > 0, "frames carry the drop count");
}

#[test]
fn snap_leaves_the_stream_as_it_found_it() {
    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    assert!(!camera.is_streaming());
    let frame = camera.snap(TIMEOUT).unwrap();
    assert_eq!(frame.width(), 64);
    assert!(
        !camera.is_streaming(),
        "snap should have stopped the stream"
    );
}

#[test]
fn automatic_white_balance_measures_and_corrects() {
    use firstlight_core::WhiteBalance;

    let backend = small_colour_backend();
    let mut camera = backend.open_first().unwrap();
    camera.set_exposure_us(20_000).unwrap();

    // Start well off balance.
    camera
        .set_white_balance(WhiteBalance {
            red: 300,
            green: 100,
            blue: 40,
        })
        .unwrap();
    camera.start_streaming().unwrap();
    let before = camera.next_frame(TIMEOUT).unwrap().channel_means().unwrap();
    assert!(
        before[0] / before[1] > 1.5,
        "the frame should start red-heavy, got {before:?}"
    );

    camera.auto_white_balance().unwrap();

    let after = camera.next_frame(TIMEOUT).unwrap().channel_means().unwrap();
    let (red, blue) = (after[0] / after[1], after[2] / after[1]);
    assert!(
        (0.7..1.4).contains(&red) && (0.7..1.4).contains(&blue),
        "expected a balanced frame, got R/G={red:.2} B/G={blue:.2} from {after:?}"
    );

    // It changed the camera, not the view: the gains moved.
    let gains = camera.white_balance().unwrap();
    assert_ne!(gains.red, 300, "the red gain should have been corrected");
}

#[test]
fn automatic_white_balance_is_refused_on_a_mono_camera() {
    let backend = SimulatorBackend::single(64, 48, PixelFormat::Mono);
    let mut camera = backend.open_first().unwrap();
    let error = camera.auto_white_balance().unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "got {error:?}");
}
