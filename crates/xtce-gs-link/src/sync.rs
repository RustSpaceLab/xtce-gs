//! Finding transfer frames in a stream that has no boundaries.
//!
//! A downlink is a continuous run of bits. CCSDS 131.0-B puts a fixed 32-bit pattern — the
//! Attached Sync Marker, `1ACFFC1D` — in front of every codeword, and frame synchronisation
//! is the business of finding it, then trusting the frame length until the pattern stops
//! turning up where it should.
//!
//! Both halves of that sentence matter. A synchroniser that searches for the marker in front
//! of every frame loses a whole frame to any bit error inside the marker itself. One that
//! finds the marker once and trusts the length forever reports a clean link on a stream it
//! silently desynchronised from an hour ago. So there are two states and a flywheel between
//! them, and the transitions are counted.
//!
//! # Where the cursor points
//!
//! One invariant carries the whole state machine: while [`SyncState::Locked`], the cursor is
//! on the **marker** of the next frame, not on the frame. So the bytes a frame needs — its
//! own marker, then `frame_length` bytes — are all ahead of the cursor, and the marker that
//! decides whether lock is still good is checked before the frame it introduces is handed
//! out, not after. The alternative, a cursor on the frame with the marker check deferred to
//! the following call, cannot emit the last frame of a stream that ends cleanly: there is no
//! next marker to wait for and the frame sits in the buffer forever.
//!
//! # Bit slip
//!
//! A receiver that recovers its bit clock and its byte clock separately can hand up a stream
//! in which the marker is present but is not on a byte boundary. CCSDS 131.0-B-5 §9 does not
//! require a synchroniser to look for that, and one that does not look reports a dead link
//! rather than a slipped one. So [`Synchronizer::new`] can be told to search all eight bit
//! offsets; a frame found at a non-zero offset is shifted back into byte alignment before it
//! is handed out, because everything downstream — Reed-Solomon, the frame parser, the packet
//! assembler — reads bytes.

/// The CCSDS attached sync marker, `1ACFFC1D`.
pub const CCSDS_ASM: [u8; 4] = [0x1A, 0xCF, 0xFC, 0x1D];

/// Consecutive missing markers tolerated before lock is given up.
///
/// Four: one miss is a bit error in the marker, and a fifth in a row is a stream that has
/// moved. The number is the count *tolerated* — lock survives four consecutive misses and is
/// dropped on the fifth — so `0` means any missing marker costs lock.
pub const DEFAULT_FLYWHEEL: u32 = 4;

/// Whether the synchroniser knows where frames start.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SyncState {
    /// No frame boundary is known; every byte is a candidate marker position.
    #[default]
    Searching,
    /// A frame boundary is known and frames are being cut at it.
    Locked,
}

/// What the synchroniser did since the last drain.
///
/// Deltas, not totals. [`xtce_gs_core::LinkStats`] is the cumulative copy the interface
/// reads, and keeping a second cumulative copy here would be two numbers that can disagree
/// about the same event.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SyncCounters {
    /// Markers found where one was expected or searched for.
    pub markers_found: u64,
    /// Transitions from [`SyncState::Searching`] to [`SyncState::Locked`].
    pub locks: u64,
    /// Transitions from [`SyncState::Locked`] back to searching.
    pub losses: u64,
    /// Frames handed out by [`Synchronizer::next_frame`].
    pub frames: u64,
    /// Bytes thrown away while searching, skipped to regain lock, or refused outright by a
    /// synchroniser whose `frame_length` is zero — see [`Synchronizer::push`].
    pub bytes_discarded: u64,
    /// Times lock was acquired at a non-zero bit offset.
    pub slips: u64,
}

/// Cuts fixed-length frames out of a byte stream at the attached sync marker.
///
/// Fed with [`Synchronizer::push`] and drained with [`Synchronizer::next_frame`], so that one
/// read from a socket can produce any number of frames — including none — without the caller
/// knowing how many bytes a frame takes.
#[derive(Debug)]
pub struct Synchronizer {
    frame_length: usize,
    marker: Vec<u8>,
    bit_slip: bool,
    flywheel: u32,
    misses: u32,
    state: SyncState,
    bit_offset: u8,
    buffer: Vec<u8>,
    cursor: usize,
    counters: SyncCounters,
}

impl Synchronizer {
    /// A synchroniser for frames of `frame_length` bytes preceded by `marker`.
    ///
    /// `frame_length` is the marker-to-marker distance *excluding* the marker — the whole
    /// codeword, Reed-Solomon parity included, not the transfer frame. See
    /// [`crate::PipelineConfig::codeword_length`], which is where that arithmetic is done
    /// once so it is not done differently here.
    ///
    /// An empty `marker` means every `frame_length` bytes is a frame and the state is
    /// [`SyncState::Locked`] from the first byte. That is what a source already carrying
    /// frame boundaries needs, and it is not the same thing as being synchronised — nothing
    /// can detect a slip in that mode, which is why it is spelled by passing no marker rather
    /// than by a flag called something reassuring. That mode also never counts a lock: the
    /// pipeline folds [`SyncCounters::locks`] into `sync_found`, and a green light for a
    /// stream nothing was ever searched for is a lie on the status bar.
    ///
    /// `bit_slip` enables the search across all eight bit offsets. It costs eight times the
    /// comparisons while searching and nothing at all once locked, and it is what makes the
    /// difference on a link whose bit and byte clocks are recovered separately.
    ///
    /// A `frame_length` of zero yields no frames at all, rather than an endless run of empty
    /// ones, and [`Self::push`] discards what it is fed rather than holding a buffer nothing
    /// can ever drain. [`crate::PipelineConfig::validate`] is what refuses it where it can
    /// still be reported; this constructor returns a `Synchronizer` and has nowhere to report
    /// to.
    #[must_use]
    pub fn new(frame_length: usize, marker: Vec<u8>, bit_slip: bool) -> Self {
        let state = if marker.is_empty() {
            SyncState::Locked
        } else {
            SyncState::Searching
        };
        Self {
            frame_length,
            marker,
            bit_slip,
            flywheel: DEFAULT_FLYWHEEL,
            misses: 0,
            state,
            bit_offset: 0,
            buffer: Vec::new(),
            cursor: 0,
            counters: SyncCounters::default(),
        }
    }

    /// Bytes between one marker and the next, marker excluded.
    #[must_use]
    pub const fn frame_length(&self) -> usize {
        self.frame_length
    }

    /// The marker being looked for.
    #[must_use]
    pub fn marker(&self) -> &[u8] {
        &self.marker
    }

    /// Whether the bit-offset search is enabled.
    #[must_use]
    pub const fn searches_bit_slip(&self) -> bool {
        self.bit_slip
    }

    /// Consecutive missing markers tolerated before lock is given up.
    #[must_use]
    pub const fn flywheel(&self) -> u32 {
        self.flywheel
    }

    /// Changes the flywheel tolerance.
    ///
    /// A noisy link wants a longer one, a link that is expected to change frame length wants
    /// a shorter one; both are operational choices this crate has no basis for making.
    pub const fn set_flywheel(&mut self, misses: u32) {
        self.flywheel = misses;
    }

    /// Markers missed in a row while still locked.
    #[must_use]
    pub const fn missed_markers(&self) -> u32 {
        self.misses
    }

    /// Whether a frame boundary is known.
    #[must_use]
    pub const fn state(&self) -> SyncState {
        self.state
    }

    /// Bit offset lock was found at, `0` when the stream is byte-aligned.
    ///
    /// Worth showing the operator: a link that locks at a non-zero offset and stays there is
    /// a receiver configuration problem, not a noise problem.
    #[must_use]
    pub const fn bit_offset(&self) -> u8 {
        self.bit_offset
    }

    /// Bytes held that have not yet been cut into a frame or discarded.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len().saturating_sub(self.cursor)
    }

    /// Feeds bytes in.
    ///
    /// Appends, and drops what the cursor has already passed. No scanning happens here: a
    /// caller feeding one byte at a time would otherwise rescan the whole buffer once per
    /// byte, which is quadratic on exactly the stream — one that never syncs — where the
    /// buffer is longest.
    ///
    /// A `frame_length` of zero discards instead, counting the bytes as
    /// [`SyncCounters::bytes_discarded`]. That mode yields no frames — see [`Self::new`] —
    /// so nothing would ever move the cursor and nothing would ever drain what was kept:
    /// every byte pushed would be resident for the life of the synchroniser. Discarding here
    /// rather than letting [`Self::next_frame`]'s tail trim bound it keeps the counters
    /// honest, because the trim runs inside the marker search and a search that takes lock
    /// would report a lock and a found marker for a frame that can never be handed out.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.frame_length == 0 {
            self.counters.bytes_discarded += bytes.len() as u64;
            return;
        }
        if self.cursor > 0 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` when there are not enough bytes for one yet.
    ///
    /// The frame is the codeword, marker excluded: still randomised, still carrying
    /// Reed-Solomon parity. Undoing those is the pipeline's job, in the order the spec sets.
    /// It is byte-aligned even when it was found at a non-zero [`bit_offset`] — the slip is
    /// shifted out here, because it is the last place that knows how far the stream slipped.
    ///
    /// Borrowed from the synchroniser's own buffer rather than copied out: Reed-Solomon
    /// corrects in place and [`crate::TmFrame::parse`] borrows, so nothing downstream wants
    /// an owned copy, and a copy per frame is a copy per frame. The borrow ends before the
    /// next call, which `while let Some(frame) = sync.next_frame()` gives for free.
    ///
    /// [`bit_offset`]: Self::bit_offset
    #[must_use]
    pub fn next_frame(&mut self) -> Option<&mut [u8]> {
        let start = self.locate_frame()?;
        self.counters.frames += 1;
        let end = start + self.frame_length;
        // In bounds by construction: `locate_frame` only returns `start` after checking that
        // `start + frame_length` bytes are buffered.
        self.buffer.get_mut(start..end)
    }

    /// What happened since the last call, zeroing the deltas.
    #[must_use]
    pub fn take_counters(&mut self) -> SyncCounters {
        std::mem::take(&mut self.counters)
    }

    /// Throws away buffered bytes and goes back to searching.
    ///
    /// Called at every datagram boundary and at every new TCP peer: the bytes either side of
    /// a gap are not one frame, and carrying the leftovers across the gap invents one.
    ///
    /// Counters are not zeroed — they describe the link, and a datagram boundary is not a new
    /// link — and no loss is counted either, for the same reason: a source that flushes per
    /// datagram would otherwise report thousands of lock losses a second on a perfect link.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.misses = 0;
        self.bit_offset = 0;
        self.state = if self.marker.is_empty() {
            SyncState::Locked
        } else {
            SyncState::Searching
        };
    }

    /// Runs the state machine until a frame is ready, and returns where it starts.
    ///
    /// Split out of [`Self::next_frame`] so that all of the mutation happens before the
    /// borrow of the buffer is taken: a loop that both mutates `self` and conditionally
    /// returns a reference into it does not type-check, and returning an index does.
    fn locate_frame(&mut self) -> Option<usize> {
        if self.frame_length == 0 {
            return None;
        }
        loop {
            if self.state == SyncState::Searching && !self.acquire() {
                return None;
            }

            let marker_len = self.marker.len();
            // A frame found at a non-zero bit offset ends one byte further on than it starts,
            // because its last bits live in the top of the next byte.
            let straddle = usize::from(self.bit_offset != 0);
            let needed = marker_len + self.frame_length + straddle;
            if self.buffered() < needed {
                // The marker is deliberately *not* verified here. Verifying before the whole
                // frame has arrived would count the same marker again on the next call.
                return None;
            }

            if marker_len > 0 {
                if self.marker_at(self.cursor, self.bit_offset) {
                    self.counters.markers_found += 1;
                    self.misses = 0;
                } else {
                    self.misses += 1;
                    if self.misses > self.flywheel {
                        self.state = SyncState::Searching;
                        self.misses = 0;
                        self.bit_offset = 0;
                        self.counters.losses += 1;
                        continue;
                    }
                    // Inside the flywheel: assume the marker was corrupted rather than
                    // absent, and hand out the frame behind it. Reed-Solomon can still
                    // correct that frame, and dropping it would throw away a frame a single
                    // bit error touched only in the marker.
                }
            }

            let start = self.cursor + marker_len;
            self.align(start);
            self.cursor = start + self.frame_length;
            return Some(start);
        }
    }

    /// Scans forward for the marker. `true` when lock was taken.
    ///
    /// On failure the buffer is trimmed to the tail that could still *begin* a marker, which
    /// is what bounds the buffer on a stream that never syncs and what keeps the rescan from
    /// being quadratic: the next scan restarts near the end of the buffer, not at its head.
    ///
    /// `self.marker` is never empty here, and the guarantee is not in this function: the
    /// three places that set [`SyncState::Searching`] — `new`, `reset` and `locate_frame`'s
    /// lock-loss path — each do so only when the marker is non-empty, and `locate_frame`
    /// calls this only while searching. A fourth such site has to keep that, because an
    /// empty marker matches at the cursor immediately and would count a lock the empty-marker
    /// mode promises never to count.
    fn acquire(&mut self) -> bool {
        let marker_len = self.marker.len();
        let offsets: u8 = if self.bit_slip { 8 } else { 1 };
        let mut index = self.cursor;
        while index + marker_len <= self.buffer.len() {
            for bit in 0..offsets {
                if self.marker_at(index, bit) {
                    self.counters.bytes_discarded += (index - self.cursor) as u64;
                    self.cursor = index;
                    self.bit_offset = bit;
                    self.state = SyncState::Locked;
                    self.misses = 0;
                    self.counters.locks += 1;
                    if bit != 0 {
                        self.counters.slips += 1;
                    }
                    return true;
                }
            }
            index += 1;
        }

        // Every position that could be checked in full has been. A marker starting in the
        // last `marker_len - 1` bytes — one more when the bit-offset search needs a byte of
        // straddle — is still possible, so those bytes stay; everything before them is gone.
        // `marker_len` is at least 1 here, by the invariant in this function's doc.
        let keep = (marker_len + usize::from(self.bit_slip)).saturating_sub(1);
        let kept_from = self.buffer.len().saturating_sub(keep);
        if kept_from > self.cursor {
            self.counters.bytes_discarded += (kept_from - self.cursor) as u64;
            self.cursor = kept_from;
        }
        false
    }

    /// Shifts `frame_length` bytes at `start` left by the locked bit offset, in place.
    ///
    /// Forward order is load-bearing: step `i` reads `start + i` and `start + i + 1` and
    /// writes `start + i`, so the byte a later step still needs has not been written yet.
    /// The byte at `start + frame_length` is read and never written, which is what leaves the
    /// next marker intact for the next call to check at the same offset.
    fn align(&mut self, start: usize) {
        let shift = self.bit_offset;
        if shift == 0 {
            return;
        }
        let mut index = start;
        let end = start + self.frame_length;
        while index < end {
            let aligned = Self::byte_at(&self.buffer, index, shift);
            if let Some(slot) = self.buffer.get_mut(index) {
                *slot = aligned;
            }
            index += 1;
        }
    }

    /// Whether the marker sits at `index`, `bit` bits in.
    ///
    /// `false` when the bytes it would need are not all buffered, so a caller never has to
    /// separate "no" from "not yet" — the next call, with more bytes, answers again.
    fn marker_at(&self, index: usize, bit: u8) -> bool {
        let needed = self.marker.len() + usize::from(bit != 0);
        if index + needed > self.buffer.len() {
            return false;
        }
        self.marker
            .iter()
            .enumerate()
            .all(|(offset, byte)| Self::byte_at(&self.buffer, index + offset, bit) == *byte)
    }

    /// The byte that begins `bit` bits into `buf[index]`, read across the byte boundary.
    ///
    /// Bits are numbered most significant first, the order CCSDS 131.0-B puts them on the
    /// wire in, so offset 1 means "drop the top bit of this byte and take the top bit of the
    /// next". Reads past the end are zero; every caller checks the length first.
    fn byte_at(buf: &[u8], index: usize, bit: u8) -> u8 {
        let high = buf.get(index).copied().unwrap_or(0);
        if bit == 0 {
            return high;
        }
        let low = buf.get(index + 1).copied().unwrap_or(0);
        (high << bit) | (low >> (8 - bit))
    }
}

#[cfg(test)]
mod tests {
    use super::{CCSDS_ASM, SyncState, Synchronizer};

    /// A frame whose bytes are a counter, so a shifted or mis-cut frame is obvious and so
    /// the marker never occurs inside a frame by accident.
    fn frame(tag: u8, length: usize) -> Vec<u8> {
        (0..length).map(|i| tag.wrapping_add(i as u8)).collect()
    }

    fn stream(tags: &[u8], length: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for tag in tags {
            out.extend_from_slice(&CCSDS_ASM);
            out.extend_from_slice(&frame(*tag, length));
        }
        out
    }

    /// The stream as a receiver that has slipped `bits` bits would deliver it: `bits` bits of
    /// padding first, then the real stream, still most significant bit first.
    fn slip(bytes: &[u8], bits: u8) -> Vec<u8> {
        let mut all = vec![false; bits as usize];
        for byte in bytes {
            for index in (0..8).rev() {
                all.push((byte >> index) & 1 == 1);
            }
        }
        while !all.len().is_multiple_of(8) {
            all.push(false);
        }
        all.chunks(8)
            .map(|chunk| {
                chunk
                    .iter()
                    .fold(0u8, |acc, bit| (acc << 1) | u8::from(*bit))
            })
            .collect()
    }

    fn drain(sync: &mut Synchronizer) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(frame) = sync.next_frame() {
            frames.push(frame.to_vec());
        }
        frames
    }

    #[test]
    fn a_stream_of_frames_is_cut_at_every_marker() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        sync.push(&stream(&[0x10, 0x20, 0x30], 8));
        let frames = drain(&mut sync);
        assert_eq!(frames, vec![frame(0x10, 8), frame(0x20, 8), frame(0x30, 8)]);
        assert_eq!(sync.state(), SyncState::Locked);
        let counters = sync.take_counters();
        assert_eq!(counters.frames, 3);
        assert_eq!(counters.markers_found, 3);
        assert_eq!(counters.locks, 1);
        assert_eq!(counters.losses, 0);
        assert_eq!(counters.bytes_discarded, 0);
    }

    #[test]
    fn bytes_before_the_first_marker_are_discarded_and_counted() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        let mut bytes = vec![0xAA; 37];
        bytes.extend_from_slice(&stream(&[0x40], 8));
        sync.push(&bytes);
        assert_eq!(drain(&mut sync), vec![frame(0x40, 8)]);
        assert_eq!(sync.take_counters().bytes_discarded, 37);
    }

    /// The last frame of a stream that ends cleanly has no marker after it. A synchroniser
    /// that waits for one loses it.
    #[test]
    fn the_last_frame_is_emitted_although_no_marker_follows_it() {
        let mut sync = Synchronizer::new(16, CCSDS_ASM.to_vec(), false);
        sync.push(&stream(&[0x01, 0x02], 16));
        assert_eq!(drain(&mut sync).len(), 2);
    }

    #[test]
    fn a_truncated_trailing_frame_yields_nothing_and_stays_buffered() {
        let mut sync = Synchronizer::new(16, CCSDS_ASM.to_vec(), false);
        let full = stream(&[0x01, 0x02], 16);
        sync.push(&full[..full.len() - 1]);
        assert_eq!(drain(&mut sync).len(), 1);
        assert_eq!(sync.buffered(), 4 + 15, "the marker and the partial frame");
        sync.push(&full[full.len() - 1..]);
        assert_eq!(drain(&mut sync), vec![frame(0x02, 16)]);
    }

    /// The whole point of the flywheel: a bit error inside the marker must not cost the frame
    /// behind it, which Reed-Solomon can still correct.
    #[test]
    fn a_single_corrupted_marker_costs_neither_the_lock_nor_the_frame() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        let mut bytes = stream(&[0x10, 0x20, 0x30], 8);
        bytes[12] ^= 0x01; // the first byte of the second frame's marker
        sync.push(&bytes);
        let frames = drain(&mut sync);
        assert_eq!(frames, vec![frame(0x10, 8), frame(0x20, 8), frame(0x30, 8)]);
        assert_eq!(sync.state(), SyncState::Locked);
        let counters = sync.take_counters();
        assert_eq!(counters.markers_found, 2, "the corrupted one is not found");
        assert_eq!(counters.frames, 3, "but its frame is still handed out");
        assert_eq!(counters.losses, 0);
    }

    /// With a flywheel of four, four misses in a row are tolerated and the fifth loses lock.
    /// The skeleton's rule is `misses > flywheel`, and this test is what pins it.
    #[test]
    fn loses_lock_on_the_fifth_consecutive_missing_marker_with_a_flywheel_of_four() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        assert_eq!(sync.flywheel(), 4);
        let mut bytes = stream(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80], 8);
        // Every marker after the first one is destroyed outright.
        for index in 1..8 {
            let at = index * 12;
            bytes[at] = 0x00;
            bytes[at + 1] = 0x00;
        }
        sync.push(&bytes);
        let frames = drain(&mut sync);
        assert_eq!(
            frames.len(),
            5,
            "the good marker and four inside the flywheel"
        );
        assert_eq!(sync.state(), SyncState::Searching);
        let counters = sync.take_counters();
        assert_eq!(counters.losses, 1);
        assert_eq!(counters.markers_found, 1);
    }

    /// Every one of the seven offsets, not one of them: an off-by-one in the shift helper
    /// passes at four bits and fails at one.
    #[test]
    fn a_stream_slipped_by_each_of_the_seven_bit_offsets_still_yields_the_frames() {
        for bits in 1..8u8 {
            let mut sync = Synchronizer::new(12, CCSDS_ASM.to_vec(), true);
            sync.push(&slip(&stream(&[0x55, 0x66], 12), bits));
            let frames = drain(&mut sync);
            assert_eq!(
                frames,
                vec![frame(0x55, 12), frame(0x66, 12)],
                "slipped by {bits} bits"
            );
            assert_eq!(sync.bit_offset(), bits);
            let counters = sync.take_counters();
            assert_eq!(counters.slips, 1, "slipped by {bits} bits");
            assert_eq!(counters.markers_found, 2, "slipped by {bits} bits");
        }
    }

    #[test]
    fn a_slipped_stream_does_not_lock_when_the_bit_offset_search_is_off() {
        let mut sync = Synchronizer::new(12, CCSDS_ASM.to_vec(), false);
        sync.push(&slip(&stream(&[0x55, 0x66], 12), 3));
        assert!(drain(&mut sync).is_empty());
        assert_eq!(sync.state(), SyncState::Searching);
    }

    /// A stream that never syncs must not be a memory leak with a counter on it.
    ///
    /// Two things this has to get right to see anything. The fixture must contain the bytes
    /// the scan is looking for — a stream with no `0x1A` in it fails on the first comparison
    /// at every index and every bit offset, so it never reaches the trim, a partial match or
    /// a near-lock. And the assertion must be on the buffer itself: `buffered()` is
    /// `len - cursor`, which is small the moment the cursor moves whether or not `push` ever
    /// released the prefix behind it, so it cannot observe the leak it is meant to catch.
    #[test]
    fn the_buffer_does_not_grow_on_a_stream_that_never_syncs() {
        // `1A CF FC` is the first three octets of the marker and `1A` alone is its first, so
        // every round matches part of the marker at several positions — including one that
        // runs to the very end of the chunk — and none of it in full at any of the eight bit
        // offsets. Rotating by the round moves the phase, so the chunk boundary lands in a
        // different place each time.
        const NOISE: [u8; 16] = [
            0x1A, 0xCF, 0xFC, 0x1C, 0x1A, 0xCF, 0x00, 0x1D, 0x1A, 0x1A, 0x1A, 0xCF, 0x55, 0x1A,
            0xCF, 0xFC,
        ];
        const CHUNK: usize = 512;
        let mut sync = Synchronizer::new(1024, CCSDS_ASM.to_vec(), true);
        for round in 0..200usize {
            let chunk: Vec<u8> = (0..CHUNK)
                .map(|i| NOISE[(i + round) % NOISE.len()])
                .collect();
            assert!(
                chunk.contains(&CCSDS_ASM[0]),
                "the fixture must be scannable"
            );
            sync.push(&chunk);
            assert!(drain(&mut sync).is_empty(), "round {round}");
            assert_eq!(sync.state(), SyncState::Searching, "round {round}");
            assert!(
                sync.buffer.capacity() <= 4 * CHUNK,
                "capacity {} after round {round}",
                sync.buffer.capacity()
            );
        }
        let counters = sync.take_counters();
        assert_eq!(
            counters.locks, 0,
            "no full marker is present at any bit offset"
        );
        assert!(
            counters.bytes_discarded >= 100_000,
            "the trim ran every round"
        );
    }

    /// The flywheel tolerates *consecutive* misses — `DEFAULT_FLYWHEEL`'s doc says so — and
    /// `self.misses = 0` on a matched marker is the whole of what makes that true. Damage
    /// every other marker: no two in a row, so a counter that resets never passes one, and a
    /// counter that only ever climbs passes four and drops lock partway through the stream.
    /// Every existing flywheel test damages an unbroken run, where the two behave alike.
    #[test]
    fn marker_damage_that_is_never_consecutive_costs_no_lock_however_much_of_it_arrives() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        assert_eq!(sync.flywheel(), 4);
        let tags: Vec<u8> = (0..12u8).map(|i| 0x10 + i * 0x10).collect();
        let mut bytes = stream(&tags, 8);
        // Frames 1, 3, 5, 7, 9 and 11: six misses, more than the flywheel tolerates in a row
        // and never two in a row.
        for index in (1..12).step_by(2) {
            let at = index * 12;
            bytes[at] = 0x00;
            bytes[at + 1] = 0x00;
        }
        sync.push(&bytes);
        let frames = drain(&mut sync);
        let expected: Vec<Vec<u8>> = tags.iter().map(|tag| frame(*tag, 8)).collect();
        assert_eq!(frames, expected, "every frame, damaged marker or not");
        assert_eq!(sync.state(), SyncState::Locked);
        assert_eq!(
            sync.missed_markers(),
            1,
            "the last marker was damaged and the one before it was not"
        );
        let counters = sync.take_counters();
        assert_eq!(counters.losses, 0, "no two misses ever came in a row");
        assert_eq!(counters.locks, 1);
        assert_eq!(counters.markers_found, 6);
        assert_eq!(counters.frames, 12);
    }

    /// A marker at a non-zero bit offset spans one byte more than its length, so the last
    /// position the scan reaches — `buffer.len() - marker_len` — is one it cannot decide:
    /// `marker_at` refuses it for want of the straddle byte. The extra byte in the trim's
    /// `keep` is what holds that position until the next push brings the rest of it.
    #[test]
    fn a_slipped_marker_at_the_trim_boundary_survives_the_trim() {
        for bits in 1..8u8 {
            let mut sync = Synchronizer::new(12, CCSDS_ASM.to_vec(), true);
            let slipped = slip(&stream(&[0x55], 12), bits);
            // Twenty bytes of nothing, then exactly the first four bytes of the five the
            // marker occupies at this offset: it begins at the last index the scan can reach
            // and cannot be confirmed there.
            let mut first = vec![0x00; 20];
            first.extend_from_slice(&slipped[..4]);
            sync.push(&first);
            assert!(drain(&mut sync).is_empty(), "slipped by {bits} bits");
            assert_eq!(sync.state(), SyncState::Searching, "slipped by {bits} bits");
            assert_eq!(
                sync.buffered(),
                4,
                "the marker's four buffered bytes are kept"
            );

            sync.push(&slipped[4..]);
            assert_eq!(
                drain(&mut sync),
                vec![frame(0x55, 12)],
                "slipped by {bits} bits"
            );
            assert_eq!(sync.bit_offset(), bits);
            let counters = sync.take_counters();
            assert_eq!(counters.slips, 1, "slipped by {bits} bits");
            assert_eq!(counters.bytes_discarded, 20, "the zeros, and nothing else");
        }
    }

    #[test]
    fn a_frame_length_of_zero_yields_no_frames_rather_than_an_endless_run_of_them() {
        let mut sync = Synchronizer::new(0, CCSDS_ASM.to_vec(), false);
        sync.push(&stream(&[0x10, 0x20], 0));
        assert!(sync.next_frame().is_none());
        assert_eq!(sync.take_counters().frames, 0);

        // With no marker either, the state is `Locked` from the first byte and nothing but
        // the length guard stands between `next_frame` and an endless run of empty frames.
        let mut bare = Synchronizer::new(0, Vec::new(), false);
        bare.push(&[1, 2, 3, 4]);
        assert!(bare.next_frame().is_none());
        assert_eq!(bare.take_counters().frames, 0);
    }

    /// A synchroniser that can never cut a frame must not hold the stream it will never cut.
    /// `locate_frame` returns before the tail trim in this mode, so the trim is not what
    /// bounds it; `push` is.
    #[test]
    fn a_frame_length_of_zero_discards_what_it_can_never_cut() {
        let mut sync = Synchronizer::new(0, CCSDS_ASM.to_vec(), true);
        for _ in 0..32 {
            sync.push(&[0xA5; 512]);
            assert!(sync.next_frame().is_none());
        }
        assert_eq!(sync.buffered(), 0);
        assert!(
            sync.buffer.capacity() <= 512,
            "capacity {}",
            sync.buffer.capacity()
        );
        let counters = sync.take_counters();
        assert_eq!(counters.frames, 0);
        assert_eq!(counters.bytes_discarded, 32 * 512);
    }

    #[test]
    fn an_empty_marker_cuts_from_the_first_byte_and_counts_no_lock() {
        let mut sync = Synchronizer::new(4, Vec::new(), false);
        assert_eq!(sync.state(), SyncState::Locked);
        sync.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(drain(&mut sync), vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        let counters = sync.take_counters();
        assert_eq!(counters.locks, 0, "nothing was searched for");
        assert_eq!(counters.markers_found, 0);
        assert_eq!(counters.frames, 2);
    }

    #[test]
    fn a_reset_drops_the_buffered_bytes_and_keeps_the_counters() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        sync.push(&stream(&[0x10], 8));
        assert_eq!(drain(&mut sync).len(), 1);
        sync.push(&CCSDS_ASM);
        sync.reset();
        assert_eq!(sync.buffered(), 0);
        assert_eq!(sync.state(), SyncState::Searching);
        assert_eq!(sync.bit_offset(), 0);
        assert_eq!(sync.missed_markers(), 0);
        let counters = sync.take_counters();
        assert_eq!(counters.frames, 1, "the reset did not zero the deltas");
        assert_eq!(counters.losses, 0, "a boundary is not a lock loss");
    }

    /// A marker split across two reads is the common case on TCP, not an edge case.
    #[test]
    fn a_marker_that_straddles_two_pushes_is_still_found() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        let bytes = stream(&[0x77], 8);
        sync.push(&bytes[..2]);
        assert!(drain(&mut sync).is_empty());
        sync.push(&bytes[2..]);
        assert_eq!(drain(&mut sync), vec![frame(0x77, 8)]);
    }

    #[test]
    fn one_byte_at_a_time_gives_the_same_frames_as_one_chunk() {
        let bytes = stream(&[0x10, 0x20, 0x30], 11);
        let mut whole = Synchronizer::new(11, CCSDS_ASM.to_vec(), true);
        whole.push(&bytes);
        let from_one_chunk = drain(&mut whole);

        let mut dribbled = Synchronizer::new(11, CCSDS_ASM.to_vec(), true);
        let mut from_many = Vec::new();
        for byte in &bytes {
            dribbled.push(std::slice::from_ref(byte));
            from_many.extend(drain(&mut dribbled));
        }
        assert_eq!(from_one_chunk, from_many);
        assert_eq!(from_one_chunk.len(), 3);
    }

    /// Once lock is lost the search resumes ahead of the cursor and finds the next real
    /// marker, rather than sitting in `Searching` on a stream that is perfectly fine again.
    #[test]
    fn lock_is_regained_at_the_next_good_marker_after_a_loss() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        sync.set_flywheel(0);
        let mut bytes = stream(&[0x10, 0x20], 8);
        bytes[12] = 0x00;
        bytes[13] = 0x00;
        bytes.extend_from_slice(&stream(&[0x30], 8));
        sync.push(&bytes);
        let frames = drain(&mut sync);
        assert_eq!(frames, vec![frame(0x10, 8), frame(0x30, 8)]);
        let counters = sync.take_counters();
        assert_eq!(counters.losses, 1);
        assert_eq!(counters.locks, 2);
    }

    /// The frame is handed out as a borrow of the buffer, and correcting it in place is the
    /// reason it is: what the caller writes is what the synchroniser holds.
    #[test]
    fn the_frame_is_a_mutable_borrow_of_the_synchronisers_own_buffer() {
        let mut sync = Synchronizer::new(8, CCSDS_ASM.to_vec(), false);
        sync.push(&stream(&[0x10, 0x20], 8));
        {
            let first = sync.next_frame().unwrap();
            first[0] = 0xFF;
            assert_eq!(first[0], 0xFF);
        }
        assert_eq!(drain(&mut sync), vec![frame(0x20, 8)]);
    }
}
