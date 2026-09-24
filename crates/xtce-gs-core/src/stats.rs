//! Link counters, written by the acquisition thread and read by the interface.
//!
//! These are atomics rather than a locked struct because the interface reads all of them
//! sixty times a second and the link writes several of them per frame. `Relaxed` ordering is
//! correct here and not a shortcut: each counter is independent, nothing is published through
//! them, and an interface that renders `frames_ok` from one instant and `crc_failures` from
//! the next is showing a number that was true a microsecond ago — which is what every counter
//! on every ground station display is.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Counters describing what the link has done since the session started.
#[derive(Debug, Default)]
pub struct LinkStats {
    /// Bytes read from the source.
    pub bytes_in: AtomicU64,
    /// Attached sync markers found.
    pub sync_found: AtomicU64,
    /// Times the synchroniser lost lock and went back to searching.
    pub sync_lost: AtomicU64,
    /// Candidate frames handed to the frame decoder.
    pub frames_seen: AtomicU64,
    /// Frames that passed every check they were configured for.
    pub frames_ok: AtomicU64,
    /// Frames dropped: uncorrectable, bad CRC, or a header that made no sense.
    pub frames_dropped: AtomicU64,
    /// Frames the Reed-Solomon decoder repaired.
    pub rs_corrected: AtomicU64,
    /// Symbols the Reed-Solomon decoder repaired, summed over frames.
    pub rs_symbols: AtomicU64,
    /// Frames Reed-Solomon could not repair.
    pub rs_uncorrectable: AtomicU64,
    /// Frames whose trailing checksum did not match.
    pub crc_failures: AtomicU64,
    /// Idle frames — first header pointer 0x7FE — seen and discarded.
    pub idle_frames: AtomicU64,
    /// Idle *packets* — an idle APID inside a real frame — seen and discarded.
    ///
    /// Separate from `idle_frames` because they answer different questions: idle frames say
    /// the spacecraft had nothing to send, idle packets say a virtual channel was padded.
    pub idle_packets: AtomicU64,
    /// Packets pulled out of the frames, or read directly from the source.
    pub packets_in: AtomicU64,
    /// Packets successfully decoded against the definition.
    pub packets_decoded: AtomicU64,
    /// Packets the definition refused or did not describe.
    pub packets_rejected: AtomicU64,
    /// Packets thrown away because a partial packet could not be completed.
    pub packets_lost: AtomicU64,
    /// Gaps in a per-APID sequence count: how many times continuity broke.
    pub sequence_gaps: AtomicU64,
    /// Packets the sequence counts say never arrived, summed over the gaps.
    ///
    /// One gap of four hundred and four hundred gaps of one are not the same link, and a
    /// single counter cannot tell them apart — so there are two.
    pub sequence_missing: AtomicU64,
    /// Nanoseconds since the Unix epoch at the last packet, or `i64::MIN` for never.
    pub last_packet: AtomicI64,
}

impl LinkStats {
    /// A fresh set of counters, all zero.
    #[must_use]
    pub fn new() -> Self {
        let stats = Self::default();
        stats.last_packet.store(i64::MIN, Ordering::Relaxed);
        stats
    }

    /// Adds to a counter.
    ///
    /// A free function over a field reference, so callers write
    /// `stats.add(&stats.frames_ok, 1)` and no counter needs its own method.
    pub fn add(&self, counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, Ordering::Relaxed);
    }

    /// Records that a packet arrived at `nanos`.
    pub fn touch(&self, nanos: i64) {
        self.last_packet.store(nanos, Ordering::Relaxed);
    }

    /// A consistent-enough copy for one frame of the interface.
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        StatsSnapshot {
            bytes_in: get(&self.bytes_in),
            sync_found: get(&self.sync_found),
            sync_lost: get(&self.sync_lost),
            frames_seen: get(&self.frames_seen),
            frames_ok: get(&self.frames_ok),
            frames_dropped: get(&self.frames_dropped),
            rs_corrected: get(&self.rs_corrected),
            rs_symbols: get(&self.rs_symbols),
            rs_uncorrectable: get(&self.rs_uncorrectable),
            crc_failures: get(&self.crc_failures),
            idle_frames: get(&self.idle_frames),
            idle_packets: get(&self.idle_packets),
            packets_in: get(&self.packets_in),
            packets_decoded: get(&self.packets_decoded),
            packets_rejected: get(&self.packets_rejected),
            packets_lost: get(&self.packets_lost),
            sequence_gaps: get(&self.sequence_gaps),
            sequence_missing: get(&self.sequence_missing),
            last_packet: match self.last_packet.load(Ordering::Relaxed) {
                i64::MIN => None,
                nanos => Some(crate::time::Utc::from_unix_nanos(nanos)),
            },
        }
    }
}

/// A plain copy of [`LinkStats`], safe to hold across a draw.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatsSnapshot {
    /// Bytes read from the source.
    pub bytes_in: u64,
    /// Attached sync markers found.
    pub sync_found: u64,
    /// Times the synchroniser lost lock.
    pub sync_lost: u64,
    /// Candidate frames handed to the frame decoder.
    pub frames_seen: u64,
    /// Frames that passed every check.
    pub frames_ok: u64,
    /// Frames dropped.
    pub frames_dropped: u64,
    /// Frames Reed-Solomon repaired.
    pub rs_corrected: u64,
    /// Symbols Reed-Solomon repaired.
    pub rs_symbols: u64,
    /// Frames Reed-Solomon could not repair.
    pub rs_uncorrectable: u64,
    /// Frames whose checksum did not match.
    pub crc_failures: u64,
    /// Idle frames discarded.
    pub idle_frames: u64,
    /// Idle packets discarded.
    pub idle_packets: u64,
    /// Packets that reached the decoder.
    pub packets_in: u64,
    /// Packets decoded.
    pub packets_decoded: u64,
    /// Packets refused.
    pub packets_rejected: u64,
    /// Packets lost to incomplete reassembly.
    pub packets_lost: u64,
    /// Sequence-count gaps: how many times continuity broke.
    pub sequence_gaps: u64,
    /// Packets the gaps imply were never received.
    pub sequence_missing: u64,
    /// When the last packet arrived.
    pub last_packet: Option<crate::time::Utc>,
}

impl StatsSnapshot {
    /// Frames that failed some check, as a fraction of frames seen.
    ///
    /// Zero when nothing has been seen — a rate over no samples is not 100 %, and a status
    /// bar that opens at "100 % frame loss" teaches the operator to ignore it.
    #[must_use]
    pub fn frame_loss(&self) -> f64 {
        if self.frames_seen == 0 {
            return 0.0;
        }
        self.frames_dropped as f64 / self.frames_seen as f64
    }

    /// Whether anything has arrived in the last `seconds`.
    #[must_use]
    pub fn is_live(&self, now: crate::time::Utc, seconds: f64) -> bool {
        self.last_packet
            .is_some_and(|last| now.secs_since(last) <= seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_link_has_never_seen_a_packet() {
        let stats = LinkStats::new();
        let snap = stats.snapshot();
        assert!(snap.last_packet.is_none());
        assert!(!snap.is_live(crate::time::Utc::now(), 5.0));
    }

    #[test]
    fn loss_over_nothing_is_zero_not_one() {
        assert_eq!(StatsSnapshot::default().frame_loss(), 0.0);
    }

    #[test]
    fn counters_count() {
        let stats = LinkStats::new();
        stats.add(&stats.frames_ok, 3);
        stats.add(&stats.frames_seen, 4);
        stats.add(&stats.frames_dropped, 1);
        let snap = stats.snapshot();
        assert_eq!(snap.frames_ok, 3);
        assert_eq!(snap.frame_loss(), 0.25);
    }
}
