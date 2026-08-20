//! Argument parsing, including the value types shared by several commands.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use firstlight_core::control::{Binning, BitDepth, ControlId, Roi};

#[derive(Debug, Parser)]
#[command(
    name = "firstlight-cli",
    version,
    about = "Astronomy camera capture: enumerate, record SER, save FITS",
    long_about = "A test harness for the FirstLight camera library.\n\n\
                  Every blocking operation has a timeout and every failure is \
                  reported with the reason, so this is also the quickest way \
                  to find out what a misbehaving camera is actually doing."
)]
pub struct Cli {
    /// Log level: error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "warn")]
    pub log: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List every attached camera.
    List {
        /// Show the controls each camera exposes, which needs opening it.
        #[arg(long)]
        verbose: bool,
    },

    /// Open one camera and print its capabilities and current settings.
    Info {
        #[command(flatten)]
        select: Select,
    },

    /// Record a stream of frames to a SER file.
    Capture {
        #[command(flatten)]
        select: Select,
        #[command(flatten)]
        settings: Settings,

        /// Where to write the SER file.
        #[arg(short, long)]
        output: PathBuf,

        /// Stop after this many frames.
        #[arg(short = 'n', long)]
        frames: Option<u64>,

        /// Stop after this long, e.g. 30s, 5m.
        #[arg(short = 'd', long, value_parser = parse_duration)]
        duration: Option<Duration>,

        /// Name recorded in the SER header.
        #[arg(long, default_value = "")]
        observer: String,

        /// Telescope recorded in the SER header.
        #[arg(long, default_value = "")]
        telescope: String,
    },

    /// Save one or more stills as FITS files.
    Snap {
        #[command(flatten)]
        select: Select,
        #[command(flatten)]
        settings: Settings,

        /// Output path. With --count above 1, a _0001 style index is added
        /// before the extension.
        #[arg(short, long, default_value = "still.fits")]
        output: PathBuf,

        /// How many stills to take.
        #[arg(short = 'n', long, default_value_t = 1)]
        count: u32,

        /// Target name for the FITS OBJECT card.
        #[arg(long, default_value = "")]
        object: String,
    },

    /// Watch a camera's event stream: useful for reproducing unplug and
    /// stall behaviour without recording anything.
    Watch {
        #[command(flatten)]
        select: Select,
        #[command(flatten)]
        settings: Settings,

        /// Stop after this long.
        #[arg(short = 'd', long, value_parser = parse_duration)]
        duration: Option<Duration>,
    },
}

/// Which camera to use.
#[derive(Debug, Args, Clone)]
pub struct Select {
    /// Camera id, as shown by `list`. Defaults to the first one found.
    #[arg(short, long)]
    pub camera: Option<String>,

    /// Restrict to one backend: touptek, simulator.
    #[arg(short, long)]
    pub backend: Option<String>,
}

/// Camera settings applied before capture.
#[derive(Debug, Args, Clone, Default)]
pub struct Settings {
    /// Exposure, e.g. 5ms, 250us, 1.5s. A bare number means microseconds.
    #[arg(short = 'e', long, value_parser = parse_duration)]
    pub exposure: Option<Duration>,

    /// Analogue gain, in the camera's native units (100 is unity).
    #[arg(short = 'g', long)]
    pub gain: Option<i64>,

    /// Black level offset, in ADU.
    #[arg(long)]
    pub offset: Option<i64>,

    /// Region of interest as X,Y,WIDTH,HEIGHT in binned pixels.
    #[arg(long, value_parser = parse_roi)]
    pub roi: Option<Roi>,

    /// Binning factor.
    #[arg(long, value_parser = parse_binning)]
    pub bin: Option<Binning>,

    /// Bit depth: 8, 10, 12, 14 or 16.
    #[arg(long, value_parser = parse_bit_depth)]
    pub bits: Option<BitDepth>,

    /// White balance as R,G,B, in the camera's own units.
    #[arg(long, value_parser = parse_triple)]
    pub wb: Option<[i64; 3]>,

    /// Have the camera measure a white balance from the current scene and
    /// store it. Point it at something neutral first.
    #[arg(long)]
    pub auto_wb: bool,

    /// Set any control by name, e.g. --set usb_bandwidth=80. Repeatable, and
    /// `vendor:<n>=<v>` reaches backend-specific options.
    #[arg(long = "set", value_parser = parse_control)]
    pub controls: Vec<(ControlId, i64)>,

    /// How long to wait for a single frame before giving up.
    #[arg(long, value_parser = parse_duration, default_value = "10s")]
    pub timeout: Duration,
}

/// `10s`, `250ms`, `40us`, or a bare number meaning microseconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (value, scale) = if let Some(rest) = text.strip_suffix("us") {
        (rest, 1e-6)
    } else if let Some(rest) = text.strip_suffix("ms") {
        (rest, 1e-3)
    } else if let Some(rest) = text.strip_suffix('m') {
        (rest, 60.0)
    } else if let Some(rest) = text.strip_suffix('s') {
        (rest, 1.0)
    } else {
        // A bare number is microseconds: exposures are the common case and
        // typing `--exposure 5000` should not silently mean 83 minutes.
        (text, 1e-6)
    };
    let value: f64 = value
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a duration (try 5ms, 250us, 1.5s, 2m)"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{text:?} is not a valid duration"));
    }
    Ok(Duration::from_secs_f64(value * scale))
}

pub fn parse_roi(text: &str) -> Result<Roi, String> {
    let parts: Vec<&str> = text.split([',', 'x', ':']).map(str::trim).collect();
    let numbers: Result<Vec<u32>, _> = parts.iter().map(|p| u32::from_str(p)).collect();
    match numbers.as_deref() {
        Ok([x, y, w, h]) => Ok(Roi::new(*x, *y, *w, *h)),
        Ok([w, h]) => Ok(Roi::new(0, 0, *w, *h)),
        _ => Err(format!(
            "{text:?} is not an ROI (use X,Y,WIDTH,HEIGHT or WIDTH,HEIGHT)"
        )),
    }
}

pub fn parse_binning(text: &str) -> Result<Binning, String> {
    // Accept both `2` and `2x2`.
    let head = text.split(['x', 'X']).next().unwrap_or(text);
    head.trim()
        .parse::<u32>()
        .map(Binning)
        .map_err(|_| format!("{text:?} is not a binning factor"))
}

pub fn parse_bit_depth(text: &str) -> Result<BitDepth, String> {
    let bits: u8 = text
        .trim()
        .trim_end_matches("-bit")
        .parse()
        .map_err(|_| format!("{text:?} is not a bit depth"))?;
    if !(1..=16).contains(&bits) {
        return Err(format!("bit depth {bits} is out of range (1..=16)"));
    }
    Ok(BitDepth(bits))
}

pub fn parse_triple(text: &str) -> Result<[i64; 3], String> {
    let parts: Result<Vec<i64>, _> = text.split(',').map(|p| p.trim().parse()).collect();
    match parts.as_deref() {
        Ok([r, g, b]) => Ok([*r, *g, *b]),
        _ => Err(format!("{text:?} is not an R,G,B triple")),
    }
}

pub fn parse_control(text: &str) -> Result<(ControlId, i64), String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} should look like name=value"))?;
    let id = ControlId::parse(name.trim())
        .ok_or_else(|| format!("unknown control {:?}", name.trim()))?;
    let value: i64 = value
        .trim()
        .parse()
        .map_err(|_| format!("{:?} is not a number", value.trim()))?;
    Ok((id, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_the_units_an_imager_would_type() {
        assert_eq!(parse_duration("250us").unwrap(), Duration::from_micros(250));
        assert_eq!(parse_duration("5ms").unwrap(), Duration::from_millis(5));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        // A bare number is microseconds, matching --exposure-us elsewhere.
        assert_eq!(parse_duration("5000").unwrap(), Duration::from_millis(5));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("-1s").is_err());
    }

    #[test]
    fn roi_accepts_both_forms() {
        assert_eq!(
            parse_roi("10,20,640,480").unwrap(),
            Roi::new(10, 20, 640, 480)
        );
        assert_eq!(parse_roi("640,480").unwrap(), Roi::new(0, 0, 640, 480));
        assert_eq!(parse_roi("640x480").unwrap(), Roi::new(0, 0, 640, 480));
        assert!(parse_roi("640").is_err());
    }

    #[test]
    fn binning_and_bit_depth_are_forgiving_about_spelling() {
        assert_eq!(parse_binning("2").unwrap(), Binning(2));
        assert_eq!(parse_binning("2x2").unwrap(), Binning(2));
        assert_eq!(parse_bit_depth("12").unwrap(), BitDepth::TWELVE);
        assert_eq!(parse_bit_depth("16-bit").unwrap(), BitDepth::SIXTEEN);
        assert!(parse_bit_depth("32").is_err());
    }

    #[test]
    fn controls_can_be_set_by_name_or_vendor_option() {
        assert_eq!(parse_control("gain=250").unwrap(), (ControlId::Gain, 250));
        assert_eq!(
            parse_control("vendor:13=1").unwrap(),
            (ControlId::Vendor(13), 1)
        );
        assert!(parse_control("gain").is_err());
        assert!(parse_control("nonsense=1").is_err());
    }
}
