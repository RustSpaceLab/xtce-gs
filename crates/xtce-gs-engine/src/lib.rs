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
//! # The decision the rest follows from
//!
//! `Decoder::decode` returns values that borrow the packet buffer *and* the definition.
//! Nothing with that type can sit in a history or cross a channel, so [`decode::project`] is
//! where the borrow ends and [`xtce_gs_core::Value`] is what it ends into. After that point
//! nothing in this workspace names a lifetime.
//!
//! # What this crate refuses
//!
//! * **It does not stop on a bad packet.** A packet the definition does not describe is a
//!   counter and a line in the log. A station that returned an error for one would stop on
//!   the first, and every real downlink has them.
//! * **It does not guess a time.** A configured spacecraft clock that cannot be resolved
//!   fails [`Session::start`]; a packet that does not carry it falls back to ground receipt
//!   and says which it used through [`xtce_gs_core::Batch::spacecraft`].
//! * **It does not archive.** [`record::Recorder`] appends bytes and nothing else — no index,
//!   no per-packet wrapper — so that a recording replays through the same source that made
//!   it. A parameter archive is a different program.
//! * **It does not draw.** Nothing here depends on egui, and the store is left in a state the
//!   interface can read at any instant rather than one it has to be told about.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// Spacecraft time is fixed-point arithmetic over fields of known width — a two-octet day
// count, a three-octet binary fraction — and a plot axis is `f64` where a timestamp is `i64`.
// Every `as` in this crate narrows or widens one of those with the width check that makes it
// exact alongside; flagging them one at a time would mean an `#[allow]` on most of
// `sctime.rs`.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
// Tests are allowed to assert loudly; the no-panic rule is about library code reached by a
// live downlink, not about test setup.
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
pub use session::{Session, Waker};
