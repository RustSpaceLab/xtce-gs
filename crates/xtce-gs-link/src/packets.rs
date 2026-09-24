//! Getting space packets back out.
//!
//! Two ways in, one way out. [`PacketAssembler`] takes transfer frames and honours the
//! first-header pointer; [`PacketStream`] takes a byte stream that is already packets back to
//! back. Both produce [`RawPacket`], and both are loud about what they lose.
//!
//! Loud is the point. A packet longer than one frame's data field arrives in pieces, and a
//! station that throws away the leading bytes of every frame — the ones before the pointer,
//! which belong to the *previous* packet — loses one packet in every few long ones and reports
//! a clean link while doing it. The counters here exist so that failure has a number.

use xtce_gs_core::{Event, RawPacket, Severity, Utc};

use crate::frame::{NO_PACKET_START, ONLY_IDLE_DATA, TmFrame};

/// Size of the CCSDS space packet primary header.
pub const SPACE_PACKET_HEADER_BYTES: usize = 6;

/// The APID reserved for idle packets.
///
/// CCSDS 133.0-B keeps all ones for fill. These are dropped here rather than handed to the
/// decoder, which has no definition for them and would reject thousands a minute.
pub const IDLE_APID: u16 = 0x7FF;

/// Total length of the packet whose primary header starts at `header`.
///
/// CCSDS 133.0-B-2 section 4.1.3.5.3: the packet data length field is the last two octets of
/// the primary header and holds `C = (octets in the packet data field) - 1`, so the whole
/// packet is `6 + C + 1` bytes. There is no length of zero and no packet shorter than seven
/// bytes, which is why the smallest value this can return is 7 and not 6.
///
/// `None` when fewer than [`SPACE_PACKET_HEADER_BYTES`] bytes are available — the caller has
/// a header split across two frames and must wait for the rest before it can know how much to
/// wait for.
#[must_use]
pub fn declared_length(header: &[u8]) -> Option<usize> {
    let hi = *header.get(4)?;
    let lo = *header.get(5)?;
    Some(SPACE_PACKET_HEADER_BYTES + usize::from(u16::from_be_bytes([hi, lo])) + 1)
}

/// What packet recovery did since the last drain.
///
/// Deltas, like [`crate::SyncCounters`]: the cumulative numbers live on
/// [`xtce_gs_core::LinkStats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PacketCounters {
    /// Complete packets handed out.
    pub packets: u64,
    /// Partial packets thrown away because they could not be completed.
    pub lost: u64,
    /// Idle packets recognised and dropped.
    ///
    /// Folded into [`xtce_gs_core::LinkStats::idle_packets`], which is its own field rather
    /// than a share of `idle_frames`: frames say the spacecraft had nothing to send, packets
    /// say a virtual channel was padded, and overloading one field with both is how a status
    /// bar starts disagreeing with itself.
    pub idle: u64,
    /// Bytes discarded as unusable: a desynchronised data field, a leading fragment with no
    /// packet in front of it.
    pub discarded_bytes: u64,
}

impl PacketCounters {
    /// Adds another set of deltas into this one.
    pub const fn merge(&mut self, other: Self) {
        self.packets = self.packets.saturating_add(other.packets);
        self.lost = self.lost.saturating_add(other.lost);
        self.idle = self.idle.saturating_add(other.idle);
        self.discarded_bytes = self.discarded_bytes.saturating_add(other.discarded_bytes);
    }
}

/// Per-virtual-channel assembly state.
///
/// One of these per virtual channel that has been seen. A packet spans frames *within* a
/// virtual channel and never across them, so the partial packet and the frame count that
/// validates it belong together and separately from every other channel's.
#[derive(Clone, Debug)]
pub struct VirtualChannel {
    vcid: u8,
    last_frame_count: Option<u8>,
    partial: Vec<u8>,
}

impl VirtualChannel {
    /// Which channel this is.
    #[must_use]
    pub const fn vcid(&self) -> u8 {
        self.vcid
    }

    /// The virtual channel frame count of the last frame seen here.
    ///
    /// `None` until the first frame. The next frame's count must be this plus one, modulo
    /// 256, or frames were lost and any partial packet is unfinishable.
    #[must_use]
    pub const fn last_frame_count(&self) -> Option<u8> {
        self.last_frame_count
    }

    /// Bytes of a packet held, waiting for the rest.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.partial.len()
    }

    /// Whether a packet is half-assembled on this channel.
    #[must_use]
    pub fn is_assembling(&self) -> bool {
        !self.partial.is_empty()
    }

    /// A channel that has seen nothing yet.
    fn new(vcid: u8) -> Self {
        Self {
            vcid,
            last_frame_count: None,
            partial: Vec::new(),
        }
    }
}

/// What feeding bytes to a half-assembled packet did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fed {
    /// The packet is still short of its declared length.
    Growing,
    /// It reached its declared length and was handed out; this many bytes were fed past it.
    Completed(usize),
    /// It declared more than `max_packet_length` and was thrown away.
    Dropped,
}

/// One channel's state plus the assembler's shared counters, borrowed together.
///
/// The methods below all need the channel *and* the counters *and* the event queue, and they
/// are three disjoint fields of [`PacketAssembler`]; taking them once here is what lets them
/// be methods at all rather than six-argument free functions.
struct Assembly<'a> {
    channel: &'a mut VirtualChannel,
    counters: &'a mut PacketCounters,
    events: &'a mut Vec<Event>,
    max_packet_length: usize,
    received: Utc,
}

impl Assembly<'_> {
    fn warn(&mut self, message: String) {
        self.events
            .push(Event::at(self.received, Severity::Warning, "link", message));
    }

    /// Checks the virtual channel frame count, returning whether this frame continues the
    /// last one seen on the channel.
    ///
    /// `false` on the first frame after a lock or a reset, and on a gap — in both cases the
    /// bytes in front of the first header pointer belong to a packet this station cannot
    /// finish, and discarding them is not news.
    fn check_continuity(&mut self, frame_count: u8) -> bool {
        let Some(last) = self.channel.last_frame_count else {
            self.channel.last_frame_count = Some(frame_count);
            return false;
        };
        self.channel.last_frame_count = Some(frame_count);
        let expected = last.wrapping_add(1);
        if frame_count == expected {
            return true;
        }
        // A repeated count reads as 255 missing, which is the only honest answer: the frame
        // count wraps and nothing else in the frame says how far.
        let missing = frame_count.wrapping_sub(expected);
        let vcid = self.channel.vcid;
        let pending = self.channel.partial.len();
        if pending == 0 {
            self.warn(format!(
                "virtual channel {vcid}: frame count jumped from {last} to {frame_count}, \
                 {missing} frame(s) lost"
            ));
        } else {
            self.channel.partial.clear();
            self.counters.lost = self.counters.lost.saturating_add(1);
            self.counters.discarded_bytes =
                self.counters.discarded_bytes.saturating_add(pending as u64);
            self.warn(format!(
                "virtual channel {vcid}: frame count jumped from {last} to {frame_count}, \
                 {missing} frame(s) lost, {pending}-byte partial packet abandoned"
            ));
        }
        false
    }

    /// Hands out one finished packet, stamped with the channel it came off.
    fn deliver(&mut self, bytes: Vec<u8>, out: &mut Vec<RawPacket>) {
        let packet = RawPacket {
            received: self.received,
            bytes,
            vcid: Some(self.channel.vcid),
        };
        deliver(packet, self.counters, out);
    }

    /// Throws the partial packet away, counting it lost.
    fn abandon(&mut self, why: &str) {
        let pending = self.channel.partial.len();
        if pending == 0 {
            return;
        }
        let vcid = self.channel.vcid;
        self.channel.partial.clear();
        self.counters.lost = self.counters.lost.saturating_add(1);
        self.counters.discarded_bytes =
            self.counters.discarded_bytes.saturating_add(pending as u64);
        self.warn(format!(
            "virtual channel {vcid}: {pending}-byte partial packet abandoned, {why}"
        ));
    }

    /// Appends `bytes` to the packet in progress, handing it out once it is whole.
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<RawPacket>) -> Fed {
        self.channel.partial.extend_from_slice(bytes);
        let Some(declared) = declared_length(&self.channel.partial) else {
            return Fed::Growing;
        };
        if declared > self.max_packet_length {
            let vcid = self.channel.vcid;
            let max = self.max_packet_length;
            let held = self.channel.partial.len();
            self.channel.partial.clear();
            self.counters.lost = self.counters.lost.saturating_add(1);
            self.counters.discarded_bytes =
                self.counters.discarded_bytes.saturating_add(held as u64);
            self.warn(format!(
                "virtual channel {vcid}: a packet spanning frames declares {declared} bytes, \
                 more than the {max} allowed — desynchronised"
            ));
            return Fed::Dropped;
        }
        let held = self.channel.partial.len();
        if held < declared {
            return Fed::Growing;
        }
        let mut packet = std::mem::take(&mut self.channel.partial);
        packet.truncate(declared);
        self.deliver(packet, out);
        Fed::Completed(held - declared)
    }

    /// Reads whole packets out of `rest`, keeping the tail for the next frame.
    fn split(&mut self, rest: &[u8], out: &mut Vec<RawPacket>) {
        let mut offset = 0;
        while offset < rest.len() {
            let remaining = &rest[offset..];
            let Some(declared) = declared_length(remaining) else {
                // A primary header split across the frame boundary: keep what there is.
                self.channel.partial.extend_from_slice(remaining);
                return;
            };
            if declared > self.max_packet_length {
                let vcid = self.channel.vcid;
                let max = self.max_packet_length;
                let dropped = remaining.len();
                self.counters.discarded_bytes =
                    self.counters.discarded_bytes.saturating_add(dropped as u64);
                self.warn(format!(
                    "virtual channel {vcid}: a packet declares {declared} bytes, more than the \
                     {max} allowed — {dropped} byte(s) of data field discarded"
                ));
                return;
            }
            if declared > remaining.len() {
                self.channel.partial.extend_from_slice(remaining);
                return;
            }
            self.deliver(remaining[..declared].to_vec(), out);
            offset += declared;
        }
    }

    /// A frame in which no packet starts: the whole data field continues the packet in
    /// progress, and with no packet in progress it is a fragment nothing can use.
    fn continue_only(&mut self, data: &[u8], contiguous: bool, out: &mut Vec<RawPacket>) {
        if !self.channel.is_assembling() {
            self.counters.discarded_bytes = self
                .counters
                .discarded_bytes
                .saturating_add(data.len() as u64);
            if contiguous && !data.is_empty() {
                let vcid = self.channel.vcid;
                let len = data.len();
                self.warn(format!(
                    "virtual channel {vcid}: {len}-byte continuation with no packet in front \
                     of it"
                ));
            }
            return;
        }
        if let Fed::Completed(leftover) = self.feed(data, out)
            && leftover > 0
        {
            self.counters.discarded_bytes = self
                .counters
                .discarded_bytes
                .saturating_add(leftover as u64);
            let vcid = self.channel.vcid;
            self.warn(format!(
                "virtual channel {vcid}: a packet ended {leftover} byte(s) before the end of a \
                 frame that declared no packet start"
            ));
        }
    }

    /// A frame in which a packet starts at `pointer`.
    fn start_at(
        &mut self,
        pointer: usize,
        data: &[u8],
        contiguous: bool,
        out: &mut Vec<RawPacket>,
    ) {
        // `TmFrame::parse` has already checked the pointer against the data field, but a
        // bound that only exists upstream is a bound that moves away from what depends on it.
        let (head, rest) = data.split_at(pointer.min(data.len()));
        if self.channel.is_assembling() {
            match self.feed(head, out) {
                Fed::Growing => self.abandon(
                    "the first header pointer says the next packet starts before it ended",
                ),
                Fed::Completed(leftover) if leftover > 0 => {
                    self.counters.discarded_bytes = self
                        .counters
                        .discarded_bytes
                        .saturating_add(leftover as u64);
                    let vcid = self.channel.vcid;
                    self.warn(format!(
                        "virtual channel {vcid}: a packet ended {leftover} byte(s) before the \
                         first header pointer"
                    ));
                }
                Fed::Completed(_) | Fed::Dropped => {}
            }
        } else if !head.is_empty() {
            self.counters.discarded_bytes = self
                .counters
                .discarded_bytes
                .saturating_add(head.len() as u64);
            if contiguous {
                let vcid = self.channel.vcid;
                let len = head.len();
                self.warn(format!(
                    "virtual channel {vcid}: {len}-byte leading fragment with no packet in \
                     front of it"
                ));
            }
        }
        self.split(rest, out);
    }
}

/// Rebuilds space packets from transfer frames.
#[derive(Debug)]
pub struct PacketAssembler {
    max_packet_length: usize,
    channels: Vec<VirtualChannel>,
    counters: PacketCounters,
    events: Vec<Event>,
}

impl PacketAssembler {
    /// An assembler that refuses any packet declaring more than `max_packet_length` bytes.
    #[must_use]
    pub fn new(max_packet_length: usize) -> Self {
        Self {
            max_packet_length,
            channels: Vec::new(),
            counters: PacketCounters::default(),
            events: Vec::new(),
        }
    }

    /// The length above which a declared packet is treated as a desync.
    #[must_use]
    pub const fn max_packet_length(&self) -> usize {
        self.max_packet_length
    }

    /// Every virtual channel seen so far.
    #[must_use]
    pub fn channels(&self) -> &[VirtualChannel] {
        &self.channels
    }

    /// The state of one virtual channel, if it has been seen.
    #[must_use]
    pub fn channel(&self, vcid: u8) -> Option<&VirtualChannel> {
        self.channels.iter().find(|channel| channel.vcid == vcid)
    }

    /// Takes one frame's data field apart into packets, appending them to `out`.
    ///
    /// `received` is stamped onto every packet the frame produced, including one that began
    /// several frames ago: a packet's time is when it was completed, because that is the only
    /// instant the ground can name for it.
    ///
    /// An adapter over [`PacketAssembler::push_data`], which holds the whole decision table.
    /// The split is deliberate: a [`TmFrame`] can only be built by parsing bytes, and the
    /// rules below are about four numbers out of the header, so they are tested against those
    /// four numbers rather than against a frame encoder written to agree with them.
    pub fn push_frame(&mut self, frame: TmFrame<'_>, received: Utc, out: &mut Vec<RawPacket>) {
        self.push_data(
            frame.vcid(),
            frame.virtual_frame_count(),
            frame.first_header_pointer(),
            frame.data_field(),
            received,
            out,
        );
    }

    /// Records a frame's virtual channel count without reading its data field.
    ///
    /// For a frame that carries something other than packets: CCSDS 132.0-B-3 §4.1.2.7.2 sets
    /// the sync flag when the data field holds virtual channel access service data, and
    /// §4.1.2.7.6.2 then leaves the first header pointer undefined — so neither the pointer
    /// nor the bytes behind it may be taken apart, and the frame still advances the count
    /// like any other. Skipping it entirely would manufacture a gap on the next packet frame,
    /// which is the same mistake idle frames used to cause.
    ///
    /// The partial packet on that channel is left alone, for the same reason an idle frame
    /// leaves it alone: the standard does not say the sender abandoned it.
    pub fn note_frame(&mut self, vcid: u8, frame_count: u8) {
        let index = if let Some(index) = self.channels.iter().position(|c| c.vcid == vcid) {
            index
        } else {
            self.channels.push(VirtualChannel::new(vcid));
            self.channels.len().saturating_sub(1)
        };
        let Some(channel) = self.channels.get_mut(index) else {
            return;
        };
        let mut state = Assembly {
            channel,
            counters: &mut self.counters,
            events: &mut self.events,
            max_packet_length: self.max_packet_length,
            received: Utc::EPOCH,
        };
        let _contiguous = state.check_continuity(frame_count);
    }

    /// The frame-to-packet rules, over one frame's header fields and its data field.
    ///
    /// CCSDS 132.0-B-3 section 4.1.2.3 for the first header pointer and section 4.1.4 for
    /// what the data field holds. In order: the virtual channel frame count decides whether
    /// anything half-assembled can still be finished, [`ONLY_IDLE_DATA`] ends the frame
    /// there, [`NO_PACKET_START`] hands the whole data field to the packet in progress, and
    /// otherwise the bytes in front of the pointer finish that packet and the rest are read
    /// by their declared lengths.
    ///
    /// The frame count is recorded even for an idle frame, which the order above reads as
    /// skipped: an idle frame is a frame the spacecraft transmitted and it advances the
    /// virtual channel frame count like any other. Not recording it would manufacture a gap
    /// on the next real frame, so a link that idles at all would report continuous loss.
    pub fn push_data(
        &mut self,
        vcid: u8,
        frame_count: u8,
        first_header_pointer: u16,
        data: &[u8],
        received: Utc,
        out: &mut Vec<RawPacket>,
    ) {
        let index = if let Some(index) = self.channels.iter().position(|c| c.vcid == vcid) {
            index
        } else {
            self.channels.push(VirtualChannel::new(vcid));
            self.channels.len().saturating_sub(1)
        };
        let Some(channel) = self.channels.get_mut(index) else {
            return;
        };
        let mut state = Assembly {
            channel,
            counters: &mut self.counters,
            events: &mut self.events,
            max_packet_length: self.max_packet_length,
            received,
        };
        let contiguous = state.check_continuity(frame_count);
        match first_header_pointer {
            // Fill. The partial packet is left alone: CCSDS 132.0-B-3 section 4.1.4 says the
            // data field holds only idle data, not that the virtual channel gave up on what
            // it was sending.
            //
            // TODO(gs-link-packets): settled — the frame reaches this arm. `Pipeline::push`
            // hands idle frames to `push_frame` like any other, so `check_continuity` above
            // has already recorded the count, the next real frame is not read as a gap, and a
            // packet that spans an idle frame is completed; `pipeline.rs`'s
            // `a_packet_that_spans_an_idle_frame_survives_it` is the test for it.
            //
            // Still open: whether a mission that emits only-idle-data frames *between* the
            // frames of one long packet means the sender abandoned that packet. If any does,
            // this arm has to call `abandon` and count it lost for that mission — which needs
            // a flag on `Framing::TmFrames` and its `PipelineConfig` plumbing, because it
            // cannot be decided from 132.0-B-3 alone and the wrong default either splices two
            // halves of different packets together or loses one packet per idle frame.
            ONLY_IDLE_DATA => {}
            NO_PACKET_START => state.continue_only(data, contiguous, out),
            pointer => state.start_at(usize::from(pointer), data, contiguous, out),
        }
    }

    /// What happened since the last call, zeroing the deltas.
    #[must_use]
    pub fn take_counters(&mut self) -> PacketCounters {
        std::mem::take(&mut self.counters)
    }

    /// Lines about what was lost and why. Drains the queue.
    #[must_use]
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Throws away every partial packet on every channel.
    ///
    /// Called when the synchroniser loses lock: the frames either side of the gap are not
    /// consecutive, so nothing half-assembled can be finished.
    ///
    /// The counters and the queued events survive it. A caller that resets the assembler
    /// still has to be told what the reset cost, and clearing them here is how that number
    /// would disappear.
    pub fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.last_frame_count = None;
            let pending = channel.partial.len();
            if pending == 0 {
                continue;
            }
            channel.partial.clear();
            self.counters.lost = self.counters.lost.saturating_add(1);
            self.counters.discarded_bytes =
                self.counters.discarded_bytes.saturating_add(pending as u64);
            let vcid = channel.vcid;
            self.events.push(Event::warning(
                "link",
                format!(
                    "virtual channel {vcid}: {pending}-byte partial packet abandoned, packet \
                     assembly was reset"
                ),
            ));
        }
    }
}

/// Hands out one finished packet, or counts it as idle fill and drops it.
///
/// A free function because its two callers hold the counters differently: [`Assembly`] borrows
/// them as a field alongside one channel, [`PacketStream`] owns them. Taking
/// `&mut PacketCounters` is what lets both share this, and it is the whole of the reason —
/// a method on either type would work, and would leave the other one duplicating it.
fn deliver(packet: RawPacket, counters: &mut PacketCounters, out: &mut Vec<RawPacket>) {
    if packet.apid() == Some(IDLE_APID) {
        counters.idle = counters.idle.saturating_add(1);
        return;
    }
    counters.packets = counters.packets.saturating_add(1);
    out.push(packet);
}

/// Splits a stream of back-to-back space packets.
///
/// The whole of the framing when a source hands over packets directly: a UDP feed with one
/// packet per datagram, a recorded file, a test fixture. There is no marker and no checksum,
/// so the only thing holding the stream together is that each packet says how long it is —
/// which means one wrong length byte desynchronises everything after it, and
/// [`PacketStream::max_packet_length`] is what catches that instead of allocating a gigabyte.
#[derive(Debug)]
pub struct PacketStream {
    max_packet_length: usize,
    buffer: Vec<u8>,
    counters: PacketCounters,
    events: Vec<Event>,
}

impl PacketStream {
    /// A stream splitter that refuses any packet declaring more than `max_packet_length`
    /// bytes.
    #[must_use]
    pub fn new(max_packet_length: usize) -> Self {
        Self {
            max_packet_length,
            buffer: Vec::new(),
            counters: PacketCounters::default(),
            events: Vec::new(),
        }
    }

    /// The length above which a declared packet is treated as a desync.
    #[must_use]
    pub const fn max_packet_length(&self) -> usize {
        self.max_packet_length
    }

    /// Bytes held that are not yet a whole packet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Feeds bytes in and appends whatever whole packets came out.
    ///
    /// Each packet's length comes from its own primary header — CCSDS 133.0-B-2 section
    /// 4.1.3.5.3, via [`declared_length`] — so one wrong length byte moves every boundary
    /// after it. A declared length above [`PacketStream::max_packet_length`] is therefore
    /// taken as a desynchronisation and not as a packet: the buffer is thrown away rather
    /// than searched, because a packet stream carries no marker to search *for*, and a
    /// station that resynchronised on a guess would hand the decoder something that parses.
    pub fn push(&mut self, bytes: &[u8], received: Utc, out: &mut Vec<RawPacket>) {
        self.buffer.extend_from_slice(bytes);
        let mut offset = 0;
        let mut desynchronised = None;
        while offset < self.buffer.len() {
            let remaining = &self.buffer[offset..];
            let Some(declared) = declared_length(remaining) else {
                break;
            };
            if declared > self.max_packet_length {
                desynchronised = Some(declared);
                break;
            }
            if declared > remaining.len() {
                break;
            }
            let packet = RawPacket {
                received,
                bytes: remaining[..declared].to_vec(),
                vcid: None,
            };
            deliver(packet, &mut self.counters, out);
            offset += declared;
        }
        if let Some(declared) = desynchronised {
            let dropped = self.buffer.len() - offset;
            let max = self.max_packet_length;
            self.counters.discarded_bytes =
                self.counters.discarded_bytes.saturating_add(dropped as u64);
            self.events.push(Event::at(
                received,
                Severity::Error,
                "link",
                format!(
                    "packet stream declares {declared} bytes, more than the {max} allowed — \
                     desynchronised, {dropped} byte(s) discarded"
                ),
            ));
            self.buffer.clear();
            return;
        }
        // Once per call, not once per packet: this is a move of everything still held.
        if offset > 0 {
            self.buffer.drain(..offset);
        }
    }

    /// Throws away the partial packet at a message boundary.
    ///
    /// A datagram holds whole packets or it holds a mistake. Carrying the tail of one
    /// datagram into the next would splice two unrelated packets together and hand the
    /// decoder something that parses.
    pub fn flush(&mut self) {
        let pending = self.buffer.len();
        if pending == 0 {
            return;
        }
        self.buffer.clear();
        self.counters.lost = self.counters.lost.saturating_add(1);
        self.counters.discarded_bytes =
            self.counters.discarded_bytes.saturating_add(pending as u64);
        self.events.push(Event::warning(
            "link",
            format!(
                "{pending} byte(s) of a partial packet discarded: the stream stopped inside \
                 a packet. On a datagram source that usually means the sender's MTU is wrong; \
                 on a file it means the recording was cut short"
            ),
        ));
    }

    /// What happened since the last call, zeroing the deltas.
    #[must_use]
    pub fn take_counters(&mut self) -> PacketCounters {
        std::mem::take(&mut self.counters)
    }

    /// Lines about what was lost and why. Drains the queue.
    #[must_use]
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A space packet with `data` as its data field, APID `apid`.
    ///
    /// CCSDS 133.0-B-2 section 4.1.3.5.3: the length count is one less than the data field,
    /// so `data` cannot be empty — there is no packet with no data field.
    fn packet(apid: u16, data: &[u8]) -> Vec<u8> {
        assert!(
            !data.is_empty(),
            "a space packet has at least one data octet"
        );
        let mut bytes = Vec::with_capacity(SPACE_PACKET_HEADER_BYTES + data.len());
        bytes.extend_from_slice(&(0x0800 | (apid & 0x07FF)).to_be_bytes());
        bytes.extend_from_slice(&0xC000_u16.to_be_bytes());
        bytes.extend_from_slice(&((data.len() - 1) as u16).to_be_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    /// A primary header declaring `total` bytes with no body behind it.
    fn header_declaring(total: usize) -> Vec<u8> {
        let mut bytes = vec![0x08, 0x02, 0xC0, 0x00];
        bytes.extend_from_slice(&((total - SPACE_PACKET_HEADER_BYTES - 1) as u16).to_be_bytes());
        bytes
    }

    fn at(seconds: i64) -> Utc {
        Utc::from_unix_secs(seconds)
    }

    #[test]
    fn declared_length_is_the_header_plus_the_count_plus_one() {
        assert_eq!(declared_length(&packet(1, &[0xAA; 4])), Some(10));
        assert_eq!(declared_length(&[0, 0, 0, 0, 0, 0]), Some(7));
        assert_eq!(declared_length(&[0, 0, 0, 0, 0xFF, 0xFF]), Some(65_542));
    }

    #[test]
    fn declared_length_needs_a_whole_primary_header() {
        assert_eq!(declared_length(&[]), None);
        assert_eq!(declared_length(&[0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn a_packet_split_across_two_frames_is_rejoined() {
        let long = packet(0x2A, &[0xAA; 14]);
        let short = packet(0x2B, &[1, 2]);
        assert_eq!((long.len(), short.len()), (20, 8));

        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 0, &long[..12], at(1), &mut out);
        assert!(out.is_empty());
        assert_eq!(assembler.channel(1).map(VirtualChannel::pending), Some(12));

        let mut rest = long[12..].to_vec();
        rest.extend_from_slice(&short);
        assembler.push_data(1, 1, 8, &rest, at(2), &mut out);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].bytes, long);
        assert_eq!(out[0].vcid, Some(1));
        // A packet's time is when it was completed, not when its first frame arrived.
        assert_eq!(out[0].received, at(2));
        assert_eq!(out[1].bytes, short);
        let counters = assembler.take_counters();
        assert_eq!(counters.packets, 2);
        assert_eq!(counters.lost, 0);
        assert_eq!(counters.discarded_bytes, 0);
        assert!(assembler.take_events().is_empty());
    }

    #[test]
    fn a_gap_in_the_frame_count_discards_the_partial_instead_of_gluing_it() {
        let long = packet(0x2A, &[0xAA; 14]);
        let short = packet(0x2B, &[1, 2]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 0, &long[..12], at(1), &mut out);

        let mut rest = long[12..].to_vec();
        rest.extend_from_slice(&short);
        // Frame 1 never arrived: the eight bytes in front of the pointer belong to a packet
        // that can no longer be finished.
        assembler.push_data(1, 2, 8, &rest, at(2), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, short);
        let counters = assembler.take_counters();
        assert_eq!(counters.packets, 1);
        assert_eq!(counters.lost, 1);
        assert_eq!(counters.discarded_bytes, 20);
        let events = assembler.take_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].message.contains("frame count jumped from 0 to 2"));
        assert!(events[0].message.contains("1 frame(s) lost"));
    }

    #[test]
    fn an_idle_frame_keeps_the_frame_count_moving() {
        let long = packet(0x2A, &[0xAA; 14]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 0, &long[..12], at(1), &mut out);
        assembler.push_data(1, 1, ONLY_IDLE_DATA, &[0x55; 16], at(2), &mut out);
        assembler.push_data(1, 2, 8, &long[12..], at(3), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, long);
        let counters = assembler.take_counters();
        assert_eq!(counters.lost, 0);
        assert_eq!(counters.discarded_bytes, 0);
        assert!(assembler.take_events().is_empty());
    }

    #[test]
    fn a_frame_with_no_packet_start_appends_the_whole_data_field() {
        let long = packet(0x03, &[0x5A; 30]);
        assert_eq!(long.len(), 36);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(0, 10, 0, &long[..12], at(1), &mut out);
        assembler.push_data(0, 11, NO_PACKET_START, &long[12..24], at(2), &mut out);
        assert!(out.is_empty());
        assert_eq!(assembler.channel(0).map(VirtualChannel::pending), Some(24));
        assembler.push_data(0, 12, NO_PACKET_START, &long[24..], at(3), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, long);
        assert_eq!(assembler.take_counters().packets, 1);
        assert!(assembler.take_events().is_empty());
    }

    #[test]
    fn a_continuation_with_no_packet_in_front_of_it_is_discarded() {
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        // The first frame after a lock: the tail of a packet whose start was never seen is
        // expected, and is not an error.
        assembler.push_data(2, 7, NO_PACKET_START, &[0x11; 12], at(1), &mut out);
        assert!(out.is_empty());
        assert!(assembler.take_events().is_empty());
        // The next one is contiguous, so the same thing now means something went wrong.
        assembler.push_data(2, 8, NO_PACKET_START, &[0x11; 12], at(2), &mut out);
        assert!(out.is_empty());
        assert_eq!(assembler.take_events().len(), 1);
        let counters = assembler.take_counters();
        assert_eq!(counters.discarded_bytes, 24);
        assert_eq!(counters.packets, 0);
        assert_eq!(counters.lost, 0);
    }

    #[test]
    fn a_leading_fragment_is_only_reported_once_the_channel_is_contiguous() {
        let short = packet(0x2B, &[1, 2]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        let mut data = vec![0xEE; 4];
        data.extend_from_slice(&short);
        assembler.push_data(3, 0, 4, &data, at(1), &mut out);
        assert_eq!(out.len(), 1);
        assert!(assembler.take_events().is_empty());

        assembler.push_data(3, 1, 4, &data, at(2), &mut out);
        assert_eq!(out.len(), 2);
        let events = assembler.take_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].message.contains("leading fragment"));
        assert_eq!(assembler.take_counters().discarded_bytes, 8);
    }

    #[test]
    fn a_declared_length_above_the_maximum_is_a_desync_not_a_packet() {
        let mut assembler = PacketAssembler::new(64);
        let mut out = Vec::new();
        let mut data = header_declaring(1_000);
        data.extend_from_slice(&[0x77; 6]);
        assembler.push_data(1, 0, 0, &data, at(1), &mut out);

        assert!(out.is_empty());
        assert_eq!(
            assembler.channel(1).map(VirtualChannel::is_assembling),
            Some(false)
        );
        let counters = assembler.take_counters();
        assert_eq!(counters.packets, 0);
        assert_eq!(counters.discarded_bytes, 12);
        let events = assembler.take_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].message.contains("1000 bytes"));
    }

    #[test]
    fn a_partial_packet_that_grows_past_the_maximum_is_dropped() {
        let mut assembler = PacketAssembler::new(64);
        let mut out = Vec::new();
        // Three octets of a header: not enough to know the length yet.
        assembler.push_data(1, 0, 0, &[0x08, 0x02, 0x00], at(1), &mut out);
        assert_eq!(assembler.channel(1).map(VirtualChannel::pending), Some(3));
        // The rest of it declares a thousand bytes.
        assembler.push_data(1, 1, NO_PACKET_START, &[0x00, 0x03, 0xC5], at(2), &mut out);

        assert!(out.is_empty());
        assert_eq!(assembler.channel(1).map(VirtualChannel::pending), Some(0));
        let counters = assembler.take_counters();
        assert_eq!(counters.lost, 1);
        assert_eq!(counters.discarded_bytes, 6);
        assert_eq!(assembler.take_events().len(), 1);
    }

    #[test]
    fn a_packet_ending_before_the_pointer_is_reported_and_the_rest_still_read() {
        let long = packet(0x2A, &[0xAA; 14]);
        let short = packet(0x2B, &[1, 2]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 0, &long[..12], at(1), &mut out);
        // The pointer says ten bytes of continuation; the packet needed eight.
        let mut rest = long[12..].to_vec();
        rest.extend_from_slice(&[0xFF, 0xFF]);
        rest.extend_from_slice(&short);
        assembler.push_data(1, 1, 10, &rest, at(2), &mut out);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].bytes, long);
        assert_eq!(out[1].bytes, short);
        let counters = assembler.take_counters();
        assert_eq!(counters.discarded_bytes, 2);
        let events = assembler.take_events();
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .message
                .contains("before the first header pointer")
        );
    }

    #[test]
    fn a_pointer_that_starts_before_the_partial_ended_loses_it() {
        let long = packet(0x2A, &[0xAA; 14]);
        let short = packet(0x2B, &[1, 2]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 0, &long[..12], at(1), &mut out);
        // A pointer of zero with a packet still in progress: the two disagree.
        assembler.push_data(1, 1, 0, &short, at(2), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, short);
        let counters = assembler.take_counters();
        assert_eq!(counters.lost, 1);
        assert_eq!(counters.discarded_bytes, 12);
        assert_eq!(assembler.take_events().len(), 1);
    }

    #[test]
    fn a_pointer_past_the_end_of_the_data_field_is_survivable() {
        // `TmFrame::parse` refuses this frame, so it should never arrive; if it does, the
        // pointer is clamped and the whole data field is discarded rather than indexed.
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 700, &[0x22; 12], at(1), &mut out);
        assert!(out.is_empty());
        assert_eq!(assembler.take_counters().discarded_bytes, 12);
        assert_eq!(assembler.channel(1).map(VirtualChannel::pending), Some(0));
    }

    #[test]
    fn a_primary_header_split_across_the_boundary_waits_for_the_rest() {
        let short = packet(0x2B, &[1, 2, 3, 4]);
        assert_eq!(short.len(), 10);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(4, 0, 0, &short[..3], at(1), &mut out);
        assert!(out.is_empty());
        assert_eq!(assembler.channel(4).map(VirtualChannel::pending), Some(3));
        assembler.push_data(4, 1, NO_PACKET_START, &short[3..], at(2), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, short);
    }

    #[test]
    fn an_idle_packet_is_counted_and_not_handed_out() {
        let idle = packet(IDLE_APID, &[0x00; 4]);
        let real = packet(0x2B, &[1, 2]);
        let mut data = idle.clone();
        data.extend_from_slice(&real);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 0, 0, &data, at(1), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, real);
        let counters = assembler.take_counters();
        assert_eq!(counters.idle, 1);
        assert_eq!(counters.packets, 1);
    }

    #[test]
    fn two_virtual_channels_keep_separate_partial_packets() {
        let a = packet(0x10, &[0xA1; 14]);
        let b = packet(0x20, &[0xB2; 14]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(0, 0, 0, &a[..12], at(1), &mut out);
        assembler.push_data(1, 200, 0, &b[..12], at(1), &mut out);
        assert!(out.is_empty());
        assembler.push_data(1, 201, NO_PACKET_START, &b[12..], at(2), &mut out);
        assembler.push_data(0, 1, NO_PACKET_START, &a[12..], at(2), &mut out);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].bytes, b);
        assert_eq!(out[0].vcid, Some(1));
        assert_eq!(out[1].bytes, a);
        assert_eq!(out[1].vcid, Some(0));
        assert_eq!(assembler.channels().len(), 2);
        assert_eq!(assembler.take_counters().lost, 0);
    }

    #[test]
    fn the_frame_count_wraps_without_reporting_a_gap() {
        let long = packet(0x2A, &[0xAA; 14]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 255, 0, &long[..12], at(1), &mut out);
        assembler.push_data(1, 0, NO_PACKET_START, &long[12..], at(2), &mut out);
        assert_eq!(out.len(), 1);
        assert!(assembler.take_events().is_empty());
        assert_eq!(assembler.take_counters().lost, 0);
    }

    #[test]
    fn a_repeated_frame_is_a_discontinuity() {
        let long = packet(0x2A, &[0xAA; 14]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(1, 5, 0, &long[..12], at(1), &mut out);
        assembler.push_data(1, 5, 0, &long[..12], at(2), &mut out);
        assert!(out.is_empty());
        assert_eq!(assembler.take_counters().lost, 1);
        assert_eq!(assembler.take_events().len(), 1);
    }

    #[test]
    fn reset_abandons_every_partial_packet_and_keeps_the_counters() {
        let a = packet(0x10, &[0xA1; 14]);
        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        assembler.push_data(0, 0, 0, &a[..12], at(1), &mut out);
        assembler.push_data(1, 0, 0, &a[..12], at(1), &mut out);
        assembler.reset();

        assert!(assembler.channels().iter().all(|c| !c.is_assembling()));
        assert!(
            assembler
                .channels()
                .iter()
                .all(|c| c.last_frame_count().is_none())
        );
        let counters = assembler.take_counters();
        assert_eq!(counters.lost, 2);
        assert_eq!(counters.discarded_bytes, 24);
        assert_eq!(assembler.take_events().len(), 2);

        // The channel is usable again, and the frame after the reset is not called a gap.
        assembler.push_data(0, 9, 0, &a[..12], at(2), &mut out);
        assert!(assembler.take_events().is_empty());
    }

    #[test]
    fn the_assembler_reports_what_it_was_built_with() {
        let assembler = PacketAssembler::new(1_024);
        assert_eq!(assembler.max_packet_length(), 1_024);
        assert!(assembler.channels().is_empty());
        assert!(assembler.channel(0).is_none());
    }

    #[test]
    fn counters_merge_by_addition() {
        let mut counters = PacketCounters {
            packets: 2,
            lost: 1,
            idle: 0,
            discarded_bytes: 10,
        };
        counters.merge(PacketCounters {
            packets: 3,
            lost: 0,
            idle: 4,
            discarded_bytes: 5,
        });
        assert_eq!(counters.packets, 5);
        assert_eq!(counters.lost, 1);
        assert_eq!(counters.idle, 4);
        assert_eq!(counters.discarded_bytes, 15);
    }

    /// A TM transfer frame with no secondary header, no insert zone and no trailers.
    ///
    /// CCSDS 132.0-B-3 section 4.1.2: two octets of version, spacecraft id, virtual channel
    /// id and OCF flag, one of master channel frame count, one of virtual channel frame
    /// count, then the data field status whose low eleven bits are the first header pointer.
    /// Segment length identifier `0b11`, as a frame carrying packets has.
    fn tm_frame(vcid: u8, frame_count: u8, first_header_pointer: u16, data: &[u8]) -> Vec<u8> {
        let word0: u16 = (0x2A << 5) | (u16::from(vcid & 0x7) << 1);
        let status: u16 = 0x1800 | (first_header_pointer & 0x07FF);
        let mut bytes = Vec::with_capacity(6 + data.len());
        bytes.extend_from_slice(&word0.to_be_bytes());
        bytes.push(frame_count);
        bytes.push(frame_count);
        bytes.extend_from_slice(&status.to_be_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn push_frame_reads_the_fields_push_data_was_tested_on() {
        let long = packet(0x2A, &[0xAA; 14]);
        let first = tm_frame(1, 0, 0, &long[..12]);
        let second = tm_frame(1, 1, NO_PACKET_START, &long[12..]);
        let options = crate::frame::FrameOptions::default();

        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        for bytes in [&first, &second] {
            let frame = TmFrame::parse(bytes, options).expect("a frame this test built");
            assembler.push_frame(frame, at(5), &mut out);
        }

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, long);
        assert_eq!(out[0].vcid, Some(1));
        assert_eq!(out[0].received, at(5));

        // The same two frames through the tested half, field by field.
        let mut direct = PacketAssembler::new(65_542);
        let mut direct_out = Vec::new();
        direct.push_data(1, 0, 0, &long[..12], at(5), &mut direct_out);
        direct.push_data(1, 1, NO_PACKET_START, &long[12..], at(5), &mut direct_out);
        assert_eq!(direct_out.len(), out.len());
        assert_eq!(direct_out[0].bytes, out[0].bytes);
        assert_eq!(direct.take_counters(), assembler.take_counters());
    }

    #[test]
    fn push_frame_leaves_an_idle_frame_alone() {
        let long = packet(0x2A, &[0xAA; 14]);
        let options = crate::frame::FrameOptions::default();
        let frames = [
            tm_frame(1, 0, 0, &long[..12]),
            tm_frame(1, 1, ONLY_IDLE_DATA, &[0x55; 12]),
            tm_frame(1, 2, NO_PACKET_START, &long[12..]),
        ];

        let mut assembler = PacketAssembler::new(65_542);
        let mut out = Vec::new();
        for bytes in &frames {
            let frame = TmFrame::parse(bytes, options).expect("a frame this test built");
            assembler.push_frame(frame, at(5), &mut out);
        }

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, long);
        assert_eq!(assembler.take_counters().lost, 0);
        assert!(assembler.take_events().is_empty());
    }

    #[test]
    fn a_stream_splits_back_to_back_packets_in_order() {
        let first = packet(0x11, &[1, 2, 3]);
        let second = packet(0x12, &[4]);
        let third = packet(0x13, &[5, 6]);
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second);
        bytes.extend_from_slice(&third);

        let mut stream = PacketStream::new(65_542);
        let mut out = Vec::new();
        stream.push(&bytes, at(7), &mut out);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].bytes, first);
        assert_eq!(out[1].bytes, second);
        assert_eq!(out[2].bytes, third);
        assert!(out.iter().all(|p| p.vcid.is_none()));
        assert_eq!(out[0].received, at(7));
        assert_eq!(stream.pending(), 0);
        assert_eq!(stream.take_counters().packets, 3);
    }

    #[test]
    fn a_stream_packet_split_across_two_reads_is_rejoined() {
        let only = packet(0x11, &[1, 2, 3, 4, 5, 6]);
        let mut stream = PacketStream::new(65_542);
        let mut out = Vec::new();
        for byte in &only {
            stream.push(std::slice::from_ref(byte), at(1), &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, only);
        assert_eq!(stream.pending(), 0);
    }

    #[test]
    fn an_empty_push_produces_nothing() {
        let mut stream = PacketStream::new(65_542);
        let mut out = Vec::new();
        stream.push(&[], at(1), &mut out);
        assert!(out.is_empty());
        assert_eq!(stream.pending(), 0);
        assert_eq!(stream.take_counters(), PacketCounters::default());
        assert!(stream.take_events().is_empty());
    }

    #[test]
    fn a_hostile_length_field_desynchronises_the_whole_stream() {
        let good = packet(0x11, &[1, 2, 3]);
        let mut bytes = good.clone();
        bytes.extend_from_slice(&header_declaring(1_000));
        bytes.extend_from_slice(&[0x99; 8]);

        let mut stream = PacketStream::new(64);
        let mut out = Vec::new();
        stream.push(&bytes, at(1), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, good);
        assert_eq!(stream.pending(), 0);
        let counters = stream.take_counters();
        assert_eq!(counters.packets, 1);
        assert_eq!(counters.discarded_bytes, 14);
        let events = stream.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, Severity::Error);
        assert!(events[0].message.contains("desynchronised"));
    }

    #[test]
    fn a_stream_flush_reports_the_truncated_tail() {
        let only = packet(0x11, &[1, 2, 3, 4]);
        let mut stream = PacketStream::new(65_542);
        let mut out = Vec::new();
        stream.push(&only[..5], at(1), &mut out);
        assert_eq!(stream.pending(), 5);
        stream.flush();

        assert!(out.is_empty());
        assert_eq!(stream.pending(), 0);
        let counters = stream.take_counters();
        assert_eq!(counters.lost, 1);
        assert_eq!(counters.discarded_bytes, 5);
        assert_eq!(stream.take_events().len(), 1);

        // A flush with nothing held says nothing.
        stream.flush();
        assert_eq!(stream.take_counters(), PacketCounters::default());
        assert!(stream.take_events().is_empty());
    }

    #[test]
    fn an_idle_packet_in_a_stream_is_dropped() {
        let idle = packet(IDLE_APID, &[0; 8]);
        let real = packet(0x11, &[1]);
        let mut bytes = idle;
        bytes.extend_from_slice(&real);

        let mut stream = PacketStream::new(65_542);
        let mut out = Vec::new();
        stream.push(&bytes, at(1), &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bytes, real);
        let counters = stream.take_counters();
        assert_eq!(counters.idle, 1);
        assert_eq!(counters.packets, 1);
        assert_eq!(stream.max_packet_length(), 65_542);
    }

    #[test]
    fn the_longest_packet_ccsds_can_express_is_not_refused() {
        let body = vec![0x5A; 65_536];
        let full = packet(0x11, &body);
        assert_eq!(full.len(), 65_542);
        let mut stream = PacketStream::new(65_542);
        let mut out = Vec::new();
        stream.push(&full, at(1), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 65_542);
        assert!(stream.take_events().is_empty());
    }
}
