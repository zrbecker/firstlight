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
}

impl DisplayImage {
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Render a frame for display.
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

    let mut rgb: Vec<[u16; 3]> = Vec::with_capacity(out_w as usize * out_h as usize);
    match (bayer, options.debayer) {
        (Some(pattern), true) => {
            for oy in 0..out_h {
                let sy = oy * 2 * step;
                for ox in 0..out_w {
                    let sx = ox * 2 * step;
                    let mut sums = [0u32; 3];
                    let mut counts = [0u32; 3];
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let c = pattern.channel_at(sx + dx, sy + dy);
                            if let Some(v) = frame.sample(sx + dx, sy + dy, 0) {
                                sums[c] += u32::from(v);
                                counts[c] += 1;
                            }
                        }
                    }
                    rgb.push([
                        (sums[0] / counts[0].max(1)) as u16,
                        (sums[1] / counts[1].max(1)) as u16,
                        (sums[2] / counts[2].max(1)) as u16,
                    ]);
                }
            }
        }
        _ => {
            let spp = frame.meta.format.samples_per_pixel();
            for oy in 0..out_h {
                let sy = (oy * step).min(src_h.saturating_sub(1));
                for ox in 0..out_w {
                    let sx = (ox * step).min(src_w.saturating_sub(1));
                    if spp >= 3 {
                        rgb.push([
                            frame.sample(sx, sy, 0).unwrap_or(0),
                            frame.sample(sx, sy, 1).unwrap_or(0),
                            frame.sample(sx, sy, 2).unwrap_or(0),
                        ]);
                    } else {
                        let v = frame.sample(sx, sy, 0).unwrap_or(0);
                        rgb.push([v, v, v]);
                    }
                }
            }
        }
    }

    let full_scale = frame.meta.bit_depth.max_value() as u16;
    let (black, white) = levels(&rgb, options.stretch, full_scale);
    let rgba = map_to_rgba(&rgb, black, white, options.gamma);

    DisplayImage {
        width: out_w,
        height: out_h,
        rgba,
        black,
        white,
    }
}

/// Number of histogram bins. 4096 keeps a 16-bit percentile accurate to ~16
/// ADU, which is far below anything visible after an 8-bit map.
const BINS: usize = 4096;

/// Black and white points for the requested stretch.
fn levels(rgb: &[[u16; 3]], stretch: Stretch, full_scale: u16) -> (u16, u16) {
    match stretch {
        Stretch::Linear => (0, full_scale.max(1)),
        Stretch::Manual { black, white } => (black, white.max(black.saturating_add(1))),
        Stretch::AutoPercentile { low_pct, high_pct } => {
            if rgb.is_empty() {
                return (0, full_scale.max(1));
            }
            let scale = f32::from(full_scale.max(1));
            let mut histogram = vec![0u32; BINS];
            // Histogram the luminance, not each channel: per-channel
            // percentiles would silently white-balance the display.
            for px in rgb {
                let lum = (u32::from(px[0]) + 2 * u32::from(px[1]) + u32::from(px[2])) / 4;
                let bin = ((lum as f32 / scale) * (BINS - 1) as f32).clamp(0.0, (BINS - 1) as f32);
                histogram[bin as usize] += 1;
            }
            let total = rgb.len() as f32;
            let low_target = (total * (low_pct.clamp(0.0, 100.0) / 100.0)) as u32;
            let high_target = (total * (high_pct.clamp(0.0, 100.0) / 100.0)) as u32;

            let mut cumulative = 0u32;
            let mut black = 0usize;
            let mut white = BINS - 1;
            let mut black_found = false;
            for (bin, count) in histogram.iter().enumerate() {
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

fn map_to_rgba(rgb: &[[u16; 3]], black: u16, white: u16, gamma: f32) -> Vec<u8> {
    let span = f32::from(white.saturating_sub(black)).max(1.0);
    let apply_gamma = (gamma - 1.0).abs() > 1e-3 && gamma > 0.0;
    let mut out = Vec::with_capacity(rgb.len() * 4);
    for px in rgb {
        for channel in px {
            let mut t = (f32::from(*channel) - f32::from(black)) / span;
            t = t.clamp(0.0, 1.0);
            if apply_gamma {
                t = t.powf(gamma);
            }
            out.push((t * 255.0 + 0.5) as u8);
        }
        out.push(255);
    }
    out
}
