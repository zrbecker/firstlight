//! Terminal output: tables, control listings and the live progress line.

use std::io::{IsTerminal, Write};
use std::time::Instant;

use firstlight_core::camera::{Camera, CameraInfo};
use firstlight_core::error::Result;
use firstlight_core::frame::Frame;

pub fn print_camera_table(cameras: &[CameraInfo]) {
    let id_width = cameras
        .iter()
        .map(|c| c.id.as_str().len())
        .chain(std::iter::once(2))
        .max()
        .unwrap_or(2);
    println!(
        "{:<id_width$}  {:<10}  {:<28}  {:<12}  FORMAT",
        "ID", "BACKEND", "NAME", "SENSOR"
    );
    for camera in cameras {
        println!(
            "{:<id_width$}  {:<10}  {:<28}  {:<12}  {}",
            camera.id.as_str(),
            camera.backend,
            truncate(&camera.display_name, 28),
            format!("{}x{}", camera.max_width, camera.max_height),
            camera.pixel_format
        );
    }
}

pub fn print_camera_detail(info: &CameraInfo) {
    println!("{} [{}]", info.display_name, info.id);
    println!("  backend      {}", info.backend);
    println!("  model        {}", info.model);
    println!(
        "  serial       {}",
        if info.serial.is_empty() {
            "(not reported)"
        } else {
            &info.serial
        }
    );
    println!("  sensor       {}x{}", info.max_width, info.max_height);
    println!("  pixel size   {:.2} um", info.pixel_size_um);
    println!("  format       {}", info.pixel_format);
    println!(
        "  bit depths   {}",
        info.bit_depths
            .iter()
            .map(|d| d.bits().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  binning      {}",
        info.binnings
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  cooler       {}",
        if info.has_cooler { "yes" } else { "no" }
    );
}

pub fn print_controls(camera: &dyn Camera) -> Result<()> {
    let controls = camera.controls()?;
    if controls.is_empty() {
        println!("  (no controls reported)");
        return Ok(());
    }
    println!(
        "  {:<30} {:>12} {:>12} {:>12} {:>12}",
        "CONTROL", "VALUE", "MIN", "MAX", "DEFAULT"
    );
    for control in controls {
        // A control that cannot be read right now is worth showing anyway:
        // its range is still useful, and hiding it would look like the camera
        // does not have it.
        let value = camera
            .control(control.id)
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "-".into());
        println!(
            "  {:<30} {:>12} {:>12} {:>12} {:>12}  {}",
            format!("{} ({})", control.label, control.id),
            value,
            control.min,
            control.max,
            control.default,
            control.unit
        );
    }
    Ok(())
}

/// `1.500 s`, `25.0 ms`, `250 us` — whichever reads best.
pub fn format_exposure(micros: u64) -> String {
    if micros >= 1_000_000 {
        format!("{:.3} s", micros as f64 / 1e6)
    } else if micros >= 1_000 {
        format!("{:.1} ms", micros as f64 / 1e3)
    } else {
        format!("{micros} us")
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A one-line progress display, refreshed in place.
pub struct Progress {
    started: Instant,
    last_print: Instant,
    bytes: u64,
    recent: std::collections::VecDeque<Instant>,
    /// Carriage-return redraws only make sense on a terminal. Piped into a
    /// file or a log they produce one enormous line, so redirected output
    /// gets ordinary lines, less often.
    interactive: bool,
}

impl Progress {
    pub fn new() -> Progress {
        let now = Instant::now();
        Progress {
            started: now,
            last_print: now,
            bytes: 0,
            recent: std::collections::VecDeque::new(),
            interactive: std::io::stdout().is_terminal(),
        }
    }

    pub fn frame(&mut self, frame: &Frame) {
        self.bytes += frame.data.len() as u64;
        let now = Instant::now();
        self.recent.push_back(now);
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t).as_secs_f32() > 1.0)
        {
            self.recent.pop_front();
        }
    }

    pub fn fps(&self) -> f32 {
        self.recent.len() as f32
    }

    /// Print at most a few times a second: a progress line that scrolls is
    /// worse than no progress line.
    pub fn maybe_print(&mut self, frames: u64, dropped: u64) {
        let period = if self.interactive { 250 } else { 5_000 };
        if self.last_print.elapsed().as_millis() < period {
            return;
        }
        self.last_print = Instant::now();
        let line = format!(
            "{frames} frames  {:.1} fps  {}  {dropped} dropped  {:.0}s",
            self.fps(),
            format_bytes(self.bytes),
            self.started.elapsed().as_secs_f32()
        );
        if self.interactive {
            print!("\r{line}   ");
            let _ = std::io::stdout().flush();
        } else {
            println!("{line}");
        }
    }
}

impl Default for Progress {
    fn default() -> Self {
        Progress::new()
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposures_are_shown_in_readable_units() {
        assert_eq!(format_exposure(250), "250 us");
        assert_eq!(format_exposure(25_000), "25.0 ms");
        assert_eq!(format_exposure(1_500_000), "1.500 s");
    }

    #[test]
    fn byte_counts_are_shown_in_readable_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn long_names_are_truncated_not_wrapped() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789abc", 6), "01234…");
    }
}
