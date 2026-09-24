//! A parameter value that owns everything it refers to.

use std::fmt;
use std::sync::Arc;

use xtce_decode::{EngValue, RawValue};

/// What kind of value this is, without carrying it.
///
/// The interface uses this to decide whether a parameter can be plotted at all: a label or a
/// blob has no position on an axis.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueKind {
    /// An unsigned integer.
    Unsigned,
    /// A signed integer.
    Signed,
    /// A float.
    Float,
    /// A boolean.
    Bool,
    /// An enumeration label.
    Label,
    /// Text.
    Text,
    /// Raw bytes.
    Bytes,
}

impl ValueKind {
    /// Whether a value of this kind has a place on a numeric axis.
    ///
    /// A boolean does: it plots as 0 and 1, which is how an operator watches a relay.
    #[must_use]
    pub const fn is_plottable(self) -> bool {
        matches!(
            self,
            Self::Unsigned | Self::Signed | Self::Float | Self::Bool
        )
    }
}

/// A decoded value with no lifetime attached.
///
/// Text and bytes are behind an [`Arc`] rather than a `String`/`Vec`, because the interface
/// clones a value every time it puts one in a table row, and a refcount bump is not a copy.
#[derive(Clone, Debug)]
pub enum Value {
    /// An unsigned integer field.
    Unsigned(u64),
    /// A signed integer field.
    Signed(i64),
    /// A float, either encoded as one or produced by a calibrator.
    Float(f64),
    /// A boolean parameter's value.
    Bool(bool),
    /// An enumeration label, copied out of the definition.
    Label(Arc<str>),
    /// Text decoded from the packet.
    Text(Arc<str>),
    /// Binary data.
    Bytes(Arc<[u8]>),
}

impl Value {
    /// Takes ownership of a raw value.
    #[must_use]
    pub fn from_raw(raw: &RawValue<'_>) -> Self {
        match raw {
            RawValue::Unsigned(value) => Self::Unsigned(*value),
            RawValue::Signed(value) => Self::Signed(*value),
            RawValue::Float(value) => Self::Float(*value),
            RawValue::Bytes(bytes) => Self::Bytes(Arc::from(bytes.as_ref())),
        }
    }

    /// Takes ownership of an engineering value.
    #[must_use]
    pub fn from_eng(eng: &EngValue<'_, '_>) -> Self {
        match eng {
            EngValue::Unsigned(value) => Self::Unsigned(*value),
            EngValue::Signed(value) => Self::Signed(*value),
            EngValue::Float(value) => Self::Float(*value),
            EngValue::Bool(value) => Self::Bool(*value),
            EngValue::Label(text) => Self::Label(Arc::from(*text)),
            EngValue::Text(text) => Self::Text(Arc::from(text.as_ref())),
            EngValue::Bytes(bytes) => Self::Bytes(Arc::from(bytes.as_ref())),
        }
    }

    /// This value's kind.
    #[must_use]
    pub const fn kind(&self) -> ValueKind {
        match self {
            Self::Unsigned(_) => ValueKind::Unsigned,
            Self::Signed(_) => ValueKind::Signed,
            Self::Float(_) => ValueKind::Float,
            Self::Bool(_) => ValueKind::Bool,
            Self::Label(_) => ValueKind::Label,
            Self::Text(_) => ValueKind::Text,
            Self::Bytes(_) => ValueKind::Bytes,
        }
    }

    /// The value on a numeric axis, when it has one.
    ///
    /// Matches [`xtce_decode::EngValue::as_f64`], including `true` becoming `1.0`, so a plot
    /// and the decoder agree on what a boolean is worth.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Unsigned(value) => Some(*value as f64),
            Self::Signed(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            Self::Bool(value) => Some(f64::from(u8::from(*value))),
            Self::Label(_) | Self::Text(_) | Self::Bytes(_) => None,
        }
    }

    /// The value as text, when it is textual.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Label(text) | Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The bytes, when this is a binary value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

impl PartialEq for Value {
    /// Equality is per-variant and exact.
    ///
    /// Two floats that are both NaN are *not* equal, because a value that changed from NaN to
    /// NaN did not change, and the interface uses this comparison to decide whether a row is
    /// worth repainting — not to decide anything about the spacecraft.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unsigned(a), Self::Unsigned(b)) => a == b,
            (Self::Signed(a), Self::Signed(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Label(a), Self::Label(b)) | (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsigned(value) => write!(f, "{value}"),
            Self::Signed(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Label(text) | Self::Text(text) => write!(f, "{text}"),
            Self::Bytes(bytes) => {
                const PREVIEW: usize = 16;
                for byte in bytes.iter().take(PREVIEW) {
                    write!(f, "{byte:02x}")?;
                }
                if bytes.len() > PREVIEW {
                    write!(f, "… ({} bytes)", bytes.len())?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_values_survive_the_buffer_they_came_from() {
        let owned = {
            let packet = vec![b'O', b'K'];
            let eng = EngValue::Text(String::from_utf8_lossy(&packet));
            Value::from_eng(&eng)
        };
        assert_eq!(owned.as_str(), Some("OK"));
    }

    #[test]
    fn a_boolean_plots_as_zero_and_one() {
        assert_eq!(Value::Bool(true).as_f64(), Some(1.0));
        assert_eq!(Value::Bool(false).as_f64(), Some(0.0));
        assert!(Value::Label(Arc::from("SAFE")).as_f64().is_none());
    }

    #[test]
    fn nan_does_not_equal_nan() {
        assert_ne!(Value::Float(f64::NAN), Value::Float(f64::NAN));
        assert_eq!(Value::Float(1.5), Value::Float(1.5));
    }
}
