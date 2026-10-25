//! Event-driven peer server.
//!
//! One thread multiplexes every connection with the operating system's
//! readiness API (epoll, kqueue, or IOCP through `mio`), so thousands of
//! peers do not need thousands of threads. After the handshakes, each
//! connection runs the shared peer protocol ([`crate::engine::wire`]).
//! Disk work it asks for (block reads, piece checks, hash requests) goes to
//! a small pool of worker threads; results come back through a channel and
//! wake the event loop. A timer ticks every connection for the parts of
//! the protocol driven by time.
//!
//! Connections arrive three ways: accepted on the TCP listeners (one per
//! address family), accepted
//! on the uTP socket, or dialed by a download and handed over ([`Control`]).
//!
//! uTP connections arrive through the uTP socket instead of `mio`. Their
//! readiness callbacks queue the connection id and wake the loop, which
//! then handles them like TCP readiness events.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};

use crate::engine::active::{ActiveTorrent, Checked, Download};
use crate::engine::wire::{Action, Flow, Link, PeerWire};
use crate::mse::{Ciphers, Opening, Responder};
use crate::peer_manager::Handshake;
use crate::{BackendError, Encryption, PeerMessage};

const WAKER: Token = Token(usize::MAX);
/// Queued when uTP connections are waiting to be accepted.
const UTP_ACCEPT: usize = usize::MAX - 1;
/// Listeners take the tokens below; one per address family in practice.
const FIRST_LISTENER: usize = usize::MAX - 2;
const MAX_LISTENERS: usize = 8;
/// Connection ids stay below the reserved ones above.
const MAX_ID: usize = FIRST_LISTENER - MAX_LISTENERS;
/// Threads answering disk reads.
const DISK_THREADS: usize = 4;
const HANDSHAKE_LENGTH: usize = 68;
/// Largest message accepted, as for outbound connections.
const MAX_FRAME: usize = 1 << 20;
/// Requests a peer may have waiting for disk; matches the `reqq` we
/// advertise. Beyond this, reading from that peer pauses.
const MAX_PENDING: usize = 250;
/// Reading from a peer pauses while this much is queued for it, and
/// resumes below the low mark, so slow peers cannot balloon memory.
const OUTPUT_HIGH: usize = 4 << 20;
const OUTPUT_LOW: usize = 1 << 20;
/// Peers send keep-alives about every two minutes.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
const TICK: Duration = Duration::from_millis(200);
/// How soon connections held back by a rate limit try again.
const THROTTLE_RETRY: Duration = Duration::from_millis(10);

enum Phase {
    /// Waiting for the BitTorrent handshake. After an encrypted (MSE)
    /// handshake, `negotiated` holds the info hash it asked for. Before
    /// one, the peer has yet to show whether it wants encryption.
    Handshake {
        deadline: Instant,
        negotiated: Option<[u8; 20]>,
    },
    /// In an encrypted (MSE) handshake.
    Negotiating {
        deadline: Instant,
        responder: Box<Responder>,
    },
    /// Running the peer protocol for a torrent.
    Active {
        torrent: Arc<ActiveTorrent>,
        wire: Box<PeerWire>,
    },
}

/// A peer's byte stream.
enum Stream {
    Tcp(TcpStream),
    Utp(crate::UtpStream),
}

impl Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buffer),
            Self::Utp(stream) => stream.read(buffer),
        }
    }

    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(data),
            Self::Utp(stream) => stream.write(data),
        }
    }
}

/// Connection ids with uTP readiness to handle.
type Ready = Arc<Mutex<Vec<usize>>>;

struct Connection {
    stream: Stream,
    address: std::net::SocketAddr,
    phase: Phase,
    input: Vec<u8>,
    output: Vec<u8>,
    /// Bytes of `output` already written.
    written: usize,
    /// Disk requests in flight for this connection.
    pending: usize,
    last_read: Instant,
    registered: Option<Interest>,
    /// Reading is paused while too much is waiting on disk or the network.
    paused: bool,
    /// Present when the stream is RC4-encrypted (MSE).
    ciphers: Option<Ciphers>,
    /// Counts this connection against the shared connection limit.
    _slot: crate::limits::Slot,
}

impl Connection {
    fn queued(&self) -> usize {
        self.output.len() - self.written
    }

    /// Pauses at the high marks and resumes only below the low ones, so a
    /// busy connection does not flap.
    fn update_pause(&mut self) {
        if self.pending >= MAX_PENDING || self.queued() >= OUTPUT_HIGH {
            self.paused = true;
        } else if self.paused && self.queued() < OUTPUT_LOW {
            self.paused = false;
        }
    }

    fn queue(&mut self, message: &PeerMessage) -> Result<(), BackendError> {
        self.send(&message.encode()?);
        Ok(())
    }

    /// Queues bytes for the peer, encrypting them on encrypted streams.
    fn send(&mut self, bytes: &[u8]) {
        let start = self.output.len();
        self.output.extend_from_slice(bytes);
        if let Some(ciphers) = &mut self.ciphers {
            ciphers.encrypt.apply(&mut self.output[start..]);
        }
    }
}

/// Disk work a connection's protocol asked for.
enum Work {
    Read {
        piece_index: u32,
        begin: u32,
        length: u32,
    },
    Check {
        download: Download,
        data: Vec<u8>,
    },
    Hashes(PeerMessage),
}

struct Job {
    connection: usize,
    torrent: Arc<ActiveTorrent>,
    work: Work,
}

/// Finished disk work, for the connection's protocol.
enum Done {
    Block {
        piece_index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Checked {
        index: usize,
        checked: Checked,
    },
    Message(PeerMessage),
}

type Reply = (usize, Result<Done, BackendError>);

impl Work {
    fn perform(self, torrent: &ActiveTorrent) -> Result<Done, BackendError> {
        match self {
            Self::Read {
                piece_index,
                begin,
                length,
            } => Ok(Done::Block {
                piece_index,
                begin,
                block: torrent.read_block(piece_index as usize, begin, length)?,
            }),
            Self::Check { mut download, data } => {
                let checked = torrent.check_piece(&mut download, &data)?;
                // A corrupt piece's claim is released here.
                Ok(Done::Checked {
                    index: download.index,
                    checked,
                })
            }
            Self::Hashes(request) => Ok(Done::Message(torrent.answer_hashes(request)?)),
        }
    }
}

/// How the server treats peers.
pub(crate) struct Settings<'a> {
    pub max_connections: usize,
    pub handshake_timeout: Duration,
    pub encryption: Encryption,
    /// Also accept peers over uTP on this socket.
    pub utp: Option<&'a crate::UtpSocket>,
    /// Connection and rate limits shared with other runtimes.
    pub limits: Arc<crate::Limits>,
}

/// A connection a download dialed, ready for the event loop, with its
/// protocol state already created (so it already counts as open).
pub(crate) struct Adopted {
    pub connection: crate::PeerConnection,
    pub address: std::net::SocketAddr,
    pub torrent: Arc<ActiveTorrent>,
    pub wire: Box<PeerWire>,
}

/// How other threads hand connections to a running event loop.
pub(crate) struct Control {
    adopted: mpsc::Sender<Adopted>,
    waker: Arc<Waker>,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Control")
    }
}

impl Control {
    pub(crate) fn adopt(&self, adopted: Adopted) -> Result<(), BackendError> {
        self.adopted
            .send(adopted)
            .map_err(|_| BackendError::Io("the runtime stopped".to_owned()))?;
        self.waker
            .wake()
            .map_err(|error| BackendError::Io(error.to_string()))
    }
}

/// Serves peers until `shutdown` is set: those connecting on `listeners`, and those handed over through the [`Control`] placed in `control`
/// while running. `connections` is kept equal to the number of open peer
/// connections.
pub(crate) fn serve(
    routes: &crate::runtime::Routes,
    listeners: Vec<std::net::TcpListener>,
    settings: Settings<'_>,
    shutdown: &AtomicBool,
    connections: &AtomicUsize,
    control: &Mutex<Option<Control>>,
) -> Result<(), BackendError> {
    let Settings {
        max_connections,
        handshake_timeout,
        encryption,
        utp,
        limits,
    } = settings;
    let io = |error: std::io::Error| BackendError::Io(error.to_string());
    let poll = Poll::new().map_err(io)?;
    let mut registered = Vec::new();
    for (index, listener) in listeners.into_iter().take(MAX_LISTENERS).enumerate() {
        listener.set_nonblocking(true).map_err(io)?;
        let mut listener = TcpListener::from_std(listener);
        poll.registry()
            .register(
                &mut listener,
                Token(FIRST_LISTENER - index),
                Interest::READABLE,
            )
            .map_err(io)?;
        registered.push(listener);
    }
    let listeners = registered;
    let waker = Arc::new(Waker::new(poll.registry(), WAKER).map_err(io)?);
    let (adopt_sender, adopted) = mpsc::channel::<Adopted>();
    *lock(control) = Some(Control {
        adopted: adopt_sender,
        waker: Arc::clone(&waker),
    });
    let ready: Ready = Arc::default();
    let utp_listener = utp.map(|socket| {
        let (ready, waker) = (Arc::clone(&ready), Arc::clone(&waker));
        socket.listen(Arc::new(move || {
            lock(&ready).push(UTP_ACCEPT);
            let _ = waker.wake();
        }))
    });

    let (jobs, queue) = mpsc::channel::<Job>();
    let (reply_sender, replies) = mpsc::channel::<Reply>();
    let queue = Arc::new(Mutex::new(queue));
    let workers: Vec<_> = (0..DISK_THREADS)
        .map(|_| {
            let (queue, replies, waker) =
                (Arc::clone(&queue), reply_sender.clone(), Arc::clone(&waker));
            std::thread::spawn(move || {
                loop {
                    let job = match queue.lock() {
                        Ok(queue) => queue.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    let result = job.work.perform(&job.torrent);
                    if replies.send((job.connection, result)).is_err() {
                        return;
                    }
                    let _ = waker.wake();
                }
            })
        })
        .collect();
    drop(reply_sender);

    let mut server = Server {
        routes,
        poll,
        connections: HashMap::new(),
        next_id: 0,
        max_connections,
        handshake_timeout,
        encryption,
        jobs,
        ready: Arc::clone(&ready),
        waker,
        limits,
        throttled: Vec::new(),
    };
    let mut events = Events::with_capacity(1024);
    let mut last_sweep = Instant::now();
    let result = loop {
        if shutdown.load(Ordering::Acquire) {
            break Ok(());
        }
        let timeout = if server.throttled.is_empty() {
            TICK
        } else {
            THROTTLE_RETRY
        };
        if let Err(error) = server.poll.poll(&mut events, Some(timeout)) {
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            break Err(io(error));
        }
        for event in &events {
            match event.token() {
                WAKER => {}
                Token(token) if token <= FIRST_LISTENER && token > MAX_ID => {
                    if let Some(listener) = listeners.get(FIRST_LISTENER - token) {
                        server.accept(listener);
                    }
                }
                Token(id) => {
                    if event.is_readable() || event.is_read_closed() || event.is_error() {
                        server.on_readable(id);
                    }
                    if event.is_writable() {
                        server.flush(id);
                    }
                }
            }
        }
        let mut woken = std::mem::take(&mut *lock(&ready));
        woken.sort_unstable();
        woken.dedup();
        for id in woken {
            match (id, &utp_listener) {
                (UTP_ACCEPT, Some(listener)) => server.accept_utp(listener),
                (UTP_ACCEPT, None) => {}
                _ => {
                    server.on_readable(id);
                    server.flush(id);
                }
            }
        }
        while let Ok((id, result)) = replies.try_recv() {
            server.on_reply(id, result);
        }
        while let Ok(connection) = adopted.try_recv() {
            server.adopt(connection);
        }
        server.retry_throttled();
        if last_sweep.elapsed() >= TICK {
            last_sweep = Instant::now();
            server.close_stale();
            server.tick();
        }
        connections.store(server.connections.len(), Ordering::Release);
    };
    *lock(control) = None;
    connections.store(0, Ordering::Release);
    drop(server);
    for worker in workers {
        let _ = worker.join();
    }
    result
}

struct Server<'a> {
    routes: &'a crate::runtime::Routes,
    poll: Poll,
    connections: HashMap<usize, Connection>,
    next_id: usize,
    max_connections: usize,
    handshake_timeout: Duration,
    encryption: Encryption,
    jobs: mpsc::Sender<Job>,
    ready: Ready,
    waker: Arc<Waker>,
    limits: Arc<crate::Limits>,
    /// Connections that found a rate limit's bucket empty, to retry soon.
    throttled: Vec<usize>,
}

impl Server<'_> {
    /// Whether another connection may open, by this runtime's cap and the
    /// limit shared by all runtimes.
    fn has_room(&self) -> bool {
        self.connections.len() < self.max_connections && self.limits.has_room(0)
    }

    /// Lets connections held back by a rate limit read and write again.
    /// Each retry starts with a different connection, so no peer is always
    /// first to the tokens.
    fn retry_throttled(&mut self) {
        let mut throttled = std::mem::take(&mut self.throttled);
        throttled.sort_unstable();
        throttled.dedup();
        if throttled.is_empty() {
            return;
        }
        let start = self.next_id % throttled.len();
        throttled.rotate_left(start);
        for id in throttled {
            self.on_readable(id);
            self.flush(id);
        }
    }

    fn accept(&mut self, listener: &TcpListener) {
        loop {
            let (mut stream, address) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(_) => return,
            };
            let address = crate::net::canonical(address);
            if !self.has_room() {
                drop(stream);
                continue;
            }
            let id = self.next_id();
            if self
                .poll
                .registry()
                .register(&mut stream, Token(id), Interest::READABLE)
                .is_err()
            {
                continue;
            }
            let _ = stream.set_nodelay(true);
            self.insert(id, Stream::Tcp(stream), address);
        }
    }

    /// Takes in waiting uTP connections. Their readiness callbacks queue
    /// them for the event loop.
    fn accept_utp(&mut self, listener: &crate::utp::UtpListener) {
        while let Some(mut stream) = listener.accept() {
            if !self.has_room() {
                continue; // dropping it closes the connection
            }
            let id = self.next_id();
            stream.set_nonblocking(true);
            let (ready, waker) = (Arc::clone(&self.ready), Arc::clone(&self.waker));
            stream.set_notify(Arc::new(move || {
                lock(&ready).push(id);
                let _ = waker.wake();
            }));
            let address = stream.peer_addr();
            self.insert(id, Stream::Utp(stream), address);
            // Data may have arrived before the callback was set.
            self.on_readable(id);
            self.flush(id);
        }
    }

    /// Takes over a connection a download dialed: it runs the peer
    /// protocol from the start, as the handshakes are done.
    fn adopt(&mut self, adopted: Adopted) {
        let Adopted {
            connection,
            address,
            torrent,
            mut wire,
        } = adopted;
        let (transport, ciphers, buffered) = connection.into_parts();
        let id = self.next_id();
        let stream = match transport {
            crate::peer_manager::Transport::Tcp(stream) => {
                if stream.set_nonblocking(true).is_err() {
                    return;
                }
                let mut stream = TcpStream::from_std(stream);
                if self
                    .poll
                    .registry()
                    .register(&mut stream, Token(id), Interest::READABLE)
                    .is_err()
                {
                    return;
                }
                Stream::Tcp(stream)
            }
            crate::peer_manager::Transport::Utp(mut stream) => {
                stream.set_nonblocking(true);
                let (ready, waker) = (Arc::clone(&self.ready), Arc::clone(&self.waker));
                stream.set_notify(Arc::new(move || {
                    lock(&ready).push(id);
                    let _ = waker.wake();
                }));
                Stream::Utp(stream)
            }
        };
        let mut out = Vec::new();
        if wire.start(&mut out).is_err() {
            return;
        }
        self.insert(id, stream, address);
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        connection.input = buffered;
        connection.ciphers = ciphers;
        connection.phase = Phase::Active { torrent, wire };
        if dispatch(id, connection, &self.jobs, out).is_err() {
            return self.close(id);
        }
        // The peer may have sent messages along with its handshake.
        if self.process(id).is_err() {
            return self.close(id);
        }
        self.on_readable(id);
    }

    /// Ids are never reused, so late disk replies for a closed connection
    /// cannot reach a new one.
    fn next_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id = (self.next_id + 1) % MAX_ID;
        id
    }

    fn insert(&mut self, id: usize, stream: Stream, address: std::net::SocketAddr) {
        self.connections.insert(
            id,
            Connection {
                stream,
                address,
                phase: Phase::Handshake {
                    deadline: Instant::now() + self.handshake_timeout,
                    negotiated: None,
                },
                input: Vec::new(),
                output: Vec::new(),
                written: 0,
                pending: 0,
                last_read: Instant::now(),
                registered: Some(Interest::READABLE),
                paused: false,
                ciphers: None,
                _slot: self.limits.open_slot(),
            },
        );
    }

    /// Reads until the socket is drained (readiness is edge-triggered) or
    /// the connection pauses, handling messages as they arrive.
    fn on_readable(&mut self, id: usize) {
        let mut buffer = [0; 64 * 1024];
        loop {
            let Some(connection) = self.connections.get_mut(&id) else {
                return;
            };
            if connection.paused {
                break;
            }
            let allowed = self.limits.take_download(buffer.len());
            if allowed == 0 {
                self.throttled.push(id);
                break;
            }
            let result = connection.stream.read(&mut buffer[..allowed]);
            let used = *result.as_ref().unwrap_or(&0);
            self.limits.refund_download(allowed - used);
            match result {
                Ok(0) => return self.close(id),
                Ok(read) => {
                    if let Some(ciphers) = &mut connection.ciphers {
                        ciphers.decrypt.apply(&mut buffer[..read]);
                    }
                    connection.input.extend_from_slice(&buffer[..read]);
                    connection.last_read = Instant::now();
                    if let Err(error) = self.process(id) {
                        return self.fail(id, error);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return self.close(id),
            }
        }
        self.flush(id);
    }

    /// Handles buffered input: the handshake, then whole messages, as far
    /// as the connection's limits allow. Returns whether any input was used.
    fn process(&mut self, id: usize) -> Result<bool, BackendError> {
        let routes = self.routes;
        let Some(connection) = self.connections.get_mut(&id) else {
            return Ok(false);
        };
        let mut progressed = false;
        if let Phase::Handshake {
            deadline,
            negotiated: None,
        } = connection.phase
        {
            match crate::mse::classify(&connection.input) {
                Opening::Undecided => return Ok(false),
                Opening::Plain if self.encryption == Encryption::Required => {
                    return Err(invalid(
                        "plain-text peers are refused: encryption is required",
                    ));
                }
                Opening::Plain => {}
                Opening::Encrypted if self.encryption == Encryption::Disabled => {
                    return Err(invalid(
                        "encrypted peers are refused: encryption is disabled",
                    ));
                }
                Opening::Encrypted => {
                    let allow_plaintext = self.encryption != Encryption::Required;
                    connection.phase = Phase::Negotiating {
                        deadline,
                        responder: Box::new(Responder::new(allow_plaintext)),
                    };
                }
            }
        }
        if let Phase::Negotiating {
            deadline,
            responder,
        } = &mut connection.phase
        {
            let established =
                responder.advance(&mut connection.input, &mut connection.output, |req2| {
                    routes.unobfuscate(req2)
                })?;
            let Some(established) = established else {
                return Ok(false);
            };
            connection.ciphers = established.ciphers;
            connection.phase = Phase::Handshake {
                deadline: *deadline,
                negotiated: Some(established.info_hash),
            };
            progressed = true;
        }
        if let Phase::Handshake { negotiated, .. } = connection.phase {
            if connection.input.len() < HANDSHAKE_LENGTH {
                return Ok(progressed);
            }
            progressed = true;
            let handshake = Handshake::decode(&connection.input[..HANDSHAKE_LENGTH])?;
            if negotiated.is_some_and(|hash| hash != *handshake.info_hash()) {
                return Err(invalid(
                    "handshake names a different torrent than its encryption",
                ));
            }
            connection.input.drain(..HANDSHAKE_LENGTH);
            let torrent = routes
                .route(&handshake)
                .ok_or_else(|| invalid("peer asked for a torrent we are not sharing"))?;
            let Some(key) = torrent.swarm.join(connection.address, Instant::now()) else {
                return Err(invalid("peer is banned for sending corrupt data"));
            };
            if !torrent
                .swarm
                .claim_peer_id(key, *handshake.peer_id(), torrent.peer_id, false)
            {
                torrent.swarm.leave(key);
                return Err(invalid("already connected to this peer"));
            }
            connection.send(&torrent.handshake_for(*handshake.info_hash()).encode());
            let link = Link {
                address: connection.address,
                key,
                outgoing: false,
                fast: handshake.supports_fast(),
                extensions: handshake.supports_extensions(),
                utp: matches!(connection.stream, Stream::Utp(_)),
                encrypted: connection.ciphers.is_some(),
            };
            // Dropping the wire releases its pieces and leaves the swarm.
            let mut wire = Box::new(PeerWire::new(Arc::clone(&torrent), link, Instant::now()));
            let mut out = Vec::new();
            wire.start(&mut out)?;
            connection.phase = Phase::Active { torrent, wire };
            dispatch(id, connection, &self.jobs, out)?;
        }
        let mut consumed = 0;
        let mut out = Vec::new();
        while !connection.paused {
            let input = &connection.input[consumed..];
            let Some(length) = input
                .get(..4)
                .map(|prefix| u32::from_be_bytes(prefix.try_into().unwrap()) as usize)
            else {
                break;
            };
            if length > MAX_FRAME {
                return Err(invalid("peer message exceeds maximum size"));
            }
            if input.len() < 4 + length {
                break;
            }
            let message = PeerMessage::decode(&input[..4 + length])?;
            consumed += 4 + length;
            let Phase::Active { wire, .. } = &mut connection.phase else {
                unreachable!("the handshakes were handled above");
            };
            if wire.on_message(message, Instant::now(), &mut out)? == Flow::Finished {
                return Err(invalid("connection finished"));
            }
            dispatch(id, connection, &self.jobs, std::mem::take(&mut out))?;
            connection.update_pause();
        }
        connection.input.drain(..consumed);
        // Requests and uploads follow the messages at once.
        if consumed > 0 {
            run_tick(id, connection, &self.jobs, routes)?;
        }
        Ok(progressed || consumed > 0)
    }

    fn on_reply(&mut self, id: usize, result: Result<Done, BackendError>) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        connection.pending = connection.pending.saturating_sub(1);
        let room = connection.queued() < OUTPUT_LOW;
        let handled = result.and_then(|done| {
            let Phase::Active { wire, .. } = &mut connection.phase else {
                return Ok(());
            };
            let mut out = Vec::new();
            match done {
                Done::Block {
                    piece_index,
                    begin,
                    block,
                } => wire.block_read(piece_index, begin, block, Instant::now(), &mut out),
                Done::Checked { index, checked } => wire.piece_checked(index, checked)?,
                Done::Message(message) => out.push(Action::Send(message)),
            }
            // More uploads while the peer keeps up with what we send.
            if room {
                wire.pump_uploads(&mut out);
            }
            dispatch(id, connection, &self.jobs, out)
        });
        if let Err(error) = handled {
            return self.fail(id, error);
        }
        connection.update_pause();
        self.flush(id);
    }

    /// Runs the timed part of the protocol on every active connection.
    fn tick(&mut self) {
        let ids: Vec<usize> = self
            .connections
            .iter()
            .filter(|(_, connection)| matches!(connection.phase, Phase::Active { .. }))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(connection) = self.connections.get_mut(&id) else {
                continue;
            };
            if let Err(error) = run_tick(id, connection, &self.jobs, self.routes) {
                self.fail(id, error);
            } else {
                self.flush(id);
            }
        }
    }

    /// Writes queued output and handles input held back while paused, until
    /// neither makes progress, then updates which readiness events the
    /// connection waits for.
    ///
    /// Held-back input must be handled here: once a peer has sent all its
    /// requests, no further read event will arrive to prompt it.
    fn flush(&mut self, id: usize) {
        loop {
            let Some(connection) = self.connections.get_mut(&id) else {
                return;
            };
            while connection.queued() > 0 {
                let allowed = self.limits.take_upload(connection.queued());
                if allowed == 0 {
                    self.throttled.push(id);
                    break;
                }
                let start = connection.written;
                let result = connection
                    .stream
                    .write(&connection.output[start..start + allowed]);
                let used = *result.as_ref().unwrap_or(&0);
                self.limits.refund_upload(allowed - used);
                match result {
                    Ok(0) => return self.close(id),
                    Ok(written) => connection.written += written,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(_) => return self.close(id),
                }
            }
            if connection.written == connection.output.len() {
                connection.output.clear();
                connection.written = 0;
            } else if connection.written > OUTPUT_LOW {
                connection.output.drain(..connection.written);
                connection.written = 0;
            }
            connection.update_pause();
            if connection.paused || connection.input.is_empty() {
                break;
            }
            match self.process(id) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return self.fail(id, error),
            }
        }
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        let wanted = match (connection.paused, connection.queued() > 0) {
            (false, false) => Some(Interest::READABLE),
            (false, true) => Some(Interest::READABLE | Interest::WRITABLE),
            (true, true) => Some(Interest::WRITABLE),
            // Waiting only on disk: no socket events needed until replies.
            (true, false) => None,
        };
        if wanted != connection.registered {
            let readable = |interest: Option<Interest>| interest.is_some_and(|i| i.is_readable());
            let result = match &mut connection.stream {
                // Re-registering re-arms readiness, so data that arrived
                // while paused is reported again.
                Stream::Tcp(stream) => {
                    let registry = self.poll.registry();
                    match (connection.registered, wanted) {
                        (Some(_), Some(interest)) => {
                            registry.reregister(stream, Token(id), interest)
                        }
                        (None, Some(interest)) => registry.register(stream, Token(id), interest),
                        (Some(_), None) => registry.deregister(stream),
                        (None, None) => Ok(()),
                    }
                }
                // uTP callbacks fire regardless; resuming reads must
                // re-check data that arrived while paused.
                Stream::Utp(_) => {
                    if readable(wanted) && !readable(connection.registered) {
                        lock(&self.ready).push(id);
                        let _ = self.waker.wake();
                    }
                    Ok(())
                }
            };
            if result.is_err() {
                return self.close(id);
            }
            connection.registered = wanted;
        }
    }

    /// Closes connections that never finished their handshake or went quiet.
    fn close_stale(&mut self) {
        let now = Instant::now();
        let stale: Vec<usize> = self
            .connections
            .iter()
            .filter(|(_, connection)| match connection.phase {
                Phase::Handshake { deadline, .. } | Phase::Negotiating { deadline, .. } => {
                    now >= deadline
                }
                Phase::Active { .. } => {
                    connection.pending == 0 && now - connection.last_read >= IDLE_TIMEOUT
                }
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.close(id);
        }
    }

    /// Closes a connection after an error, noting it on its torrent.
    fn fail(&mut self, id: usize, error: BackendError) {
        if let Some(connection) = self.connections.get(&id) {
            // Addresses only at debug: a default log keeps no record of who
            // we traded with.
            tracing::debug!(target: "peer", "{} dropped: {error}", connection.address);
            if let Phase::Active { torrent, .. } = &connection.phase {
                torrent.note_error(error);
            }
        }
        self.close(id);
    }

    fn close(&mut self, id: usize) {
        if let Some(mut connection) = self.connections.remove(&id)
            && connection.registered.is_some()
            && let Stream::Tcp(stream) = &mut connection.stream
        {
            let _ = self.poll.registry().deregister(stream);
        }
    }
}

/// Performs a connection's protocol actions: messages are queued, disk
/// work goes to the workers.
fn dispatch(
    id: usize,
    connection: &mut Connection,
    jobs: &mpsc::Sender<Job>,
    actions: Vec<Action>,
) -> Result<(), BackendError> {
    for action in actions {
        let work = match action {
            Action::Send(message) => {
                connection.queue(&message)?;
                continue;
            }
            Action::ReadBlock {
                piece_index,
                begin,
                length,
            } => Work::Read {
                piece_index,
                begin,
                length,
            },
            Action::CheckPiece { download, data } => Work::Check { download, data },
            Action::AnswerHashes(request) => Work::Hashes(request),
        };
        let Phase::Active { torrent, .. } = &connection.phase else {
            continue;
        };
        let job = Job {
            connection: id,
            torrent: Arc::clone(torrent),
            work,
        };
        connection.pending += 1;
        jobs.send(job)
            .map_err(|_| BackendError::Io("disk workers stopped".to_owned()))?;
    }
    Ok(())
}

/// Ticks one active connection's protocol and performs what it asks. A
/// connection whose torrent left the runtime closes.
fn run_tick(
    id: usize,
    connection: &mut Connection,
    jobs: &mpsc::Sender<Job>,
    routes: &crate::runtime::Routes,
) -> Result<(), BackendError> {
    let Phase::Active { torrent, wire } = &mut connection.phase else {
        return Ok(());
    };
    if !routes.is_current(torrent) {
        return Err(invalid("the torrent is no longer shared"));
    }
    let mut out = Vec::new();
    if wire.tick(Instant::now(), &mut out)? == Flow::Finished {
        return Err(invalid("connection finished"));
    }
    dispatch(id, connection, jobs, out)?;
    connection.update_pause();
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn invalid(message: &str) -> BackendError {
    BackendError::InvalidPeerMessage(message.to_owned())
}
