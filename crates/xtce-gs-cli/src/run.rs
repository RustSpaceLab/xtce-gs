//! The four things the command line actually does.
//!
//! Two of them start a session and two do not. `gui` and `headless` differ only in what reads
//! the store — a window, or a line of counters once a second — and share every failure mode,
//! which is why they take the same [`SessionConfig`] and the choice is one flag rather than
//! two subcommands. `export` and `probe` do not open a window, do not keep a history, and end
//! when the file does.
//!
//! # Why headless exists
//!
//! A station on a machine with no display is the normal case for an unattended pass: a
//! recorder writing bytes, checked on over ssh. `--headless` with `--record` is that station,
//! and the counters it prints are the same [`xtce_gs_core::StatsSnapshot`] the status bar
//! draws — so a fault diagnosed over ssh is diagnosed with the same numbers as one diagnosed
//! at the desk.
//!
//! # Which stream a line goes to
//!
//! `export` writes its CSV to standard output and everything else — progress, the summary,
//! every event — to standard error, because `xtce-gs export … > pass.csv` is how it is run
//! and a progress line in the middle of a CSV is a row nothing can parse. `headless` and
//! `probe` put their report on standard output, which is the thing being asked for, and their
//! event lines on standard error.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use xtce_decode::Decoder;
use xtce_gs_core::{EventLog, LinkStats, RawPacket, Severity, StatsSnapshot, Utc};
use xtce_gs_engine::decode::{APID_COUNT, SEQUENCE_MODULUS};
use xtce_gs_engine::record::{BATCH_HEADER, EXPORT_CHUNK_BYTES};
use xtce_gs_engine::{
    EngineError, PacketDecoder, Session, SessionConfig, SpacecraftClock, empty_batch, write_batch,
};
use xtce_gs_link::sync::CCSDS_ASM;
use xtce_gs_link::{Framing, Pipeline, PipelineConfig, Source, SourceSpec};
use xtce_model::XtceDb;

use crate::CliError;

/// How often headless mode prints a line.
pub const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// Packets between two progress lines on an export.
///
/// A thousand: on the JPSS recording that is a line every few hundred milliseconds, which is
/// often enough that an operator can see an export is moving and rare enough that the
/// progress does not become the slowest part of it. Not a flag — an export whose progress
/// needs tuning is an export that should be redirected.
pub const PROGRESS_PACKETS: u64 = 1000;

/// Lengths worth trying on a stream nobody has documented.
///
/// 1115 is RS(255, 223) interleaved 5 deep, which is most of the missions in reach; 1279 is
/// *that same link* measured marker to marker with the parity and the ASM counted, which is
/// what an operator is usually told when they are told a number at all; 2048 is a common
/// uncoded frame; 892 is RS(255, 223) interleaved 4 deep.
///
/// They are not all the same quantity, and that is the point of printing two columns per
/// row: 1279 matching as a *stride* and 1115 matching as a *frame* are the same link, and an
/// operator handed 1279 as a `--frame-length` gets a station that locks once and then finds
/// nothing. [`advise`] is what turns a measured stride back into flags, and it is the line to
/// read rather than the table.
const FRAME_LENGTH_CANDIDATES: [u64; 4] = [1115, 1279, 2048, 892];

/// Symbols in a Reed-Solomon codeword: CCSDS 131.0-B-5 section 4.
///
/// The number that makes a coded stride recognisable. A codeword block is a whole multiple of
/// it, and the multiple is the interleaving depth.
const CODEWORD_SYMBOLS: u64 = 255;

/// Interleaving depths CCSDS 131.0-B-5 section 4 defines: I = 1, 2, 3, 4, 5 and 8.
///
/// A stride that divides by 255 into anything else is a coincidence, not a code.
const CCSDS_INTERLEAVE: [u64; 6] = [1, 2, 3, 4, 5, 8];

/// Opens the window and runs until the operator closes it.
///
/// # Errors
///
/// [`CliError::Gui`] for a session that will not start, a layout that will not parse, or a
/// platform that will not give the process a window.
pub fn gui(config: SessionConfig) -> Result<(), CliError> {
    xtce_gs_gui::run(config).map_err(CliError::from)
}

/// Runs with no window, printing counters once a second.
///
/// One line per interval and *counters*, not deltas: a line that prints the change since the
/// last one is unreadable once the terminal has scrolled, and an operator reading it over ssh
/// is comparing it with the line from ten minutes ago. The fields are the ones
/// [`xtce_gs_gui::panels::status`] draws, in the same order, so that the two can be compared
/// at all.
///
/// Ends on ctrl-c, on the SIGTERM a supervisor sends, or on a source that ran out. A file
/// replay that reached the end is a successful exit, not an error — the pass is over, and its
/// counters are the result.
///
/// # Errors
///
/// [`CliError::Engine`] for a session that will not start, [`CliError::Io`] for a reactor that
/// will not build or a terminal that cannot be written to.
pub fn headless(config: SessionConfig, verbose: bool) -> Result<(), CliError> {
    let runtime = tokio::runtime::Runtime::new()?;
    let framed = matches!(config.pipeline.framing, Framing::TmFrames { .. });
    let session = Session::start(config, runtime.handle(), None)?;
    eprintln!("reading {}", session.describe_source());

    let mut cursor = EventCursor::default();
    let mut line = String::new();

    let outcome: Result<(), CliError> = runtime.block_on(async {
        let mut stdout = std::io::stdout();
        let mut ticker = tokio::time::interval(STATUS_INTERVAL);
        // Created once and polled by reference. A fresh `stop_requested()` future per
        // iteration registers a fresh receiver each time, and a signal that lands in the gap
        // between one `select!` cancelling it and the next creating it is a stop the station
        // never sees.
        let interrupt = stop_requested();
        tokio::pin!(interrupt);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    print_events(session.events(), &mut cursor, verbose);
                    status_line(&mut line, &session.stats().snapshot(), framed);
                    writeln!(stdout, "{line}")?;
                    stdout.flush()?;
                    if !session.is_running() {
                        break;
                    }
                }
                result = &mut interrupt => {
                    if let Err(error) = result {
                        // No handler means no clean stop; saying so beats waiting for one.
                        eprintln!("a stop signal cannot be caught here ({error}); stopping now");
                    }
                    break;
                }
            }
        }
        Ok(())
    });

    session.shutdown();
    print_events(session.events(), &mut cursor, verbose);
    status_line(&mut line, &session.stats().snapshot(), framed);
    eprintln!("final {line}");
    outcome
}

/// Formats one status line into `line`, which is reused between intervals.
///
/// The frame, Reed-Solomon and checksum figures are left out when the link is not carrying
/// transfer frames: they are always zero there, and a row of zeroes that can never move
/// teaches an operator to skip the row. `probe` is the place where a zero *is* the answer.
fn status_line(line: &mut String, snapshot: &StatsSnapshot, framed: bool) {
    line.clear();
    let _ = write!(line, "{} bytes={}", Utc::now(), snapshot.bytes_in);
    if framed {
        let _ = write!(
            line,
            " frames={}/{} loss={:.2}% rs={}/{} crc={}",
            snapshot.frames_ok,
            snapshot.frames_seen,
            snapshot.frame_loss() * 100.0,
            snapshot.rs_corrected,
            snapshot.rs_uncorrectable,
            snapshot.crc_failures,
        );
    }
    let _ = write!(
        line,
        " packets={} decoded={} rejected={} lost={} gaps={} missing={}",
        snapshot.packets_in,
        snapshot.packets_decoded,
        snapshot.packets_rejected,
        // Counted by the link and, until this line carried it, visible only as an event: a
        // recording cut short mid-packet, or a partial abandoned at a virtual channel gap.
        snapshot.packets_lost,
        snapshot.sequence_gaps,
        snapshot.sequence_missing,
    );
    match snapshot.last_packet {
        Some(last) => {
            let _ = write!(line, " last={:.1}s ago", Utc::now().secs_since(last));
        }
        None => line.push_str(" last=never"),
    }
}

/// Prints the log lines `cursor` has not seen to stderr and moves it past them.
///
/// A cursor and never [`EventLog::clear`]: the log belongs to the session and the interface
/// may be reading it too. [`EventLog::push`] bumps a collapsed line's count on every repeat,
/// and the cursor is counted in pushes, so a line that keeps happening reprints with a rising
/// count — which is what an operator watching a failing link wants, and what a printer that
/// remembered the message instead of the position would suppress.
fn print_events(log: &Mutex<EventLog>, cursor: &mut EventCursor, verbose: bool) {
    for line in unprinted_events(log, cursor, verbose) {
        eprintln!("{line}");
    }
}

/// How many pushes into an [`EventLog`] a printer has already considered.
///
/// A position and deliberately **not** a timestamp. [`Utc::now`] is `SystemTime::now`, whose
/// granularity here is about a microsecond, so two events raised back to back usually carry
/// the *same* stamp: a loop building 100 000 pairs of `Event::warning` and comparing their
/// `time` found 93 258 of them tied. A cursor that skipped `time <= last` therefore dropped
/// the second of such a pair, permanently, because the cursor only moves forward — and the
/// line that costs an operator most is the last one, since [`headless`] prints once more
/// after [`Session::shutdown`] and that is the line saying why the session stopped.
type EventCursor = u64;

/// The lines `cursor` has not seen yet, oldest first, moving it past them.
///
/// # Why a push count is an exact cursor
///
/// [`EventLog::total`] counts every push, repeats included, and [`EventLog::iter`] yields the
/// distinct entries still held with how many pushes each stands for. So the entries account
/// for the last `sum(count)` pushes, and the entry ending at push `n` can be named by `n`
/// alone. That name survives eviction — dropping the oldest entry lowers `sum(count)` by
/// exactly what it stood for, leaving every surviving `n` where it was — and only the *last*
/// entry's count can grow, which is what makes a collapsed line reprint when, and only when,
/// its count has risen.
///
/// The cursor moves past every entry it considers, printed or not, so an [`Severity::Info`]
/// line held back without `--verbose` cannot make the warning behind it reprint.
fn unprinted_events(log: &Mutex<EventLog>, cursor: &mut EventCursor, verbose: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let guard = log.lock().unwrap_or_else(PoisonError::into_inner);
    let held: u64 = guard.iter().map(|(_, count)| u64::from(count)).sum();
    // Where the oldest entry still held begins: everything the ring has already dropped.
    let mut at = guard.total().saturating_sub(held);
    for (event, count) in guard.iter() {
        at = at.saturating_add(u64::from(count));
        if at <= *cursor {
            continue;
        }
        *cursor = at;
        if !verbose && event.severity == Severity::Info {
            continue;
        }
        let repeats = if count > 1 {
            format!(" (x{count})")
        } else {
            String::new()
        };
        lines.push(format!(
            "{} {} {}: {}{repeats}",
            event.time,
            event.severity.letter(),
            event.source,
            event.message
        ));
    }
    lines
}

/// Resolves when the operator or a supervisor asks for the pass to end.
///
/// ctrl-c is the operator at a terminal. SIGTERM is what systemd, docker and a `kill` in a
/// script send, and the station this module's header describes — "a recorder writing bytes,
/// checked on over ssh" — is the process all three of those stop. A signal left at its
/// default disposition kills the process outright, and a process killed by a signal runs no
/// destructor: up to [`xtce_gs_engine::record::RECORD_BUFFER_BYTES`] of recording never
/// reaches the file and [`xtce_gs_engine::record::Recorder`]'s `Drop` safety net never runs.
///
/// Measured with `xtce-gs run … --headless --record`, killed two seconds in: with ctrl-c
/// alone handled, `kill -TERM` ended the process with status 143, the log stopped at
/// `reading file://…` with no final counter line, and the recording ended at the last full
/// buffer; with this, `kill -TERM` and `kill -INT` both exit 0, print the final line, and
/// leave a recording exactly as long as the `bytes=` the station reports.
///
/// Windows has no SIGTERM and `ctrl_c` there already covers what the console sends.
async fn stop_requested() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            // `recv` is `None` only when the handler is torn down, which nothing here does.
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// What one export run found.
#[derive(Debug, Default)]
struct ExportRun {
    /// Packets taken off the stream, decoded or not.
    packets: u64,
    /// Rows written: one per decoded parameter value.
    ///
    rows: u64,
    /// Packets the definition refused, by the kind of refusal.
    rejected: Vec<(&'static str, u64)>,
    /// Whether the output went away before the stream ended.
    closed: bool,
}

impl ExportRun {
    /// Files one refusal under its kind.
    fn reject(&mut self, kind: &'static str) {
        match self.rejected.iter_mut().find(|(name, _)| *name == kind) {
            Some((_, count)) => *count = count.saturating_add(1),
            None => self.rejected.push((kind, 1)),
        }
    }

    /// How many packets were refused in total.
    fn rejected_total(&self) -> u64 {
        self.rejected.iter().map(|(_, count)| *count).sum()
    }
}

/// Decodes a stream into CSV.
///
/// Deliberately *not* a [`Session`]: a session files everything into a store an interface
/// reads, and an export reads each batch once and writes it out. So the source is opened
/// directly, the bytes go through a [`Pipeline`] — which is what makes `--framing tm` work
/// here, and is why this does not call [`xtce_gs_engine::record::export_stream`], which reads
/// back-to-back space packets and nothing else — and each assembled packet is decoded and
/// written.
///
/// Streaming and never collecting: a two-hour pass is 7 200 packets a minute, and an export
/// that held them all would hold the pass.
///
/// An output that *closes* — `| head -1` — is a clean stop and not a failure; see
/// [`is_broken_pipe`]. A stream that ends in the middle of a packet is neither: the partial
/// is counted on `packets_lost` and named in the summary, and the exit stays 0, because
/// [`xtce_gs_engine::record::export_stream`] reports the same truncation as a field of its
/// summary rather than as an error and the two should not disagree.
///
/// # Errors
///
/// [`CliError::Engine`] for a definition that will not load, [`CliError::Link`] for a source
/// that will not open, [`CliError::Io`] for an output that cannot be written.
pub fn export(
    config: &SessionConfig,
    output: Option<&Path>,
    limit: Option<usize>,
    verbose: bool,
) -> Result<(), CliError> {
    let runtime = tokio::runtime::Runtime::new()?;

    // Loaded before the output is opened: a definition that will not parse should not leave a
    // truncated CSV behind.
    let db = XtceDb::from_path(&config.definition).map_err(EngineError::from)?;
    let decoder = match config.root_container.as_deref() {
        Some(name) => Decoder::with_root(&db, name),
        None => Decoder::new(&db),
    }
    .map_err(EngineError::from)?;
    let clock = match config.spacecraft_time.as_ref() {
        Some(source) => Some(SpacecraftClock::resolve(&db, source)?),
        None => None,
    };

    // Opened before the output is created, for the same reason the definition is loaded
    // before it: a stream that is not there should not leave a one-line CSV behind where a
    // pass used to be.
    let source = runtime.block_on(Source::connect(&config.source))?;

    let mut out: Box<dyn Write> = match output {
        Some(path) => Box::new(BufWriter::with_capacity(
            EXPORT_CHUNK_BYTES,
            std::fs::File::create(path)?,
        )),
        None => Box::new(BufWriter::with_capacity(
            EXPORT_CHUNK_BYTES,
            std::io::stdout(),
        )),
    };
    out.write_all(BATCH_HEADER.as_bytes())?;
    out.write_all(b"\n")?;

    let stats = Arc::new(LinkStats::new());
    let mut pipeline = Pipeline::new(config.pipeline.clone(), Arc::clone(&stats));
    let mut packets = PacketDecoder::new(clock, Arc::clone(&stats));
    let mut run = ExportRun::default();

    let result = runtime.block_on(export_loop(
        source,
        &decoder,
        &mut pipeline,
        &mut packets,
        &mut out,
        limit,
        verbose,
        &mut run,
    ));
    // Flushed before the result is examined, so that a failure part way through still leaves
    // the rows that were written on the far side of the buffer.
    if let Err(error) = out.flush() {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            run.closed = true;
        } else {
            return Err(error.into());
        }
    }
    result?;

    if run.closed {
        eprintln!(
            "export: the output closed after {} packets; stopping there",
            run.packets
        );
    }
    eprintln!(
        "export: {} packets, {} rows, {} refused",
        run.packets,
        run.rows,
        run.rejected_total()
    );
    for (kind, count) in &run.rejected {
        eprintln!("  {count} refused: {kind}");
    }
    let snapshot = stats.snapshot();
    if snapshot.packets_lost > 0 {
        // The same counter `probe` prints as "packets lost", and the same one a stream that
        // ended in the middle of a packet moves. Worded as the counter and not as the end of
        // the stream, because a framed link loses partial packets mid-pass too.
        eprintln!("  {} partial packet(s) abandoned", snapshot.packets_lost);
    }
    if matches!(config.pipeline.framing, Framing::TmFrames { .. }) {
        eprintln!(
            "  frames {}/{} ok, {} uncorrectable, {} checksum failures",
            snapshot.frames_ok,
            snapshot.frames_seen,
            snapshot.rs_uncorrectable,
            snapshot.crc_failures,
        );
    }
    if snapshot.sequence_gaps > 0 {
        eprintln!(
            "  {} sequence gaps, {} packets missing",
            snapshot.sequence_gaps, snapshot.sequence_missing
        );
    }
    Ok(())
}

/// The read-decode-write loop, with every argument the caller already owns.
#[allow(clippy::too_many_arguments)]
async fn export_loop(
    mut source: Source,
    decoder: &Decoder<'_>,
    pipeline: &mut Pipeline,
    packets: &mut PacketDecoder,
    out: &mut Box<dyn Write>,
    limit: Option<usize>,
    verbose: bool,
    run: &mut ExportRun,
) -> Result<(), CliError> {
    let mut bytes = Vec::with_capacity(EXPORT_CHUNK_BYTES);
    let mut assembled: Vec<RawPacket> = Vec::new();
    let mut batch = empty_batch();
    let mut next_progress = PROGRESS_PACKETS;

    loop {
        // Tested before the read, the way `xtce_gs_engine::record::export_stream` tests it at
        // the top of its own loop: `--limit 0` is "stop after none of them", and a check made
        // after the first packet was written exports one instead — the CLI disagreeing with
        // the library function that shares its name and its flag.
        if limit.is_some_and(|max| run.packets >= max as u64) {
            break;
        }
        bytes.clear();
        let read = source.read_chunk(&mut bytes).await?;
        if source.take_restart() {
            // A replay that wrapped, or a listener that took a new peer: the framing state and
            // the sequence counts from before the seam are both wrong after it.
            pipeline.flush_message();
            packets.reset();
        }
        if read == 0 {
            // The stream ended. CCSDS 133.0-B-2 §4.1.3.5.3 makes a packet's length
            // self-declared, so a stream that stopped in the middle of one is detectable
            // here — and this is the only place it can be reported, because what the
            // assembler is still holding otherwise goes away with the `Pipeline` and takes
            // the last packet of the pass with it, uncounted. The flush is what moves
            // `packets_lost` and queues the line saying how many octets went.
            pipeline.flush_message();
            break;
        }
        pipeline.push(&bytes, Utc::now(), &mut assembled);
        if source.is_datagram() {
            pipeline.flush_message();
        }

        for packet in assembled.drain(..) {
            if limit.is_some_and(|max| run.packets >= max as u64) {
                break;
            }
            run.packets = run.packets.saturating_add(1);
            match packets.decode_owned(decoder, &packet, &mut batch) {
                Ok(()) => {
                    match write_batch(out, &batch, decoder.db()) {
                        // Rows written, not samples offered: `write_batch` skips a sample
                        // whose parameter the definition cannot name, so the summary would
                        // otherwise claim more lines than the file has.
                        Ok(rows) => run.rows = run.rows.saturating_add(rows as u64),
                        Err(error) if is_broken_pipe(&error) => {
                            run.closed = true;
                            report_events(pipeline, packets, verbose);
                            return Ok(());
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) => run.reject(xtce_gs_engine::decode::error_kind(&error)),
            }
            if run.packets >= next_progress {
                eprintln!("export: {} packets, {} rows", run.packets, run.rows);
                next_progress = next_progress.saturating_add(PROGRESS_PACKETS);
            }
        }
        report_events(pipeline, packets, verbose);
    }
    report_events(pipeline, packets, verbose);
    Ok(())
}

/// Whether a failure is the reader at the other end of the output going away.
///
/// `xtce-gs export … | head -1` is an ordinary way to look at an export — this module's
/// header names redirection as what the subcommand is for — and Rust masks SIGPIPE, so a
/// write to a pipe nobody is reading comes back as `EPIPE` instead of ending the process.
/// Treating that as a failure gives exit 1 and `error: Broken pipe (os error 32)`, which
/// aborts any `set -e` script that samples the header of a CSV. Nothing is lost that the
/// operator asked for: the reader stopped listening first.
fn is_broken_pipe(error: &EngineError) -> bool {
    matches!(error, EngineError::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Puts whatever the link and the decoder had to say on stderr.
///
/// Drained every chunk whether or not it is printed: the queues are bounded, and a pipeline
/// that is never drained reports "lines dropped" for the life of the export.
fn report_events(pipeline: &mut Pipeline, packets: &mut PacketDecoder, verbose: bool) {
    for event in pipeline
        .take_events()
        .into_iter()
        .chain(packets.take_events())
    {
        if verbose || event.severity != Severity::Info {
            eprintln!(
                "{} {} {}: {}",
                event.time,
                event.severity.letter(),
                event.source,
                event.message
            );
        }
    }
}

/// What one APID looked like on the wire.
#[derive(Clone, Copy, Debug)]
struct ApidTally {
    /// Packets seen with this APID.
    count: u64,
    /// Shortest and longest packet, header included.
    shortest: usize,
    /// The longest one.
    longest: usize,
    /// The last sequence count seen, for the continuity check.
    last: Option<u16>,
    /// Times continuity broke.
    gaps: u64,
    /// Packets the counts say never arrived, summed over the gaps.
    missing: u64,
}

impl ApidTally {
    /// An APID nothing has been seen on.
    const EMPTY: Self = Self {
        count: 0,
        shortest: usize::MAX,
        longest: 0,
        last: None,
        gaps: 0,
        missing: 0,
    };

    /// Files one packet.
    fn observe(&mut self, length: usize, sequence: u16) {
        self.count = self.count.saturating_add(1);
        self.shortest = self.shortest.min(length);
        self.longest = self.longest.max(length);
        if let Some(previous) = self.last {
            let expected = (u32::from(previous) + 1) % SEQUENCE_MODULUS;
            if u32::from(sequence) != expected {
                self.gaps = self.gaps.saturating_add(1);
                let lost = (u32::from(sequence) + SEQUENCE_MODULUS - expected) % SEQUENCE_MODULUS;
                self.missing = self.missing.saturating_add(u64::from(lost));
            }
        }
        self.last = Some(sequence);
    }
}

/// Where the attached sync markers are, and how far apart.
///
/// Constant memory: the stride histogram and a three-byte carry, never the positions. A file
/// with a marker every kilobyte has one histogram entry; a file of random bytes has a marker
/// every four gigabytes and would have filled a `Vec`.
#[derive(Debug, Default)]
struct MarkerScan {
    /// The last three bytes of the previous chunk, so a marker across the seam is still found.
    carry: Vec<u8>,
    /// Bytes consumed so far.
    position: u64,
    /// Markers found.
    count: u64,
    /// Where the first one was.
    first: Option<u64>,
    /// Where the last one was, for the next stride.
    previous: Option<u64>,
    /// How many times each marker-to-marker distance occurred.
    strides: HashMap<u64, u64>,
}

impl MarkerScan {
    /// Scans one chunk, remembering just enough of it for the next.
    fn push(&mut self, bytes: &[u8]) {
        let mut window = std::mem::take(&mut self.carry);
        let carried = window.len() as u64;
        window.extend_from_slice(bytes);
        let base = self.position.saturating_sub(carried);

        for (offset, candidate) in window.windows(CCSDS_ASM.len()).enumerate() {
            if candidate != CCSDS_ASM {
                continue;
            }
            let at = base + offset as u64;
            self.count = self.count.saturating_add(1);
            if let Some(previous) = self.previous {
                *self.strides.entry(at.saturating_sub(previous)).or_insert(0) += 1;
            }
            self.previous = Some(at);
            if self.first.is_none() {
                self.first = Some(at);
            }
        }

        self.position = self.position.saturating_add(bytes.len() as u64);
        let keep = window.len().min(CCSDS_ASM.len() - 1);
        self.carry = window.split_off(window.len() - keep);
    }

    /// The most common marker-to-marker distance, and how many times it occurred.
    fn modal_stride(&self) -> Option<(u64, u64)> {
        self.strides
            .iter()
            .max_by_key(|(stride, count)| (**count, std::cmp::Reverse(**stride)))
            .map(|(stride, count)| (*stride, *count))
    }

    /// How many strides there were in total.
    fn stride_count(&self) -> u64 {
        self.strides.values().sum()
    }
}

/// Counts frames and packets in a file, with no definition.
///
/// This is the subcommand that is used when nothing works. A stream that produces no packets
/// under the given framing is the normal result, and the counters are what say whether the
/// frame length, the randomiser or the parity is the flag that is wrong — so every counter is
/// reported even when it is zero. Here, unlike on the status bar, a row of zeroes is the
/// answer.
///
/// Two passes over one read of the file. The first is whatever `pipeline` says the bytes are,
/// which defaults to back-to-back space packets; the second looks for the CCSDS attached sync
/// marker at any offset and reports the distances between the ones it finds, which is what
/// says whether the stream is transfer frames at all and at what length.
///
/// # Errors
///
/// [`CliError::Link`] for a file that will not open or a framing that cannot be used,
/// [`CliError::Io`] for a terminal that cannot be written to.
pub fn probe(
    source: &SourceSpec,
    pipeline: &PipelineConfig,
    limit: Option<u64>,
    sample: usize,
    verbose: bool,
) -> Result<(), CliError> {
    let runtime = tokio::runtime::Runtime::new()?;
    let stats = Arc::new(LinkStats::new());
    let mut stage = Pipeline::new(pipeline.clone(), Arc::clone(&stats));
    let mut apids = vec![ApidTally::EMPTY; APID_COUNT];
    let mut scan = MarkerScan::default();
    let mut first: Vec<(u16, u16, usize)> = Vec::new();
    let mut total = 0u64;

    runtime.block_on(async {
        let mut reader = Source::connect(source).await?;
        let mut bytes = Vec::new();
        let mut assembled: Vec<RawPacket> = Vec::new();
        loop {
            bytes.clear();
            let read = reader.read_chunk(&mut bytes).await?;
            if read == 0 {
                break;
            }
            if let Some(max) = limit {
                let room = max.saturating_sub(total);
                if room == 0 {
                    break;
                }
                bytes.truncate(usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len()));
            }
            total = total.saturating_add(bytes.len() as u64);

            scan.push(&bytes);
            stage.push(&bytes, Utc::now(), &mut assembled);
            if reader.is_datagram() {
                stage.flush_message();
            }
            for packet in assembled.drain(..) {
                let apid = packet.apid().unwrap_or_default();
                let sequence = packet.sequence_count().unwrap_or_default();
                if first.len() < sample {
                    first.push((apid, sequence, packet.len()));
                }
                if let Some(tally) = apids.get_mut(usize::from(apid)) {
                    tally.observe(packet.len(), sequence);
                }
            }
            if verbose {
                for event in stage.take_events() {
                    eprintln!(
                        "{} {} {}: {}",
                        event.time,
                        event.severity.letter(),
                        event.source,
                        event.message
                    );
                }
            }
        }
        Ok::<(), CliError>(())
    })?;

    let mut out = BufWriter::new(std::io::stdout());
    writeln!(out, "stream: {source}")?;
    writeln!(out, "read:   {total} bytes")?;
    report_counters(&mut out, &stats.snapshot())?;
    report_packets(&mut out, &apids, &first)?;
    report_frame_trial(&mut out, &scan, total)?;
    out.flush()?;
    Ok(())
}

/// Every link counter, including the zeroes.
fn report_counters<W: Write>(out: &mut W, snapshot: &StatsSnapshot) -> Result<(), CliError> {
    writeln!(out, "\nlink counters")?;
    for (name, value) in [
        ("bytes in", snapshot.bytes_in),
        ("sync found", snapshot.sync_found),
        ("sync lost", snapshot.sync_lost),
        ("frames seen", snapshot.frames_seen),
        ("frames ok", snapshot.frames_ok),
        ("frames dropped", snapshot.frames_dropped),
        ("rs corrected", snapshot.rs_corrected),
        ("rs symbols", snapshot.rs_symbols),
        ("rs uncorrectable", snapshot.rs_uncorrectable),
        ("crc failures", snapshot.crc_failures),
        ("idle frames", snapshot.idle_frames),
        ("idle packets", snapshot.idle_packets),
        ("packets in", snapshot.packets_in),
        ("packets lost", snapshot.packets_lost),
    ] {
        writeln!(out, "  {name:<18} {value}")?;
    }
    Ok(())
}

/// The APIDs that were seen, and the first few packets in the order they arrived.
fn report_packets<W: Write>(
    out: &mut W,
    apids: &[ApidTally],
    first: &[(u16, u16, usize)],
) -> Result<(), CliError> {
    let seen = apids.iter().filter(|tally| tally.count > 0).count();
    writeln!(out, "\napids: {seen}")?;
    if seen > 0 {
        writeln!(
            out,
            "  {:>5}  {:>9}  {:>6}  {:>6}  {:>6}  {:>8}",
            "apid", "packets", "min", "max", "gaps", "missing"
        )?;
    }
    for (apid, tally) in apids.iter().enumerate() {
        if tally.count == 0 {
            continue;
        }
        writeln!(
            out,
            "  {:>5}  {:>9}  {:>6}  {:>6}  {:>6}  {:>8}",
            apid, tally.count, tally.shortest, tally.longest, tally.gaps, tally.missing
        )?;
    }

    if !first.is_empty() {
        writeln!(out, "\nfirst {} packets", first.len())?;
        writeln!(out, "  {:>5}  {:>9}  {:>6}", "apid", "sequence", "length")?;
        for (apid, sequence, length) in first {
            writeln!(out, "  {apid:>5}  {sequence:>9}  {length:>6}")?;
        }
    }
    Ok(())
}

/// Where the attached sync markers are, and which frame length fits the distance between them.
///
/// The marker is CCSDS 131.0-B-5 §9.3's 0x1ACFFC1D — figure 9-1, the 32-bit pattern for
/// uncoded, convolutional, Reed-Solomon and concatenated data — and the distance between two
/// of them is the *codeword* plus the marker: the frame plus the Reed-Solomon parity, if
/// there is any. So each candidate is tried twice: as the distance itself, and as a frame
/// with the four marker octets in front of it.
///
/// Not 132.0-B-3: the TM Space Data Link Protocol says the start of a frame "is signaled by
/// the underlying Channel Coding Sublayer" (§4.1.1.2, note 3) and never prints the pattern.
fn report_frame_trial<W: Write>(
    out: &mut W,
    scan: &MarkerScan,
    total: u64,
) -> Result<(), CliError> {
    writeln!(out, "\ntransfer frames")?;
    writeln!(out, "  sync markers found: {}", scan.count)?;
    if scan.count == 0 {
        writeln!(
            out,
            "  no 0x1ACFFC1D in {total} bytes. Either this is not a transfer frame stream, or\n  \
             the markers were stripped by the receiver — try --framing tm --no-asm with a\n  \
             --frame-length the mission gave you."
        )?;
        return Ok(());
    }

    if let Some(at) = scan.first {
        writeln!(out, "  first marker at: {at}")?;
    }
    match scan.modal_stride() {
        Some((stride, count)) => writeln!(
            out,
            "  most common stride: {stride} bytes, {count} of {} intervals",
            scan.stride_count()
        )?,
        None => writeln!(out, "  only one marker: no stride to measure")?,
    }

    for candidate in FRAME_LENGTH_CANDIDATES {
        let bare = scan.strides.get(&candidate).copied().unwrap_or(0);
        let with_marker = candidate + CCSDS_ASM.len() as u64;
        let counted = scan.strides.get(&with_marker).copied().unwrap_or(0);
        writeln!(
            out,
            "  {candidate:>5}: {bare} intervals at {candidate}, {counted} at {with_marker} \
             (marker included)"
        )?;
    }

    // The advice comes from the measured stride and not from whichever candidate matched.
    // A stride is a marker-to-marker distance; a `--frame-length` is the transfer frame with
    // the marker and the parity taken off it, and only one of the two columns above is ever
    // the second thing.
    if let Some((stride, count)) = scan.modal_stride() {
        if count * 2 < scan.stride_count() {
            writeln!(
                out,
                "  the stride is not consistent: {count} of {} intervals. The markers are \
                 probably\n  in the data rather than between frames.",
                scan.stride_count()
            )?;
        }
        for line in advise(stride) {
            writeln!(out, "  {line}")?;
        }
    }
    Ok(())
}

/// Turns a measured marker-to-marker stride into the flags that would read it.
///
/// A stride is the attached sync marker plus one codeword block. CCSDS 131.0-B-5 §9.4 is what
/// makes that subtraction exact: the ASM "shall immediately precede" the codeblock and there
/// "shall be no intervening bits (data or fill)" before it, and §9.5.1 keeps it out of the
/// Reed-Solomon codeblock's encoded data space. CCSDS 131.0-B-5 section 4 makes the block a
/// whole number of 255-symbol codewords when the link is coded. So the block is the stride
/// less the four marker octets, and a block that divides by 255 is Reed-Solomon with the
/// quotient as the interleaving depth.
///
/// The parity cannot be recovered from the stride — E=8 and E=16 produce the same block
/// length and differ only in how much of it is the frame — so both are offered. Guessing one
/// would be guessing the frame length, and a frame length that is wrong by 80 octets per
/// codeword is a synchroniser that never finds a second marker.
fn advise(stride: u64) -> Vec<String> {
    let Some(block) = stride
        .checked_sub(CCSDS_ASM.len() as u64)
        .filter(|b| *b > 0)
    else {
        return vec![format!(
            "a stride of {stride} is shorter than the sync marker: these are not frames"
        )];
    };

    let interleave = block / CODEWORD_SYMBOLS;
    if block % CODEWORD_SYMBOLS == 0 && CCSDS_INTERLEAVE.contains(&interleave) {
        return vec![
            format!(
                "{block} octets between markers is {interleave} x 255 \
                 (CCSDS 131.0-B-5 section 4): a Reed-Solomon coded link."
            ),
            format!(
                "try --framing tm --rs 32 --interleave {interleave} --frame-length {}",
                interleave * (CODEWORD_SYMBOLS - 32)
            ),
            format!(
                "  or --framing tm --rs 16 --interleave {interleave} --frame-length {} \
                 (E=8 instead of E=16)",
                interleave * (CODEWORD_SYMBOLS - 16)
            ),
        ];
    }

    vec![
        format!("{block} octets between markers is not a multiple of 255: an uncoded link."),
        format!("try --framing tm --frame-length {block}"),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use xtce_gs_core::Event;

    use super::*;

    /// A definition with one container and one 32-bit parameter.
    ///
    /// Written from the test rather than read out of the sibling `xtce-rs` checkout the way
    /// the engine's and the link crate's fixtures are: a test that skips depending on what
    /// else the developer happens to have cloned is a test that stops being run, and nothing
    /// below needs a mission — what is under test is where the export loop stops and what it
    /// reports, not what the values mean.
    const MINIMAL_XTCE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xtce:SpaceSystem name="TestSystem"
                  xmlns:xtce="http://www.omg.org/spec/XTCE/20180204">
  <xtce:TelemetryMetaData>
    <xtce:ParameterTypeSet>
      <xtce:IntegerParameterType name="UINT32" signed="false">
        <xtce:IntegerDataEncoding sizeInBits="32" encoding="unsigned"/>
      </xtce:IntegerParameterType>
    </xtce:ParameterTypeSet>
    <xtce:ParameterSet>
      <xtce:Parameter name="PKT_VALUE" parameterTypeRef="UINT32"/>
    </xtce:ParameterSet>
    <xtce:ContainerSet>
      <xtce:SequenceContainer name="PKT_CONTAINER">
        <xtce:EntryList>
          <xtce:ParameterRefEntry parameterRef="PKT_VALUE"/>
        </xtce:EntryList>
      </xtce:SequenceContainer>
    </xtce:ContainerSet>
  </xtce:TelemetryMetaData>
</xtce:SpaceSystem>"#;

    /// A directory of this run's own, named after the test that asked for it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtce-gs-cli-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `count` back-to-back space packets on APID 100, each carrying `payload` octets.
    ///
    /// CCSDS 133.0-B-2 §4.1.3.5.3: the length field is one less than the number of octets
    /// after the six-octet primary header, which is what makes a truncated tail detectable.
    fn space_packets(count: u16, payload: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for sequence in 0..count {
            bytes.extend_from_slice(&100u16.to_be_bytes());
            bytes.extend_from_slice(&(0xC000 | sequence).to_be_bytes());
            bytes.extend_from_slice(&((payload - 1) as u16).to_be_bytes());
            bytes.extend((0..payload).map(|i| (sequence as usize + i) as u8));
        }
        bytes
    }

    /// The far end of `| head -1`: every write is `EPIPE`, as a closed pipe's is.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Broken pipe (os error 32)",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs [`export_loop`] over `bytes` with everything [`export`] would give it.
    fn export_bytes(
        name: &str,
        bytes: &[u8],
        limit: Option<usize>,
        out: Box<dyn Write>,
    ) -> (Result<(), CliError>, ExportRun, StatsSnapshot) {
        let dir = scratch(name);
        let definition = dir.join("mission.xml");
        std::fs::write(&definition, MINIMAL_XTCE).unwrap();
        let path = dir.join("stream.bin");
        std::fs::write(&path, bytes).unwrap();

        let db = XtceDb::from_path(&definition).unwrap();
        let decoder = Decoder::new(&db).unwrap();
        let stats = Arc::new(LinkStats::new());
        let mut pipeline = Pipeline::new(PipelineConfig::default(), Arc::clone(&stats));
        let mut packets = PacketDecoder::new(None, Arc::clone(&stats));
        let mut run = ExportRun::default();
        let mut out = out;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let spec = SourceSpec::File {
            path,
            bytes_per_second: None,
            chunk: 64,
            repeat: false,
        };
        let source = runtime.block_on(Source::connect(&spec)).unwrap();
        let result = runtime.block_on(export_loop(
            source,
            &decoder,
            &mut pipeline,
            &mut packets,
            &mut out,
            limit,
            false,
            &mut run,
        ));
        (result, run, stats.snapshot())
    }

    /// One line into a log behind a mutex.
    fn push(log: &Mutex<EventLog>, event: Event) {
        log.lock().unwrap().push(event);
    }

    #[test]
    fn a_sequence_count_that_wraps_is_not_a_gap() {
        // CCSDS 133.0-B-2 section 4.1.2.4.2: the count is fourteen bits and 16 383 is
        // followed by 0. A tracker that subtracted would call every wrap a loss of 16 383
        // packets, once every 16 384 — which on a 1 Hz housekeeping APID is once every four
        // and a half hours, and looks exactly like a real outage.
        let mut tally = ApidTally::EMPTY;
        tally.observe(71, 16_382);
        tally.observe(71, 16_383);
        tally.observe(71, 0);
        tally.observe(71, 1);
        assert_eq!(tally.count, 4);
        assert_eq!(tally.gaps, 0);
        assert_eq!(tally.missing, 0);
    }

    #[test]
    fn a_gap_counts_the_packets_it_implies_and_not_only_itself() {
        let mut tally = ApidTally::EMPTY;
        tally.observe(71, 10);
        tally.observe(71, 14);
        assert_eq!(tally.gaps, 1);
        assert_eq!(tally.missing, 3, "11, 12 and 13 never arrived");
    }

    #[test]
    fn a_gap_across_the_wrap_counts_forwards() {
        let mut tally = ApidTally::EMPTY;
        tally.observe(71, 16_380);
        tally.observe(71, 2);
        assert_eq!(tally.gaps, 1);
        // 16 381, 16 382, 16 383, 0, 1 — five, not a negative number and not 16 378.
        assert_eq!(tally.missing, 5);
    }

    #[test]
    fn the_first_packet_of_an_apid_is_never_a_gap() {
        let mut tally = ApidTally::EMPTY;
        tally.observe(71, 9_000);
        assert_eq!(tally.gaps, 0);
        assert_eq!(tally.shortest, 71);
        assert_eq!(tally.longest, 71);
    }

    #[test]
    fn a_marker_split_across_two_chunks_is_still_found() {
        // The whole reason `MarkerScan` carries three bytes. A scanner that searched each
        // chunk on its own misses one marker in every `chunk / 1119` on a framed link, which
        // reads as a link that keeps losing lock.
        let mut scan = MarkerScan::default();
        scan.push(&[0x00, 0x00, 0x1A, 0xCF]);
        scan.push(&[0xFC, 0x1D, 0x00]);
        assert_eq!(scan.count, 1);
        assert_eq!(scan.first, Some(2));
    }

    #[test]
    fn a_marker_is_not_counted_twice_at_a_seam() {
        let mut scan = MarkerScan::default();
        scan.push(&[0x1A, 0xCF, 0xFC, 0x1D]);
        scan.push(&[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(scan.count, 1);
        assert_eq!(scan.stride_count(), 0, "one marker has no stride");
        assert_eq!(scan.modal_stride(), None);
    }

    #[test]
    fn strides_are_measured_marker_to_marker() {
        let mut scan = MarkerScan::default();
        let mut stream = Vec::new();
        for _ in 0..3 {
            stream.extend_from_slice(&CCSDS_ASM);
            stream.extend_from_slice(&[0x00; 10]);
        }
        // Dribbled in one byte at a time: the seam handling must not depend on where the
        // reads happen to fall, and a 4 096-byte chunk hides a carry bug on a 14-byte stride.
        for byte in &stream {
            scan.push(&[*byte]);
        }
        assert_eq!(scan.count, 3);
        assert_eq!(scan.modal_stride(), Some((14, 2)));
    }

    #[test]
    fn an_empty_stream_has_nothing_to_say_and_does_not_panic() {
        let mut scan = MarkerScan::default();
        scan.push(&[]);
        scan.push(&[0x1A, 0xCF]);
        scan.push(&[]);
        assert_eq!(scan.count, 0);
        assert_eq!(scan.modal_stride(), None);
        assert_eq!(scan.position, 2);
    }

    #[test]
    fn a_stream_shorter_than_a_marker_is_not_a_marker() {
        let mut scan = MarkerScan::default();
        scan.push(&[0x1A, 0xCF, 0xFC]);
        assert_eq!(scan.count, 0);
    }

    #[test]
    fn a_coded_stride_is_advised_as_a_frame_length_and_not_as_itself() {
        // 1279 is in `FRAME_LENGTH_CANDIDATES` *because* it is the stride of RS(255, 223)
        // interleaved 5 deep: 4 marker octets + 5 x 255. An operator handed 1279 as a
        // `--frame-length` gets a synchroniser expecting markers 1283 apart, which locks once
        // and then finds nothing — the exact failure this subcommand exists to prevent.
        let advice = advise(1279).join("\n");
        assert!(advice.contains("--rs 32"), "{advice}");
        assert!(advice.contains("--interleave 5"), "{advice}");
        assert!(advice.contains("--frame-length 1115"), "{advice}");
        assert!(
            !advice.contains("--frame-length 1279"),
            "the stride was handed back as a frame length: {advice}"
        );
        // E=8 over the same block is a 1195-octet frame, and the stride cannot tell them
        // apart, so both are offered.
        assert!(advice.contains("--frame-length 1195"), "{advice}");
    }

    #[test]
    fn an_uncoded_stride_is_the_frame_plus_the_marker() {
        let advice = advise(1119).join("\n");
        assert!(advice.contains("--frame-length 1115"), "{advice}");
        assert!(
            !advice.contains("--rs"),
            "1115 is not 255 x anything: {advice}"
        );
    }

    #[test]
    fn a_block_that_divides_by_255_into_a_depth_ccsds_does_not_define_is_not_a_code() {
        // 255 x 6. CCSDS 131.0-B-5 section 4 defines I = 1, 2, 3, 4, 5 and 8, so this is a
        // coincidence and advising `--interleave 6` would be advising a code the link crate
        // refuses.
        let advice = advise(4 + 255 * 6).join("\n");
        assert!(!advice.contains("--rs"), "{advice}");
        assert!(advice.contains("--frame-length 1530"), "{advice}");
    }

    #[test]
    fn a_stride_shorter_than_the_marker_is_not_a_frame() {
        let advice = advise(3).join("\n");
        assert!(advice.contains("not frames"), "{advice}");
        assert!(!advice.contains("--frame-length"), "{advice}");
        // And the boundary itself: a stride of exactly the marker leaves no frame.
        assert!(advise(4).join("\n").contains("not frames"));
    }

    #[test]
    fn the_status_line_hides_the_frame_counters_on_an_unframed_link() {
        let mut line = String::new();
        let snapshot = StatsSnapshot {
            bytes_in: 511_200,
            packets_in: 7200,
            packets_decoded: 7200,
            ..StatsSnapshot::default()
        };

        status_line(&mut line, &snapshot, false);
        assert!(line.contains("bytes=511200"), "{line}");
        assert!(line.contains("packets=7200"), "{line}");
        assert!(
            !line.contains("frames="),
            "a packet link has no frames: {line}"
        );
        assert!(line.ends_with("last=never"), "{line}");

        status_line(&mut line, &snapshot, true);
        assert!(line.contains("frames=0/0"), "{line}");
        assert!(
            line.contains("loss=0.00%"),
            "loss over nothing is 0, not 100: {line}"
        );
    }

    #[test]
    fn the_status_line_is_reused_and_not_appended_to() {
        let mut line = String::new();
        status_line(&mut line, &StatsSnapshot::default(), false);
        let once = line.len();
        status_line(&mut line, &StatsSnapshot::default(), false);
        assert_eq!(line.len(), once, "the buffer grew: {line}");
    }

    #[test]
    fn refusals_are_counted_by_kind() {
        let mut run = ExportRun::default();
        run.reject("no container matches");
        run.reject("packet too short");
        run.reject("no container matches");
        assert_eq!(run.rejected_total(), 3);
        assert_eq!(
            run.rejected,
            vec![("no container matches", 2), ("packet too short", 1)]
        );
    }

    // ---------------------------------------------------------------- events

    #[test]
    fn two_events_at_the_same_instant_are_both_printed() {
        // `Utc::now()` is `SystemTime::now()`, whose granularity here is about a microsecond,
        // so two events raised back to back routinely carry the same stamp: 93 258 of 100 000
        // pairs, measured on this machine (see `EventCursor`). A printer that skipped
        // `time <= cursor` dropped the second one and, because the cursor only moves forward,
        // dropped it forever. The line that costs an operator most is the last one:
        // `headless` prints once more after `shutdown`, and that is the line saying why the
        // session stopped.
        let stamp = Utc::now();
        let log = Mutex::new(EventLog::with_capacity(8));
        let mut cursor = EventCursor::default();

        push(
            &log,
            Event::at(
                stamp,
                Severity::Warning,
                "link",
                "frame synchronisation lost",
            ),
        );
        let first = unprinted_events(&log, &mut cursor, false);
        push(
            &log,
            Event::at(stamp, Severity::Error, "session", "the decode thread ended"),
        );
        let second = unprinted_events(&log, &mut cursor, false);

        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(
            second.len(),
            1,
            "an event whose stamp ties the newest already printed was dropped: {second:?}"
        );
        assert!(second[0].contains("the decode thread ended"), "{second:?}");
    }

    #[test]
    fn a_collapsed_line_reprints_only_when_its_count_has_risen() {
        // `EventLog::push` folds a repeated line into the one before it, moving its time
        // forward and bumping the count. Every stamp here is the same instant, so the only
        // thing that changes on a repeat is the count — which is what the printer has to
        // notice, and what a time cursor cannot.
        let stamp = Utc::now();
        let log = Mutex::new(EventLog::with_capacity(8));
        let mut cursor = EventCursor::default();
        let crc = || Event::at(stamp, Severity::Warning, "frame", "CRC failed");

        push(&log, crc());
        let once = unprinted_events(&log, &mut cursor, false);
        assert_eq!(once.len(), 1, "{once:?}");
        assert!(!once[0].contains("(x"), "one is not a repeat: {once:?}");

        assert!(
            unprinted_events(&log, &mut cursor, false).is_empty(),
            "an unchanged line printed twice"
        );

        push(&log, crc());
        push(&log, crc());
        let again = unprinted_events(&log, &mut cursor, false);
        assert_eq!(again.len(), 1, "{again:?}");
        assert!(
            again[0].contains("(x3)"),
            "the count did not rise: {again:?}"
        );
        assert!(
            unprinted_events(&log, &mut cursor, false).is_empty(),
            "the repeat printed twice at the same count"
        );
    }

    #[test]
    fn an_info_line_is_held_back_without_hiding_the_warning_behind_it() {
        let stamp = Utc::now();
        let log = Mutex::new(EventLog::with_capacity(8));
        let mut cursor = EventCursor::default();

        push(
            &log,
            Event::at(stamp, Severity::Info, "link", "reading file://pass.dat"),
        );
        push(
            &log,
            Event::at(
                stamp,
                Severity::Warning,
                "link",
                "1 partial packet(s) abandoned",
            ),
        );
        let quiet = unprinted_events(&log, &mut cursor, false);
        assert_eq!(quiet.len(), 1, "{quiet:?}");
        assert!(quiet[0].contains("partial packet"), "{quiet:?}");
    }

    #[test]
    fn a_line_the_ring_has_evicted_does_not_reprint_the_ones_behind_it() {
        // The cursor is a count of pushes, and eviction must not renumber what is left: the
        // pushes an evicted entry accounted for stay counted in `EventLog::total`.
        let stamp = Utc::now();
        let log = Mutex::new(EventLog::with_capacity(2));
        let mut cursor = EventCursor::default();

        push(&log, Event::at(stamp, Severity::Warning, "link", "one"));
        push(&log, Event::at(stamp, Severity::Warning, "link", "two"));
        assert_eq!(unprinted_events(&log, &mut cursor, false).len(), 2);

        push(&log, Event::at(stamp, Severity::Warning, "link", "three"));
        let after = unprinted_events(&log, &mut cursor, false);
        assert_eq!(after.len(), 1, "{after:?}");
        assert!(after[0].contains("three"), "{after:?}");
    }

    // ---------------------------------------------------------------- export

    #[test]
    fn a_stream_that_ends_mid_packet_is_counted_and_not_dropped() {
        // CCSDS 133.0-B-2 §4.1.3.5.3 makes a packet's length self-declared, so a stream that
        // stops in the middle of one is detectable at end of file and has to be reported:
        // an export that drops the tail with the `Pipeline` loses the last packet of every
        // pass and says nothing — no counter, no event, exit 0.
        let mut bytes = space_packets(10, 4);
        bytes.extend_from_slice(&space_packets(1, 4)[..8]);
        let (result, run, stats) = export_bytes("truncated", &bytes, None, Box::new(Vec::new()));

        result.expect("a truncated stream is a report, not a failure");
        assert_eq!(run.packets, 10, "the ten whole packets still arrive");
        assert_eq!(
            stats.packets_lost, 1,
            "the eleventh packet's 8 octets went nowhere and nothing counted them"
        );
    }

    #[test]
    fn a_stream_that_ends_on_a_packet_boundary_loses_nothing() {
        let bytes = space_packets(10, 4);
        let (result, run, stats) = export_bytes("whole", &bytes, None, Box::new(Vec::new()));

        result.expect("a clean end of file is not a failure");
        assert_eq!(run.packets, 10);
        assert_eq!(
            stats.packets_lost, 0,
            "a stream that ends where a packet ends has no partial to abandon"
        );
    }

    #[test]
    fn a_limit_of_zero_exports_nothing() {
        // `xtce_gs_engine::record::export_stream`, which shares this subcommand's name and
        // its `--limit`, tests the count at the top of its loop and exports none for
        // `Some(0)`. A CLI that exports one packet for `--limit 0` and one for `--limit 1`
        // disagrees with the library and with the flag's own help text.
        let bytes = space_packets(10, 4);
        let (result, run, _) = export_bytes("limit-zero", &bytes, Some(0), Box::new(Vec::new()));

        result.expect("a limit of zero is a legal limit");
        assert_eq!(run.packets, 0, "--limit 0 exported a packet");
        assert_eq!(run.rows, 0);
    }

    #[test]
    fn a_limit_stops_the_export_where_it_says() {
        let bytes = space_packets(10, 4);
        let (result, run, _) = export_bytes("limit-three", &bytes, Some(3), Box::new(Vec::new()));

        result.expect("three packets is a legal limit");
        assert_eq!(run.packets, 3);
    }

    #[test]
    fn an_output_that_closed_early_ends_the_export_rather_than_failing_it() {
        // `xtce-gs export … | head -1` is an ordinary way to look at an export — the module
        // header names redirection as what this subcommand is for — and Rust masks SIGPIPE,
        // so the write comes back as EPIPE rather than killing the process. Treating that as
        // a failure gives exit 1 and `error: Broken pipe (os error 32)`, which aborts any
        // `set -e` script that samples the header of a CSV.
        let bytes = space_packets(10, 4);
        let (result, run, _) = export_bytes("closed-pipe", &bytes, None, Box::new(ClosedPipe));

        result.expect("a reader that went away is a clean stop, not a failure");
        assert!(run.closed, "the summary would not say the output closed");
        assert!(
            run.packets < 10,
            "the export ran to the end of a stream nothing was reading: {}",
            run.packets
        );
    }

    // ---------------------------------------------------------------- signals

    #[cfg(unix)]
    #[test]
    fn a_supervisors_sigterm_is_a_stop_and_not_a_kill() {
        // `headless` is "a recorder writing bytes, checked on over ssh" — the process systemd,
        // docker and a `kill` in a script stop with SIGTERM. A signal left at its default
        // disposition kills the process outright, and a process killed by a signal runs no
        // destructor: the recorder's buffered tail never reaches the file and
        // `Recorder::drop`'s safety net never runs. See `stop_requested` for the measurement.
        //
        // This signals its own process. With no handler registered the default disposition
        // applies and the whole test binary dies, which is the failure being pinned.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let stop = stop_requested();
            tokio::pin!(stop);
            // Polled once first: the handler is registered when the future is first polled,
            // and a signal that lands before that is a signal nothing is catching.
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut stop)
                    .await
                    .is_err(),
                "something asked for a stop before the test did"
            );

            let killed = std::process::Command::new("/bin/kill")
                .arg("-TERM")
                .arg(std::process::id().to_string())
                .status()
                .expect("/bin/kill");
            assert!(killed.success());

            tokio::time::timeout(Duration::from_secs(5), &mut stop)
                .await
                .expect("SIGTERM did not end the wait")
                .expect("SIGTERM came back as an error");
        });
    }
}
