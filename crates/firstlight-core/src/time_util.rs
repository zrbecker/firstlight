//! Just enough calendar arithmetic for FITS `DATE-OBS` and SER timestamps.
//!
//! Both formats need civil UTC broken down from a `SystemTime`, and nothing
//! else, so this stays a dependency-free 60 lines instead of pulling in a
//! date-time crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// UTC civil time, already split into fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Utc {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanos: u32,
}

/// Howard Hinnant's `civil_from_days`, the standard branch-free conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds since the Unix epoch (negative for earlier) split into UTC fields.
pub fn utc_from_unix(secs: i64, nanos: u32) -> Utc {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Utc {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        minute: ((rem % 3600) / 60) as u32,
        second: (rem % 60) as u32,
        nanos,
    }
}

pub fn utc_from_system_time(t: SystemTime) -> Utc {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => utc_from_unix(d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            // Before 1970. Only reachable from a badly set clock, but the
            // writers must still emit something well-formed.
            let d: Duration = e.duration();
            let secs = -(d.as_secs() as i64);
            let nanos = d.subsec_nanos();
            if nanos == 0 {
                utc_from_unix(secs, 0)
            } else {
                utc_from_unix(secs - 1, 1_000_000_000 - nanos)
            }
        }
    }
}

impl std::fmt::Display for Utc {
    /// ISO-8601 with milliseconds, the form FITS `DATE-OBS` wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.nanos / 1_000_000
        )
    }
}

/// Ticks (100 ns units) since 0001-01-01T00:00:00 — the .NET `DateTime.Ticks`
/// encoding that the SER format borrowed for its timestamps.
pub const TICKS_AT_UNIX_EPOCH: i64 = 621_355_968_000_000_000;

pub fn ticks_from_system_time(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => TICKS_AT_UNIX_EPOCH + (d.as_nanos() / 100) as i64,
        Err(e) => TICKS_AT_UNIX_EPOCH - (e.duration().as_nanos() / 100) as i64,
    }
}
