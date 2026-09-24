//! The composition: bytes in one end, packets out the other.
//!
//! Everything else in this crate is a piece that can be tested on its own. This is the piece
//! that knows the order they go in, which is the only thing about them that is not obvious
//! from their signatures and the only thing a mistake in is invisible — every stage still
//! reports success, and the packets are simply never there.

use std::fmt::Write as _;
use std::sync::Arc;

use xtce_gs_core::{Event, LinkStats, RawPacket, Severity, Utc};

use crate::csp::{CspHeader, CspVersion};
use crate::derand::derandomize;
use crate::error::LinkError;
use crate::frame::{
    FECF_BYTES, FrameError, FrameOptions, OCF_BYTES, PRIMARY_HEADER_BYTES, TmFrame,
};
use crate::packets::{PacketAssembler, PacketCounters, PacketStream, SPACE_PACKET_HEADER_BYTES};
use crate::rs::{CODEWORD_SYMBOLS, ReedSolomon, RsError};
use crate::sync::{CCSDS_ASM, DEFAULT_FLYWHEEL, SyncCounters, SyncState, Synchronizer};

/// The largest a CCSDS space packet can be: six header bytes and 65 536 of data field.
pub const DEFAULT_MAX_PACKET_LENGTH: usize = 65_542;

/// Parity symbols per codeword CCSDS defines: E=8 and E=16.
///
/// The authority is [`ReedSolomon::ccsds`], which is the thing that will refuse the pair.
/// Repeated here because `ccsds` returns `None` and [`PipelineConfig::validate`] owes the
/// operator a message naming the field and the value.
const ACCEPTED_PARITY: [usize; 2] = [16, 32];

/// Interleaving depths CCSDS 131.0-B-5 section 4 defines: I = 1, 2, 3, 4, 5, 8.
const ACCEPTED_INTERLEAVE: [usize; 6] = [1, 2, 3, 4, 5, 8];

/// Lines [`Pipeline`] holds between two calls to [`Pipeline::take_events`].
///
/// A stream of pure noise produces an event per frame. Repeats are collapsed before they are
/// formatted, so a link failing the same way costs one line however long it fails for; this
/// cap is what bounds a link failing a *different* way every frame, which collapsing cannot
/// help with. Lines past it are counted and the count is reported on the next drain.
const EVENT_QUEUE_CAPACITY: usize = 64;

/// Reed-Solomon parameters for a transfer frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RsConfig {
    /// How many codewords are interleaved across one frame.
    pub interleave: usize,
    /// Parity symbols per codeword: 32 for E=16, 16 for E=8.
    pub parity_symbols: usize,
}

impl Default for RsConfig {
    /// RS(255, 223) with no interleaving — the CCSDS default, and the one a mission that has
    /// not said otherwise almost certainly means.
    fn default() -> Self {
        Self {
            interleave: 1,
            parity_symbols: 32,
        }
    }
}

impl RsConfig {
    /// Symbol errors per codeword this configuration can correct.
    #[must_use]
    pub const fn correction_capacity(self) -> usize {
        self.parity_symbols / 2
    }

    /// Bytes of parity on the end of one frame, every codeword counted.
    #[must_use]
    pub const fn parity_bytes(self) -> usize {
        self.parity_symbols.saturating_mul(self.interleave)
    }

    /// The transfer frame length this code produces: the data half of the block.
    ///
    /// A frame of any other length makes [`ReedSolomon::decode`] return
    /// [`RsError::BadLength`] on every frame for the life of the session, which is why
    /// [`PipelineConfig::validate`] checks the configured length against this one.
    #[must_use]
    pub const fn frame_bytes(self) -> usize {
        CODEWORD_SYMBOLS
            .saturating_sub(self.parity_symbols)
            .saturating_mul(self.interleave)
    }
}

/// What the bytes coming off the source are.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Framing {
    /// CCSDS space packets, back to back in a stream or one per datagram.
    #[default]
    Packets,
    /// CCSDS 132.0-B TM transfer frames.
    TmFrames {
        /// The transfer frame, ASM and RS parity excluded.
        frame_length: usize,
        /// Whether each frame is preceded by the attached sync marker.
        attached_sync_marker: bool,
        /// Whether to search the seven intermediate bit offsets as well as the byte ones.
        ///
        /// Costs eight times the comparisons while searching and nothing once locked. It is
        /// what makes the difference on a link whose bit and byte clocks are recovered
        /// separately, and it is dead weight on a recording.
        bit_slip: bool,
        /// Consecutive missed markers tolerated before lock is given up.
        flywheel: u32,
        /// The Reed-Solomon parameters, when the frames are coded.
        reed_solomon: Option<RsConfig>,
        /// Whether the pseudo-randomiser was applied.
        derandomize: bool,
        /// Whether the frame ends with a frame error control field.
        has_fecf: bool,
        /// Whether the frame carries an operational control field.
        has_ocf: bool,
        /// Bytes of insert zone after the primary header.
        insert_zone: usize,
    },
}

impl Framing {
    /// TM transfer frames of `frame_length` octets, with the defaults a bare link has.
    ///
    /// An attached sync marker, no bit-slip search, [`DEFAULT_FLYWHEEL`], no coding and no
    /// trailers. Every other field is set by the caller, because a constructor with nine
    /// boolean arguments is a constructor nobody reads correctly.
    #[must_use]
    pub const fn tm_frames(frame_length: usize) -> Self {
        Self::TmFrames {
            frame_length,
            attached_sync_marker: true,
            bit_slip: false,
            flywheel: DEFAULT_FLYWHEEL,
            reed_solomon: None,
            derandomize: false,
            has_fecf: false,
            has_ocf: false,
            insert_zone: 0,
        }
    }

    /// The frame layout, when this is frame framing.
    ///
    /// The three layout flags live in this enum for the operator's sake and in
    /// [`FrameOptions`] for the parser's; deriving one from the other here is what keeps them
    /// from drifting apart in two call sites.
    #[must_use]
    pub const fn frame_options(&self) -> Option<FrameOptions> {
        match self {
            Self::Packets => None,
            Self::TmFrames {
                has_fecf,
                has_ocf,
                insert_zone,
                ..
            } => Some(FrameOptions {
                has_fecf: *has_fecf,
                has_ocf: *has_ocf,
                insert_zone: *insert_zone,
            }),
        }
    }

    /// The transfer frame length, when this is frame framing.
    ///
    /// The frame, not the codeword. See [`PipelineConfig::codeword_length`].
    #[must_use]
    pub const fn frame_length(&self) -> Option<usize> {
        match self {
            Self::Packets => None,
            Self::TmFrames { frame_length, .. } => Some(*frame_length),
        }
    }
}

/// How a session's bytes are to be taken apart.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PipelineConfig {
    /// What the bytes are.
    pub framing: Framing,
    /// The CSP header wrapped around each packet, when there is one.
    pub csp: Option<CspVersion>,
    /// A packet longer than this is a desync, not a packet.
    pub max_packet_length: usize,
}

impl Default for PipelineConfig {
    /// Space packets, no CSP, and the largest packet CCSDS can express.
    fn default() -> Self {
        Self {
            framing: Framing::Packets,
            csp: None,
            max_packet_length: DEFAULT_MAX_PACKET_LENGTH,
        }
    }
}

impl PipelineConfig {
    /// Bytes between one sync marker and the next, marker excluded.
    ///
    /// **The frame length and the codeword length are not the same number.** `frame_length`
    /// is the transfer frame — what comes out of the Reed-Solomon decoder — and the markers
    /// on the wire are `frame_length + parity_symbols * interleave` apart. A synchroniser
    /// given the frame length on a coded link locks on the first marker, cuts short, and
    /// never finds another one where it expects it; with a long enough flywheel it will look
    /// locked while producing nothing but rubbish. This is the only place that arithmetic is
    /// written, and [`Synchronizer::new`] is documented to want its result.
    #[must_use]
    pub const fn codeword_length(&self) -> Option<usize> {
        match &self.framing {
            Framing::Packets => None,
            Framing::TmFrames {
                frame_length,
                reed_solomon,
                ..
            } => Some(match reed_solomon {
                Some(rs) => frame_length.saturating_add(rs.parity_bytes()),
                None => *frame_length,
            }),
        }
    }

    /// Checks the configuration before a session is built on it.
    ///
    /// The engine calls this at start-up, where a mistake can still be shown to the operator
    /// as a refused session. [`Pipeline::new`] cannot report anything — it returns a
    /// `Pipeline` — so this is the only place a bad configuration is a failure rather than a
    /// degraded link.
    ///
    /// # Errors
    ///
    /// [`LinkError::Config`] naming the field and the value that cannot be used.
    pub fn validate(&self) -> Result<(), LinkError> {
        if self.max_packet_length < SPACE_PACKET_HEADER_BYTES + 1 {
            return Err(LinkError::Config(format!(
                "max_packet_length is {}: a CCSDS space packet is a six-octet primary header \
                 and at least one octet of data field (CCSDS 133.0-B-2 section 4.1.3.5.3), so \
                 nothing below {} can ever be a packet",
                self.max_packet_length,
                SPACE_PACKET_HEADER_BYTES + 1
            )));
        }

        let Framing::TmFrames {
            frame_length,
            reed_solomon,
            has_fecf,
            has_ocf,
            insert_zone,
            ..
        } = &self.framing
        else {
            // Under `Framing::Packets` the frame flags do not exist to be wrong. CSP is the
            // only thing left to check, and packet framing is the framing CSP needs.
            return Ok(());
        };

        if self.csp.is_some() {
            return Err(LinkError::Config(
                "csp is set together with framing = TM frames: CSP and CCSDS transfer frames \
                 are two link layers doing the same job, a mission runs one of them, and \
                 accepting both would mean choosing silently which one the bytes are in"
                    .to_owned(),
            ));
        }

        if *frame_length == 0 {
            return Err(LinkError::Config(
                "frame_length is 0: a transfer frame is a fixed-length container and the \
                 synchroniser has nothing to cut at"
                    .to_owned(),
            ));
        }

        // CCSDS 132.0-B-3 section 4.1: primary header, then the insert zone, then the data
        // field, then the optional trailers. What is left over is what packets can live in.
        let overhead = PRIMARY_HEADER_BYTES
            .saturating_add(*insert_zone)
            .saturating_add(if *has_ocf { OCF_BYTES } else { 0 })
            .saturating_add(if *has_fecf { FECF_BYTES } else { 0 });
        if overhead >= *frame_length {
            return Err(LinkError::Config(format!(
                "a {frame_length}-octet frame with insert_zone = {insert_zone}, has_ocf = \
                 {has_ocf} and has_fecf = {has_fecf} is {overhead} octets of header and \
                 trailer: no data field is left for a packet to be in"
            )));
        }

        let Some(rs) = reed_solomon else {
            return Ok(());
        };

        if !ACCEPTED_PARITY.contains(&rs.parity_symbols) {
            return Err(LinkError::Config(format!(
                "reed_solomon.parity_symbols is {}: CCSDS 131.0-B-5 section 4 defines 16 (E=8) \
                 and 32 (E=16), and nothing else",
                rs.parity_symbols
            )));
        }
        if !ACCEPTED_INTERLEAVE.contains(&rs.interleave) {
            return Err(LinkError::Config(format!(
                "reed_solomon.interleave is {}: CCSDS 131.0-B-5 section 4 defines I = 1, 2, 3, \
                 4, 5 and 8",
                rs.interleave
            )));
        }
        // Without this the link looks plausible and produces nothing: `ReedSolomon::decode`
        // hands back `BadLength` for every frame, for ever, and the only symptom is a
        // dropped-frame count equal to the seen-frame count.
        if *frame_length != rs.frame_bytes() {
            return Err(LinkError::Config(format!(
                "frame_length is {frame_length}, but RS({}, {}) interleaved {} deep produces a \
                 {}-octet transfer frame; give --frame-length {} or change the interleave",
                CODEWORD_SYMBOLS,
                CODEWORD_SYMBOLS - rs.parity_symbols,
                rs.interleave,
                rs.frame_bytes(),
                rs.frame_bytes()
            )));
        }

        // TODO(gs-link-pipeline): shortened Reed-Solomon codeblocks — CCSDS 131.0-B-5 section
        // 4.2's virtual fill — are refused by the length check above, because `ReedSolomon`
        // has no shortening parameter to check against. A mission that transmits them needs
        // an explicit `virtual_fill: usize` on `RsConfig`, threaded into `ReedSolomon::ccsds`
        // and subtracted from both `data_length` and `block_length`. Nothing in reach sends
        // them, so the decision is which of the two ways round the fill is counted — from the
        // start of the information block, per the Blue Book — and that is all it needs.
        Ok(())
    }
}

/// Which line of the log a message is, so a repeat can be recognised before it is formatted.
///
/// A discriminator and not the message itself: the message costs a `format!` and an
/// allocation, and on a noise stream that allocation happens once per frame. Collapsing has
/// to happen *before* the formatting or it is not collapsing, it is tidying up afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reason {
    /// A configuration the pipeline could not carry out and degraded around.
    Config,
    /// The synchroniser found a frame boundary.
    SyncLocked,
    /// The synchroniser gave up a frame boundary.
    SyncLost,
    /// Lock was found at a non-zero bit offset.
    SyncSlip,
    /// Bytes thrown away while searching for a marker.
    SyncDiscard,
    /// Reed-Solomon repaired a block.
    RsCorrected,
    /// Reed-Solomon could not repair a block.
    RsUncorrectable,
    /// Reed-Solomon was handed a block of the wrong length.
    RsBadLength,
    /// A frame's error control field did not match.
    FrameChecksum,
    /// A frame's header could not be read as the configured layout.
    FrameHeader,
    /// A virtual channel carries access service data rather than packets.
    FrameVca,
    /// Packets were lost or bytes discarded by a packet stage.
    PacketLoss,
    /// A CSP header could not be read.
    Csp,
}

/// A bounded, repeat-collapsing queue of lines for the operator.
///
/// The event log downstream collapses repeats too, but it cannot help here: the cost being
/// avoided is the `format!` that builds the message, which happens on this side of the queue.
/// A run of identical lines is therefore reported as one line with the count appended, which
/// means the log's own repeat count undercounts — the trade is deliberate, and the number the
/// operator reads is the one in the message.
#[derive(Debug)]
struct Events {
    queued: Vec<Event>,
    last: Option<(Severity, &'static str, Reason)>,
    repeats: u64,
    dropped: u64,
}

impl Events {
    fn new() -> Self {
        Self {
            queued: Vec::new(),
            last: None,
            repeats: 0,
            dropped: 0,
        }
    }

    /// Queues a line, unless it repeats the last one or the queue is full.
    ///
    /// `message` is a closure and not a `String` because the whole point is that it is not
    /// built on the path that is taken thousands of times a second.
    fn push<F: FnOnce() -> String>(
        &mut self,
        severity: Severity,
        source: &'static str,
        reason: Reason,
        message: F,
    ) {
        if self.last == Some((severity, source, reason)) {
            self.repeats = self.repeats.saturating_add(1);
            return;
        }
        self.close_run();
        if self.queued.len() >= EVENT_QUEUE_CAPACITY {
            // `last` is cleared so that repeats of a dropped line cannot later be appended to
            // whichever line happens to be at the end of a full queue.
            self.last = None;
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.last = Some((severity, source, reason));
        self.queued
            .push(Event::at(Utc::now(), severity, source, message()));
    }

    /// Takes lines a sub-stage formatted for itself, subject to the same cap.
    ///
    /// These arrive already built, so there is nothing to save by collapsing them and no
    /// discriminator to collapse them on; identical consecutive messages are still merged,
    /// because a virtual channel losing every frame produces one per frame.
    fn absorb(&mut self, lines: Vec<Event>) {
        for line in lines {
            if self
                .queued
                .last()
                .is_some_and(|held| held.message == line.message && held.source == line.source)
            {
                self.repeats = self.repeats.saturating_add(1);
                continue;
            }
            self.close_run();
            if self.queued.len() >= EVENT_QUEUE_CAPACITY {
                self.last = None;
                self.dropped = self.dropped.saturating_add(1);
                continue;
            }
            self.last = None;
            self.queued.push(line);
        }
    }

    /// Writes a finished run's repeat count onto the line it repeated.
    fn close_run(&mut self) {
        if self.repeats == 0 {
            return;
        }
        let repeats = self.repeats;
        self.repeats = 0;
        if let Some(last) = self.queued.last_mut() {
            // Writing into the existing `String` rather than building a second one. The
            // result is discarded because a `fmt::Write` into a `String` cannot fail, and
            // this crate does not panic on a live downlink.
            let _ = write!(last.message, " (and {repeats} more like it)");
        }
    }

    /// Every line held, and a final line if any were dropped.
    fn drain(&mut self) -> Vec<Event> {
        self.close_run();
        let mut lines = std::mem::take(&mut self.queued);
        self.last = None;
        if self.dropped > 0 {
            lines.push(Event::warning(
                "link",
                format!(
                    "{} further event(s) not shown: the link queues at most \
                     {EVENT_QUEUE_CAPACITY} lines between drains",
                    self.dropped
                ),
            ));
            self.dropped = 0;
        }
        lines
    }
}

/// Bytes in, packets out.
///
/// Owns whichever of the stages the configuration asked for and nothing else: a session
/// reading space packets allocates no synchroniser and no Reed-Solomon tables.
#[derive(Debug)]
pub struct Pipeline {
    config: PipelineConfig,
    stats: Arc<LinkStats>,
    sync: Option<Synchronizer>,
    rs: Option<ReedSolomon>,
    assembler: PacketAssembler,
    stream: PacketStream,
    events: Events,
    /// Whether the next bytes begin a message, and so carry a CSP header of their own.
    ///
    /// Set by [`Pipeline::flush_message`] and true from construction, because the first bytes
    /// a pipeline ever sees begin a message by definition.
    message_start: bool,
    /// Virtual channels already reported as carrying access service data rather than packets.
    ///
    /// One bit per channel — a TM virtual channel identifier is three bits, so there are
    /// eight — because the alternative is a line per frame at the frame rate for a channel
    /// that is behaving exactly as its spacecraft intends.
    vca_reported: u8,
}

impl Pipeline {
    /// Builds the pipeline the configuration describes.
    ///
    /// Infallible by contract. A configuration that cannot be carried out becomes an event
    /// rather than an error — see [`PipelineConfig::validate`] for where it should have been
    /// caught.
    #[must_use]
    pub fn new(config: PipelineConfig, stats: Arc<LinkStats>) -> Self {
        let mut events = Events::new();
        let mut sync = None;
        let mut rs = None;

        if let Framing::TmFrames {
            attached_sync_marker,
            bit_slip,
            flywheel,
            reed_solomon,
            ..
        } = &config.framing
        {
            // The synchroniser is given the *codeword* length, which is the frame plus the
            // parity: that is the marker-to-marker distance on the wire. See
            // `PipelineConfig::codeword_length`, which is the one place that sum is written.
            let codeword = config.codeword_length().unwrap_or_default();
            let marker = if *attached_sync_marker {
                CCSDS_ASM.to_vec()
            } else {
                Vec::new()
            };
            let mut synchronizer = Synchronizer::new(codeword, marker, *bit_slip);
            synchronizer.set_flywheel(*flywheel);
            sync = Some(synchronizer);

            if let Some(configured) = reed_solomon {
                let codec = ReedSolomon::ccsds(configured.parity_symbols, configured.interleave);
                if codec.is_none() {
                    // Degrade rather than produce nothing: an uncorrected pass still shows
                    // telemetry on a clean link, and this line is why it should not be
                    // trusted. `PipelineConfig::validate` is what stops it getting here.
                    let (parity, interleave) = (configured.parity_symbols, configured.interleave);
                    events.push(Severity::Error, "link", Reason::Config, || {
                        format!(
                            "Reed-Solomon with {parity} parity symbols interleaved \
                             {interleave} deep is not a code CCSDS 131.0-B-5 section 4 \
                             defines; frames are passed through uncorrected and every \
                             symbol error will show up as a frame checksum failure"
                        )
                    });
                }
                rs = codec;
            }
        }

        let max_packet_length = config.max_packet_length;
        Self {
            config,
            stats,
            sync,
            rs,
            assembler: PacketAssembler::new(max_packet_length),
            stream: PacketStream::new(max_packet_length),
            events,
            message_start: true,
            vca_reported: 0,
        }
    }

    /// The configuration this was built from.
    #[must_use]
    pub const fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// The counters this pipeline writes.
    #[must_use]
    pub const fn stats(&self) -> &Arc<LinkStats> {
        &self.stats
    }

    /// Whether the synchroniser has a frame boundary, when there is a synchroniser.
    #[must_use]
    pub fn sync_state(&self) -> Option<SyncState> {
        self.sync.as_ref().map(Synchronizer::state)
    }

    /// The Reed-Solomon codec, when the frames are coded and the configuration was usable.
    #[must_use]
    pub const fn reed_solomon(&self) -> Option<&ReedSolomon> {
        self.rs.as_ref()
    }

    /// The frame-to-packet assembler, for its per-channel state.
    #[must_use]
    pub const fn assembler(&self) -> &PacketAssembler {
        &self.assembler
    }

    /// The packet stream splitter, used when the framing is [`Framing::Packets`].
    #[must_use]
    pub const fn stream(&self) -> &PacketStream {
        &self.stream
    }

    /// Feeds bytes in, appends whatever packets came out.
    ///
    /// Never blocks and never allocates per byte. `received` is the instant the bytes arrived
    /// at the ground and is stamped onto every packet they complete. The only allocation on a
    /// clean pass is the [`RawPacket`] each finished packet becomes: every buffer between the
    /// socket and that packet is a field of this struct or of a stage it owns.
    ///
    /// # The order of the coding stages, and why it is this way round
    ///
    /// The stages run
    ///
    /// ```text
    /// synchronise -> derandomise -> Reed-Solomon decode -> parse the frame -> assemble
    /// ```
    ///
    /// because the transmit side applies the pseudo-randomiser to the *codeblock*, after
    /// Reed-Solomon encoding and with the attached sync marker excluded — CCSDS 131.0-B-5
    /// §10.2 — so undoing the outer transform first means derandomising before decoding.
    /// The marker is outside both, which is what lets the synchroniser find a frame boundary
    /// in a stream it has not yet decoded or derandomised.
    ///
    /// §10 gives the reason in the same breath: the Reed-Solomon codes *by themselves cannot
    /// guarantee sufficient bit transitions to keep receiver symbol synchronizers in lock*.
    /// An all-zero transfer frame has all-zero check symbols, so a randomiser that stopped at
    /// the frame would leave 256 bits of the codeblock without a transition in them.
    ///
    /// An operator who gets this backwards sees a link that finds sync — the marker is
    /// untouched either way — and then fails every frame afterwards with a checksum failure
    /// or a nonsensical header. Note what they do *not* see: Reed-Solomon reports no errors.
    /// The randomiser sequence is itself a valid codeword and the code is linear, so a
    /// codeblock that has not been derandomised is still a codeword and decodes cleanly to
    /// the wrong data — see
    /// `tests::the_randomiser_sequence_is_a_codeword_so_the_wrong_order_decodes_cleanly_and_lies`.
    /// That is why `rs_uncorrectable` and `crc_failures` are separate counters rather than
    /// one "bad frame" number: with the stages swapped, only the second one moves.
    ///
    pub fn push(&mut self, bytes: &[u8], received: Utc, out: &mut Vec<RawPacket>) {
        let Self {
            config,
            stats,
            sync,
            rs,
            assembler,
            stream,
            events,
            message_start,
            vca_reported,
        } = self;
        let stats: &LinkStats = stats;
        let config: &PipelineConfig = config;

        stats.add(&stats.bytes_in, bytes.len() as u64);
        if bytes.is_empty() {
            return;
        }

        // CSP is stripped before either packet stage sees a byte. The header sits *outside*
        // the space packet, so at offset 0 of a wrapped message there is a CSP header and not
        // a primary header, and a packet stage reading a length field out of one desynchronises
        // on the first message. `CspHeader::parse` hands back a borrow of the payload, so this
        // costs a subslice and not a copy.
        let payload = match (config.csp, *message_start) {
            (Some(version), true) => {
                *message_start = false;
                match CspHeader::parse(bytes, version) {
                    Ok((_, payload)) => payload,
                    Err(err) => {
                        let dropped = bytes.len();
                        events.push(Severity::Warning, "link", Reason::Csp, || {
                            format!("CSP: {err}; {dropped} octets of the message dropped")
                        });
                        return;
                    }
                }
            }
            _ => bytes,
        };

        match &config.framing {
            Framing::Packets => stream.push(payload, received, out),
            Framing::TmFrames {
                frame_length,
                derandomize: randomised,
                ..
            } => {
                let Some(sync) = sync.as_mut() else {
                    return;
                };
                let mut stage = FrameStage {
                    sync,
                    rs: rs.as_ref(),
                    assembler,
                    stats,
                    events,
                    options: config.framing.frame_options().unwrap_or_default(),
                    frame_length: *frame_length,
                    randomised: *randomised,
                    vca_reported,
                };
                stage.run(payload, received, out);
                fold_sync(stage.sync.take_counters(), stats, events);
                fold_packets(assembler.take_counters(), stats, events);
                events.absorb(assembler.take_events());
            }
        }

        fold_packets(stream.take_counters(), stats, events);
        events.absorb(stream.take_events());
    }

    /// Tells the caller what happened since the last call. Drains the queue.
    #[must_use]
    pub fn take_events(&mut self) -> Vec<Event> {
        self.events.drain()
    }

    /// A datagram source restarts framing at each message boundary.
    ///
    /// Also called when a listening TCP source accepts a new peer: in both cases the bytes on
    /// either side of the boundary are not one frame and not one packet.
    ///
    /// A boundary is not free. It is where a half-assembled packet is thrown away, and a
    /// flush that reported nothing would hide a wrong MTU for the life of a mission — so each
    /// stage is reset and then *drained*, because none of them clears its own counters on
    /// reset.
    pub fn flush_message(&mut self) {
        let Self {
            stats,
            sync,
            assembler,
            stream,
            events,
            message_start,
            ..
        } = self;
        let stats: &LinkStats = stats;

        if let Some(sync) = sync.as_mut() {
            sync.reset();
            fold_sync(sync.take_counters(), stats, events);
        }
        assembler.reset();
        fold_packets(assembler.take_counters(), stats, events);
        events.absorb(assembler.take_events());
        stream.flush();
        fold_packets(stream.take_counters(), stats, events);
        events.absorb(stream.take_events());

        *message_start = true;
    }
}

/// The transfer-frame half of [`Pipeline::push`], with each field borrowed separately.
///
/// A struct and not seven arguments, and borrowed field by field rather than through
/// `&mut Pipeline`, because the frame handed out by [`Synchronizer::next_frame`] borrows the
/// synchroniser's own buffer for as long as it is being parsed — so the assembler, the
/// counters and the event queue have to be reachable while that borrow is live.
struct FrameStage<'a> {
    sync: &'a mut Synchronizer,
    rs: Option<&'a ReedSolomon>,
    assembler: &'a mut PacketAssembler,
    stats: &'a LinkStats,
    events: &'a mut Events,
    options: FrameOptions,
    frame_length: usize,
    randomised: bool,
    vca_reported: &'a mut u8,
}

impl FrameStage<'_> {
    fn run(&mut self, payload: &[u8], received: Utc, out: &mut Vec<RawPacket>) {
        let Self {
            sync,
            rs,
            assembler,
            stats,
            events,
            options,
            frame_length,
            randomised,
            vca_reported,
        } = self;

        sync.push(payload);
        while let Some(codeword) = sync.next_frame() {
            stats.add(&stats.frames_seen, 1);

            // ---- the coding order; see `Pipeline::push` for why it is this way round ----
            // The randomiser sits outside the coding layer, so it comes off the whole
            // codeblock — parity included — before Reed-Solomon sees a symbol. Derandomising
            // only the frame, or doing it after the decode, hands the decoder a codeword that
            // is not one and every block reads as uncorrectable.
            if *randomised {
                derandomize(codeword);
            }

            if let Some(codec) = *rs {
                match codec.decode(codeword) {
                    Ok(0) => {}
                    Ok(symbols) => {
                        stats.add(&stats.rs_corrected, 1);
                        stats.add(&stats.rs_symbols, symbols as u64);
                        let capacity = codec.correction_capacity();
                        events.push(Severity::Info, "frame", Reason::RsCorrected, || {
                            format!(
                                "Reed-Solomon repaired {symbols} symbols in a block \
                                 (capacity {capacity} per codeword)"
                            )
                        });
                    }
                    Err(RsError::Uncorrectable) => {
                        stats.add(&stats.rs_uncorrectable, 1);
                        stats.add(&stats.frames_dropped, 1);
                        events.push(Severity::Warning, "frame", Reason::RsUncorrectable, || {
                            String::from(
                                "Reed-Solomon could not repair a block; the frame is dropped \
                                 rather than guessed at, because past its capacity the decoder \
                                 can land on a different valid codeword",
                            )
                        });
                        continue;
                    }
                    Err(err @ RsError::BadLength { .. }) => {
                        stats.add(&stats.frames_dropped, 1);
                        events.push(Severity::Error, "frame", Reason::RsBadLength, || {
                            format!(
                                "{err}: the configured frame length and the Reed-Solomon \
                                 parameters do not describe the same codeblock"
                            )
                        });
                        continue;
                    }
                }
            }

            // The configured frame length, not `ReedSolomon::data_length`: the two are the
            // same number by `PipelineConfig::validate`, and if they are not it is the
            // operator's number that `TmFrame::parse` has to be checked against. The clamp
            // covers the uncoded case, where the codeword *is* the frame.
            let frame_length = (*frame_length).min(codeword.len());
            // ---- end of the coding order ----

            match TmFrame::parse(&codeword[..frame_length], *options) {
                Ok(frame) => {
                    stats.add(&stats.frames_ok, 1);

                    // CCSDS 132.0-B-3 §4.1.2.7.2: the sync flag says the data field carries
                    // virtual channel access service data and not packets, and §4.1.2.7.6.2
                    // then leaves the first header pointer undefined — so it must not be
                    // read as an offset and the data field must not be taken apart. The
                    // frame still advances the virtual channel count, so it is recorded;
                    // skipping it would manufacture a gap on the next packet frame.
                    if frame.sync_flag() {
                        let vcid = frame.vcid();
                        assembler.note_frame(vcid, frame.virtual_frame_count());
                        let bit = 1u8 << (vcid & 0x07);
                        if **vca_reported & bit == 0 {
                            **vca_reported |= bit;
                            events.push(Severity::Info, "frame", Reason::FrameVca, || {
                                format!(
                                    "virtual channel {vcid} carries access service data, not \
                                     packets; its frames are counted and not taken apart"
                                )
                            });
                        }
                        continue;
                    }

                    if frame.is_idle() {
                        stats.add(&stats.idle_frames, 1);
                    }
                    // Including an idle one. `PacketAssembler::push_data` reads
                    // `ONLY_IDLE_DATA` as "end the frame here and leave the partial packet
                    // alone", which is the same outcome as skipping it — except that it
                    // records the virtual channel frame count first. Skipping the call
                    // instead makes the next real frame's count look like a jump, so a
                    // channel that idles at all reports one manufactured gap and one lost
                    // packet per idle frame, and abandons a packet that legitimately spans
                    // one. CCSDS 132.0-B-3 §4.1.4: an idle frame is a frame the spacecraft
                    // transmitted, and it advances the count like any other.
                    assembler.push_frame(frame, received, out);
                }
                Err(FrameError::BadChecksum { expected, found }) => {
                    stats.add(&stats.crc_failures, 1);
                    stats.add(&stats.frames_dropped, 1);
                    events.push(Severity::Warning, "frame", Reason::FrameChecksum, || {
                        format!("frame checksum is {found:#06x}, computed {expected:#06x}")
                    });
                }
                Err(err) => {
                    stats.add(&stats.frames_dropped, 1);
                    events.push(Severity::Warning, "frame", Reason::FrameHeader, || {
                        format!("frame refused: {err}")
                    });
                }
            }
        }
    }
}

/// Folds the synchroniser's deltas into the cumulative counters and the log.
///
/// `SyncCounters::slips` and `SyncCounters::bytes_discarded` have no counter on
/// [`LinkStats`], which is a fixed type this crate does not own — so they reach the operator
/// as events or not at all.
fn fold_sync(counters: SyncCounters, stats: &LinkStats, events: &mut Events) {
    if counters == SyncCounters::default() {
        return;
    }
    stats.add(&stats.sync_found, counters.markers_found);
    stats.add(&stats.sync_lost, counters.losses);

    if counters.locks > 0 {
        let locks = counters.locks;
        events.push(Severity::Info, "link", Reason::SyncLocked, || {
            format!("frame synchronisation acquired ({locks}x)")
        });
    }
    if counters.losses > 0 {
        let losses = counters.losses;
        events.push(Severity::Warning, "link", Reason::SyncLost, || {
            format!("frame synchronisation lost ({losses}x)")
        });
    }
    if counters.slips > 0 {
        let slips = counters.slips;
        events.push(Severity::Warning, "link", Reason::SyncSlip, || {
            format!(
                "lock acquired at a non-zero bit offset {slips}x: the receiver's byte clock \
                 is not aligned with the frame, which is a configuration problem rather than \
                 a noise problem"
            )
        });
    }
    if counters.bytes_discarded > 0 {
        let bytes = counters.bytes_discarded;
        events.push(Severity::Info, "link", Reason::SyncDiscard, || {
            format!("{bytes} octets discarded while searching for a sync marker")
        });
    }
}

/// Folds a packet stage's deltas into the cumulative counters and the log.
fn fold_packets(counters: PacketCounters, stats: &LinkStats, events: &mut Events) {
    if counters == PacketCounters::default() {
        return;
    }
    stats.add(&stats.packets_in, counters.packets);
    stats.add(&stats.packets_lost, counters.lost);
    stats.add(&stats.idle_packets, counters.idle);

    if counters.lost > 0 || counters.discarded_bytes > 0 {
        let (lost, bytes) = (counters.lost, counters.discarded_bytes);
        events.push(Severity::Warning, "link", Reason::PacketLoss, || {
            format!("{lost} partial packet(s) abandoned, {bytes} octets discarded")
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::derand::randomize;
    use crate::frame::{NO_PACKET_START, ONLY_IDLE_DATA, crc16_ccitt};
    use crate::packets::IDLE_APID;

    /// The one check on the coding order that does not go through the fixture — and the
    /// reason nothing else in this module could have caught the order being wrong.
    ///
    /// CCSDS 131.0-B-5 §10: *on the sending end, the **Codeblock** or Transfer Frame is
    /// randomized*, and on the receiving end, *after locating the ASM in the received data
    /// stream, the pseudo-random sequence is exclusive-ORed with the data bits immediately
    /// following the ASM*. So the randomiser covers the Reed-Solomon check symbols, and a
    /// receiver takes it off the whole codeblock before it decodes.
    ///
    /// What makes getting this backwards dangerous rather than merely wrong: the 255-octet
    /// randomiser sequence is **itself a valid RS(255,223) codeword**, which the first
    /// assertion below establishes with the encoder — the half of `rs.rs` that is verified
    /// octet for octet against Phil Karn's. A Reed-Solomon code is linear, so a codeword
    /// exclusive-ORed with the sequence is another codeword. A receiver that decodes before
    /// it derandomises therefore gets `Ok(0)` — *no errors* — hands on a frame of rubbish,
    /// and reports a clean link while doing it. The failure surfaces two stages later as a
    /// frame checksum failure, or as a header that makes no sense, which is why the event log
    /// distinguishes those from `rs_uncorrectable`.
    #[test]
    fn the_randomiser_sequence_is_a_codeword_so_the_wrong_order_decodes_cleanly_and_lies() {
        let codec = ReedSolomon::ccsds(32, 1).expect("RS(255,223), I=1");

        // 1. The sequence is a codeword. Asked of the encoder, not of the decoder.
        let sequence = crate::derand::sequence();
        let mut reencoded = Vec::new();
        codec
            .encode(&sequence[..codec.data_length()], &mut reencoded)
            .expect("encode");
        assert_eq!(
            &reencoded[codec.data_length()..],
            &sequence[codec.data_length()..],
            "the randomiser sequence is no longer a codeword; the rest of this test, and the \
             warning in its doc comment, need rewriting"
        );

        // 2. So a still-randomised codeblock decodes as error-free, and is not the frame.
        let frame: Vec<u8> = (0..codec.data_length())
            .map(|i| ((i * 31 + 7) % 251) as u8)
            .collect();
        let mut wire = Vec::new();
        codec.encode(&frame, &mut wire).expect("encode");
        randomize(&mut wire);

        let mut decoded_too_early = wire.clone();
        assert_eq!(
            codec.decode(&mut decoded_too_early),
            Ok(0),
            "Reed-Solomon cannot see this mistake — that is the point"
        );
        assert_ne!(
            &decoded_too_early[..codec.data_length()],
            &frame[..],
            "and what it hands on is not the frame that was sent"
        );

        // 3. The order the standard fixes recovers the frame exactly.
        let mut received = wire;
        derandomize(&mut received);
        assert_eq!(codec.decode(&mut received), Ok(0));
        assert_eq!(&received[..codec.data_length()], &frame[..]);
    }

    /// A JPSS recording of back-to-back space packets, in the sibling `xtce-rs` checkout.
    ///
    // In this repository, not in a sibling checkout: the fixture below is a real pass, and the
    // whole of this module's framing is asserted against the packets in it. Provenance is in
    // `testdata/SOURCES.md`.
    const JPSS: &str = "../../testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1";

    /// Spacecraft identifier stamped into the fixture's frames. NOAA-20 is 157.
    const SCID: u16 = 157;
    /// Virtual channel the fixture transmits on.
    const VCID: u8 = 0;
    /// Transfer frame length when the fixture is uncoded.
    const PLAIN_FRAME_LENGTH: usize = 223;

    /// A TM framing with the fields the configuration tests vary, and the rest at their
    /// defaults. Enum variants take no functional-record-update, so this is the shorthand.
    fn tm(
        frame_length: usize,
        rs: Option<RsConfig>,
        has_fecf: bool,
        insert_zone: usize,
    ) -> Framing {
        Framing::TmFrames {
            frame_length,
            attached_sync_marker: true,
            bit_slip: false,
            flywheel: DEFAULT_FLYWHEEL,
            reed_solomon: rs,
            derandomize: false,
            has_fecf,
            has_ocf: false,
            insert_zone,
        }
    }

    fn received() -> Utc {
        Utc::from_unix_secs(1_600_000_000)
    }

    /// The first `count` packets of the JPSS recording.
    ///
    /// Panics if the recording is not there, and that is the point. This used to fall back to
    /// packets made up on the spot, which kept every test below green while it quietly stopped
    /// testing the thing it is named after — a real pass, with the field widths and the
    /// sequence counts a real spacecraft produced. The recording is vendored in this
    /// repository now, so its absence means somebody deleted it, and a deleted fixture should
    /// cost a red test rather than a silent change of subject.
    fn packets(count: usize) -> Vec<Vec<u8>> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(JPSS);
        let bytes = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "{}: {error}. It is vendored in this repository; see testdata/SOURCES.md",
                path.display()
            )
        });
        let mut out = Vec::with_capacity(count);
        let mut at = 0usize;
        while out.len() < count {
            let Some(len) = crate::packets::declared_length(&bytes[at..]) else {
                break;
            };
            if at + len > bytes.len() {
                break;
            }
            out.push(bytes[at..at + len].to_vec());
            at += len;
        }
        assert_eq!(out.len(), count, "the recording ran out of packets");
        out
    }

    /// Packets of deliberately uneven length, so no assertion can pass by arithmetic luck.
    fn synthetic_packets(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| {
                let len = SPACE_PACKET_HEADER_BYTES + 1 + i % 37;
                let mut packet = vec![0u8; len];
                packet[0..2].copy_from_slice(&(0x0800u16 | 0x0b).to_be_bytes());
                packet[2..4].copy_from_slice(&(0xC000u16 | (i as u16 & 0x3FFF)).to_be_bytes());
                packet[4..6]
                    .copy_from_slice(&((len - SPACE_PACKET_HEADER_BYTES - 1) as u16).to_be_bytes());
                for (k, byte) in packet[SPACE_PACKET_HEADER_BYTES..].iter_mut().enumerate() {
                    *byte = (i.wrapping_mul(31).wrapping_add(k)) as u8;
                }
                packet
            })
            .collect()
    }

    /// An idle packet of exactly `len` octets, used to pad the last frame's data field.
    fn idle_packet(len: usize) -> Vec<u8> {
        let mut packet = vec![0u8; len];
        packet[0..2].copy_from_slice(&(0x0800u16 | IDLE_APID).to_be_bytes());
        packet[2..4].copy_from_slice(&0xC000u16.to_be_bytes());
        packet[4..6].copy_from_slice(&((len - SPACE_PACKET_HEADER_BYTES - 1) as u16).to_be_bytes());
        packet
    }

    /// A byte stream built the way a spacecraft builds one, with what should come back out.
    struct Fixture {
        /// The bytes as they lie on the wire.
        wire: Vec<u8>,
        /// Octets of one whole codeblock, sync marker excluded.
        block_length: usize,
        /// Transfer frames in `wire`.
        frames: u64,
        /// Idle packets used as padding, which the assembler must drop.
        idle: u64,
        /// The packets that went in, in order, which must come back out identical.
        expected: Vec<Vec<u8>>,
        /// The configuration that reads `wire`.
        config: PipelineConfig,
    }

    /// Wraps packets in transfer frames the way CCSDS 132.0-B-3 section 4.1 lays them out.
    ///
    /// The coding order here is the inverse of [`Pipeline::push`]'s and has to be flipped with
    /// it: randomise the transfer frame, then Reed-Solomon encode the randomised frame, then
    /// put the marker in front of the block. A fixture that mirrors the pipeline cannot decide
    /// which order is right — it passes either way round — which is exactly why the argument
    /// is written out in `Pipeline::push` rather than left to this test.
    fn fixture(count: usize, rs: Option<RsConfig>, scramble: bool, asm: bool) -> Fixture {
        let expected = packets(count);
        let frame_length = rs.map_or(PLAIN_FRAME_LENGTH, RsConfig::frame_bytes);
        let data_field = frame_length - PRIMARY_HEADER_BYTES - FECF_BYTES;

        // Lay the packets end to end, remembering where each one starts, then pad with idle
        // packets until the last frame's data field is exactly full. Padding with anything
        // else would hand the assembler bytes it has to read as a packet header.
        let mut stream = Vec::new();
        let mut starts = Vec::new();
        for packet in &expected {
            starts.push(stream.len());
            stream.extend_from_slice(packet);
        }
        let mut idle = 0u64;
        while !stream.len().is_multiple_of(data_field) {
            let short = data_field - stream.len() % data_field;
            let len = if short > SPACE_PACKET_HEADER_BYTES {
                short
            } else {
                short + data_field
            };
            starts.push(stream.len());
            stream.extend_from_slice(&idle_packet(len));
            idle += 1;
        }

        let frames = stream.len() / data_field;
        let codec = rs.and_then(|c| ReedSolomon::ccsds(c.parity_symbols, c.interleave));
        assert_eq!(rs.is_some(), codec.is_some(), "the fixture needs the codec");
        let mut wire = Vec::new();
        let mut next_start = 0usize;
        for index in 0..frames {
            let base = index * data_field;
            while next_start < starts.len() && starts[next_start] < base {
                next_start += 1;
            }
            let pointer = match starts.get(next_start) {
                Some(&start) if start < base + data_field => (start - base) as u16,
                _ => NO_PACKET_START,
            };

            let mut frame = Vec::with_capacity(frame_length);
            frame.extend_from_slice(&((SCID << 4) | (u16::from(VCID) << 1)).to_be_bytes());
            frame.push(index as u8);
            frame.push(index as u8);
            // No secondary header, no VCA service, segment length identifier 0b11 for packets.
            frame.extend_from_slice(&((0b11u16 << 11) | pointer).to_be_bytes());
            frame.extend_from_slice(&stream[base..base + data_field]);
            let crc = crc16_ccitt(&frame);
            frame.extend_from_slice(&crc.to_be_bytes());
            assert_eq!(frame.len(), frame_length);

            // The transmit order CCSDS 131.0-B-5 §10.2 fixes: encode, then randomise the
            // whole codeblock. Building the fixture the other way round would make it agree
            // with a receiver that had the same misreading, and agree with nothing else.
            let mut block = match codec.as_ref() {
                Some(codec) => {
                    let mut block = Vec::new();
                    codec.encode(&frame, &mut block).expect("encode");
                    block
                }
                None => frame,
            };
            if scramble {
                randomize(&mut block);
            }
            if asm {
                wire.extend_from_slice(&CCSDS_ASM);
            }
            wire.extend_from_slice(&block);
        }
        let block_length = codec
            .as_ref()
            .map_or(frame_length, ReedSolomon::block_length);
        // One more marker on the end. A synchroniser is entitled to confirm the next marker
        // before it hands out the frame in front of it, and a fixture that stopped at the
        // last octet would make the last frame's fate an implementation detail of `sync`.
        if asm {
            wire.extend_from_slice(&CCSDS_ASM);
        }

        Fixture {
            wire,
            block_length,
            frames: frames as u64,
            idle,
            expected,
            config: PipelineConfig {
                framing: Framing::TmFrames {
                    frame_length,
                    attached_sync_marker: asm,
                    bit_slip: false,
                    flywheel: DEFAULT_FLYWHEEL,
                    reed_solomon: rs,
                    derandomize: scramble,
                    has_fecf: true,
                    has_ocf: false,
                    insert_zone: 0,
                },
                csp: None,
                max_packet_length: DEFAULT_MAX_PACKET_LENGTH,
            },
        }
    }

    /// Offset of one codeblock in `wire`, marker excluded.
    fn block_at(fixture: &Fixture, index: usize) -> usize {
        index * (CCSDS_ASM.len() + fixture.block_length) + CCSDS_ASM.len()
    }

    fn run(config: &PipelineConfig, wire: &[u8], chunk: usize) -> (Pipeline, Vec<RawPacket>) {
        let mut pipeline = Pipeline::new(config.clone(), Arc::new(LinkStats::new()));
        let mut out = Vec::new();
        for piece in wire.chunks(chunk) {
            pipeline.push(piece, received(), &mut out);
        }
        (pipeline, out)
    }

    // ---- the whole chain ----------------------------------------------------------------

    #[test]
    fn the_packets_that_went_in_come_back_out_whatever_the_chunk_size_was() {
        let fixture = fixture(200, Some(RsConfig::default()), true, true);
        assert!(fixture.frames > 1, "the fixture must span several frames");

        // One byte at a time is the case that breaks a streaming parser: every marker, every
        // length field and every frame boundary falls across a call.
        for chunk in [1usize, 7, 65_536] {
            let (pipeline, out) = run(&fixture.config, &fixture.wire, chunk);

            assert_eq!(out.len(), fixture.expected.len(), "chunk {chunk}");
            for (index, (got, want)) in out.iter().zip(&fixture.expected).enumerate() {
                assert_eq!(&got.bytes, want, "packet {index} at chunk {chunk}");
                assert_eq!(got.vcid, Some(VCID), "packet {index} at chunk {chunk}");
                assert_eq!(got.received, received(), "packet {index} at chunk {chunk}");
            }

            let snapshot = pipeline.stats().snapshot();
            assert_eq!(
                snapshot.bytes_in,
                fixture.wire.len() as u64,
                "chunk {chunk}"
            );
            assert_eq!(snapshot.frames_seen, fixture.frames, "chunk {chunk}");
            assert_eq!(snapshot.frames_ok, fixture.frames, "chunk {chunk}");
            assert_eq!(snapshot.frames_dropped, 0, "chunk {chunk}");
            assert_eq!(snapshot.crc_failures, 0, "chunk {chunk}");
            assert_eq!(snapshot.rs_uncorrectable, 0, "chunk {chunk}");
            assert_eq!(snapshot.rs_corrected, 0, "chunk {chunk}");
            assert_eq!(snapshot.rs_symbols, 0, "chunk {chunk}");
            assert_eq!(snapshot.sync_lost, 0, "chunk {chunk}");
            assert_eq!(snapshot.idle_frames, 0, "chunk {chunk}");
            assert_eq!(snapshot.idle_packets, fixture.idle, "chunk {chunk}");
            assert_eq!(
                snapshot.packets_in,
                fixture.expected.len() as u64,
                "chunk {chunk}"
            );
            assert_eq!(snapshot.packets_lost, 0, "chunk {chunk}");
            assert_eq!(
                pipeline.sync_state(),
                Some(SyncState::Locked),
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn an_uncoded_link_is_the_same_packets_at_every_chunk_size() {
        // The same claim as the test above, with the channel coding taken out: whatever the
        // chain does to the bytes, the chunk boundaries must not be visible in the result.
        let fixture = fixture(64, None, false, true);
        for chunk in [1usize, 7, 65_536] {
            let (pipeline, out) = run(&fixture.config, &fixture.wire, chunk);
            assert_eq!(out.len(), fixture.expected.len(), "chunk {chunk}");
            for (index, (got, want)) in out.iter().zip(&fixture.expected).enumerate() {
                assert_eq!(&got.bytes, want, "packet {index} at chunk {chunk}");
                assert_eq!(got.vcid, Some(VCID), "packet {index} at chunk {chunk}");
            }
            let snapshot = pipeline.stats().snapshot();
            assert_eq!(
                snapshot.bytes_in,
                fixture.wire.len() as u64,
                "chunk {chunk}"
            );
            assert_eq!(snapshot.frames_seen, fixture.frames, "chunk {chunk}");
            assert_eq!(snapshot.frames_ok, fixture.frames, "chunk {chunk}");
            assert_eq!(snapshot.frames_dropped, 0, "chunk {chunk}");
            assert_eq!(snapshot.crc_failures, 0, "chunk {chunk}");
            assert_eq!(snapshot.sync_lost, 0, "chunk {chunk}");
            assert_eq!(snapshot.idle_packets, fixture.idle, "chunk {chunk}");
            assert_eq!(
                snapshot.packets_in,
                fixture.expected.len() as u64,
                "chunk {chunk}"
            );
            assert_eq!(snapshot.packets_lost, 0, "chunk {chunk}");
            assert!(pipeline.reed_solomon().is_none());
        }
    }

    #[test]
    fn frames_with_no_marker_are_cut_by_length_alone() {
        let fixture = fixture(64, None, false, false);
        let (pipeline, out) = run(&fixture.config, &fixture.wire, 512);
        assert_eq!(out.len(), fixture.expected.len());
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.frames_ok, fixture.frames);
        assert_eq!(snapshot.sync_found, 0, "there is no marker to find");
    }

    #[test]
    fn a_burst_inside_the_correction_capacity_is_repaired_and_counted() {
        let rs = RsConfig::default();
        let mut fixture = fixture(64, Some(rs), true, true);
        let at = block_at(&fixture, 1);
        for byte in &mut fixture.wire[at + 10..at + 10 + rs.correction_capacity()] {
            *byte ^= 0xFF;
        }

        let (pipeline, out) = run(&fixture.config, &fixture.wire, 1);
        assert_eq!(out.len(), fixture.expected.len(), "nothing should be lost");
        assert!(
            out.iter()
                .zip(&fixture.expected)
                .all(|(g, w)| &g.bytes == w)
        );
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.rs_corrected, 1);
        assert_eq!(snapshot.rs_symbols, rs.correction_capacity() as u64);
        assert_eq!(snapshot.rs_uncorrectable, 0);
        assert_eq!(snapshot.frames_dropped, 0);
        assert_eq!(snapshot.crc_failures, 0);
    }

    #[test]
    fn a_burst_beyond_the_correction_capacity_drops_the_frame_rather_than_guessing() {
        let rs = RsConfig::default();
        let mut fixture = fixture(64, Some(rs), true, true);
        let at = block_at(&fixture, 1);
        for byte in &mut fixture.wire[at + 10..at + 10 + rs.correction_capacity() + 4] {
            *byte ^= 0xFF;
        }

        let (mut pipeline, out) = run(&fixture.config, &fixture.wire, 4096);
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.rs_uncorrectable, 1);
        assert_eq!(snapshot.frames_dropped, 1);
        assert_eq!(snapshot.frames_seen, fixture.frames);
        assert_eq!(snapshot.frames_ok, fixture.frames - 1);
        assert!(
            out.len() < fixture.expected.len(),
            "a dropped frame has to cost packets, or it was not carrying any"
        );
        assert!(
            snapshot.packets_lost >= 1,
            "the frame-count gap has to abandon the packet that spanned it"
        );
        let events = pipeline.take_events();
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("Reed-Solomon")),
            "the operator has to be told which stage refused the frame: {events:?}"
        );
    }

    #[test]
    fn a_frame_whose_checksum_does_not_match_is_counted_as_a_checksum_failure() {
        // No Reed-Solomon, so a flipped octet reaches the frame parser untouched and the
        // frame error control field is the thing that catches it.
        let mut fixture = fixture(64, None, false, true);
        let at = block_at(&fixture, 1);
        fixture.wire[at + 20] ^= 0x01;

        let (mut pipeline, _out) = run(&fixture.config, &fixture.wire, 4096);
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.crc_failures, 1);
        assert_eq!(snapshot.frames_dropped, 1);
        assert_eq!(snapshot.rs_uncorrectable, 0, "nothing coded this link");
        let events = pipeline.take_events();
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("checksum"))
        );
    }

    #[test]
    fn a_stream_cut_off_mid_frame_yields_no_packet_from_the_half_of_one() {
        // Framing, not coding: the half frame has to be held, not invented.
        let fixture = fixture(64, None, false, true);
        let truncated = &fixture.wire[..fixture.wire.len() - fixture.block_length / 2];
        let (pipeline, out) = run(&fixture.config, truncated, 1);
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.frames_seen, fixture.frames - 1);
        assert_eq!(snapshot.frames_ok, fixture.frames - 1);
        assert!(out.len() < fixture.expected.len());
        // Whatever is left is held, not invented.
        assert_eq!(snapshot.packets_in, out.len() as u64);
    }

    #[test]
    fn an_empty_push_moves_nothing() {
        let fixture = fixture(8, None, false, true);
        let (mut pipeline, mut out) = run(&fixture.config, &[], 1);
        pipeline.push(&[], received(), &mut out);
        assert!(out.is_empty());
        assert_eq!(pipeline.stats().snapshot().bytes_in, 0);
        assert!(pipeline.take_events().is_empty());
    }

    #[test]
    fn a_link_that_fails_every_frame_the_same_way_costs_one_line_not_one_per_frame() {
        // Markers where they should be, rubbish behind every one of them: the shape a link
        // takes when the receiver is locked to a stream it is misreading. Without collapsing,
        // this is one `format!` and one queued line per frame, for as long as the pass lasts.
        let mut fixture = fixture(64, None, false, true);
        for index in 0..fixture.frames as usize {
            let at = block_at(&fixture, index) + 20;
            fixture.wire[at] ^= 0xFF;
        }

        let whole = fixture.wire.len();
        let (mut pipeline, out) = run(&fixture.config, &fixture.wire, whole);
        assert!(out.is_empty(), "not one frame should have survived");
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.crc_failures, fixture.frames);
        assert_eq!(snapshot.frames_dropped, fixture.frames);
        assert_eq!(snapshot.frames_ok, 0);

        let events = pipeline.take_events();
        assert!(
            events.len() <= 3,
            "{} frames became {} lines: {events:?}",
            fixture.frames,
            events.len()
        );
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("more like it")),
            "the repeat count is the number the operator actually reads: {events:?}"
        );
    }

    #[test]
    fn an_octet_lost_off_the_wire_costs_lock_and_the_link_says_which() {
        // Deleting one octet shifts every marker after it by one. The flywheel absorbs four
        // misses — that is what it is for — and the fifth gives up lock; the synchroniser
        // then finds the stream again at its new offset.
        let mut fixture = fixture(64, None, false, true);
        assert!(
            fixture.frames > 10,
            "the flywheel needs frames to run through"
        );
        let at = block_at(&fixture, 3);
        fixture.wire.remove(at);

        let (pipeline, out) = run(&fixture.config, &fixture.wire, 1);
        let snapshot = pipeline.stats().snapshot();
        assert!(snapshot.sync_found > 1, "{snapshot:?}");
        assert_eq!(snapshot.sync_lost, 1, "{snapshot:?}");
        assert!(snapshot.frames_dropped > 0, "{snapshot:?}");
        assert!(!out.is_empty(), "the frames before the gap were good");
        assert!(
            out.len() < fixture.expected.len(),
            "losing lock has to cost something, or nothing was lost"
        );
        assert_eq!(pipeline.sync_state(), Some(SyncState::Locked));
    }

    #[test]
    fn a_virtual_channel_access_frame_is_counted_and_not_taken_apart() {
        /// Bit 14 of the transfer frame data field status: CCSDS 132.0-B-3 §4.1.2.7.2.
        const SYNC_FLAG: u16 = 1 << 14;

        // CCSDS 132.0-B-3 §4.1.2.7.2: the sync flag says the data field is not packets, and
        // §4.1.2.7.6.2 leaves the first header pointer undefined when it is set — so the
        // pointer below is deliberately hostile. Read as an offset it would splice 200 octets
        // of somebody else's service data into the packet in flight.
        let frame_length = PLAIN_FRAME_LENGTH;
        let data_field = frame_length - PRIMARY_HEADER_BYTES - FECF_BYTES;

        let mut packet = vec![0u8; 300];
        packet[0] = 0x08;
        packet[1] = 11;
        packet[2] = 0xC0;
        packet[3] = 2;
        packet[4..6].copy_from_slice(&(300u16 - 7).to_be_bytes());
        for (index, byte) in packet.iter_mut().enumerate().skip(6) {
            *byte = (index % 251) as u8;
        }
        let mut idle = vec![0u8; 130];
        idle[0] = 0x08 | ((IDLE_APID >> 8) as u8);
        idle[1] = (IDLE_APID & 0xFF) as u8;
        idle[2] = 0xC0;
        idle[4..6].copy_from_slice(&(130u16 - 7).to_be_bytes());

        let frame = |count: u8, status: u16, data: &[u8]| {
            let mut frame = Vec::with_capacity(frame_length);
            frame.extend_from_slice(&((SCID << 4) | (u16::from(VCID) << 1)).to_be_bytes());
            frame.push(count);
            frame.push(count);
            frame.extend_from_slice(&status.to_be_bytes());
            let mut field = data.to_vec();
            field.resize(data_field, 0xA5);
            frame.extend_from_slice(&field);
            let crc = crc16_ccitt(&frame);
            frame.extend_from_slice(&crc.to_be_bytes());
            frame
        };

        let mut tail = packet[215..].to_vec();
        tail.extend_from_slice(&idle);

        let mut wire = Vec::new();
        for built in [
            frame(0, 0b11u16 << 11, &packet[..215]),
            // The access service frame: sync flag set, pointer 200, data that is not packets.
            frame(1, SYNC_FLAG | (0b11u16 << 11) | 0x00c8, &[0x5A; 64]),
            frame(2, (0b11u16 << 11) | (300 - 215) as u16, &tail),
        ] {
            wire.extend_from_slice(&CCSDS_ASM);
            wire.extend_from_slice(&built);
        }
        wire.extend_from_slice(&CCSDS_ASM);

        let config = PipelineConfig {
            framing: tm(frame_length, None, true, 0),
            ..PipelineConfig::default()
        };
        let (pipeline, out) = run(&config, &wire, 7);
        let snapshot = pipeline.stats().snapshot();

        assert_eq!(
            snapshot.frames_ok, 3,
            "an access service frame is a good frame"
        );
        assert_eq!(
            snapshot.packets_lost, 0,
            "the access service frame was read as packets or as a break in the count"
        );
        assert_eq!(out.len(), 1, "the packet that spanned it did not survive");
        assert_eq!(out[0].bytes, packet);
    }

    #[test]
    fn a_packet_that_spans_an_idle_frame_survives_it() {
        // The failure this covers is invisible to every test that calls `PacketAssembler`
        // directly: the assembler records an idle frame's virtual channel frame count, and
        // for a while `Pipeline::push` skipped the call entirely. The next real frame then
        // looked like a jump, so a channel that idles — which is what a channel does when it
        // has nothing to send — reported one manufactured gap and one lost packet per idle
        // frame, and threw away the packet that spanned it.
        let frame_length = PLAIN_FRAME_LENGTH;
        let data_field = frame_length - PRIMARY_HEADER_BYTES - FECF_BYTES;

        // One 300-octet packet, then a 130-octet idle packet as the tail padding of the
        // frame that finishes it: 215 octets of the first frame, 85 + 130 of the third.
        let mut packet = vec![0u8; 300];
        packet[0] = 0x08; // version 0, telemetry, no secondary header, APID 11
        packet[1] = 11;
        packet[2] = 0xC0; // unsegmented
        packet[3] = 1; // sequence 1
        packet[4..6].copy_from_slice(&(300u16 - 7).to_be_bytes());
        for (index, byte) in packet.iter_mut().enumerate().skip(6) {
            *byte = (index % 251) as u8;
        }
        let mut idle = vec![0u8; 130];
        idle[0] = 0x08 | ((IDLE_APID >> 8) as u8);
        idle[1] = (IDLE_APID & 0xFF) as u8;
        idle[2] = 0xC0;
        idle[4..6].copy_from_slice(&(130u16 - 7).to_be_bytes());

        let frame = |count: u8, pointer: u16, data: &[u8]| {
            let mut frame = Vec::with_capacity(frame_length);
            frame.extend_from_slice(&((SCID << 4) | (u16::from(VCID) << 1)).to_be_bytes());
            frame.push(count);
            frame.push(count);
            frame.extend_from_slice(&((0b11u16 << 11) | pointer).to_be_bytes());
            let mut field = data.to_vec();
            field.resize(data_field, 0);
            frame.extend_from_slice(&field);
            let crc = crc16_ccitt(&frame);
            frame.extend_from_slice(&crc.to_be_bytes());
            frame
        };

        let mut tail = packet[215..].to_vec();
        tail.extend_from_slice(&idle);
        assert_eq!(
            tail.len(),
            data_field,
            "the fixture must fill the third frame exactly"
        );

        let mut wire = Vec::new();
        for built in [
            frame(0, 0, &packet[..215]),
            frame(1, ONLY_IDLE_DATA, &[0x55; 8]),
            frame(2, (300 - 215) as u16, &tail),
        ] {
            wire.extend_from_slice(&CCSDS_ASM);
            wire.extend_from_slice(&built);
        }
        wire.extend_from_slice(&CCSDS_ASM);

        let config = PipelineConfig {
            framing: tm(frame_length, None, true, 0),
            ..PipelineConfig::default()
        };
        let (pipeline, out) = run(&config, &wire, 7);
        let snapshot = pipeline.stats().snapshot();

        assert_eq!(snapshot.frames_ok, 3);
        assert_eq!(snapshot.idle_frames, 1);
        assert_eq!(
            snapshot.packets_lost, 0,
            "the idle frame was read as a break in the virtual channel frame count"
        );
        assert_eq!(
            out.len(),
            1,
            "the packet that spanned the idle frame is gone"
        );
        assert_eq!(out[0].bytes, packet, "and it came back changed");
        assert_eq!(
            snapshot.idle_packets, 1,
            "the tail padding is an idle packet"
        );
    }

    #[test]
    fn an_idle_frame_is_counted_and_produces_no_packet() {
        // CCSDS 132.0-B-3: a first header pointer of 0x7FE says the data field is fill. It is
        // a good frame — it passes every check — and it is the only thing that moves
        // `idle_frames`, which nothing else in the workspace writes.
        let frame_length = PLAIN_FRAME_LENGTH;
        let data_field = frame_length - PRIMARY_HEADER_BYTES - FECF_BYTES;
        let mut frame = Vec::with_capacity(frame_length);
        frame.extend_from_slice(&((SCID << 4) | (u16::from(VCID) << 1)).to_be_bytes());
        frame.push(0);
        frame.push(0);
        frame.extend_from_slice(&((0b11u16 << 11) | ONLY_IDLE_DATA).to_be_bytes());
        frame.extend_from_slice(&vec![0x55u8; data_field]);
        let crc = crc16_ccitt(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());

        let mut wire = CCSDS_ASM.to_vec();
        wire.extend_from_slice(&frame);
        wire.extend_from_slice(&CCSDS_ASM);

        let config = PipelineConfig {
            framing: tm(frame_length, None, true, 0),
            ..PipelineConfig::default()
        };
        let (pipeline, out) = run(&config, &wire, 3);
        let snapshot = pipeline.stats().snapshot();
        assert_eq!(snapshot.frames_seen, 1);
        assert_eq!(snapshot.frames_ok, 1, "fill is a good frame");
        assert_eq!(snapshot.idle_frames, 1);
        assert_eq!(snapshot.frames_dropped, 0);
        assert_eq!(snapshot.packets_in, 0);
        assert_eq!(snapshot.packets_lost, 0, "fill disturbs no partial packet");
        assert!(out.is_empty());
    }

    #[test]
    fn a_message_boundary_abandons_the_half_frame_and_says_what_it_cost() {
        let fixture = fixture(64, None, false, true);
        let half = fixture.wire.len() / 2;
        let (mut pipeline, mut out) = run(&fixture.config, &fixture.wire[..half], 4096);
        let before = out.len();
        assert!(before > 0, "half a stream should still be packets");
        let _ = pipeline.take_events();

        pipeline.flush_message();
        assert_eq!(pipeline.sync_state(), Some(SyncState::Searching));
        assert!(
            pipeline
                .assembler()
                .channel(VCID)
                .is_none_or(|channel| !channel.is_assembling()),
            "a datagram boundary is not a place to keep half a packet"
        );

        // The second half is not the continuation of the first as far as the link is
        // concerned; nothing may be spliced across the boundary.
        pipeline.push(&fixture.wire[half..], received(), &mut out);
        assert!(
            out.iter()
                .take(before)
                .zip(&fixture.expected)
                .all(|(g, w)| &g.bytes == w)
        );
        assert!(out.len() <= fixture.expected.len());
    }

    // ---- configuration ------------------------------------------------------------------

    #[test]
    fn the_default_configuration_is_accepted() {
        assert!(PipelineConfig::default().validate().is_ok());
    }

    #[test]
    fn a_codeword_is_the_frame_plus_every_codewords_parity() {
        let config = PipelineConfig {
            framing: tm(
                1115,
                Some(RsConfig {
                    interleave: 5,
                    parity_symbols: 32,
                }),
                false,
                0,
            ),
            ..PipelineConfig::default()
        };
        assert_eq!(config.codeword_length(), Some(1115 + 160));
        assert_eq!(config.validate().map_err(|e| e.to_string()), Ok(()));
    }

    #[test]
    fn a_frame_length_of_zero_is_refused_by_name() {
        let config = PipelineConfig {
            framing: Framing::tm_frames(0),
            ..PipelineConfig::default()
        };
        let message = config.validate().unwrap_err().to_string();
        assert!(message.contains("frame_length"), "{message}");
    }

    #[test]
    fn a_frame_with_no_room_left_for_a_data_field_is_refused() {
        let config = PipelineConfig {
            framing: tm(206, None, true, 200),
            ..PipelineConfig::default()
        };
        let message = config.validate().unwrap_err().to_string();
        assert!(message.contains("insert_zone"), "{message}");
        assert!(message.contains("data field"), "{message}");
    }

    #[test]
    fn a_max_packet_length_below_the_shortest_space_packet_is_refused() {
        let config = PipelineConfig {
            max_packet_length: SPACE_PACKET_HEADER_BYTES,
            ..PipelineConfig::default()
        };
        let message = config.validate().unwrap_err().to_string();
        assert!(message.contains("max_packet_length"), "{message}");
    }

    #[test]
    fn a_reed_solomon_pair_ccsds_does_not_define_is_refused_by_value() {
        for (parity, interleave, expected) in [
            (24usize, 1usize, "parity_symbols"),
            (32, 6, "interleave"),
            (32, 0, "interleave"),
        ] {
            let config = PipelineConfig {
                framing: tm(
                    223,
                    Some(RsConfig {
                        interleave,
                        parity_symbols: parity,
                    }),
                    false,
                    0,
                ),
                ..PipelineConfig::default()
            };
            let message = config.validate().unwrap_err().to_string();
            assert!(
                message.contains(expected),
                "{parity}/{interleave}: {message}"
            );
        }
    }

    #[test]
    fn a_frame_length_the_reed_solomon_block_cannot_produce_is_refused() {
        // 1115 is RS(255, 223) interleaved five deep. With interleave 1 it is a frame length
        // that makes every block `BadLength` for the life of the session.
        let config = PipelineConfig {
            framing: tm(1115, Some(RsConfig::default()), false, 0),
            ..PipelineConfig::default()
        };
        let message = config.validate().unwrap_err().to_string();
        assert!(message.contains("223"), "{message}");
    }

    #[test]
    fn csp_and_transfer_frames_are_two_link_layers_for_one_job() {
        let config = PipelineConfig {
            framing: Framing::tm_frames(223),
            csp: Some(CspVersion::V1),
            ..PipelineConfig::default()
        };
        let message = config.validate().unwrap_err().to_string();
        assert!(message.contains("csp"), "{message}");
    }

    #[test]
    fn packet_framing_builds_no_synchroniser_and_no_tables() {
        let pipeline = Pipeline::new(PipelineConfig::default(), Arc::new(LinkStats::new()));
        assert!(pipeline.sync_state().is_none());
        assert!(pipeline.reed_solomon().is_none());
        assert_eq!(
            pipeline.stream().max_packet_length(),
            DEFAULT_MAX_PACKET_LENGTH
        );
        assert_eq!(
            pipeline.assembler().max_packet_length(),
            DEFAULT_MAX_PACKET_LENGTH
        );
        assert_eq!(pipeline.config().framing, Framing::Packets);
    }

    // ---- the event queue ----------------------------------------------------------------

    #[test]
    fn a_repeated_line_is_collapsed_before_it_is_ever_formatted() {
        // The first line has to be built; every repeat after it must not reach the closure,
        // because that closure is a `format!` and on a noise stream it would run per frame.
        let mut events = Events::new();
        events.push(Severity::Warning, "frame", Reason::FrameChecksum, || {
            "checksum".to_owned()
        });
        for _ in 0..5_000u32 {
            events.push(Severity::Warning, "frame", Reason::FrameChecksum, || {
                panic!("a repeat must not reach the formatter")
            });
        }
        let drained = events.drain();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].message.contains("5000"), "{:?}", drained[0]);
    }

    // ---- CSP ----------------------------------------------------------------------------

    /// A CSP 1 header of four zero octets: priority, addresses, ports and flags all zero,
    /// whatever order libcsp packs them in. The point of the test is where the header is
    /// stripped, not how it is read — `csp.rs` owns that and tests it.
    const BLANK_CSP_V1: [u8; 4] = [0, 0, 0, 0];

    fn csp_packets() -> PipelineConfig {
        PipelineConfig {
            csp: Some(CspVersion::V1),
            ..PipelineConfig::default()
        }
    }

    #[test]
    fn one_csp_header_is_stripped_per_message_not_per_packet() {
        let packets = synthetic_packets(3);
        let mut message = BLANK_CSP_V1.to_vec();
        message.extend_from_slice(&packets[0]);
        message.extend_from_slice(&packets[1]);

        let mut pipeline = Pipeline::new(csp_packets(), Arc::new(LinkStats::new()));
        let mut out = Vec::new();
        pipeline.push(&message, received(), &mut out);
        assert_eq!(out.len(), 2, "two packets behind one header");
        assert_eq!(out[0].bytes, packets[0]);
        assert_eq!(out[1].bytes, packets[1]);

        // The next datagram carries its own header, and only because the boundary said so.
        pipeline.flush_message();
        let mut next = BLANK_CSP_V1.to_vec();
        next.extend_from_slice(&packets[2]);
        pipeline.push(&next, received(), &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].bytes, packets[2]);
        assert_eq!(pipeline.stats().snapshot().packets_in, 3);
    }

    #[test]
    fn a_message_too_short_to_hold_a_csp_header_is_dropped_with_a_reason() {
        let mut pipeline = Pipeline::new(csp_packets(), Arc::new(LinkStats::new()));
        let mut out = Vec::new();
        pipeline.push(&[0, 0], received(), &mut out);
        assert!(out.is_empty());
        assert_eq!(
            pipeline.stats().snapshot().bytes_in,
            2,
            "the octets arrived"
        );
        let events = pipeline.take_events();
        assert!(
            events.iter().any(|event| event.message.contains("CSP")),
            "{events:?}"
        );
    }

    #[test]
    fn the_queue_is_capped_and_reports_how_many_it_dropped() {
        let reasons = [
            Reason::FrameChecksum,
            Reason::FrameHeader,
            Reason::RsUncorrectable,
        ];
        let mut events = Events::new();
        for index in 0..1_000usize {
            let reason = reasons[index % reasons.len()];
            events.push(Severity::Warning, "frame", reason, || {
                format!("line {index}")
            });
        }
        let drained = events.drain();
        assert_eq!(drained.len(), EVENT_QUEUE_CAPACITY + 1);
        let last = &drained[EVENT_QUEUE_CAPACITY];
        assert!(last.message.contains("not shown"), "{last:?}");
        assert!(
            drained
                .iter()
                .all(|event| event.severity <= Severity::Error),
            "the summary is a line like any other"
        );
        // Draining resets the cap, so the next second of a bad link is reported afresh.
        events.push(Severity::Warning, "frame", Reason::FrameChecksum, || {
            "after".to_owned()
        });
        assert_eq!(events.drain().len(), 1);
    }
}
