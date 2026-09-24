//! Everything a session is told before it starts.
//!
//! One struct, built by the command line or by the interface, and validated once before
//! anything is opened. Nothing here has an effect on its own — [`crate::Session::start`] is
//! what acts on it — so a configuration that cannot work is a refused session with a sentence
//! saying why, rather than a window that shows an operator no telemetry and no reason.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use xtce_gs_core::{Limit, LimitSet, Range, Utc};
use xtce_gs_link::{PipelineConfig, SourceSpec};

use crate::error::EngineError;
use crate::sctime::MAX_TIME_BYTES;

/// Points of history kept per watched parameter when the caller does not say.
///
/// 4 096 points is 64 KB per plot, and a plot is a thousand pixels wide: four screens of
/// history at one sample a pixel.
pub const DEFAULT_HISTORY_DEPTH: usize = 4096;

/// Lines the event log holds when the caller does not say.
pub const DEFAULT_EVENT_CAPACITY: usize = 2048;

/// The UDP port a session binds when the caller does not name a source.
///
/// Arbitrary, and the same number `xtce-gs run` prints in its help: an operator who typed no
/// source gets a station listening somewhere rather than an error about a missing argument.
pub const DEFAULT_UDP_PORT: u16 = 10015;

/// How a spacecraft stamps its packets, and which parameter carries the stamp.
///
/// Optional on a session. Without it every sample is placed at ground receipt, which is right
/// for a live pass and wrong for a recorder dump — see [`xtce_gs_core::Batch::time`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TimeSource {
    /// Qualified or leaf parameter name, as [`xtce_model::XtceDb::find_parameter`] takes it.
    pub parameter: String,
    /// The byte layout the parameter's value is in.
    pub format: TimeFormat,
}

/// The three time codes a mission in reach actually sends.
///
/// CCSDS 301.0-B defines the first two; the third is not a CCSDS code at all but what an XTCE
/// `<AbsoluteTimeParameter>` usually decodes to once its calibrator has run — a count of
/// seconds — and pretending it is a CUC with zero fine bytes would lose the fraction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeFormat {
    /// CCSDS unsegmented, `coarse` seconds and `fine` sub-second bytes from `epoch`.
    Cuc {
        /// Octets of whole seconds, most significant first.
        coarse_bytes: u8,
        /// Octets of binary fraction below the second.
        fine_bytes: u8,
        /// What the count is measured from.
        epoch: Utc,
    },
    /// CCSDS day segmented.
    Cds {
        /// Octets of the day count: two for the 1958 epoch, three for a longer mission.
        day_bytes: u8,
        /// Octets below the millisecond: zero, two for microseconds, four for picoseconds.
        submillisecond_bytes: u8,
        /// What the day count is measured from.
        epoch: Utc,
    },
    /// The parameter is already seconds since an epoch (an XTCE `AbsoluteTime` often is).
    Seconds {
        /// What the count is measured from.
        epoch: Utc,
    },
}

impl TimeFormat {
    /// What the count in this format is measured from.
    #[must_use]
    pub const fn epoch(self) -> Utc {
        match self {
            Self::Cuc { epoch, .. } | Self::Cds { epoch, .. } | Self::Seconds { epoch } => epoch,
        }
    }

    /// How many bytes a field in this format occupies, when that is fixed.
    ///
    /// `None` for [`TimeFormat::Seconds`], which is a number and not a byte layout: the
    /// parameter's own encoding decides its width, and a check against it here would refuse a
    /// definition that is perfectly decodable.
    #[must_use]
    pub const fn field_bytes(self) -> Option<usize> {
        match self {
            Self::Cuc {
                coarse_bytes,
                fine_bytes,
                ..
            } => Some(coarse_bytes as usize + fine_bytes as usize),
            Self::Cds {
                day_bytes,
                submillisecond_bytes,
                ..
            } => Some(
                day_bytes as usize
                    + crate::sctime::CDS_MILLISECONDS_BYTES
                    + submillisecond_bytes as usize,
            ),
            Self::Seconds { .. } => None,
        }
    }
}

/// Everything one session needs.
///
/// Not serialisable, deliberately: it holds a [`SourceSpec`] and a [`PipelineConfig`], which
/// the link crate does not derive serde on, and the thing an operator saves between runs is a
/// layout, not a session. The command line builds this and the interface reads it back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionConfig {
    /// The XTCE document to decode against.
    pub definition: PathBuf,
    /// Which container to start decoding at. `None` takes the definition's only root.
    pub root_container: Option<String>,
    /// Where the bytes come from.
    pub source: SourceSpec,
    /// How those bytes are taken apart.
    pub pipeline: PipelineConfig,
    /// Points of history per watched parameter.
    pub history_depth: usize,
    /// Lines the event log holds.
    pub event_capacity: usize,
    /// A JSON limits file, keyed by qualified parameter name.
    pub limits: Option<PathBuf>,
    /// Append every received byte to this file.
    pub record: Option<PathBuf>,
    /// Which parameter carries spacecraft time, and in what layout.
    pub spacecraft_time: Option<TimeSource>,
}

impl Default for SessionConfig {
    /// A station listening on UDP for space packets, with no definition named.
    ///
    /// `definition` is left empty rather than guessed, so [`SessionConfig::validate`] refuses
    /// a config nobody finished filling in instead of failing later on a path that reads as a
    /// typo.
    fn default() -> Self {
        Self {
            definition: PathBuf::new(),
            root_container: None,
            source: SourceSpec::Udp(SocketAddr::from(([0, 0, 0, 0], DEFAULT_UDP_PORT))),
            pipeline: PipelineConfig::default(),
            history_depth: DEFAULT_HISTORY_DEPTH,
            event_capacity: DEFAULT_EVENT_CAPACITY,
            limits: None,
            record: None,
            spacecraft_time: None,
        }
    }
}

/// What a source is, for a message about the framing it can carry.
fn source_kind(source: &SourceSpec) -> &'static str {
    match source {
        SourceSpec::Udp(_) => "UDP",
        SourceSpec::TcpConnect(_) | SourceSpec::TcpListen(_) => "TCP",
        SourceSpec::File { .. } => "file",
    }
}

/// Checks a spacecraft clock's layout, which the byte readers in [`crate::sctime`] can only
/// report on one packet at a time.
///
/// Stricter than those readers on purpose: [`crate::sctime::cds_from_bytes`] refuses only a
/// day segment of nothing, while this refuses anything but the 16- or 24-bit segments
/// CCSDS 301.0-B-4 section 3.3 defines and the fourth octet a longer mission count would
/// need. A layout this accepts and the reader refuses would be a session that starts and
/// then stamps every packet with ground receipt.
fn validate_time_source(source: &TimeSource) -> Result<(), EngineError> {
    if source.parameter.trim().is_empty() {
        return Err(EngineError::config(
            "spacecraft_time.parameter: empty; name the parameter that carries the clock",
        ));
    }
    match source.format {
        TimeFormat::Cuc {
            coarse_bytes,
            fine_bytes,
            ..
        } => {
            if coarse_bytes == 0 {
                return Err(EngineError::config(
                    "spacecraft_time.format: a CUC with 0 coarse bytes counts no seconds; \
                     CCSDS 301.0-B-4 section 3.2 names one to four",
                ));
            }
            let width = usize::from(coarse_bytes) + usize::from(fine_bytes);
            if width > MAX_TIME_BYTES {
                return Err(EngineError::config(format!(
                    "spacecraft_time.format: a CUC of {coarse_bytes} coarse and {fine_bytes} \
                     fine bytes is {width} bytes wide, and this reads at most {MAX_TIME_BYTES}"
                )));
            }
        }
        TimeFormat::Cds {
            day_bytes,
            submillisecond_bytes,
            ..
        } => {
            if !(2..=4).contains(&day_bytes) {
                return Err(EngineError::config(format!(
                    "spacecraft_time.format: a CDS day segment of {day_bytes} bytes; \
                     CCSDS 301.0-B-4 section 3.3 defines 16 or 24 bits, and a fourth octet is \
                     the most a longer mission count can be"
                )));
            }
            if !matches!(submillisecond_bytes, 0 | 2 | 4) {
                return Err(EngineError::config(format!(
                    "spacecraft_time.format: a CDS submillisecond field of \
                     {submillisecond_bytes} bytes; CCSDS 301.0-B-4 section 3.3 defines 0, \
                     2 (microseconds) or 4 (picoseconds)"
                )));
            }
        }
        TimeFormat::Seconds { .. } => {}
    }
    Ok(())
}

impl SessionConfig {
    /// Checks the configuration before anything is opened.
    ///
    /// Called by [`crate::Session::start`] first, so that a mistake is a refused session the
    /// operator can read rather than a task that dies in the background. The paths are *not*
    /// checked for existence here: a file that vanishes between this call and the open is a
    /// race, and reporting the same failure in two places means reporting it differently in
    /// two places.
    ///
    /// # Errors
    ///
    /// [`EngineError::Config`] naming the field and the value that cannot be used, or
    /// [`EngineError::Link`] when [`PipelineConfig::validate`] refuses the framing.
    pub fn validate(&self) -> Result<(), EngineError> {
        if self.definition.as_os_str().is_empty() {
            return Err(EngineError::config(
                "definition: no XTCE document named; a session has nothing to decode against",
            ));
        }
        if self.history_depth == 0 {
            return Err(EngineError::config(
                "history_depth: 0; a ring of no points drops every sample it is given, and \
                 the operator would watch an empty plot with nothing on the log",
            ));
        }
        if self.event_capacity == 0 {
            return Err(EngineError::config(
                "event_capacity: 0; the log would throw away the line saying why",
            ));
        }
        if let Some(time) = self.spacecraft_time.as_ref() {
            validate_time_source(time)?;
        }
        // A CSP header carries no length field, so a CSP stream cannot be resynchronised
        // inside a byte stream: the framing has to come from somewhere else, and on a
        // datagram source it comes from the datagram.
        if self.pipeline.csp.is_some() && !matches!(self.source, SourceSpec::Udp(_)) {
            return Err(EngineError::config(format!(
                "pipeline.csp: set on a {} source; a CSP header has no length field, so a \
                 CSP stream can only be read where the message boundaries come from the \
                 transport — a UDP source",
                source_kind(&self.source)
            )));
        }
        self.pipeline.validate()?;
        Ok(())
    }
}

/// The JSON a limits file holds.
///
/// A file rather than the definition because `xtce-model` does not model `<AlarmSet>` — see
/// [`xtce_gs_core::limits`]. Keyed by qualified parameter name and not by index, because an
/// index is a property of one build of one definition and this file outlives both.
///
/// ```json
/// { "parameters": { "/Sat/TEMP": { "warning_low": -10, "warning_high": 50,
///                                  "alarm_low": -20, "alarm_high": 70 } } }
/// ```
///
/// The `parameters` wrapper may be left off and the entries written as the whole document;
/// see [`LimitFile::from_json`] for why the two cannot be confused.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitFile {
    /// Limits by qualified parameter name.
    pub parameters: HashMap<String, LimitEntry>,
}

/// One parameter's four thresholds.
///
/// Every end is optional, and an entry with none of them is a parameter that is listed and
/// unchecked — which is different from a parameter that is absent, and shows as
/// [`xtce_gs_core::LimitState::Unknown`] either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitEntry {
    /// Below this is a warning.
    pub warning_low: Option<f64>,
    /// Above this is a warning.
    pub warning_high: Option<f64>,
    /// Below this is an alarm.
    pub alarm_low: Option<f64>,
    /// Above this is an alarm.
    pub alarm_high: Option<f64>,
}

impl LimitEntry {
    /// The core limit this entry describes.
    #[must_use]
    pub const fn to_limit(self) -> Limit {
        Limit {
            warning: Range {
                low: self.warning_low,
                high: self.warning_high,
            },
            alarm: Range {
                low: self.alarm_low,
                high: self.alarm_high,
            },
        }
    }
}

/// What a JSON value is, for a message that says what was found instead.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// The member the entries may be wrapped in.
const PARAMETERS_KEY: &str = "parameters";

impl LimitFile {
    /// Parses a limits file.
    ///
    /// Two passes, because one `serde_json::from_str` into [`LimitFile`] would report a
    /// string where a number belongs as a line and a column, and an operator editing four
    /// thresholds per parameter by hand needs the parameter's name as well. The first pass is
    /// the syntax — that is where a line and a column mean something — and the second reads
    /// one entry at a time.
    ///
    /// The entries may be wrapped in a `parameters` member or stand as the whole document. A
    /// qualified parameter name starts with `/`, so a file of parameters is never mistaken
    /// for a wrapper.
    ///
    /// Unknown keys are refused by `deny_unknown_fields`, which is the point of it: a file
    /// that spells `warning_high` as `warn_high` would otherwise load, apply no limit, and
    /// look like a parameter the operator forgot.
    ///
    /// # Errors
    ///
    /// [`EngineError::Config`] naming the line and column the JSON failed at, or the
    /// parameter whose entry would not read.
    pub fn from_json(text: &str) -> Result<Self, EngineError> {
        let document: serde_json::Value = serde_json::from_str(text).map_err(|error| {
            EngineError::config(format!(
                "invalid JSON at line {}, column {}: {error}",
                error.line(),
                error.column()
            ))
        })?;
        let document = document.as_object().ok_or_else(|| {
            EngineError::config(format!(
                "expected an object of parameter names, found {}",
                json_kind(&document)
            ))
        })?;
        let entries = match document.get(PARAMETERS_KEY) {
            Some(wrapped) => {
                // `deny_unknown_fields` is what refuses a misspelled threshold inside an
                // entry; it cannot see this level, because the document is taken apart by
                // hand so that an entry's failure can name the entry. So the siblings of
                // `parameters` are refused here, for the same reason: a member nobody reads
                // is a member the operator thinks is doing something.
                if document.len() > 1 {
                    let unexpected: Vec<&str> = document
                        .keys()
                        .map(String::as_str)
                        .filter(|key| *key != PARAMETERS_KEY)
                        .collect();
                    return Err(EngineError::config(format!(
                        "unknown member{} beside `{PARAMETERS_KEY}`: {}",
                        if unexpected.len() == 1 { "" } else { "s" },
                        unexpected.join(", ")
                    )));
                }
                wrapped.as_object().ok_or_else(|| {
                    EngineError::config(format!(
                        "`{PARAMETERS_KEY}` is {}, not an object of parameter names",
                        json_kind(wrapped)
                    ))
                })?
            }
            None => document,
        };

        let mut parameters = HashMap::with_capacity(entries.len());
        for (name, value) in entries {
            let entry = LimitEntry::deserialize(value)
                .map_err(|error| EngineError::config(format!("parameter `{name}`: {error}")))?;
            parameters.insert(name.clone(), entry);
        }
        Ok(Self { parameters })
    }

    /// Turns the file into the set the store is checked against.
    ///
    /// A name that is in the file and not in the definition is kept rather than refused: the
    /// set is looked up by name, a parameter that never arrives is never checked, and
    /// refusing the file would mean one retired parameter stops a station from starting.
    #[must_use]
    pub fn into_limit_set(self) -> LimitSet {
        let mut set = LimitSet::new();
        for (name, entry) in self.parameters {
            set.insert(name, entry.to_limit());
        }
        set
    }
}

/// Reads a limits file from disk.
///
/// # Errors
///
/// [`EngineError::Io`] when the file cannot be read, [`EngineError::Config`] when its JSON
/// does not parse. The path is in the message either way — a station is started with a
/// relative path from a shell whose working directory nobody remembers.
pub fn load_limits(path: &Path) -> Result<LimitSet, EngineError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        // The path is rebuilt into the message rather than left to `io::Error`, which never
        // carries one: "No such file or directory" alone sends an operator looking for a file
        // whose name they cannot see.
        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    })?;
    let file = LimitFile::from_json(&text)
        .map_err(|error| EngineError::config(format!("{}: {error}", path.display())))?;
    Ok(file.into_limit_set())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionConfig {
        SessionConfig {
            definition: PathBuf::from("mission.xml"),
            ..SessionConfig::default()
        }
    }

    fn message(result: Result<(), EngineError>) -> String {
        match result {
            Ok(()) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn a_session_with_a_definition_and_the_defaults_is_accepted() {
        assert!(session().validate().is_ok());
    }

    #[test]
    fn a_config_nobody_finished_filling_in_is_refused() {
        let message = message(SessionConfig::default().validate());
        assert!(message.contains("definition"), "{message}");
    }

    #[test]
    fn a_history_depth_of_zero_names_the_field_and_the_value() {
        let config = SessionConfig {
            history_depth: 0,
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("history_depth: 0"), "{message}");
    }

    #[test]
    fn an_event_capacity_of_zero_is_refused() {
        let config = SessionConfig {
            event_capacity: 0,
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("event_capacity: 0"), "{message}");
    }

    #[test]
    fn a_clock_with_no_parameter_named_is_refused() {
        let config = SessionConfig {
            spacecraft_time: Some(TimeSource {
                parameter: "   ".into(),
                format: TimeFormat::Seconds {
                    epoch: Utc::CCSDS_EPOCH,
                },
            }),
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("spacecraft_time.parameter"), "{message}");
    }

    #[test]
    fn a_cuc_with_no_coarse_bytes_counts_no_seconds() {
        let config = SessionConfig {
            spacecraft_time: Some(TimeSource {
                parameter: "/Sat/TIME".into(),
                format: TimeFormat::Cuc {
                    coarse_bytes: 0,
                    fine_bytes: 2,
                    epoch: Utc::CCSDS_EPOCH,
                },
            }),
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("CCSDS 301.0-B-4 section 3.2"), "{message}");
    }

    #[test]
    fn a_cuc_wider_than_the_reader_is_refused_before_the_first_packet() {
        let config = SessionConfig {
            spacecraft_time: Some(TimeSource {
                parameter: "/Sat/TIME".into(),
                format: TimeFormat::Cuc {
                    coarse_bytes: 6,
                    fine_bytes: 4,
                    epoch: Utc::CCSDS_EPOCH,
                },
            }),
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("10 bytes wide"), "{message}");
    }

    #[test]
    fn a_cds_submillisecond_field_of_three_bytes_is_refused() {
        let clock = |submillisecond_bytes| TimeSource {
            parameter: "/Sat/TIME".into(),
            format: TimeFormat::Cds {
                day_bytes: 2,
                submillisecond_bytes,
                epoch: Utc::CCSDS_EPOCH,
            },
        };
        for width in [1u8, 3, 5, 8] {
            let config = SessionConfig {
                spacecraft_time: Some(clock(width)),
                ..session()
            };
            let message = message(config.validate());
            assert!(message.contains("submillisecond"), "{message}");
        }
        for width in [0u8, 2, 4] {
            let config = SessionConfig {
                spacecraft_time: Some(clock(width)),
                ..session()
            };
            assert!(config.validate().is_ok(), "{width} bytes should be a CDS");
        }
    }

    #[test]
    fn a_cds_day_segment_of_one_byte_is_not_a_day_segment() {
        let config = SessionConfig {
            spacecraft_time: Some(TimeSource {
                parameter: "/Sat/TIME".into(),
                format: TimeFormat::Cds {
                    day_bytes: 1,
                    submillisecond_bytes: 0,
                    epoch: Utc::CCSDS_EPOCH,
                },
            }),
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("day segment of 1 bytes"), "{message}");
    }

    #[test]
    fn csp_on_a_byte_stream_is_refused_with_the_reason() {
        let config = SessionConfig {
            source: SourceSpec::TcpConnect(SocketAddr::from(([127, 0, 0, 1], 10_015))),
            pipeline: PipelineConfig {
                csp: Some(xtce_gs_link::CspVersion::V1),
                ..PipelineConfig::default()
            },
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("no length field"), "{message}");
        // The same pipeline on a datagram source is fine: the datagram is the boundary.
        let config = SessionConfig {
            pipeline: PipelineConfig {
                csp: Some(xtce_gs_link::CspVersion::V1),
                ..PipelineConfig::default()
            },
            ..session()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_refused_pipeline_is_reported_in_the_pipelines_own_words() {
        let config = SessionConfig {
            pipeline: PipelineConfig {
                max_packet_length: 3,
                ..PipelineConfig::default()
            },
            ..session()
        };
        let message = message(config.validate());
        assert!(message.contains("max_packet_length"), "{message}");
    }

    #[test]
    fn a_good_limits_file_loads_every_threshold() {
        let text = r#"{ "parameters": {
            "/Sat/Bus/TEMP": { "warning_low": -10, "warning_high": 50,
                               "alarm_low": -20, "alarm_high": 70 } } }"#;
        let set = LimitFile::from_json(text).unwrap().into_limit_set();
        let limit = set.get("/Sat/Bus/TEMP").unwrap();
        assert_eq!(limit.warning, Range::new(-10.0, 50.0));
        assert_eq!(limit.alarm, Range::new(-20.0, 70.0));
    }

    #[test]
    fn the_parameters_wrapper_may_be_left_off() {
        let wrapped = r#"{"parameters": {"/Sat/TEMP": {"alarm_high": 70}}}"#;
        let bare = r#"{"/Sat/TEMP": {"alarm_high": 70}}"#;
        let one = LimitFile::from_json(wrapped).unwrap().into_limit_set();
        let other = LimitFile::from_json(bare).unwrap().into_limit_set();
        assert_eq!(one.get("/Sat/TEMP"), other.get("/Sat/TEMP"));
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn an_entry_may_set_only_an_upper_alarm() {
        let set = LimitFile::from_json(r#"{"/Sat/TEMP": {"alarm_high": 70}}"#)
            .unwrap()
            .into_limit_set();
        let limit = set.get("/Sat/TEMP").unwrap();
        assert_eq!(limit.alarm.high, Some(70.0));
        assert_eq!(limit.alarm.low, None);
        assert!(!limit.warning.is_set());
        // One-sided, so nothing below is a violation and 70 itself is inside.
        assert_eq!(limit.evaluate(-1e30), xtce_gs_core::LimitState::Nominal);
        assert_eq!(limit.evaluate(70.0), xtce_gs_core::LimitState::Nominal);
        assert_eq!(limit.evaluate(70.5), xtce_gs_core::LimitState::Alarm);
    }

    #[test]
    fn an_entry_with_no_thresholds_is_listed_and_unchecked() {
        let set = LimitFile::from_json(r#"{"/Sat/TEMP": {}}"#)
            .unwrap()
            .into_limit_set();
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.evaluate("/Sat/TEMP", &xtce_gs_core::Value::Float(3.3)),
            xtce_gs_core::LimitState::Unknown
        );
    }

    #[test]
    fn a_string_where_a_number_belongs_names_the_parameter() {
        let text = r#"{"/Sat/A": {"alarm_high": 70}, "/Sat/B": {"warning_low": "cold"}}"#;
        let message = match LimitFile::from_json(text) {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("/Sat/B"), "{message}");
        assert!(!message.contains("/Sat/A"), "{message}");
    }

    #[test]
    fn a_misspelled_threshold_is_refused_rather_than_silently_dropped() {
        let message = match LimitFile::from_json(r#"{"/Sat/TEMP": {"warn_high": 70}}"#) {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("/Sat/TEMP"), "{message}");
        assert!(message.contains("warn_high"), "{message}");
    }

    #[test]
    fn a_syntax_error_carries_the_line_and_the_column() {
        let text = "{\n  \"/Sat/TEMP\": { \"alarm_high\": 70,\n}\n";
        let message = match LimitFile::from_json(text) {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains("column"), "{message}");
    }

    #[test]
    fn a_file_that_is_not_an_object_says_what_it_found() {
        let message = match LimitFile::from_json("[1, 2, 3]") {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("an array"), "{message}");
        let message = match LimitFile::from_json(r#"{"parameters": 7}"#) {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("a number"), "{message}");
    }

    #[test]
    fn an_empty_file_is_no_limits_and_not_an_error() {
        assert!(
            LimitFile::from_json("{}")
                .unwrap()
                .into_limit_set()
                .is_empty()
        );
        assert!(
            LimitFile::from_json(r#"{"parameters": {}}"#)
                .unwrap()
                .into_limit_set()
                .is_empty()
        );
    }

    #[test]
    fn a_limits_file_that_is_not_there_is_named_in_the_message() {
        let path = Path::new("/nonexistent/xtce-gs/limits.json");
        let Err(error) = load_limits(path) else {
            panic!("expected a refusal")
        };
        let message = error.to_string();
        assert!(
            message.contains("/nonexistent/xtce-gs/limits.json"),
            "{message}"
        );
        // A missing file is the filesystem's refusal, not the operator's.
        assert!(matches!(error, EngineError::Io(_)), "{error:?}");
    }

    #[test]
    fn a_member_beside_the_wrapper_is_refused_rather_than_ignored() {
        let text = r#"{"parameters": {"/Sat/TEMP": {}}, "note": "for the next shift"}"#;
        let message = match LimitFile::from_json(text) {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("note"), "{message}");
    }

    #[test]
    fn a_bad_limits_file_on_disk_names_the_file_and_the_parameter() {
        let mut path = std::env::temp_dir();
        path.push(format!("xtce-gs-limits-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"/Sat/TEMP": {"alarm_high": "hot"}}"#).unwrap();
        let message = match load_limits(&path) {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        };
        std::fs::remove_file(&path).unwrap();
        assert!(message.contains("xtce-gs-limits-"), "{message}");
        assert!(message.contains("/Sat/TEMP"), "{message}");
    }

    #[test]
    fn a_good_limits_file_on_disk_round_trips() {
        let mut path = std::env::temp_dir();
        path.push(format!("xtce-gs-limits-ok-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"/Sat/TEMP": {"warning_high": 50}}"#).unwrap();
        let set = load_limits(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(set.get("/Sat/TEMP").unwrap().warning.high, Some(50.0));
    }

    #[test]
    fn the_field_widths_a_format_reports_are_the_ones_it_is_read_with() {
        let cuc = TimeFormat::Cuc {
            coarse_bytes: 4,
            fine_bytes: 2,
            epoch: Utc::CCSDS_EPOCH,
        };
        assert_eq!(cuc.field_bytes(), Some(6));
        let cds = TimeFormat::Cds {
            day_bytes: 2,
            submillisecond_bytes: 4,
            epoch: Utc::CCSDS_EPOCH,
        };
        assert_eq!(cds.field_bytes(), Some(10));
        assert_eq!(
            TimeFormat::Seconds { epoch: Utc::EPOCH }.field_bytes(),
            None
        );
        assert_eq!(cds.epoch(), Utc::CCSDS_EPOCH);
    }
}
