//! Out-of-limits checking.
//!
//! XTCE has `<AlarmSet>`, `<DefaultAlarm>` and context alarms, and `xtce-model` does not model
//! any of them — the decoder it was written for has no use for a threshold. So limits here come
//! from a file the operator keeps, keyed by qualified parameter name, and the day the model
//! grows alarms this type gains a second constructor and the file becomes an override rather
//! than the only source.
//!
//! The states are ordered, `Unknown < Nominal < Warning < Alarm`, so a display summarising a
//! group of parameters takes the maximum and gets the answer an operator expects.

use std::collections::HashMap;

use crate::value::Value;

/// Where a value sits relative to its limits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum LimitState {
    /// No limit is defined for this parameter, or the value is not numeric.
    #[default]
    Unknown,
    /// Inside every limit.
    Nominal,
    /// Outside a warning limit.
    Warning,
    /// Outside an alarm limit.
    Alarm,
}

impl LimitState {
    /// Whether this state should draw the operator's eye.
    #[must_use]
    pub const fn is_violation(self) -> bool {
        matches!(self, Self::Warning | Self::Alarm)
    }

    /// A short label for a status column.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "—",
            Self::Nominal => "ok",
            Self::Warning => "WARN",
            Self::Alarm => "ALARM",
        }
    }
}

/// A range with either end optional.
///
/// Both ends are inclusive: XTCE's alarm ranges carry their own exclusivity flags, and a
/// ground station that guesses differently from the flight software reports an alarm the
/// spacecraft does not have. Inclusive is the choice; it is written down here so the day a
/// definition says otherwise, this is the line to change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Range {
    /// Values below this are outside.
    pub low: Option<f64>,
    /// Values above this are outside.
    pub high: Option<f64>,
}

impl Range {
    /// A range bounded at both ends.
    #[must_use]
    pub const fn new(low: f64, high: f64) -> Self {
        Self {
            low: Some(low),
            high: Some(high),
        }
    }

    /// Whether a value is inside this range.
    ///
    /// A NaN is inside nothing — it fails both comparisons, which is the answer that shows the
    /// operator a value it cannot check rather than a clean one.
    #[must_use]
    pub fn contains(&self, value: f64) -> bool {
        if value.is_nan() {
            return false;
        }
        self.low.is_none_or(|low| value >= low) && self.high.is_none_or(|high| value <= high)
    }

    /// Whether either end is set.
    #[must_use]
    pub const fn is_set(&self) -> bool {
        self.low.is_some() || self.high.is_some()
    }
}

/// The warning and alarm ranges for one parameter.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limit {
    /// The inner range: outside it is a warning.
    pub warning: Range,
    /// The outer range: outside it is an alarm.
    pub alarm: Range,
}

impl Limit {
    /// Where a value sits.
    ///
    /// The alarm range is checked first, so a file whose ranges are the wrong way round — the
    /// warning range wider than the alarm range — still reports the more serious state rather
    /// than silently reporting the milder one.
    #[must_use]
    pub fn evaluate(&self, value: f64) -> LimitState {
        if value.is_nan() {
            return LimitState::Unknown;
        }
        if self.alarm.is_set() && !self.alarm.contains(value) {
            return LimitState::Alarm;
        }
        if self.warning.is_set() && !self.warning.contains(value) {
            return LimitState::Warning;
        }
        if self.alarm.is_set() || self.warning.is_set() {
            LimitState::Nominal
        } else {
            LimitState::Unknown
        }
    }
}

/// Limits for a whole definition, keyed by qualified parameter name.
#[derive(Clone, Debug, Default)]
pub struct LimitSet {
    entries: HashMap<String, Limit>,
}

impl LimitSet {
    /// An empty set. Every parameter is [`LimitState::Unknown`] against it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the limit for a parameter.
    pub fn insert(&mut self, name: impl Into<String>, limit: Limit) {
        self.entries.insert(name.into(), limit);
    }

    /// The limit for a parameter, if one is defined.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Limit> {
        self.entries.get(name)
    }

    /// Where a value sits, by parameter name.
    ///
    /// A non-numeric value is [`LimitState::Unknown`] and not an error: an enumeration has no
    /// order, and a station that coloured a label by its raw integer would be colouring a
    /// number the operator is not looking at.
    #[must_use]
    pub fn evaluate(&self, name: &str, value: &Value) -> LimitState {
        let Some(limit) = self.entries.get(name) else {
            return LimitState::Unknown;
        };
        let Some(number) = value.as_f64() else {
            return LimitState::Unknown;
        };
        limit.evaluate(number)
    }

    /// Every parameter that has limits.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Limit)> {
        self.entries
            .iter()
            .map(|(name, limit)| (name.as_str(), limit))
    }

    /// How many parameters have limits.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no limits are defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit() -> Limit {
        Limit {
            warning: Range::new(-10.0, 50.0),
            alarm: Range::new(-20.0, 70.0),
        }
    }

    #[test]
    fn the_bands_nest() {
        let limit = limit();
        assert_eq!(limit.evaluate(20.0), LimitState::Nominal);
        assert_eq!(limit.evaluate(55.0), LimitState::Warning);
        assert_eq!(limit.evaluate(80.0), LimitState::Alarm);
        assert_eq!(limit.evaluate(-15.0), LimitState::Warning);
        assert_eq!(limit.evaluate(-25.0), LimitState::Alarm);
    }

    #[test]
    fn the_edges_are_inside() {
        let limit = limit();
        assert_eq!(limit.evaluate(50.0), LimitState::Nominal);
        assert_eq!(limit.evaluate(70.0), LimitState::Warning);
    }

    #[test]
    fn a_one_sided_range_only_bounds_one_side() {
        let limit = Limit {
            warning: Range {
                low: None,
                high: Some(100.0),
            },
            alarm: Range::default(),
        };
        assert_eq!(limit.evaluate(-1e30), LimitState::Nominal);
        assert_eq!(limit.evaluate(101.0), LimitState::Warning);
    }

    #[test]
    fn nan_is_unknown_not_nominal() {
        assert_eq!(limit().evaluate(f64::NAN), LimitState::Unknown);
        assert!(!Range::new(0.0, 1.0).contains(f64::NAN));
    }

    #[test]
    fn a_parameter_with_no_limit_is_unknown() {
        let mut set = LimitSet::new();
        set.insert("/Sat/TEMP", limit());
        assert_eq!(
            set.evaluate("/Sat/VOLTS", &Value::Float(3.3)),
            LimitState::Unknown
        );
        assert_eq!(
            set.evaluate("/Sat/TEMP", &Value::Float(3.3)),
            LimitState::Nominal
        );
    }

    #[test]
    fn a_label_is_not_coloured_by_its_raw_number() {
        let mut set = LimitSet::new();
        set.insert("/Sat/MODE", limit());
        assert_eq!(
            set.evaluate("/Sat/MODE", &Value::Label(std::sync::Arc::from("SAFE"))),
            LimitState::Unknown
        );
    }

    #[test]
    fn states_order_so_a_group_can_be_summarised() {
        let worst = [LimitState::Nominal, LimitState::Alarm, LimitState::Warning]
            .into_iter()
            .max();
        assert_eq!(worst, Some(LimitState::Alarm));
    }
}
