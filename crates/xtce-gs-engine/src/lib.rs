//! Packets in, a store the interface reads out.
//!
//! This crate is the middle of the station: it loads the definition, opens the source through
//! [`xtce_gs_link`], decodes what arrives against [`xtce_decode`], and files the result in an
//! [`xtce_gs_core::ParameterStore`] that another thread draws from. It owns the threads, and
//! it is the only place in the workspace where a borrowed decoded value is turned into an
//! owned one.
//!
//! ```no_run
//! # fn main() -> Result<(), xtce_gs_engine::EngineError> {
//! use xtce_gs_engine::{Session, SessionConfig};
//!
//! let runtime = tokio::runtime::Runtime::new()?;
//! let config = SessionConfig {
//!     definition: "mission.xml".into(),
//!     source: "udp://0.0.0.0:10015".parse()?,
//!     ..SessionConfig::default()
//! };
//!
//! let session = Session::start(config, runtime.handle(), None)?;
//! println!("{}", session.describe_source());
//! // ... the interface reads `session.store()` until the operator quits ...
//! session.shutdown();
//! # Ok(())
//! # }
//! ```
//!
//! `Decoder::decode` returns values that borrow the packet buffer *and* the definition, and
//! nothing with that type can sit in a history or cross a channel. [`decode::project`] is
//! where the borrow ends and [`xtce_gs_core::Value`] is what it ends into; past that point
//! nothing in this workspace names a lifetime.
//!
//! A packet the definition does not describe is a counter and a log line, not an error —
//! every real downlink has them. A recording is bytes and nothing else, so that it replays
//! through the same source that wrote it.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// Spacecraft time is fixed-point arithmetic over fields of known width, and every `as` here
// carries the width check that makes it exact.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
// The no-panic rule is about code a live downlink reaches, not about test setup.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp,
        clippy::unreadable_literal
    )
)]

pub mod config;
pub mod decode;
pub mod error;
pub mod record;
pub mod sctime;
pub mod session;

pub use config::{LimitEntry, LimitFile, SessionConfig, TimeFormat, TimeSource, load_limits};
pub use decode::{PacketDecoder, SequenceTracker, empty_batch, project};
pub use error::EngineError;
pub use record::{Recorder, write_batch, write_history, write_snapshot};
pub use sctime::SpacecraftClock;
pub use session::{Outcome, Session, Waker};
