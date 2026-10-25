//! Mainline DHT (BEP 5): finds peers for an info hash without trackers.
//!
//! There is one node per address family (BEP 32): an IPv4 node, and an
//! IPv6 node where the system has IPv6, each with its own socket on the
//! same port and its own routing table. IPv6 nodes exchange contacts as
//! `nodes6` and 18-byte peers. Lookups run on both and their peers merge.
//!
//! Each node's UDP socket serves both directions. A receiver thread answers other
//! nodes' queries and routes responses to waiting lookups; a maintenance
//! thread bootstraps, refreshes the routing table, rotates announce tokens,
//! expires stored peers, and saves known nodes for the next run.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};

type NodeId = [u8; 20];

/// Nodes per routing-table bucket (Kademlia `k`).
const BUCKET_SIZE: usize = 8;
/// Queries in flight per lookup round.
const LOOKUP_PARALLELISM: usize = 8;
const LOOKUP_ROUNDS: usize = 10;
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const TOKEN_ROTATION: Duration = Duration::from_secs(5 * 60);
/// How long peers announced to us are kept.
const PEER_LIFETIME: Duration = Duration::from_secs(30 * 60);
const MAX_STORED_HASHES: usize = 2_000;
const MAX_PEERS_PER_HASH: usize = 100;
/// Below this many nodes the table is still filling, so refresh often.
const HEALTHY_NODES: usize = 64;
const FILLING_REFRESH: Duration = Duration::from_secs(15);
/// How long to wait for the remaining nodes once most have answered.
const STRAGGLER_GRACE: Duration = Duration::from_millis(250);
/// How long an address that ignored a query is skipped.
const SILENT_BACKOFF: Duration = Duration::from_secs(60);
const MAX_SILENT: usize = 10_000;
/// A node that missed this many queries in a row may be replaced.
const MAX_FAILURES: u8 = 2;

/// Well-known public bootstrap nodes.
pub const DEFAULT_BOOTSTRAP: [&str; 3] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
];

#[derive(Debug, Clone)]
struct Node {
    id: NodeId,
    address: SocketAddr,
    last_seen: Instant,
    failures: u8,
}

#[derive(Debug)]
struct RoutingTable {
    own_id: NodeId,
    buckets: Vec<Vec<Node>>,
}

impl RoutingTable {
    fn new(own_id: NodeId) -> Self {
        Self {
            own_id,
            buckets: vec![Vec::new(); 160],
        }
    }

    fn bucket_index(&self, id: &NodeId) -> Option<usize> {
        let distance = xor(&self.own_id, id);
        let zeros = leading_zeros(&distance);
        (zeros < 160).then_some(zeros)
    }

    /// Records a node that just responded or queried us.
    fn insert(&mut self, id: NodeId, address: SocketAddr) {
        let Some(index) = self.bucket_index(&id) else {
            return;
        };
        let bucket = &mut self.buckets[index];
        if let Some(node) = bucket.iter_mut().find(|node| node.id == id) {
            node.address = address;
            node.last_seen = Instant::now();
            node.failures = 0;
            return;
        }
        let node = Node {
            id,
            address,
            last_seen: Instant::now(),
            failures: 0,
        };
        if bucket.len() < BUCKET_SIZE {
            bucket.push(node);
        } else if let Some(stale) = bucket
            .iter_mut()
            .find(|existing| existing.failures >= MAX_FAILURES)
        {
            *stale = node;
        }
    }

    fn mark_failed(&mut self, address: SocketAddr) {
        for node in self.buckets.iter_mut().flatten() {
            if node.address == address {
                node.failures = node.failures.saturating_add(1);
            }
        }
    }

    fn closest(&self, target: &NodeId, count: usize) -> Vec<Node> {
        let mut nodes: Vec<Node> = self
            .buckets
            .iter()
            .flatten()
            .filter(|node| node.failures < MAX_FAILURES)
            .cloned()
            .collect();
        nodes.sort_by_key(|node| xor(&node.id, target));
        nodes.truncate(count);
        nodes
    }

    fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    fn all(&self) -> impl Iterator<Item = &Node> {
        self.buckets.iter().flatten()
    }
}

#[derive(Debug, Default)]
struct PeerStore {
    peers: HashMap<NodeId, Vec<(SocketAddr, Instant)>>,
}

impl PeerStore {
    fn add(&mut self, info_hash: NodeId, peer: SocketAddr) {
        if !self.peers.contains_key(&info_hash) && self.peers.len() >= MAX_STORED_HASHES {
            return;
        }
        let peers = self.peers.entry(info_hash).or_default();
        peers.retain(|(existing, _)| *existing != peer);
        if peers.len() >= MAX_PEERS_PER_HASH {
            peers.remove(0);
        }
        peers.push((peer, Instant::now()));
    }

    fn get(&self, info_hash: &NodeId) -> Vec<SocketAddr> {
        self.peers
            .get(info_hash)
            .map(|peers| peers.iter().map(|(peer, _)| *peer).collect())
            .unwrap_or_default()
    }

    fn expire(&mut self) {
        for peers in self.peers.values_mut() {
            peers.retain(|(_, added)| added.elapsed() < PEER_LIFETIME);
        }
        self.peers.retain(|_, peers| !peers.is_empty());
    }
}

/// A response routed back to a waiting query.
type Reply = (SocketAddr, Value);

#[derive(Debug)]
struct Pending {
    address: SocketAddr,
    sender: mpsc::Sender<Reply>,
}

#[derive(Debug)]
struct Secrets {
    current: [u8; 20],
    previous: [u8; 20],
    rotated: Instant,
}

#[derive(Debug)]
struct Inner {
    socket: UdpSocket,
    /// Whether this is the IPv6 node.
    v6: bool,
    own_id: NodeId,
    table: Mutex<RoutingTable>,
    pending: Mutex<HashMap<[u8; 2], Pending>>,
    next_transaction: AtomicU16,
    store: Mutex<PeerStore>,
    secrets: Mutex<Secrets>,
    shutdown: AtomicBool,
    bootstrap: Vec<String>,
    /// Resolved bootstrap addresses and when they were resolved, so
    /// concurrent lookups do not each repeat the DNS queries.
    bootstrap_cache: Mutex<Option<(Instant, Vec<SocketAddr>)>>,
    /// Addresses that recently ignored a query, skipped until the time
    /// stored, so one silent node does not stall every lookup.
    silent: Mutex<HashMap<SocketAddr, Instant>>,
    state_path: Option<PathBuf>,
    /// uTP connections sharing this UDP port.
    utp: std::sync::OnceLock<Arc<crate::utp::Shared>>,
}

/// A running DHT: an IPv4 node, and an IPv6 node where the system has
/// IPv6. Cheap to share through `Arc`.
#[derive(Debug)]
pub struct Dht {
    families: Vec<Arc<Inner>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Dht {
    /// Binds the DHT sockets on `port` (IPv4, and IPv6 where available)
    /// and starts their threads. `bootstrap` lists `host:port` nodes used
    /// to join the network; `state_path`, when given, keeps the node ids
    /// and known nodes across runs.
    pub fn start(
        port: u16,
        bootstrap: Vec<String>,
        state_path: Option<PathBuf>,
    ) -> Result<Arc<Self>, crate::BackendError> {
        Self::start_on(&crate::Binding::any(), port, bootstrap, state_path)
    }

    /// Like [`Self::start`], on one network interface only. The sockets are
    /// bound to the interface's addresses, so they work again when a VPN
    /// reconnects with the same addresses.
    pub fn start_on(
        binding: &crate::Binding,
        port: u16,
        bootstrap: Vec<String>,
        state_path: Option<PathBuf>,
    ) -> Result<Arc<Self>, crate::BackendError> {
        let sockets = crate::net::udp_sockets(&binding.listen_addresses(port)?)
            .map_err(|error| crate::BackendError::Io(format!("DHT port {port}: {error}")))?;
        let mut families = Vec::new();
        let mut threads = Vec::new();
        for socket in sockets {
            socket
                .set_read_timeout(Some(Duration::from_millis(250)))
                .map_err(|error| crate::BackendError::Io(error.to_string()))?;
            let v6 = socket.local_addr().is_ok_and(|address| address.is_ipv6());
            let path = state_path.as_deref().map(|path| {
                if v6 {
                    state_path_v6(path)
                } else {
                    path.to_owned()
                }
            });
            let inner = Arc::new(Inner::new(socket, bootstrap.clone(), path));
            threads.push({
                let inner = Arc::clone(&inner);
                std::thread::spawn(move || inner.receive_loop())
            });
            threads.push({
                let inner = Arc::clone(&inner);
                std::thread::spawn(move || inner.maintenance_loop())
            });
            families.push(inner);
        }
        Ok(Arc::new(Self {
            families,
            threads: Mutex::new(threads),
        }))
    }

    /// The default public bootstrap nodes.
    pub fn default_bootstrap() -> Vec<String> {
        DEFAULT_BOOTSTRAP
            .iter()
            .map(|node| (*node).to_owned())
            .collect()
    }

    /// Nodes currently in the routing tables, of both families.
    pub fn node_count(&self) -> usize {
        self.families
            .iter()
            .map(|inner| inner.table.lock().map_or(0, |table| table.len()))
            .sum()
    }

    /// Nodes in the IPv6 routing table; 0 without IPv6.
    pub fn ipv6_node_count(&self) -> usize {
        self.families
            .iter()
            .filter(|inner| inner.v6)
            .map(|inner| inner.table.lock().map_or(0, |table| table.len()))
            .sum()
    }

    /// Looks up peers for `info_hash`.
    pub fn get_peers(&self, info_hash: &[u8; 20]) -> Vec<SocketAddr> {
        merged(&self.lookups(info_hash))
    }

    /// Looks up peers for `info_hash` and announces that we serve it on
    /// `port` to the closest nodes. Returns the peers found.
    pub fn announce(&self, info_hash: &[u8; 20], port: u16) -> Vec<SocketAddr> {
        self.announce_then(info_hash, port, |_| {})
    }

    /// Like [`Dht::announce`], but hands the peers found to `found` before
    /// waiting for the announcements to be acknowledged.
    pub(crate) fn announce_then(
        &self,
        info_hash: &[u8; 20],
        port: u16,
        found: impl FnOnce(&[SocketAddr]),
    ) -> Vec<SocketAddr> {
        let lookups = self.lookups(info_hash);
        let peers = merged(&lookups);
        found(&peers);
        let receivers: Vec<_> = lookups
            .iter()
            .flat_map(|(inner, lookup)| {
                lookup.tokens.iter().map(|(address, token)| {
                    inner.query(
                        *address,
                        b"announce_peer",
                        [
                            (&b"info_hash"[..], Value::bytes(*info_hash)),
                            (b"port", Value::Int(port.into())),
                            (b"token", Value::bytes(token.clone())),
                            (b"implied_port", Value::Int(0)),
                        ],
                    )
                })
            })
            .collect();
        for receiver in receivers.into_iter().flatten() {
            let _ = receiver.recv_timeout(QUERY_TIMEOUT);
        }
        peers
    }

    /// Runs a lookup on each family's node at once.
    fn lookups(&self, info_hash: &[u8; 20]) -> Vec<(&Arc<Inner>, Lookup)> {
        std::thread::scope(|scope| {
            let running: Vec<_> = self
                .families
                .iter()
                .map(|inner| (inner, scope.spawn(move || inner.lookup(info_hash))))
                .collect();
            running
                .into_iter()
                .filter_map(|(inner, lookup)| Some((inner, lookup.join().ok()?)))
                .collect()
        })
    }

    /// Handles to the DHT's UDP sockets, one per family, for uTP to send
    /// through.
    pub(crate) fn udp_sockets(&self) -> Result<Vec<UdpSocket>, crate::BackendError> {
        self.families
            .iter()
            .map(|inner| inner.socket.try_clone())
            .collect::<Result<_, _>>()
            .map_err(|error| crate::BackendError::Io(error.to_string()))
    }

    /// Hands uTP datagrams arriving on the DHT's port to `utp`.
    pub(crate) fn route_utp(&self, utp: Arc<crate::utp::Shared>) {
        for inner in &self.families {
            let _ = inner.utp.set(Arc::clone(&utp));
        }
    }

    /// Stops the nodes and saves known nodes.
    pub fn stop(&self) {
        for inner in &self.families {
            inner.shutdown.store(true, Ordering::Release);
        }
        if let Ok(mut threads) = self.threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
        for inner in &self.families {
            inner.save();
        }
    }
}

impl Drop for Dht {
    fn drop(&mut self) {
        for inner in &self.families {
            inner.shutdown.store(true, Ordering::Release);
        }
    }
}

/// The peers of several lookups, each once.
fn merged(lookups: &[(&Arc<Inner>, Lookup)]) -> Vec<SocketAddr> {
    let mut peers: Vec<SocketAddr> = lookups
        .iter()
        .flat_map(|(_, lookup)| lookup.peers.iter().copied())
        .collect();
    peers.sort_unstable();
    peers.dedup();
    peers
}

/// A background DHT search for one torrent, polled from a transfer loop so
/// a slow lookup never delays peers that trackers already returned.
pub(crate) struct PeerSearch {
    dht: Arc<Dht>,
    info_hash: [u8; 20],
    /// When set, we also announce ourselves on this port.
    announce_port: Option<u16>,
    in_flight: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    next: Instant,
    backoff: Backoff,
    /// Peers earlier searches returned, to tell new ones apart.
    seen: std::collections::HashSet<SocketAddr>,
}

/// Repeat interval for DHT searches that found plenty of peers.
const SEARCH_REPEAT: Duration = Duration::from_secs(5 * 60);
/// Fewer peers than this and a search is repeated on the retry schedule.
const ENOUGH_PEERS: usize = 50;
/// First retry after a search found few or no peers (common right after
/// startup, while the routing table fills); doubles up to the maximum.
const SEARCH_RETRY: Duration = Duration::from_secs(2);
const MAX_SEARCH_RETRY: Duration = Duration::from_secs(60);
/// After this long without a new peer, retries slow down further...
const QUIET_PERIOD: Duration = Duration::from_secs(5 * 60);
/// ...doubling up to this, as often as libtorrent searches. A torrent
/// stalled for days then costs a lookup every 15 minutes, not every minute.
const MAX_QUIET_RETRY: Duration = Duration::from_secs(15 * 60);
/// Bound on remembered peers; past it, the memory starts over.
const MAX_SEEN: usize = 10_000;

/// When to search again, from what earlier searches found.
#[derive(Debug)]
struct Backoff {
    retry: Duration,
    /// When a search last turned up a peer not seen before.
    last_new_peer: Instant,
}

impl Backoff {
    fn new(now: Instant) -> Self {
        Self {
            retry: SEARCH_RETRY,
            last_new_peer: now,
        }
    }

    /// The wait before the next search, after one that returned `peers`
    /// peers, `new` of them not seen before.
    fn after(&mut self, peers: usize, new: usize, now: Instant) -> Duration {
        if new > 0 {
            self.last_new_peer = now;
        }
        if peers >= ENOUGH_PEERS {
            self.retry = SEARCH_RETRY;
            return SEARCH_REPEAT;
        }
        // Retries double up to a minute; once nothing new has turned up for
        // a while, they keep doubling up to 15 minutes. A new peer brings
        // them back to a minute at most.
        let limit = if now.saturating_duration_since(self.last_new_peer) < QUIET_PERIOD {
            MAX_SEARCH_RETRY
        } else {
            MAX_QUIET_RETRY
        };
        let delay = self.retry.min(limit);
        self.retry = delay.saturating_mul(2).min(limit);
        delay
    }
}

impl PeerSearch {
    pub(crate) fn new(dht: Arc<Dht>, info_hash: [u8; 20], announce_port: Option<u16>) -> Self {
        Self {
            dht,
            info_hash,
            announce_port,
            in_flight: None,
            next: Instant::now(),
            backoff: Backoff::new(Instant::now()),
            seen: std::collections::HashSet::new(),
        }
    }

    /// Starts a search when one is due and returns the peers of a search
    /// that just finished.
    pub(crate) fn poll(&mut self) -> Vec<SocketAddr> {
        if let Some(receiver) = &self.in_flight {
            match receiver.try_recv() {
                Ok(peers) => {
                    self.in_flight = None;
                    if self.seen.len() + peers.len() > MAX_SEEN {
                        self.seen.clear();
                    }
                    let new = peers.iter().filter(|peer| self.seen.insert(**peer)).count();
                    // A thin result is common while the routing table
                    // fills, so it is retried soon rather than in minutes.
                    let now = Instant::now();
                    self.next = now + self.backoff.after(peers.len(), new, now);
                    return peers;
                }
                Err(mpsc::TryRecvError::Empty) => return Vec::new(),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.in_flight = None;
                    let now = Instant::now();
                    self.next = now + self.backoff.after(0, 0, now);
                }
            }
        }
        if Instant::now() >= self.next {
            let (sender, receiver) = mpsc::channel();
            let (dht, info_hash, port) =
                (Arc::clone(&self.dht), self.info_hash, self.announce_port);
            std::thread::spawn(move || match port {
                // Peers go out as soon as the lookup ends; announcing
                // ourselves waits on replies.
                Some(port) => {
                    dht.announce_then(&info_hash, port, |peers| {
                        let _ = sender.send(peers.to_vec());
                    });
                }
                None => {
                    let _ = sender.send(dht.get_peers(&info_hash));
                }
            });
            self.in_flight = Some(receiver);
        }
        Vec::new()
    }

    pub(crate) fn searching(&self) -> bool {
        self.in_flight.is_some()
    }
}

struct Lookup {
    peers: Vec<SocketAddr>,
    /// Closest nodes that handed us an announce token.
    tokens: Vec<(SocketAddr, Vec<u8>)>,
}

impl Inner {
    /// Loads saved state and makes a node for `socket`'s address family.
    fn new(socket: UdpSocket, bootstrap: Vec<String>, state_path: Option<PathBuf>) -> Self {
        let v6 = socket.local_addr().is_ok_and(|address| address.is_ipv6());
        let (own_id, saved_nodes) = state_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| load_state(&bytes))
            .unwrap_or_else(|| (random_bytes(), Vec::new()));
        let mut table = RoutingTable::new(own_id);
        for (id, address) in saved_nodes {
            if address.is_ipv6() == v6 {
                table.insert(id, address);
            }
        }
        Self {
            socket,
            v6,
            own_id,
            table: Mutex::new(table),
            pending: Mutex::new(HashMap::new()),
            next_transaction: AtomicU16::new(u16::from_be_bytes([random_bytes::<1>()[0], 0])),
            store: Mutex::new(PeerStore::default()),
            secrets: Mutex::new(Secrets {
                current: random_bytes(),
                previous: random_bytes(),
                rotated: Instant::now(),
            }),
            shutdown: AtomicBool::new(false),
            bootstrap,
            bootstrap_cache: Mutex::new(None),
            silent: Mutex::new(HashMap::new()),
            state_path,
            utp: std::sync::OnceLock::new(),
        }
    }

    /// The key nodes are exchanged under: `nodes`, or `nodes6` (BEP 32).
    fn nodes_key(&self) -> &'static [u8] {
        if self.v6 { b"nodes6" } else { b"nodes" }
    }

    fn receive_loop(&self) {
        let batch = std::cell::RefCell::new(crate::utp::Batch::default());
        let _ = crate::net::receive_batches(
            &self.socket,
            &self.shutdown,
            |bytes, from| {
                // uTP shares the port; its headers never start with `d`.
                if crate::utp::is_utp(bytes) {
                    if let Some(utp) = self.utp.get() {
                        utp.handle_in_batch(bytes, from, &mut batch.borrow_mut());
                    }
                    return;
                }
                // Each node takes its own family only.
                let from = crate::net::canonical(from);
                if from.is_ipv6() != self.v6 {
                    return;
                }
                let Ok(message) = bencode::decode(bytes) else {
                    return;
                };
                match message.get_bytes(b"y") {
                    Some(b"q") => self.answer(&message, from),
                    Some(b"r") | Some(b"e") => self.deliver(message, from),
                    _ => {}
                }
            },
            || {
                if let Some(utp) = self.utp.get() {
                    utp.finish(batch.take());
                }
            },
        );
    }

    /// Routes a response to the query waiting on its transaction id.
    fn deliver(&self, message: Value, from: SocketAddr) {
        let Some(transaction) = message
            .get_bytes(b"t")
            .and_then(|t| <[u8; 2]>::try_from(t).ok())
        else {
            return;
        };
        let pending = match self.pending.lock() {
            Ok(mut pending) => match pending.get(&transaction) {
                // Only the node we asked may answer.
                Some(entry) if entry.address == from => pending.remove(&transaction),
                _ => None,
            },
            Err(_) => None,
        };
        let Some(pending) = pending else { return };
        if let Some(id) = message
            .get(b"r")
            .and_then(|response| response.get_bytes(b"id"))
            .and_then(|id| NodeId::try_from(id).ok())
            && let Ok(mut table) = self.table.lock()
        {
            table.insert(id, from);
        }
        let _ = pending.sender.send((from, message));
    }

    /// Answers another node's query.
    fn answer(&self, message: &Value, from: SocketAddr) {
        let Some(transaction) = message.get_bytes(b"t").filter(|t| t.len() <= 8) else {
            return;
        };
        let arguments = message.get(b"a");
        let Some(sender_id) = arguments
            .and_then(|a| a.get_bytes(b"id"))
            .and_then(|id| NodeId::try_from(id).ok())
        else {
            return;
        };
        if let Ok(mut table) = self.table.lock() {
            table.insert(sender_id, from);
        }
        let arguments = arguments.expect("arguments checked above");
        let own_id = Value::bytes(self.own_id);
        let response = match message.get_bytes(b"q") {
            Some(b"ping") => Some(Value::dict([(b"id", own_id)])),
            Some(b"find_node") => arguments
                .get_bytes(b"target")
                .and_then(|target| NodeId::try_from(target).ok())
                .map(|target| {
                    Value::dict([
                        (b"id", own_id),
                        (
                            self.nodes_key(),
                            Value::bytes(self.compact_closest(&target)),
                        ),
                    ])
                }),
            Some(b"get_peers") => arguments
                .get_bytes(b"info_hash")
                .and_then(|hash| NodeId::try_from(hash).ok())
                .map(|info_hash| {
                    let token = Value::bytes(self.token(&from.ip(), false));
                    let peers = self
                        .store
                        .lock()
                        .map(|store| store.get(&info_hash))
                        .unwrap_or_default();
                    if peers.is_empty() {
                        Value::dict([
                            (b"id", own_id),
                            (b"token", token),
                            (
                                self.nodes_key(),
                                Value::bytes(self.compact_closest(&info_hash)),
                            ),
                        ])
                    } else {
                        let values = peers
                            .iter()
                            .map(|peer| Value::bytes(compact_peer(peer)))
                            .collect();
                        Value::dict([
                            (b"id", own_id),
                            (b"token", token),
                            (b"values", Value::List(values)),
                        ])
                    }
                }),
            Some(b"announce_peer") => self
                .store_announce(arguments, from)
                .then(|| Value::dict([(b"id", own_id)])),
            _ => None,
        };
        let reply = match response {
            Some(response) => Value::dict([
                (b"t", Value::bytes(transaction)),
                (b"y", Value::bytes(*b"r")),
                (b"r", response),
            ]),
            None => Value::dict([
                (b"t", Value::bytes(transaction)),
                (b"y", Value::bytes(*b"e")),
                (
                    b"e",
                    Value::List(vec![Value::Int(203), Value::bytes(*b"Protocol Error")]),
                ),
            ]),
        };
        let _ = self.socket.send_to(&reply.encode(), from);
    }

    fn store_announce(&self, arguments: &Value, from: SocketAddr) -> bool {
        let Some(info_hash) = arguments
            .get_bytes(b"info_hash")
            .and_then(|hash| NodeId::try_from(hash).ok())
        else {
            return false;
        };
        let token = arguments.get_bytes(b"token").unwrap_or_default();
        let valid = token == self.token(&from.ip(), false) || token == self.token(&from.ip(), true);
        let port = if arguments.get_int(b"implied_port") == Some(1) {
            Some(from.port())
        } else {
            arguments
                .get_int(b"port")
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port != 0)
        };
        match (valid, port) {
            (true, Some(port)) => {
                if let Ok(mut store) = self.store.lock() {
                    store.add(info_hash, SocketAddr::new(from.ip(), port));
                }
                true
            }
            _ => false,
        }
    }

    /// An announce token bound to the requester's IP, valid for two
    /// rotation periods.
    fn token(&self, ip: &IpAddr, previous: bool) -> Vec<u8> {
        let secret = self
            .secrets
            .lock()
            .map(|secrets| {
                if previous {
                    secrets.previous
                } else {
                    secrets.current
                }
            })
            .unwrap_or_default();
        let mut hasher = Sha1::new();
        hasher.update(secret);
        match ip {
            IpAddr::V4(ip) => hasher.update(ip.octets()),
            IpAddr::V6(ip) => hasher.update(ip.octets()),
        }
        hasher.finalize()[..8].to_vec()
    }

    fn compact_closest(&self, target: &NodeId) -> Vec<u8> {
        let nodes = self
            .table
            .lock()
            .map(|table| table.closest(target, BUCKET_SIZE))
            .unwrap_or_default();
        nodes
            .iter()
            .flat_map(|node| {
                let mut bytes = node.id.to_vec();
                bytes.extend(compact_peer(&node.address));
                bytes
            })
            .collect()
    }

    /// Sends a query; the returned receiver yields the response.
    fn query<const N: usize>(
        &self,
        address: SocketAddr,
        method: &[u8],
        arguments: [(&[u8], Value); N],
    ) -> Option<mpsc::Receiver<Reply>> {
        let (sender, receiver) = mpsc::channel();
        self.query_into(address, method, arguments, sender)
            .map(|_| receiver)
    }

    fn query_into<const N: usize>(
        &self,
        address: SocketAddr,
        method: &[u8],
        arguments: [(&[u8], Value); N],
        sender: mpsc::Sender<Reply>,
    ) -> Option<[u8; 2]> {
        let transaction = self
            .next_transaction
            .fetch_add(1, Ordering::Relaxed)
            .to_be_bytes();
        let mut arguments: std::collections::BTreeMap<Vec<u8>, Value> = arguments
            .into_iter()
            .map(|(key, value)| (key.to_vec(), value))
            .collect();
        arguments.insert(b"id".to_vec(), Value::bytes(self.own_id));
        let message = Value::dict([
            (b"t", Value::bytes(transaction)),
            (b"y", Value::bytes(*b"q")),
            (b"q", Value::bytes(method)),
            (b"a", Value::Dict(arguments)),
            (b"v", Value::bytes(crate::identity::DHT_VERSION)),
        ]);
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(transaction, Pending { address, sender });
        }
        match self.socket.send_to(&message.encode(), address) {
            Ok(_) => Some(transaction),
            Err(_) => {
                self.forget(&[transaction]);
                None
            }
        }
    }

    fn forget(&self, transactions: &[[u8; 2]]) {
        if let Ok(mut pending) = self.pending.lock() {
            for transaction in transactions {
                pending.remove(transaction);
            }
        }
    }

    /// Queries several nodes at once and collects the responses that
    /// arrive before the timeout. Silent nodes are marked as failing.
    fn query_all(&self, addresses: &[SocketAddr], method: &[u8], target: &NodeId) -> Vec<Reply> {
        let (sender, receiver) = mpsc::channel();
        let key: &[u8] = if method == b"find_node" {
            b"target"
        } else {
            b"info_hash"
        };
        let transactions: Vec<[u8; 2]> = addresses
            .iter()
            .filter_map(|address| {
                self.query_into(
                    *address,
                    method,
                    [(key, Value::bytes(*target))],
                    sender.clone(),
                )
            })
            .collect();
        let sent = transactions.len();
        drop(sender);
        let mut deadline = Instant::now() + QUERY_TIMEOUT;
        let mut replies = Vec::new();
        while replies.len() < sent {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(reply) => {
                    replies.push(reply);
                    // Once most nodes answered, give stragglers only a
                    // moment: on the public DHT some never reply.
                    if replies.len() * 2 >= sent {
                        deadline = deadline.min(Instant::now() + STRAGGLER_GRACE);
                    }
                }
                Err(_) => break,
            }
        }
        let answered: HashSet<SocketAddr> = replies.iter().map(|(address, _)| *address).collect();
        if let Ok(mut table) = self.table.lock() {
            for address in addresses
                .iter()
                .filter(|address| !answered.contains(address))
            {
                table.mark_failed(*address);
            }
        }
        if let Ok(mut silent) = self.silent.lock() {
            let retry_at = Instant::now() + SILENT_BACKOFF;
            for address in addresses {
                if answered.contains(address) {
                    silent.remove(address);
                } else if silent.len() < MAX_SILENT {
                    silent.insert(*address, retry_at);
                }
            }
        }
        // Answered transactions were removed on delivery; drop the rest.
        self.forget(&transactions);
        replies
    }

    /// Iteratively queries the nodes closest to `target`: `get_peers` for an
    /// info hash, collecting peers and announce tokens along the way.
    fn lookup(&self, target: &NodeId) -> Lookup {
        self.iterate(target, b"get_peers")
    }

    fn iterate(&self, target: &NodeId, method: &[u8]) -> Lookup {
        let mut candidates: HashMap<SocketAddr, NodeId> = self
            .table
            .lock()
            .map(|table| table.closest(target, BUCKET_SIZE * 2))
            .unwrap_or_default()
            .into_iter()
            .map(|node| (node.address, node.id))
            .collect();
        // Bootstrap nodes have unknown ids; rank them as far away so real
        // nodes are preferred once known.
        if candidates.len() < BUCKET_SIZE {
            for address in self.resolve_bootstrap() {
                candidates
                    .entry(address)
                    .or_insert_with(|| xor(target, &[0xff; 20]));
            }
        }
        let mut queried = HashSet::new();
        let mut peers = HashSet::new();
        let mut tokens: Vec<(NodeId, SocketAddr, Vec<u8>)> = Vec::new();
        for _ in 0..LOOKUP_ROUNDS {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            candidates.retain(|address, _| !self.is_silent(address));
            let mut ranked: Vec<(SocketAddr, NodeId)> = candidates
                .iter()
                .map(|(address, id)| (*address, *id))
                .collect();
            ranked.sort_by_key(|(_, id)| xor(id, target));
            let closest_done = ranked
                .iter()
                .take(BUCKET_SIZE)
                .all(|(address, _)| queried.contains(address));
            if closest_done {
                break;
            }
            let batch: Vec<SocketAddr> = ranked
                .iter()
                .filter(|(address, _)| !queried.contains(address))
                .take(LOOKUP_PARALLELISM)
                .map(|(address, _)| *address)
                .collect();
            queried.extend(batch.iter().copied());
            let replies = self.query_all(&batch, method, target);
            // Silent nodes leave the candidate set, so the lookup only ends
            // once the closest nodes that actually answer were asked.
            for address in &batch {
                if !replies.iter().any(|(from, _)| from == address) {
                    candidates.remove(address);
                }
            }
            for (from, reply) in replies {
                let Some(response) = reply.get(b"r") else {
                    continue;
                };
                if let Some(id) = response
                    .get_bytes(b"id")
                    .and_then(|id| NodeId::try_from(id).ok())
                {
                    candidates.insert(from, id);
                    if let Some(token) = response
                        .get_bytes(b"token")
                        .filter(|token| token.len() <= 64)
                    {
                        tokens.push((id, from, token.to_vec()));
                    }
                }
                if let Some(nodes) = response.get_bytes(self.nodes_key()) {
                    for (id, address) in parse_compact_nodes(nodes, self.v6) {
                        if id != self.own_id {
                            candidates.entry(address).or_insert(id);
                        }
                    }
                }
                for value in response
                    .get(b"values")
                    .and_then(Value::as_list)
                    .unwrap_or_default()
                {
                    if let Some(peer) = value.as_bytes().and_then(parse_compact_peer) {
                        peers.insert(peer);
                    }
                }
            }
        }
        tokens.sort_by_key(|(id, _, _)| xor(id, target));
        tokens.truncate(BUCKET_SIZE);
        Lookup {
            peers: peers.into_iter().collect(),
            tokens: tokens
                .into_iter()
                .map(|(_, address, token)| (address, token))
                .collect(),
        }
    }

    fn resolve_bootstrap(&self) -> Vec<SocketAddr> {
        if let Ok(cache) = self.bootstrap_cache.lock()
            && let Some((resolved, addresses)) = cache.as_ref()
            && resolved.elapsed() < REFRESH_INTERVAL
            && !addresses.is_empty()
        {
            return addresses.clone();
        }
        let addresses: Vec<SocketAddr> = self
            .bootstrap
            .iter()
            .filter_map(|host| host.to_socket_addrs().ok())
            .flatten()
            .filter(|address| address.is_ipv6() == self.v6)
            .collect();
        if let Ok(mut cache) = self.bootstrap_cache.lock() {
            *cache = Some((Instant::now(), addresses.clone()));
        }
        addresses
    }

    fn is_silent(&self, address: &SocketAddr) -> bool {
        self.silent
            .lock()
            .ok()
            .and_then(|silent| silent.get(address).copied())
            .is_some_and(|retry_at| retry_at > Instant::now())
    }

    fn maintenance_loop(&self) {
        let mut last_refresh: Option<Instant> = None;
        let mut last_save = Instant::now();
        while !self.shutdown.load(Ordering::Acquire) {
            let filling = self.table.lock().map_or(0, |table| table.len()) < HEALTHY_NODES;
            let interval = if filling {
                FILLING_REFRESH
            } else {
                REFRESH_INTERVAL
            };
            if last_refresh.is_none_or(|at| at.elapsed() >= interval) {
                // Looking up our own id fills the buckets nearest to us;
                // random ids spread entries across the rest.
                self.iterate(&self.own_id, b"find_node");
                self.iterate(&random_bytes(), b"find_node");
                if filling {
                    self.iterate(&random_bytes(), b"find_node");
                }
                last_refresh = Some(Instant::now());
            }
            if let Ok(mut secrets) = self.secrets.lock()
                && secrets.rotated.elapsed() >= TOKEN_ROTATION
            {
                secrets.previous = secrets.current;
                secrets.current = random_bytes();
                secrets.rotated = Instant::now();
            }
            if let Ok(mut silent) = self.silent.lock() {
                let now = Instant::now();
                silent.retain(|_, retry_at| *retry_at > now);
            }
            if let Ok(mut store) = self.store.lock() {
                store.expire();
            }
            if last_save.elapsed() >= REFRESH_INTERVAL {
                self.save();
                last_save = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn save(&self) {
        let (Some(path), Ok(table)) = (&self.state_path, self.table.lock()) else {
            return;
        };
        let mut bytes = if self.v6 { STATE_V6 } else { STATE_V4 }.to_vec();
        bytes.extend(self.own_id);
        for node in table.all().filter(|node| node.failures < MAX_FAILURES) {
            bytes.extend(node.id);
            bytes.extend(compact_peer(&node.address));
        }
        let temporary = path.with_extension("tmp");
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(temporary, path);
        }
    }
}

/// Saved state headers, for the IPv4 and the IPv6 node.
const STATE_V4: &[u8; 6] = b"EBDHT1";
const STATE_V6: &[u8; 6] = b"EBDHT6";

fn load_state(bytes: &[u8]) -> Option<(NodeId, Vec<(NodeId, SocketAddr)>)> {
    let (rest, v6) = match bytes.strip_prefix(STATE_V4) {
        Some(rest) => (rest, false),
        None => (bytes.strip_prefix(STATE_V6)?, true),
    };
    let own_id = NodeId::try_from(rest.get(..20)?).ok()?;
    Some((own_id, parse_compact_nodes(&rest[20..], v6)))
}

/// Compact node info: a 20-byte id and a 6-byte (IPv4) or 18-byte (IPv6,
/// BEP 32 `nodes6`) contact.
fn parse_compact_nodes(bytes: &[u8], v6: bool) -> Vec<(NodeId, SocketAddr)> {
    let size = if v6 { 38 } else { 26 };
    bytes
        .chunks_exact(size)
        .filter_map(|node| {
            let id = NodeId::try_from(&node[..20]).ok()?;
            Some((id, parse_compact_peer(&node[20..])?))
        })
        .collect()
}

/// A compact peer: 4 address bytes and a port, or 16 and a port for IPv6.
fn parse_compact_peer(bytes: &[u8]) -> Option<SocketAddr> {
    let (ip, port): (IpAddr, &[u8]) = match bytes.len() {
        6 => (
            Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[..4]).ok()?).into(),
            &bytes[4..],
        ),
        18 => (
            Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[..16]).ok()?).into(),
            &bytes[16..],
        ),
        _ => return None,
    };
    let port = u16::from_be_bytes([port[0], port[1]]);
    (port != 0).then(|| SocketAddr::new(ip, port))
}

fn compact_peer(address: &SocketAddr) -> Vec<u8> {
    let mut bytes = match address.ip() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    bytes.extend(address.port().to_be_bytes());
    bytes
}

/// Where the IPv6 node keeps its state: next to the IPv4 node's, with a
/// `6` after the name (`dht.dat` and `dht6.dat`).
fn state_path_v6(path: &std::path::Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(extension) => format!("{stem}6.{}", extension.to_string_lossy()),
        None => format!("{stem}6"),
    };
    path.with_file_name(name)
}

fn xor(left: &NodeId, right: &NodeId) -> NodeId {
    std::array::from_fn(|index| left[index] ^ right[index])
}

fn leading_zeros(id: &NodeId) -> usize {
    let mut zeros = 0;
    for byte in id {
        if *byte == 0 {
            zeros += 8;
        } else {
            return zeros + byte.leading_zeros() as usize;
        }
    }
    zeros
}

/// Unpredictable bytes from the standard library's per-process random
/// hasher keys, mixed with a counter and the clock.
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut out = [0; N];
    for chunk in out.chunks_mut(8) {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
        );
        let value = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs searches back to back from `start`, each returning `result`
    /// (peers, new peers), and returns the delays chosen.
    fn delays(
        backoff: &mut Backoff,
        now: &mut Instant,
        searches: usize,
        result: (usize, usize),
    ) -> Vec<u64> {
        (0..searches)
            .map(|_| {
                let delay = backoff.after(result.0, result.1, *now);
                *now += delay;
                delay.as_secs()
            })
            .collect()
    }

    #[test]
    fn searches_for_a_stalled_torrent_slow_down_to_every_15_minutes() {
        let mut now = Instant::now();
        let mut backoff = Backoff::new(now);
        // At first, three peers that cannot help: fast retries capped at a
        // minute, for the first five quiet minutes.
        assert_eq!(delays(&mut backoff, &mut now, 1, (3, 3)), [2]);
        let early = delays(&mut backoff, &mut now, 9, (3, 0));
        assert_eq!(early, [4, 8, 16, 32, 60, 60, 60, 60, 60]);
        // Then the same peers keep coming back: retries double to 15 minutes
        // and stay there.
        let later = delays(&mut backoff, &mut now, 8, (3, 0));
        assert_eq!(later, [120, 240, 480, 900, 900, 900, 900, 900]);
        // A new peer turns up: back to a minute at most.
        assert_eq!(delays(&mut backoff, &mut now, 2, (4, 1)), [60, 60]);
        // A big swarm is searched every five minutes.
        assert_eq!(delays(&mut backoff, &mut now, 2, (80, 0)), [300, 300]);
        assert_eq!(delays(&mut backoff, &mut now, 1, (0, 0)), [2]);
    }

    fn local_node(bootstrap: Vec<String>) -> Arc<Dht> {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        Dht::start(port, bootstrap, None).unwrap()
    }

    fn port_of(dht: &Dht) -> u16 {
        dht.families[0].socket.local_addr().unwrap().port()
    }

    #[test]
    fn routing_table_orders_by_distance_and_replaces_failed_nodes() {
        let mut table = RoutingTable::new([0; 20]);
        let address = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut far = [0; 20];
        far[0] = 0x80;
        let mut near = [0; 20];
        near[19] = 1;
        table.insert(far, address(1));
        table.insert(near, address(2));
        table.insert([0; 20], address(3)); // our own id is never stored
        assert_eq!(table.len(), 2);
        assert_eq!(table.closest(&[0; 20], 1)[0].id, near);

        // Fill the bucket for ids starting with 1, then fail one entry.
        for index in 1..=BUCKET_SIZE as u8 {
            let mut id = far;
            id[19] = index;
            table.insert(id, address(100 + u16::from(index)));
        }
        let bucket = table.bucket_index(&far).unwrap();
        assert_eq!(table.buckets[bucket].len(), BUCKET_SIZE);
        table.mark_failed(address(1));
        table.mark_failed(address(1));
        let mut newcomer = far;
        newcomer[18] = 9;
        table.insert(newcomer, address(200));
        assert!(table.buckets[bucket].iter().any(|node| node.id == newcomer));
    }

    #[test]
    fn peers_announced_through_the_dht_can_be_found() {
        // Three local nodes: a bootstrap node and two that join through it.
        let root = local_node(Vec::new());
        let bootstrap = vec![format!("127.0.0.1:{}", port_of(&root))];
        let seeder = local_node(bootstrap.clone());
        let leecher = local_node(bootstrap);
        let info_hash = [0x42; 20];

        seeder.announce(&info_hash, 51_413);
        let deadline = Instant::now() + Duration::from_secs(10);
        let peers = loop {
            let peers = leecher.get_peers(&info_hash);
            if !peers.is_empty() || Instant::now() > deadline {
                break peers;
            }
        };
        assert_eq!(peers, ["127.0.0.1:51413".parse().unwrap()]);
        assert!(leecher.node_count() >= 1);
        for node in [&root, &seeder, &leecher] {
            node.stop();
        }
    }

    #[test]
    fn peers_announced_through_the_ipv6_dht_can_be_found() {
        let root = local_node(Vec::new());
        let Some(v6) = root.families.iter().find(|inner| inner.v6) else {
            eprintln!("skipped: no IPv6 here");
            return;
        };
        // Joining through an IPv6 bootstrap node, only the IPv6 nodes meet.
        let port = v6.socket.local_addr().unwrap().port();
        let bootstrap = vec![format!("[::1]:{port}")];
        let seeder = local_node(bootstrap.clone());
        let leecher = local_node(bootstrap);
        let info_hash = [0x66; 20];

        seeder.announce(&info_hash, 51_413);
        let deadline = Instant::now() + Duration::from_secs(10);
        let peers = loop {
            let peers = leecher.get_peers(&info_hash);
            if !peers.is_empty() || Instant::now() > deadline {
                break peers;
            }
        };
        assert_eq!(peers, ["[::1]:51413".parse().unwrap()]);
        assert!(leecher.ipv6_node_count() >= 1);
        for node in [&root, &seeder, &leecher] {
            node.stop();
        }
    }

    #[test]
    fn encodes_ipv6_contacts_as_bep_32_says() {
        let peer: SocketAddr = "[2001:db8::1]:6881".parse().unwrap();
        let compact = compact_peer(&peer);
        assert_eq!(compact.len(), 18);
        assert_eq!(parse_compact_peer(&compact), Some(peer));
        let mut nodes = [5; 20].to_vec();
        nodes.extend(&compact);
        assert_eq!(parse_compact_nodes(&nodes, true), [([5; 20], peer)]);
        // 38-byte entries read as IPv4 ones would be garbage; they are not.
        assert!(parse_compact_nodes(&nodes, false).is_empty());

        let mut state = STATE_V6.to_vec();
        state.extend([7; 20]);
        state.extend(&nodes);
        assert_eq!(load_state(&state), Some(([7; 20], vec![([5; 20], peer)])));
        assert_eq!(
            state_path_v6(std::path::Path::new("/data/dht.dat")),
            std::path::Path::new("/data/dht6.dat")
        );
    }

    #[test]
    fn rejects_announces_without_a_valid_token() {
        let node = local_node(Vec::new());
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let query = Value::dict([
            (b"t", Value::bytes(*b"aa")),
            (b"y", Value::bytes(*b"q")),
            (b"q", Value::bytes(*b"announce_peer")),
            (
                b"a",
                Value::dict([
                    (b"id", Value::bytes([1; 20])),
                    (b"info_hash", Value::bytes([2; 20])),
                    (b"port", Value::Int(6881)),
                    (b"token", Value::bytes(*b"forged")),
                ]),
            ),
        ]);
        client
            .send_to(&query.encode(), ("127.0.0.1", port_of(&node)))
            .unwrap();
        let mut buffer = [0; 512];
        let length = client.recv(&mut buffer).unwrap();
        let reply = bencode::decode(&buffer[..length]).unwrap();
        assert_eq!(reply.get_bytes(b"y"), Some(&b"e"[..]));
        assert!(
            node.families[0]
                .store
                .lock()
                .unwrap()
                .get(&[2; 20])
                .is_empty()
        );
        node.stop();
    }

    /// Needs network access: joins the public mainline DHT through the
    /// default bootstrap nodes. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn joins_the_public_dht() {
        let node = local_node(Dht::default_bootstrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while node.node_count() < 20 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        let joined = node.node_count();
        node.stop();
        assert!(joined >= 20, "only {joined} nodes after 30 s");
    }

    #[test]
    fn persists_node_id_and_nodes() {
        let path = std::env::temp_dir().join(format!("empirebt-dht-{}.dat", std::process::id()));
        let mut bytes = b"EBDHT1".to_vec();
        bytes.extend([7; 20]);
        bytes.extend([9; 20]);
        bytes.extend([127, 0, 0, 1, 0x1a, 0xe1]);
        std::fs::write(&path, &bytes).unwrap();
        let (id, nodes) = load_state(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(id, [7; 20]);
        assert_eq!(nodes, [([9; 20], "127.0.0.1:6881".parse().unwrap())]);
        let _ = std::fs::remove_file(path);
    }
}
