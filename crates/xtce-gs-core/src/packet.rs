//! A CCSDS space packet, owned, with the time it arrived.
//!
//! The acquisition side produces these and the decode side consumes them. They own their
//! bytes because the datagram buffer they came out of is reused for the next read, and
//! because the two sides are different threads.

use xtce_decode::SpacePacketBytes;

use crate::time::Utc;

/// One space packet as it came off the link.
#[derive(Clone, Debug)]
pub struct RawPacket {
    /// When the ground received it. For a file replay, when the replay produced it.
    pub received: Utc,
    /// The packet, primary header first. Exactly one packet, never a fragment.
    pub bytes: Vec<u8>,
    /// The virtual channel it was assembled from, when the link supplied transfer frames.
    pub vcid: Option<u8>,
}

impl RawPacket {
    /// Wraps bytes received now.
    #[must_use]
    pub fn now(bytes: Vec<u8>) -> Self {
        Self {
            received: Utc::now(),
            bytes,
            vcid: None,
        }
    }

    /// A view of the primary header, when there are enough bytes for one.
    ///
    /// `None` means fewer than six bytes, which the acquisition side should already have
    /// refused — but a length check that only exists upstream is a length check that moves
    /// away from the code that depends on it.
    #[must_use]
    pub fn header(&self) -> Option<SpacePacketBytes<'_>> {
        if self.bytes.len() < 6 {
            return None;
        }
        Some(SpacePacketBytes::new(&self.bytes))
    }

    /// Application process identifier, or `None` if the packet is too short to have one.
    #[must_use]
    pub fn apid(&self) -> Option<u16> {
        self.header().map(SpacePacketBytes::apid)
    }

    /// Sequence count, or `None` if the packet is too short to have one.
    #[must_use]
    pub fn sequence_count(&self) -> Option<u16> {
        self.header().map(SpacePacketBytes::sequence_count)
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the packet carries no bytes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}
