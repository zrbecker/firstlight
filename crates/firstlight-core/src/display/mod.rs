//! Turning a raw frame into something a screen can show.
//!
//! Everything here operates on a *copy* and never touches what gets recorded.
//! Display-only processing that leaks into a saved file destroys data that
//! cannot be recovered, so the separation is enforced by construction: these
//! functions take `&Frame` and hand back an unrelated RGBA buffer.

use crate::frame::Frame;

/// How raw sample values are mapped to the 0..255 the screen wants.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Stretch {
    /// Straight linear map of the full bit depth. Astronomical frames look
    /// almost black this way, which is the honest rendering.
    #[default]
    Linear,
    /// Clip to the given percentiles of the frame's own histogram, then map
    /// linearly. This is the "auto-stretch" toggle in the GUI.
    AutoPercentile { low_pct: f32, high_pct: f32 },
    /// Fixed black and white points, in raw sample units.
    Manual { black: u16, white: u16 },
}

impl Stretch {
    /// The default auto-stretch: clip the bottom 0.5% and top 0.05%. Faint
    /// enough to keep the sky dark, aggressive enough to show stars.
    pub fn auto() -> Stretch {
        Stretch::AutoPercentile {
            low_pct: 0.5,
            high_pct: 99.95,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayOptions {
    pub stretch: Stretch,
    /// Stretch each colour channel against its own histogram, so the picture
    /// on screen looks neutral whatever the light.
    ///
    /// This is a *preview* white balance and nothing else: it never touches
    /// what gets recorded or saved. It exists because a live view that swings
    /// green in a hallway and red at a monitor is unusable for framing and
    /// focusing, which is why other capture software does the same thing. The
    /// levels it applies are reported in [`DisplayImage::channel_levels`] so
    /// the correction is visible rather than silent.
    pub white_balance_preview: bool,
    /// Debayer colour frames for display. Off shows the raw mosaic, which is
    /// useful for checking focus and for spotting a wrong Bayer phase.
    pub debayer: bool,
    /// Take every nth pixel. Anything above 1 costs image quality and buys
    /// frame rate; the GUI raises it for sensors larger than the viewport.
    /// Forced even on Bayer data so the colour phase survives.
    pub subsample: u32,
    /// Applied after the stretch. 1.0 is off; 0.45 lifts the faint end.
    pub gamma: f32,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        DisplayOptions {
            stretch: Stretch::Linear,
            white_balance_preview: true,
            debayer: true,
            subsample: 1,
            gamma: 1.0,
        }
    }
}

/// An 8-bit RGBA image ready to hand to a GPU texture, plus the levels that
/// were used so a UI can show them.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayImage {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA, 4 bytes per pixel, alpha always 255.
    pub rgba: Vec<u8>,
    /// Black point actually applied, raw sample units.
    pub black: u16,
    /// White point actually applied, raw sample units.
    pub white: u16,
    /// The levels applied to each channel. Identical to the pair above unless
    /// the preview white balance is on, in which case they are what makes the
    /// picture neutral — and worth showing, so the correction is never
    /// invisible.
    pub channel_levels: [(u16, u16); 3],
}

impl DisplayImage {
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Render a frame for display.
///
/// The inner loops here matter: at 1920x1080 this runs on every delivered
/// frame, and a naive implementation (a bounds-checked accessor call per
/// sample, a colour-filter lookup per sample) costs over a hundred
/// milliseconds per frame in a debug build — enough to make the whole
/// application feel broken. So the colour filter phase is resolved once per
/// frame rather than per pixel, rows are indexed directly, and the histogram
/// is accumulated in the same pass that reads the pixels.
pub fn render(frame: &Frame, options: &DisplayOptions) -> DisplayImage {
    let bayer = frame.meta.format.bayer();
    let mut step = options.subsample.max(1);
    if bayer.is_some() && options.debayer && step % 2 != 0 && step > 1 {
        step += 1;
    }

    let (src_w, src_h) = (frame.meta.width, frame.meta.height);
    let (out_w, out_h) = match (bayer, options.debayer) {
        // One RGB pixel per 2x2 mosaic cell: nearest-neighbour debayer, which
        // is all a live view needs and costs nothing.
        (Some(_), true) => (src_w / (2 * step).max(1), src_h / (2 * step).max(1)),
        _ => (src_w.div_ceil(step), src_h.div_ceil(step)),
    };
    let (out_w, out_h) = (out_w.max(1), out_h.max(1));

    let data = &frame.data;
    let stride = frame.meta.stride();
    let bps = frame.meta.bytes_per_sample();
    let spp = frame.meta.format.samples_per_pixel();
    let full_scale = frame.meta.bit_depth.max_value() as u16;

    let mut rgb: Vec<[u16; 3]> = Vec::with_capacity(out_w as usize * out_h as usize);
    // Histogram of luminance, accumulated as the pixels are read so they are
    // only walked once.
    //
    // Luminance rather than per-channel, deliberately: stretching each
    // channel separately would white-balance the picture, and a live view
    // that quietly corrects a colour cast stops you noticing that the
    // camera's own white balance wants setting.
    let mut histogram = vec![0u32; BINS];
    let mut channels = [vec![0u32; BINS], vec![0u32; BINS], vec![0u32; BINS]];
    let bin_scale = (BINS - 1) as f32 / f32::from(full_scale.max(1));
    let bin_of = |value: u32| ((value as f32 * bin_scale) as usize).min(BINS - 1);

    let note = |histogram: &mut Vec<u32>, channels: &mut [Vec<u32>; 3], px: [u16; 3]| {
        let lum = (u32::from(px[0]) + 2 * u32::from(px[1]) + u32::from(px[2])) / 4;
        histogram[bin_of(lum)] += 1;
        for (channel, value) in channels.iter_mut().zip(px) {
            channel[bin_of(u32::from(value))] += 1;
        }
    };

    match (bayer, options.debayer) {
        (Some(pattern), true) => {
            // Every 2x2 cell we sample starts on an even boundary, so the
            // filter phase is the same for all of them: work out which corner
            // is which colour once.
            let corner = |dx: u32, dy: u32| pattern.channel_at(dx, dy);
            let corners = [
                (0u32, 0u32, corner(0, 0)),
                (1, 0, corner(1, 0)),
                (0, 1, corner(0, 1)),
                (1, 1, corner(1, 1)),
            ];
            for oy in 0..out_h {
                let sy = (oy * 2 * step) as usize;
                for ox in 0..out_w {
                    let sx = (ox * 2 * step) as usize;
                    let mut sums = [0u32; 3];
                    let mut counts = [0u32; 3];
                    for (dx, dy, channel) in corners {
                        let offset = (sy + dy as usize) * stride + (sx + dx as usize) * bps;
                        sums[channel] += u32::from(read_sample(data, offset, bps));
                        counts[channel] += 1;
                    }
                    let px = [
                        (sums[0] / counts[0].max(1)) as u16,
                        (sums[1] / counts[1].max(1)) as u16,
                        (sums[2] / counts[2].max(1)) as u16,
                    ];
                    note(&mut histogram, &mut channels, px);
                    rgb.push(px);
                }
            }
        }
        _ => {
            for oy in 0..out_h {
                let sy = (oy * step).min(src_h.saturating_sub(1)) as usize;
                let row = sy * stride;
                for ox in 0..out_w {
                    let sx = (ox * step).min(src_w.saturating_sub(1)) as usize;
                    let offset = row + sx * spp * bps;
                    let px = if spp >= 3 {
                        [
                            read_sample(data, offset, bps),
                            read_sample(data, offset + bps, bps),
                            read_sample(data, offset + 2 * bps, bps),
                        ]
                    } else {
                        let v = read_sample(data, offset, bps);
                        [v, v, v]
                    };
                    note(&mut histogram, &mut channels, px);
                    rgb.push(px);
                }
            }
        }
    }

    let (black, white) = levels(&histogram, rgb.len(), options.stretch, full_scale);
    // Each channel stretched against its own histogram is what neutralises a
    // cast: whatever the red response was, its own top percentile maps to
    // white just as green's does.
    let channel_levels = if options.white_balance_preview {
        [
            levels(&channels[0], rgb.len(), options.stretch, full_scale),
            levels(&channels[1], rgb.len(), options.stretch, full_scale),
            levels(&channels[2], rgb.len(), options.stretch, full_scale),
        ]
    } else {
        [(black, white); 3]
    };
    let rgba = map_to_rgba(&rgb, channel_levels, options.gamma);

    DisplayImage {
        width: out_w,
        height: out_h,
        rgba,
        black,
        white,
        channel_levels,
    }
}

/// One sample, without the per-call geometry arithmetic of
/// [`Frame::sample`]. Out-of-range reads yield zero rather than panicking:
/// a display is not worth crashing over.
#[inline(always)]
fn read_sample(data: &[u8], offset: usize, bytes_per_sample: usize) -> u16 {
    if bytes_per_sample == 1 {
        data.get(offset).copied().map(u16::from).unwrap_or(0)
    } else {
        match (data.get(offset), data.get(offset + 1)) {
            (Some(&low), Some(&high)) => u16::from_le_bytes([low, high]),
            _ => 0,
        }
    }
}

/// Number of histogram bins. 4096 keeps a 16-bit percentile accurate to ~16
/// ADU, which is far below anything visible after an 8-bit map.
const BINS: usize = 4096;

/// Black and white points for the requested stretch.
///
/// The histogram is of luminance rather than of each channel: per-channel
/// percentiles would silently white-balance the display.
fn levels(histogram: &[u32], pixels: usize, stretch: Stretch, full_scale: u16) -> (u16, u16) {
    match stretch {
        Stretch::Linear => (0, full_scale.max(1)),
        Stretch::Manual { black, white } => (black, white.max(black.saturating_add(1))),
        Stretch::AutoPercentile { low_pct, high_pct } => {
            if pixels == 0 {
                return (0, full_scale.max(1));
            }
            let scale = f32::from(full_scale.max(1));
            let total = pixels as f32;
            let low_target = (total * (low_pct.clamp(0.0, 100.0) / 100.0)) as u32;
            let high_target = (total * (high_pct.clamp(0.0, 100.0) / 100.0)) as u32;

            let mut cumulative = 0u32;
            let histogram = histogram.iter();
            let mut black = 0usize;
            let mut white = BINS - 1;
            let mut black_found = false;
            for (bin, count) in histogram.enumerate() {
                cumulative += count;
                if !black_found && cumulative >= low_target {
                    black = bin;
                    black_found = true;
                }
                if cumulative >= high_target {
                    white = bin;
                    break;
                }
            }
            let to_value = |bin: usize| {
                ((bin as f32 / (BINS - 1) as f32) * scale)
                    .round()
                    .clamp(0.0, scale) as u16
            };
            let black_value = to_value(black);
            // Guarantee a usable span even on a flat frame, or the map below
            // divides by ~zero and the display flashes to pure white.
            let white_value = to_value(white).max(black_value.saturating_add(1));
            (black_value, white_value)
        }
    }
}

fn map_to_rgba(rgb: &[[u16; 3]], levels: [(u16, u16); 3], gamma: f32) -> Vec<u8> {
    let scale: Vec<(f32, f32)> = levels
        .iter()
        .map(|(black, white)| {
            (
                f32::from(*black),
                f32::from(white.saturating_sub(*black)).max(1.0),
            )
        })
        .collect();
    let apply_gamma = (gamma - 1.0).abs() > 1e-3 && gamma > 0.0;
    let mut out = Vec::with_capacity(rgb.len() * 4);
    for px in rgb {
        for (index, channel) in px.iter().enumerate() {
            let (black, span) = scale[index];
            let mut t = ((f32::from(*channel) - black) / span).clamp(0.0, 1.0);
            if apply_gamma {
                t = t.powf(gamma);
            }
            out.push((t * 255.0 + 0.5) as u8);
        }
        out.push(255);
    }
    out
}
