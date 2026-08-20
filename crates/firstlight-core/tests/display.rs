//! Display rendering: debayer phase, auto-stretch levels, and the promise
//! that none of it touches the frame being recorded.

use std::time::SystemTime;

use firstlight_core::control::{Binning, BitDepth, Roi};
use firstlight_core::display::{self, DisplayOptions, Stretch};
use firstlight_core::frame::{BayerPattern, Frame, FrameMeta, PixelFormat};

fn bayer_frame(pattern: BayerPattern, red: u8, green: u8, blue: u8) -> Frame {
    let (w, h) = (4u32, 4u32);
    let meta = FrameMeta {
        sequence: 0,
        timestamp: SystemTime::now(),
        width: w,
        height: h,
        format: PixelFormat::Bayer(pattern),
        bit_depth: BitDepth::EIGHT,
        exposure_us: 1000,
        gain: 100,
        offset: 0,
        binning: Binning::ONE,
        roi: Roi::full(w, h),
        dropped: 0,
        temperature_c: None,
    };
    let mut data = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            data[(y * w + x) as usize] = match pattern.channel_at(x, y) {
                0 => red,
                1 => green,
                _ => blue,
            };
        }
    }
    Frame::new(meta, data).unwrap()
}

#[test]
fn nearest_neighbour_debayer_recovers_the_channel_values() {
    for pattern in [
        BayerPattern::Rggb,
        BayerPattern::Bggr,
        BayerPattern::Grbg,
        BayerPattern::Gbrg,
    ] {
        let frame = bayer_frame(pattern, 200, 120, 60);
        let image = display::render(&frame, &DisplayOptions::default());
        assert_eq!((image.width, image.height), (2, 2), "{pattern}");
        for pixel in image.rgba.chunks(4) {
            assert_eq!(
                [pixel[0], pixel[1], pixel[2], pixel[3]],
                [200, 120, 60, 255],
                "{pattern} debayered to the wrong channels"
            );
        }
    }
}

#[test]
fn debayer_can_be_turned_off_to_show_the_raw_mosaic() {
    let frame = bayer_frame(BayerPattern::Rggb, 200, 120, 60);
    let options = DisplayOptions {
        debayer: false,
        ..DisplayOptions::default()
    };
    let image = display::render(&frame, &options);
    assert_eq!((image.width, image.height), (4, 4));
    // Raw mosaic is rendered grey: the top-left RGGB pixel is red-filtered.
    assert_eq!(&image.rgba[0..4], &[200, 200, 200, 255]);
}

#[test]
fn linear_display_uses_the_full_bit_depth_not_the_frame_maximum() {
    let frame = bayer_frame(BayerPattern::Rggb, 8, 8, 8);
    let image = display::render(&frame, &DisplayOptions::default());
    assert_eq!(image.black, 0);
    assert_eq!(image.white, 255, "8-bit data stretches against 0..255");
    assert_eq!(image.rgba[0], 8, "a faint frame should stay faint");
}

#[test]
fn auto_stretch_pulls_a_dark_frame_up() {
    let frame = bayer_frame(BayerPattern::Rggb, 12, 10, 8);
    let options = DisplayOptions {
        stretch: Stretch::auto(),
        ..DisplayOptions::default()
    };
    let image = display::render(&frame, &options);
    assert!(image.white <= 13, "white point was {}", image.white);
    assert!(
        image.rgba[0] > 200,
        "the brightest channel should be near white, got {}",
        image.rgba[0]
    );
}

#[test]
fn a_flat_frame_does_not_divide_by_zero() {
    let frame = bayer_frame(BayerPattern::Rggb, 42, 42, 42);
    let options = DisplayOptions {
        stretch: Stretch::auto(),
        ..DisplayOptions::default()
    };
    let image = display::render(&frame, &options);
    assert!(
        image.white > image.black,
        "levels must span at least one step"
    );
    // Every pixel is identical, so the output must be uniform rather than
    // NaN-derived garbage.
    let first = &image.rgba[0..4];
    assert!(image.rgba.chunks(4).all(|px| px == first));
}

#[test]
fn manual_levels_clip_where_asked() {
    let frame = bayer_frame(BayerPattern::Rggb, 200, 100, 50);
    let options = DisplayOptions {
        stretch: Stretch::Manual {
            black: 100,
            white: 200,
        },
        ..DisplayOptions::default()
    };
    let image = display::render(&frame, &options);
    assert_eq!(&image.rgba[0..3], &[255, 0, 0], "clipped both ends");
}

#[test]
fn subsampling_shrinks_the_image_without_breaking_bayer_phase() {
    let frame = bayer_frame(BayerPattern::Rggb, 200, 120, 60);
    let options = DisplayOptions {
        subsample: 2,
        ..DisplayOptions::default()
    };
    let image = display::render(&frame, &options);
    assert_eq!((image.width, image.height), (1, 1));
    assert_eq!(&image.rgba[0..3], &[200, 120, 60]);
}

#[test]
fn rendering_leaves_the_source_frame_untouched() {
    let frame = bayer_frame(BayerPattern::Rggb, 200, 120, 60);
    let before = frame.data.to_vec();
    let options = DisplayOptions {
        stretch: Stretch::auto(),
        gamma: 0.45,
        ..DisplayOptions::default()
    };
    let _ = display::render(&frame, &options);
    assert_eq!(
        frame.data.to_vec(),
        before,
        "display processing must never alter the frame that gets recorded"
    );
}

/// Mean of each channel across an RGBA buffer.
fn channel_means(rgba: &[u8]) -> [f32; 3] {
    let mut sums = [0f64; 3];
    let pixels = rgba.len() / 4;
    for pixel in rgba.chunks(4) {
        for channel in 0..3 {
            sums[channel] += f64::from(pixel[channel]);
        }
    }
    [
        (sums[0] / pixels as f64) as f32,
        (sums[1] / pixels as f64) as f32,
        (sums[2] / pixels as f64) as f32,
    ]
}

/// A frame with a deliberate colour cast: red twice green, blue half of it,
/// varying across the frame so each channel has a real histogram.
fn cast_frame() -> Frame {
    let (w, h) = (64u32, 64u32);
    let meta = FrameMeta {
        sequence: 0,
        timestamp: SystemTime::now(),
        width: w,
        height: h,
        format: PixelFormat::Bayer(BayerPattern::Rggb),
        bit_depth: BitDepth::SIXTEEN,
        exposure_us: 1000,
        gain: 100,
        offset: 0,
        binning: Binning::ONE,
        roi: Roi::full(w, h),
        dropped: 0,
        temperature_c: None,
    };
    let mut data = vec![0u8; (w * h) as usize * 2];
    for y in 0..h {
        for x in 0..w {
            let base = 400 + (x as u32 % 32) * 300;
            let value = match BayerPattern::Rggb.channel_at(x, y) {
                0 => base * 2,
                1 => base,
                _ => base / 2,
            } as u16;
            let index = (y * w + x) as usize * 2;
            data[index..index + 2].copy_from_slice(&value.to_le_bytes());
        }
    }
    Frame::new(meta, data).unwrap()
}

#[test]
fn a_colour_cast_is_cancelled_for_display_when_asked() {
    let frame = cast_frame();

    // Off: the display shows the cast, because that is what the sensor saw.
    let honest = display::render(
        &frame,
        &DisplayOptions {
            stretch: Stretch::auto(),
            neutralise_colour: false,
            ..DisplayOptions::default()
        },
    );
    let [r, g, b] = channel_means(&honest.rgba);
    assert!(
        r > g + 10.0 && g > b + 10.0,
        "expected the cast to survive, got r={r:.1} g={g:.1} b={b:.1}"
    );
    assert_eq!(
        honest.channel_levels[0], honest.channel_levels[2],
        "with cancelling off, every channel shares one pair of levels"
    );

    // On: each channel is stretched against its own histogram, so the same
    // frame comes out neutral.
    let neutral = display::render(
        &frame,
        &DisplayOptions {
            stretch: Stretch::auto(),
            neutralise_colour: true,
            ..DisplayOptions::default()
        },
    );
    let [r, g, b] = channel_means(&neutral.rgba);
    assert!(
        (r - g).abs() < 6.0 && (g - b).abs() < 6.0,
        "expected a neutral rendering, got r={r:.1} g={g:.1} b={b:.1}"
    );
    // The red channel's own white point is about twice green's, which is the
    // cast being measured rather than guessed at.
    let (red_white, green_white) = (neutral.channel_levels[0].1, neutral.channel_levels[1].1);
    assert!(
        red_white > green_white,
        "red {red_white} should stretch against a higher white point than green {green_white}"
    );
}

#[test]
fn cancelling_a_cast_does_not_touch_the_frame_or_a_mono_image() {
    let frame = cast_frame();
    let before = frame.data.to_vec();
    let _ = display::render(
        &frame,
        &DisplayOptions {
            stretch: Stretch::auto(),
            neutralise_colour: true,
            ..DisplayOptions::default()
        },
    );
    assert_eq!(
        frame.data.to_vec(),
        before,
        "the recorded data is untouched"
    );

    // A mono frame has nothing to neutralise; it must come out the same
    // either way rather than being scaled per "channel".
    let mono = bayer_frame(BayerPattern::Rggb, 200, 200, 200);
    let with = display::render(
        &mono,
        &DisplayOptions {
            neutralise_colour: true,
            ..DisplayOptions::default()
        },
    );
    let without = display::render(
        &mono,
        &DisplayOptions {
            neutralise_colour: false,
            ..DisplayOptions::default()
        },
    );
    assert_eq!(with.rgba, without.rgba);
}
