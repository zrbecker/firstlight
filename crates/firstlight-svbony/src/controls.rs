//! Mapping the SDK's control table onto [`firstlight_core::ControlId`].
//!
//! The camera reports its own controls, ranges and names at runtime, so the
//! only thing hardcoded here is which vendor control means which portable
//! one. Everything the portable API has no name for stays reachable as
//! [`ControlId::Vendor`], with the label the camera itself supplied — which
//! is how a control like "Frame speed" ends up in the GUI without this crate
//! knowing anything about it.

use firstlight_core::ControlId;

pub const SVB_GAIN: i32 = 0;
pub const SVB_EXPOSURE: i32 = 1;
pub const SVB_GAMMA: i32 = 2;
pub const SVB_GAMMA_CONTRAST: i32 = 3;
pub const SVB_WB_R: i32 = 4;
pub const SVB_WB_G: i32 = 5;
pub const SVB_WB_B: i32 = 6;
pub const SVB_FLIP: i32 = 7;
pub const SVB_FRAME_SPEED_MODE: i32 = 8;
pub const SVB_CONTRAST: i32 = 9;
pub const SVB_SHARPNESS: i32 = 10;
pub const SVB_SATURATION: i32 = 11;
pub const SVB_AUTO_TARGET_BRIGHTNESS: i32 = 12;
pub const SVB_BLACK_LEVEL: i32 = 13;
pub const SVB_COOLER_ENABLE: i32 = 14;
pub const SVB_TARGET_TEMPERATURE: i32 = 15;
pub const SVB_CURRENT_TEMPERATURE: i32 = 16;
pub const SVB_COOLER_POWER: i32 = 17;
pub const SVB_BAD_PIXEL_CORRECTION_ENABLE: i32 = 18;
pub const SVB_BAD_PIXEL_CORRECTION_THRESHOLD: i32 = 19;

/// The portable id for a vendor control type.
pub fn to_control_id(control_type: i32) -> ControlId {
    match control_type {
        SVB_GAIN => ControlId::Gain,
        SVB_EXPOSURE => ControlId::ExposureUs,
        SVB_BLACK_LEVEL => ControlId::Offset,
        SVB_WB_R => ControlId::WbRed,
        SVB_WB_G => ControlId::WbGreen,
        SVB_WB_B => ControlId::WbBlue,
        SVB_COOLER_ENABLE => ControlId::Cooler,
        SVB_TARGET_TEMPERATURE => ControlId::TargetTemperatureMilliC,
        other => ControlId::Vendor(other as u32),
    }
}

/// The vendor control type for a portable id.
pub fn to_control_type(id: ControlId) -> Option<i32> {
    Some(match id {
        ControlId::Gain => SVB_GAIN,
        ControlId::ExposureUs => SVB_EXPOSURE,
        ControlId::Offset => SVB_BLACK_LEVEL,
        ControlId::WbRed => SVB_WB_R,
        ControlId::WbGreen => SVB_WB_G,
        ControlId::WbBlue => SVB_WB_B,
        ControlId::Cooler => SVB_COOLER_ENABLE,
        ControlId::TargetTemperatureMilliC => SVB_TARGET_TEMPERATURE,
        ControlId::Vendor(n) => n as i32,
        // No SVBONY camera exposes a bandwidth limit; it has a discrete
        // "Frame speed" instead, which arrives as a vendor control.
        ControlId::UsbBandwidth => return None,
    })
}

/// This SDK reports temperatures in tenths of a degree; the portable API uses
/// thousandths, because a set-point of 0.05 C is a thing on cooled cameras.
pub fn temperature_to_milli_c(tenths: i64) -> i64 {
    tenths * 100
}

pub fn milli_c_to_temperature(milli_c: i64) -> i64 {
    milli_c / 100
}

/// Unit suffix for a control, for a GUI to show after the number.
///
/// Deliberately sparse: guessing wrong is worse than saying nothing, and the
/// camera's own name for the control usually carries the meaning.
pub fn unit_for(control_type: i32) -> &'static str {
    match control_type {
        SVB_EXPOSURE => "us",
        SVB_BLACK_LEVEL => "ADU",
        SVB_TARGET_TEMPERATURE | SVB_CURRENT_TEMPERATURE => "0.1C",
        SVB_COOLER_POWER => "%",
        _ => "",
    }
}

/// Exposure spans six orders of magnitude on these cameras; a linear slider
/// for it is unusable.
pub fn is_logarithmic(control_type: i32) -> bool {
    control_type == SVB_EXPOSURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portable_controls_round_trip() {
        for (id, control_type) in [
            (ControlId::Gain, SVB_GAIN),
            (ControlId::ExposureUs, SVB_EXPOSURE),
            (ControlId::Offset, SVB_BLACK_LEVEL),
            (ControlId::WbRed, SVB_WB_R),
            (ControlId::WbGreen, SVB_WB_G),
            (ControlId::WbBlue, SVB_WB_B),
            (ControlId::Cooler, SVB_COOLER_ENABLE),
            (ControlId::TargetTemperatureMilliC, SVB_TARGET_TEMPERATURE),
        ] {
            assert_eq!(to_control_id(control_type), id);
            assert_eq!(to_control_type(id), Some(control_type));
        }
    }

    #[test]
    fn everything_else_stays_reachable_as_a_vendor_control() {
        // Frame speed is the control the user will actually reach for, and
        // the portable API has no name for it.
        assert_eq!(
            to_control_id(SVB_FRAME_SPEED_MODE),
            ControlId::Vendor(SVB_FRAME_SPEED_MODE as u32)
        );
        assert_eq!(
            to_control_type(ControlId::Vendor(SVB_FRAME_SPEED_MODE as u32)),
            Some(SVB_FRAME_SPEED_MODE)
        );
        for control_type in [
            SVB_GAMMA,
            SVB_CONTRAST,
            SVB_FLIP,
            SVB_BAD_PIXEL_CORRECTION_ENABLE,
        ] {
            assert!(matches!(to_control_id(control_type), ControlId::Vendor(_)));
        }
    }

    #[test]
    fn usb_bandwidth_is_reported_as_unsupported_rather_than_silently_mapped() {
        // Mapping it onto some unrelated control would be worse than saying
        // this camera does not have one.
        assert_eq!(to_control_type(ControlId::UsbBandwidth), None);
    }

    #[test]
    fn temperatures_convert_both_ways() {
        assert_eq!(temperature_to_milli_c(215), 21_500);
        assert_eq!(milli_c_to_temperature(-10_500), -105);
        assert_eq!(milli_c_to_temperature(temperature_to_milli_c(-100)), -100);
    }
}
