//! Small pieces of vendor-specific arithmetic, kept out of the SDK-gated
//! modules so they compile and are tested without the vendor headers.

use firstlight_core::control::BitDepth;

/// Largest black level (offset) the SDK accepts at a given bit depth.
///
/// The vendor documents 31 at 8 bit, scaling by four for every two extra
/// bits. Deriving it here avoids depending on header constants that have been
/// renamed between SDK releases.
pub fn black_level_max(depth: BitDepth) -> i64 {
    31i64 << (depth.bits().saturating_sub(8))
}

/// The `bits` argument for `Toupcam_PullImageV3`: raw data always arrives in
/// an 8 or 16 bit container, whatever the sensor's significant bit count.
pub fn pull_bits(depth: BitDepth) -> i32 {
    if depth.bits() <= 8 { 8 } else { 16 }
}

/// `TOUPCAM_OPTION_BITDEPTH` is a flag, not a bit count: 0 selects the 8 bit
/// output and 1 selects the sensor's deepest raw mode.
pub fn bitdepth_option(depth: BitDepth) -> i32 {
    i32::from(depth.bits() > 8)
}

/// White balance gains are signed offsets around unity, not percentages.
/// The SDK's range is documented as -127..=127 with 0 meaning unity, so map
/// the core API's percentage onto it.
pub fn wb_percent_to_gain(percent: i64) -> i32 {
    (percent.clamp(0, 400) - 100).clamp(-127, 127) as i32
}

pub fn wb_gain_to_percent(gain: i32) -> i64 {
    i64::from(gain) + 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_level_scales_with_bit_depth() {
        assert_eq!(black_level_max(BitDepth::EIGHT), 31);
        assert_eq!(black_level_max(BitDepth::TEN), 31 * 4);
        assert_eq!(black_level_max(BitDepth::TWELVE), 31 * 16);
        assert_eq!(black_level_max(BitDepth::SIXTEEN), 31 * 256);
    }

    #[test]
    fn raw_pulls_use_an_eight_or_sixteen_bit_container() {
        assert_eq!(pull_bits(BitDepth::EIGHT), 8);
        assert_eq!(pull_bits(BitDepth::TWELVE), 16);
        assert_eq!(pull_bits(BitDepth::SIXTEEN), 16);
    }

    #[test]
    fn the_bitdepth_option_is_a_flag() {
        assert_eq!(bitdepth_option(BitDepth::EIGHT), 0);
        assert_eq!(bitdepth_option(BitDepth::TWELVE), 1);
    }

    #[test]
    fn white_balance_round_trips_through_the_vendor_units() {
        assert_eq!(wb_percent_to_gain(100), 0, "unity gain is zero offset");
        assert_eq!(wb_gain_to_percent(0), 100);
        assert_eq!(wb_percent_to_gain(150), 50);
        assert_eq!(wb_gain_to_percent(wb_percent_to_gain(120)), 120);
        // The vendor range is narrower than ours, so clamp rather than wrap.
        assert_eq!(wb_percent_to_gain(400), 127);
        assert_eq!(wb_percent_to_gain(0), -100);
    }
}
