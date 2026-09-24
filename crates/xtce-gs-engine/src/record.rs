//! The two files a session writes: the bytes that arrived, and a table of what they meant.
//!
//! A recording is every byte the source produced, in order, with nothing added — no index, no
//! per-packet wrapper, no header. That is what makes it replayable through the same
//! [`xtce_gs_link::Source`] the live pass came through, and it is the reason this workspace
//! has no archive format: a file of bytes and the definition that describes them is the whole
//! archive.
//!
//! The CSV side is the opposite: it is for something that is not this program. Columns are
//! fixed here so that a spreadsheet built on one export still opens the next one.
//!
//! # Which CSV shape, and why there are three
//!
//! An operator asks two different questions of an export and they do not fit one table.
//!
//! *What is the spacecraft doing now* is one row per **parameter** — the newest value of
//! everything, wide enough to read down: [`SNAPSHOT_HEADER`] and [`write_snapshot`]. It is
//! the table a shift hand-off is written from, and it carries the unit and the limit state
//! because both are about the value that is in front of the operator.
//!
//! *What did the spacecraft do* is one row per **sample** — [`BATCH_HEADER`] and
//! [`write_batch`], the long form every spreadsheet and every plotting tool pivots from. This
//! is what `xtce-gs export` writes, because an export of a pass is read by something that
//! groups it, and a wide table cannot be streamed: the columns would not be known until the
//! last packet. [`HISTORY_HEADER`] is the same shape narrowed to what is watched, for the
//! interface's "save this plot" — a history exists only for a watched parameter.
//!
//! The batch rows carry no limit column, and that is not an omission: the export path never
//! loads a [`LimitSet`] — `xtce-gs export` has no `--limits` flag — so a limit column there
//! would be a column of `—`, which reads as "in limits" to anyone who did not write it.
//!
//! Escaping is RFC 4180 §2 and is written by hand: there is no `csv` crate in this workspace
//! and there will not be one. See [`write_field`].

use std::fmt;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt as _;
use xtce_decode::ccsds::PRIMARY_HEADER_BYTES;
use xtce_decode::{Decoder, SpacePacketBytes};
use xtce_gs_core::{Batch, LimitSet, ParameterStore, Utc};
use xtce_model::{ParamId, XtceDb};

use crate::error::EngineError;

/// Columns of a store snapshot.
pub const SNAPSHOT_HEADER: &str = "parameter,time,raw,eng,unit,updates,limit";

/// Columns of a watched parameter's history.
pub const HISTORY_HEADER: &str = "parameter,time,value";

/// Columns of a decoded batch.
///
// TODO(gs-engine-record-columns): no `unit` and no `limit state`. Both belong in an export
// and both are left out, for reasons that are defensible and are not the same reason. The unit
// is a `Vec<NameId>` on the parameter *type* — XTCE allows compound units — so a column would
// need a rule for joining several, and joining them with a comma inside a CSV is exactly the
// kind of thing this module escapes against. The limit state is not a property of the packet
// at all: it depends on a `LimitSet` that `write_batch` is not given, and an export whose
// limit column silently reads "unknown" because no `--limits` was passed is worse than no
// column. Adding them means a `&LimitSet` argument and a decision on the unit separator —
// `·` is what the interface uses, and it is not an ASCII character.
pub const BATCH_HEADER: &str = "time,received,apid,sequence,parameter,raw,eng";

/// Bytes a [`Recorder`] holds before it writes them out.
///
/// A 1 Mbit/s link delivers a few thousand datagrams a second; one write syscall each is
/// thousands of syscalls a second on the same thread that is reading the socket. 64 KiB is
/// about half a second of that link and one write on a spinning disk either way.
pub const RECORD_BUFFER_BYTES: usize = 64 * 1024;

/// How long a [`Recorder`] holds a partial buffer before writing it out anyway.
///
/// The size bound alone would leave the last few kilobytes of a quiet link unwritten for as
/// long as the link stays quiet, which is exactly when an operator goes looking at the file
/// to see whether anything is arriving at all.
pub const RECORD_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Bytes [`export_stream`] reads from the stream at a time.
pub const EXPORT_CHUNK_BYTES: usize = 64 * 1024;

/// Appends every received byte to a file.
///
/// Owned by the acquisition task and written from it, which is why the file is a
/// [`tokio::fs::File`]: a blocking write on a runtime worker stalls every other task on that
/// worker, and a recording is the one thing here that writes as fast as the link reads.
///
/// Writes are buffered to [`RECORD_BUFFER_BYTES`] and flushed on that bound or on
/// [`RECORD_FLUSH_INTERVAL`], whichever comes first — a bound on bytes alone loses the tail
/// of a quiet link, and a bound on time alone is a syscall per datagram on a loud one.
#[derive(Debug)]
pub struct Recorder {
    file: tokio::fs::File,
    path: PathBuf,
    bytes: u64,
    buffer: Vec<u8>,
    last_flush: Instant,
}

impl Recorder {
    /// Opens `path` for appending.
    ///
    /// Opened with `append(true).create(true)` and never `truncate`. Appending to an existing
    /// recording is the failure that costs nothing — the file is then two passes back to
    /// back, which still replays, and the packets say where the seam is. Truncating is the
    /// failure that costs a pass nobody can get back.
    ///
    /// # Errors
    ///
    /// [`EngineError::Io`] when the file cannot be created or opened.
    pub async fn open(path: &Path) -> Result<Self, EngineError> {
        let file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .await?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            bytes: 0,
            buffer: Vec::with_capacity(RECORD_BUFFER_BYTES),
            last_flush: Instant::now(),
        })
    }

    /// The file being written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many bytes this recorder has written.
    ///
    /// Counted as they are accepted, so the last few kilobytes may still be in the buffer;
    /// the number answers "is this recording growing", which is what the status line asks.
    #[must_use]
    pub const fn bytes_written(&self) -> u64 {
        self.bytes
    }

    /// How many accepted bytes are not on the file yet.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Appends bytes to the recording.
    ///
    /// Buffered: there is no write syscall per call and no `fsync` anywhere, because the
    /// recording is written from the same task that reads the socket and a sync per datagram
    /// turns a 2 Mbit/s link into whatever the disk will do per syscall.
    ///
    /// # Errors
    ///
    /// [`EngineError::Io`] when the write fails. The caller stops recording and says so on
    /// the log; it does *not* stop the session — a full disk should not end a pass.
    //
    // TODO(gs-engine-record): the time bound is only tested when something is written, so a
    // link that goes silent with a half-full buffer keeps its tail until the next datagram or
    // until shutdown. Nothing is lost — [`Recorder::flush`] and [`Drop`] both write it — but
    // an operator tailing the file sees it stop. Closing that means the acquisition loop
    // calling `flush()` on the timeout arm of its `select!`, which is `session.rs`'s to add:
    // a timer inside this type would need the recorder to own a `tokio::time::Interval` and
    // to be polled by something, and nothing polls it.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        self.buffer.extend_from_slice(bytes);
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if self.buffer.len() >= RECORD_BUFFER_BYTES
            || self.last_flush.elapsed() >= RECORD_FLUSH_INTERVAL
        {
            self.flush().await?;
        }
        Ok(())
    }

    /// Flushes what is buffered.
    ///
    /// Called when the session ends, and nowhere per packet — see [`Recorder::write`].
    ///
    /// The `flush` on the file is not decoration: [`tokio::fs::File`] dispatches its write to
    /// a blocking thread and only `flush` awaits that thread, so without it a write can still
    /// be in flight when this returns — and [`Recorder`]'s own [`Drop`] appends the tail
    /// through a second handle on the same path, which would then land ahead of it.
    ///
    /// # Errors
    ///
    /// [`EngineError::Io`] when the flush fails. The buffer is dropped either way, so a
    /// failed flush loses those bytes rather than writing them twice.
    pub async fn flush(&mut self) -> Result<(), EngineError> {
        self.last_flush = Instant::now();
        if !self.buffer.is_empty() {
            let written = self.file.write_all(&self.buffer).await;
            self.buffer.clear();
            written?;
        }
        self.file.flush().await?;
        Ok(())
    }
}

impl Drop for Recorder {
    /// Writes the tail, synchronously, if the session did not flush.
    ///
    /// A safety net and not the plan: the acquisition task is expected to call
    /// [`Recorder::flush`] on shutdown. `Drop` cannot await, so the tail — at most
    /// [`RECORD_BUFFER_BYTES`] — goes out through a plain [`std::fs::File`] opened on the
    /// same path in append mode. Safe to order after the async writes because
    /// [`Recorder::flush`] leaves nothing in flight. Errors have nowhere to go and are
    /// dropped: the alternative is a panic in `Drop`.
    fn drop(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
        {
            let _ = file.write_all(&self.buffer);
            let _ = file.flush();
        }
    }
}

/// Writes one CSV field, quoting it if it needs quoting.
///
/// RFC 4180 §2: a field containing a comma, a double quote, a carriage return or a line feed
/// is enclosed in double quotes and every quote inside it is doubled; anything else is
/// written as it is, spaces included — §2 makes a leading or trailing space part of the
/// field, not a reason to quote.
///
/// This is not optional prettiness. An enumeration label reading `SAFE, NO COMMS` is one
/// column in every spreadsheet that reads an unquoted export, and the row after it is shifted
/// by one for good.
///
/// Public because the writers below share it and a caller adding a column of its own needs
/// the same escaping.
///
/// # Errors
///
/// [`EngineError::Io`] from the underlying writer.
pub fn write_field<W: Write>(out: &mut W, field: &str) -> Result<(), EngineError> {
    if !field.contains([',', '"', '\r', '\n']) {
        out.write_all(field.as_bytes())?;
        return Ok(());
    }
    out.write_all(b"\"")?;
    let mut rest = field;
    while let Some(at) = rest.find('"') {
        // `"` is ASCII, so `at` is a character boundary and the split cannot panic.
        let (before, after) = rest.split_at(at);
        out.write_all(before.as_bytes())?;
        out.write_all(b"\"\"")?;
        rest = after.get(1..).unwrap_or("");
    }
    out.write_all(rest.as_bytes())?;
    out.write_all(b"\"")?;
    Ok(())
}

/// Formats a value into `scratch` and writes it as one escaped field.
///
/// One reused `String` for the whole export rather than a `to_string` per column: a pass is
/// tens of thousands of rows of seven columns.
fn write_display<W: Write, D: fmt::Display>(
    out: &mut W,
    scratch: &mut String,
    value: &D,
) -> Result<(), EngineError> {
    scratch.clear();
    // Formatting into a `String` cannot fail; there is no error here to report.
    let _ = write!(scratch, "{value}");
    write_field(out, scratch)
}

/// Writes a parameter's engineering units as one field.
///
/// XTCE permits compound units — `<UnitSet>` is a list — so they are joined with a space, the
/// way they are written in a definition's own descriptions. A parameter whose type declares
/// none gets an empty field rather than a placeholder: a unit nobody declared is not `—`.
fn write_unit<W: Write>(
    out: &mut W,
    scratch: &mut String,
    db: &XtceDb,
    parameter: ParamId,
) -> Result<(), EngineError> {
    scratch.clear();
    if let Some(kind) = db.type_of(parameter) {
        for (index, unit) in kind.units.iter().enumerate() {
            if index > 0 {
                scratch.push(' ');
            }
            scratch.push_str(db.name(*unit));
        }
    }
    write_field(out, scratch)
}

/// The four columns every row of one packet repeats: where it came from and when.
#[derive(Clone, Copy, Debug)]
struct PacketStamp {
    /// Spacecraft time when the packet carried one, ground receipt otherwise.
    time: Utc,
    /// When the ground received the packet.
    received: Utc,
    /// Application process identifier from the primary header.
    apid: u16,
    /// Sequence count from the primary header.
    sequence: u16,
}

/// Writes one row in [`BATCH_HEADER`]'s columns.
fn write_sample_row<W: Write, R: fmt::Display, E: fmt::Display>(
    out: &mut W,
    scratch: &mut String,
    stamp: PacketStamp,
    name: &str,
    raw: &R,
    eng: &E,
) -> Result<(), EngineError> {
    write_display(out, scratch, &stamp.time)?;
    out.write_all(b",")?;
    write_display(out, scratch, &stamp.received)?;
    out.write_all(b",")?;
    write_display(out, scratch, &stamp.apid)?;
    out.write_all(b",")?;
    write_display(out, scratch, &stamp.sequence)?;
    out.write_all(b",")?;
    write_field(out, name)?;
    out.write_all(b",")?;
    write_display(out, scratch, raw)?;
    out.write_all(b",")?;
    write_display(out, scratch, eng)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// The qualified name of a parameter, as the definition spells it.
fn qualified_name(db: &XtceDb, parameter: ParamId) -> Option<&str> {
    db.parameter(parameter)
        .map(|declared| db.name(declared.qualified_name))
}

/// Writes the latest value of everything the store has seen.
///
/// One row per parameter, newest value, in [`SNAPSHOT_HEADER`]'s columns. Only parameters
/// that have arrived are written — [`ParameterStore::seen`] gives them, and a store on CTIM
/// has 9 493 slots of which a pass fills a few hundred, so iterating `seen` rather than every
/// index is the difference between an export and a file of commas.
///
/// # Errors
///
/// [`EngineError::Io`] from the underlying writer.
pub fn write_snapshot<W: Write>(
    out: &mut W,
    store: &ParameterStore,
    db: &XtceDb,
    limits: &LimitSet,
) -> Result<(), EngineError> {
    out.write_all(SNAPSHOT_HEADER.as_bytes())?;
    out.write_all(b"\n")?;
    let mut scratch = String::new();
    for parameter in store.seen() {
        let (Some(name), Some(sample)) = (qualified_name(db, parameter), store.latest(parameter))
        else {
            // A store built for a different definition than the one being named. The engine
            // builds a new store rather than growing one, so this is unreachable in a
            // session; skipping keeps it a missing row instead of a shifted file.
            continue;
        };
        write_field(out, name)?;
        out.write_all(b",")?;
        write_display(out, &mut scratch, &sample.time)?;
        out.write_all(b",")?;
        write_display(out, &mut scratch, &sample.raw)?;
        out.write_all(b",")?;
        write_display(out, &mut scratch, &sample.eng)?;
        out.write_all(b",")?;
        write_unit(out, &mut scratch, db, parameter)?;
        out.write_all(b",")?;
        write_display(out, &mut scratch, &store.updates(parameter))?;
        out.write_all(b",")?;
        write_field(out, limits.evaluate(name, &sample.eng).label())?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Writes the history of every watched parameter.
///
/// One row per point, oldest first, parameter by parameter. Only watched parameters have a
/// history at all; a caller that wanted everything had to watch everything first, and that is
/// its decision to make and to pay for.
///
/// # Errors
///
/// [`EngineError::Io`] from the underlying writer.
pub fn write_history<W: Write>(
    out: &mut W,
    store: &ParameterStore,
    db: &XtceDb,
) -> Result<(), EngineError> {
    out.write_all(HISTORY_HEADER.as_bytes())?;
    out.write_all(b"\n")?;
    let mut scratch = String::new();
    for parameter in store.watched() {
        let (Some(name), Some(history)) = (qualified_name(db, parameter), store.history(parameter))
        else {
            continue;
        };
        for point in history {
            write_field(out, name)?;
            out.write_all(b",")?;
            // A `Point` carries seconds as an `f64` because that is the plot's axis; the
            // column is ISO-8601 because a spreadsheet's is not.
            let time = Utc::from_unix_nanos((point.t * 1e9) as i64);
            write_display(out, &mut scratch, &time)?;
            out.write_all(b",")?;
            write_display(out, &mut scratch, &point.v)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Writes one decoded batch as rows, without a header.
///
/// The caller writes [`BATCH_HEADER`] once and then streams batches through this, so that
/// `xtce-gs export` can decode a file of 7 200 packets without holding any of them. `time` is
/// [`Batch::time`] — spacecraft time when the packet carried one — and `received` is the
/// ground stamp: both columns, because an export that lost the distinction cannot be checked
/// against the pass it came from.
///
/// # Errors
///
/// [`EngineError::Io`] from the underlying writer.
pub fn write_batch<W: Write>(
    out: &mut W,
    batch: &Batch,
    db: &XtceDb,
) -> Result<usize, EngineError> {
    let mut scratch = String::new();
    let stamp = PacketStamp {
        time: batch.time(),
        received: batch.received,
        apid: batch.apid,
        sequence: batch.sequence,
    };
    // The count is of rows *written* and not of samples offered: a sample whose `ParamId`
    // the definition cannot turn back into a qualified name is skipped, so the two numbers
    // differ, and a caller that reports the wrong one claims more rows than the file has.
    let mut rows = 0;
    for sample in &batch.samples {
        let Some(name) = qualified_name(db, sample.parameter) else {
            continue;
        };
        write_sample_row(out, &mut scratch, stamp, name, &sample.raw, &sample.eng)?;
        rows += 1;
    }
    Ok(rows)
}

/// What an [`export_stream`] run found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Packets taken off the stream, decoded or not.
    pub packets: u64,
    /// Rows written: one per decoded parameter value.
    pub rows: u64,
    /// Packets the definition refused. Counted and skipped, never fatal — every real
    /// downlink carries packets a given definition does not describe.
    pub rejected: u64,
    /// Whether the stream ended in the middle of a packet.
    pub truncated: bool,
}

/// Decodes a stream of CCSDS space packets straight to CSV.
///
/// This is what `xtce-gs export` runs, and it is deliberately not a [`crate::Session`]: a
/// session files everything into a store an interface reads, and an export reads each packet
/// once and writes it out. The header is written first and then one row per decoded value, in
/// [`BATCH_HEADER`]'s columns.
///
/// Streaming in both directions. At most [`EXPORT_CHUNK_BYTES`] plus one packet is held at a
/// time, so a two-hour pass — 7 200 packets a minute — exports in constant memory. Packet
/// boundaries come from the primary header alone: the total length is
/// `6 + packet data length + 1` octets, CCSDS 133.0-B-2 §4.1.3.5.3.
///
/// A packet the definition refuses is counted in [`ExportSummary::rejected`] and skipped; a
/// stream that ends mid-packet sets [`ExportSummary::truncated`] and stops there. Neither is
/// an error: a definition pointed at the wrong stream refuses every packet, and that result
/// is the summary, not a failure to write CSV.
///
// TODO(gs-engine-record): this reads back-to-back space packets only — a recording of a
// packet stream, or a `.DAT1` file. A stream carrying TM transfer frames, Reed-Solomon parity
// or CSP has to be fed through `xtce_gs_link::Pipeline` first; `xtce-gs-cli`'s `export` is
// where that composition belongs, and it should call `write_batch` per assembled packet
// rather than this. Deciding whether this function should grow a `PipelineConfig` argument
// instead means deciding whether the engine may depend on the link's framing, which the crate
// split so far says no to.
//
// TODO(gs-engine-record): the `time` column repeats `received` here, because resolving
// spacecraft time needs a `SpacecraftClock` and this function is not given one. When
// `sctime.rs` is finished, take an `Option<&SpacecraftClock>` and fill `time` from the packet
// — an export of a recorder dump is exactly the case where ground receipt is the wrong axis.
/// # Errors
///
/// [`EngineError::Io`] when the stream cannot be read or the output cannot be written.
pub fn export_stream<R: Read, W: Write>(
    decoder: &Decoder<'_>,
    mut stream: R,
    out: &mut W,
    limit: Option<usize>,
) -> Result<ExportSummary, EngineError> {
    out.write_all(BATCH_HEADER.as_bytes())?;
    out.write_all(b"\n")?;

    let db = decoder.db();
    let mut summary = ExportSummary::default();
    let mut scratch = String::new();
    let mut buffer: Vec<u8> = Vec::with_capacity(EXPORT_CHUNK_BYTES);
    let mut chunk = vec![0u8; EXPORT_CHUNK_BYTES];
    let mut at = 0usize;
    let mut ended = false;

    loop {
        if limit.is_some_and(|max| summary.packets >= max as u64) {
            break;
        }
        let available = buffer.len() - at;
        let needed = if available >= PRIMARY_HEADER_BYTES {
            let header = SpacePacketBytes::new(buffer.get(at..).unwrap_or_default());
            PRIMARY_HEADER_BYTES + usize::from(header.data_length()) + 1
        } else {
            PRIMARY_HEADER_BYTES
        };

        if available < needed {
            if ended {
                // Whatever is left cannot be a packet: either fewer than six octets, or a
                // header whose declared length the stream never delivered. Reporting it is
                // the point — a silent short read is how an export loses the last packet of
                // every pass and nobody notices.
                summary.truncated = available > 0;
                break;
            }
            if at > 0 {
                buffer.drain(..at);
                at = 0;
            }
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                ended = true;
            } else {
                buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
            }
            // Every iteration either consumes a packet or reads bytes, and `ended` makes the
            // no-bytes case terminate: a hostile length field desynchronises the stream, it
            // does not spin here.
            continue;
        }

        let end = at + needed;
        let packet = buffer.get(at..end).unwrap_or_default();
        at = end;
        summary.packets = summary.packets.saturating_add(1);

        let header = SpacePacketBytes::new(packet);
        // No spacecraft clock here — see the TODO above — so both time columns are the
        // instant the row was made, which for a file is when the export read it.
        let received = Utc::now();
        let stamp = PacketStamp {
            time: received,
            received,
            apid: header.apid(),
            sequence: header.sequence_count(),
        };

        match decoder.decode(packet) {
            Ok(parsed) => {
                for value in parsed.values() {
                    let Some(name) = qualified_name(db, value.parameter) else {
                        continue;
                    };
                    write_sample_row(out, &mut scratch, stamp, name, &value.raw, &value.eng)?;
                    summary.rows = summary.rows.saturating_add(1);
                }
            }
            Err(_) => summary.rejected = summary.rejected.saturating_add(1),
        }
    }

    out.flush()?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use xtce_gs_core::{Sample, Value};
    use xtce_model::ContainerId;

    use super::*;

    /// A JPSS recording of back-to-back space packets, in the sibling `xtce-rs` checkout.
    ///
    // Under `testdata/`, so an export test reads the same recording the link tests frame.
    const JPSS_STREAM: &str = "../../testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1";
    const JPSS_DEFINITION: &str = "../../testdata/jpss/jpss1_geolocation_xtce_v1.xml";

    /// Values every packet of that recording decodes to: seven primary-header fields, three
    /// of the secondary header, and seventeen of the attitude and ephemeris payload — the 27
    /// `<ParameterRefEntry>` elements of `CCSDSPacket` and the containers it descends into.
    const PARAMETERS_PER_JPSS_PACKET: u64 = 27;

    /// Three parameters, one of them named to break an unescaped export, and a container
    /// that reads the first eight octets of a packet — so a packet shorter than that is one
    /// the definition refuses.
    const DEFINITION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xtce:SpaceSystem name="Sat" xmlns:xtce="http://www.omg.org/spec/XTCE/20180204">
  <xtce:TelemetryMetaData>
    <xtce:ParameterTypeSet>
      <xtce:IntegerParameterType name="UINT32" signed="false">
        <xtce:UnitSet><xtce:Unit>deg C</xtce:Unit></xtce:UnitSet>
        <xtce:IntegerDataEncoding sizeInBits="32" encoding="unsigned"/>
      </xtce:IntegerParameterType>
    </xtce:ParameterTypeSet>
    <xtce:ParameterSet>
      <xtce:Parameter name="PLAIN" parameterTypeRef="UINT32"/>
      <xtce:Parameter name="MODE, &quot;ODD&quot;" parameterTypeRef="UINT32"/>
      <xtce:Parameter name="SECOND" parameterTypeRef="UINT32"/>
    </xtce:ParameterSet>
    <xtce:ContainerSet>
      <xtce:SequenceContainer name="PKT">
        <xtce:EntryList>
          <xtce:ParameterRefEntry parameterRef="PLAIN"/>
          <xtce:ParameterRefEntry parameterRef="SECOND"/>
        </xtce:EntryList>
      </xtce:SequenceContainer>
    </xtce:ContainerSet>
  </xtce:TelemetryMetaData>
</xtce:SpaceSystem>"#;

    fn db() -> XtceDb {
        XtceDb::from_xml(DEFINITION).expect("the test definition loads")
    }

    fn find(db: &XtceDb, name: &str) -> ParamId {
        db.find_parameter(name)
            .unwrap_or_else(|| panic!("{name} is declared"))
    }

    /// A unique path under the system temporary directory.
    ///
    /// No `tempfile` crate here — see the workspace dependency list — so uniqueness is the
    /// process id and a counter, and the test removes what it made.
    fn temp_path(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "xtce-gs-record-{}-{tag}-{unique}.bin",
            std::process::id()
        ))
    }

    /// One CCSDS packet: six-octet primary header and `body`.
    fn packet(apid: u16, sequence: u16, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PRIMARY_HEADER_BYTES + body.len());
        bytes.extend_from_slice(&(apid & 0x07ff).to_be_bytes());
        bytes.extend_from_slice(&(0xc000 | (sequence & 0x3fff)).to_be_bytes());
        // CCSDS 133.0-B-2 §4.1.3.5.3: the field is the octet count of the data field, less 1.
        let length = u16::try_from(body.len().saturating_sub(1)).unwrap_or(0);
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    /// Splits a CSV document into records and fields, honouring RFC 4180 §2 quoting.
    ///
    /// Written out rather than asserted against a whole string, because the assertion that
    /// matters is *how many fields a row has*: a shifted column is invisible in a string
    /// comparison that was written from the same broken output.
    fn parse_csv(text: &str) -> Vec<Vec<String>> {
        let mut records = Vec::new();
        let mut record = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        quoted = false;
                    }
                }
                '"' => quoted = true,
                ',' if !quoted => record.push(std::mem::take(&mut field)),
                '\n' if !quoted => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                '\r' if !quoted => {}
                other => field.push(other),
            }
        }
        if !field.is_empty() || !record.is_empty() {
            record.push(field);
            records.push(record);
        }
        records
    }

    fn sample(parameter: ParamId, raw: u64, eng: f64) -> Sample {
        Sample {
            parameter,
            time: Utc::from_unix_secs(1_600_000_000),
            raw: Value::Unsigned(raw),
            eng: Value::Float(eng),
        }
    }

    #[tokio::test]
    async fn a_recording_round_trips_through_a_temp_file() {
        let path = temp_path("roundtrip");
        let mut recorder = Recorder::open(&path).await.unwrap();
        recorder.write(b"first chunk ").await.unwrap();
        recorder.write(b"second chunk").await.unwrap();
        recorder.flush().await.unwrap();

        // The file, not the counter: a recorder that buffered forever would satisfy
        // `bytes_written` and record nothing.
        assert_eq!(std::fs::read(&path).unwrap(), b"first chunk second chunk");
        assert_eq!(recorder.bytes_written(), 24);
        assert_eq!(recorder.buffered(), 0);
        assert_eq!(recorder.path(), path);
        drop(recorder);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_second_pass_appends_rather_than_truncating() {
        let path = temp_path("append");
        let mut first = Recorder::open(&path).await.unwrap();
        first.write(b"pass one").await.unwrap();
        first.flush().await.unwrap();
        drop(first);

        let mut second = Recorder::open(&path).await.unwrap();
        second.write(b"|pass two").await.unwrap();
        second.flush().await.unwrap();
        drop(second);

        assert_eq!(std::fs::read(&path).unwrap(), b"pass one|pass two");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_tail_survives_a_recorder_dropped_without_a_flush() {
        let path = temp_path("tail");
        let mut recorder = Recorder::open(&path).await.unwrap();
        recorder.write(b"the last datagram").await.unwrap();
        // Nothing is on the file yet: under the size bound and inside the time bound.
        assert_eq!(recorder.buffered(), 17);
        drop(recorder);

        assert_eq!(std::fs::read(&path).unwrap(), b"the last datagram");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_write_larger_than_the_buffer_goes_out_whole() {
        let path = temp_path("large");
        let mut recorder = Recorder::open(&path).await.unwrap();
        let big = vec![0xa5u8; RECORD_BUFFER_BYTES * 2 + 7];
        recorder.write(&big).await.unwrap();
        assert_eq!(recorder.buffered(), 0);
        recorder.flush().await.unwrap();
        drop(recorder);

        assert_eq!(std::fs::read(&path).unwrap(), big);
        let _ = std::fs::remove_file(&path);
    }

    fn escaped(field: &str) -> String {
        let mut out = Vec::new();
        write_field(&mut out, field).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_field_with_a_comma_a_quote_or_a_newline_is_quoted() {
        assert_eq!(escaped("SAFE, NO COMMS"), "\"SAFE, NO COMMS\"");
        assert_eq!(escaped("say \"hello\""), "\"say \"\"hello\"\"\"");
        assert_eq!(escaped("two\nlines"), "\"two\nlines\"");
        // A bare carriage return counts too — RFC 4180 §2 names CR as well as LF.
        assert_eq!(escaped("two\rlines"), "\"two\rlines\"");
        assert_eq!(
            escaped("all\r\nof, \"them\""),
            "\"all\r\nof, \"\"them\"\"\""
        );
    }

    #[test]
    fn a_field_that_is_one_quote_becomes_four() {
        assert_eq!(escaped("\""), "\"\"\"\"");
        assert_eq!(escaped("\"\""), "\"\"\"\"\"\"");
    }

    #[test]
    fn a_field_needing_nothing_is_written_as_it_is() {
        assert_eq!(escaped("/Sat/TEMP"), "/Sat/TEMP");
        assert_eq!(escaped(""), "");
        // RFC 4180 §2 makes a surrounding space part of the field, not a reason to quote.
        assert_eq!(escaped("  padded  "), "  padded  ");
        assert_eq!(escaped("deg C"), "deg C");
    }

    #[test]
    fn a_parameter_name_with_a_comma_does_not_shift_the_columns() {
        let db = db();
        let odd = find(&db, "MODE, \"ODD\"");
        let mut store = ParameterStore::new(db.parameters().len(), 8);
        store.push(sample(odd, 3, 3.0));

        let mut out = Vec::new();
        write_snapshot(&mut out, &store, &db, &LimitSet::new()).unwrap();
        let text = String::from_utf8(out).unwrap();
        let rows = parse_csv(&text);

        let columns = SNAPSHOT_HEADER.split(',').count();
        assert_eq!(rows.len(), 2, "header and one parameter");
        assert_eq!(rows[0].join(","), SNAPSHOT_HEADER);
        assert_eq!(rows[1].len(), columns, "the name must not add a column");
        assert_eq!(rows[1][0], "/Sat/MODE, \"ODD\"");
        assert_eq!(rows[1][4], "deg C");
    }

    #[test]
    fn a_snapshot_has_a_row_per_parameter_seen_not_per_slot() {
        let db = db();
        let mut store = ParameterStore::new(db.parameters().len(), 8);
        assert!(store.parameter_count() >= 3);
        store.push(sample(find(&db, "PLAIN"), 1, 1.0));
        store.push(sample(find(&db, "MODE, \"ODD\""), 2, 2.0));

        let mut out = Vec::new();
        write_snapshot(&mut out, &store, &db, &LimitSet::new()).unwrap();
        let rows = parse_csv(&String::from_utf8(out).unwrap());
        assert_eq!(rows.len(), 3, "one header and two seen parameters");
    }

    #[test]
    fn a_snapshot_carries_the_limit_state_of_the_engineering_value() {
        let db = db();
        let plain = find(&db, "PLAIN");
        let mut store = ParameterStore::new(db.parameters().len(), 8);
        store.push(sample(plain, 99, 99.0));

        let mut limits = LimitSet::new();
        limits.insert(
            "/Sat/PLAIN",
            xtce_gs_core::Limit {
                warning: xtce_gs_core::Range::new(0.0, 50.0),
                alarm: xtce_gs_core::Range::new(-10.0, 200.0),
            },
        );

        let mut out = Vec::new();
        write_snapshot(&mut out, &store, &db, &limits).unwrap();
        let rows = parse_csv(&String::from_utf8(out).unwrap());
        assert_eq!(rows[1][6], "WARN");
        assert_eq!(rows[1][5], "1", "one update");
    }

    #[test]
    fn a_history_is_written_oldest_first_and_only_for_what_is_watched() {
        let db = db();
        let plain = find(&db, "PLAIN");
        let mut store = ParameterStore::new(db.parameters().len(), 8);
        store.watch(plain);
        for (index, value) in [10.0f64, 20.0, 30.0].into_iter().enumerate() {
            let mut point = sample(plain, index as u64, value);
            point.time = Utc::from_unix_secs(1_600_000_000 + index as i64);
            store.push(point);
        }
        // Not watched: it has a latest value and no history.
        store.push(sample(find(&db, "MODE, \"ODD\""), 7, 7.0));

        let mut out = Vec::new();
        write_history(&mut out, &store, &db).unwrap();
        let rows = parse_csv(&String::from_utf8(out).unwrap());

        assert_eq!(rows[0].join(","), HISTORY_HEADER);
        assert_eq!(rows.len(), 4, "one header and three points");
        assert_eq!(rows[1][1], "2020-09-13T12:26:40.000Z");
        assert_eq!(rows[1][2], "10");
        assert_eq!(rows[3][2], "30");
    }

    #[test]
    fn a_batch_is_one_row_per_sample_with_no_header() {
        let db = db();
        let batch = Batch {
            container: ContainerId::new(0),
            received: Utc::from_unix_secs(1_600_000_000),
            spacecraft: Some(Utc::from_unix_secs(1_600_000_100)),
            apid: 11,
            sequence: 4095,
            sequence_gap: false,
            samples: vec![
                sample(find(&db, "PLAIN"), 1, 1.5),
                sample(find(&db, "MODE, \"ODD\""), 2, 2.5),
            ],
        };

        let mut out = Vec::new();
        let rows = write_batch(&mut out, &batch, &db).unwrap();
        assert_eq!(
            rows,
            batch.samples.len(),
            "every sample here has a nameable parameter"
        );
        let rows = parse_csv(&String::from_utf8(out).unwrap());

        assert_eq!(rows.len(), 2, "no header, two samples");
        assert_eq!(rows[0].len(), BATCH_HEADER.split(',').count());
        // Spacecraft time in `time`, ground receipt in `received`; both columns, or the
        // export cannot be checked against the pass.
        assert_eq!(rows[0][0], "2020-09-13T12:28:20.000Z");
        assert_eq!(rows[0][1], "2020-09-13T12:26:40.000Z");
        assert_eq!(rows[0][2], "11");
        assert_eq!(rows[0][3], "4095");
        assert_eq!(rows[1][4], "/Sat/MODE, \"ODD\"");
    }

    #[test]
    fn an_empty_stream_exports_the_header_and_nothing_else() {
        let db = db();
        let decoder = Decoder::new(&db).unwrap();
        let mut out = Vec::new();
        let summary = export_stream(&decoder, &[][..], &mut out, None).unwrap();

        assert_eq!(summary, ExportSummary::default());
        assert_eq!(String::from_utf8(out).unwrap(), format!("{BATCH_HEADER}\n"));
    }

    #[test]
    fn an_export_writes_a_row_per_decoded_value() {
        let db = db();
        let decoder = Decoder::new(&db).unwrap();
        let mut stream = Vec::new();
        for sequence in 0..3u16 {
            stream.extend_from_slice(&packet(0x40, sequence, &[0, sequence as u8]));
        }

        let mut out = Vec::new();
        let summary = export_stream(&decoder, stream.as_slice(), &mut out, None).unwrap();
        let rows = parse_csv(&String::from_utf8(out).unwrap());

        assert_eq!(summary.packets, 3);
        assert_eq!(summary.rows, 6, "the container declares two parameters");
        assert_eq!(summary.rejected, 0);
        assert!(!summary.truncated);
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[1][3], "0", "the first packet's sequence count");
        assert_eq!(rows[1][4], "/Sat/PLAIN");
        assert_eq!(rows[6][3], "2");
    }

    #[test]
    fn an_export_stops_at_the_limit() {
        let db = db();
        let decoder = Decoder::new(&db).unwrap();
        let mut stream = Vec::new();
        for sequence in 0..10u16 {
            stream.extend_from_slice(&packet(0x40, sequence, &[0, 1]));
        }

        let mut out = Vec::new();
        let summary = export_stream(&decoder, stream.as_slice(), &mut out, Some(4)).unwrap();
        assert_eq!(summary.packets, 4);
        assert!(!summary.truncated);
    }

    #[test]
    fn a_hostile_length_field_ends_the_export_instead_of_spinning() {
        let db = db();
        let decoder = Decoder::new(&db).unwrap();
        // A header claiming 65 536 octets of data with four on the stream.
        let mut stream = packet(0x40, 0, &[0, 1]);
        if let Some(slice) = stream.get_mut(4..6) {
            slice.copy_from_slice(&u16::MAX.to_be_bytes());
        }

        let mut out = Vec::new();
        let summary = export_stream(&decoder, stream.as_slice(), &mut out, None).unwrap();
        assert_eq!(summary.packets, 0, "the packet was never complete");
        assert!(summary.truncated);
    }

    #[test]
    fn a_stream_that_ends_inside_a_header_is_truncated_not_a_packet() {
        let db = db();
        let decoder = Decoder::new(&db).unwrap();
        let mut stream = packet(0x40, 0, &[0, 1]);
        stream.truncate(3);

        let mut out = Vec::new();
        let summary = export_stream(&decoder, stream.as_slice(), &mut out, None).unwrap();
        assert_eq!(summary.packets, 0);
        assert!(summary.truncated);
        assert_eq!(summary.rows, 0);
    }

    #[test]
    fn a_packet_the_definition_refuses_is_counted_and_skipped() {
        let db = db();
        let decoder = Decoder::new(&db).unwrap();
        // Seven octets on the stream, eight in the container's entry list.
        let stream = packet(0x40, 0, &[0]);

        let mut out = Vec::new();
        let summary = export_stream(&decoder, stream.as_slice(), &mut out, None).unwrap();
        assert_eq!(summary.packets, 1);
        assert_eq!(summary.rejected, 1);
        assert_eq!(summary.rows, 0);
    }

    fn sibling(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn the_first_hundred_jpss_packets_export_with_the_header_and_no_rejects() {
        let definition = sibling(JPSS_DEFINITION);
        let stream = sibling(JPSS_STREAM);
        // Both are vendored in this repository, so a failure here is a deleted fixture and
        // not an absent one. It used to return early and pass, which made the test's name a
        // claim about a recording it had stopped reading.
        let db = XtceDb::from_path(&definition)
            .unwrap_or_else(|error| panic!("{}: {error}", definition.display()));
        let file = std::fs::File::open(&stream)
            .unwrap_or_else(|error| panic!("{}: {error}", stream.display()));
        let decoder = Decoder::new(&db).unwrap();

        // One packet first, so that the hundred-packet count below is a hundred times a
        // known row count rather than an average that would hide a short packet.
        let mut first = Vec::new();
        let one = export_stream(&decoder, &file, &mut first, Some(1)).unwrap();
        assert_eq!(one.packets, 1);
        assert_eq!(one.rows, PARAMETERS_PER_JPSS_PACKET);

        let file = std::fs::File::open(&stream).unwrap();
        let mut out = Vec::new();
        let summary = export_stream(&decoder, file, &mut out, Some(100)).unwrap();
        let text = String::from_utf8(out).unwrap();
        let rows = parse_csv(&text);

        assert_eq!(summary.packets, 100);
        assert_eq!(summary.rejected, 0, "the definition describes this stream");
        assert!(!summary.truncated);
        // Every packet in this recording decodes to the same parameters, so the row count is
        // exact rather than a lower bound.
        assert_eq!(summary.rows, 100 * PARAMETERS_PER_JPSS_PACKET);
        assert_eq!(rows.len(), 2701, "one header and one row per value");
        assert_eq!(rows[0].join(","), BATCH_HEADER);
        let columns = BATCH_HEADER.split(',').count();
        for row in &rows {
            assert_eq!(row.len(), columns);
        }
        assert!(rows[1][4].starts_with('/'), "names are qualified");
    }
}
