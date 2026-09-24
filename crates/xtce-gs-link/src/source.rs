//! Where the bytes come from.
//!
//! Three transports and a file, behind one `async` call that appends bytes to a buffer. The
//! synchroniser downstream is fed bytes and finds its own frames, so the difference between
//! them is not a difference in framing — it is only [`Source::is_datagram`], which tells the
//! pipeline whether a message boundary exists to restart at.
//!
//! # TODO(gs-link-serial): a serial source would attach here
//!
//! It would be a fifth [`SourceSpec`] variant (`serial:///dev/ttyUSB0?baud=115200`) and a
//! fifth [`Source`] variant wrapping a port, with the same `read_chunk` shape as
//! [`TcpSource`] — a byte stream with no message boundaries, which is exactly what the
//! synchroniser already expects. It is absent because `serialport` is a binding to a C
//! library on some platforms, which would make this workspace's build depend on a toolchain
//! it does not otherwise need, and because the radios in reach speak UDP. Add it when a
//! mission arrives with a modem that does not.

use std::fmt;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::error::LinkError;

/// Bytes read from a stream source in one call, when the URL does not say.
pub const DEFAULT_CHUNK: usize = 4096;

/// The largest datagram IPv4 can carry, and therefore the most one `recv` can produce.
pub const MAX_DATAGRAM: usize = 65_535;

/// What [`SourceSpec::from_str`] will accept, quoted back when it is given something else.
const ACCEPTED: &str = "udp://host:port, tcp://host:port, tcp-listen://host:port, \
                        file:///path?rate=&chunk=&repeat, or a bare path";

/// A read buffer that does not print itself.
///
/// The sources read into their own scratch and copy out, rather than growing the caller's
/// buffer and truncating it back: `read_chunk` is awaited inside a `tokio::select!` against
/// the session's shutdown, and a future cancelled between the grow and the truncate would
/// leave the caller holding a chunk of zeroes it never received. Copying the bytes that did
/// arrive costs a `memcpy` of what was read; the alternative costs correctness.
///
/// `Debug` is hand-written because the UDP scratch is 64 KiB and a derived one would render
/// every byte of it into whatever log line asked for the source.
struct Scratch(Vec<u8>);

impl Scratch {
    /// A scratch buffer of `size` readable bytes, allocated once.
    fn new(size: usize) -> Self {
        Self(vec![0; size])
    }
}

impl fmt::Debug for Scratch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} bytes]", self.0.len())
    }
}

/// Where a session gets its bytes, before anything has been opened.
///
/// Parsed from a URL so that a source survives a config file, a command line and a saved
/// layout without a builder in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSpec {
    /// Bind a UDP socket and read datagrams.
    Udp(SocketAddr),
    /// Connect a TCP socket to a server that is already listening.
    TcpConnect(SocketAddr),
    /// Listen for TCP and read from whoever connects.
    TcpListen(SocketAddr),
    /// Replay a file of raw bytes.
    File {
        /// The file to read.
        path: PathBuf,
        /// Replay speed. `None` reads as fast as the disk allows, which is right for a test
        /// and wrong for anything an operator is watching.
        bytes_per_second: Option<u64>,
        /// Bytes per read.
        chunk: usize,
        /// Whether to start over at the end instead of ending the session.
        repeat: bool,
    },
}

/// Parses `host:port` for one scheme, naming the scheme when it will not parse.
fn parse_addr(scheme: &str, rest: &str) -> Result<SocketAddr, LinkError> {
    if rest.is_empty() {
        return Err(LinkError::Config(format!(
            "{scheme}: expected an address and a port, as in {scheme}127.0.0.1:10015"
        )));
    }
    rest.parse::<SocketAddr>().map_err(|error| {
        LinkError::Config(format!(
            "{scheme}: expected an address and a port, as in {scheme}127.0.0.1:10015, \
             but \"{rest}\" is not one ({error})"
        ))
    })
}

/// Parses everything after `file://`: a path, then the replay options.
fn parse_file(rest: &str) -> Result<SourceSpec, LinkError> {
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, query),
        None => (rest, ""),
    };
    if path.is_empty() {
        return Err(LinkError::Config(
            "file://: expected a path, as in file:///var/tmp/pass.dat".to_owned(),
        ));
    }

    let mut bytes_per_second = None;
    let mut chunk = DEFAULT_CHUNK;
    let mut repeat = false;
    for item in query.split('&').filter(|item| !item.is_empty()) {
        let (key, value) = match item.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (item, None),
        };
        match (key, value) {
            // A rate of zero is no rate limit rather than a replay that never advances, and
            // it is normalised here so that `Display` round-trips back to the same spec.
            ("rate", Some(value)) => {
                let rate: u64 = value.parse().map_err(|error| {
                    LinkError::Config(format!(
                        "file://: rate must be bytes per second, but \"{value}\" is not a \
                         number ({error})"
                    ))
                })?;
                bytes_per_second = (rate > 0).then_some(rate);
            }
            ("chunk", Some(value)) => {
                let bytes: usize = value.parse().map_err(|error| {
                    LinkError::Config(format!(
                        "file://: chunk must be a number of bytes, but \"{value}\" is not a \
                         number ({error})"
                    ))
                })?;
                chunk = if bytes == 0 { DEFAULT_CHUNK } else { bytes };
            }
            ("repeat", None) => repeat = true,
            ("repeat", Some(_)) => {
                return Err(LinkError::Config(
                    "file://: repeat is a flag, written \"repeat\" with no value".to_owned(),
                ));
            }
            ("rate" | "chunk", None) => {
                return Err(LinkError::Config(format!(
                    "file://: {key} needs a value, as in {key}=4096"
                )));
            }
            _ => {
                return Err(LinkError::Config(format!(
                    "file://: unknown option \"{key}\"; expected rate, chunk or repeat"
                )));
            }
        }
    }

    Ok(SourceSpec::File {
        path: PathBuf::from(path),
        bytes_per_second,
        chunk,
        repeat,
    })
}

impl FromStr for SourceSpec {
    type Err = LinkError;

    /// Parses one of the four URL forms.
    ///
    /// ```text
    /// udp://0.0.0.0:10015
    /// tcp://127.0.0.1:10015
    /// tcp-listen://0.0.0.0:10015
    /// file:///path/to/stream.dat?rate=1000000&chunk=4096&repeat
    /// /path/to/stream.dat
    /// ```
    ///
    /// A string with no scheme is a file path, because that is what an operator types and
    /// because no other variant can be named by a bare word. `repeat` is a bare flag rather
    /// than `repeat=true`: it is either there or it is not.
    ///
    /// The query is parsed only after `file://`. A bare path is taken whole, `?` and all,
    /// because `?` is a legal character in a file name on every platform this runs on and
    /// splitting one silently would open a different file than the operator typed. The cost
    /// is that a `file://` URL cannot name a path containing `?`; such a path has to be given
    /// bare, and the replay options then come from the command line instead.
    ///
    /// # Errors
    ///
    /// [`LinkError::Config`] naming what could not be parsed — an unknown scheme, an address
    /// without a port, a `rate` or `chunk` that is not a number. A file path is not checked
    /// for existence here; that is [`Source::connect`]'s failure to report, and reporting it
    /// twice means reporting it differently twice.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(LinkError::Config(format!(
                "empty source: expected {ACCEPTED}"
            )));
        }
        if let Some(rest) = s.strip_prefix("udp://") {
            return Ok(Self::Udp(parse_addr("udp://", rest)?));
        }
        if let Some(rest) = s.strip_prefix("tcp-listen://") {
            return Ok(Self::TcpListen(parse_addr("tcp-listen://", rest)?));
        }
        if let Some(rest) = s.strip_prefix("tcp://") {
            return Ok(Self::TcpConnect(parse_addr("tcp://", rest)?));
        }
        if let Some(rest) = s.strip_prefix("file://") {
            return parse_file(rest);
        }
        if let Some((scheme, _)) = s.split_once("://") {
            return Err(LinkError::Config(format!(
                "unknown source scheme \"{scheme}://\": expected {ACCEPTED}"
            )));
        }
        Ok(Self::File {
            path: PathBuf::from(s),
            bytes_per_second: None,
            chunk: DEFAULT_CHUNK,
            repeat: false,
        })
    }
}

impl fmt::Display for SourceSpec {
    /// The URL this was parsed from, close enough to round-trip through [`FromStr`].
    ///
    /// The status bar and the recorded session header both want one line naming the source,
    /// and a `Debug` rendering of a `PathBuf` is not it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Udp(addr) => write!(f, "udp://{addr}"),
            Self::TcpConnect(addr) => write!(f, "tcp://{addr}"),
            Self::TcpListen(addr) => write!(f, "tcp-listen://{addr}"),
            Self::File {
                path,
                bytes_per_second,
                chunk,
                repeat,
            } => {
                write!(f, "file://{}", path.display())?;
                let mut separator = '?';
                if let Some(rate) = bytes_per_second {
                    write!(f, "{separator}rate={rate}")?;
                    separator = '&';
                }
                write!(f, "{separator}chunk={chunk}")?;
                if *repeat {
                    write!(f, "&repeat")?;
                }
                Ok(())
            }
        }
    }
}

/// An open source, reading.
///
/// One variant per [`SourceSpec`] variant, each wrapping the transport in a type that owns
/// whatever state reading it needs. The variants hold those wrappers rather than the `tokio`
/// handles directly so that a socket can grow a scratch buffer or a reconnect count without
/// that becoming part of this enum's shape.
#[derive(Debug)]
pub enum Source {
    /// A bound UDP socket.
    Udp(UdpSource),
    /// A TCP connection this station opened.
    TcpConnect(TcpSource),
    /// A TCP port this station is listening on.
    TcpListen(TcpListenSource),
    /// A file being replayed.
    File(FileReplay),
}

impl Source {
    /// Opens the source the spec names.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the address cannot be bound, the peer refuses the connection,
    /// or the file cannot be opened. A `tcp-listen` source binds here and does *not* wait for
    /// a peer — a session that blocked in `connect` would show an operator an empty window
    /// with no explanation until someone dialled in.
    pub async fn connect(spec: &SourceSpec) -> Result<Self, LinkError> {
        match spec {
            SourceSpec::Udp(addr) => Ok(Self::Udp(UdpSource::bind(*addr).await?)),
            SourceSpec::TcpConnect(addr) => Ok(Self::TcpConnect(TcpSource::connect(*addr).await?)),
            SourceSpec::TcpListen(addr) => Ok(Self::TcpListen(TcpListenSource::bind(*addr).await?)),
            SourceSpec::File {
                path,
                bytes_per_second,
                chunk,
                repeat,
            } => Ok(Self::File(
                FileReplay::open(path, *chunk, *bytes_per_second, *repeat).await?,
            )),
        }
    }

    /// Appends the next chunk of bytes to `buf`. `Ok(0)` means the source ended.
    ///
    /// Appends rather than fills, so the caller owns one buffer for the whole session and the
    /// read never allocates once it has grown.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] for a transport failure, and [`LinkError::Ended`] for a read issued
    /// after a previous read already returned `Ok(0)`.
    pub async fn read_chunk(&mut self, buf: &mut Vec<u8>) -> Result<usize, LinkError> {
        match self {
            Self::Udp(source) => source.read_chunk(buf).await,
            Self::TcpConnect(source) => source.read_chunk(buf).await,
            Self::TcpListen(source) => source.read_chunk(buf).await,
            Self::File(source) => source.read_chunk(buf).await,
        }
    }

    /// Whether the byte stream restarted since the last call: a new TCP peer, or a replay
    /// that wrapped. Drained by the read — the caller acts on it once.
    ///
    /// The acquisition task calls this after every [`Source::read_chunk`] and, when it is
    /// true, calls `Pipeline::flush_message` and tells the decode side to drop its partial
    /// state: framing and sequence counts from before the restart describe a different
    /// stream, and carrying them across the boundary turns one restart into a run of
    /// spurious gaps. A UDP socket and an outgoing TCP connection never restart — a UDP
    /// source restarts framing at every datagram anyway, and a closed TCP connection is the
    /// end of the session rather than the start of a new stream.
    pub fn take_restart(&mut self) -> bool {
        match self {
            Self::Udp(_) | Self::TcpConnect(_) => false,
            Self::TcpListen(source) => source.take_restart(),
            Self::File(source) => source.take_restart(),
        }
    }

    /// One line naming what this is reading, for the status bar and the event log.
    ///
    /// The *bound* address rather than the configured one, because `:0` is a legal thing to
    /// ask for and `udp://0.0.0.0:0` tells an operator nothing about where to point a feeder.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Udp(source) => match source.socket.local_addr() {
                Ok(addr) => format!("udp://{addr}"),
                Err(error) => format!("udp://(unbound: {error})"),
            },
            Self::TcpConnect(source) => match source.stream.peer_addr() {
                Ok(addr) => format!("tcp://{addr}"),
                Err(error) => format!("tcp://(disconnected: {error})"),
            },
            Self::TcpListen(source) => {
                let local = match source.listener.local_addr() {
                    Ok(addr) => addr.to_string(),
                    Err(error) => format!("(unbound: {error})"),
                };
                match source.stream.as_ref().map(TcpStream::peer_addr) {
                    Some(Ok(peer)) => format!("tcp-listen://{local} (peer {peer})"),
                    Some(Err(error)) => format!("tcp-listen://{local} (peer: {error})"),
                    None => format!("tcp-listen://{local} (no peer)"),
                }
            }
            Self::File(source) => SourceSpec::File {
                path: source.path.clone(),
                bytes_per_second: source.limiter.as_ref().map(RateLimiter::bytes_per_second),
                chunk: source.chunk,
                repeat: source.repeat,
            }
            .to_string(),
        }
    }

    /// Whether reads arrive with message boundaries.
    ///
    /// True only for UDP. The pipeline restarts framing at each boundary on a datagram
    /// source, because a datagram that was lost took a whole message with it and the bytes
    /// either side of the gap do not belong to one frame.
    #[must_use]
    pub const fn is_datagram(&self) -> bool {
        matches!(self, Self::Udp(_))
    }
}

/// A bound UDP socket.
///
/// One read is one datagram: a short read does not exist here, and a datagram larger than
/// [`MAX_DATAGRAM`] cannot arrive.
#[derive(Debug)]
pub struct UdpSource {
    socket: UdpSocket,
    scratch: Scratch,
}

impl UdpSource {
    /// Binds a socket to `addr`.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the address is in use or not local.
    pub async fn bind(addr: SocketAddr) -> Result<Self, LinkError> {
        Ok(Self {
            socket: UdpSocket::bind(addr).await?,
            scratch: Scratch::new(MAX_DATAGRAM),
        })
    }

    /// The socket itself.
    ///
    /// Exposed because a station on a multicast feed has to call `join_multicast_v4` on it
    /// and the URL grammar has no syntax for a group. Nothing here does it for the caller.
    #[must_use]
    pub const fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    /// Appends one datagram to `buf` and returns how many bytes it held.
    ///
    /// **Never returns `Ok(0)`.** A bound UDP socket has no end — a sender that stopped is
    /// indistinguishable from one that has not started, and the socket stays readable either
    /// way — and a zero-length datagram is a legal keepalive that several stacks send. So an
    /// empty datagram is waited past, not reported: returning `Ok(0)` for one would end the
    /// session on the first keepalive.
    ///
    /// Having no end has one consequence worth stating, because nothing else states it: the
    /// step that hands a session's queued packets on before it finishes hangs off the
    /// end-of-stream arm of the acquisition loop alone, so no way out of that loop drains for
    /// a `udp://` source, and whatever is still queued when the session stops is discarded —
    /// counted neither as decoded nor as dropped. That is deliberate: the queue is a jitter
    /// buffer between a socket and a decoder, not a store.
    ///
    /// The scratch is [`MAX_DATAGRAM`] bytes because a datagram that does not fit the buffer
    /// is silently truncated by every Berkeley-sockets stack: the surplus is discarded by the
    /// kernel and nothing reports it, so a short buffer would corrupt one frame per oversized
    /// datagram with no counter moving.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] for a socket failure.
    pub async fn read_chunk(&mut self, buf: &mut Vec<u8>) -> Result<usize, LinkError> {
        loop {
            let read = self.socket.recv(&mut self.scratch.0).await?;
            if read == 0 {
                continue;
            }
            buf.extend_from_slice(&self.scratch.0[..read]);
            return Ok(read);
        }
    }
}

/// A TCP connection this station opened.
#[derive(Debug)]
pub struct TcpSource {
    stream: TcpStream,
    scratch: Scratch,
    ended: bool,
}

impl TcpSource {
    /// Connects to `addr`.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the peer refuses or is unreachable. There is no retry here: a
    /// session that silently reconnects is a session whose status bar lies about the gap.
    pub async fn connect(addr: SocketAddr) -> Result<Self, LinkError> {
        Ok(Self {
            stream: TcpStream::connect(addr).await?,
            scratch: Scratch::new(DEFAULT_CHUNK),
            ended: false,
        })
    }

    /// The stream itself, for `set_nodelay` and for naming the peer.
    #[must_use]
    pub const fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// Whether the peer has closed the connection.
    #[must_use]
    pub const fn has_ended(&self) -> bool {
        self.ended
    }

    /// Appends up to [`DEFAULT_CHUNK`] bytes to `buf`. `Ok(0)` means the peer closed.
    ///
    /// The end is remembered: `Ok(0)` is returned once, and every read after it is
    /// [`LinkError::Ended`], so a caller that does not stop on the zero is told rather than
    /// left spinning on an endless run of them.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] for a socket failure, [`LinkError::Ended`] after the close.
    pub async fn read_chunk(&mut self, buf: &mut Vec<u8>) -> Result<usize, LinkError> {
        if self.ended {
            return Err(LinkError::Ended);
        }
        let read = self.stream.read(&mut self.scratch.0).await?;
        if read == 0 {
            self.ended = true;
            return Ok(0);
        }
        buf.extend_from_slice(&self.scratch.0[..read]);
        Ok(read)
    }
}

/// A TCP port this station listens on, with at most one peer at a time.
///
/// At most one: a downlink is one stream, and two peers interleaving into one synchroniser
/// would produce frames that belong to neither. A second dialler is therefore left in the
/// listen backlog until the first one hangs up, rather than accepted and mixed in; the
/// alternative — accepting both and picking one — is a station that silently ignores the
/// feeder an operator just started.
#[derive(Debug)]
pub struct TcpListenSource {
    listener: TcpListener,
    stream: Option<TcpStream>,
    scratch: Scratch,
    restart: bool,
}

impl TcpListenSource {
    /// Binds the listening socket. Does not wait for a peer.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the address is in use or not local.
    pub async fn bind(addr: SocketAddr) -> Result<Self, LinkError> {
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
            stream: None,
            scratch: Scratch::new(DEFAULT_CHUNK),
            restart: false,
        })
    }

    /// The listening socket, for naming the bound address.
    #[must_use]
    pub const fn listener(&self) -> &TcpListener {
        &self.listener
    }

    /// Whether a peer is connected right now.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Whether a peer was accepted since this was last asked. Drained by the call.
    ///
    /// Set on every accept, the first one included: the bytes of a new connection are not a
    /// continuation of the last one, and flushing an empty pipeline costs nothing while
    /// special-casing the first peer is a branch that can only ever be wrong.
    pub fn take_restart(&mut self) -> bool {
        std::mem::take(&mut self.restart)
    }

    /// Appends bytes from the connected peer, accepting one first if none is connected.
    ///
    /// **A closed peer is not the end of the session.** When the peer disconnects this waits
    /// for the next one and keeps reading, because a ground station left listening overnight
    /// should still be listening in the morning, and an operator who restarts the downlink
    /// feeder should not have to restart the station too. So this never returns `Ok(0)` at
    /// all: a disconnect goes back round the loop to accept the next peer, and a listener
    /// that has failed is the [`LinkError::Io`] below rather than a zero. The pipeline is
    /// told to restart framing at each new peer through [`TcpListenSource::take_restart`],
    /// since the bytes either side of a disconnection are not one frame.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the listener fails. A failed `accept` of one peer is not an
    /// error: it is waited through. A read that fails the way a dropped connection fails —
    /// reset, aborted, broken pipe, timed out — is a disconnect and is waited through too,
    /// because those describe the peer and not this socket.
    pub async fn read_chunk(&mut self, buf: &mut Vec<u8>) -> Result<usize, LinkError> {
        loop {
            // Both arms of this `else` continue: a peer that was just accepted is read on
            // the next turn of the loop, through the `Some` branch, and never here.
            let Some(stream) = self.stream.as_mut() else {
                match self.listener.accept().await {
                    Ok((stream, _peer)) => {
                        self.stream = Some(stream);
                        self.restart = true;
                    }
                    Err(error) if per_connection(&error) => continue,
                    Err(error) => return Err(error.into()),
                }
                continue;
            };

            match stream.read(&mut self.scratch.0).await {
                Ok(0) => self.stream = None,
                Ok(read) => {
                    buf.extend_from_slice(&self.scratch.0[..read]);
                    return Ok(read);
                }
                Err(error) if per_connection(&error) => self.stream = None,
                Err(error) => {
                    self.stream = None;
                    return Err(error.into());
                }
            }
        }
    }
}

/// Whether an `io::Error` describes one connection rather than the socket underneath it.
///
/// `accept` reports a connection the client abandoned during the handshake as an error of the
/// listener, and a peer that vanishes reports as a reset rather than a clean close. Neither
/// says anything about this station's ability to keep listening, so neither ends the session.
///
/// `TimedOut` is deliberately *not* in the set. On a socket with no receive timeout it means
/// the keepalives failed, which is a link this station should report rather than a peer it
/// should quietly re-accept in a loop nobody sees.
fn per_connection(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{
        BrokenPipe, ConnectionAborted, ConnectionReset, Interrupted, NotConnected,
    };
    matches!(
        error.kind(),
        ConnectionAborted | ConnectionReset | BrokenPipe | NotConnected | Interrupted
    )
}

/// A file replayed as if it were a downlink.
///
/// The point of the rate limit is that a recorded pass replayed at disk speed arrives in
/// milliseconds, and every plot the operator was meant to read becomes one vertical line.
#[derive(Debug)]
pub struct FileReplay {
    file: tokio::fs::File,
    path: PathBuf,
    chunk: usize,
    repeat: bool,
    limiter: Option<RateLimiter>,
    bytes_read: u64,
    scratch: Scratch,
    ended: bool,
    restart: bool,
}

impl FileReplay {
    /// Opens `path` for replay.
    ///
    /// A `chunk` of zero is replaced by [`DEFAULT_CHUNK`]; a `bytes_per_second` of zero
    /// disables the limiter rather than stopping the replay forever.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the file cannot be opened. The path is put into the message,
    /// because "No such file or directory" on its own is what an operator reads at the top of
    /// an empty window and cannot act on.
    pub async fn open(
        path: &Path,
        chunk: usize,
        bytes_per_second: Option<u64>,
        repeat: bool,
    ) -> Result<Self, LinkError> {
        let file = tokio::fs::File::open(path).await.map_err(|error| {
            std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        let chunk = if chunk == 0 { DEFAULT_CHUNK } else { chunk };
        Ok(Self {
            file,
            path: path.to_path_buf(),
            chunk,
            repeat,
            limiter: bytes_per_second
                .filter(|rate| *rate > 0)
                .map(RateLimiter::new),
            bytes_read: 0,
            scratch: Scratch::new(chunk),
            ended: false,
            restart: false,
        })
    }

    /// The file being replayed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes per read.
    #[must_use]
    pub const fn chunk(&self) -> usize {
        self.chunk
    }

    /// Whether the replay starts over at the end.
    #[must_use]
    pub const fn repeats(&self) -> bool {
        self.repeat
    }

    /// Bytes handed out so far, wrapped replays included.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// The rate limiter, if the replay has one.
    #[must_use]
    pub const fn limiter(&self) -> Option<&RateLimiter> {
        self.limiter.as_ref()
    }

    /// Whether the replay has run to its end. Cumulative counts cannot answer this.
    ///
    /// [`FileReplay::bytes_read`] counts across wraps and never resets, so it says nothing
    /// about whether the last read hit the end.
    #[must_use]
    pub const fn has_ended(&self) -> bool {
        self.ended
    }

    /// Whether the replay wrapped since this was last asked. Drained by the call.
    pub fn take_restart(&mut self) -> bool {
        std::mem::take(&mut self.restart)
    }

    /// Appends up to [`FileReplay::chunk`] bytes to `buf`, paced by the rate limiter.
    ///
    /// The limiter is charged *after* the read and for what the read actually produced:
    /// charging first makes the last, short chunk of a file take a full chunk's worth of time
    /// to not arrive.
    ///
    /// At most one rewind happens per call. A `repeat` replay of an empty file would
    /// otherwise read zero, seek to zero, and do it again forever without ever awaiting
    /// anything — a busy loop with no bytes in it — so a rewind that still yields nothing
    /// ends the replay.
    ///
    /// # Errors
    ///
    /// [`LinkError::Io`] when the read or the rewind fails, and [`LinkError::Ended`] for a
    /// read after the replay already ended.
    pub async fn read_chunk(&mut self, buf: &mut Vec<u8>) -> Result<usize, LinkError> {
        if self.ended {
            return Err(LinkError::Ended);
        }
        let mut rewound = false;
        loop {
            let read = self.file.read(&mut self.scratch.0).await?;
            if read == 0 {
                if self.repeat && !rewound {
                    self.file.seek(SeekFrom::Start(0)).await?;
                    self.restart = true;
                    rewound = true;
                    continue;
                }
                self.ended = true;
                return Ok(0);
            }
            buf.extend_from_slice(&self.scratch.0[..read]);
            self.bytes_read = self.bytes_read.saturating_add(read as u64);
            if let Some(limiter) = self.limiter.as_mut() {
                limiter.consume(read).await;
            }
            return Ok(read);
        }
    }
}

/// Paces a replay to a byte rate.
///
/// Budgeted against the start of the replay rather than against the previous chunk: pacing
/// chunk-to-chunk accumulates every scheduler delay, so a replay asked for 1 Mbit/s drifts
/// slower and slower for as long as it runs, and the timestamps an operator reads off the
/// plot stop matching the pass.
#[derive(Debug)]
pub struct RateLimiter {
    bytes_per_second: u64,
    started: tokio::time::Instant,
    issued: u64,
}

impl RateLimiter {
    /// A limiter allowing `bytes_per_second`, starting now.
    #[must_use]
    pub fn new(bytes_per_second: u64) -> Self {
        Self {
            bytes_per_second,
            started: tokio::time::Instant::now(),
            issued: 0,
        }
    }

    /// The rate this was built for.
    #[must_use]
    pub const fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second
    }

    /// Bytes charged so far.
    #[must_use]
    pub const fn issued(&self) -> u64 {
        self.issued
    }

    /// How long the limiter has been running.
    #[must_use]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// When the bytes charged so far are due, measured from the start of the replay.
    ///
    /// Split into whole seconds and a remainder rather than computed in nanoseconds:
    /// `issued * 1_000_000_000` overflows a `u64` at about 18 GB, which a recording of a real
    /// pass reaches, and the overflow is a panic in a debug build.
    #[must_use]
    fn due(&self) -> Duration {
        if self.bytes_per_second == 0 {
            return Duration::ZERO;
        }
        let seconds = self.issued / self.bytes_per_second;
        let remainder = u128::from(self.issued % self.bytes_per_second);
        let nanos = (remainder * 1_000_000_000) / u128::from(self.bytes_per_second);
        Duration::from_secs(seconds) + Duration::from_nanos(nanos as u64)
    }

    /// Charges `bytes` against the budget and waits until they are due.
    ///
    /// Returns immediately when the replay is running behind the rate, which is the normal
    /// case on the first chunk and after any stall. That falls out of sleeping until a
    /// deadline instead of for a duration: a deadline already past is not a wait, and no
    /// arithmetic is needed to notice it. A rate of zero charges and never sleeps.
    pub async fn consume(&mut self, bytes: usize) {
        self.issued = self.issued.saturating_add(bytes as u64);
        if self.bytes_per_second == 0 {
            return;
        }
        // A deadline that does not fit an `Instant` is some 292 years of replay away; there
        // is nothing to wait for that cannot also be not waited for.
        if let Some(deadline) = self.started.checked_add(self.due()) {
            tokio::time::sleep_until(deadline).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant as StdInstant;

    use tokio::io::AsyncWriteExt;

    /// A path in the system temporary directory, unique per test and per process.
    ///
    /// Cargo runs the tests of one binary on several threads at once, so a shared name is two
    /// tests writing one file. There is no `tempfile` in this workspace and there is not
    /// going to be one for four tests.
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xtce-gs-source-{}-{name}.bin", std::process::id()))
    }

    async fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let path = temp_path(name);
        tokio::fs::write(&path, bytes).await.unwrap();
        path
    }

    fn config_message(spec: &str) -> String {
        match spec.parse::<SourceSpec>() {
            Err(LinkError::Config(message)) => message,
            other => panic!("expected a Config error for {spec:?}, got {other:?}"),
        }
    }

    #[test]
    fn every_url_form_parses() {
        assert_eq!(
            "udp://0.0.0.0:10015".parse::<SourceSpec>().unwrap(),
            SourceSpec::Udp(SocketAddr::from(([0, 0, 0, 0], 10015)))
        );
        assert_eq!(
            "tcp://127.0.0.1:10015".parse::<SourceSpec>().unwrap(),
            SourceSpec::TcpConnect(SocketAddr::from(([127, 0, 0, 1], 10015)))
        );
        assert_eq!(
            "tcp-listen://0.0.0.0:10015".parse::<SourceSpec>().unwrap(),
            SourceSpec::TcpListen(SocketAddr::from(([0, 0, 0, 0], 10015)))
        );
        assert_eq!(
            "file:///var/tmp/pass.dat?rate=1000000&chunk=8192&repeat"
                .parse::<SourceSpec>()
                .unwrap(),
            SourceSpec::File {
                path: PathBuf::from("/var/tmp/pass.dat"),
                bytes_per_second: Some(1_000_000),
                chunk: 8192,
                repeat: true,
            }
        );
    }

    #[test]
    fn a_bare_path_is_a_file_with_the_defaults() {
        assert_eq!(
            "/var/tmp/pass.dat".parse::<SourceSpec>().unwrap(),
            SourceSpec::File {
                path: PathBuf::from("/var/tmp/pass.dat"),
                bytes_per_second: None,
                chunk: DEFAULT_CHUNK,
                repeat: false,
            }
        );
    }

    #[test]
    fn a_bare_path_keeps_its_question_mark() {
        // A `?` is legal in a file name, so a bare path is not split on one. Only `file://`
        // has a query.
        assert_eq!(
            "/var/tmp/odd?name.dat".parse::<SourceSpec>().unwrap(),
            SourceSpec::File {
                path: PathBuf::from("/var/tmp/odd?name.dat"),
                bytes_per_second: None,
                chunk: DEFAULT_CHUNK,
                repeat: false,
            }
        );
    }

    #[test]
    fn an_ipv6_address_parses() {
        assert_eq!(
            "udp://[::1]:10015".parse::<SourceSpec>().unwrap(),
            SourceSpec::Udp("[::1]:10015".parse().unwrap())
        );
    }

    #[test]
    fn a_port_that_is_not_a_number_is_refused() {
        let message = config_message("udp://127.0.0.1:telemetry");
        assert!(message.contains("udp://"), "{message}");
        assert!(message.contains("127.0.0.1:telemetry"), "{message}");
    }

    #[test]
    fn an_address_without_a_port_is_refused() {
        let message = config_message("tcp://127.0.0.1");
        assert!(message.contains("port"), "{message}");
    }

    #[test]
    fn a_rate_that_is_not_a_number_is_refused() {
        let message = config_message("file:///var/tmp/pass.dat?rate=fast");
        assert!(message.contains("rate"), "{message}");
        assert!(message.contains("fast"), "{message}");
    }

    #[test]
    fn a_chunk_that_is_not_a_number_is_refused() {
        let message = config_message("file:///var/tmp/pass.dat?chunk=-1");
        assert!(message.contains("chunk"), "{message}");
    }

    #[test]
    fn an_unknown_scheme_names_the_ones_it_knows() {
        let message = config_message("serial:///dev/ttyUSB0");
        assert!(message.contains("serial://"), "{message}");
        assert!(message.contains("tcp-listen://"), "{message}");
    }

    #[test]
    fn an_unknown_query_option_is_refused() {
        let message = config_message("file:///var/tmp/pass.dat?loop");
        assert!(message.contains("loop"), "{message}");
        assert!(message.contains("repeat"), "{message}");
    }

    #[test]
    fn repeat_is_a_flag_and_says_so() {
        let message = config_message("file:///var/tmp/pass.dat?repeat=true");
        assert!(message.contains("flag"), "{message}");
    }

    #[test]
    fn an_empty_source_is_refused() {
        let message = config_message("   ");
        assert!(message.contains("empty source"), "{message}");
    }

    #[test]
    fn a_scheme_with_nothing_after_it_is_refused() {
        assert!(config_message("udp://").contains("udp://"));
        assert!(config_message("file://").contains("path"));
    }

    #[test]
    fn a_rate_of_zero_is_no_rate_limit() {
        // Zero means "as fast as the disk allows" everywhere else in the CLI; a replay that
        // is allowed nothing would simply never advance.
        let SourceSpec::File {
            bytes_per_second, ..
        } = "file:///var/tmp/pass.dat?rate=0".parse().unwrap()
        else {
            panic!("expected a file source");
        };
        assert_eq!(bytes_per_second, None);
    }

    #[test]
    fn a_chunk_of_zero_becomes_the_default() {
        let SourceSpec::File { chunk, .. } = "file:///var/tmp/pass.dat?chunk=0".parse().unwrap()
        else {
            panic!("expected a file source");
        };
        assert_eq!(chunk, DEFAULT_CHUNK);
    }

    #[test]
    fn display_round_trips_by_value() {
        // By value, not by string: a bare path comes back as a `file://` URL carrying the
        // defaults it was given, which is the same source and a different spelling.
        for spec in [
            "udp://0.0.0.0:10015",
            "tcp://127.0.0.1:10015",
            "tcp-listen://0.0.0.0:10015",
            "file:///var/tmp/pass.dat?rate=1000000&chunk=8192&repeat",
            "file:///var/tmp/pass.dat",
            "/var/tmp/pass.dat",
        ] {
            let parsed: SourceSpec = spec.parse().unwrap();
            let again: SourceSpec = parsed.to_string().parse().unwrap();
            assert_eq!(parsed, again, "{spec} rendered as {parsed}");
        }
    }

    #[tokio::test]
    async fn a_udp_datagram_arrives_whole_and_is_appended() {
        let mut source = UdpSource::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = source.socket().local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"one datagram", addr).await.unwrap();

        let mut buf = vec![0xAA];
        let read = tokio::time::timeout(Duration::from_secs(5), source.read_chunk(&mut buf))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(read, 12);
        // The caller's byte is still there: `read_chunk` appends, it does not fill.
        assert_eq!(buf, b"\xAAone datagram");
    }

    #[tokio::test]
    async fn a_zero_length_datagram_is_not_the_end_of_the_stream() {
        let mut source = UdpSource::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = source.socket().local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"", addr).await.unwrap();
        sender.send_to(b"after the keepalive", addr).await.unwrap();

        let mut buf = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), source.read_chunk(&mut buf))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(read, 19);
        assert_eq!(buf, b"after the keepalive");
    }

    #[tokio::test]
    async fn a_tcp_connection_is_read_to_the_end_and_then_says_so() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let feeder = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            peer.write_all(b"telemetry").await.unwrap();
            peer.shutdown().await.unwrap();
        });

        let mut source = TcpSource::connect(addr).await.unwrap();
        let mut buf = Vec::new();
        while buf.len() < 9 {
            assert!(source.read_chunk(&mut buf).await.unwrap() > 0);
        }
        assert_eq!(buf, b"telemetry");

        assert_eq!(source.read_chunk(&mut buf).await.unwrap(), 0);
        // The second read past the end is an error, not another zero to spin on.
        assert!(matches!(
            source.read_chunk(&mut buf).await,
            Err(LinkError::Ended)
        ));
        assert_eq!(buf, b"telemetry", "an ended read must not touch the buffer");
        feeder.await.unwrap();
    }

    #[tokio::test]
    async fn a_listening_source_accepts_a_peer_and_reports_the_restart() {
        let mut source = TcpListenSource::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = source.listener().local_addr().unwrap();
        assert!(!source.is_connected());

        let feeder = tokio::spawn(async move {
            let mut peer = TcpStream::connect(addr).await.unwrap();
            peer.write_all(b"abc").await.unwrap();
            peer.shutdown().await.unwrap();
        });

        let mut buf = Vec::new();
        while buf.len() < 3 {
            assert!(source.read_chunk(&mut buf).await.unwrap() > 0);
        }
        assert_eq!(buf, b"abc");
        assert!(source.is_connected());
        // Drained by the call: the pipeline is flushed once per new peer, not once per read.
        assert!(source.take_restart());
        assert!(!source.take_restart());
        feeder.await.unwrap();
    }

    #[tokio::test]
    async fn a_closed_peer_leaves_the_listener_waiting_rather_than_ending_it() {
        let mut source = TcpListenSource::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = source.listener().local_addr().unwrap();
        let first = tokio::spawn(async move {
            let mut peer = TcpStream::connect(addr).await.unwrap();
            peer.write_all(b"x").await.unwrap();
            peer.shutdown().await.unwrap();
        });

        let mut buf = Vec::new();
        assert_eq!(source.read_chunk(&mut buf).await.unwrap(), 1);
        first.await.unwrap();

        // The peer has gone. A station left listening overnight is still listening in the
        // morning, so this waits for the next one instead of returning `Ok(0)`.
        let waited =
            tokio::time::timeout(Duration::from_millis(100), source.read_chunk(&mut buf)).await;
        assert!(
            waited.is_err(),
            "a disconnect ended the session: {waited:?}"
        );
        assert_eq!(buf, b"x");
    }

    #[tokio::test]
    async fn a_replay_yields_exactly_the_bytes_of_the_file() {
        let bytes: Vec<u8> = (0..2500u32).map(|i| (i % 251) as u8).collect();
        let path = write_temp("exact", &bytes).await;

        let mut replay = FileReplay::open(&path, 1024, None, false).await.unwrap();
        let mut buf = Vec::new();
        let mut reads = 0;
        loop {
            let read = replay.read_chunk(&mut buf).await.unwrap();
            if read == 0 {
                break;
            }
            assert!(read <= 1024, "a read of {read} exceeded the chunk");
            reads += 1;
        }

        assert_eq!(buf, bytes);
        assert_eq!(replay.bytes_read(), 2500);
        assert!(
            reads >= 3,
            "2500 bytes in chunks of 1024 is at least 3 reads"
        );
        assert!(replay.has_ended());
        assert!(matches!(
            replay.read_chunk(&mut buf).await,
            Err(LinkError::Ended)
        ));
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn an_empty_replay_ends_instead_of_reading_nothing() {
        let path = write_temp("empty", b"").await;
        let mut replay = FileReplay::open(&path, 16, None, false).await.unwrap();
        let mut buf = Vec::new();
        assert_eq!(replay.read_chunk(&mut buf).await.unwrap(), 0);
        assert!(buf.is_empty());
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn an_empty_replay_that_repeats_still_ends() {
        // Read zero, rewind, read zero: without the one-rewind rule this is a loop with no
        // await in it that never yields and never produces a byte.
        let path = write_temp("empty-repeat", b"").await;
        let mut replay = FileReplay::open(&path, 16, None, true).await.unwrap();
        let mut buf = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), replay.read_chunk(&mut buf))
            .await
            .expect("a repeating replay of an empty file span forever")
            .unwrap();
        assert_eq!(read, 0);
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn a_wrap_resumes_at_the_first_byte_when_the_chunk_does_not_divide_the_file() {
        // 25 bytes in chunks of 10 wraps mid-chunk: an end that dropped the short final read,
        // or a rewind that seeked anywhere but zero, shows up here and nowhere else.
        let bytes: Vec<u8> = (0..25u8).collect();
        let path = write_temp("wrap-uneven", &bytes).await;
        let mut replay = FileReplay::open(&path, 10, None, true).await.unwrap();

        let mut buf = Vec::new();
        let mut restarts = 0;
        while buf.len() < 40 {
            assert!(replay.read_chunk(&mut buf).await.unwrap() > 0);
            if replay.take_restart() {
                restarts += 1;
            }
        }

        let mut expected: Vec<u8> = bytes.clone();
        expected.extend_from_slice(&bytes);
        expected.truncate(buf.len());
        assert_eq!(buf, expected);
        assert_eq!(restarts, 1, "one wrap, reported once");
        assert_eq!(replay.bytes_read(), buf.len() as u64);
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn a_repeating_replay_wraps_and_reports_the_restart() {
        let path = write_temp("repeat", b"0123456789").await;
        let mut replay = FileReplay::open(&path, 10, None, true).await.unwrap();
        let mut buf = Vec::new();

        assert_eq!(replay.read_chunk(&mut buf).await.unwrap(), 10);
        assert!(!replay.take_restart(), "the first pass is not a restart");
        assert_eq!(replay.read_chunk(&mut buf).await.unwrap(), 10);
        assert!(replay.take_restart(), "the wrap must be visible to framing");
        assert!(!replay.take_restart());

        assert_eq!(buf, b"01234567890123456789");
        assert!(!replay.has_ended());
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn a_rate_limited_replay_takes_at_least_the_time_it_should() {
        // 1000 bytes at 10 000 bytes per second is 100 ms of pass, whatever the disk does.
        let path = write_temp("rate", &vec![0x5A; 1000]).await;
        let started = StdInstant::now();
        let mut replay = FileReplay::open(&path, 100, Some(10_000), false)
            .await
            .unwrap();
        let mut buf = Vec::new();
        while replay.read_chunk(&mut buf).await.unwrap() > 0 {}
        let elapsed = started.elapsed();

        assert_eq!(buf.len(), 1000);
        // Exactly the file, not a chunk more: the limiter is charged after the read and for
        // what the read produced, so the final `Ok(0)` costs nothing. Charging first would
        // bill 100 bytes that do not exist and this number would be 1100.
        assert_eq!(replay.limiter().map(RateLimiter::issued), Some(1000));
        // The lower bound only: an upper bound is a bet on CI's scheduler. 95 ms rather than
        // 100 because tokio's timer wheel has millisecond granularity and may fire a tick
        // early; the drift this guards against is measured in whole chunks.
        assert!(
            elapsed >= Duration::from_millis(95),
            "1000 bytes at 10 kB/s took {elapsed:?}"
        );
        tokio::fs::remove_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn the_rate_limiter_does_not_drift_over_many_chunks() {
        // Pacing chunk-to-chunk would make this finish late by the sum of 50 scheduler
        // delays. Budgeting from the start means the deadline for the last chunk is where it
        // always was, so the whole run is bounded above by its own arithmetic.
        let mut limiter = RateLimiter::new(100_000);
        let started = StdInstant::now();
        for _ in 0..50 {
            limiter.consume(200).await;
        }
        let elapsed = started.elapsed();
        assert_eq!(limiter.issued(), 10_000);
        assert!(elapsed >= Duration::from_millis(95), "{elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(500),
            "50 chunks totalling 100 ms of budget took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_rate_of_zero_never_waits() {
        let mut limiter = RateLimiter::new(0);
        let started = StdInstant::now();
        limiter.consume(1_000_000).await;
        assert_eq!(limiter.issued(), 1_000_000);
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn a_huge_charge_does_not_overflow_the_deadline() {
        // `issued * 1_000_000_000` overflows a u64 above about 18 GB. A recording of a real
        // pass is bigger than that, and the overflow would be a panic in a debug build.
        let mut limiter = RateLimiter::new(u64::MAX);
        limiter.consume(usize::MAX).await;
        assert_eq!(limiter.issued(), u64::MAX);
        assert!(limiter.due() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_missing_file_names_itself() {
        let path = temp_path("absent");
        let _ = tokio::fs::remove_file(&path).await;
        let spec = SourceSpec::File {
            path: path.clone(),
            bytes_per_second: None,
            chunk: 0,
            repeat: false,
        };
        match Source::connect(&spec).await {
            Err(LinkError::Io(error)) => {
                assert!(
                    error.to_string().contains(&path.display().to_string()),
                    "{error}"
                );
            }
            other => panic!("expected an Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn describe_names_the_bound_address_not_the_asked_for_one() {
        let spec = SourceSpec::Udp("127.0.0.1:0".parse().unwrap());
        let mut source = Source::connect(&spec).await.unwrap();
        let described = source.describe();
        assert!(described.starts_with("udp://127.0.0.1:"), "{described}");
        assert!(!described.ends_with(":0"), "{described}");
        assert!(source.is_datagram());
        assert!(!source.take_restart(), "a datagram socket never restarts");
    }
}
