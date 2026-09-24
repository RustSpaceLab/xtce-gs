//! Turning a value into the characters a row shows.
//!
//! Every function here writes into a `String` the caller owns instead of returning one. A
//! table is several hundred rows and a plot legend is another dozen, sixty times a second; a
//! `String` returned per cell is an allocation and a free per cell per frame, which is the
//! one cost an immediate-mode interface adds that a retained one does not have to pay. The
//! caller keeps one scratch buffer, clears it, and hands it back.
//!
//! Writing into a `String` cannot fail — `impl std::fmt::Write for String` returns `Ok` for
//! every call — so every `write!` here is discarded with `let _ =`. That is not a swallowed
//! error; it is the one `Write` implementation that has none, and the alternative spellings
//! (`expect`, `unwrap`) are denied by the crate root for the good reason that they are
//! indistinguishable from the ones that do fail.
//!
//! # What this module refuses
//!
//! * **It does not round for looks.** A float is printed at the precision it survives a
//!   round trip at, because an operator comparing a plot against a flight-software log needs
//!   the two to be the same number.
//! * **It does not invent units.** A type with no `<UnitSet>` gets no suffix. A guessed unit
//!   on a telemetry display is how a number in counts is read as a temperature.
//! * **It does not hide a length.** A binary value is shown as a prefix and its full byte
//!   count, never as a prefix alone.

use std::fmt::Write as _;

use xtce_gs_core::{LimitState, Severity, Utc, Value};
use xtce_model::{ParamId, XtceDb};

/// Bytes of a binary value shown before the length takes over.
///
/// Eight, because sixteen hex digits is about what fits a table cell next to a name and a
/// time, and because the first bytes of a blob are the header an operator is looking at.
pub const HEX_PREVIEW_BYTES: usize = 8;

/// Seconds after which a value's age is worth drawing the operator's eye.
///
/// Not a limit and not an error: a parameter that arrives once a minute is stale by this
/// measure and perfectly healthy. It only decides a colour.
pub const AGE_STALE_SECONDS: f64 = 5.0;

/// Significant digits [`float`] keeps.
///
/// Six. A telemetry float is a calibrator's output over an integer field of at most 32 bits,
/// so the seventh digit is the polynomial's rounding and not the measurement — and printing
/// it invites an operator to read a change that is not there. Six is also what fits a table
/// cell beside a name, a unit and an age.
pub const FLOAT_SIGNIFICANT_DIGITS: u32 = 6;

/// Decimal exponent at or above which [`float`] switches to scientific notation.
///
/// A number with more integer digits than [`FLOAT_SIGNIFICANT_DIGITS`] cannot be shown to
/// that many digits in positional form without printing digits it does not have.
const FLOAT_MAX_EXPONENT: i32 = 9;

/// Decimal exponent below which [`float`] switches to scientific notation.
///
/// Below 1e-4 the leading zeros cost more cell width than the digits do: `0.0001` is six
/// characters and the six significant digits after it are ten more.
const FLOAT_MIN_EXPONENT: i32 = -4;

/// Appends a value's display form to `out`.
///
/// Integers, booleans, text and enumeration labels print as themselves, unquoted — a table
/// cell is not a CSV field. A float prints through `{}`, which is Rust's shortest form that
/// reads back as the same `f64`, and deliberately *not* through a fixed number of decimals:
/// the engine exports the same value to CSV through `Display`, and a table that disagreed
/// with the export is a bug report nobody can reproduce. Use [`float`] where a fixed width
/// matters more than the round trip. Bytes go through [`hex`].
///
/// `out` is appended to and not cleared: a caller building "value unit" wants both in one
/// buffer.
pub fn value(out: &mut String, value: &Value) {
    match value {
        Value::Unsigned(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Signed(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Float(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Bool(flag) => {
            let _ = write!(out, "{flag}");
        }
        Value::Label(text) | Value::Text(text) => out.push_str(text),
        Value::Bytes(bytes) => hex(out, bytes, HEX_PREVIEW_BYTES),
    }
}

/// Appends a value and its unit to `out`.
///
/// The unit follows one space. A `None` unit — and an empty one, which is what an
/// `<UnitSet>` with a blank `form` produces — appends nothing at all: not "n/a", not a dash.
/// The column is read down, and a filler repeated three hundred times is three hundred
/// characters of noise.
pub fn value_with_unit(out: &mut String, value: &Value, unit: Option<&str>) {
    self::value(out, value);
    if let Some(unit) = unit
        && !unit.is_empty()
    {
        out.push(' ');
        out.push_str(unit);
    }
}

/// Appends a float to [`FLOAT_SIGNIFICANT_DIGITS`] significant digits.
///
/// The number of decimals is decided by magnitude rather than fixed, because a fixed count is
/// wrong at both ends: `{:.2}` turns a 3 mV offset into `0.00` and `{:.17}` turns a
/// temperature into seventeen characters of a quantity known to three. So the decimals are
/// whatever leaves six significant digits — `1.23457`, `123.457`, `123457` — trailing zeros
/// are trimmed so an integral value prints as `22` and not `22.000`. A magnitude below 1e-4,
/// or at 1e9 and above, goes to scientific notation rather than spending a cell on zeros:
/// positional form covers `1e-4 ..< 1e9`, and both ends are named by
/// [`FLOAT_MIN_EXPONENT`] and [`FLOAT_MAX_EXPONENT`]. Inside that range the integer part is
/// never shortened — `999999999` keeps its nine digits — because six significant figures of
/// an integer is a value nothing measured.
///
/// Not what the table's value column uses: see [`value`] for why that one round-trips.
/// Non-finite values print as `NaN`, `inf` and `-inf`, and both zeros print as `0`.
pub fn float(out: &mut String, value: f64) {
    if !value.is_finite() {
        let _ = write!(out, "{value}");
        return;
    }
    if value == 0.0 {
        out.push('0');
        return;
    }

    // `log10` lands on the wrong side of a decade boundary often enough to matter — the
    // exponent of 1000.0 comes back as 2.9999999999999996 on some inputs — so it is a guess
    // that is then corrected against the powers themselves.
    let magnitude = value.abs();
    let mut exponent = magnitude.log10().floor() as i32;
    if 10f64.powi(exponent) > magnitude {
        exponent -= 1;
    } else if 10f64.powi(exponent + 1) <= magnitude {
        exponent += 1;
    }

    if !(FLOAT_MIN_EXPONENT..FLOAT_MAX_EXPONENT).contains(&exponent) {
        let _ = write!(out, "{value:.*e}", FLOAT_SIGNIFICANT_DIGITS as usize - 1);
        return;
    }

    let decimals = (FLOAT_SIGNIFICANT_DIGITS as i32 - 1 - exponent).clamp(0, 12) as usize;
    let start = out.len();
    let _ = write!(out, "{value:.decimals$}");
    trim_trailing_zeros(out, start);
}

/// Drops the trailing zeros, and then a bare decimal point, from what was written at `start`.
fn trim_trailing_zeros(out: &mut String, start: usize) {
    let Some(written) = out.get(start..) else {
        return;
    };
    if !written.contains('.') {
        return;
    }
    let trimmed = written.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    let len = start + trimmed.len();
    out.truncate(len);
}

/// Appends bytes as hex, truncated to `preview` bytes.
///
/// Lowercase, no separators, then `…` and the full length when there were more.
/// [`xtce_gs_core::Value`]'s own `Display` does the same thing with its own constant; this
/// one takes the budget because a tooltip can afford more of the blob than a cell can.
pub fn hex(out: &mut String, bytes: &[u8], preview: usize) {
    for byte in bytes.iter().take(preview) {
        let _ = write!(out, "{byte:02x}");
    }
    if bytes.len() > preview {
        let _ = write!(out, "… ({} bytes)", bytes.len());
    }
}

/// Appends the time between two instants as an age.
///
/// The largest unit that keeps the number short — `0.4 s`, `12 s`, `1.5 m`, `26 h`, `3.2 d` —
/// with one decimal below ten of that unit and none at or above it, which is two or three
/// significant figures either way.
///
/// A *negative* age — a spacecraft clock ahead of the ground, which a wrong epoch produces
/// immediately — prints with its sign rather than saturating at zero, because that is the
/// display that shows the operator the epoch is wrong on the first packet instead of on the
/// first plot.
pub fn age(out: &mut String, now: Utc, then: Utc) {
    const MINUTE: f64 = 60.0;
    const HOUR: f64 = 3600.0;
    const DAY: f64 = 86_400.0;

    let seconds = now.secs_since(then);
    let magnitude = seconds.abs();
    let (scaled, suffix) = if magnitude < MINUTE {
        (seconds, "s")
    } else if magnitude < HOUR {
        (seconds / MINUTE, "m")
    } else if magnitude < DAY {
        (seconds / HOUR, "h")
    } else {
        (seconds / DAY, "d")
    };
    if scaled.abs() < 10.0 {
        let _ = write!(out, "{scaled:.1} {suffix}");
    } else {
        let _ = write!(out, "{scaled:.0} {suffix}");
    }
}

/// Appends an instant as a time of day.
///
/// `hh:mm:ss.mmm`, with no date. The date is on the status bar once; repeating it in every
/// row and on every axis tick costs the ten characters that the value needs. [`Utc`]'s own
/// `Display` is the full ISO-8601 form and is what the event log and the CSV use.
pub fn clock(out: &mut String, time: Utc) {
    let (_, _, _, hour, minute, second, nanos) = time.civil();
    let _ = write!(
        out,
        "{hour:02}:{minute:02}:{second:02}.{:03}",
        nanos / 1_000_000
    );
}

/// Appends a counter in an abbreviated form.
///
/// Plain digits below 10 000 — four digits need no grouping — then `k`, `M`, `G`, `T`, `P`,
/// `E` with one decimal. The status bar shows eighteen counters side by side and a raw
/// `1743920155` in one of them makes the row jump every time it gains a digit. The ladder
/// runs to `E` so that a byte counter at `u64::MAX` is `18.4E` and not a column-widening
/// `18446744073.7G`.
pub fn count(out: &mut String, value: u64) {
    const LADDER: [(u64, char); 6] = [
        (1_000, 'k'),
        (1_000_000, 'M'),
        (1_000_000_000, 'G'),
        (1_000_000_000_000, 'T'),
        (1_000_000_000_000_000, 'P'),
        (1_000_000_000_000_000_000, 'E'),
    ];

    if value < 10_000 {
        let _ = write!(out, "{value}");
        return;
    }
    // The largest step the value is at least one of; the table is short enough to walk.
    let mut chosen = LADDER[0];
    for step in LADDER {
        if value >= step.0 {
            chosen = step;
        }
    }
    let (step, suffix) = chosen;
    let _ = write!(out, "{:.1}{suffix}", value as f64 / step as f64);
}

/// Appends a byte count with a binary-multiple suffix.
///
/// KiB, MiB, GiB and up, with one decimal, because a recording is compared against a file
/// size the operating system reports in the same units. Below a kibibyte the count is exact.
pub fn bytes(out: &mut String, value: u64) {
    const LADDER: [(u64, &str); 6] = [
        (1 << 10, "KiB"),
        (1 << 20, "MiB"),
        (1 << 30, "GiB"),
        (1 << 40, "TiB"),
        (1 << 50, "PiB"),
        (1 << 60, "EiB"),
    ];

    if value < 1024 {
        let _ = write!(out, "{value} B");
        return;
    }
    let mut chosen = LADDER[0];
    for step in LADDER {
        if value >= step.0 {
            chosen = step;
        }
    }
    let (step, suffix) = chosen;
    let _ = write!(out, "{:.1} {suffix}", value as f64 / step as f64);
}

/// Appends a fraction as a percentage.
///
/// Two decimals below one percent, one above. Frame loss is the number an operator points an
/// antenna by, and `0 %` versus `0.03 %` is the difference between a clean pass and one
/// packet in three thousand gone.
///
/// A non-finite fraction prints as [`LimitState::Unknown`]'s dash: a loss ratio is
/// `dropped / seen`, which is `0.0 / 0.0` before the first frame arrives, and `NaN %` on the
/// status bar reads as a fault rather than as the absence of a measurement.
pub fn percent(out: &mut String, fraction: f64) {
    if !fraction.is_finite() {
        out.push_str(LimitState::Unknown.label());
        return;
    }
    let value = fraction * 100.0;
    if fraction.abs() < 0.01 {
        let _ = write!(out, "{value:.2} %");
    } else {
        let _ = write!(out, "{value:.1} %");
    }
}

/// Whether `haystack` contains `needle`, ignoring ASCII case.
///
/// The comparison both the parameter list and the value table filter with. ASCII-only
/// because an XTCE name is an XML `NCName` used as a C identifier by every code generator
/// that reads one, so a name with a Turkish dotless i in it is a name nothing downstream can
/// compile — and a Unicode-correct fold would mean lowercasing 9 493 names per keystroke.
/// A non-ASCII needle still matches, exactly.
///
/// An empty needle matches everything, which is what an empty filter box has to mean.
#[must_use]
pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// The parameter's leaf name, or `""` when the id is not from this definition.
///
/// A `ParamId` this definition does not contain returns `""` rather than panicking: the
/// store is sized from the definition, so it cannot happen — and a display that would abort
/// the interface if it did is not worth the assertion.
#[must_use]
pub fn name_of(db: &XtceDb, parameter: ParamId) -> &str {
    db.parameter(parameter)
        .map_or("", |parameter| db.name(parameter.name))
}

/// The parameter's fully qualified name, or `""` when the id is not from this definition.
///
/// This is the key the limits file and the saved layout use — both outlive any one build of
/// the definition, and an arena index does not.
#[must_use]
pub fn qualified_name_of(db: &XtceDb, parameter: ParamId) -> &str {
    db.parameter(parameter)
        .map_or("", |parameter| db.name(parameter.qualified_name))
}

/// The first declared unit of a parameter's type, if it has one.
///
/// XTCE permits compound units — `<UnitSet>` is a list — and this shows the first and drops
/// the rest, because a column header wide enough for `kg·m/s²` is a column that pushed the
/// value off the screen. The tooltip is where the full list belongs.
#[must_use]
pub fn unit_of(db: &XtceDb, parameter: ParamId) -> Option<&str> {
    let kind = db.type_of(parameter)?;
    kind.units.first().map(|unit| db.name(*unit))
}

/// The colour a limit state is drawn in, for the theme in use.
///
/// [`LimitState::Nominal`] is the ordinary text colour: a value inside its limits must not be
/// coloured green, or the table becomes a colour the eye stops reading. Taking the other
/// three from the visuals rather than naming RGB is what keeps the light theme legible.
#[must_use]
pub fn limit_color(state: LimitState, visuals: &egui::Visuals) -> egui::Color32 {
    match state {
        LimitState::Unknown => visuals.weak_text_color(),
        LimitState::Nominal => visuals.text_color(),
        LimitState::Warning => visuals.warn_fg_color,
        LimitState::Alarm => visuals.error_fg_color,
    }
}

/// The colour an event severity is drawn in, for the theme in use.
///
/// The same three colours as [`limit_color`], by severity. One function each because the two
/// scales are not the same scale: an [`Severity::Info`] line is ordinary, a
/// [`LimitState::Nominal`] value is a value that was checked.
#[must_use]
pub fn severity_color(severity: Severity, visuals: &egui::Visuals) -> egui::Color32 {
    match severity {
        Severity::Info => visuals.text_color(),
        Severity::Warning => visuals.warn_fg_color,
        Severity::Error => visuals.error_fg_color,
    }
}

/// The literal an operator reads when a parameter has never arrived.
///
/// Kept here rather than written at each call site so that the tree, the table and anything
/// that grows an age column later all say the same word.
pub const NEVER: &str = "never";

/// [`age`], for an instant that may not exist.
///
/// The case is a parameter the definition declares and the spacecraft has not sent — which
/// is the *first* question an operator asks of a definition they did not write, and the one
/// `ParameterStore::seen()` cannot answer, because it only yields parameters that have a
/// sample.
pub fn age_of(out: &mut String, now: Utc, then: Option<Utc>) {
    match then {
        Some(then) => age(out, now, then),
        None => out.push_str(NEVER),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn shown(write: impl FnOnce(&mut String)) -> String {
        let mut out = String::new();
        write(&mut out);
        out
    }

    #[test]
    fn a_float_value_keeps_every_digit_it_round_trips_at() {
        // The engineering column and the CSV export must be the same characters.
        let value = shown(|out| self::value(out, &Value::Float(0.1 + 0.2)));
        assert_eq!(value, "0.30000000000000004");
        assert_eq!(value.parse::<f64>(), Ok(0.1 + 0.2));
    }

    #[test]
    fn a_label_is_not_quoted() {
        assert_eq!(
            shown(|out| value(out, &Value::Label(Arc::from("SAFE")))),
            "SAFE"
        );
    }

    #[test]
    fn a_value_with_no_unit_gains_no_filler() {
        assert_eq!(
            shown(|out| value_with_unit(out, &Value::Unsigned(7), None)),
            "7"
        );
        assert_eq!(
            shown(|out| value_with_unit(out, &Value::Unsigned(7), Some(""))),
            "7"
        );
        assert_eq!(
            shown(|out| value_with_unit(out, &Value::Unsigned(7), Some("K"))),
            "7 K"
        );
    }

    #[test]
    fn a_float_is_shown_to_six_significant_digits_whatever_its_magnitude() {
        assert_eq!(shown(|out| float(out, 1.234_567_89)), "1.23457");
        assert_eq!(shown(|out| float(out, 123.456_789)), "123.457");
        assert_eq!(shown(|out| float(out, 123_456.789)), "123457");
        assert_eq!(shown(|out| float(out, 0.001_234_567_8)), "0.00123457");
    }

    #[test]
    fn an_integral_float_does_not_print_its_zeros() {
        assert_eq!(shown(|out| float(out, 22.0)), "22");
        assert_eq!(shown(|out| float(out, -3.5)), "-3.5");
    }

    #[test]
    fn a_float_at_a_decade_boundary_does_not_slip_a_digit() {
        // `log10(1000.0).floor()` is the case that argues for correcting the guess.
        assert_eq!(shown(|out| float(out, 1000.0)), "1000");
        assert_eq!(shown(|out| float(out, 999.999_9)), "1000");
        assert_eq!(shown(|out| float(out, 0.1)), "0.1");
    }

    #[test]
    fn a_float_outside_the_positional_range_goes_to_an_exponent() {
        assert_eq!(shown(|out| float(out, 1e300)), "1.00000e300");
        assert_eq!(shown(|out| float(out, 1e-300)), "1.00000e-300");
        assert_eq!(shown(|out| float(out, 2.5e9)), "2.50000e9");
    }

    #[test]
    fn the_positional_range_ends_where_the_leading_zeros_win() {
        // 1e-4 is the last magnitude worth spelling out; below it the zeros outnumber the
        // digits and cost more of the cell than the exponent does.
        assert_eq!(shown(|out| float(out, 1e-4)), "0.0001");
        assert_eq!(shown(|out| float(out, 9.9e-5)), "9.90000e-5");
        // The last positional decade prints its integer part whole: nine digits is what the
        // number is, and rounding an integer to six would report a value nothing measured.
        assert_eq!(shown(|out| float(out, 999_999_999.0)), "999999999");
        assert_eq!(shown(|out| float(out, 1e9)), "1.00000e9");
    }

    #[test]
    fn both_zeros_and_the_non_finite_floats_are_printable() {
        assert_eq!(shown(|out| float(out, 0.0)), "0");
        assert_eq!(shown(|out| float(out, -0.0)), "0");
        assert_eq!(shown(|out| float(out, f64::NAN)), "NaN");
        assert_eq!(shown(|out| float(out, f64::INFINITY)), "inf");
        assert_eq!(shown(|out| float(out, f64::NEG_INFINITY)), "-inf");
    }

    #[test]
    fn hex_shows_a_length_rather_than_a_truncated_prefix_alone() {
        let blob: Vec<u8> = (0..12).collect();
        assert_eq!(
            shown(|out| hex(out, &blob, 4)),
            "00010203… (12 bytes)",
            "a prefix with no length lets two different blobs look identical"
        );
        assert_eq!(shown(|out| hex(out, &blob, 12)), "000102030405060708090a0b");
        assert_eq!(shown(|out| hex(out, &blob, 99)), "000102030405060708090a0b");
    }

    #[test]
    fn hex_of_nothing_is_nothing() {
        assert_eq!(shown(|out| hex(out, &[], 8)), "");
        assert_eq!(shown(|out| hex(out, &[0xab], 0)), "… (1 bytes)");
    }

    #[test]
    fn an_age_picks_the_unit_that_keeps_the_number_short() {
        let now = Utc::from_unix_secs(1_000_000);
        let ago = |secs: f64| {
            let then = now.offset_nanos(-((secs * 1e9) as i64));
            shown(|out| age(out, now, then))
        };
        assert_eq!(ago(0.4), "0.4 s");
        assert_eq!(ago(12.0), "12 s");
        assert_eq!(ago(59.9), "60 s");
        assert_eq!(ago(60.0), "1.0 m");
        assert_eq!(ago(90.0), "1.5 m");
        assert_eq!(ago(1800.0), "30 m");
        assert_eq!(ago(3600.0), "1.0 h");
        assert_eq!(ago(86_400.0), "1.0 d");
    }

    #[test]
    fn an_age_from_a_clock_ahead_of_the_ground_keeps_its_sign() {
        // A wrong CUC epoch puts every packet in the future; saturating at zero would hide it
        // until the first plot came out empty.
        let now = Utc::from_unix_secs(1_000_000);
        let ahead = now.offset_nanos(5_000_000_000);
        assert_eq!(shown(|out| age(out, now, ahead)), "-5.0 s");
    }

    #[test]
    fn a_clock_before_the_epoch_is_not_a_negative_hour() {
        assert_eq!(shown(|out| clock(out, Utc::EPOCH)), "00:00:00.000");
        assert_eq!(
            shown(|out| clock(out, Utc::from_unix_nanos(-1))),
            "23:59:59.999"
        );
    }

    #[test]
    fn a_counter_stops_widening_the_column() {
        assert_eq!(shown(|out| count(out, 0)), "0");
        assert_eq!(shown(|out| count(out, 9_999)), "9999");
        assert_eq!(shown(|out| count(out, 10_000)), "10.0k");
        assert_eq!(shown(|out| count(out, 1_743_920_155)), "1.7G");
        assert_eq!(shown(|out| count(out, u64::MAX)), "18.4E");
    }

    #[test]
    fn a_byte_count_is_in_the_units_the_operating_system_reports() {
        assert_eq!(shown(|out| bytes(out, 0)), "0 B");
        assert_eq!(shown(|out| bytes(out, 1023)), "1023 B");
        assert_eq!(shown(|out| bytes(out, 1024)), "1.0 KiB");
        assert_eq!(shown(|out| bytes(out, 5 * (1 << 20))), "5.0 MiB");
        assert_eq!(shown(|out| bytes(out, u64::MAX)), "16.0 EiB");
    }

    #[test]
    fn a_loss_ratio_keeps_the_digits_that_decide_a_pass() {
        assert_eq!(shown(|out| percent(out, 0.0)), "0.00 %");
        assert_eq!(shown(|out| percent(out, 0.000_3)), "0.03 %");
        assert_eq!(shown(|out| percent(out, 0.01)), "1.0 %");
        assert_eq!(shown(|out| percent(out, 1.0)), "100.0 %");
        assert_eq!(shown(|out| percent(out, -0.02)), "-2.0 %");
    }

    #[test]
    fn a_ratio_of_nothing_over_nothing_is_not_a_fault() {
        // `frames_dropped / frames_seen` before the first frame: zero over zero.
        let (dropped, seen) = (0.0_f64, 0.0_f64);
        assert_eq!(shown(|out| percent(out, dropped / seen)), "—");
        assert_eq!(shown(|out| percent(out, f64::INFINITY)), "—");
    }

    #[test]
    fn the_filter_ignores_case_but_not_order() {
        assert!(contains_ignore_ascii_case("/Sat/Thermal/TEMP_A", "temp"));
        assert!(contains_ignore_ascii_case("/Sat/Thermal/TEMP_A", "THERMAL"));
        assert!(contains_ignore_ascii_case("anything", ""));
        assert!(!contains_ignore_ascii_case("/Sat/TEMP", "temperature"));
        assert!(!contains_ignore_ascii_case("short", "much longer needle"));
        assert!(!contains_ignore_ascii_case("/Sat/A_TEMP", "tempa"));
    }

    #[test]
    fn a_parameter_id_from_another_definition_renders_as_nothing() {
        let db = XtceDb::from_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<SpaceSystem xmlns="http://www.omg.org/spec/XTCE/20180204" name="Sat">
  <TelemetryMetaData>
    <ParameterTypeSet>
      <IntegerParameterType name="Counts"><UnitSet/><IntegerDataEncoding sizeInBits="8" encoding="unsigned"/></IntegerParameterType>
      <FloatParameterType name="Kelvin" sizeInBits="32">
        <UnitSet><Unit form="calibrated">K</Unit><Unit form="calibrated">s</Unit></UnitSet>
        <FloatDataEncoding sizeInBits="32"/>
      </FloatParameterType>
    </ParameterTypeSet>
    <ParameterSet>
      <Parameter name="COUNT" parameterTypeRef="Counts"/>
      <Parameter name="TEMP_A" parameterTypeRef="Kelvin"/>
    </ParameterSet>
    <ContainerSet/>
  </TelemetryMetaData>
</SpaceSystem>"#,
        )
        .expect("the definition loads");

        let temp = db.find_parameter("TEMP_A").expect("TEMP_A is declared");
        assert_eq!(name_of(&db, temp), "TEMP_A");
        assert_eq!(qualified_name_of(&db, temp), "/Sat/TEMP_A");
        // XTCE allows compound units; the cell shows the first and the tooltip the rest.
        assert_eq!(unit_of(&db, temp), Some("K"));

        let count = db.find_parameter("COUNT").expect("COUNT is declared");
        assert_eq!(unit_of(&db, count), None);

        // A store is sized from the definition, so this cannot happen — and if it did, the
        // window must not go down over a label.
        let stray = ParamId::new(9_999);
        assert_eq!(name_of(&db, stray), "");
        assert_eq!(qualified_name_of(&db, stray), "");
        assert_eq!(unit_of(&db, stray), None);
    }

    #[test]
    fn a_parameter_that_has_never_arrived_says_so() {
        let now = Utc::from_unix_secs(1_700_000_000);
        let mut out = String::new();
        age_of(&mut out, now, None);
        assert_eq!(out, NEVER);

        out.clear();
        age_of(&mut out, now, Some(now.offset_nanos(-1_500_000_000)));
        assert_eq!(
            out, "1.5 s",
            "and an instant that exists formats as it always did"
        );
    }

    #[test]
    fn a_value_inside_its_limits_is_not_coloured() {
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            assert_eq!(
                limit_color(LimitState::Nominal, &visuals),
                visuals.text_color(),
                "a green table is a table the eye stops reading"
            );
            assert_eq!(
                limit_color(LimitState::Alarm, &visuals),
                visuals.error_fg_color
            );
            assert_eq!(
                limit_color(LimitState::Warning, &visuals),
                visuals.warn_fg_color
            );
            assert_eq!(
                limit_color(LimitState::Unknown, &visuals),
                visuals.weak_text_color()
            );
            assert_eq!(
                severity_color(Severity::Info, &visuals),
                visuals.text_color()
            );
            assert_eq!(
                severity_color(Severity::Error, &visuals),
                visuals.error_fg_color
            );
        }
    }
}
