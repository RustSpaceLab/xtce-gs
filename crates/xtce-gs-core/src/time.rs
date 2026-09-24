//! One clock type, in nanoseconds, with no dependency behind it.
//!
//! A ground station needs two times per packet: when the ground received it, and when the
//! spacecraft says it was made. This type carries both — it is just an instant — and the
//! engine is what knows how to pull the second one out of a packet.
//!
//! `chrono` and `time` are not dependencies here. What this crate needs from a calendar is
//! formatting an instant as ISO-8601 for a label and a log line, which is twenty lines of
//! integer arithmetic (Howard Hinnant's `civil_from_days`), and leap seconds are not modelled
//! by either of those crates for UTC display anyway.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since the Unix epoch, UTC.
///
/// Signed, so an epoch before 1970 — CCSDS counts from 1958 — is representable without a
/// separate type. `i64` nanoseconds runs from 1678 to 2262, which covers every mission that
/// will ever point this program at a socket.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Utc(i64);

impl Utc {
    /// The Unix epoch itself.
    pub const EPOCH: Self = Self(0);

    /// The CCSDS epoch, 1958-01-01T00:00:00, as a Unix time.
    ///
    /// 4 383 days before 1970, which is 12 years with three leap days among them.
    pub const CCSDS_EPOCH: Self = Self(-378_691_200_000_000_000);

    /// The GPS epoch, 1980-01-06T00:00:00.
    pub const GPS_EPOCH: Self = Self(315_964_800_000_000_000);

    /// The current wall-clock time.
    ///
    /// A clock set before 1970 gives a negative instant rather than an error; there is no
    /// useful failure to report to a caller stamping an arriving datagram.
    #[must_use]
    pub fn now() -> Self {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => Self(i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)),
            Err(before) => Self(
                i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, i64::wrapping_neg),
            ),
        }
    }

    /// Wraps a count of nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn from_unix_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Wraps a count of seconds since the Unix epoch.
    #[must_use]
    pub const fn from_unix_secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1_000_000_000))
    }

    /// Nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn unix_nanos(self) -> i64 {
        self.0
    }

    /// Seconds since the Unix epoch, as a plot axis wants them.
    #[must_use]
    pub fn unix_secs_f64(self) -> f64 {
        self.0 as f64 / 1e9
    }

    /// This instant moved by a signed number of nanoseconds, saturating at the ends.
    #[must_use]
    pub const fn offset_nanos(self, nanos: i64) -> Self {
        Self(self.0.saturating_add(nanos))
    }

    /// Nanoseconds from `earlier` to this instant; negative if `earlier` is later.
    #[must_use]
    pub const fn since(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }

    /// Seconds from `earlier` to this instant.
    #[must_use]
    pub fn secs_since(self, earlier: Self) -> f64 {
        self.since(earlier) as f64 / 1e9
    }

    /// Calendar breakdown: year, month, day, hour, minute, second, nanosecond.
    ///
    /// Proleptic Gregorian, no leap seconds — a UTC leap second is rendered as the second
    /// that follows it, which is what every library that does not carry a leap-second table
    /// does, and what the log line is read as anyway.
    #[must_use]
    pub const fn civil(self) -> (i32, u32, u32, u32, u32, u32, u32) {
        // Floor-divide into whole days and a remainder that is always non-negative, so an
        // instant before 1970 lands on the right day rather than one day late.
        //
        // `div_euclid`/`rem_euclid` and not the subtraction that computes the same thing:
        // at `i64::MIN` the day count is -106 752 and multiplying it back by a day's
        // nanoseconds lands below `i64::MIN`, which panics in a debug build and wraps in a
        // release one. These two cannot overflow for any input, which is the property this
        // needs — a timestamp read out of a hostile packet reaches here.
        let nanos = self.0;
        let days = nanos.div_euclid(86_400_000_000_000);
        let rem_nanos = nanos.rem_euclid(86_400_000_000_000);

        let (year, month, day) = civil_from_days(days);
        let secs_of_day = rem_nanos / 1_000_000_000;
        let nano = (rem_nanos % 1_000_000_000) as u32;
        (
            year,
            month,
            day,
            (secs_of_day / 3600) as u32,
            ((secs_of_day / 60) % 60) as u32,
            (secs_of_day % 60) as u32,
            nano,
        )
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
///
/// Hinnant's algorithm, shifted to an era starting in March so that the leap day is the last
/// day of the year and the month lengths fall into a repeating pattern.
const fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

impl fmt::Display for Utc {
    /// ISO-8601 with millisecond resolution: `2026-09-12T14:03:07.412Z`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day, hour, min, sec, nano) = self.civil();
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{:03}Z",
            nano / 1_000_000
        )
    }
}

impl fmt::Debug for Utc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Utc({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_renders_as_the_epoch() {
        assert_eq!(Utc::EPOCH.to_string(), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn the_ccsds_epoch_is_1958() {
        assert_eq!(Utc::CCSDS_EPOCH.to_string(), "1958-01-01T00:00:00.000Z");
    }

    #[test]
    fn the_gps_epoch_is_the_sixth_of_january() {
        assert_eq!(Utc::GPS_EPOCH.to_string(), "1980-01-06T00:00:00.000Z");
    }

    #[test]
    fn a_leap_day_is_a_day() {
        // 2024-02-29T12:34:56.789Z
        let t = Utc::from_unix_secs(1_709_210_096).offset_nanos(789_000_000);
        assert_eq!(t.to_string(), "2024-02-29T12:34:56.789Z");
    }

    #[test]
    fn instants_before_the_epoch_do_not_land_a_day_late() {
        // One second before the epoch is the last second of 1969.
        let t = Utc::from_unix_nanos(-1_000_000_000);
        assert_eq!(t.to_string(), "1969-12-31T23:59:59.000Z");
        // And a fraction of a second before it is still that day.
        let t = Utc::from_unix_nanos(-1);
        assert_eq!(t.to_string(), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn differences_are_signed() {
        let a = Utc::from_unix_secs(100);
        let b = Utc::from_unix_secs(130);
        assert_eq!(b.secs_since(a), 30.0);
        assert_eq!(a.secs_since(b), -30.0);
    }
}

#[cfg(test)]
mod extremes {
    use super::*;

    #[test]
    fn the_ends_of_the_range_do_not_overflow() {
        for nanos in [i64::MIN, i64::MIN + 1, i64::MAX, i64::MAX - 1, 0, -1] {
            let rendered = Utc::from_unix_nanos(nanos).to_string();
            assert!(!rendered.is_empty(), "{nanos} rendered as nothing");
        }
    }
}
