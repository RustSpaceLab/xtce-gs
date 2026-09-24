//! Bytes in, CCSDS space packets out.
//!
//! Everything between a socket and a packet lives here: finding frames in a stream that has
//! no boundaries, undoing the channel coding a spacecraft applied on the way down, and
//! putting back together a packet that arrived in three pieces. Nothing in this crate has
//! read an XTCE definition, and nothing in it knows what a parameter is — the only thing it
//! produces is [`xtce_gs_core::RawPacket`], which the engine decodes.
//!
//! ```no_run
//! # async fn example() -> Result<(), xtce_gs_link::LinkError> {
//! use std::sync::Arc;
//! use xtce_gs_core::{LinkStats, RawPacket, Utc};
//! use xtce_gs_link::{Pipeline, PipelineConfig, Source, SourceSpec};
//!
//! let spec: SourceSpec = "udp://0.0.0.0:10015".parse()?;
//! let mut source = Source::connect(&spec).await?;
//! let mut pipeline = Pipeline::new(PipelineConfig::default(), Arc::new(LinkStats::new()));
//!
//! let mut bytes = Vec::new();
//! let mut packets: Vec<RawPacket> = Vec::new();
//! while source.read_chunk(&mut bytes).await? > 0 {
//!     pipeline.push(&bytes, Utc::now(), &mut packets);
//!     bytes.clear();
//!     if source.is_datagram() {
//!         pipeline.flush_message();
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # What this crate refuses
//!
//! * **It does not transmit.** Every type here reads. Uplink is `xtce-flight`'s half of the
//!   problem and needs a link that can transmit, which is a different conversation.
//! * **It does not guess.** A frame that fails its checksum is dropped and counted, a
//!   Reed-Solomon block that cannot be corrected is reported rather than repaired to
//!   something plausible, and a packet whose declared length exceeds
//!   [`PipelineConfig::max_packet_length`] is treated as a desync, not as a packet.
//! * **It does not panic.** It sits on a live downlink fed by hardware nobody here controls,
//!   so every failure is a `Result` or a counter.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// This crate reinterprets bit patterns for a living: a three-bit virtual channel id, an
// eleven-bit first-header pointer, a symbol in GF(256). Every `as` below narrows a field of
// known width with the mask that makes it exact immediately alongside, and flagging them one
// at a time would mean an `#[allow]` on most lines of `frame.rs`, `csp.rs` and `rs.rs`.
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

pub mod csp;
pub mod derand;
pub mod error;
pub mod frame;
pub mod packets;
pub mod pipeline;
pub mod rs;
pub mod source;
pub mod sync;

pub use csp::{CspError, CspHeader, CspVersion, crc32c};
pub use derand::{derandomize, randomize};
pub use error::LinkError;
pub use frame::{FrameError, FrameOptions, TmFrame, crc16_ccitt};
pub use packets::{PacketAssembler, PacketCounters, PacketStream, VirtualChannel};
pub use pipeline::{Framing, Pipeline, PipelineConfig, RsConfig};
pub use rs::{ReedSolomon, RsError};
pub use source::{
    FileReplay, RateLimiter, Source, SourceSpec, TcpListenSource, TcpSource, UdpSource,
};
pub use sync::{SyncCounters, SyncState, Synchronizer};
