//! The frame ring is the one place where "we lost data" is a legitimate
//! outcome, so its accounting has to be exact.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use firstlight_core::Error;
use firstlight_core::control::{Binning, BitDepth, Roi};
use firstlight_core::frame::{Frame, FrameMeta, PixelFormat};
use firstlight_core::ring::{FrameRing, StreamStop};

fn frame(sequence: u64) -> Frame {
    let meta = FrameMeta {
        sequence,
        timestamp: SystemTime::now(),
        width: 2,
        height: 2,
        format: PixelFormat::Mono,
        bit_depth: BitDepth::EIGHT,
        exposure_us: 1000,
        gain: 100,
        offset: 0,
        binning: Binning::ONE,
        roi: Roi::full(2, 2),
        dropped: 0,
        temperature_c: None,
        settings_settled: true,
    };
    Frame::new(meta, vec![0u8; 4]).expect("frame geometry")
}

#[test]
fn full_ring_drops_the_oldest_frame_and_counts_it() {
    let ring = FrameRing::new(2);
    for seq in 0..5 {
        ring.push(frame(seq));
    }
    assert_eq!(ring.dropped(), 3, "three frames should have been evicted");
    // What survives is the newest, which is what a live view wants.
    assert_eq!(ring.recv_timeout(Duration::ZERO).unwrap().meta.sequence, 3);
    assert_eq!(ring.recv_timeout(Duration::ZERO).unwrap().meta.sequence, 4);
}

#[test]
fn empty_ring_times_out_rather_than_blocking_forever() {
    let ring = FrameRing::new(2);
    let started = Instant::now();
    let result = ring.recv_timeout(Duration::from_millis(150));
    assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    assert!(started.elapsed() >= Duration::from_millis(140));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn device_loss_wakes_a_waiting_consumer_immediately() {
    let ring = Arc::new(FrameRing::new(2));
    let producer = ring.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        producer.stop(StreamStop::DeviceLost("unplugged".into()));
    });

    let started = Instant::now();
    let result = ring.recv_timeout(Duration::from_secs(30));
    assert!(
        matches!(result, Err(Error::DeviceLost(_))),
        "got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the waiter should not have run out its 30s timeout"
    );
}

#[test]
fn queued_frames_are_delivered_before_the_stop_reason() {
    let ring = FrameRing::new(4);
    ring.push(frame(7));
    ring.stop(StreamStop::DeviceLost("unplugged".into()));
    assert_eq!(ring.recv_timeout(Duration::ZERO).unwrap().meta.sequence, 7);
    assert!(matches!(
        ring.recv_timeout(Duration::ZERO),
        Err(Error::DeviceLost(_))
    ));
}

#[test]
fn the_first_stop_reason_wins() {
    let ring = FrameRing::new(1);
    ring.stop(StreamStop::UsbStall("pipe error".into()));
    ring.stop(StreamStop::Stopped);
    assert!(matches!(
        ring.recv_timeout(Duration::ZERO),
        Err(Error::UsbStall(_))
    ));
}

#[test]
fn reset_clears_frames_drops_and_stop_reason() {
    let ring = FrameRing::new(1);
    ring.push(frame(0));
    ring.push(frame(1));
    ring.stop(StreamStop::Stopped);
    ring.reset();
    assert_eq!(ring.dropped(), 0);
    assert!(ring.is_empty());
    assert!(matches!(
        ring.recv_timeout(Duration::ZERO),
        Err(Error::Timeout(_))
    ));
}
