//! What a session can refuse to do.
//!
//! Five variants, and the boundary between them is where the failure came from rather than
//! what it means: the definition, the decoder, the link, the filesystem, or the operator's
//! own configuration. Everything that goes wrong *during* a session — a packet the definition
//! does not describe, a gap in a sequence count, a frame that failed its checksum — is a
//! counter on [`xtce_gs_core::LinkStats`] and a line in the event log. A ground station that
//! stopped on the first undescribed packet would stop on every real downlink.

/// A session could not be started, or a packet could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The XTCE definition could not be read or lowered.
    #[error("{0}")]
    Xtce(#[from] xtce_model::XtceError),

    /// A packet could not be decoded against the definition.
    ///
    /// Returned by [`crate::decode::PacketDecoder::decode_into`] and turned into an event by
    /// the decode task. It reaches a caller of [`crate::Session::start`] only when the root
    /// container named in the configuration does not exist.
    #[error("{0}")]
    Decode(#[from] xtce_decode::DecodeError),

    /// The source could not be opened, read, or configured.
    #[error("{0}")]
    Link(#[from] xtce_gs_link::LinkError),

    /// A file the session owns — the recording, the limits, a CSV export — said no.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// The configuration cannot be carried out.
    ///
    /// A history depth of zero, a limits file whose JSON does not parse, a spacecraft time
    /// parameter no definition declares. `serde_json::Error` is deliberately *not* a variant
    /// of its own: the contract fixes this enum at five, and a parse failure is something the
    /// operator wrote, which is what this variant means. The message carries the file, the
    /// line and the column, because "invalid configuration" sends someone reading JSON by
    /// eye.
    #[error("{0}")]
    Config(String),
}

impl EngineError {
    /// A configuration failure, naming what cannot be used.
    ///
    /// Every caller writes the same `EngineError::Config(format!(...))` without it.
    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }
}
