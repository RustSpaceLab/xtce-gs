//! The one place the borrow ends.
//!
//! `Decoder::decode` returns a `DecodedPacket<'db, 'p>` whose values point into the packet
//! buffer and into the definition. That is the right shape for a decoder and the wrong one
//! for anything after it: nothing with those lifetimes can sit in a history, cross a channel,
//! or outlive the datagram it came from. Every value the station keeps is copied out here,
//! once, into [`xtce_gs_core::Sample`] — and after this module nothing in the workspace names
//! a lifetime.
//!
//! This is the hot path. A pass at 2 Mbit/s is a few thousand packets a second and each one
//! carries tens of parameters, so no *container* is allocated per packet: the caller owns one
//! [`Batch`] and one [`PacketDecoder`] for the life of the session and they are refilled, not
//! rebuilt. What is left is one heap allocation and one memcpy per enumeration label, text or
//! binary value — not a refcount bump. [`xtce_gs_core::Value`] holds an `Arc`, but the value
//! it is built from is a `Cow` borrowed from the packet or a `&'db str` borrowed from the
//! definition, so `Value::from_raw` and `Value::from_eng` reach `Arc::from` on a borrowed
//! `&[u8]` or `&str`, which allocates a fresh block and copies into it. The `Arc` buys
//! sharing *after* this module, not on the way in.
//!
//! An enumeration label is the case that costs most, because the bytes it copies already live
//! in the `XtceDb` for the life of the session: ten enumerated parameters at 2 000 packets a
//! second is 20 000 allocations a second. Interning them per (parameter, label) would make
//! the copy a real refcount bump, and is not done here — [`project`] is re-exported from the
//! crate root, so the cache it would need is a public signature change. A definition with no
//! enumeration, no string and no binary field — JPSS, which is why this went unnoticed —
//! allocates nothing per packet at all.
//!
//! # Two entry points, because the borrow checker allows exactly two
//!
//! [`Decoder::decode_into`] is declared `decode_into<'p>(&self, packet: &mut DecodedPacket<'db,
//! 'p>, data: &'p [u8])`. The buffer's `'p` *is* the packet bytes' `'p`, so one buffer can
//! only ever serve packets that all borrow from the same live region. That splits the callers
//! in two, and this module offers one function for each rather than pretending the split is
//! not there:
//!
//! * [`PacketDecoder::decode_into`] — for a caller holding every packet at once: a drained
//!   `Vec<RawPacket>`, or a recording read whole into memory by the export path. All of them
//!   borrow the same `'p`, which is the *only* case in which a [`DecodedPacket`] can be
//!   reused, so this is the only entry point that reuses the decoder's own `Vec` and hash
//!   table. It allocates nothing per packet.
//! * [`PacketDecoder::decode_owned`] — for a caller holding one owned [`RawPacket`] at a time,
//!   each dropped before the next arrives. Nothing outlives the bytes, so no buffer can be
//!   threaded through; this calls [`Decoder::decode`] and the reuse it still gets is the
//!   [`Batch`] and its `Vec<Sample>`, which is most of the allocation either way.
//!
//! Neither needs `unsafe`, and a [`PacketDecoder`] holding the buffer as a field would: the
//! field's type names two lifetimes, one of which is the definition this crate deliberately
//! reaches through an `Arc` — see [`crate::session`] for why nothing here is stored beside it.

use std::sync::Arc;

use xtce_decode::{DecodeError, DecodedPacket, Decoder};
use xtce_gs_core::{Batch, Event, LinkStats, RawPacket, Sample, Severity, Utc, Value};
use xtce_model::ContainerId;

use crate::sctime::SpacecraftClock;

/// APIDs an eleven-bit field can name.
pub const APID_COUNT: usize = 2048;

/// The modulus of the CCSDS packet sequence count: fourteen bits.
pub const SEQUENCE_MODULUS: u32 = 16_384;

/// What this module calls itself on the event log.
const SOURCE: &str = "decode";

/// An empty batch, for a caller that reuses one across packets.
///
/// The container is a placeholder that [`PacketDecoder::decode_into`] overwrites. It exists
/// because [`Batch`] has no `Default` — a batch with no container is not a thing the store
/// should ever be handed — and a caller reusing a buffer needs somewhere to start.
#[must_use]
pub fn empty_batch() -> Batch {
    Batch {
        container: ContainerId::new(0),
        received: Utc::EPOCH,
        spacecraft: None,
        apid: 0,
        sequence: 0,
        sequence_gap: false,
        samples: Vec::new(),
    }
}

/// A short, stable name for the *kind* of a decode failure.
///
/// A rejection is counted here and reported by the caller, which holds the error — see
/// [`PacketDecoder::decode_into`] for why the split runs that way. But "refused 7 200 packets"
/// with no reason is useless to an operator, and 7 200 `Display` lines are not a summary
/// either: a definition pointed at the wrong stream refuses every packet with
/// `UnrecognizedPacket`, and one that is merely a version behind refuses a few with `Bits`.
/// Those are different faults, and this is the label that keeps them apart in a tally.
///
/// One caller tallies by it: `xtce-gs export`, which keeps a count per kind and prints them
/// beside the packet total at the end of the run. [`crate::Session`] does not — its decode
/// loop keeps the first error's full `Display` per batch and logs that one line — so a live
/// session still reports the two faults above as whichever happened to come first. Closing
/// that is a change to [`crate::session`], not to this function.
///
/// [`DecodeError`] is `#[non_exhaustive]`, so a variant added upstream lands on `"other"`
/// rather than failing this crate's build.
#[must_use]
pub fn error_kind(error: &DecodeError) -> &'static str {
    match error {
        DecodeError::Bits { .. } => "field past the end of the packet",
        DecodeError::NoSuchContainer { .. } => "no such container",
        DecodeError::AmbiguousRoot { .. } => "ambiguous root container",
        DecodeError::UnrecognizedPacket { .. } => "packet type not described",
        DecodeError::AmbiguousPacket { .. } => "ambiguous packet type",
        DecodeError::ParameterNotYetDecoded { .. } => "forward parameter reference",
        DecodeError::IncomparableValue { .. } => "incomparable comparison literal",
        DecodeError::InvalidText { .. } => "invalid text",
        DecodeError::UnterminatedString { .. } => "unterminated string",
        DecodeError::UnknownEnumeration { .. } => "value outside its enumeration",
        DecodeError::Calibration { .. } => "calibration refused its input",
        DecodeError::BadFieldSize { .. } => "unusable dynamic field size",
        DecodeError::NoDiscreteLookupMatch { .. } => "no discrete lookup matched",
        DecodeError::Unsupported { .. } => "unsupported XTCE construct",
        DecodeError::DanglingIndex { .. } => "inconsistent definition",
        _ => "other",
    }
}

/// Projects every decoded value into an owned sample. `out` is cleared first.
///
/// Every sample takes the *same* time, because the packet is the instant. Giving parameters
/// within one packet different times would put a slope on a plot that is really one sample.
pub fn project(decoded: &DecodedPacket<'_, '_>, time: Utc, out: &mut Vec<Sample>) {
    // `clear` keeps the capacity; `reserve` after it is a no-op once the vector has grown to
    // the widest container the session sees. Between them this is the allocation that the
    // whole signature exists to avoid — `Vec::with_capacity` per packet is thousands of
    // allocations a second at 2 Mbit/s.
    out.clear();
    out.reserve(decoded.len());
    for value in decoded.values() {
        out.push(Sample {
            parameter: value.parameter,
            time,
            raw: Value::from_raw(&value.raw),
            eng: Value::from_eng(&value.eng),
        });
    }
}

/// Per-APID packet sequence counts, and what they say was lost.
///
/// CCSDS 133.0-B-2, Packet Sequence Control (§4.1.3.4): each APID has its own fourteen-bit
/// packet sequence count, incremented once per packet and wrapping at 16 384. A gap in it is
/// the only evidence a ground station has that a packet existed and did not arrive — the link
/// cannot see it, because a packet lost before the ground was never a frame here — so this is
/// where `sequence_missing` on a real pass actually comes from.
#[derive(Clone, Debug)]
pub struct SequenceTracker {
    last: Vec<Option<u16>>,
    /// Per APID: how many gaps have been reported quietly since the last full line, and how
    /// many packets have arrived contiguously since the last gap.
    ///
    /// A dropout is one line and that is right. A link losing one packet in three is a
    /// thousand lines a second, each naming a different sequence count — so `EventLog`'s
    /// collapsing cannot fold them, and a bounded log fills with them while the one line
    /// that says something else is evicted. See [`SequenceTracker::observe`].
    burst: Vec<Burst>,
    gaps: u64,
    missing: u64,
}

/// What one APID's run of gaps has cost since it was last reported in full.
#[derive(Clone, Copy, Debug, Default)]
struct Burst {
    /// Gaps folded into the running total rather than named.
    gaps: u32,
    /// Packets those gaps imply.
    missing: u32,
    /// Packets that arrived contiguously since the last gap.
    ///
    /// A link that recovers and then breaks again is two dropouts, and the second deserves
    /// the same full line the first got.
    healthy: u32,
}

impl Default for SequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// How many gaps on one APID are folded before the running total is said out loud.
const GAP_SUMMARY_INTERVAL: u32 = 64;

/// Contiguous packets that end a dropout, after which the next gap is named in full again.
///
/// A link that recovers and breaks again is two dropouts, and the second deserves the line
/// the first got. One transfer frame of packets is the order of magnitude wanted here: long
/// enough that a gap every other packet does not reset it, short enough that a pass with two
/// separate dropouts reports two.
const HEALTHY_RUN: u32 = 64;

/// What the log should say about one APID's sequence count, if anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GapReport {
    /// Nothing: either the count was contiguous, or this gap is inside a run already reported.
    Quiet,
    /// The first gap of a dropout. Names the sequence count, which is what says *where*.
    First,
    /// The running total for a dropout that is still going, or has just ended.
    Running {
        /// Gaps on this APID since the first one was named.
        gaps: u32,
        /// Packets those gaps imply.
        missing: u32,
        /// Whether the link has recovered, which is what ends the run.
        recovered: bool,
    },
}

impl SequenceTracker {
    /// Files what `observe` returned and says what the log should carry.
    ///
    /// The counters keep every gap; this decides how many of them become sentences. One line
    /// per gap is right for a dropout and wrong for a link losing one packet in three, where
    /// it is a thousand lines a second that `EventLog` cannot collapse — each names a
    /// different sequence count — and the log then holds nothing else.
    pub fn note(&mut self, apid: u16, missed: u16) -> GapReport {
        let Some(burst) = self.burst.get_mut(usize::from(apid)) else {
            return GapReport::Quiet;
        };
        if missed == 0 {
            burst.healthy = burst.healthy.saturating_add(1);
            if burst.healthy < HEALTHY_RUN || burst.gaps == 0 {
                return GapReport::Quiet;
            }
            let report = GapReport::Running {
                gaps: burst.gaps,
                missing: burst.missing,
                recovered: true,
            };
            *burst = Burst::default();
            return report;
        }

        burst.healthy = 0;
        burst.missing = burst.missing.saturating_add(u32::from(missed));
        if burst.gaps == 0 {
            burst.gaps = 1;
            return GapReport::First;
        }
        burst.gaps = burst.gaps.saturating_add(1);
        if burst.gaps % GAP_SUMMARY_INTERVAL == 0 {
            return GapReport::Running {
                gaps: burst.gaps,
                missing: burst.missing,
                recovered: false,
            };
        }
        GapReport::Quiet
    }

    /// A tracker with a slot for every APID.
    ///
    /// One flat vector of 2 048 slots — 4 KB — rather than a map: the APID *is* the index,
    /// and a hash lookup per packet on the hot path buys nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: vec![None; APID_COUNT],
            burst: vec![Burst::default(); APID_COUNT],
            gaps: 0,
            missing: 0,
        }
    }

    /// Files one packet's count and returns how many packets were missed before it.
    ///
    /// Zero means continuous, and three different things are continuous:
    ///
    /// * the *first* packet of an APID, which has nothing to be compared with — reporting a
    ///   gap for it would put a loss on the log for every APID at the start of every pass;
    /// * a count one above the last, which is the normal case;
    /// * a repeated count, which the modular difference would call 16 383 missing. It is a
    ///   retransmission or a replay that looped, not the loss of a whole counter's worth of
    ///   packets, and a station that reported it as one would be lying about the link.
    ///
    /// Neither out-of-range input is refused, because there is no failure here a caller could
    /// do anything with — but they are not handled the same way, and the difference matters:
    ///
    /// * a count at or above [`SEQUENCE_MODULUS`] cannot exist in fourteen bits and is
    ///   *reduced* modulo it, so 16 384 is filed as 0 and the step from 16 383 to it stays
    ///   continuous;
    /// * an APID at or above [`APID_COUNT`] cannot exist in eleven bits and is *not tracked*:
    ///   nothing is filed, nothing is returned but 0, and it never becomes a gap. It is
    ///   deliberately not masked with `& 0x7FF`, because that would file APID 2 048 in APID
    ///   0's slot and put a fabricated gap on a real channel — a wrong number about a real
    ///   APID is worse than no number about an impossible one.
    ///
    /// `xtce-gs-link` cannot produce either: `SpacePacketBytes::apid` is eleven bits wide and
    /// the count fourteen. This is public API, so a caller outside the crate can.
    pub fn observe(&mut self, apid: u16, sequence: u16) -> u16 {
        let current = u32::from(sequence) % SEQUENCE_MODULUS;
        let Some(slot) = self.last.get_mut(usize::from(apid)) else {
            return 0;
        };
        let Some(previous) = slot.replace(current as u16) else {
            return 0; // First packet of this APID.
        };
        let previous = u32::from(previous) % SEQUENCE_MODULUS;
        if current == previous {
            // TODO(gs-engine-decode-repeat): a repeated count is counted as nothing at all.
            // If a mission ever needs to see retransmissions, this is where a third counter
            // goes — `repeats`, beside `gaps` and `missing`, with a `repeats()` accessor and
            // a `LinkStats` field to match. It needs a decision first: a file replay that
            // loops produces one repeat per APID per lap, and an operator watching a live
            // pass and an analyst replaying a recording want opposite defaults for it.
            return 0;
        }
        // Modular difference done in `u32` with the modulus added first: `sequence` may be
        // below `previous` across a wrap (16 383 → 0 is continuous, not 16 383 lost), and
        // computing this in `u16` underflows there.
        let missed = (current + SEQUENCE_MODULUS - previous - 1) % SEQUENCE_MODULUS;
        if missed == 0 {
            return 0;
        }
        self.gaps = self.gaps.saturating_add(1);
        self.missing = self.missing.saturating_add(u64::from(missed));
        // `missed` is a remainder modulo 16 384 and cannot exceed 16 383.
        missed as u16
    }

    /// The last count seen for an APID, if one has arrived.
    #[must_use]
    pub fn last_seen(&self, apid: u16) -> Option<u16> {
        self.last.get(apid as usize).copied().flatten()
    }

    /// How many times a gap was seen, over every APID.
    #[must_use]
    pub const fn gaps(&self) -> u64 {
        self.gaps
    }

    /// How many packets those gaps say were lost.
    ///
    /// Separate from [`SequenceTracker::gaps`] because one gap of 400 and 400 gaps of one are
    /// different links: the first is a dropout, the second is a station losing one packet in
    /// every few and needs a different thing looked at.
    #[must_use]
    pub const fn missing(&self) -> u64 {
        self.missing
    }

    /// Forgets every count, keeping the totals.
    ///
    /// Called when a source restarts — a file replay that loops, a listening socket that
    /// accepted a new peer — where the next count is unrelated to the last and comparing them
    /// would report a gap of some thousands.
    ///
    /// `gaps` and `missing` are cumulative for the life of the session, because the status bar
    /// reads them: a replay that looped four times would otherwise report the loss of the
    /// fourth lap as the loss of the whole pass.
    pub fn reset(&mut self) {
        self.last.fill(None);
        self.burst.fill(Burst::default());
    }
}

/// Turns raw packets into batches the store can take.
///
/// Not the XTCE decoder: that is [`xtce_decode::Decoder`], which borrows the definition and
/// is therefore passed in per call rather than held here. See [`crate::Session::start`] for
/// why nothing in this crate stores one beside an `Arc<XtceDb>`.
#[derive(Debug)]
pub struct PacketDecoder {
    clock: Option<SpacecraftClock>,
    sequence: SequenceTracker,
    stats: Arc<LinkStats>,
    events: Vec<Event>,
    /// Containers already warned about undescribed trailing bits, indexed by container.
    ///
    /// A definition that does not describe a whole packet does not describe *any* packet of
    /// that container, so the warning is one fact about the definition and not one fact per
    /// packet. Without this, a 7 200-packet replay puts 7 200 identical lines on a log that
    /// holds 2 048 — the event log collapses repeats, but only while nothing else is
    /// interleaved, and on a live station something always is.
    warned: Vec<bool>,
}

impl PacketDecoder {
    /// A decoder writing its counters to `stats`.
    #[must_use]
    pub fn new(clock: Option<SpacecraftClock>, stats: Arc<LinkStats>) -> Self {
        Self {
            clock,
            sequence: SequenceTracker::new(),
            stats,
            events: Vec::new(),
            warned: Vec::new(),
        }
    }

    /// The configured spacecraft clock, when there is one.
    #[must_use]
    pub fn clock(&self) -> Option<&SpacecraftClock> {
        self.clock.as_ref()
    }

    /// The per-APID sequence counts.
    #[must_use]
    pub const fn sequence(&self) -> &SequenceTracker {
        &self.sequence
    }

    /// The counters this decoder writes.
    #[must_use]
    pub const fn stats(&self) -> &Arc<LinkStats> {
        &self.stats
    }

    /// Decodes one packet into `batch`, reusing `decoded` as well as the batch.
    ///
    /// `buffer` comes from [`Decoder::new_packet`] and is reused across every packet that
    /// borrows the same `'p` — a drained `Vec<RawPacket>`, or a recording held whole in
    /// memory. That is the only case that can have this: [`Decoder::decode_into`] ties the
    /// buffer's lifetime to the bytes it last decoded, so a caller whose packets are each
    /// dropped before the next arrives cannot keep one at all, and reaches for
    /// [`PacketDecoder::decode_owned`] instead. The gain over `decode_owned` is the `Vec` and
    /// the hash table inside `buffer`, which a wide container regrows several times per
    /// packet when they are rebuilt.
    ///
    /// `batch` is overwritten in full, so a caller keeps one for the whole session and hands
    /// it to [`xtce_gs_core::ParameterStore::ingest`], which clones out only what it keeps.
    ///
    /// # What is counted and what is said
    ///
    /// This crate owns five [`LinkStats`] counters and writes all of them here:
    /// `packets_decoded`, `packets_rejected`, `sequence_gaps`, `sequence_missing` and
    /// `last_packet`. `last_packet` takes the *receipt* time and is touched for a refused
    /// packet too: the status bar answers "is anything arriving", which neither spacecraft
    /// time nor a successful decode is the question for.
    ///
    /// A rejection is counted here, before the `Err` is returned, and no event is queued for
    /// it — the caller holds the error and the packet and can say more about it than this can;
    /// [`error_kind`] is what makes a run's rejections summarisable. A sequence gap is the
    /// other way round: it is queued as an event here, because it is discovered here and
    /// nobody downstream can see it. Getting this backwards gives an operator either a silent
    /// loss or the same line twice.
    ///
    /// # Errors
    ///
    /// [`xtce_decode::DecodeError`] when the definition does not describe the packet, or
    /// describes it in a way the bytes do not satisfy. `batch` then holds whatever was
    /// decoded before the failure and must not be ingested.
    pub fn decode_into<'db, 'p>(
        &mut self,
        decoder: &Decoder<'db>,
        buffer: &mut DecodedPacket<'db, 'p>,
        packet: &'p RawPacket,
        batch: &mut Batch,
    ) -> Result<(), DecodeError> {
        let header = self.observe_header(packet);
        if let Err(error) = decoder.decode_into(buffer, packet.bytes.as_slice()) {
            self.stats.add(&self.stats.packets_rejected, 1);
            return Err(error);
        }
        self.fill(buffer, packet, header, batch);
        Ok(())
    }

    /// Decodes one owned packet into `batch`, reusing only the batch.
    ///
    /// The live path. A packet arrives, is decoded, and is dropped before the next one; no
    /// [`DecodedPacket`] can outlive it, so this calls [`Decoder::decode`] and accepts one
    /// `Vec` and one hash table per packet. What is still reused is `batch` and its
    /// `Vec<Sample>`, which is the larger of the two allocations on any container worth
    /// plotting. A caller that *does* hold all its packets at once should call
    /// [`PacketDecoder::decode_into`] instead and pay neither.
    ///
    /// Counters and events are exactly as [`PacketDecoder::decode_into`] describes them.
    ///
    /// # Errors
    ///
    /// [`xtce_decode::DecodeError`], as [`PacketDecoder::decode_into`].
    pub fn decode_owned(
        &mut self,
        decoder: &Decoder<'_>,
        packet: &RawPacket,
        batch: &mut Batch,
    ) -> Result<(), DecodeError> {
        let header = self.observe_header(packet);
        let buffer = match decoder.decode(packet.bytes.as_slice()) {
            Ok(buffer) => buffer,
            Err(error) => {
                self.stats.add(&self.stats.packets_rejected, 1);
                return Err(error);
            }
        };
        self.fill(&buffer, packet, header, batch);
        Ok(())
    }

    /// Lines about what the sequence counts said was lost, and about a definition that does
    /// not describe a whole packet. Drains the queue.
    #[must_use]
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Forgets every sequence count, for a source that restarted.
    ///
    /// Keeps the trailing-bit warnings: those are facts about the definition, and a replay
    /// that loops would otherwise repeat them once per lap.
    pub fn reset(&mut self) {
        self.sequence.reset();
    }

    /// Reads the primary header, files the count, and says what it cost.
    ///
    /// Both reads are `Option`, and `None` means a packet shorter than six bytes. The link
    /// does not produce one, so there is no separate path for it here: take zero for the two
    /// fields, file *nothing* with the sequence tracker — a fabricated APID 0 would put a
    /// false gap on a real channel — and let the decoder be what reports it, which on any
    /// definition with a primary header in its root container it will.
    // TODO(gs-engine-decode-headerless): the zero fields reach the store when a *definition*
    // has no primary header in its root container — `testdata/spp/test_xtce_4byte.xml` is one:
    // a single 32-bit parameter, so a five-byte packet decodes cleanly and the batch then
    // carries a fabricated APID 0 and sequence 0. Unreachable through `xtce-gs-link`, which
    // refuses a packet below seven bytes, and reachable by any caller of this method. Fixing
    // it needs a decision this module cannot make alone: `Batch::apid` and `Batch::sequence`
    // are plain `u16` in `xtce-gs-core`, which is final, so saying "no header" means either
    // making them `Option<u16>` there — every reader of a batch changes — or refusing such a
    // packet here, which would break the one definition shape that legitimately has no CCSDS
    // header at all.
    fn observe_header(&mut self, packet: &RawPacket) -> Header {
        self.stats.touch(packet.received.unix_nanos());
        let (Some(apid), Some(sequence)) = (packet.apid(), packet.sequence_count()) else {
            return Header::default();
        };
        let missed = self.sequence.observe(apid, sequence);
        if missed == 0 {
            // Still worth a call: a run of contiguous packets is what ends a dropout, and the
            // line that says a link recovered is the one an operator was waiting for.
            if let GapReport::Running {
                gaps,
                missing,
                recovered: true,
            } = self.sequence.note(apid, 0)
            {
                self.events.push(Event::at(
                    packet.received,
                    Severity::Info,
                    SOURCE,
                    format!(
                        "APID {apid}: contiguous again after {gaps} gap(s), \
                         {missing} packet(s) missing in all"
                    ),
                ));
            }
            return Header {
                apid,
                sequence,
                gap: false,
            };
        }
        self.stats.add(&self.stats.sequence_gaps, 1);
        self.stats
            .add(&self.stats.sequence_missing, u64::from(missed));
        // Not one line per gap: see `SequenceTracker::note`. The first gap of a dropout is
        // named with the sequence count, which is what says where it began; after that the
        // counters carry it and the log gets a running total every sixty-fourth gap.
        match self.sequence.note(apid, missed) {
            GapReport::Quiet => {}
            GapReport::First => self.events.push(Event::at(
                packet.received,
                Severity::Warning,
                SOURCE,
                format!("APID {apid}: {missed} packet(s) missing before sequence count {sequence}"),
            )),
            GapReport::Running { gaps, missing, .. } => self.events.push(Event::at(
                packet.received,
                Severity::Warning,
                SOURCE,
                format!("APID {apid}: {gaps} gaps so far, {missing} packet(s) missing"),
            )),
        }
        Header {
            apid,
            sequence,
            gap: true,
        }
    }

    /// Fills `batch` from a packet that decoded.
    fn fill(
        &mut self,
        decoded: &DecodedPacket<'_, '_>,
        packet: &RawPacket,
        header: Header,
        batch: &mut Batch,
    ) {
        let spacecraft = self.clock.and_then(|clock| clock.read(decoded));
        batch.container = decoded.container();
        batch.received = packet.received;
        batch.spacecraft = spacecraft;
        batch.apid = header.apid;
        batch.sequence = header.sequence;
        batch.sequence_gap = header.gap;
        // The same instant [`Batch::time`] would return, computed once for every sample.
        project(
            decoded,
            spacecraft.unwrap_or(packet.received),
            &mut batch.samples,
        );
        self.warn_trailing(decoded, packet.received);
        self.stats.add(&self.stats.packets_decoded, 1);
    }

    /// Warns once per container that the definition did not describe the whole packet.
    ///
    /// Not an error: `xtce-decode` and the reference implementation both decode such a packet
    /// and keep going, and a secondary header the definition omits is a normal reason for it.
    /// It is still worth surfacing, because the other reason is that the definition is a
    /// revision behind the spacecraft, and that one is silently wrong.
    fn warn_trailing(&mut self, decoded: &DecodedPacket<'_, '_>, received: Utc) {
        let trailing = decoded.trailing_bits();
        if trailing <= 0 {
            return;
        }
        let db = decoded.db();
        let container = decoded.container();
        if self.warned.len() < db.containers().len() {
            self.warned.resize(db.containers().len(), false);
        }
        let Some(seen) = self.warned.get_mut(container.index()) else {
            return;
        };
        if *seen {
            return;
        }
        *seen = true;
        let name = db
            .container(container)
            .map_or("?", |container| db.name(container.qualified_name));
        self.events.push(Event::at(
            received,
            Severity::Warning,
            SOURCE,
            format!("{name}: {trailing} bit(s) of the packet are not described by the definition"),
        ));
    }
}

/// What the primary header said, and what the sequence count cost.
#[derive(Clone, Copy, Debug, Default)]
struct Header {
    apid: u16,
    sequence: u16,
    gap: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use xtce_decode::PacketIter;
    use xtce_gs_core::{Value, ValueKind};
    use xtce_model::XtceDb;

    use super::*;

    /// A file under the workspace's `testdata/`, named relative to it.
    ///
    /// Reached from this crate's manifest rather than from the working directory: `cargo test
    /// -p` runs with the crate root as the CWD, and a relative path that assumed the
    /// workspace root would break under `cargo test` alone.
    fn testdata(name: &str) -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(name);
        assert!(path.exists(), "test data missing: {}", path.display());
        path
    }

    fn jpss_db() -> XtceDb {
        XtceDb::from_path(testdata("jpss/jpss1_geolocation_xtce_v1.xml"))
            .expect("the definition loads")
    }

    fn jpss_stream() -> Vec<u8> {
        std::fs::read(testdata("jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1")).expect("the pass")
    }

    /// Splits the recording into owned packets, a millisecond apart, as a link would.
    fn jpss_packets(stream: &[u8]) -> Vec<RawPacket> {
        let start = Utc::from_unix_secs(1_617_926_400); // 2021-04-09T00:00:00Z
        PacketIter::new(stream, 0)
            .enumerate()
            .map(|(index, framed)| {
                let framed = framed.expect("a clean concatenation of packets");
                RawPacket {
                    received: start.offset_nanos(index as i64 * 1_000_000),
                    bytes: framed.bytes().to_vec(),
                    vcid: None,
                }
            })
            .collect()
    }

    /// Splits any recording into owned packets, stamped at the epoch.
    fn owned_packets(stream: &[u8]) -> Vec<RawPacket> {
        PacketIter::new(stream, 0)
            .map(|framed| RawPacket {
                received: Utc::EPOCH,
                bytes: framed.expect("a clean stream").bytes().to_vec(),
                vcid: None,
            })
            .collect()
    }

    /// One parameter's sample out of a batch.
    fn sample_of(batch: &Batch, parameter: xtce_model::ParamId) -> &Sample {
        batch
            .samples
            .iter()
            .find(|sample| sample.parameter == parameter)
            .expect("the parameter is in the packet")
    }

    /// Gaps and missing packets, derived from *which packets were removed*.
    ///
    /// The point is that this shares no arithmetic with [`SequenceTracker::observe`]: no
    /// modulus, no subtraction of one count from another, no reading of a sequence count at
    /// all. It counts holes in a list. An earlier version of this helper retyped `observe`'s
    /// own `(current + 16_384 - previous - 1) % 16_384`, which could only ever prove that the
    /// same expression gives the same answer twice.
    ///
    /// `apids[i]` is the APID of position `i` in the *complete* stream and `removed` lists
    /// the positions taken out of it. Each APID is counted on its own, because CCSDS
    /// 133.0-B-2 §4.1.3.4 gives each APID its own count. Within one APID's own sub-stream a
    /// maximal run of consecutive removed positions is one gap that says the run's length is
    /// missing — except a run touching the start or the end of that sub-stream, which has no
    /// surviving packet on one side and so nothing that could notice it.
    ///
    /// Exact only while a per-APID run is shorter than a whole counter's worth: at 16 383 the
    /// tracker reports 16 382 and at 16 384 the repeated count is reported as nothing at all,
    /// where this would still report the run. Both call sites remove one to three in a row,
    /// which is what a link loses; a fixture that removed thousands would need the wrap rules
    /// here, and would be back to retyping `observe`.
    fn expected_from_removals(apids: &[u16], removed: &[usize]) -> (u64, u64) {
        let mut kept_by_apid: BTreeMap<u16, Vec<bool>> = BTreeMap::new();
        for (index, apid) in apids.iter().enumerate() {
            kept_by_apid
                .entry(*apid)
                .or_default()
                .push(!removed.contains(&index));
        }

        let (mut gaps, mut missing) = (0u64, 0u64);
        for kept in kept_by_apid.values() {
            let (Some(first), Some(last)) = (
                kept.iter().position(|keep| *keep),
                kept.iter().rposition(|keep| *keep),
            ) else {
                continue; // Every packet of this APID was removed: nothing arrived to notice.
            };
            let mut run = 0u64;
            for keep in &kept[first..=last] {
                if *keep {
                    if run > 0 {
                        gaps += 1;
                        missing += run;
                        run = 0;
                    }
                } else {
                    run += 1;
                }
            }
        }
        (gaps, missing)
    }

    /// A stream of `(apid, sequence count)` pairs, round-robin over `apids`.
    ///
    /// Each APID carries its own count, starting at its own offset and incrementing by one
    /// per packet of that APID, wrapping at 16 384 — which is all CCSDS 133.0-B-2 §4.1.3.4
    /// says the count does. The literal is written out rather than read from
    /// [`SEQUENCE_MODULUS`] on purpose: a fixture that reads the constant under test moves
    /// with it, and would hide a change to it instead of catching one.
    fn round_robin_counts(starts: &[(u16, u16)], per_apid: usize) -> Vec<(u16, u16)> {
        let mut stream = Vec::with_capacity(starts.len() * per_apid);
        for step in 0..per_apid {
            for (apid, start) in starts {
                let count = (u32::from(*start) + step as u32) % 16_384;
                stream.push((*apid, count as u16));
            }
        }
        stream
    }

    #[test]
    fn the_whole_jpss_pass_decodes_and_says_nothing_about_it() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);
        assert_eq!(packets.len(), 7200);

        let stats = Arc::new(LinkStats::new());
        let mut engine = PacketDecoder::new(None, Arc::clone(&stats));
        let mut batch = empty_batch();
        // One buffer for all 7 200 packets: they all borrow `packets`, which is the only
        // shape in which this is allowed.
        let mut buffer = decoder.new_packet(b"");
        let mut ok = 0usize;
        for packet in &packets {
            match engine.decode_into(&decoder, &mut buffer, packet, &mut batch) {
                Ok(()) => ok += 1,
                Err(error) => panic!("packet {ok} refused: {} ({error})", error_kind(&error)),
            }
        }

        assert_eq!(ok, 7200);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.packets_decoded, 7200);
        assert_eq!(snapshot.packets_rejected, 0);
        assert_eq!(snapshot.last_packet, Some(packets[7199].received));
        // 71 bytes is 568 bits and JPSS_ATT_EPHEM describes all 568 of them, so the pass is
        // clean: no gap, nothing undescribed, nothing to tell the operator.
        assert!(engine.take_events().is_empty());
    }

    #[test]
    fn the_owned_path_and_the_reusing_path_agree_packet_for_packet() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);

        let mut owned = PacketDecoder::new(None, Arc::new(LinkStats::new()));
        let mut reusing = PacketDecoder::new(None, Arc::new(LinkStats::new()));
        let mut left = empty_batch();
        let mut right = empty_batch();
        let mut buffer = decoder.new_packet(b"");

        for packet in packets.iter().take(64) {
            owned
                .decode_owned(&decoder, packet, &mut left)
                .expect("decodes");
            reusing
                .decode_into(&decoder, &mut buffer, packet, &mut right)
                .expect("decodes");
            assert_eq!(left.container, right.container);
            assert_eq!(left.apid, right.apid);
            assert_eq!(left.sequence, right.sequence);
            assert_eq!(left.samples.len(), right.samples.len());
            for (a, b) in left.samples.iter().zip(&right.samples) {
                assert_eq!(a.parameter, b.parameter);
                assert_eq!(a.raw, b.raw);
                assert_eq!(a.eng, b.eng);
                assert_eq!(a.time, b.time);
            }
        }
    }

    #[test]
    fn a_batch_carries_both_the_bits_and_what_they_mean() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);

        let mut engine = PacketDecoder::new(None, Arc::new(LinkStats::new()));
        let mut batch = empty_batch();
        engine
            .decode_owned(&decoder, &packets[0], &mut batch)
            .expect("the first packet decodes");

        assert_eq!(batch.samples.len(), 27);
        assert_eq!(batch.apid, 11);
        assert_eq!(batch.sequence, 2606);
        assert!(!batch.sequence_gap);
        assert_eq!(batch.spacecraft, None); // no clock configured
        assert_eq!(batch.time(), packets[0].received);
        assert!(batch.samples.iter().all(|s| s.time == batch.time()));

        // An integer field: both sides are the sixteen bits, because this definition declares
        // no calibrator — see `the_engineering_value_is_not_a_copy_of_the_raw_one` for the
        // case where the two sides differ, which JPSS cannot show.
        let doy = db.find_parameter("DOY").expect("DOY is declared");
        let sample = sample_of(&batch, doy);
        assert_eq!(sample.raw.kind(), ValueKind::Unsigned);
        assert_eq!(sample.eng, sample.raw);

        // And a float encoded as one, to show the raw side is not always an integer: a
        // projection that read `as_f64` off both sides would lose the variant here.
        let posx = db
            .find_parameter("ADGPSPOSX")
            .expect("ADGPSPOSX is declared");
        let sample = sample_of(&batch, posx);
        assert_eq!(sample.raw, Value::Float(6_389_695.5));
        assert_eq!(sample.eng, Value::Float(6_389_695.5));
    }

    #[test]
    fn the_engineering_value_is_not_a_copy_of_the_raw_one() {
        // No bundled *mission* definition has a calibrator or an enumeration — `SOURCES.md`
        // in the `xtce-rs` corpus says so outright — so on JPSS every sample's two sides are
        // equal, and a projection that filled `eng` from `raw` would pass every assertion in
        // this file. This definition has both, and its second packet is the golden one.
        let db = XtceDb::from_path(testdata("context_calibrators.xml")).expect("it loads");
        let decoder = Decoder::new(&db).expect("a default root");
        let stream = std::fs::read(testdata("context_calibrators_stream.bin")).expect("bytes");
        let packets = owned_packets(&stream);

        let stats = Arc::new(LinkStats::new());
        let mut engine = PacketDecoder::new(None, Arc::clone(&stats));
        let mut batch = empty_batch();

        // The first packet of this stream is deliberately of a type the definition does not
        // describe: the rejection path, with a real error rather than a fabricated one.
        let error = engine
            .decode_owned(&decoder, &packets[0], &mut batch)
            .expect_err("the first packet is not described");
        assert!(matches!(error, DecodeError::UnrecognizedPacket { .. }));
        assert_eq!(error_kind(&error), "packet type not described");
        assert_eq!(stats.snapshot().packets_rejected, 1);
        // Counted, not said: the caller holds the error and says more than this could.
        assert!(engine.take_events().is_empty());

        engine
            .decode_owned(&decoder, &packets[1], &mut batch)
            .expect("the second packet is a Payload");

        let mode = db.find_parameter("MODE").expect("MODE is declared");
        let sample = sample_of(&batch, mode);
        assert_eq!(sample.raw, Value::Unsigned(0));
        assert_eq!(sample.eng.kind(), ValueKind::Label);
        assert_eq!(sample.eng.as_str(), Some("IDLE"));

        // A context calibrator: the raw side is the counts, the engineering side is what the
        // polynomial made of them.
        let sensor = db.find_parameter("SENSOR").expect("SENSOR is declared");
        let sample = sample_of(&batch, sensor);
        assert_eq!(sample.raw, Value::Unsigned(8160));
        assert_eq!(sample.eng, Value::Float(8260.0));
    }

    #[test]
    fn the_real_pass_has_no_sequence_gaps_and_the_tracker_agrees() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);

        // What the corpus *is*, stated as data rather than recomputed: 7 200 packets of one
        // APID whose counts are the unbroken run 2 606..=9 805 — well short of the fourteen-
        // bit wrap, so no packet of it can be missing. That is what makes the zeros below
        // mean something; without it they would only say the tracker agreed with itself.
        let counts: Vec<u16> = packets
            .iter()
            .map(|packet| packet.sequence_count().expect("a primary header"))
            .collect();
        assert!(packets.iter().all(|packet| packet.apid() == Some(11)));
        assert_eq!(counts, (2606u16..=9805).collect::<Vec<u16>>());
        let (gaps, missing) = (0u64, 0u64);

        let stats = Arc::new(LinkStats::new());
        let mut engine = PacketDecoder::new(None, Arc::clone(&stats));
        let mut batch = empty_batch();
        let mut buffer = decoder.new_packet(b"");
        for packet in &packets {
            engine
                .decode_into(&decoder, &mut buffer, packet, &mut batch)
                .expect("decodes");
        }

        assert_eq!(engine.sequence().gaps(), gaps);
        assert_eq!(engine.sequence().missing(), missing);
        assert_eq!(stats.snapshot().sequence_gaps, gaps);
        assert_eq!(stats.snapshot().sequence_missing, missing);
        assert_eq!(engine.sequence().last_seen(11), Some(9805));
        assert_eq!(engine.sequence().last_seen(12), None);
    }

    #[test]
    fn packets_taken_out_of_the_pass_are_reported_as_exactly_that_many() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let all = jpss_packets(&stream);

        // A dropout of three, and a single loss much later: one counter cannot tell the two
        // apart, which is why there are two.
        let dropped = [1000usize, 1001, 1002, 4000];
        let kept: Vec<RawPacket> = all
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, packet)| packet.clone())
            .collect();
        assert_eq!(kept.len(), 7196);

        // Two holes in the list of positions — one three long, one one long — and the whole
        // pass is APID 11. Read off the removals, not off the counts.
        let (gaps, missing) = expected_from_removals(&vec![11u16; all.len()], &dropped);
        assert_eq!((gaps, missing), (2, 4));

        let stats = Arc::new(LinkStats::new());
        let mut engine = PacketDecoder::new(None, Arc::clone(&stats));
        let mut batch = empty_batch();
        let mut buffer = decoder.new_packet(b"");
        let mut flagged = 0usize;
        for packet in &kept {
            engine
                .decode_into(&decoder, &mut buffer, packet, &mut batch)
                .expect("decodes");
            if batch.sequence_gap {
                flagged += 1;
            }
        }

        assert_eq!(engine.sequence().gaps(), gaps);
        assert_eq!(engine.sequence().missing(), missing);
        assert_eq!(stats.snapshot().sequence_gaps, gaps);
        assert_eq!(stats.snapshot().sequence_missing, missing);
        assert_eq!(flagged, 2);
        // Four lines for two dropouts: each is named when it starts, with the sequence count
        // that says where, and again when the link has been contiguous long enough to call it
        // over. The counters above carry every gap; the log carries the shape of the pass.
        let events = engine.take_events();
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(events[0].message.contains("APID 11: 3 packet(s) missing"));
        assert_eq!(events[0].severity, Severity::Warning);
        assert!(
            events[1]
                .message
                .contains("contiguous again after 1 gap(s)")
        );
        assert_eq!(events[1].severity, Severity::Info);
        assert!(events[2].message.contains("APID 11: 1 packet(s) missing"));
        assert!(
            events[3]
                .message
                .contains("contiguous again after 1 gap(s)")
        );
    }

    #[test]
    fn a_link_losing_a_third_of_its_packets_does_not_fill_the_log() {
        // The failure this policy exists for: every gap names a different sequence count, so
        // `EventLog`'s collapsing cannot fold them, and one line per gap evicts everything
        // else from a bounded log. The counters keep all of it; the log gets the first line
        // of the dropout and a running total every sixty-fourth gap after that.
        let stats = Arc::new(LinkStats::new());
        let mut tracker = SequenceTracker::new();
        let mut lines = 0usize;
        let mut first = 0usize;
        for step in 0..600u32 {
            // Every third packet, so two of every three counts are a gap of one.
            let sequence = (step * 3) as u16;
            let missed = tracker.observe(11, sequence);
            match tracker.note(11, missed) {
                GapReport::Quiet => {}
                GapReport::First => {
                    first += 1;
                    lines += 1;
                }
                GapReport::Running { .. } => lines += 1,
            }
        }
        let _ = &stats;

        assert_eq!(first, 1, "the dropout is named once, not once per gap");
        assert!(
            lines <= 600 / 64 + 2,
            "{lines} lines for 599 gaps is not a rate limit"
        );
        assert_eq!(tracker.gaps(), 599, "and every gap is still counted");
    }

    #[test]
    fn sixteen_thousand_three_hundred_and_eighty_three_is_followed_by_zero() {
        let mut tracker = SequenceTracker::new();
        assert_eq!(tracker.observe(11, 16_383), 0); // first of this APID
        assert_eq!(tracker.observe(11, 0), 0); // the wrap, not a gap
        assert_eq!(tracker.observe(11, 1), 0);
        assert_eq!(tracker.gaps(), 0);
        assert_eq!(tracker.missing(), 0);
        // And the wrap still sees a gap across it when there is one.
        assert_eq!(tracker.observe(11, 3), 1);
        assert_eq!(tracker.observe(11, 16_383), 16_379);
        assert_eq!(tracker.observe(11, 2), 2);
        assert_eq!(tracker.gaps(), 3);
        assert_eq!(tracker.missing(), 16_382);
    }

    #[test]
    fn the_first_packet_of_an_apid_is_not_a_gap() {
        let mut tracker = SequenceTracker::new();
        assert_eq!(tracker.observe(1, 9000), 0);
        assert_eq!(tracker.observe(2, 3), 0);
        assert_eq!(tracker.observe(2047, 16_383), 0);
        assert_eq!(tracker.gaps(), 0);
        assert_eq!(tracker.last_seen(1), Some(9000));
        assert_eq!(tracker.last_seen(2047), Some(16_383));
    }

    #[test]
    fn a_repeated_count_is_not_sixteen_thousand_lost_packets() {
        let mut tracker = SequenceTracker::new();
        tracker.observe(11, 100);
        assert_eq!(tracker.observe(11, 100), 0);
        assert_eq!(tracker.gaps(), 0);
        assert_eq!(tracker.missing(), 0);
    }

    #[test]
    fn a_reset_forgets_the_counts_and_keeps_the_totals() {
        let mut tracker = SequenceTracker::new();
        tracker.observe(11, 10);
        assert_eq!(tracker.observe(11, 20), 9);
        tracker.reset();
        assert_eq!(tracker.last_seen(11), None);
        // The lap after a restart starts from nothing, so no gap of some thousands.
        assert_eq!(tracker.observe(11, 9000), 0);
        assert_eq!(tracker.gaps(), 1);
        assert_eq!(tracker.missing(), 9);
    }

    #[test]
    fn an_apid_is_tracked_on_its_own() {
        let mut tracker = SequenceTracker::new();
        tracker.observe(11, 1);
        tracker.observe(12, 500);
        assert_eq!(tracker.observe(11, 2), 0);
        assert_eq!(tracker.observe(12, 501), 0);
        assert_eq!(tracker.gaps(), 0);
    }

    #[test]
    fn three_interleaved_apids_report_only_their_own_removals() {
        // Three APIDs round-robin, each with its own count and its own starting offset, and
        // five packets taken out of the *stream* at known positions. The expectation is read
        // off those positions by `expected_from_removals`, which does no modular arithmetic
        // at all, so nothing here can agree with `observe` by sharing its formula.
        //
        // The offsets are the point. APID 11 starts at 16 380, so its fourth and fifth
        // packets are counts 16 383 and 0 — removing exactly those puts the gap *across* the
        // wrap, which is the only shape in which the modulus is load-bearing: below the wrap
        // `(current + M - previous - 1) % M` is just `current - previous - 1` for any M
        // larger than the step, and a wrong modulus gives the right answer. Interleaving
        // three APIDs is the other half: one shared slot, or an APID ignored, turns every
        // count here into a step of some thousands.
        let starts = [(11u16, 16_380u16), (12, 0), (700, 9_000)];
        let stream = round_robin_counts(&starts, 10);
        assert_eq!(stream.len(), 30);
        assert_eq!(stream[9], (11, 16_383));
        assert_eq!(stream[12], (11, 0)); // the wrap, one APID-11 packet later

        // Positions 9 and 12 straddle APID 11's wrap; 19 is one packet of APID 12; 2 and 29
        // are the first and the last packet of APID 700, where there is no surviving
        // neighbour on one side and so nothing a station could have noticed.
        let removed = [2usize, 9, 12, 19, 29];
        let apids: Vec<u16> = stream.iter().map(|(apid, _)| *apid).collect();
        let (gaps, missing) = expected_from_removals(&apids, &removed);
        assert_eq!((gaps, missing), (2, 3));

        let mut tracker = SequenceTracker::new();
        let mut reported = Vec::new();
        for (index, (apid, sequence)) in stream.iter().enumerate() {
            if removed.contains(&index) {
                continue;
            }
            let missed = tracker.observe(*apid, *sequence);
            if missed > 0 {
                reported.push((*apid, *sequence, missed));
            }
        }

        assert_eq!(tracker.gaps(), gaps);
        assert_eq!(tracker.missing(), missing);
        // And the gaps are reported against the right APID, at the right count, of the right
        // size: two across APID 11's wrap, one on APID 12, nothing on APID 700.
        assert_eq!(reported, vec![(11, 1, 2), (12, 7, 1)]);
        assert_eq!(tracker.last_seen(11), Some(5));
        assert_eq!(tracker.last_seen(12), Some(9));
        assert_eq!(tracker.last_seen(700), Some(9_008));
        // No slot but its own was touched — an APID folded onto another would show here.
        assert_eq!(tracker.last_seen(0), None);
        assert_eq!(tracker.last_seen(10), None);
        assert_eq!(tracker.last_seen(699), None);
    }

    #[test]
    fn an_apid_wider_than_eleven_bits_is_not_tracked_and_is_not_folded() {
        // The other half of `observe`'s out-of-range sentence. 2 048 is the first APID
        // eleven bits cannot name; it files nothing, and in particular it does not file
        // itself in APID 0's slot, which is what masking with `& 0x7FF` would do.
        let mut tracker = SequenceTracker::new();
        tracker.observe(0, 100);
        assert_eq!(tracker.observe(2_048, 9_000), 0);
        assert_eq!(tracker.observe(u16::MAX, 9_000), 0);
        assert_eq!(tracker.last_seen(2_048), None);
        // APID 0 still reads its own last count, and its next packet is continuous.
        assert_eq!(tracker.last_seen(0), Some(100));
        assert_eq!(tracker.observe(0, 101), 0);
        assert_eq!(tracker.gaps(), 0);
        assert_eq!(tracker.missing(), 0);
    }

    #[test]
    fn a_count_wider_than_fourteen_bits_is_reduced_not_wrapped_into_a_gap() {
        // `observe` is public and a caller could hand it anything; the count is fourteen bits
        // by definition, so 16 384 is 0 and the step from 16 383 to it is still continuous.
        let mut tracker = SequenceTracker::new();
        tracker.observe(11, 16_383);
        assert_eq!(tracker.observe(11, 16_384), 0);
        assert_eq!(tracker.last_seen(11), Some(0));
    }

    #[test]
    fn a_packet_too_short_to_have_a_header_files_nothing() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stats = Arc::new(LinkStats::new());
        let mut engine = PacketDecoder::new(None, Arc::clone(&stats));
        let mut batch = empty_batch();

        let stump = RawPacket::now(vec![0x08, 0x0B, 0xCA, 0x2E, 0x00]);
        let error = engine
            .decode_owned(&decoder, &stump, &mut batch)
            .expect_err("five bytes are not a packet");

        // Nothing was filed: a fabricated APID 0 would put a false gap on a real channel the
        // first time a real APID 0 packet arrived.
        assert_eq!(engine.sequence().last_seen(0), None);
        assert_eq!(engine.sequence().gaps(), 0);
        assert!(engine.take_events().is_empty());
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.packets_rejected, 1);
        assert_eq!(snapshot.packets_decoded, 0);
        // Still an arrival: the status bar's question is whether anything is coming in.
        assert_eq!(snapshot.last_packet, Some(stump.received));
        assert_eq!(error_kind(&error), "field past the end of the packet");
    }

    #[test]
    fn an_empty_packet_is_refused_rather_than_indexed() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let mut engine = PacketDecoder::new(None, Arc::new(LinkStats::new()));
        let mut batch = empty_batch();
        let empty = RawPacket::now(Vec::new());
        assert!(empty.header().is_none());
        assert!(engine.decode_owned(&decoder, &empty, &mut batch).is_err());
    }

    #[test]
    fn a_header_that_claims_more_bytes_than_it_has_does_not_read_past_them() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);

        // The header of a real packet, the declared length of a much larger one, and only
        // twenty bytes behind it. The decoder must stop at the buffer, not at the claim.
        let mut hostile = packets[0].clone();
        hostile.bytes.truncate(20);
        hostile.bytes[4] = 0xFF;
        hostile.bytes[5] = 0xFF;

        let mut engine = PacketDecoder::new(None, Arc::new(LinkStats::new()));
        let mut batch = empty_batch();
        let error = engine
            .decode_owned(&decoder, &hostile, &mut batch)
            .expect_err("the packet ends before the definition does");
        assert!(matches!(error, DecodeError::Bits { .. }));
        // The truncated packet still carried a readable APID and count, and those were filed:
        // it arrived, whatever the definition then made of it.
        assert_eq!(engine.sequence().last_seen(11), Some(2606));
    }

    #[test]
    fn a_definition_that_describes_only_part_of_a_packet_says_so_once_per_container() {
        // The contrived definition has a second concrete container, `UNUSED`, whose entry
        // list is three fields — so a 71-byte packet decoded against it leaves 504 bits
        // undescribed, and the same packet decoded from the default root leaves none until
        // bytes are added past what the definition claims. Every packet is decoded exactly
        // once, in stream order, so the sequence counts stay continuous and the only lines
        // on the log are the ones under test.
        let db = XtceDb::from_path(testdata("jpss/contrived_inheritance_structure.xml"))
            .expect("the definition loads");
        let stream = jpss_stream();
        let mut packets = jpss_packets(&stream);
        packets.truncate(9);

        let normal = Decoder::new(&db).expect("a default root");
        let partial = Decoder::with_root(&db, "UNUSED").expect("UNUSED is a root container");
        let mut engine = PacketDecoder::new(None, Arc::new(LinkStats::new()));
        let mut batch = empty_batch();

        // Four packets against a container that claims three fields of them: one warning.
        let mut containers = Vec::new();
        for packet in &packets[..4] {
            engine
                .decode_owned(&partial, packet, &mut batch)
                .expect("three fields fit in 71 bytes");
            containers.push(batch.container);
        }
        // Padding past the declared length: the bits are in the buffer and no entry claims
        // them, which is exactly what `trailing_bits` reports.
        for packet in &mut packets[4..] {
            packet.bytes.extend_from_slice(&[0u8; 4]);
        }
        // Four more against the real container: one more warning, not four.
        for packet in &packets[4..8] {
            engine
                .decode_owned(&normal, packet, &mut batch)
                .expect("decodes");
            containers.push(batch.container);
        }

        assert_eq!(containers[0], containers[3]);
        assert_ne!(containers[0], containers[4]);
        assert_eq!(containers[4], containers[7]);
        let events = engine.take_events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events[0].message.contains("UNUSED: 504 bit(s)"),
            "{events:?}"
        );
        assert!(
            events[1].message.contains("JPSS_ATT_EPHEM: 32 bit(s)"),
            "{events:?}"
        );
        assert!(events.iter().all(|e| e.severity == Severity::Warning));

        // And a ninth packet of a container already warned about adds nothing — the queue
        // was drained above, so anything here is a second line about the same fact.
        engine
            .decode_owned(&normal, &packets[8], &mut batch)
            .expect("decodes");
        assert!(engine.take_events().is_empty());
    }

    #[test]
    fn projecting_into_a_used_vector_leaves_nothing_of_the_last_packet() {
        let db = jpss_db();
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);
        let buffer = decoder
            .decode(packets[0].bytes.as_slice())
            .expect("decodes");

        let mut out = vec![
            Sample {
                parameter: buffer.values()[0].parameter,
                time: Utc::EPOCH,
                raw: Value::Unsigned(0),
                eng: Value::Unsigned(0),
            };
            200
        ];
        let capacity = out.capacity();
        let time = Utc::from_unix_secs(1_617_926_400);
        project(&buffer, time, &mut out);

        assert_eq!(out.len(), buffer.len());
        assert!(out.iter().all(|sample| sample.time == time));
        assert_eq!(out.capacity(), capacity, "the allocation is reused");
    }

    #[test]
    fn an_empty_decode_projects_to_nothing() {
        let db = jpss_db();
        // `UNUSED` with no bytes behind it: a container that claims nothing of a packet that
        // is nothing. The projection must empty the vector rather than leave the last one.
        let decoder = Decoder::new(&db).expect("one root container");
        let stream = jpss_stream();
        let packets = jpss_packets(&stream);
        let buffer = decoder
            .decode(packets[0].bytes.as_slice())
            .expect("decodes");
        let mut out = Vec::new();
        project(&buffer, Utc::EPOCH, &mut out);
        assert_eq!(out.len(), 27);
        let nothing = decoder.new_packet(b"");
        project(&nothing, Utc::EPOCH, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn every_decode_error_has_a_name_that_is_not_other() {
        // The tally label is what an operator reads at the end of an export that a definition
        // refused; a variant falling through to "other" makes two different faults look alike.
        let cases = [
            DecodeError::NoSuchContainer { name: "x".into() },
            DecodeError::AmbiguousRoot { candidates: 2 },
            DecodeError::UnrecognizedPacket {
                container: "x".into(),
                candidates: Vec::new(),
            },
            DecodeError::Unsupported {
                element: "x".into(),
                context: "y".into(),
            },
            DecodeError::DanglingIndex { what: "parameter" },
        ];
        for case in &cases {
            assert_ne!(error_kind(case), "other", "{case:?}");
        }
    }
}
