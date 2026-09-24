//! The spine every other crate in this workspace agrees on.
//!
//! `xtce-decode` hands back values that borrow twice over — `RawValue<'p>` from the packet
//! buffer, `EngValue<'db, 'p>` from the packet *and* the definition. That is the right shape
//! for a decoder whose output is consumed before the next packet arrives, and the wrong one
//! for a ground station, where a value has to outlive the datagram it came from, cross a
//! channel to another thread, and sit in a history that is redrawn sixty times a second.
//!
//! This crate is where the borrow ends. [`Value`] owns what it refers to, [`Sample`] pairs it
//! with a parameter and a time, [`Batch`] is one decoded packet, and [`ParameterStore`] is
//! what the interface reads. Nothing here knows about sockets, and nothing here draws.
//!
//! ```
//! use xtce_gs_core::{ParameterStore, Utc, Value};
//! use xtce_model::ParamId;
//!
//! let mut store = ParameterStore::new(4, 1024);
//! store.watch(ParamId::new(2));
//! // ... engine ingests batches ...
//! assert!(store.history(ParamId::new(2)).is_some());
//! assert!(store.history(ParamId::new(3)).is_none()); // not watched: no ring allocated
//! ```
//!
//! # Why the store is not one ring per parameter
//!
//! CTIM declares 9 493 parameters. A 4 096-point history for each is 600 MB of resident
//! memory for data nobody is looking at. So every parameter keeps its *latest* sample — that
//! is one enum wide, and the table needs it — and only a watched parameter gets a ring. The
//! interface decides what is watched when the operator opens a plot.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// A plot axis is `f64` and a counter is `u64`; converting between them is this crate's job,
// and the widths involved (a sample count, a unix timestamp in seconds) are nowhere near
// where `f64` stops being exact.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::float_cmp
    )
)]

pub mod decimate;
pub mod event;
pub mod limits;
pub mod packet;
pub mod ring;
pub mod sample;
pub mod stats;
pub mod store;
pub mod time;
pub mod value;

pub use decimate::{lttb, min_max_lttb, rdp};
pub use event::{Event, EventLog, Severity};
pub use limits::{Limit, LimitSet, LimitState, Range};
pub use packet::RawPacket;
pub use ring::RingBuffer;
pub use sample::{Batch, Sample};
pub use stats::{LinkStats, StatsSnapshot};
pub use store::{ParameterStore, Point};
pub use time::Utc;
pub use value::{Value, ValueKind};

#[cfg(test)]
mod assumptions {
    //! The two facts the whole thread layout rests on. If either stops being true, the
    //! engine's `Arc<XtceDb>` and the store behind one `RwLock` both stop compiling — and
    //! this is where the reason is stated, rather than in the error message that would
    //! follow.

    const fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn a_definition_can_be_shared_between_threads() {
        // Built once, never mutated, read by the decode thread and the interface at once.
        assert_send_sync::<xtce_model::XtceDb>();
    }

    #[test]
    fn a_sample_can_cross_a_channel() {
        // `Value` holds `Arc<str>` and `Arc<[u8]>`; those are `Send + Sync` only because the
        // contents are immutable. The decode thread makes them and the interface reads them.
        assert_send_sync::<crate::Sample>();
        assert_send_sync::<crate::Batch>();
        assert_send_sync::<crate::RawPacket>();
        assert_send_sync::<crate::ParameterStore>();
        assert_send_sync::<crate::LinkStats>();
    }
}
