//! The task graph: a socket at one end, a store the interface reads at the other.
//!
//! Three threads, and they are not symmetric. The runtime owns the source and the framing
//! pipeline and blocks on the socket; one plain thread owns the decoder and blocks on a
//! channel of packets; the interface owns the window and blocks on the display. What joins
//! them is an `Arc<RwLock<ParameterStore>>` written once per batch and read once per frame,
//! and a waker that tells the interface a frame is worth drawing.
//!
//! # Why the decoder is not a field
//!
//! [`xtce_decode::Decoder`] borrows the [`XtceDb`] it decodes against. A struct holding both
//! is self-referential, which in Rust is `unsafe` or a crate that hides the `unsafe`, and
//! neither is worth it for a borrow whose lifetime is obvious: the decode thread holds an
//! `Arc<XtceDb>`, creates the decoder as a local at the top of its loop, and the `Arc`
//! outlives it by construction. So [`Session`] stores the definition and not the decoder, and
//! [`crate::decode::PacketDecoder::decode_into`] takes a `&Decoder` per call.
//!
//! # What happens when the decoder falls behind
//!
//! The queue between the two halves is bounded, and a bounded queue that fills has to lose
//! something: time, or packets. Which one is the right thing to lose is a property of the
//! *source*, so this module decides it per source and not once:
//!
//! * **A file replay blocks.** The file is the authority. A replay that dropped packets would
//!   give a different answer from the same bytes on every run, which makes a recording
//!   useless for reproducing anything — and there is no deadline to miss: a replay that takes
//!   a second longer is a replay that took a second longer.
//! * **A live link drops, at the old end, and counts it.** There is no back-pressure to apply
//!   to a radio. What a station can choose is *which* packets it loses, and the useful end is
//!   the old one: when the overload passes, what is left in the queue is the most recent
//!   telemetry, so the display catches up to now instead of replaying the last ten seconds
//!   first. The count is [`Session::dropped`], and the acquisition task says so on the log.
//!
//! `tokio::sync::mpsc` cannot evict its own oldest element — a sender holds no receiver — so
//! the live path keeps an overflow ring of its own in front of the channel and evicts there.
//! The packet it drops is therefore the oldest one *not yet handed to the decoder*; up to
//! [`PACKET_QUEUE_DEPTH`] older ones are already in the channel and are still decoded. That
//! is the honest description of the policy, and it is the property the paragraph above rests
//! on: nothing newer is ever thrown away to make room for something older.

// TODO(gs-engine-session): the eviction is one queue late, and making it exact needs a
// different queue. `tokio::sync::mpsc` is what the cross-crate decisions fixed on, and its
// sender cannot reach into the channel, so the oldest packet a full station drops is the
// oldest one still in the ring and not the oldest one in flight. Making it exact means
// replacing the channel with a `Mutex<VecDeque<Acquired>>` plus a `Condvar` the decode thread
// parks on — the decode side is a plain thread, so that is a natural fit — and an async
// "room appeared" signal for the replay policy. Decide it against a measurement: how far
// behind does a real station's display get on a link it cannot keep up with, and does one
// queue's worth of that lag matter to the operator watching it?

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use xtce_decode::Decoder;
use xtce_gs_core::{Batch, Event, EventLog, LimitSet, LinkStats, ParameterStore, RawPacket, Utc};
use xtce_gs_link::source::MAX_DATAGRAM;
use xtce_gs_link::{LinkError, Pipeline, PipelineConfig, Source, SourceSpec};
use xtce_model::XtceDb;

use crate::config::{SessionConfig, load_limits};
use crate::decode::{PacketDecoder, empty_batch};
use crate::error::EngineError;
use crate::record::Recorder;
use crate::sctime::SpacecraftClock;

/// Packets the acquisition side may run ahead of the decoder by.
///
/// Bounded, and it bounds two things: the channel the decode thread drains, and the overflow
/// ring in front of it on a live source. Both, so that a station that cannot keep up holds at
/// most two queues of packets and not a growing list of them — a station that buffered
/// everything would trade a display three seconds late for a display three minutes late and
/// then for an allocator failure.
///
/// 1 024 is also what bounds a batch: the decode thread drains whatever is queued, decodes all
/// of it, and takes the store's write lock once for the lot. See [`Session::start`].
pub const PACKET_QUEUE_DEPTH: usize = 1024;

/// Consecutive failed reads tolerated before the source is called gone.
///
/// A read failure is not always the end, and the failures this budget is for are the ones the
/// sources this station actually opens can produce:
///
/// * `Interrupted` on a file replay. `std::io::Read::read` is documented to surface `EINTR`
///   rather than retry it, and [`xtce_gs_link::source::FileReplay`] reads a `tokio::fs::File`
///   through exactly that — so a signal landing in the blocking read is a failure whose only
///   correct answer is another read, and a process taking signals can take several.
/// * `ConnectionReset` on `tcp://`. A peer that vanishes resets rather than closing, and the
///   reset is reported on the next read; a station that ended the pass on the first one would
///   end it over a feeder that was restarted.
/// * `TimedOut` on `tcp-listen://`, which `source::per_connection` deliberately excludes from
///   the disconnects it swallows, so it arrives here instead.
///
/// **Not UDP.** [`xtce_gs_link::source::UdpSource::bind`] binds and never calls `connect` and
/// never sends, so there is no last send for an ICMP port-unreachable to be reported against
/// and the `ConnectionReset`-on-a-connected-socket case does not arise on this path.
///
/// `WouldBlock` is in [`is_transient`]'s set too, as belt-and-braces: tokio's readiness loop
/// consumes it before a read ever returns, so it is not what this budget is sized for.
///
/// A reset socket answers every read the same way and answers it immediately, so the retry is
/// a spin unless it is bounded: a failure that repeats this many times with no successful read
/// in between is a source that is not coming back, and the session stops.
const READ_ERROR_LIMIT: u32 = 16;

/// What the engine calls when a batch has been ingested.
///
/// The interface fills it with `egui::Context::request_repaint`. It is an `Arc<dyn Fn()>`
/// rather than a channel because egui is immediate mode and the only thing the engine has to
/// say is "there is something new": a station that repainted on a timer would burn a laptop
/// battery on a stream sending one packet a minute, and one that never asked would show a
/// frozen number.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// What the acquisition task sends the decode task.
///
/// Not `RawPacket` alone. A byte stream can restart underneath a station — a listening socket
/// accepts a new peer, a replay wraps — and the decode side holds state that is wrong across
/// that boundary: a per-APID sequence count from the previous stream turns one restart into a
/// run of spurious gaps. The restart has to travel in the same queue as the packets, because
/// it has a *place* among them, and a flag read out of band would be applied to the wrong side
/// of the boundary by however many packets were in flight.
#[derive(Debug)]
pub enum Acquired {
    /// One packet off the link.
    Packet(RawPacket),
    /// The byte stream restarted: drop partial state and reset sequence tracking.
    Restart,
}

/// Appends a line to a log whose mutex may be poisoned.
///
/// A panic on the decode thread must not take the event log with it — the log is where the
/// operator would read *about* that panic. [`EventLog::push`] cannot leave the ring
/// inconsistent, so recovering the guard through `PoisonError::into_inner` is correct here
/// rather than merely convenient, and dropping the line on a poisoned lock would be the one
/// failure mode that erases its own explanation.
pub fn log(events: &Mutex<EventLog>, event: Event) {
    let mut guard = events.lock().unwrap_or_else(PoisonError::into_inner);
    guard.push(event);
}

/// A running station: one definition, one source, one store.
///
/// Cheap to clone *around* — every field the interface touches is an `Arc` — but not `Clone`
/// itself, because shutting down is a thing one owner does once.
pub struct Session {
    config: SessionConfig,
    db: Arc<XtceDb>,
    store: Arc<RwLock<ParameterStore>>,
    stats: Arc<LinkStats>,
    events: Arc<Mutex<EventLog>>,
    limits: LimitSet,
    /// What the acquisition task actually opened, which is not always what was configured.
    source: Arc<RwLock<String>>,
    running: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    stop: watch::Sender<bool>,
}

impl Session {
    /// Loads the definition, builds the store, and spawns the tasks on `runtime`.
    ///
    /// Returns as soon as the tasks are spawned. The source is opened *by* the acquisition
    /// task — connecting is `async`, and `Handle::block_on` panics when `start` is called
    /// from a runtime worker, which this crate cannot rule out and must not panic on — so a
    /// bad address shows up on the event log and in [`Session::is_running`], not here. The
    /// address that was actually *bound* shows up there too: [`Session::describe_source`] can
    /// only name the configured one, and `udp://0.0.0.0:0` tells an operator nothing about
    /// where to point a feeder.
    ///
    /// # Errors
    ///
    /// [`EngineError::Config`] for a configuration that cannot be carried out — including a
    /// spacecraft time parameter the definition does not declare — [`EngineError::Link`] for
    /// framing the pipeline refuses, [`EngineError::Xtce`] for a definition that will not
    /// load, [`EngineError::Decode`] for a root container that is not in it, and
    /// [`EngineError::Io`] for a limits file that cannot be read or a thread the operating
    /// system will not give.
    pub fn start(
        config: SessionConfig,
        runtime: &Handle,
        waker: Option<Waker>,
    ) -> Result<Self, EngineError> {
        // Everything below opens something. Nothing below should be the first thing to notice
        // a history depth of zero.
        config.validate()?;

        // This blocks, and it blocks the *caller* — the interface thread, before its window
        // exists. Deliberate: a definition that does not parse must be an error the operator
        // reads, not an empty window.
        let db = Arc::new(XtceDb::from_path(&config.definition)?);

        // Built only to be dropped. It turns a mistyped container name into a refused session
        // here rather than a decode thread that dies on the first packet; the decode thread
        // builds its own by the same rule, because it cannot be handed this one — see the
        // module header.
        let _probe = match config.root_container.as_deref() {
            Some(name) => Decoder::with_root(&db, name)?,
            None => Decoder::new(&db)?,
        };

        let limits = match config.limits.as_deref() {
            Some(path) => load_limits(path)?,
            None => LimitSet::new(),
        };
        // Resolved here and not per packet: a name that does not resolve must fail the start.
        // An operator who named a clock parameter and silently got ground receipt has plots
        // that are wrong with nothing on the log to say so.
        let clock = match config.spacecraft_time.as_ref() {
            Some(source) => Some(SpacecraftClock::resolve(&db, source)?),
            None => None,
        };

        let store = Arc::new(RwLock::new(ParameterStore::new(
            db.parameters().len(),
            config.history_depth,
        )));
        let events = Arc::new(Mutex::new(EventLog::with_capacity(config.event_capacity)));
        let stats = Arc::new(LinkStats::new());
        let running = Arc::new(AtomicBool::new(true));
        let dropped = Arc::new(AtomicU64::new(0));

        let (packets_tx, packets_rx) = mpsc::channel::<Acquired>(PACKET_QUEUE_DEPTH);
        let (stop_tx, stop_rx) = watch::channel(false);

        // The decode thread first. If the operating system refuses the thread, `packets_tx`
        // is dropped on the way out of this function and nothing has been spawned; the other
        // order would leave an acquisition task reading a socket for a session that failed
        // to start.
        let decode = DecodeTask {
            db: Arc::clone(&db),
            root: config.root_container.clone(),
            clock,
            store: Arc::clone(&store),
            stats: Arc::clone(&stats),
            events: Arc::clone(&events),
            packets: packets_rx,
            waker,
        };
        // Detached on purpose. `shutdown` is a request and not a join — see its doc comment —
        // and the thread ends on its own when the sender is dropped.
        let _decode = std::thread::Builder::new()
            .name("xtce-gs-decode".to_owned())
            .spawn(move || decode_loop(decode))?;

        let replay = matches!(config.source, SourceSpec::File { .. });
        // Shared with the acquisition task, which overwrites it with what it really opened:
        // `udp://0.0.0.0:0` is a configuration and not an address.
        let described = Arc::new(RwLock::new(config.source.to_string()));

        let acquisition = Acquisition {
            spec: config.source.clone(),
            described: Arc::clone(&described),
            pipeline: config.pipeline.clone(),
            record: config.record.clone(),
            stats: Arc::clone(&stats),
            events: Arc::clone(&events),
            running: Arc::clone(&running),
            stop: stop_rx,
            queue: Dispatch::new(
                packets_tx,
                PACKET_QUEUE_DEPTH,
                !replay,
                Arc::clone(&dropped),
            ),
        };
        let _acquire = runtime.spawn(acquire(acquisition));

        Ok(Self {
            source: described,
            config,
            db,
            store,
            stats,
            events,
            limits,
            running,
            dropped,
            stop: stop_tx,
        })
    }

    /// The configuration this session was started from.
    #[must_use]
    pub const fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The definition, shared with the decode thread.
    ///
    /// An `Arc` and not an `Arc<RwLock<…>>`: an [`XtceDb`] is built once and never mutated,
    /// and a lock around something nothing writes is a lock that only costs.
    #[must_use]
    pub const fn db(&self) -> &Arc<XtceDb> {
        &self.db
    }

    /// The store the decode thread writes and the interface reads.
    ///
    /// One lock for the whole store. The interface takes the read lock once per frame and
    /// copies out only what it draws; it must not hold the guard across a repaint.
    #[must_use]
    pub const fn store(&self) -> &Arc<RwLock<ParameterStore>> {
        &self.store
    }

    /// The link counters, for the status bar.
    #[must_use]
    pub const fn stats(&self) -> &Arc<LinkStats> {
        &self.stats
    }

    /// The event log, bounded and shared.
    #[must_use]
    pub const fn events(&self) -> &Arc<Mutex<EventLog>> {
        &self.events
    }

    /// The limits loaded at start-up.
    ///
    /// Not behind a lock and not reloadable: a limits file is read once, and an operator who
    /// edits it restarts the session. Owned by the session rather than shared because only
    /// the interface reads it — the decode thread stores values, it does not judge them.
    #[must_use]
    pub const fn limits(&self) -> &LimitSet {
        &self.limits
    }

    /// Packets thrown away because the decoder was behind.
    ///
    /// Not a [`LinkStats`] counter. `LinkStats::packets_lost` belongs to the link and means
    /// something else — a partial packet abandoned at a frame gap — and a station that added
    /// this to it would report a slow decoder as a bad downlink. Nonzero here means the
    /// machine could not decode as fast as the radio delivered, which is a different fault
    /// with a different fix.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// One line naming what this session is reading.
    ///
    /// The configured source, not the bound one: the socket is opened by the acquisition
    /// task, after this is set. The bound address is the first line on the event log.
    ///
    // Returns an owned `String` and not a `&str`, which is what lets the acquisition task
    // replace it with the address it really bound. The cost is one small allocation per
    // frame on a status bar that already formats a dozen numbers into strings to draw them,
    // and the value is that `udp://0.0.0.0:0` stops being what the operator is told when the
    // kernel picked the port.
    #[must_use]
    pub fn describe_source(&self) -> String {
        self.source
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the acquisition task is still reading.
    ///
    /// False once the source ended, failed to open, or was shut down. A file replay that ran
    /// to the end reports false while the store keeps everything it decoded — the session is
    /// over, the telemetry is not.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Asks both tasks to stop.
    ///
    /// A request, not a join: this returns immediately and the tasks notice at their next
    /// wake — within one read for acquisition, within one batch for decode. Nothing here
    /// waits for them, because the caller is the interface thread and a station that froze
    /// its window until a socket returned would look like the crash it is trying to avoid.
    /// Call [`Session::is_running`] to see that it took effect.
    ///
    // TODO(gs-engine-session): nothing can wait for the tasks to have *finished*. Both are
    // detached, so a caller that wants to reopen the same recording for writing, or to unit
    // test that the decode thread stopped, has to poll `is_running` and then guess about the
    // decode thread, which `is_running` says nothing about. Adding it means keeping the
    // `JoinHandle` and the runtime's `JoinHandle` in the session and a `join_timeout` that is
    // explicitly *not* called from `Drop` — `Drop` runs on the interface thread. Decide what
    // the timeout is and what a caller does when it expires, because a join that can hang is
    // worse than no join at all.
    ///
    /// Idempotent. A `watch` and not a `Notify`: `notify_waiters` wakes only tasks that are
    /// already parked, so a shutdown that lands while the acquisition task is inside
    /// `read_chunk` would be lost and the task would read the socket forever. A watch holds
    /// the value, so the task sees it whenever it next looks.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        // `send` fails once every receiver is gone, which is the normal state of a session
        // whose tasks have already ended. `send_replace` cannot fail, and shutting down twice
        // has to be as harmless as shutting down once.
        let _previous = self.stop.send_replace(true);
    }
}

impl Drop for Session {
    /// Signals shutdown. Does not wait.
    ///
    /// A dropped session must not leave a thread reading a socket into a store nobody holds.
    /// Joining here would be the other mistake: `Drop` runs on the interface thread.
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl fmt::Debug for Session {
    /// Hand-written because [`XtceDb`] is not `Debug` and a store is 9 493 slots wide.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("source", &self.source)
            .field("parameters", &self.db.parameters().len())
            .field("running", &self.is_running())
            .finish_non_exhaustive()
    }
}

/// The decode side is gone: the channel is closed, or shutdown was asked for.
///
/// Not an [`EngineError`]: nothing failed. The acquisition task has nowhere left to put a
/// packet, so it stops, and the reason is already on the log.
#[derive(Clone, Copy, Debug)]
struct Gone;

/// Flips `running` to false however the acquisition task ends.
///
/// A guard and not a store at the bottom of the function, because the task can also end by
/// being dropped — a runtime that shuts down cancels it mid-`await` — and a station whose
/// status bar reads "running" with nothing reading the socket is the one lie an operator
/// cannot recover from.
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// The acquisition side of the queue: the channel, and the overflow ring in front of it.
///
/// See the module header for why the ring exists and which end it drops from.
struct Dispatch {
    packets: mpsc::Sender<Acquired>,
    backlog: VecDeque<Acquired>,
    capacity: usize,
    lossy: bool,
    dropped: Arc<AtomicU64>,
}

impl Dispatch {
    /// A dispatcher over `packets`, holding at most `capacity` items of overflow.
    ///
    /// `lossy` is the live-link policy: drop the oldest packet in the ring when it is full.
    /// A replay is not lossy and waits instead.
    fn new(
        packets: mpsc::Sender<Acquired>,
        capacity: usize,
        lossy: bool,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            packets,
            backlog: VecDeque::new(),
            capacity,
            lossy,
            dropped,
        }
    }

    /// Whether anything is waiting in front of the channel.
    fn is_backed_up(&self) -> bool {
        !self.backlog.is_empty()
    }

    /// Moves as much of the ring into the channel as fits right now. Never blocks.
    fn flush(&mut self) -> Result<(), Gone> {
        while let Some(item) = self.backlog.pop_front() {
            match self.packets.try_send(item) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(item)) => {
                    self.backlog.push_front(item);
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(Gone),
            }
        }
        Ok(())
    }

    /// Hands one item to the decode side, by whichever policy this source has.
    ///
    /// Ordering is preserved in both policies: the ring is emptied into the channel before
    /// anything new is offered to it, so a [`Acquired::Restart`] can never overtake the
    /// packets it comes after.
    async fn send(&mut self, item: Acquired, stop: &mut watch::Receiver<bool>) -> Result<(), Gone> {
        self.flush()?;
        let item = if self.is_backed_up() {
            // Something is already queued in front of this one; the channel is full and the
            // ring is where this belongs, whatever the policy is.
            item
        } else {
            match self.packets.try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(Gone),
                Err(mpsc::error::TrySendError::Full(item)) => item,
            }
        };
        if self.lossy {
            self.push(item);
            return Ok(());
        }
        self.wait_and_send(item, stop).await
    }

    /// Waits for room and sends. The replay policy.
    ///
    /// Selects on the shutdown watch as well, because a replay whose decode thread has
    /// stalled would otherwise wait here for a session the operator has already closed. The
    /// item is lost when the shutdown wins, which is what shutting down means.
    async fn wait_and_send(
        &mut self,
        item: Acquired,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), Gone> {
        tokio::select! {
            biased;
            _ = stop.changed() => Err(Gone),
            result = self.packets.send(item) => result.map_err(|_| Gone),
        }
    }

    /// Appends to the ring, dropping its oldest *packet* when it is full. The live policy.
    ///
    /// A [`Acquired::Restart`] in the ring is never what gets dropped. It is one byte of
    /// bookkeeping and it is the whole reason the restart travels in the queue rather than
    /// beside it: losing it leaves `PacketDecoder` and `SequenceTracker` holding state from
    /// before a stream boundary, and the sequence counts across that boundary then read as
    /// thousands of packets missing that were never sent. Dropping the oldest packet costs
    /// one packet; dropping the marker costs the rest of the pass.
    ///
    /// The only source that is both lossy and able to restart is `tcp-listen://`, so this
    /// runs when a feeder reconnects while the decoder is behind — which is exactly when the
    /// ring is full.
    fn push(&mut self, item: Acquired) {
        if self.backlog.len() >= self.capacity {
            let oldest_packet = self
                .backlog
                .iter()
                .position(|queued| matches!(queued, Acquired::Packet(_)));
            if let Some(index) = oldest_packet {
                self.backlog.remove(index);
                self.dropped.fetch_add(1, Ordering::Relaxed);
            } else if matches!(item, Acquired::Packet(_)) {
                // A ring of nothing but markers, which means the decoder has not run at all.
                // Refusing the new packet keeps the markers in order; it is counted like any
                // other drop.
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.backlog.push_back(item);
    }

    /// Empties the ring into the channel, waiting for room, at the end of a stream.
    ///
    /// Called once, when the source has ended: the last few hundred packets of a pass are in
    /// the ring precisely when the pass was busy at the end, and they are exactly the ones an
    /// operator is about to look at.
    ///
    /// Two of the four sources reach it, and only one of those has anything in the ring.
    /// `Ok(0)` comes from `tcp://` when the peer closes and from `file://` at the end of the
    /// replay; `udp://` documents that it never returns `Ok(0)` — a bound socket has no end —
    /// and `tcp-listen://` re-accepts instead of ending, so neither arrives here at all. Of
    /// the two that do, `file://` is the source that is not lossy, so its ring is empty by
    /// construction and this is a no-op. `tcp://` is the one case with packets to save.
    async fn drain(&mut self, stop: &mut watch::Receiver<bool>) -> Result<(), Gone> {
        while let Some(item) = self.backlog.pop_front() {
            self.wait_and_send(item, stop).await?;
        }
        Ok(())
    }

    /// How many packets have been dropped for want of room.
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Everything the acquisition task owns.
struct Acquisition {
    spec: SourceSpec,
    /// What [`Session::describe_source`] reports, replaced once the source is open.
    described: Arc<RwLock<String>>,
    pipeline: PipelineConfig,
    record: Option<PathBuf>,
    stats: Arc<LinkStats>,
    events: Arc<Mutex<EventLog>>,
    running: Arc<AtomicBool>,
    stop: watch::Receiver<bool>,
    queue: Dispatch,
}

/// Whether a read failure is worth another read.
///
/// Everything not listed is fatal, including [`LinkError::Config`] — a source that refuses its
/// own configuration will refuse it again — and [`LinkError::Ended`], which is not a failure at
/// all but a read issued after the stream finished.
///
/// The `Ended` arm cannot fire from the only caller, and it is the caller that makes it so:
/// `Ended` is produced only by a read *after* one that returned `Ok(0)`, and [`acquire`]
/// breaks out of its loop on the `Ok(0)` and never issues that second read. `LinkError`'s own
/// doc says the same from the other side — "a caller that stops on `Ok(0)` never sees
/// `Ended`". The arm stays because an exhaustive match over three variants costs nothing and
/// makes the next variant somebody adds a compile error here rather than a silent `false`.
fn is_transient(error: &LinkError) -> bool {
    use std::io::ErrorKind::{ConnectionReset, Interrupted, TimedOut, WouldBlock};
    match error {
        LinkError::Io(io) => matches!(
            io.kind(),
            ConnectionReset | Interrupted | WouldBlock | TimedOut
        ),
        LinkError::Config(_) | LinkError::Ended => false,
    }
}

/// Opens the recording, or says on the log why there is none.
///
/// A recording that will not open is not a reason to lose the pass: the bytes still arrive,
/// they still decode, and the operator still sees telemetry. It is a reason to say so once,
/// loudly, because the other way an operator finds out is at the end of the pass.
async fn open_recorder(path: Option<&Path>, events: &Mutex<EventLog>) -> Option<Recorder> {
    let path = path?;
    match Recorder::open(path).await {
        Ok(recorder) => {
            log(
                events,
                Event::info("session", format!("recording to {}", path.display())),
            );
            Some(recorder)
        }
        Err(error) => {
            log(
                events,
                Event::error("session", format!("{}: {error}", path.display())),
            );
            None
        }
    }
}

/// Reads the source until it ends, is shut down, or stops answering.
/// Opens the source and, if one is configured, the recorder.
///
/// Split out of [`acquire`] because everything here happens once and everything there happens
/// per chunk, and because the order of the three steps below is the whole of what it has to
/// get right.
async fn open(task: &mut Acquisition) -> Option<(Source, Option<Recorder>)> {
    // Selected on the shutdown watch like every other await in this task. A `tcp://` connect
    // to an unrouted address does not return for as long as the kernel's timeout, and
    // `Session::shutdown` promises the task notices "within one read" — there is no read to
    // notice within here, so without this arm the task, its `Arc<XtceDb>` and its store
    // reference outlive the window by tens of seconds.
    let connected = tokio::select! {
        biased;
        _ = task.stop.changed() => return None,
        result = Source::connect(&task.spec) => result,
    };
    let source = match connected {
        Ok(source) => source,
        Err(error) => {
            let spec = &task.spec;
            log(
                &task.events,
                Event::error("session", format!("{spec}: {error}")),
            );
            return None;
        }
    };

    // Before the recorder, because opening one creates a file and logs a line. A session
    // stopped between `start` and the task's first poll must not leave a zero-byte recording
    // and two lines claiming a station is reading a socket it will never read.
    if *task.stop.borrow() {
        return None;
    }

    let opened = source.describe();
    if let Ok(mut described) = task.described.write() {
        described.clone_from(&opened);
    }
    log(
        &task.events,
        Event::info("session", format!("reading {opened}")),
    );

    let recorder = open_recorder(task.record.as_deref(), &task.events).await;
    Some((source, recorder))
}

/// Says why acquisition stopped, and tells the two reasons apart.
///
/// The channel to the decode thread closes for two reasons that look identical from here: the
/// session was shut down and the receiver went with it, or the decode thread ended on its own
/// — which, the sender aside, means it panicked. An operator who closed a window should not
/// read that a thread died, and one whose thread died must not read nothing at all.
///
/// The stop flag is read here rather than carried in `Gone`, which would be exact and would
/// make four call sites match on a two-variant enum. What that exactness would buy is the
/// right sentence in one interleaving: a decode thread that panics in the same instant the
/// operator closes the window is reported as a close. That is a race between two things that
/// both end the session, and the counters say the same either way.
fn report_decoder_gone(task: &Acquisition) {
    if *task.stop.borrow() {
        log(
            &task.events,
            Event::info("session", "the session was closed"),
        );
    } else {
        log(
            &task.events,
            Event::error(
                "session",
                "the decode thread is gone; acquisition has stopped. Nothing below the \
                 decoder failed — the counters up to `packets_in` are the last ones that \
                 mean anything for this pass",
            ),
        );
    }
}

async fn acquire(mut task: Acquisition) {
    let _running = RunningGuard(Arc::clone(&task.running));

    let Some((mut source, mut recorder)) = open(&mut task).await else {
        return;
    };

    let mut pipeline = Pipeline::new(task.pipeline.clone(), Arc::clone(&task.stats));
    let mut buffer = Vec::with_capacity(MAX_DATAGRAM);
    let mut packets = Vec::new();
    let mut failures = 0u32;

    loop {
        let stopped = *task.stop.borrow();
        if stopped {
            break;
        }
        if task.queue.flush().is_err() {
            report_decoder_gone(&task);
            break;
        }

        let read = tokio::select! {
            biased;
            _ = task.stop.changed() => break,
            // Room appeared while nothing was arriving. Without this arm a burst followed by
            // silence would leave the ring's packets sitting behind a full channel until the
            // next byte off the link, which on a pass that has just ended is never. The
            // permit is dropped and the flush at the top of the loop uses the slot: holding
            // it would borrow the sender the ring also needs.
            permit = task.queue.packets.reserve(), if task.queue.is_backed_up() => {
                if permit.is_err() {
                    break;
                }
                continue;
            }
            result = source.read_chunk(&mut buffer) => result,
        };

        match read {
            Ok(0) => {
                // Flush before the log line, so what the stream ended in the middle of is
                // reported as a loss rather than dropped with the pipeline. A space packet
                // declares its own length, so a recording cut short leaves the assembler
                // holding a partial that nothing will ever complete — and without this it
                // went with no counter, no event and a clean exit. The same hole was in
                // `xtce-gs export` and was found there first.
                pipeline.flush_message();
                for event in pipeline.take_events() {
                    log(&task.events, event);
                }
                log(&task.events, Event::info("session", "the source ended"));
                let _ = task.queue.drain(&mut task.stop).await;
                break;
            }
            Ok(_) => failures = 0,
            Err(error) => {
                log(&task.events, Event::error("link", error.to_string()));
                failures += 1;
                if !is_transient(&error) || failures >= READ_ERROR_LIMIT {
                    log(
                        &task.events,
                        Event::error("session", "the source is gone; the session has stopped"),
                    );
                    break;
                }
                buffer.clear();
                continue;
            }
        }

        if handle_chunk(
            &mut task,
            &mut source,
            &mut pipeline,
            &mut recorder,
            &buffer,
            &mut packets,
        )
        .await
        .is_err()
        {
            report_decoder_gone(&task);
            break;
        }
        buffer.clear();
    }

    if let Some(recorder) = recorder.as_mut()
        && let Err(error) = recorder.flush().await
    {
        log(
            &task.events,
            Event::error("session", format!("recording: {error}")),
        );
    }
}

/// Records, frames and dispatches one chunk of bytes.
///
/// The recording is written *before* the pipeline sees a byte, and it is the bytes exactly as
/// the source produced them: no framing, no index, no per-packet wrapper. That is what lets a
/// recording be replayed later through a *different* pipeline configuration — which is the
/// whole point of keeping one, because the configuration is the thing an operator gets wrong.
async fn handle_chunk(
    task: &mut Acquisition,
    source: &mut Source,
    pipeline: &mut Pipeline,
    recorder: &mut Option<Recorder>,
    buffer: &[u8],
    packets: &mut Vec<RawPacket>,
) -> Result<(), Gone> {
    if let Some(open) = recorder.as_mut()
        && let Err(error) = open.write(buffer).await
    {
        // A full disk should not end a pass. It should be impossible to miss, though, so the
        // recorder is closed rather than left silently dropping bytes.
        log(
            &task.events,
            Event::error("session", format!("recording stopped: {error}")),
        );
        *recorder = None;
    }

    // A new TCP peer, or a replay that wrapped. Framing state from before the restart
    // describes a different stream, and so do the sequence counts on the other side of the
    // channel; both are told, in that order, before any byte of the new stream is pushed.
    if source.take_restart() {
        pipeline.flush_message();
        log(&task.events, Event::info("link", "the stream restarted"));
        task.queue.send(Acquired::Restart, &mut task.stop).await?;
    }

    let before = task.queue.dropped();
    pipeline.push(buffer, Utc::now(), packets);
    // One datagram is one message: the bytes either side of a lost one are not one frame.
    if source.is_datagram() {
        pipeline.flush_message();
    }
    for event in pipeline.take_events() {
        log(&task.events, event);
    }
    for packet in packets.drain(..) {
        task.queue
            .send(Acquired::Packet(packet), &mut task.stop)
            .await?;
    }

    // One line per chunk and not one per packet: the log collapses repeats, but the mutex
    // behind it would still be taken once per dropped packet on the one path that is already
    // too slow. The number is on `Session::dropped`.
    if task.queue.dropped() > before {
        log(
            &task.events,
            Event::warning(
                "session",
                "the decoder is behind; the oldest queued packets are being dropped",
            ),
        );
    }
    Ok(())
}

/// Everything the decode thread owns.
struct DecodeTask {
    db: Arc<XtceDb>,
    root: Option<String>,
    clock: Option<SpacecraftClock>,
    store: Arc<RwLock<ParameterStore>>,
    stats: Arc<LinkStats>,
    events: Arc<Mutex<EventLog>>,
    packets: mpsc::Receiver<Acquired>,
    waker: Option<Waker>,
}

/// Decodes queued packets into the store, one lock per batch.
///
/// The `Decoder` is built here, from the `Arc` this thread owns, for the reason in the module
/// header. The loop below is the reason the channel carries [`Acquired`] and not `RawPacket`:
/// a restart has a place in the sequence of packets and is applied where it belongs.
fn decode_loop(mut task: DecodeTask) {
    let decoder = match task.root.as_deref() {
        Some(name) => Decoder::with_root(&task.db, name),
        None => Decoder::new(&task.db),
    };
    let decoder = match decoder {
        Ok(decoder) => decoder,
        Err(error) => {
            // `Session::start` already built one of these and refused the session if it
            // failed, so this is unreachable — and unreachable is not a reason to panic on
            // the one thread whose death is silent.
            log(
                &task.events,
                Event::error("decode", format!("no decoder: {error}")),
            );
            return;
        }
    };

    let mut packets = PacketDecoder::new(task.clock, Arc::clone(&task.stats));
    // One batch per packet in the drained set, reused for the life of the session: the store
    // is written once for the whole set, so every batch in it has to still exist when the
    // lock is taken. `Vec::clear` on the samples inside keeps the allocation.
    let mut batches: Vec<Batch> = Vec::new();
    let mut queued: Vec<Acquired> = Vec::with_capacity(PACKET_QUEUE_DEPTH);

    while let Some(first) = task.packets.blocking_recv() {
        queued.push(first);
        // Everything already waiting goes in this batch. This is what turns a lock per packet
        // into a lock per wake-up: a lock taken 10 000 times a second is a lock the interface
        // never gets.
        while let Ok(next) = task.packets.try_recv() {
            queued.push(next);
        }

        let mut ready = 0usize;
        let mut reason: Option<String> = None;
        {
            // One decoded-packet buffer for the whole batch. It borrows the packets in
            // `queued`, so it has to be gone before `queued` is cleared — which is what this
            // block is for.
            let mut buffer = decoder.new_packet(&[]);
            for item in &queued {
                let packet = match item {
                    Acquired::Restart => {
                        packets.reset();
                        continue;
                    }
                    Acquired::Packet(packet) => packet,
                };
                if batches.len() <= ready {
                    batches.push(empty_batch());
                }
                let Some(batch) = batches.get_mut(ready) else {
                    continue;
                };
                match packets.decode_into(&decoder, &mut buffer, packet, batch) {
                    Ok(()) => ready += 1,
                    Err(error) => {
                        // `packets_rejected` was written by `decode_into`. What is added here
                        // is the sentence, once per batch: a definition pointed at the wrong
                        // stream rejects every packet, and one line per packet would take the
                        // log's mutex a thousand times to say the same thing a thousand times.
                        if reason.is_none() {
                            reason = Some(error.to_string());
                        }
                    }
                }
            }
        }

        // Both guarded on `ready`. A batch that decoded nothing — every packet refused by a
        // definition pointed at the wrong stream, or a wake-up carrying only a restart marker
        // — has nothing to file and nothing to repaint for. Taking the write lock to run a
        // loop over no batches blocks the interface's read for no reason; calling the waker
        // repaints the window at the packet rate with nothing on it able to change, which is
        // worse than the timer the waker exists to replace.
        if ready > 0 {
            {
                let mut store = task.store.write().unwrap_or_else(PoisonError::into_inner);
                for batch in batches.iter().take(ready) {
                    store.ingest(batch);
                }
            }
            // After the lock is released, never while holding it: a waker that repaints
            // synchronously would put the interface's frame inside the decoder's critical
            // section.
            if let Some(waker) = task.waker.as_ref() {
                waker();
            }
        }

        for event in packets.take_events() {
            log(&task.events, event);
        }
        if let Some(reason) = reason {
            log(&task.events, Event::warning("decode", reason));
        }
        queued.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::UdpSocket;
    use xtce_gs_core::Severity;

    use super::*;

    /// A definition small enough to sit in this file, for the tests that need a session
    /// object and not a decoder.
    const MINIMAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xtce:SpaceSystem name="TestSystem" xmlns:xtce="http://www.omg.org/spec/XTCE/20180204">
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

    /// The JPSS recording and the definition that describes it, in the sibling `xtce-rs`
    /// checkout. 7 200 packets of 71 octets, one APID, no gaps.
    ///
    // Both live under `testdata/`; see `testdata/SOURCES.md` for where they came from.
    const JPSS_DEFINITION: &str = "jpss/jpss1_geolocation_xtce_v1.xml";
    const JPSS_STREAM: &str = "jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1";
    /// Packets in the JPSS recording, per `testdata/SOURCES.md`.
    const JPSS_PACKETS: u64 = 7200;

    fn testdata(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(relative)
    }

    /// The JPSS definition and recording, both of which are in this repository.
    ///
    /// Panics if either is missing. It used to return `None` and let the five tests that need
    /// it return early, which meant they passed without asserting anything — a fixture that
    /// went missing cost nothing and said nothing. The pair is vendored here now, so absence
    /// means deletion, and deletion should be red.
    fn jpss() -> (PathBuf, PathBuf) {
        let definition = testdata(JPSS_DEFINITION);
        let stream = testdata(JPSS_STREAM);
        assert!(
            definition.is_file() && stream.is_file(),
            "{} and {} are vendored in this repository; see testdata/SOURCES.md",
            definition.display(),
            stream.display()
        );
        (definition, stream)
    }

    fn config(definition: PathBuf, source: SourceSpec) -> SessionConfig {
        SessionConfig {
            definition,
            source,
            history_depth: 64,
            ..SessionConfig::default()
        }
    }

    impl Session {
        /// A session with no tasks and no source, for testing what does not need them.
        fn idle() -> Self {
            let db = Arc::new(XtceDb::from_xml(MINIMAL).expect("the fixture is valid XTCE"));
            let (stop, _stop_rx) = watch::channel(false);
            Self {
                config: SessionConfig::default(),
                store: Arc::new(RwLock::new(ParameterStore::new(db.parameters().len(), 8))),
                db,
                stats: Arc::new(LinkStats::new()),
                events: Arc::new(Mutex::new(EventLog::with_capacity(8))),
                limits: LimitSet::new(),
                source: Arc::new(RwLock::new("none".to_owned())),
                running: Arc::new(AtomicBool::new(true)),
                dropped: Arc::new(AtomicU64::new(0)),
                stop,
            }
        }
    }

    fn packet(sequence: u16) -> Acquired {
        let mut bytes = vec![0u8; 7];
        bytes[0..2].copy_from_slice(&0x0800u16.to_be_bytes());
        bytes[2..4].copy_from_slice(&(0xC000 | sequence).to_be_bytes());
        Acquired::Packet(RawPacket::now(bytes))
    }

    fn sequence_of(item: &Acquired) -> Option<u16> {
        match item {
            Acquired::Packet(packet) => packet.sequence_count(),
            Acquired::Restart => None,
        }
    }

    /// Splits a stream of back-to-back space packets into datagrams of whole packets.
    ///
    /// CCSDS 133.0-B-2 section 4.1.3.5.3: the last field of the primary header is the number
    /// of octets in the data field *minus one*, so one packet is six header octets plus that
    /// plus one. `limit` is the most a datagram may carry; a single packet longer than it
    /// still goes out on its own, because splitting it would be the very thing this avoids.
    fn datagrams(stream: &[u8], limit: usize) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let (mut start, mut at) = (0usize, 0usize);
        while at + 6 <= stream.len() {
            let length = usize::from(u16::from_be_bytes([stream[at + 4], stream[at + 5]])) + 7;
            if at + length > stream.len() {
                break;
            }
            if at > start && at + length - start > limit {
                out.push(&stream[start..at]);
                start = at;
            }
            at += length;
        }
        if at > start {
            out.push(&stream[start..at]);
        }
        out
    }

    /// Polls `ready` every millisecond until it holds or `seconds` have passed.
    async fn until(seconds: u64, mut ready: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        while tokio::time::Instant::now() < deadline {
            if ready() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        ready()
    }

    /// The address the acquisition task says it bound, read off the event log.
    ///
    /// `describe_source` cannot answer this — the socket is opened after `start` returns — and
    /// a test that guessed a port would be a test that fails when something else has it.
    async fn bound_address(session: &Session) -> Option<std::net::SocketAddr> {
        let mut found = None;
        until(5, || {
            let events = session
                .events()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (event, _) in events.iter() {
                if let Some(rest) = event.message.strip_prefix("reading udp://") {
                    found = rest.parse().ok();
                    return true;
                }
            }
            false
        })
        .await;
        found
    }

    #[test]
    fn a_poisoned_event_log_still_takes_a_line() {
        let events = Mutex::new(EventLog::with_capacity(4));
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = events.lock().unwrap();
            panic!("the decode thread died here");
        }));
        std::panic::set_hook(hook);
        assert!(poisoned.is_err());
        assert!(events.is_poisoned());

        log(&events, Event::error("session", "and this is why"));

        let guard = events.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(guard.len(), 1);
        assert_eq!(
            guard.last().map(|event| event.severity),
            Some(Severity::Error)
        );
    }

    #[test]
    fn shutting_down_twice_is_fine_and_the_second_time_changes_nothing() {
        let session = Session::idle();
        assert!(session.is_running());
        session.shutdown();
        assert!(!session.is_running());
        session.shutdown();
        assert!(!session.is_running());
        // And once more through `Drop`, which is the third call.
        drop(session);
    }

    /// A session shut down before the acquisition task is ever polled opens nothing.
    ///
    /// The ordering is fixed by the runtime and not by a sleep: this is a current-thread
    /// test, so the task `Session::start` spawned cannot be polled until this future
    /// awaits, and `Session::shutdown` is synchronous. The stop is therefore in the watch
    /// before the task's first poll, every run.
    ///
    /// Two things are asserted, and they are the two that leave a trace: no "reading" line
    /// on the event log and no recording file. An operator who closed the window before it
    /// opened would otherwise be left with a zero-byte recording and a log claiming a station
    /// is reading a source nobody is reading. The third guard on this path — the `select!`
    /// over `Source::connect` — is not separately asserted here, because what it buys is time
    /// on a `tcp://` connect to an unrouted address and a file open leaves nothing behind.
    ///
    /// `stop.is_closed()` is how this waits for the task to have *finished*: the acquisition
    /// task holds the only receiver, and `is_running` cannot answer it because `shutdown`
    /// flips that flag itself.
    #[tokio::test]
    async fn a_session_stopped_before_the_first_poll_opens_neither_source_nor_recording() {
        let base = std::env::temp_dir().join(format!(
            "xtce-gs-unpolled-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let definition = base.with_extension("xml");
        let stream = base.with_extension("dat");
        let recording = base.with_extension("rec");
        std::fs::write(&definition, MINIMAL).expect("a writable temporary directory");
        std::fs::write(&stream, [0u8; 64]).expect("a writable temporary directory");
        let _ = std::fs::remove_file(&recording);

        let mut config = config(
            definition.clone(),
            SourceSpec::File {
                path: stream.clone(),
                bytes_per_second: None,
                chunk: 16,
                repeat: false,
            },
        );
        config.record = Some(recording.clone());
        let session = Session::start(config, &Handle::current(), None)
            .expect("the fixture definition loads and the configuration is sound");
        // Nothing has awaited since `start` returned, so the task has not run yet.
        session.shutdown();

        let finished = until(5, || session.stop.is_closed()).await;
        let recorded = recording.exists();
        let opened = {
            let events = session
                .events()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            events
                .iter()
                .any(|(event, _)| event.message.starts_with("reading "))
        };
        let _ = std::fs::remove_file(&definition);
        let _ = std::fs::remove_file(&stream);
        let _ = std::fs::remove_file(&recording);

        assert!(finished, "the acquisition task never ended");
        assert!(
            !opened,
            "the task logged that it was reading a source the session had already stopped"
        );
        assert!(
            !recorded,
            "a session stopped before its first poll left a recording behind"
        );
    }

    /// A restart marker is never the thing a full queue throws away.
    ///
    /// `tcp-listen://` is both lossy and able to restart, so this is the state a feeder
    /// reconnecting into a station whose decoder is behind actually reaches. Losing the
    /// marker there leaves `SequenceTracker` comparing counts across a stream boundary, and a
    /// pass then reports thousands of packets missing that were never sent.
    #[tokio::test]
    async fn a_full_live_queue_drops_a_packet_rather_than_a_restart() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_stop_tx, mut stop) = watch::channel(false);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut dispatch = Dispatch::new(tx, 2, true, Arc::clone(&dropped));

        // Fill the channel and the ring, then restart, then keep sending.
        for sequence in 0..3u16 {
            dispatch
                .send(packet(sequence), &mut stop)
                .await
                .expect("live");
        }
        dispatch
            .send(Acquired::Restart, &mut stop)
            .await
            .expect("live");
        for sequence in 3..8u16 {
            dispatch
                .send(packet(sequence), &mut stop)
                .await
                .expect("live");
        }

        let mut seen_restart = false;
        let mut packets = Vec::new();
        while let Ok(item) = rx.try_recv() {
            match &item {
                Acquired::Restart => seen_restart = true,
                Acquired::Packet(_) => packets.extend(sequence_of(&item)),
            }
            if dispatch.flush().is_err() {
                break;
            }
        }
        while let Ok(item) = rx.try_recv() {
            match &item {
                Acquired::Restart => seen_restart = true,
                Acquired::Packet(_) => packets.extend(sequence_of(&item)),
            }
        }

        assert!(
            seen_restart,
            "the restart marker was evicted; the decoder will never reset at the boundary"
        );
        assert!(
            dropped.load(Ordering::Relaxed) > 0,
            "the fixture did not actually overflow the queue"
        );
        assert!(
            packets.windows(2).all(|pair| pair[0] < pair[1]),
            "packets came back out of order: {packets:?}"
        );
    }

    /// The oldest packet *in the ring*, which is not the oldest packet in flight.
    ///
    /// Named for what it proves and not for the policy: up to one channel's worth of older
    /// packets have already been handed to the decoder and are decoded normally, so the
    /// eviction is one queue late. That is the module header's description and the subject of
    /// the `TODO(gs-engine-session)` above it; a test called "drops the oldest packet" would
    /// read as the specification that the eviction is exact.
    #[tokio::test]
    async fn a_full_live_queue_drops_the_oldest_queued_packet_and_counts_it() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_stop_tx, mut stop) = watch::channel(false);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut dispatch = Dispatch::new(tx, 2, true, Arc::clone(&dropped));

        for sequence in 0..5u16 {
            dispatch
                .send(packet(sequence), &mut stop)
                .await
                .expect("a live source never gives up on a full queue");
        }

        // One slot in the channel, two in the ring: packet 0 was handed over, 3 and 4 are the
        // newest survivors, and 1 and 2 — the oldest still in hand — are what went.
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        assert_eq!(rx.try_recv().as_ref().ok().and_then(sequence_of), Some(0));
        dispatch.flush().expect("the receiver is still here");
        assert_eq!(rx.try_recv().as_ref().ok().and_then(sequence_of), Some(3));
        dispatch.flush().expect("the receiver is still here");
        assert_eq!(rx.try_recv().as_ref().ok().and_then(sequence_of), Some(4));
    }

    #[tokio::test]
    async fn a_replay_waits_for_room_rather_than_dropping() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_stop_tx, mut stop) = watch::channel(false);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut dispatch = Dispatch::new(tx, 2, false, Arc::clone(&dropped));

        dispatch.send(packet(0), &mut stop).await.expect("room");
        // The channel is full now. The send must not return until something is taken out.
        let waiting = dispatch.send(packet(1), &mut stop);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err(),
            "a replay must block, not drop"
        );
        assert_eq!(rx.try_recv().as_ref().ok().and_then(sequence_of), Some(0));
        waiting.await.expect("room appeared");
        assert_eq!(rx.try_recv().as_ref().ok().and_then(sequence_of), Some(1));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    /// A recording cut off mid-packet must say so.
    ///
    /// A space packet declares its own length, so a stream that ends inside one leaves the
    /// assembler holding a partial that nothing can complete. Before the flush at the
    /// `Ok(0)` arm it went with the pipeline: no counter, no event, a clean exit and a packet
    /// count one short of what the operator counted in the file. The same hole was found in
    /// `xtce-gs export` first, which is why this one is here.
    #[tokio::test]
    async fn a_stream_that_ends_inside_a_packet_reports_the_partial() {
        let (definition, stream) = jpss();
        // Ten whole JPSS packets are 710 octets; 750 is ten and 40 octets of an eleventh.
        let whole = std::fs::read(&stream).expect("the recording reads");
        let truncated = std::env::temp_dir().join("xtce-gs-truncated.dat");
        std::fs::write(&truncated, &whole[..750]).expect("the fixture writes");

        let session = Session::start(
            config(
                definition,
                SourceSpec::File {
                    path: truncated.clone(),
                    bytes_per_second: None,
                    chunk: 4096,
                    repeat: false,
                },
            ),
            &Handle::current(),
            None,
        )
        .expect("the configuration is sound");

        let ended = until(120, || !session.is_running()).await;
        let snapshot = session.stats().snapshot();
        assert!(
            ended,
            "the replay did not finish; counters were {snapshot:?}"
        );
        assert_eq!(
            snapshot.packets_decoded, 10,
            "ten whole packets are in 750 octets"
        );
        assert_eq!(
            snapshot.packets_lost, 1,
            "the 40 octets of the eleventh went unreported"
        );
        let log = session
            .events()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            log.iter()
                .any(|(event, _)| event.message.contains("partial")),
            "nothing in the log says a packet was abandoned"
        );
        drop(log);
        let _ = std::fs::remove_file(&truncated);
    }

    #[tokio::test]
    async fn a_shut_down_replay_stops_waiting_for_room() {
        let (tx, _rx) = mpsc::channel(1);
        let (stop_tx, mut stop) = watch::channel(false);
        let mut dispatch = Dispatch::new(tx, 2, false, Arc::new(AtomicU64::new(0)));

        dispatch.send(packet(0), &mut stop).await.expect("room");
        let _previous = stop_tx.send_replace(true);
        // The queue is full and shutdown has landed: this must return rather than wait for a
        // decoder nobody is going to run again.
        assert!(dispatch.send(packet(1), &mut stop).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_definition_that_is_not_there_is_an_error_and_not_a_panic() {
        let config = config(
            testdata("no/such/definition.xml"),
            SourceSpec::Udp(([127, 0, 0, 1], 0).into()),
        );
        let started = Session::start(config, &Handle::current(), None);
        assert!(
            matches!(started, Err(EngineError::Xtce(_))),
            "a missing definition is an EngineError::Xtce"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_udp_session_fills_the_store_and_moves_the_counters() {
        let (definition, stream) = jpss();
        let bytes = std::fs::read(stream).expect("the recording is readable");

        let session = Session::start(
            config(definition, SourceSpec::Udp(([127, 0, 0, 1], 0).into())),
            &Handle::current(),
            None,
        )
        .expect("the JPSS definition loads and the configuration is sound");
        let address = bound_address(&session)
            .await
            .expect("the acquisition task logs the address it bound");

        // Whole packets per datagram. `flush_message` throws away a half-assembled packet at
        // every message boundary, so a datagram that ends mid-packet is a packet deliberately
        // lost — and the boundaries come out of the packet headers rather than out of this
        // recording happening to hold packets all of one length.
        let feeder = UdpSocket::bind("127.0.0.1:0").await.expect("a free port");
        let mut sent = 0usize;
        for datagram in datagrams(&bytes, 1400).iter().take(20) {
            if feeder.send_to(datagram, address).await.is_ok() {
                sent += 1;
            }
            // Localhost UDP drops when the receive buffer fills, and this test is about the
            // session and not about the kernel's socket buffer.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(sent > 0, "the feeder sent something");

        let filled = until(10, || {
            let store = session
                .store()
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            store.seen_count() > 0 && session.stats().snapshot().packets_decoded > 0
        })
        .await;
        let snapshot = session.stats().snapshot();
        assert!(filled, "the store never filled; counters were {snapshot:?}");
        assert!(snapshot.bytes_in > 0);
        assert!(snapshot.packets_in > 0);
        assert!(snapshot.last_packet.is_some());
        assert!(session.is_running(), "a UDP source does not end on its own");

        session.shutdown();
        assert!(!session.is_running());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recording_holds_every_byte_the_source_produced() {
        let (definition, stream) = jpss();
        let original = std::fs::read(&stream).expect("the recording is readable");
        let into = std::env::temp_dir().join(format!(
            "xtce-gs-session-{}-{:?}.dat",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&into);

        let mut config = config(
            definition,
            SourceSpec::File {
                path: stream,
                bytes_per_second: None,
                chunk: 4096,
                repeat: false,
            },
        );
        config.record = Some(into.clone());
        let session = Session::start(config, &Handle::current(), None)
            .expect("the JPSS definition loads and the configuration is sound");

        // `is_running` goes false in the guard that drops *after* the recorder is flushed, so
        // this is also how a caller knows the file is complete.
        assert!(
            until(120, || !session.is_running()).await,
            "the replay did not finish"
        );

        let recorded = std::fs::read(&into).expect("the recording was written");
        let _ = std::fs::remove_file(&into);
        assert_eq!(
            recorded.len(),
            original.len(),
            "a recording is every byte the source produced, before framing"
        );
        assert!(recorded == original, "and in the order it produced them");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replay_that_wraps_does_not_report_the_wrap_as_a_loss() {
        let (definition, stream) = jpss();
        // The recording's sequence counts run 2 606 to 9 805 without a break. Read twice
        // without telling the decoder the stream restarted, 9 805 is followed by 2 606 and the
        // tracker reports 9 184 packets missing that were never sent. `Acquired::Restart` is
        // what stops that, and this is the assertion that proves it travelled.
        let session = Session::start(
            config(
                definition,
                SourceSpec::File {
                    path: stream,
                    bytes_per_second: None,
                    chunk: 65_536,
                    repeat: true,
                },
            ),
            &Handle::current(),
            None,
        )
        .expect("the JPSS definition loads and the configuration is sound");

        let twice = JPSS_PACKETS * 2;
        let wrapped = until(120, || session.stats().snapshot().packets_decoded >= twice).await;
        let snapshot = session.stats().snapshot();
        assert!(
            wrapped,
            "the replay did not wrap; counters were {snapshot:?}"
        );
        assert_eq!(snapshot.sequence_gaps, 0, "the wrap is not a gap");
        assert_eq!(snapshot.sequence_missing, 0);
        assert_eq!(
            snapshot.packets_lost, 0,
            "the file ends on a packet boundary"
        );
        assert!(session.is_running(), "a repeating replay does not end");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_replay_decodes_every_packet_and_then_stops() {
        let (definition, stream) = jpss();
        let source = SourceSpec::File {
            path: stream,
            bytes_per_second: None,
            chunk: 65_536,
            repeat: false,
        };
        let session = Session::start(config(definition, source), &Handle::current(), None)
            .expect("the JPSS definition loads and the configuration is sound");

        let done = until(120, || {
            session.stats().snapshot().packets_decoded >= JPSS_PACKETS
        })
        .await;
        let snapshot = session.stats().snapshot();
        assert!(
            done,
            "the replay did not finish; counters were {snapshot:?}"
        );
        assert_eq!(snapshot.packets_in, JPSS_PACKETS, "the link found them all");
        assert_eq!(
            snapshot.packets_decoded, JPSS_PACKETS,
            "the definition describes every packet in this recording"
        );
        assert_eq!(snapshot.packets_rejected, 0);
        assert_eq!(session.dropped(), 0, "a replay waits rather than dropping");

        // The acquisition task ends when the file does, and nothing was dropped on the way.
        assert!(
            until(5, || !session.is_running()).await,
            "a replay that ran out of file is not running any more"
        );
        let store = session
            .store()
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(store.seen_count() > 0, "the store kept what was decoded");
        assert_eq!(store.ingested(), JPSS_PACKETS);
    }
}
