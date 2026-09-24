//! Spacecraft time: what the packet says about when it was made.
//!
//! Ground receipt is always available and is rarely the axis an operator wants. A pass
//! replayed from an on-board recorder arrives hours after it happened, and plotted against
//! receipt it is a vertical wall; plotted against the on-board clock it is the orbit it came
//! from. So a session may name one parameter as the clock, and this module is what turns that
//! parameter's bits into a [`Utc`].
//!
//! # What this refuses
//!
//! * **It does not guess the layout.** There is no sniffing of plausible epochs and no
//!   fallback from CUC to CDS. A field that does not match its configured layout yields
//!   `None`, the batch falls back to ground receipt, and the operator is told once.
//! * **It does not model leap seconds.** [`Utc`] does not carry a leap table, so a CUC count
//!   from the CCSDS epoch is treated as a count of SI seconds against a UTC scale that has
//!   none. For a station watching a pass this is a constant offset of tens of seconds and it
//!   is stated here rather than discovered later; a mission that needs TAI-correct stamps
//!   needs a leap table, which is a dependency this workspace does not have.
//! * **It does not calibrate.** [`TimeFormat::Cuc`] and [`TimeFormat::Cds`] read the *raw*
//!   bytes, because a calibrator on a time parameter would have been applied to a count that
//!   is really two fields glued together. [`TimeFormat::Seconds`] reads the engineering
//!   value, because there the calibrator is exactly what produces the seconds.

use xtce_decode::{DecodedPacket, EngValue, RawValue};
use xtce_gs_core::Utc;
use xtce_model::{ParamId, XtceDb};

use crate::config::{TimeFormat, TimeSource};
use crate::error::EngineError;

/// Nanoseconds in a second, as the fixed-point conversions below need it.
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Octets of milliseconds-of-day in a CDS field. Always four, by CCSDS 301.0-B-4.
pub const CDS_MILLISECONDS_BYTES: usize = 4;

/// The most bytes one big-endian read may cover.
///
/// Eight, because the accumulator is a `u64`. It caps a CUC's whole field — coarse and fine
/// are one number — and each CDS field on its own, which is why a CDS of a three-octet day
/// count, four octets of milliseconds and four of picoseconds is eleven bytes in total and is
/// not refused: it is read in three pieces. CCSDS allows a CUC with more octets through the
/// P-field extension; no mission in reach sends one, and a field that wide would need an
/// accumulator this code does not have — so it is refused rather than truncated.
pub const MAX_TIME_BYTES: usize = 8;

/// Milliseconds in a day, plus the one leap second this clock does not model.
///
/// CCSDS 301.0-B-4 section 3.3.3 allows the milliseconds-of-day field to run to 86 400 999
/// during a positive leap second. A count at or above this constant is a field that is not
/// the layout it was configured as.
const MAX_MILLISECONDS_OF_DAY: u64 = 86_401_000;

/// Seconds in a day.
const SECONDS_PER_DAY: i128 = 86_400;

/// Reads up to eight big-endian octets as an unsigned integer.
///
/// Longer input silently loses its leading octets, so every caller checks the width first;
/// that check is what `MAX_TIME_BYTES` is.
fn be_unsigned(bytes: &[u8]) -> u64 {
    let mut value: u64 = 0;
    for &byte in bytes {
        value = (value << 8) | u64::from(byte);
    }
    value
}

/// An epoch moved by a count of nanoseconds, or `None` when the result leaves [`Utc`].
///
/// [`Utc::offset_nanos`] saturates, and a saturated stamp is the year 2262 on a plot axis —
/// which reads as a bug in the station rather than as a bad time field. So the addition is
/// done in `i128` and the narrowing is the check.
fn instant(epoch: Utc, nanos: i128) -> Option<Utc> {
    let total = i128::from(epoch.unix_nanos()).checked_add(nanos)?;
    i64::try_from(total).ok().map(Utc::from_unix_nanos)
}

/// Nanoseconds from a CUC's two fields.
///
/// CCSDS 301.0-B-4 section 3.2.2: the fine octets are the binary fraction below the second,
/// so the first of them is 1/256 s and the field's value is `fine / 256^fine_bytes` of a
/// second. The division is a shift on a `u128` intermediate rather than an `f64` multiply
/// because a four-octet fine field asks for 32 bits of fraction and `f64` has 53 bits of
/// mantissa left over for the whole seconds — not enough for both at a 2094 epoch.
fn cuc_nanos(coarse: u64, fine: u64, fine_bytes: usize) -> i128 {
    let whole = i128::from(coarse) * i128::from(NANOS_PER_SECOND);
    let fraction = (u128::from(fine) * (NANOS_PER_SECOND as u128)) >> (8 * fine_bytes as u32);
    whole + fraction as i128
}

/// Decodes a CUC field laid out as bytes.
///
/// CCSDS 301.0-B-4 section 3.2: an unsegmented count of `coarse_bytes` whole seconds from
/// `epoch` followed by `fine_bytes` of binary fraction, both big-endian and both unsigned.
/// The basic P-field names one to four coarse and zero to three fine octets; wider fields are
/// accepted here up to [`MAX_TIME_BYTES`] in total, which is what the `u64` accumulator
/// holds.
///
/// A `bytes` longer than the field is read from its front and the rest ignored: a parameter
/// declared wider than its time code is a thing definitions do.
///
/// `None` when the field is too short, too wide, or the resulting instant is outside the
/// range [`Utc`] can name.
#[must_use]
pub fn cuc_from_bytes(bytes: &[u8], coarse_bytes: u8, fine_bytes: u8, epoch: Utc) -> Option<Utc> {
    let coarse_bytes = usize::from(coarse_bytes);
    let fine_bytes = usize::from(fine_bytes);
    if coarse_bytes == 0 || coarse_bytes + fine_bytes > MAX_TIME_BYTES {
        return None;
    }
    // A time field split across two packets is not a time.
    let field = bytes.get(..coarse_bytes + fine_bytes)?;
    let (coarse, fine) = field.split_at(coarse_bytes);
    instant(
        epoch,
        cuc_nanos(be_unsigned(coarse), be_unsigned(fine), fine_bytes),
    )
}

/// Decodes a CUC field that arrived as a single unsigned integer.
///
/// An XTCE definition that declares the CUC as an `<IntegerParameterType>` of 48 bits hands
/// the decoder a `u64` with the coarse count in the high bits and the fraction in the low
/// `8 * fine_bytes` — the same number [`cuc_from_bytes`] builds out of the octets.
///
/// `None` when the two widths do not fit in the integer, or the instant is out of range.
#[must_use]
pub fn cuc_from_unsigned(value: u64, coarse_bytes: u8, fine_bytes: u8, epoch: Utc) -> Option<Utc> {
    let coarse_bytes = usize::from(coarse_bytes);
    let fine_bytes = usize::from(fine_bytes);
    if coarse_bytes == 0 || coarse_bytes + fine_bytes > MAX_TIME_BYTES {
        return None;
    }
    let fine_bits = 8 * fine_bytes as u32;
    // `fine_bytes` is at most seven here, so neither shift reaches the width of the type.
    let coarse = value >> fine_bits;
    let fine = if fine_bits == 0 {
        0
    } else {
        value & ((1u64 << fine_bits) - 1)
    };
    instant(epoch, cuc_nanos(coarse, fine, fine_bytes))
}

/// Nanoseconds from a CDS submillisecond field.
///
/// CCSDS 301.0-B-4 section 3.3.2: two octets are microseconds within the millisecond and four
/// are picoseconds. They are different fields and confusing them is a factor of a million, so
/// the width decides and nothing else does. Picoseconds are truncated to the nanosecond
/// [`Utc`] counts in.
fn cds_submillisecond_nanos(field: &[u8]) -> Option<i128> {
    match field.len() {
        0 => Some(0),
        2 => Some(i128::from(be_unsigned(field)) * 1_000),
        4 => Some(i128::from(be_unsigned(field)) / 1_000),
        _ => None,
    }
}

// TODO(gs-engine-sctime-submilli): a submillisecond field is refused on its *width* and not on
// its value, so a microsecond count above 999 or a picosecond count above 999 999 999 is
// carried into the instant and pushes it past the millisecond it belongs in. CCSDS 301.0-B-4
// section 3.3.2 gives those fields no range beyond the field width, and refusing an
// out-of-range count would reject real data from a spacecraft that packs the field
// differently. Deciding it needs a mission that does, plus a ruling on whether an
// out-of-range count is a dropped sample or a clamped one.
/// Decodes a CDS field.
///
/// CCSDS 301.0-B-4 section 3.3, three big-endian fields in order: `day_bytes` of whole days
/// from `epoch`, then [`CDS_MILLISECONDS_BYTES`] of milliseconds of day, then
/// `submillisecond_bytes` of microseconds (two) or picoseconds (four). The day segment is 16
/// or 24 bits in the standard; the width is not refused here beyond a zero, because
/// [`crate::SessionConfig::validate`] is where the layout policy lives and it is the stricter
/// of the two.
///
/// Milliseconds of day are refused at or above 86 401 000: a day plus the leap second this
/// clock does not model. Beyond that the field is not the layout it was configured as.
///
/// `None` when the field is too short, the submillisecond width is not one CCSDS allows, or
/// the instant is out of range.
#[must_use]
pub fn cds_from_bytes(
    bytes: &[u8],
    day_bytes: u8,
    submillisecond_bytes: u8,
    epoch: Utc,
) -> Option<Utc> {
    let day_bytes = usize::from(day_bytes);
    let submillisecond_bytes = usize::from(submillisecond_bytes);
    if day_bytes == 0 || day_bytes > MAX_TIME_BYTES {
        return None;
    }
    let width = day_bytes + CDS_MILLISECONDS_BYTES + submillisecond_bytes;
    let field = bytes.get(..width)?;
    let (days, rest) = field.split_at(day_bytes);
    let (milliseconds, submillisecond) = rest.split_at(CDS_MILLISECONDS_BYTES);

    let milliseconds = be_unsigned(milliseconds);
    if milliseconds >= MAX_MILLISECONDS_OF_DAY {
        return None;
    }
    let submillisecond = cds_submillisecond_nanos(submillisecond)?;

    let nanos = i128::from(be_unsigned(days))
        .checked_mul(SECONDS_PER_DAY * i128::from(NANOS_PER_SECOND))?
        + i128::from(milliseconds) * 1_000_000
        + submillisecond;
    instant(epoch, nanos)
}

/// Converts a count of seconds since `epoch`.
///
/// `None` for a value that is not a finite number of nanoseconds away from the epoch — a NaN,
/// an infinity, or a magnitude no `i64` of nanoseconds holds.
#[must_use]
pub fn seconds_from_f64(seconds: f64, epoch: Utc) -> Option<Utc> {
    if !seconds.is_finite() {
        return None;
    }
    let nanos = (seconds * NANOS_PER_SECOND as f64).round();
    // `i64::MAX as f64` rounds up, so the comparison is `>=` and the boundary nanosecond is
    // lost rather than saturated. That instant is the year 2262.
    if nanos.abs() >= i64::MAX as f64 {
        return None;
    }
    instant(epoch, nanos as i128)
}

/// Reads one decoded parameter as an instant.
///
/// Both values are taken because which one holds the time depends on the format — see this
/// module's header on why a calibrator must not touch a CUC. [`TimeFormat::Cuc`] and
/// [`TimeFormat::Cds`] read the raw value, as bytes or as the single unsigned integer an
/// `<IntegerParameterType>` of the right width decodes to; [`TimeFormat::Seconds`] reads the
/// engineering value, so that a calibrated `<AbsoluteTimeParameter>` and a plain float both
/// work.
#[must_use]
pub fn from_values(format: TimeFormat, raw: &RawValue<'_>, eng: &EngValue<'_, '_>) -> Option<Utc> {
    match format {
        TimeFormat::Cuc {
            coarse_bytes,
            fine_bytes,
            epoch,
        } => match raw {
            RawValue::Bytes(bytes) => cuc_from_bytes(bytes, coarse_bytes, fine_bytes, epoch),
            RawValue::Unsigned(value) => cuc_from_unsigned(*value, coarse_bytes, fine_bytes, epoch),
            RawValue::Signed(_) | RawValue::Float(_) => None,
        },
        TimeFormat::Cds {
            day_bytes,
            submillisecond_bytes,
            epoch,
        } => match raw {
            RawValue::Bytes(bytes) => cds_from_bytes(bytes, day_bytes, submillisecond_bytes, epoch),
            // A 2 + 4 + 0 CDS is 48 bits and fits an integer parameter; the octets it would
            // have arrived as are the low bytes of that integer.
            RawValue::Unsigned(value) => {
                let width = usize::from(day_bytes)
                    + CDS_MILLISECONDS_BYTES
                    + usize::from(submillisecond_bytes);
                let bytes = value.to_be_bytes();
                let start = bytes.len().checked_sub(width)?;
                cds_from_bytes(&bytes[start..], day_bytes, submillisecond_bytes, epoch)
            }
            RawValue::Signed(_) | RawValue::Float(_) => None,
        },
        TimeFormat::Seconds { epoch } => eng
            .as_f64()
            .and_then(|seconds| seconds_from_f64(seconds, epoch)),
    }
}

/// A resolved spacecraft clock: which parameter, and how to read it.
///
/// The name is resolved to a [`ParamId`] once, when the session starts. Looking it up per
/// packet would be a string hash on the hot path, and a name that does not resolve would be
/// discovered on the first packet instead of before the window opens.
#[derive(Clone, Copy, Debug)]
pub struct SpacecraftClock {
    parameter: ParamId,
    format: TimeFormat,
}

impl SpacecraftClock {
    /// Resolves the configured parameter name against the definition.
    ///
    /// A miss is a refused session and not a fall back to ground receipt: an operator who
    /// configured a clock and got receipt time anyway sees plots that are subtly wrong with
    /// nothing on the log to say so.
    ///
    /// # Errors
    ///
    /// [`EngineError::Config`] when no parameter of that name exists.
    pub fn resolve(db: &XtceDb, source: &TimeSource) -> Result<Self, EngineError> {
        let parameter = db.find_parameter(&source.parameter).ok_or_else(|| {
            EngineError::config(format!(
                "spacecraft_time.parameter: the definition declares no parameter named `{}`",
                source.parameter
            ))
        })?;
        Ok(Self::new(parameter, source.format))
    }

    /// Builds a clock for an already-resolved parameter.
    #[must_use]
    pub const fn new(parameter: ParamId, format: TimeFormat) -> Self {
        Self { parameter, format }
    }

    /// Which parameter carries the time.
    #[must_use]
    pub const fn parameter(self) -> ParamId {
        self.parameter
    }

    /// The layout its bits are in.
    #[must_use]
    pub const fn format(self) -> TimeFormat {
        self.format
    }

    /// The instant a decoded packet claims, if it carried the clock parameter.
    ///
    /// `None` means the packet does not contain that parameter — most packets on most
    /// missions do not — or that its value is not in the configured layout. The caller falls
    /// back to ground receipt; see [`xtce_gs_core::Batch::time`].
    #[must_use]
    pub fn read(self, packet: &DecodedPacket<'_, '_>) -> Option<Utc> {
        let value = packet.get(self.parameter)?;
        from_values(self.format, &value.raw, &value.eng)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    /// A CUC that is all zeroes is the epoch itself, whatever the widths.
    #[test]
    fn a_cuc_of_zero_is_the_ccsds_epoch_and_that_epoch_is_1958() {
        let time = cuc_from_bytes(&[0, 0, 0, 0, 0, 0], 4, 2, Utc::CCSDS_EPOCH).unwrap();
        assert_eq!(time.to_string(), "1958-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_cuc_with_zero_fine_bytes_is_whole_seconds() {
        // 4 383 days from 1958-01-01 to 1970-01-01, and 86 400 s in each: 378 691 200 s,
        // which is 0x1692_5E80.
        let time = cuc_from_bytes(&[0x16, 0x92, 0x5E, 0x80], 4, 0, Utc::CCSDS_EPOCH).unwrap();
        assert_eq!(time, Utc::EPOCH);
        assert_eq!(time.to_string(), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn the_fine_field_is_a_binary_fraction_not_a_decimal_one() {
        // One coarse second and a fine octet of 0x80: 128/256 s, which is exactly half.
        let time = cuc_from_bytes(&[0, 0, 0, 1, 0x80], 4, 1, Utc::EPOCH).unwrap();
        assert_eq!(time.unix_nanos(), 1_500_000_000);
    }

    #[test]
    fn a_three_octet_fraction_truncates_toward_the_second_it_is_in() {
        // 0xFFFFFF / 2^24 s = 16 777 215 * 1e9 >> 24 ns = 999 999 940 ns, floored.
        let time = cuc_from_bytes(&[0, 0xFF, 0xFF, 0xFF], 1, 3, Utc::EPOCH).unwrap();
        assert_eq!(time.unix_nanos(), 999_999_940);
    }

    #[test]
    fn a_cuc_as_one_integer_splits_where_the_octets_would_have() {
        let bytes = [0x00, 0x00, 0x00, 0x2A, 0x40, 0x00];
        let from_bytes = cuc_from_bytes(&bytes, 4, 2, Utc::EPOCH).unwrap();
        // The same 48 bits: 42 seconds and 0x4000/65536 = a quarter.
        let from_integer = cuc_from_unsigned(0x0000_002A_4000, 4, 2, Utc::EPOCH).unwrap();
        assert_eq!(from_bytes, from_integer);
        assert_eq!(from_bytes.unix_nanos(), 42_250_000_000);
    }

    #[test]
    fn a_truncated_cuc_is_not_a_time() {
        // Five octets where the layout says six.
        assert_eq!(cuc_from_bytes(&[0, 0, 0, 1, 0x80], 4, 2, Utc::EPOCH), None);
        assert_eq!(cuc_from_bytes(&[], 1, 0, Utc::EPOCH), None);
    }

    #[test]
    fn a_cuc_wider_than_the_accumulator_or_with_no_seconds_is_refused() {
        let wide = [0u8; 16];
        assert_eq!(cuc_from_bytes(&wide, 8, 3, Utc::EPOCH), None);
        assert_eq!(cuc_from_bytes(&wide, 0, 4, Utc::EPOCH), None);
        assert_eq!(cuc_from_unsigned(u64::MAX, 8, 1, Utc::EPOCH), None);
        assert_eq!(cuc_from_unsigned(0, 0, 4, Utc::EPOCH), None);
    }

    #[test]
    fn a_cuc_field_may_be_followed_by_the_rest_of_the_parameter() {
        let time = cuc_from_bytes(&[0, 0, 0, 1, 0xDE, 0xAD], 4, 0, Utc::EPOCH).unwrap();
        assert_eq!(time.unix_nanos(), 1_000_000_000);
    }

    #[test]
    fn a_cds_day_and_millisecond_of_day_make_the_instant() {
        // 4 383 days from the CCSDS epoch is 1970-01-01; 3 600 000 ms of day is 01:00.
        let bytes = [0x11, 0x1F, 0x00, 0x36, 0xEE, 0x80];
        let time = cds_from_bytes(&bytes, 2, 0, Utc::CCSDS_EPOCH).unwrap();
        assert_eq!(time.to_string(), "1970-01-01T01:00:00.000Z");
        assert_eq!(time.unix_nanos(), 3_600 * 1_000_000_000);
    }

    #[test]
    fn microseconds_and_picoseconds_are_a_factor_of_a_million_apart() {
        // 123 microseconds within the millisecond, in the two-octet field.
        let micros = [0, 0, 0, 0, 0, 0, 0x00, 0x7B];
        // The same 123 microseconds as picoseconds: 123 000 000 of them, 0x0754_D4C0.
        let picos = [0, 0, 0, 0, 0, 0, 0x07, 0x54, 0xD4, 0xC0];
        let from_micros = cds_from_bytes(&micros, 2, 2, Utc::EPOCH).unwrap();
        let from_picos = cds_from_bytes(&picos, 2, 4, Utc::EPOCH).unwrap();
        assert_eq!(from_micros.unix_nanos(), 123_000);
        assert_eq!(from_micros, from_picos);
    }

    #[test]
    fn a_submillisecond_width_of_three_is_not_a_cds_field() {
        let bytes = [0u8; 16];
        assert_eq!(cds_from_bytes(&bytes, 2, 3, Utc::EPOCH), None);
        assert_eq!(cds_from_bytes(&bytes, 2, 1, Utc::EPOCH), None);
        // And a day segment of nothing is not one either.
        assert_eq!(cds_from_bytes(&bytes, 0, 0, Utc::EPOCH), None);
    }

    #[test]
    fn a_truncated_cds_is_not_a_time() {
        // Two day octets and four of milliseconds is six; five is one short.
        assert_eq!(cds_from_bytes(&[0, 0, 0, 0, 0], 2, 0, Utc::EPOCH), None);
        // And the submillisecond field counts toward the width.
        assert_eq!(cds_from_bytes(&[0; 6], 2, 2, Utc::EPOCH), None);
    }

    #[test]
    fn the_leap_second_is_inside_the_day_and_the_second_after_it_is_not() {
        // 86 400 000 ms: the leap second CCSDS 301.0-B-4 section 3.3.3 allows.
        let leap = [0, 0, 0x05, 0x26, 0x5C, 0x00];
        assert_eq!(be_unsigned(&leap[2..]), 86_400_000);
        assert!(cds_from_bytes(&leap, 2, 0, Utc::EPOCH).is_some());
        // 86 401 000 ms is a day and a second, which no day holds.
        let beyond = [0, 0, 0x05, 0x26, 0x5F, 0xE8];
        assert_eq!(be_unsigned(&beyond[2..]), 86_401_000);
        assert_eq!(cds_from_bytes(&beyond, 2, 0, Utc::EPOCH), None);
    }

    #[test]
    fn an_instant_outside_the_range_utc_names_is_none_not_a_saturated_stamp() {
        // A three-octet day count of 0xFFFFFF is 16.7 million days: the year 47 000.
        let far = [0xFF, 0xFF, 0xFF, 0, 0, 0, 0];
        assert_eq!(cds_from_bytes(&far, 3, 0, Utc::CCSDS_EPOCH), None);
        // Not the saturated instant `Utc::offset_nanos` would have produced.
        assert_ne!(
            cds_from_bytes(&far, 3, 0, Utc::CCSDS_EPOCH),
            Some(Utc::from_unix_nanos(i64::MAX))
        );
    }

    #[test]
    fn seconds_carry_their_fraction_into_nanoseconds() {
        let time = seconds_from_f64(1.5, Utc::EPOCH).unwrap();
        assert_eq!(time.unix_nanos(), 1_500_000_000);
        // And a negative count runs back from the epoch.
        let time = seconds_from_f64(-0.25, Utc::EPOCH).unwrap();
        assert_eq!(time.unix_nanos(), -250_000_000);
    }

    #[test]
    fn seconds_that_are_not_a_number_or_not_in_range_are_none() {
        assert_eq!(seconds_from_f64(f64::NAN, Utc::EPOCH), None);
        assert_eq!(seconds_from_f64(f64::INFINITY, Utc::EPOCH), None);
        assert_eq!(seconds_from_f64(f64::NEG_INFINITY, Utc::EPOCH), None);
        assert_eq!(seconds_from_f64(1e30, Utc::EPOCH), None);
        // In range as nanoseconds, out of range once the epoch is added.
        assert_eq!(
            seconds_from_f64(9.0e9, Utc::from_unix_nanos(i64::MAX)),
            None
        );
    }

    #[test]
    fn a_cuc_reads_the_raw_value_and_seconds_read_the_engineering_one() {
        let cuc = TimeFormat::Cuc {
            coarse_bytes: 4,
            fine_bytes: 0,
            epoch: Utc::EPOCH,
        };
        let raw = RawValue::Bytes(Cow::Borrowed(&[0, 0, 0, 7]));
        // The engineering value is a calibrated nonsense number and must be ignored.
        let eng = EngValue::Float(-273.15);
        assert_eq!(
            from_values(cuc, &raw, &eng).map(Utc::unix_nanos),
            Some(7_000_000_000)
        );

        let seconds = TimeFormat::Seconds { epoch: Utc::EPOCH };
        let raw = RawValue::Unsigned(7);
        let eng = EngValue::Float(2.5);
        assert_eq!(
            from_values(seconds, &raw, &eng).map(Utc::unix_nanos),
            Some(2_500_000_000)
        );
    }

    #[test]
    fn a_cds_that_arrived_as_one_integer_reads_like_its_octets() {
        let format = TimeFormat::Cds {
            day_bytes: 2,
            submillisecond_bytes: 0,
            epoch: Utc::CCSDS_EPOCH,
        };
        let integer = 0x111F_0036_EE80u64;
        let eng = EngValue::Unsigned(integer);
        let from_integer = from_values(format, &RawValue::Unsigned(integer), &eng).unwrap();
        let bytes = [0x11, 0x1F, 0x00, 0x36, 0xEE, 0x80];
        let from_bytes =
            from_values(format, &RawValue::Bytes(Cow::Borrowed(&bytes)), &eng).unwrap();
        assert_eq!(from_integer, from_bytes);
        assert_eq!(from_integer.to_string(), "1970-01-01T01:00:00.000Z");
    }

    #[test]
    fn a_time_parameter_that_decoded_to_a_label_is_not_a_time() {
        let cuc = TimeFormat::Cuc {
            coarse_bytes: 4,
            fine_bytes: 0,
            epoch: Utc::EPOCH,
        };
        assert_eq!(
            from_values(cuc, &RawValue::Float(1.0), &EngValue::Label("SAFE")),
            None
        );
        let seconds = TimeFormat::Seconds { epoch: Utc::EPOCH };
        assert_eq!(
            from_values(seconds, &RawValue::Unsigned(1), &EngValue::Label("SAFE")),
            None
        );
        assert_eq!(
            from_values(
                seconds,
                &RawValue::Unsigned(1),
                &EngValue::Bytes(Cow::Borrowed(&[0, 1]))
            ),
            None
        );
    }
}
