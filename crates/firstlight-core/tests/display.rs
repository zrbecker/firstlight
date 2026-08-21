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
        settings_settled: true,
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
        // The balance is off here: this is testing that the debayer puts each
        // colour in the right place, and a correction would equalise exactly
        // the differences being checked.
        let image = display::render(
            &frame,
            &DisplayOptions {
                white_balance_preview: false,
                ..DisplayOptions::default()
            },
        );
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
        // Off: this is testing where the stretch clips, not the balance.
        white_balance_preview: false,
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
        // Off: this is testing that subsampling keeps the Bayer phase, and a
        // correction would equalise the very channel differences being read.
        white_balance_preview: false,
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
        settings_settled: true,
    };
    let mut data = vec![0u8; (w * h) as usize * 2];
    for y in 0..h {
        for x in 0..w {
            let base = 400 + (x % 32) * 300;
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
fn the_preview_white_balance_neutralises_a_cast_without_touching_the_frame() {
    let frame = cast_frame();
    let before = frame.data.to_vec();

    let image = display::render(
        &frame,
        &DisplayOptions {
            stretch: Stretch::auto(),
            white_balance_preview: true,
            ..DisplayOptions::default()
        },
    );
    let [r, g, b] = channel_means(&image.rgba);
    assert!(
        (r - g).abs() < 6.0 && (g - b).abs() < 6.0,
        "expected a neutral preview, got r={r:.1} g={g:.1} b={b:.1}"
    );
    // The correction is reported rather than hidden: red was twice green in
    // the frame, so it is scaled down by about half.
    assert!(
        image.channel_gains[0] < 0.75 && image.channel_gains[2] > 1.5,
        "gains should record the correction: {:?}",
        image.channel_gains
    );
    assert_eq!(image.channel_gains[1], 1.0, "green is the reference");
    assert_eq!(
        frame.data.to_vec(),
        before,
        "the preview must never alter the frame that gets recorded"
    );
}

#[test]
fn a_colour_cast_is_shown_rather_than_corrected() {
    // Deliberate: all three channels share one pair of levels, taken from
    // luminance. Stretching each channel separately would white-balance the
    // preview, and a live view that hides a cast stops you noticing that the
    // camera's own white balance wants setting — which is a thing the camera
    // remembers between sessions and other software may have left wrong.
    let frame = cast_frame();
    let image = display::render(
        &frame,
        &DisplayOptions {
            stretch: Stretch::auto(),
            white_balance_preview: false,
            ..DisplayOptions::default()
        },
    );
    let [r, g, b] = channel_means(&image.rgba);
    assert!(
        r > g + 10.0 && g > b + 10.0,
        "with the preview balance off the cast should reach the screen, got \
         r={r:.1} g={g:.1} b={b:.1}"
    );
    assert_eq!(
        image.channel_gains, [1.0; 3],
        "off means no per-channel scaling at all"
    );
}

#[test]
fn the_preview_white_balance_works_with_every_stretch() {
    // The bug this exists for: white balance was implemented as per-channel
    // stretching, so it silently did nothing whenever the stretch was not
    // itself derived from the data. With a linear or manual stretch the
    // toggle had no effect at all, which looked like the feature being
    // randomly broken.
    let frame = cast_frame();
    for (name, stretch) in [
        ("linear", Stretch::Linear),
        ("auto", Stretch::auto()),
        (
            "manual",
            Stretch::Manual {
                black: 0,
                white: 20_000,
            },
        ),
    ] {
        let on = display::render(
            &frame,
            &DisplayOptions {
                stretch,
                white_balance_preview: true,
                ..DisplayOptions::default()
            },
        );
        let off = display::render(
            &frame,
            &DisplayOptions {
                stretch,
                white_balance_preview: false,
                ..DisplayOptions::default()
            },
        );
        assert_ne!(
            on.rgba, off.rgba,
            "{name} stretch: the white balance toggle changed nothing"
        );

        let [r, g, b] = channel_means(&on.rgba);
        assert!(
            (r - g).abs() < 8.0 && (g - b).abs() < 8.0,
            "{name} stretch: expected a neutral preview, got r={r:.1} g={g:.1} b={b:.1}"
        );
    }
}

/// A colour-cast scene that also contains something bright, which is what a
/// room, a window or a monitor in shot actually looks like.
fn cast_frame_with_highlight(bright_fraction: f64) -> Frame {
    let (w, h) = (128u32, 128u32);
    let meta = FrameMeta {
        sequence: 0,
        timestamp: SystemTime::now(),
        width: w,
        height: h,
        format: PixelFormat::Bayer(BayerPattern::Grbg),
        bit_depth: BitDepth::SIXTEEN,
        exposure_us: 20_000,
        gain: 100,
        offset: 0,
        binning: Binning::ONE,
        roi: Roi::full(w, h),
        dropped: 0,
        temperature_c: None,
        settings_settled: true,
    };
    let bright_rows = (f64::from(h) * bright_fraction) as u32;
    let mut data = vec![0u8; (w * h) as usize * 2];
    for y in 0..h {
        for x in 0..w {
            let base = 8000.0 + f64::from(x % 40) * 60.0;
            let mut value = match BayerPattern::Grbg.channel_at(x, y) {
                0 => base * 0.55,
                1 => base,
                _ => base * 0.40,
            };
            if y < bright_rows {
                // Bright enough to saturate every channel at once.
                value = 62_000.0;
            }
            let index = (y * w + x) as usize * 2;
            data[index..index + 2].copy_from_slice(&(value as u16).to_le_bytes());
        }
    }
    Frame::new(meta, data).unwrap()
}

#[test]
fn a_bright_highlight_does_not_defeat_the_preview_white_balance() {
    // Reported from a real scene: the preview stayed green while pressing
    // Auto WB fixed it. The cause was judging the balance by the top of each
    // channel's range — anything bright enough saturates all three at once,
    // so they look equally bright however strong the cast is, and the
    // correction collapses to nothing.
    for fraction in [0.0, 0.001, 0.02, 0.10] {
        let frame = cast_frame_with_highlight(fraction);
        let image = display::render(
            &frame,
            &DisplayOptions {
                stretch: Stretch::auto(),
                white_balance_preview: true,
                ..DisplayOptions::default()
            },
        );
        let [red, _, blue] = image.channel_gains;
        assert!(
            red > 1.3 && blue > 1.8,
            "{:.1}% bright: the cast was left uncorrected, gains {:?}",
            fraction * 100.0,
            image.channel_gains
        );
    }
}

#[test]
fn the_preview_agrees_with_what_auto_white_balance_would_do() {
    // The two used different statistics, so the preview could call a scene
    // balanced while the camera disagreed. Both now compare the average level
    // of each channel.
    let frame = cast_frame_with_highlight(0.02);
    let means = frame.channel_means().unwrap();
    let camera_would = [means[1] / means[0], means[1] / means[2]];

    let image = display::render(
        &frame,
        &DisplayOptions {
            stretch: Stretch::auto(),
            white_balance_preview: true,
            ..DisplayOptions::default()
        },
    );
    let preview = [image.channel_gains[0], image.channel_gains[2]];
    for (index, name) in [(0, "red"), (1, "blue")] {
        let ratio = preview[index] / camera_would[index] as f32;
        assert!(
            (0.6..1.7).contains(&ratio),
            "{name}: preview would apply {:.2} where the camera would apply {:.2}",
            preview[index],
            camera_would[index]
        );
    }
}
