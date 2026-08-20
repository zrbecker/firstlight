//! Camera controls, geometry and their descriptors.

use crate::error::{Error, Result};

/// Controls every backend speaks in the same units.
///
/// Anything a specific vendor exposes beyond this list is reachable through
/// [`ControlId::Vendor`], which carries the backend-defined option id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ControlId {
    /// Exposure time in microseconds.
    ExposureUs,
    /// Analogue gain, in the camera's native units (see [`ControlInfo`]).
    Gain,
    /// Black level / offset in ADU.
    Offset,
    /// Red channel white-balance gain (colour cameras).
    WbRed,
    /// Green channel white-balance gain.
    WbGreen,
    /// Blue channel white-balance gain.
    WbBlue,
    /// Sensor set-point in milli-degrees Celsius (cooled cameras).
    TargetTemperatureMilliC,
    /// Cooler on/off, 0 or 1.
    Cooler,
    /// USB bandwidth / traffic limit, percent.
    UsbBandwidth,
    /// Backend-specific escape hatch.
    Vendor(u32),
}

impl ControlId {
    pub fn name(&self) -> &'static str {
        match self {
            ControlId::ExposureUs => "exposure_us",
            ControlId::Gain => "gain",
            ControlId::Offset => "offset",
            ControlId::WbRed => "wb_red",
            ControlId::WbGreen => "wb_green",
            ControlId::WbBlue => "wb_blue",
            ControlId::TargetTemperatureMilliC => "target_temperature_mc",
            ControlId::Cooler => "cooler",
            ControlId::UsbBandwidth => "usb_bandwidth",
            ControlId::Vendor(_) => "vendor",
        }
    }

    /// Parse the names used by the CLI and the GUI.
    pub fn parse(s: &str) -> Option<ControlId> {
        Some(match s {
            "exposure_us" | "exposure" => ControlId::ExposureUs,
            "gain" => ControlId::Gain,
            "offset" | "brightness" => ControlId::Offset,
            "wb_red" | "wb_r" => ControlId::WbRed,
            "wb_green" | "wb_g" => ControlId::WbGreen,
            "wb_blue" | "wb_b" => ControlId::WbBlue,
            "target_temperature_mc" | "target_temp" => ControlId::TargetTemperatureMilliC,
            "cooler" => ControlId::Cooler,
            "usb_bandwidth" | "bandwidth" => ControlId::UsbBandwidth,
            other => {
                let n = other.strip_prefix("vendor:")?;
                ControlId::Vendor(n.parse().ok()?)
            }
        })
    }
}

impl std::fmt::Display for ControlId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlId::Vendor(id) => write!(f, "vendor:{id}"),
            other => f.write_str(other.name()),
        }
    }
}

/// What a camera says about one of its controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlInfo {
    pub id: ControlId,
    /// Human readable label for a GUI.
    pub label: String,
    pub min: i64,
    pub max: i64,
    /// Smallest meaningful increment; always >= 1.
    pub step: i64,
    pub default: i64,
    /// Unit suffix for display, e.g. "us", "%", "ADU".
    pub unit: &'static str,
    /// The camera can drive this control itself (auto-exposure, auto-WB).
    pub auto_supported: bool,
    /// Readable but not writable.
    pub read_only: bool,
    /// A logarithmic slider suits this control better than a linear one.
    pub logarithmic: bool,
}

impl ControlInfo {
    pub fn new(id: ControlId, label: impl Into<String>, min: i64, max: i64, default: i64) -> Self {
        ControlInfo {
            id,
            label: label.into(),
            min,
            max,
            step: 1,
            default,
            unit: "",
            auto_supported: false,
            read_only: false,
            logarithmic: false,
        }
    }

    pub fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }

    pub fn step(mut self, step: i64) -> Self {
        self.step = step.max(1);
        self
    }

    pub fn logarithmic(mut self, yes: bool) -> Self {
        self.logarithmic = yes;
        self
    }

    pub fn auto(mut self, yes: bool) -> Self {
        self.auto_supported = yes;
        self
    }

    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = yes;
        self
    }

    /// Reject out-of-range values before they reach the SDK, and snap to the
    /// advertised step so backends never see a value they must silently round.
    pub fn validate(&self, value: i64) -> Result<i64> {
        if value < self.min || value > self.max {
            return Err(Error::OutOfRange {
                control: self.id.name(),
                value,
                min: self.min,
                max: self.max,
            });
        }
        if self.step > 1 {
            let snapped = self.min + ((value - self.min) / self.step) * self.step;
            return Ok(snapped);
        }
        Ok(value)
    }
}

/// Region of interest in *binned* pixels, relative to the full sensor at the
/// current binning factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Roi {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Roi {
            x,
            y,
            width,
            height,
        }
    }

    pub fn full(width: u32, height: u32) -> Self {
        Roi::new(0, 0, width, height)
    }

    pub fn is_full(&self, width: u32, height: u32) -> bool {
        *self == Roi::full(width, height)
    }

    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Check the ROI fits the sensor and respects the alignment a Bayer sensor
    /// needs: origin and size on even boundaries, or the colour filter phase
    /// shifts and every debayer downstream comes out wrong.
    pub fn validate(&self, max_width: u32, max_height: u32, bayer: bool) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidGeometry("ROI has zero area".into()));
        }
        if self.x.saturating_add(self.width) > max_width
            || self.y.saturating_add(self.height) > max_height
        {
            return Err(Error::InvalidGeometry(format!(
                "ROI {self} exceeds sensor {max_width}x{max_height}"
            )));
        }
        if bayer
            && (self.x % 2 != 0 || self.y % 2 != 0 || self.width % 2 != 0 || self.height % 2 != 0)
        {
            return Err(Error::InvalidGeometry(format!(
                "ROI {self} must be even-aligned on a Bayer sensor"
            )));
        }
        Ok(())
    }

    /// Snap to even boundaries, the usual fix-up for a GUI-entered ROI.
    pub fn align_even(self) -> Roi {
        Roi {
            x: self.x & !1,
            y: self.y & !1,
            width: (self.width & !1).max(2),
            height: (self.height & !1).max(2),
        }
    }
}

impl std::fmt::Display for Roi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}+{}+{}", self.width, self.height, self.x, self.y)
    }
}

/// Symmetric binning factor. None of the cameras this library targets bin
/// asymmetrically, so a single factor keeps the API honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binning(pub u32);

impl Binning {
    pub const ONE: Binning = Binning(1);

    pub fn factor(&self) -> u32 {
        self.0.max(1)
    }
}

impl Default for Binning {
    fn default() -> Self {
        Binning::ONE
    }
}

impl std::fmt::Display for Binning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{0}x{0}", self.factor())
    }
}

/// Significant bits per pixel coming off the sensor. The transport container
/// is 8 bit for `Eight` and 16 bit little-endian for everything deeper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BitDepth(pub u8);

impl BitDepth {
    pub const EIGHT: BitDepth = BitDepth(8);
    pub const TEN: BitDepth = BitDepth(10);
    pub const TWELVE: BitDepth = BitDepth(12);
    pub const FOURTEEN: BitDepth = BitDepth(14);
    pub const SIXTEEN: BitDepth = BitDepth(16);

    pub fn bits(&self) -> u8 {
        self.0
    }

    /// Bytes each sample occupies in a [`crate::Frame`] buffer.
    pub fn bytes_per_sample(&self) -> usize {
        if self.0 <= 8 { 1 } else { 2 }
    }

    /// Largest value a sample can hold.
    pub fn max_value(&self) -> u32 {
        (1u32 << self.0) - 1
    }
}

impl Default for BitDepth {
    fn default() -> Self {
        BitDepth::SIXTEEN
    }
}

impl std::fmt::Display for BitDepth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-bit", self.0)
    }
}

/// Per-channel white balance gains, in the camera's native units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WhiteBalance {
    pub red: i64,
    pub green: i64,
    pub blue: i64,
}
