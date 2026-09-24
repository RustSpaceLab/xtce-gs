//! What one decoded packet becomes once it has stopped borrowing.

use xtce_model::{ContainerId, ParamId};

use crate::time::Utc;
use crate::value::Value;

/// One parameter's value at one instant.
#[derive(Clone, Debug)]
pub struct Sample {
    /// Which parameter, as an index into the definition's arena.
    pub parameter: ParamId,
    /// The time this sample is placed at — see [`Batch::time`].
    pub time: Utc,
    /// The bits as they appeared in the packet.
    pub raw: Value,
    /// The value after calibration, enumeration lookup or text decoding.
    pub eng: Value,
}

/// One decoded packet: every parameter it carried, and where it came from.
#[derive(Clone, Debug)]
pub struct Batch {
    /// The container the decoder matched.
    pub container: ContainerId,
    /// When the ground received the packet.
    pub received: Utc,
    /// When the spacecraft says the packet was made, if a time parameter was found.
    pub spacecraft: Option<Utc>,
    /// Application process identifier from the primary header.
    pub apid: u16,
    /// Sequence count from the primary header.
    pub sequence: u16,
    /// Whether the sequence count skipped one or more packets for this APID.
    pub sequence_gap: bool,
    /// The values, in the order the container lists them.
    pub samples: Vec<Sample>,
}

impl Batch {
    /// The time this batch's samples are plotted at.
    ///
    /// Spacecraft time when there is one, ground receipt otherwise. Which is the right axis
    /// is an operational question — a downlink replayed from a recorder arrives hours after
    /// it was made — and this is the answer the plots and the history both use, so that a
    /// value read in a table and a point on a plot never disagree about *when*.
    #[must_use]
    pub fn time(&self) -> Utc {
        self.spacecraft.unwrap_or(self.received)
    }
}
