//! What a source can refuse to do.
//!
//! Deliberately three variants. Everything that goes wrong *inside* the link — a frame that
//! fails its checksum, a Reed-Solomon block beyond repair, a packet lost to a gap in the
//! virtual channel frame count — is a counter on [`xtce_gs_core::LinkStats`] and a line in
//! the event log, not a `Result`. A ground station that returned an error for a bad frame
//! would stop on the first one, and a downlink with no bad frames is a downlink nobody is
//! pointing at a spacecraft.

/// A source could not be opened, read, or described.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The socket or the file said no.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// The configuration cannot be carried out: an unparseable source URL, a Reed-Solomon
    /// interleave the codec does not implement, a frame length of zero.
    #[error("{0}")]
    Config(String),

    /// The source has already ended and was read again.
    ///
    /// The *first* end of a stream is `Ok(0)` from [`crate::Source::read_chunk`], never this
    /// — a caller that stops on `Ok(0)` never sees `Ended`. It exists so that a caller which
    /// keeps reading past the end gets told, instead of spinning on an endless run of zeroes.
    #[error("the source has ended")]
    Ended,
}
