//! The command line, and the [`SessionConfig`] each subcommand turns into.
//!
//! Four subcommands, and the split between them is what the operator already knows rather
//! than what the code shares: `run` is a live pass, `replay` is a recording played back at a
//! rate, `export` is a file turned into a table, and `probe` is a stream nobody has a
//! definition for yet. Three of the four build a [`SessionConfig`] and hand it to the same
//! engine; `probe` is the one that cannot, because a `SessionConfig` names a definition and
//! not having one is the whole point of it.
//!
//! The doc comments in this file are `--help` text, not rustdoc. They are written to be read
//! in a terminal by someone who is about to point a station at a spacecraft, which is why
//! they say what a flag *is* rather than what it sets.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use xtce_gs_engine::{SessionConfig, TimeFormat, TimeSource};
use xtce_gs_link::{CspVersion, Framing, PipelineConfig, RsConfig, SourceSpec};

use crate::CliError;

/// Where a `run` session listens when the operator names no source.
///
/// The same port [`xtce_gs_engine::config::DEFAULT_UDP_PORT`] names. Repeated as a URL here
/// because this is the string the help text prints.
pub const DEFAULT_SOURCE: &str = "udp://0.0.0.0:10015";

/// Transfer frame length assumed when the operator does not say.
///
/// 1115 octets: CCSDS RS(255, 223) with interleave 5, which is what most missions in reach
/// send. It is a guess that is right often enough to be useful and wrong quietly enough to be
/// worth stating — a frame length that does not match the link produces a synchroniser that
/// locks once and then finds nothing.
pub const DEFAULT_FRAME_LENGTH: usize = 1115;

/// Replay rate when `replay` is given none, in bytes per second.
///
/// 125 000 B/s is a 1 Mbit/s downlink. Zero means "as fast as the disk allows", which is right
/// for a test and wrong for anything an operator is watching: a recorded pass replayed at disk
/// speed arrives in milliseconds and every plot becomes one vertical line.
pub const DEFAULT_REPLAY_RATE: u64 = 125_000;

/// Codewords interleaved across one frame when `--rs` is given and `--interleave` is not.
///
/// One, which is [`RsConfig::default`]'s depth and CCSDS 131.0-B-5 section 4's I = 1. A
/// mission that interleaves says so; a mission that does not says nothing, and this is what
/// nothing means.
pub const DEFAULT_INTERLEAVE: usize = 1;

/// Points of history an `export` keeps.
///
/// One. Nothing reads the history on this path — the rows go straight to the CSV — and
/// [`SessionConfig::validate`] refuses zero, so this is the smallest legal value rather than
/// a number anybody chose. A definition of 9 493 parameters at the interface's default would
/// be 600 MB of rings for an export that never looks in one.
pub const EXPORT_HISTORY_DEPTH: usize = 1;

/// A ground station for CCSDS telemetry described by XTCE.
#[derive(Parser, Debug)]
#[command(name = "xtce-gs", version, about, long_about = None)]
pub struct Cli {
    /// Say more: every event the link and the decoder raise, not only the ones that matter.
    ///
    /// Global so that it can be typed on either side of the subcommand — an operator who has
    /// already pressed return once and is retrying with more detail should not have to
    /// remember which end it goes on.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The four things this program does.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open a live session on a socket or a file and watch it.
    Run(RunArgs),

    /// Replay a recording at a rate, as if it were a pass.
    Replay(ReplayArgs),

    /// Decode a stream into CSV and exit.
    Export(ExportArgs),

    /// Count frames and packets in a file, with no definition.
    Probe(ProbeArgs),
}

/// `xtce-gs run`
#[derive(Args, Debug)]
pub struct RunArgs {
    /// XTCE definition to decode against.
    pub definition: PathBuf,

    /// Where the bytes come from: udp://host:port, tcp://host:port, tcp-listen://host:port,
    /// file:///path?rate=1000000&chunk=4096&repeat, or a bare path.
    #[arg(long, default_value = DEFAULT_SOURCE)]
    pub source: String,

    /// Framing and channel coding.
    #[command(flatten)]
    pub pipeline: PipelineArgs,

    /// Which parameter carries spacecraft time.
    #[command(flatten)]
    pub time: TimeArgs,

    /// Points of history kept per watched parameter.
    #[arg(long, default_value_t = xtce_gs_engine::config::DEFAULT_HISTORY_DEPTH)]
    pub history: usize,

    /// JSON limits file, keyed by qualified parameter name.
    #[arg(long)]
    pub limits: Option<PathBuf>,

    /// Append every received byte to this file.
    #[arg(long)]
    pub record: Option<PathBuf>,

    /// Container to start decoding at. Defaults to the definition's only root.
    #[arg(long)]
    pub root: Option<String>,

    /// Run without a window, printing counters once a second.
    #[arg(long)]
    pub headless: bool,
}

impl RunArgs {
    /// Turns the arguments into a session.
    ///
    /// The source is parsed through [`SourceSpec::from_str`], so a bare path is a file —
    /// which is what the link crate documents and what an operator types. Validated here and
    /// not in [`crate::run`]: a configuration the engine will refuse should be refused while
    /// the operator is still looking at the terminal they typed into, not after a window has
    /// opened over it.
    ///
    /// # Errors
    ///
    /// [`CliError::Link`] for a source URL or a framing that cannot be parsed or used,
    /// [`CliError::Engine`] for a configuration the engine refuses.
    pub fn to_session_config(&self) -> Result<SessionConfig, CliError> {
        let config = SessionConfig {
            definition: self.definition.clone(),
            root_container: self.root.clone(),
            source: self.source.parse::<SourceSpec>()?,
            pipeline: self.pipeline.to_pipeline_config()?,
            history_depth: self.history,
            event_capacity: xtce_gs_engine::config::DEFAULT_EVENT_CAPACITY,
            limits: self.limits.clone(),
            record: self.record.clone(),
            spacecraft_time: self.time.to_time_source(),
        };
        config.validate()?;
        Ok(config)
    }
}

/// `xtce-gs replay`
#[derive(Args, Debug)]
pub struct ReplayArgs {
    /// XTCE definition to decode against.
    pub definition: PathBuf,

    /// The recording to replay: a file of raw bytes, exactly as `run --record` wrote it.
    pub recording: PathBuf,

    /// Replay speed in bytes per second. 0 reads as fast as the disk allows.
    #[arg(long, default_value_t = DEFAULT_REPLAY_RATE)]
    pub rate: u64,

    /// Bytes per read. Larger is faster and coarser: the whole chunk shares one receipt time.
    #[arg(long, default_value_t = xtce_gs_link::source::DEFAULT_CHUNK)]
    pub chunk: usize,

    /// Start over at the end instead of ending the session.
    #[arg(long)]
    pub repeat: bool,

    /// Framing and channel coding.
    #[command(flatten)]
    pub pipeline: PipelineArgs,

    /// Which parameter carries spacecraft time.
    #[command(flatten)]
    pub time: TimeArgs,

    /// Points of history kept per watched parameter.
    #[arg(long, default_value_t = xtce_gs_engine::config::DEFAULT_HISTORY_DEPTH)]
    pub history: usize,

    /// JSON limits file, keyed by qualified parameter name.
    #[arg(long)]
    pub limits: Option<PathBuf>,

    /// Container to start decoding at. Defaults to the definition's only root.
    #[arg(long)]
    pub root: Option<String>,

    /// Run without a window, printing counters once a second.
    #[arg(long)]
    pub headless: bool,
}

impl ReplayArgs {
    /// Turns the arguments into a session.
    ///
    /// The same [`SessionConfig`] as `run`, with the source built directly as
    /// [`SourceSpec::File`] rather than parsed from a URL: the operator gave a path and a
    /// rate as separate arguments, and re-encoding them into a URL only to parse it back is
    /// two places to get the escaping wrong. A rate of zero is no rate limit rather than a
    /// replay that never advances. `record` is deliberately absent — recording a replay
    /// writes a copy of the file it is reading.
    ///
    /// # Errors
    ///
    /// [`CliError::Link`] for a framing that cannot be used, [`CliError::Engine`] for a
    /// configuration the engine refuses.
    pub fn to_session_config(&self) -> Result<SessionConfig, CliError> {
        let config = SessionConfig {
            definition: self.definition.clone(),
            root_container: self.root.clone(),
            source: SourceSpec::File {
                path: self.recording.clone(),
                bytes_per_second: (self.rate > 0).then_some(self.rate),
                chunk: self.chunk,
                repeat: self.repeat,
            },
            pipeline: self.pipeline.to_pipeline_config()?,
            history_depth: self.history,
            event_capacity: xtce_gs_engine::config::DEFAULT_EVENT_CAPACITY,
            limits: self.limits.clone(),
            record: None,
            spacecraft_time: self.time.to_time_source(),
        };
        config.validate()?;
        Ok(config)
    }
}

/// `xtce-gs export`
#[derive(Args, Debug)]
pub struct ExportArgs {
    /// XTCE definition to decode against.
    pub definition: PathBuf,

    /// The stream to decode: a recording, or a file of packets.
    pub stream: PathBuf,

    /// Write here instead of standard output.
    #[arg(long, short)]
    pub output: Option<PathBuf>,

    /// Framing and channel coding.
    #[command(flatten)]
    pub pipeline: PipelineArgs,

    /// Which parameter carries spacecraft time.
    #[command(flatten)]
    pub time: TimeArgs,

    /// Container to start decoding at. Defaults to the definition's only root.
    #[arg(long)]
    pub root: Option<String>,

    /// Stop after this many packets.
    #[arg(long)]
    pub limit: Option<usize>,
}

impl ExportArgs {
    /// Turns the arguments into a session.
    ///
    /// The source is the stream at no rate limit — an export is not watched, so pacing it to
    /// a downlink's bit rate would make a two-minute pass take two minutes — read in
    /// [`xtce_gs_engine::record::EXPORT_CHUNK_BYTES`] bites, and the history is
    /// [`EXPORT_HISTORY_DEPTH`] because nothing on this path reads one.
    ///
    /// # Errors
    ///
    /// [`CliError::Link`] for a framing that cannot be used, [`CliError::Engine`] for a
    /// configuration the engine refuses.
    pub fn to_session_config(&self) -> Result<SessionConfig, CliError> {
        let config = SessionConfig {
            definition: self.definition.clone(),
            root_container: self.root.clone(),
            source: SourceSpec::File {
                path: self.stream.clone(),
                bytes_per_second: None,
                chunk: xtce_gs_engine::record::EXPORT_CHUNK_BYTES,
                repeat: false,
            },
            pipeline: self.pipeline.to_pipeline_config()?,
            history_depth: EXPORT_HISTORY_DEPTH,
            event_capacity: xtce_gs_engine::config::DEFAULT_EVENT_CAPACITY,
            limits: None,
            record: None,
            spacecraft_time: self.time.to_time_source(),
        };
        config.validate()?;
        Ok(config)
    }
}

/// `xtce-gs probe`
///
/// What a new stream is characterised with before anyone writes a definition for it: whether
/// there are sync markers in it, at what spacing, how many frames pass their checksum, and
/// which APIDs come out. Everything here is answerable from the bytes alone.
#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// The file to characterise.
    pub stream: PathBuf,

    /// Framing and channel coding to try.
    #[command(flatten)]
    pub pipeline: PipelineArgs,

    /// Stop after this many bytes.
    #[arg(long)]
    pub limit: Option<u64>,

    /// Print the first n packets' APID, sequence count and length.
    #[arg(long, default_value_t = 16)]
    pub sample: usize,
}

impl ProbeArgs {
    /// The source this probe reads.
    ///
    /// [`SourceSpec::File`] over `stream` with no rate limit and no repeat. Not a
    /// [`SessionConfig`]: that names a definition, and a stream nobody has one for is what
    /// this subcommand exists for.
    ///
    /// # Errors
    ///
    /// [`CliError::Link`] when the path cannot be turned into a source. None of the forms
    /// this builds can fail today; the `Result` is here because the path is the operator's
    /// and the next thing to go in it — a URL, a device — can.
    /// The framing this probe tries.
    ///
    /// [`PipelineArgs::to_pipeline_config`] plus the refusal [`SessionConfig::validate`] would
    /// have made if a probe had a session: a CSP header carries no length field, so a CSP
    /// stream can only be read where the message boundaries come from the transport, and a
    /// probe always reads a file. Without this the pipeline strips exactly one CSP header —
    /// the first chunk's — and reports a stream of nothing.
    ///
    /// # Errors
    ///
    /// [`CliError::Link`] naming the flag and the value that cannot be used.
    pub fn to_pipeline_config(&self) -> Result<PipelineConfig, CliError> {
        if self.pipeline.csp.is_some() {
            return Err(CliError::Link(xtce_gs_link::LinkError::Config(
                "--csp given to probe, which reads a file: a CSP header has no length field, \
                 so a CSP stream can only be read where the message boundaries come from the \
                 transport — a UDP source, which probe is not"
                    .to_owned(),
            )));
        }
        self.pipeline.to_pipeline_config()
    }

    // The `Result` is deliberate and `unnecessary_wraps` is wrong about it: the signature is
    // the one `main::dispatch` calls with `?`, and the next thing an operator puts in this
    // argument — a URL, a device, a directory of passes — fails to parse. Narrowing it now
    // means widening it again, and a caller has to change either way round.
    #[allow(clippy::unnecessary_wraps)]
    pub fn to_source_spec(&self) -> Result<SourceSpec, CliError> {
        Ok(SourceSpec::File {
            path: self.stream.clone(),
            bytes_per_second: None,
            chunk: xtce_gs_link::source::DEFAULT_CHUNK,
            repeat: false,
        })
    }
}

/// How the bytes are taken apart, shared by every subcommand.
///
/// Four independent flags, which is what a CCSDS frame layout is: the randomiser, the error
/// control field, the operational control field and the sync marker are each present or
/// absent on a real link in any combination. Folding them into a state machine, as
/// `struct_excessive_bools` suggests, would mean inventing names for sixteen combinations and
/// a command line nobody could type.
#[allow(clippy::struct_excessive_bools)]
#[derive(Args, Debug)]
pub struct PipelineArgs {
    /// What the bytes are: back-to-back CCSDS space packets, or TM transfer frames.
    #[arg(long, value_enum, default_value_t = FramingKind::Packets)]
    pub framing: FramingKind,

    /// Length of the transfer frame, ASM and Reed-Solomon parity excluded.
    ///
    /// This is not the spacing of the sync markers on the wire: on a coded link the markers
    /// are this plus the parity apart. Giving the codeword length here makes the synchroniser
    /// cut every frame short.
    #[arg(long, default_value_t = DEFAULT_FRAME_LENGTH)]
    pub frame_length: usize,

    /// Reed-Solomon parity symbols per codeword: 32 for E=16, 16 for E=8. Omit for no coding.
    #[arg(long)]
    pub rs: Option<usize>,

    /// Codewords interleaved across one frame. Defaults to 1.
    #[arg(long)]
    pub interleave: Option<usize>,

    /// Undo the CCSDS pseudo-randomiser.
    #[arg(long)]
    pub derandomize: bool,

    /// The frame ends with a frame error control field.
    #[arg(long)]
    pub fecf: bool,

    /// The frame carries an operational control field.
    #[arg(long)]
    pub ocf: bool,

    /// Bytes of insert zone after the primary header. Defaults to none.
    #[arg(long)]
    pub insert_zone: Option<usize>,

    /// The frames are not preceded by an attached sync marker.
    #[arg(long)]
    pub no_asm: bool,

    /// Search the seven intermediate bit offsets as well as the byte ones.
    ///
    /// Costs eight times the comparisons while the synchroniser is searching and nothing once
    /// it has locked. Worth it on a link whose bit and byte clocks are recovered separately;
    /// dead weight on a recording.
    #[arg(long)]
    pub bit_slip: bool,

    /// Consecutive missed sync markers tolerated before lock is given up.
    #[arg(long, default_value_t = xtce_gs_link::sync::DEFAULT_FLYWHEEL)]
    pub flywheel: u32,

    /// A CSP header is wrapped around each packet.
    #[arg(long, value_enum)]
    pub csp: Option<CspKind>,

    /// A packet longer than this is a desync, not a packet.
    #[arg(long, default_value_t = xtce_gs_link::pipeline::DEFAULT_MAX_PACKET_LENGTH)]
    pub max_packet_length: usize,
}

/// The flags that describe a transfer frame and nothing else.
///
/// Named here so that the refusal message and the check cannot drift apart, and in the order
/// the message reports them: the first one the operator typed is the one it names.
const FRAME_ONLY_FLAGS: [&str; 6] = [
    "--rs",
    "--interleave",
    "--derandomize",
    "--fecf",
    "--ocf",
    "--insert-zone",
];

impl PipelineArgs {
    /// Turns the arguments into a pipeline configuration.
    ///
    /// # What is refused, and what is merely ignored
    ///
    /// The six flags in [`FRAME_ONLY_FLAGS`] are refused under `--framing packets` rather
    /// than dropped. None of them is a flag that happens not to apply: each is a claim about
    /// the link — that it is coded, that it was randomised, that the frame carries a trailer
    /// — and packet framing cannot honour any of them. Silently ignoring one is how a station
    /// reports zero uncorrectable frames on a coded downlink it is not decoding.
    ///
    /// `--interleave` and `--insert-zone` are `Option` for exactly this: with clap defaults
    /// of 1 and 0 there is no way to tell an operator who typed them from one who did not,
    /// and a check against the value would refuse every packet-framing run in the workspace.
    ///
    /// `--frame-length`, `--no-asm`, `--bit-slip` and `--flywheel` are *not* refused. They
    /// describe where a frame starts and how long it is rather than what is done to its
    /// contents, they are the flags an operator flips `--framing` back and forth over while
    /// hunting for a stride, and three of the four have defaults that are a synchroniser's
    /// ordinary settings.
    ///
    // TODO(gs-cli-args): where the line between the two lists falls is a choice made here and
    // not one a Blue Book makes, and `--no-asm` is the arguable one — it is as much
    // a claim about the link as `--derandomize` is. Moving it means deciding what
    // `--framing packets --no-asm` should mean for an operator who is scripting both framings
    // from one set of flags; adding it to `FRAME_ONLY_FLAGS` is the whole change otherwise.
    ///
    /// # Errors
    ///
    /// [`CliError::Link`] naming the flag and the value that cannot be used.
    pub fn to_pipeline_config(&self) -> Result<PipelineConfig, CliError> {
        let framing = match self.framing {
            FramingKind::Packets => {
                self.refuse_frame_flags()?;
                Framing::Packets
            }
            FramingKind::Tm => {
                self.refuse_interleave_without_coding()?;
                Framing::TmFrames {
                    frame_length: self.frame_length,
                    attached_sync_marker: !self.no_asm,
                    bit_slip: self.bit_slip,
                    flywheel: self.flywheel,
                    reed_solomon: self.rs.map(|parity_symbols| RsConfig {
                        interleave: self.interleave.unwrap_or(DEFAULT_INTERLEAVE),
                        parity_symbols,
                    }),
                    derandomize: self.derandomize,
                    has_fecf: self.fecf,
                    has_ocf: self.ocf,
                    insert_zone: self.insert_zone.unwrap_or(0),
                }
            }
        };

        let config = PipelineConfig {
            framing,
            csp: self.csp.map(CspVersion::from),
            max_packet_length: self.max_packet_length,
        };
        config.validate()?;
        Ok(config)
    }

    /// Which of [`FRAME_ONLY_FLAGS`] the operator typed, first one first.
    fn frame_only_flags_given(&self) -> Vec<&'static str> {
        let given = [
            self.rs.is_some(),
            self.interleave.is_some(),
            self.derandomize,
            self.fecf,
            self.ocf,
            self.insert_zone.is_some(),
        ];
        FRAME_ONLY_FLAGS
            .iter()
            .zip(given)
            .filter_map(|(flag, given)| given.then_some(*flag))
            .collect()
    }

    /// Refuses a frame flag under packet framing.
    ///
    /// Each half of the message names the document that defines the thing it is talking
    /// about, and the two halves are different documents. Reed-Solomon parity and the
    /// pseudo-randomiser are the channel coding wrapped *around* a frame, CCSDS 131.0-B-5
    /// sections 4 and 10; the frame error control field and the operational control field
    /// are fields *of* the frame, CCSDS 132.0-B-3 §4.1.1.1 d) and e); and the insert zone is
    /// not a TM frame field at all — it is the AOS one, CCSDS 732.0-B-4 §4.1.3. An operator
    /// who opens 132.0-B-3 to find out what `--rs` means finds nothing: the words
    /// "Reed-Solomon", "pseudo-random" and "insert zone" do not occur in it.
    fn refuse_frame_flags(&self) -> Result<(), CliError> {
        let given = self.frame_only_flags_given();
        if given.is_empty() {
            return Ok(());
        }
        Err(CliError::Link(xtce_gs_link::LinkError::Config(format!(
            "{} set with --framing packets, which has no transfer frames to apply {} to. \
             Reed-Solomon parity and the pseudo-randomiser are the channel coding around a \
             transfer frame (CCSDS 131.0-B-5 sections 4 and 10); the frame error control \
             field and the operational control field are fields of one (CCSDS 132.0-B-3 \
             section 4.1.1.1); the insert zone is a field of an AOS frame (CCSDS 732.0-B-4 \
             section 4.1.3). A stream of back-to-back space packets carries none of them. \
             Give --framing tm, or drop {}",
            given.join(", "),
            if given.len() == 1 { "it" } else { "them" },
            given.join(" and "),
        ))))
    }

    /// Refuses an interleave with nothing to interleave.
    ///
    /// `--interleave` only ever reaches [`RsConfig`], so without `--rs` it is a number this
    /// would drop on the floor — the same silence the packet-framing refusal exists to
    /// prevent, one layer in.
    fn refuse_interleave_without_coding(&self) -> Result<(), CliError> {
        if self.interleave.is_some() && self.rs.is_none() {
            return Err(CliError::Link(xtce_gs_link::LinkError::Config(
                "--interleave given without --rs: interleaving is a property of the \
                 Reed-Solomon code (CCSDS 131.0-B-5 section 4) and an uncoded frame has \
                 nothing to interleave. Give --rs 32 or --rs 16, or drop --interleave"
                    .to_owned(),
            )));
        }
        Ok(())
    }
}

/// Which parameter carries spacecraft time, and how it is laid out.
///
/// Absent means every sample is placed at ground receipt, which is right for a live pass and
/// wrong for a recorder dump — see [`xtce_gs_core::Batch::time`].
#[derive(Args, Debug)]
pub struct TimeArgs {
    /// Qualified or leaf name of the parameter carrying spacecraft time.
    #[arg(long = "time-parameter")]
    pub parameter: Option<String>,

    /// How that parameter's bits are laid out.
    #[arg(long = "time-format", value_enum, default_value_t = TimeFormatKind::Seconds)]
    pub format: TimeFormatKind,

    /// Octets of whole seconds in a CUC field.
    #[arg(long = "time-coarse-bytes", default_value_t = 4)]
    pub coarse_bytes: u8,

    /// Octets of binary fraction below the second in a CUC field.
    #[arg(long = "time-fine-bytes", default_value_t = 2)]
    pub fine_bytes: u8,

    /// Octets of the day count in a CDS field.
    #[arg(long = "time-day-bytes", default_value_t = 2)]
    pub day_bytes: u8,

    /// Octets below the millisecond in a CDS field: 0, 2 for microseconds, 4 for picoseconds.
    #[arg(long = "time-submillisecond-bytes", default_value_t = 0)]
    pub submillisecond_bytes: u8,

    /// What the count is measured from.
    #[arg(long = "time-epoch", value_enum, default_value_t = EpochKind::Ccsds)]
    pub epoch: EpochKind,
}

impl TimeArgs {
    /// The clock these flags describe, or `None` when no parameter was named.
    ///
    /// The layout flags all have defaults, so there is nothing to distinguish "no clock" by
    /// except the parameter name — which is the one flag that has no sensible default, since
    /// no two missions call it the same thing. The layout that does not match the named
    /// format is not checked here: [`SessionConfig::validate`] owns that message, and the
    /// alternative is the same refusal written twice with two wordings.
    #[must_use]
    pub fn to_time_source(&self) -> Option<TimeSource> {
        let parameter = self.parameter.clone()?;
        let epoch = xtce_gs_core::Utc::from(self.epoch);
        let format = match self.format {
            TimeFormatKind::Cuc => TimeFormat::Cuc {
                coarse_bytes: self.coarse_bytes,
                fine_bytes: self.fine_bytes,
                epoch,
            },
            TimeFormatKind::Cds => TimeFormat::Cds {
                day_bytes: self.day_bytes,
                submillisecond_bytes: self.submillisecond_bytes,
                epoch,
            },
            TimeFormatKind::Seconds => TimeFormat::Seconds { epoch },
        };
        Some(TimeSource { parameter, format })
    }
}

/// What the bytes coming off the source are.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum FramingKind {
    /// CCSDS space packets, back to back in a stream or one per datagram.
    Packets,
    /// CCSDS 132.0-B TM transfer frames.
    Tm,
}

/// Which CSP header is wrapped around each packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum CspKind {
    /// CSP 1.x: a four-byte header.
    V1,
    /// CSP 2.x: a six-byte header.
    V2,
}

impl From<CspKind> for CspVersion {
    /// The link crate's name for the same header.
    fn from(kind: CspKind) -> Self {
        match kind {
            CspKind::V1 => Self::V1,
            CspKind::V2 => Self::V2,
        }
    }
}

/// How a spacecraft time parameter is laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum TimeFormatKind {
    /// CCSDS unsegmented time code.
    Cuc,
    /// CCSDS day segmented time code.
    Cds,
    /// A count of seconds — what an XTCE AbsoluteTimeParameter usually decodes to.
    Seconds,
}

/// What a spacecraft clock counts from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum EpochKind {
    /// 1958-01-01, the CCSDS epoch.
    Ccsds,
    /// 1970-01-01, the Unix epoch.
    Unix,
    /// 1980-01-06, the GPS epoch.
    Gps,
}

impl From<EpochKind> for xtce_gs_core::Utc {
    /// The instant an epoch names.
    ///
    /// A conversion rather than a method because the three subcommands that build a
    /// [`xtce_gs_engine::TimeFormat`] all want a [`xtce_gs_core::Utc`] and none of them want
    /// to name the mapping.
    fn from(epoch: EpochKind) -> Self {
        match epoch {
            EpochKind::Ccsds => Self::CCSDS_EPOCH,
            EpochKind::Unix => Self::EPOCH,
            EpochKind::Gps => Self::GPS_EPOCH,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Parses a command line, or says which one would not parse.
    fn parse(args: &[&str]) -> Cli {
        match Cli::try_parse_from(args) {
            Ok(cli) => cli,
            Err(error) => panic!("{:?} did not parse: {error}", &args[1..]),
        }
    }

    /// The framing half of a parsed command line, whichever subcommand carries it.
    fn pipeline_of(cli: &Cli) -> &PipelineArgs {
        match &cli.command {
            Command::Run(args) => &args.pipeline,
            Command::Replay(args) => &args.pipeline,
            Command::Export(args) => &args.pipeline,
            Command::Probe(args) => &args.pipeline,
        }
    }

    #[test]
    fn run_takes_a_definition_and_defaults_the_rest() {
        let cli = parse(&["xtce-gs", "run", "mission.xml"]);
        let Command::Run(args) = &cli.command else {
            panic!("not a run");
        };
        assert_eq!(args.definition, Path::new("mission.xml"));
        assert_eq!(args.source, DEFAULT_SOURCE);
        assert!(!args.headless);
        let config = args.to_session_config().unwrap();
        assert_eq!(config.source, SourceSpec::Udp(([0, 0, 0, 0], 10015).into()));
        assert_eq!(config.pipeline.framing, Framing::Packets);
        assert!(config.spacecraft_time.is_none());
    }

    #[test]
    fn a_bare_path_is_a_file_source() {
        let cli = parse(&["xtce-gs", "run", "mission.xml", "--source", "/tmp/pass.dat"]);
        let Command::Run(args) = &cli.command else {
            panic!("not a run");
        };
        let config = args.to_session_config().unwrap();
        let SourceSpec::File { path, repeat, .. } = &config.source else {
            panic!("not a file: {:?}", config.source);
        };
        assert_eq!(path, Path::new("/tmp/pass.dat"));
        assert!(!repeat);
    }

    #[test]
    fn replay_builds_the_file_source_from_its_own_flags() {
        let cli = parse(&[
            "xtce-gs",
            "replay",
            "mission.xml",
            "pass.dat",
            "--rate",
            "2000",
            "--chunk",
            "512",
            "--repeat",
        ]);
        let Command::Replay(args) = &cli.command else {
            panic!("not a replay");
        };
        let config = args.to_session_config().unwrap();
        assert_eq!(
            config.source,
            SourceSpec::File {
                path: "pass.dat".into(),
                bytes_per_second: Some(2000),
                chunk: 512,
                repeat: true,
            }
        );
        // A replay of a recording must not write a second copy of it.
        assert!(config.record.is_none());
    }

    #[test]
    fn a_replay_rate_of_zero_is_no_rate_limit() {
        let cli = parse(&["xtce-gs", "replay", "m.xml", "pass.dat", "--rate", "0"]);
        let Command::Replay(args) = &cli.command else {
            panic!("not a replay");
        };
        let config = args.to_session_config().unwrap();
        let SourceSpec::File {
            bytes_per_second, ..
        } = &config.source
        else {
            panic!("not a file");
        };
        assert_eq!(*bytes_per_second, None);
    }

    #[test]
    fn an_export_keeps_the_smallest_legal_history() {
        let cli = parse(&["xtce-gs", "export", "m.xml", "pass.dat", "-o", "out.csv"]);
        let Command::Export(args) = &cli.command else {
            panic!("not an export");
        };
        assert_eq!(args.output.as_deref(), Some(Path::new("out.csv")));
        let config = args.to_session_config().unwrap();
        assert_eq!(config.history_depth, EXPORT_HISTORY_DEPTH);
        // The smallest legal value, not merely a small one.
        assert!(config.validate().is_ok());
        let SourceSpec::File {
            bytes_per_second, ..
        } = &config.source
        else {
            panic!("not a file");
        };
        assert_eq!(*bytes_per_second, None, "an export is not paced");
    }

    #[test]
    fn a_probe_reads_the_file_once_at_full_speed() {
        let cli = parse(&["xtce-gs", "probe", "unknown.bin"]);
        let Command::Probe(args) = &cli.command else {
            panic!("not a probe");
        };
        assert_eq!(args.sample, 16);
        assert_eq!(
            args.to_source_spec().unwrap(),
            SourceSpec::File {
                path: "unknown.bin".into(),
                bytes_per_second: None,
                chunk: xtce_gs_link::source::DEFAULT_CHUNK,
                repeat: false,
            }
        );
    }

    #[test]
    fn packet_framing_with_no_frame_flags_is_accepted() {
        // The check that catches the catastrophic version of the refusal below: a value-based
        // test on --interleave or --insert-zone would refuse this, which is every ordinary
        // run of this program.
        let cli = parse(&["xtce-gs", "probe", "unknown.bin"]);
        let config = pipeline_of(&cli).to_pipeline_config().unwrap();
        assert_eq!(config.framing, Framing::Packets);
    }

    #[test]
    fn every_frame_flag_is_refused_under_packet_framing() {
        for (flag, value) in [
            ("--rs", Some("32")),
            ("--interleave", Some("5")),
            ("--derandomize", None),
            ("--fecf", None),
            ("--ocf", None),
            ("--insert-zone", Some("4")),
        ] {
            let mut line = vec!["xtce-gs", "probe", "unknown.bin", flag];
            line.extend(value);
            let cli = parse(&line);
            let error = pipeline_of(&cli)
                .to_pipeline_config()
                .expect_err("{flag} was accepted under packet framing");
            let message = error.to_string();
            assert!(
                message.contains(flag),
                "the refusal of {flag} does not name it: {message}"
            );
            assert!(
                message.contains("--framing tm"),
                "the refusal of {flag} does not say what to do: {message}"
            );
        }
    }

    #[test]
    fn the_refusal_cites_the_document_that_defines_each_thing_it_names() {
        // Rule 3: the refusal is where an operator is sent to read the standard, so what it
        // cites has to define what it names. Asserted structurally — the first document named
        // after each subject — because the message this replaced attributed Reed-Solomon
        // parity, the pseudo-randomiser and the insert zone to CCSDS 132.0-B-3, which
        // contains none of those three terms, and the test that pinned it asserted only that
        // the flag was named.
        let cli = parse(&["xtce-gs", "probe", "unknown.bin", "--rs", "32"]);
        let message = pipeline_of(&cli)
            .to_pipeline_config()
            .expect_err("--rs was accepted under packet framing")
            .to_string();

        for (subject, document) in [
            // CCSDS 131.0-B-5 §4.3.2 and §10: the coding sublayer, around the frame.
            (
                "Reed-Solomon parity and the pseudo-randomiser",
                "CCSDS 131.0-B-5",
            ),
            // CCSDS 132.0-B-3 §4.1.1.1 d) and e): fields of the TM transfer frame.
            (
                "frame error control field and the operational control field",
                "CCSDS 132.0-B-3",
            ),
            // CCSDS 732.0-B-4 §4.1.3: an AOS field, and in no TM frame at all.
            ("insert zone", "CCSDS 732.0-B-4"),
        ] {
            let at = message
                .find(subject)
                .unwrap_or_else(|| panic!("the refusal no longer names {subject}: {message}"));
            let after = message.get(at..).unwrap_or_default();
            let cited = after
                .find("CCSDS ")
                .and_then(|start| after.get(start..start + document.len()));
            assert_eq!(
                cited,
                Some(document),
                "{subject} is attributed to the wrong document: {message}"
            );
        }
    }

    #[test]
    fn an_explicit_interleave_of_one_is_still_refused_under_packet_framing() {
        // The value clap would have defaulted to. An `Option` is what makes this reachable;
        // a `!= 1` check would take it for a flag nobody typed.
        let cli = parse(&["xtce-gs", "probe", "unknown.bin", "--interleave", "1"]);
        let error = pipeline_of(&cli).to_pipeline_config().unwrap_err();
        assert!(error.to_string().contains("--interleave"));
    }

    #[test]
    fn the_same_flags_are_accepted_under_transfer_frames() {
        let cli = parse(&[
            "xtce-gs",
            "probe",
            "unknown.bin",
            "--framing",
            "tm",
            "--rs",
            "32",
            "--interleave",
            "5",
            "--derandomize",
            "--fecf",
            "--ocf",
            "--insert-zone",
            "4",
            "--bit-slip",
            "--flywheel",
            "8",
        ]);
        let config = pipeline_of(&cli).to_pipeline_config().unwrap();
        assert_eq!(
            config.framing,
            Framing::TmFrames {
                frame_length: DEFAULT_FRAME_LENGTH,
                attached_sync_marker: true,
                bit_slip: true,
                flywheel: 8,
                reed_solomon: Some(RsConfig {
                    interleave: 5,
                    parity_symbols: 32
                }),
                derandomize: true,
                has_fecf: true,
                has_ocf: true,
                insert_zone: 4,
            }
        );
    }

    #[test]
    fn no_asm_turns_the_marker_off_and_not_on() {
        let cli = parse(&["xtce-gs", "probe", "x.bin", "--framing", "tm", "--no-asm"]);
        let config = pipeline_of(&cli).to_pipeline_config().unwrap();
        let Framing::TmFrames {
            attached_sync_marker,
            ..
        } = config.framing
        else {
            panic!("not frames");
        };
        assert!(!attached_sync_marker);
    }

    #[test]
    fn an_interleave_with_nothing_to_interleave_is_refused() {
        let cli = parse(&[
            "xtce-gs",
            "probe",
            "x.bin",
            "--framing",
            "tm",
            "--interleave",
            "5",
        ]);
        let error = pipeline_of(&cli).to_pipeline_config().unwrap_err();
        assert!(error.to_string().contains("--rs"), "{error}");
    }

    #[test]
    fn a_frame_length_the_code_cannot_produce_is_refused() {
        // RS(255, 223) interleaved 4 deep is a 892-octet frame, not 1115. The link crate owns
        // this message; what is tested here is that the CLI asks for it at all.
        let cli = parse(&[
            "xtce-gs",
            "probe",
            "x.bin",
            "--framing",
            "tm",
            "--rs",
            "32",
            "--interleave",
            "4",
            "--frame-length",
            "1115",
        ]);
        let error = pipeline_of(&cli).to_pipeline_config().unwrap_err();
        assert!(error.to_string().contains("892"), "{error}");
    }

    #[test]
    fn csp_and_transfer_frames_are_two_link_layers_and_are_refused_together() {
        let cli = parse(&[
            "xtce-gs",
            "probe",
            "x.bin",
            "--framing",
            "tm",
            "--csp",
            "v1",
        ]);
        let error = pipeline_of(&cli).to_pipeline_config().unwrap_err();
        assert!(error.to_string().contains("csp"), "{error}");
    }

    #[test]
    fn csp_on_a_file_source_is_refused_by_the_session() {
        // `PipelineConfig::validate` accepts CSP over packet framing; it is the *source* that
        // decides, and a file has no message boundaries to strip a headerless CSP frame at.
        let cli = parse(&["xtce-gs", "replay", "m.xml", "pass.dat", "--csp", "v1"]);
        let Command::Replay(args) = &cli.command else {
            panic!("not a replay");
        };
        let error = args.to_session_config().unwrap_err();
        assert!(error.to_string().contains("csp"), "{error}");
    }

    #[test]
    fn csp_on_a_probe_is_refused_because_a_file_has_no_message_boundaries() {
        let cli = parse(&["xtce-gs", "probe", "x.bin", "--csp", "v1"]);
        let Command::Probe(args) = &cli.command else {
            panic!("not a probe");
        };
        // The plain pipeline conversion accepts it — CSP over packet framing is legal — and
        // it is the source that decides. A probe that forwarded to it would read one header.
        assert!(args.pipeline.to_pipeline_config().is_ok());
        let error = args.to_pipeline_config().unwrap_err();
        assert!(error.to_string().contains("--csp"), "{error}");
    }

    #[test]
    fn a_named_clock_becomes_a_time_source_and_an_unnamed_one_does_not() {
        let cli = parse(&[
            "xtce-gs",
            "run",
            "m.xml",
            "--time-parameter",
            "/Sat/OBT",
            "--time-format",
            "cuc",
            "--time-coarse-bytes",
            "4",
            "--time-fine-bytes",
            "2",
            "--time-epoch",
            "gps",
        ]);
        let Command::Run(args) = &cli.command else {
            panic!("not a run");
        };
        assert_eq!(
            args.time.to_time_source(),
            Some(TimeSource {
                parameter: "/Sat/OBT".to_owned(),
                format: TimeFormat::Cuc {
                    coarse_bytes: 4,
                    fine_bytes: 2,
                    epoch: xtce_gs_core::Utc::GPS_EPOCH,
                },
            })
        );

        let bare = parse(&["xtce-gs", "run", "m.xml"]);
        let Command::Run(args) = &bare.command else {
            panic!("not a run");
        };
        assert!(args.time.to_time_source().is_none());
    }

    #[test]
    fn a_clock_layout_the_engine_refuses_is_refused_here() {
        // A CDS day segment of one byte. `SessionConfig::validate` owns the message; the
        // point is that it is reached before a window opens.
        let cli = parse(&[
            "xtce-gs",
            "run",
            "m.xml",
            "--time-parameter",
            "/Sat/OBT",
            "--time-format",
            "cds",
            "--time-day-bytes",
            "1",
        ]);
        let Command::Run(args) = &cli.command else {
            panic!("not a run");
        };
        let error = args.to_session_config().unwrap_err();
        assert!(error.to_string().contains("day segment"), "{error}");
    }

    #[test]
    fn a_history_of_zero_is_refused_before_anything_is_opened() {
        let cli = parse(&["xtce-gs", "run", "m.xml", "--history", "0"]);
        let Command::Run(args) = &cli.command else {
            panic!("not a run");
        };
        let error = args.to_session_config().unwrap_err();
        assert!(error.to_string().contains("history_depth"), "{error}");
    }

    #[test]
    fn verbose_is_global_and_may_be_typed_on_either_side() {
        assert!(parse(&["xtce-gs", "--verbose", "probe", "x.bin"]).verbose);
        assert!(parse(&["xtce-gs", "probe", "x.bin", "--verbose"]).verbose);
        assert!(parse(&["xtce-gs", "probe", "-v", "x.bin"]).verbose);
        assert!(!parse(&["xtce-gs", "probe", "x.bin"]).verbose);
    }

    #[test]
    fn version_prints_the_crate_version() {
        let error = Cli::try_parse_from(["xtce-gs", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(
            error.to_string().contains(env!("CARGO_PKG_VERSION")),
            "{error}"
        );
    }
}
