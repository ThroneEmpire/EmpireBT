//! What one download's peer connections share: who we upload to (choking),
//! which newly verified pieces to announce, and how far each peer is
//! trusted.
//!
//! Choking follows BEP 3's tit-for-tat. Every 10 seconds the interested
//! peers that gave us the most data in the last interval get the regular
//! upload slots. One more slot, the optimistic unchoke, rotates every 30
//! seconds among the rest, with peers that joined recently three times as
//! likely to get it. A new peer then has a chance to prove itself, and we
//! find better partners than the current ones.
//!
//! In a runtime, the event loop chokes for every torrent at once
//! ([`rechoke_all`]), as libtorrent's session does: a global number of
//! upload slots goes to the best peers of all torrents, at most a
//! torrent's own number to each, and a fifth of them rotate
//! optimistically. Among peers that sent us equally much, those of the
//! torrent its swarm most needs (seed rank) come first. A swarm on its own
//! chokes for itself the same way, with no global limit.
//!
//! Each piece is downloaded from a single peer, so a piece that fails its
//! hash check names its sender. That peer loses trust, good pieces earn it
//! back, and an address that keeps sending corrupt data is banned for the
//! rest of the download. Bans cover the whole IP address, as in libtorrent,
//! except on loopback, where separate local programs share one address.
//!
//! Connected peers' addresses are what we tell other peers about through
//! peer exchange (BEP 11). Addresses learned that way are untrusted, so
//! they are checked and limited before joining the connection queue.
//! Candidates are ranked by past results, then by canonical peer priority
//! (BEP 40) against our public address, which is learned from what peers
//! report seeing us as.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Upload slots, including the optimistic one.
pub(crate) const DEFAULT_UPLOAD_SLOTS: usize = 4;
const RECHOKE_INTERVAL: Duration = Duration::from_secs(10);
const OPTIMISTIC_INTERVAL: Duration = Duration::from_secs(30);
/// Peers this new get three chances at the optimistic slot.
const NEW_PEER: Duration = Duration::from_secs(60);
const TRUST_MAX: i32 = 8;
const CORRUPT_PENALTY: i32 = 2;
/// Three corrupt pieces without good ones in between.
const TRUST_BAN: i32 = -6;
/// Addresses accepted from any one peer's exchanges, so no single source
/// fills the connection queue.
const MAX_FROM_ONE_SOURCE: usize = 100;
/// Addresses remembered from peer exchange.
const MAX_DISCOVERED: usize = 1000;
/// Addresses from trackers and the DHT waiting to be tried.
const MAX_OFFERED: usize = 2000;

/// What trust and bans attach to: the IP address, or for loopback peers
/// the full socket address.
fn identity(address: SocketAddr) -> SocketAddr {
    if address.ip().is_loopback() {
        address
    } else {
        SocketAddr::new(address.ip(), 0)
    }
}

/// A connected peer, from [`Swarm::join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PeerKey(u64);

#[derive(Debug)]
struct Peer {
    address: SocketAddr,
    joined: Instant,
    interested: bool,
    unchoked: bool,
    /// Payload bytes received from the peer, and at the last rechoke.
    downloaded: u64,
    downloaded_before: u64,
    /// Bytes per rechoke interval it sent us last time.
    rate: u64,
    uploaded: u64,
    /// Bytes we sent it by the last rechoke, and in the interval before.
    uploaded_before: u64,
    uploaded_round: u64,
    /// When we last unchoked it (when it connected, before that), and how
    /// much we had sent it then.
    last_unchoke: Instant,
    uploaded_at_unchoke: u64,
    /// The address other peers can reach it on, and its BEP 11 flags.
    pex: Option<(SocketAddr, u8)>,
    /// Its handshake peer id, once claimed.
    peer_id: Option<[u8; 20]>,
    /// Set when a duplicate connection to the same peer replaced this one.
    evicted: bool,
    /// How the connection is carried, for display.
    outgoing: bool,
    utp: bool,
    encrypted: bool,
    /// The encryption handshake was used, whether or not the stream then
    /// stayed encrypted.
    obfuscated: bool,
    /// It stopped sending blocks we asked for.
    snubbed: bool,
    /// It supports the hole-punch extension (BEP 55).
    holepunch: bool,
    /// We reached it by punching a hole through a NAT (BEP 55).
    holepunched: bool,
    /// `ut_holepunch` messages for it, from relaying for another peer;
    /// its connection sends them on its next tick.
    holepunch_outbox: Vec<crate::holepunch::Message>,
    /// The client it named in its extension handshake.
    client: Option<String>,
    /// Pieces it has, of how many.
    pieces: Option<(usize, usize)>,
    /// Whether it lets us download, and whether we want to.
    choking_us: bool,
    interesting: bool,
    /// Totals and time of the last rate sample, and the rates then.
    sampled: (u64, u64, Instant),
    rates: (f64, f64),
}

/// A connected peer, as shown to the user.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub address: SocketAddr,
    /// The client's name: from its extension handshake, else decoded from
    /// its peer id.
    pub client: Option<String>,
    /// We connected to it (rather than it to us).
    pub outgoing: bool,
    pub utp: bool,
    /// The stream is RC4-encrypted.
    pub encrypted: bool,
    /// The encryption handshake was used; without `encrypted`, the stream
    /// went on in plain text.
    pub handshake_encrypted: bool,
    /// It has the optimistic upload slot.
    pub optimistic: bool,
    /// It stopped sending blocks we asked for.
    pub snubbed: bool,
    /// Where we heard of its address (`SOURCE_*` flags; none for peers
    /// that connected to us).
    pub sources: u8,
    /// We reached it by punching a hole through a NAT (BEP 55).
    pub holepunched: bool,
    /// Its share of the torrent, once it said which pieces it has.
    pub progress: Option<f64>,
    /// Bytes per second from and to it, measured over at least a second.
    pub down_rate: f64,
    pub up_rate: f64,
    /// Payload bytes from and to it on this connection.
    pub downloaded: u64,
    pub uploaded: u64,
    /// It wants pieces from us, and we let it have them.
    pub interested: bool,
    pub unchoked: bool,
    /// We want pieces from it, and it lets us have them.
    pub interesting: bool,
    pub choking_us: bool,
}

impl PeerInfo {
    /// The connection's flags as qBittorrent shows them (libtorrent's
    /// letters, in qBittorrent's order), each with its meaning.
    pub fn flags(&self) -> Vec<(char, &'static str)> {
        let mut flags = Vec::new();
        if self.interesting {
            flags.push(if self.choking_us {
                ('d', "Interested (local) and choked (peer)")
            } else {
                ('D', "Interested (local) and unchoked (peer)")
            });
        }
        if self.interested {
            flags.push(if self.unchoked {
                ('U', "Interested (peer) and unchoked (local)")
            } else {
                ('u', "Interested (peer) and choked (local)")
            });
        }
        if !self.choking_us && !self.interesting {
            flags.push(('K', "Not interested (local) and unchoked (peer)"));
        }
        if self.unchoked && !self.interested {
            flags.push(('?', "Not interested (peer) and unchoked (local)"));
        }
        if self.optimistic {
            flags.push(('O', "Optimistic unchoke"));
        }
        if self.snubbed {
            flags.push(('S', "Peer snubbed"));
        }
        if !self.outgoing {
            flags.push(('I', "Incoming connection"));
        }
        if self.sources & SOURCE_DHT != 0 {
            flags.push(('H', "Peer from DHT"));
        }
        if self.sources & SOURCE_PEX != 0 {
            flags.push(('X', "Peer from PEX"));
        }
        if self.sources & SOURCE_LSD != 0 {
            flags.push(('L', "Peer from LSD"));
        }
        if self.encrypted {
            flags.push(('E', "Encrypted traffic"));
        } else if self.handshake_encrypted {
            flags.push(('e', "Encrypted handshake"));
        }
        if self.utp {
            flags.push(('P', "μTP"));
        }
        if self.holepunched {
            flags.push(('h', "Peer is using NAT hole punching"));
        }
        flags
    }
}

/// Hole-punch messages queued for one peer at most, so a peer asking for
/// introductions over and over cannot pile them up.
const MAX_HOLEPUNCH_QUEUED: usize = 16;

/// Where an address came from, as [`PeerInfo::sources`] has it.
pub const SOURCE_TRACKER: u8 = 1;
pub const SOURCE_DHT: u8 = 2;
pub const SOURCE_PEX: u8 = 4;
pub const SOURCE_LSD: u8 = 8;

/// After a failed connection an address waits this long times its
/// failures before it is tried again, as libtorrent's
/// `min_reconnect_time`...
const RECONNECT_DELAY: Duration = Duration::from_secs(60);
/// ...and after this many failures in a row (libtorrent's `max_failcount`)
/// it rests until trackers and the DHT have had time to say it is back.
const MAX_FAILURES: u32 = 3;
const GIVE_UP_DELAY: Duration = Duration::from_secs(30 * 60);

/// Rates are measured over windows at least this long.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// What we know about an address across connections.
#[derive(Debug, Default, Clone, Copy)]
struct History {
    /// Payload bytes it has sent us.
    received: u64,
    /// Connections that failed in a row before any data arrived.
    failures: u32,
    /// Not dialed again before this, after failing.
    retry_at: Option<Instant>,
}

#[derive(Debug)]
struct Inner {
    peers: HashMap<PeerKey, Peer>,
    next_key: u64,
    /// Verified pieces, in the order they completed.
    haves: Vec<u32>,
    trust: HashMap<SocketAddr, i32>,
    banned: Vec<SocketAddr>,
    history: HashMap<SocketAddr, History>,
    last_rechoke: Option<Instant>,
    last_optimistic: Option<Instant>,
    optimistic: Option<PeerKey>,
    random: u64,
    /// Every address already known, from any source.
    known: HashSet<SocketAddr>,
    /// Addresses from peer exchange waiting to be tried.
    discovered: VecDeque<SocketAddr>,
    /// Addresses from peer exchange, for later rounds to retry.
    exchanged: Vec<SocketAddr>,
    from_source: HashMap<SocketAddr, usize>,
    /// Where each known address came from (`SOURCE_*` flags).
    sources: HashMap<SocketAddr, u8>,
    /// Addresses peer exchange said can be hole-punched (BEP 55), and the
    /// connection that said so, to ask for an introduction if dialing
    /// them fails. Taken once per address, so nobody is asked twice.
    introducers: HashMap<SocketAddr, SocketAddr>,
    /// Addresses an introducer told us to connect to now, over uTP.
    holepunch_dials: Vec<SocketAddr>,
    /// Who reported seeing us at each address (BEP 10 `yourip`).
    votes: HashMap<IpAddr, HashSet<IpAddr>>,
    listen_port: Option<u16>,
}

#[derive(Debug)]
pub(crate) struct Swarm {
    inner: Mutex<Inner>,
    upload_slots: usize,
    /// Set once the event loop chokes for this swarm: the shared slot
    /// settings and count, which then replace `upload_slots`.
    managed: std::sync::OnceLock<std::sync::Arc<crate::limits::UploadSlots>>,
}

impl Swarm {
    pub fn new(upload_slots: usize) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_nanos() as u64)
            | 1;
        Self {
            inner: Mutex::new(Inner {
                peers: HashMap::new(),
                next_key: 0,
                haves: Vec::new(),
                trust: HashMap::new(),
                banned: Vec::new(),
                history: HashMap::new(),
                last_rechoke: None,
                last_optimistic: None,
                optimistic: None,
                random: seed,
                known: HashSet::new(),
                sources: HashMap::new(),
                introducers: HashMap::new(),
                holepunch_dials: Vec::new(),
                discovered: VecDeque::new(),
                exchanged: Vec::new(),
                from_source: HashMap::new(),
                votes: HashMap::new(),
                listen_port: None,
            }),
            upload_slots: upload_slots.max(1),
            managed: std::sync::OnceLock::new(),
        }
    }

    /// Hands choking to the event loop, which from now on decides for
    /// this swarm with every other ([`rechoke_all`]) under `slots`.
    pub(crate) fn manage(&self, slots: &std::sync::Arc<crate::limits::UploadSlots>) {
        let inner = self.lock();
        if self.managed.set(std::sync::Arc::clone(slots)).is_ok() {
            let unchoked = inner.peers.values().filter(|peer| peer.unchoked).count();
            slots.unchoked.fetch_add(unchoked, Ordering::Relaxed);
        }
    }

    /// Upload slots for this torrent.
    fn slots(&self) -> usize {
        self.managed
            .get()
            .map_or(self.upload_slots, |slots| slots.per_torrent())
    }

    /// Counts a peer unchoked (`1`) or choked (`-1`) in the shared count,
    /// when the event loop manages this swarm.
    fn count_unchoked(&self, unchoked: bool) {
        if let Some(slots) = self.managed.get() {
            if unchoked {
                slots.unchoked.fetch_add(1, Ordering::Relaxed);
            } else {
                // Never below 0, should a count race with a recount.
                let mut count = slots.unchoked.load(Ordering::Relaxed);
                while let Err(actual) = slots.unchoked.compare_exchange_weak(
                    count,
                    count.saturating_sub(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    count = actual;
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a connected peer, or returns `None` if its address is
    /// banned.
    pub fn join(&self, address: SocketAddr, now: Instant) -> Option<PeerKey> {
        let mut inner = self.lock();
        if inner.banned.contains(&identity(address)) {
            return None;
        }
        // It answered: failures before are forgiven.
        if let Some(history) = inner.history.get_mut(&address) {
            history.failures = 0;
            history.retry_at = None;
        }
        let key = PeerKey(inner.next_key);
        inner.next_key += 1;
        inner.peers.insert(
            key,
            Peer {
                address,
                joined: now,
                interested: false,
                unchoked: false,
                downloaded: 0,
                downloaded_before: 0,
                rate: 0,
                uploaded: 0,
                uploaded_before: 0,
                uploaded_round: 0,
                last_unchoke: now,
                uploaded_at_unchoke: 0,
                pex: None,
                peer_id: None,
                evicted: false,
                outgoing: false,
                utp: false,
                encrypted: false,
                obfuscated: false,
                snubbed: false,
                holepunch: false,
                holepunched: false,
                holepunch_outbox: Vec::new(),
                client: None,
                pieces: None,
                choking_us: true,
                interesting: false,
                sampled: (0, 0, now),
                rates: (0.0, 0.0),
            },
        );
        inner.known.insert(address);
        Some(key)
    }

    /// Whether a connected peer is at `address`, or listens there.
    pub fn is_connected_to(&self, address: SocketAddr) -> bool {
        self.lock().peers.values().any(|peer| {
            peer.address == address || peer.pex.is_some_and(|(listen, _)| listen == address)
        })
    }

    /// Records `key`'s peer id. When we are already connected to that
    /// peer, one connection must go, and both sides must pick the same
    /// one: as in libtorrent, the connection started by the side with the
    /// greater peer id stays. Returns false if `key` is the one to close;
    /// otherwise an older duplicate is evicted.
    pub fn claim_peer_id(
        &self,
        key: PeerKey,
        theirs: [u8; 20],
        ours: [u8; 20],
        outgoing: bool,
    ) -> bool {
        let mut inner = self.lock();
        let duplicate = inner
            .peers
            .iter()
            .find(|(other, peer)| **other != key && peer.peer_id == Some(theirs) && !peer.evicted)
            .map(|(other, _)| *other);
        // The side with the greater id initiates the connection that stays.
        let keep_new = outgoing == (ours > theirs);
        if let Some(other) = duplicate {
            if !keep_new {
                return false;
            }
            if let Some(peer) = inner.peers.get_mut(&other) {
                peer.evicted = true;
            }
        }
        if let Some(peer) = inner.peers.get_mut(&key) {
            peer.peer_id = Some(theirs);
        }
        true
    }

    /// Whether a duplicate connection replaced `key`.
    pub fn is_evicted(&self, key: PeerKey) -> bool {
        self.lock().peers.get(&key).is_some_and(|peer| peer.evicted)
    }

    pub fn leave(&self, key: PeerKey) {
        let mut inner = self.lock();
        if inner.peers.remove(&key).is_some_and(|peer| peer.unchoked) {
            self.count_unchoked(false);
        }
        if inner.optimistic == Some(key) {
            inner.optimistic = None;
        }
    }

    pub fn is_banned(&self, address: SocketAddr) -> bool {
        self.lock().banned.contains(&identity(address))
    }

    /// The peer became interested in our pieces or lost interest. A peer
    /// that becomes interested while a slot is free, this torrent's and
    /// (when the event loop chokes) a global one, is unchoked at once
    /// instead of waiting for the next rechoke, as libtorrent's
    /// `preemptive_unchoke`.
    pub fn set_interested(&self, key: PeerKey, interested: bool) {
        let slots = self.slots();
        let room = self.managed.get().is_none_or(|slots| slots.has_room());
        let mut inner = self.lock();
        let unchoked = inner.peers.values().filter(|peer| peer.unchoked).count();
        let Some(peer) = inner.peers.get_mut(&key) else {
            return;
        };
        peer.interested = interested;
        let was = peer.unchoked;
        if !interested {
            peer.unchoked = false;
        } else if unchoked < slots && room {
            peer.unchoke(Instant::now());
        }
        if peer.unchoked != was {
            self.count_unchoked(peer.unchoked);
        }
    }

    /// Whether we let `key` download from us. A swarm on its own rechokes
    /// here when it is due; one the event loop manages, there.
    pub fn is_unchoked(&self, key: PeerKey, now: Instant) -> bool {
        let mut inner = self.lock();
        if self.managed.get().is_none()
            && inner
                .last_rechoke
                .is_none_or(|last| now - last >= RECHOKE_INTERVAL)
        {
            let policy = Policy {
                global: None,
                per_torrent: self.upload_slots,
                choking: crate::SeedChoking::default(),
                rate_based: false,
            };
            let interval = inner
                .last_rechoke
                .map_or(RECHOKE_INTERVAL, |last| now.saturating_duration_since(last));
            let candidates = inner.candidates(0, Facts::default(), now);
            let rotate = inner
                .last_optimistic
                .is_none_or(|last| now - last >= OPTIMISTIC_INTERVAL);
            if rotate {
                inner.last_optimistic = Some(now);
            }
            let previous: Vec<Slot> = inner.optimistic.map(|key| (0, key)).into_iter().collect();
            let mut random = inner.random;
            let chosen = choose(
                candidates,
                &policy,
                interval,
                &previous,
                rotate,
                &mut random,
            );
            inner.random = random;
            inner.apply(0, &chosen, now);
        }
        inner.peers.get(&key).is_some_and(|peer| peer.unchoked)
    }

    pub fn record_download(&self, key: PeerKey, bytes: u64) {
        let mut inner = self.lock();
        let Some(peer) = inner.peers.get_mut(&key) else {
            return;
        };
        peer.downloaded += bytes;
        let address = peer.address;
        inner.history.entry(address).or_default().received += bytes;
    }

    /// Records how a connection is carried.
    pub fn describe(
        &self,
        key: PeerKey,
        outgoing: bool,
        utp: bool,
        encrypted: bool,
        obfuscated: bool,
    ) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            (peer.outgoing, peer.utp, peer.encrypted, peer.obfuscated) =
                (outgoing, utp, encrypted, obfuscated);
        }
    }

    /// Records that a peer supports the hole-punch extension.
    pub fn set_holepunch(&self, key: PeerKey) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            peer.holepunch = true;
        }
    }

    /// Introduces the peer `from` to the one at `target` (BEP 55): both
    /// get a `connect` naming the other, sent on their connections' next
    /// ticks. Fails with the BEP's error code when `target` is not a peer
    /// of this torrent that supports it, or is `from` itself; a local peer
    /// and a public one are not introduced at all, as libtorrent does not.
    pub fn introduce(&self, from: PeerKey, target: SocketAddr) -> Result<(), u32> {
        use crate::holepunch::{Kind, Message};
        let mut inner = self.lock();
        let reachable = |peer: &Peer| {
            // uTP runs on the listen port, so its address is the one to
            // punch to; over TCP the peer's listen address, when known.
            if peer.utp {
                peer.address
            } else {
                peer.pex.map_or(peer.address, |(address, _)| address)
            }
        };
        let Some(asker) = inner.peers.get(&from).map(reachable) else {
            return Err(crate::holepunch::NOT_CONNECTED);
        };
        let Some((key, peer)) = inner.peers.iter().find(|(_, peer)| {
            !peer.evicted
                && (peer.address == target
                    || peer.pex.is_some_and(|(address, _)| address == target))
        }) else {
            return Err(crate::holepunch::NOT_CONNECTED);
        };
        let (key, supported) = (*key, peer.holepunch);
        if key == from {
            return Err(crate::holepunch::NO_SELF);
        }
        if !supported {
            return Err(crate::holepunch::NO_SUPPORT);
        }
        if crate::dht_id::is_local(target.ip()) != crate::dht_id::is_local(asker.ip()) {
            return Ok(());
        }
        let mut send = |to: PeerKey, about: SocketAddr| {
            if let Some(peer) = inner.peers.get_mut(&to)
                && peer.holepunch_outbox.len() < MAX_HOLEPUNCH_QUEUED
            {
                peer.holepunch_outbox
                    .push(Message::new(Kind::Connect, about, 0));
            }
        };
        send(from, target);
        send(key, asker);
        Ok(())
    }

    /// Takes the hole-punch messages waiting for a peer.
    pub fn take_holepunch(&self, key: PeerKey) -> Vec<crate::holepunch::Message> {
        self.lock()
            .peers
            .get_mut(&key)
            .map(|peer| std::mem::take(&mut peer.holepunch_outbox))
            .unwrap_or_default()
    }

    /// Records whether a peer stopped sending the blocks we asked for.
    pub fn set_snubbed(&self, key: PeerKey, snubbed: bool) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            peer.snubbed = snubbed;
        }
    }

    /// Records the client a peer named.
    pub fn set_client(&self, key: PeerKey, client: Option<String>) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            peer.client = client;
        }
    }

    /// Records how many of `total` pieces a peer has.
    pub fn set_pieces(&self, key: PeerKey, have: usize, total: usize) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            peer.pieces = Some((have.min(total), total));
        }
    }

    /// Records whether a peer lets us download and whether we want to.
    pub fn set_download_state(&self, key: PeerKey, choking_us: bool, interesting: bool) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            (peer.choking_us, peer.interesting) = (choking_us, interesting);
        }
    }

    /// Every connected peer, oldest connection first.
    pub fn peers(&self) -> Vec<PeerInfo> {
        let now = Instant::now();
        let mut inner = self.lock();
        let optimistic = inner.optimistic;
        let Inner { peers, sources, .. } = &mut *inner;
        let mut peers: Vec<(&PeerKey, &mut Peer)> =
            peers.iter_mut().filter(|(_, peer)| !peer.evicted).collect();
        peers.sort_by_key(|(_, peer)| peer.joined);
        peers
            .into_iter()
            .map(|(key, peer)| {
                let (down, up, at) = peer.sampled;
                let elapsed = now.saturating_duration_since(at);
                if elapsed >= RATE_WINDOW {
                    let seconds = elapsed.as_secs_f64();
                    peer.rates = (
                        peer.downloaded.saturating_sub(down) as f64 / seconds,
                        peer.uploaded.saturating_sub(up) as f64 / seconds,
                    );
                    peer.sampled = (peer.downloaded, peer.uploaded, now);
                }
                let client = peer
                    .client
                    .clone()
                    .or_else(|| peer.peer_id.as_ref().and_then(crate::identity::client_name));
                PeerInfo {
                    address: peer.address,
                    client,
                    outgoing: peer.outgoing,
                    utp: peer.utp,
                    encrypted: peer.encrypted,
                    handshake_encrypted: peer.obfuscated,
                    optimistic: optimistic == Some(*key),
                    snubbed: peer.snubbed,
                    holepunched: peer.holepunched,
                    sources: if peer.outgoing {
                        sources.get(&peer.address).copied().unwrap_or(0)
                    } else {
                        0
                    },
                    progress: peer.pieces.map(|(have, total)| {
                        if total == 0 {
                            0.0
                        } else {
                            have as f64 / total as f64
                        }
                    }),
                    down_rate: peer.rates.0,
                    up_rate: peer.rates.1,
                    downloaded: peer.downloaded,
                    uploaded: peer.uploaded,
                    interested: peer.interested,
                    unchoked: peer.unchoked,
                    interesting: peer.interesting,
                    choking_us: peer.choking_us,
                }
            })
            .collect()
    }

    pub fn record_upload(&self, key: PeerKey, bytes: u64) {
        if let Some(peer) = self.lock().peers.get_mut(&key) {
            peer.uploaded += bytes;
        }
    }

    /// A connection to `address` failed before it sent anything: it is not
    /// dialed again for a minute per failure in a row, half an hour after
    /// three, so a peer that refuses us is not asked every round.
    pub fn connect_failed(&self, address: SocketAddr) {
        self.connect_failed_at(address, Instant::now());
    }

    fn connect_failed_at(&self, address: SocketAddr, now: Instant) {
        let mut inner = self.lock();
        let history = inner.history.entry(address).or_default();
        history.failures += 1;
        let wait = if history.failures >= MAX_FAILURES {
            GIVE_UP_DELAY
        } else {
            RECONNECT_DELAY * history.failures
        };
        history.retry_at = Some(now + wait);
        // Perhaps it is behind a NAT: the peer that told us of it can
        // introduce us, so both sides connect at once (BEP 55), as
        // libtorrent asks after a failed connection.
        let Some(introducer) = inner.introducers.remove(&address) else {
            return;
        };
        if let Some(peer) = inner
            .peers
            .values_mut()
            .find(|peer| peer.address == introducer && peer.holepunch && !peer.evicted)
            && peer.holepunch_outbox.len() < MAX_HOLEPUNCH_QUEUED
        {
            peer.holepunch_outbox.push(crate::holepunch::Message::new(
                crate::holepunch::Kind::Rendezvous,
                address,
                0,
            ));
        }
    }

    /// Asks for a uTP connection to `address` now, as an introducer said
    /// (BEP 55): the peer there is connecting to us at the same moment.
    /// Peers already connected, banned ones, and a full queue are skipped.
    pub fn queue_holepunch_dial(&self, address: SocketAddr) {
        let connected = self.is_connected_to(address);
        let mut inner = self.lock();
        if connected
            || inner.banned.contains(&identity(address))
            || inner.holepunch_dials.contains(&address)
            || inner.holepunch_dials.len() >= MAX_HOLEPUNCH_QUEUED
        {
            return;
        }
        inner.holepunch_dials.push(address);
    }

    /// Takes the addresses to punch a hole to.
    pub fn take_holepunch_dials(&self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.lock().holepunch_dials)
    }

    /// Records that the connection to `address` came through a punched
    /// hole.
    pub fn mark_holepunched(&self, address: SocketAddr) {
        if let Some(peer) = self
            .lock()
            .peers
            .values_mut()
            .find(|peer| peer.address == address && !peer.evicted)
        {
            peer.holepunched = true;
        }
    }

    /// Announces a verified piece to every connection.
    pub fn add_have(&self, index: u32) {
        self.lock().haves.push(index);
    }

    /// Pieces verified since `cursor`, which it then moves past them.
    pub fn haves_since(&self, cursor: &mut usize) -> Vec<u32> {
        let inner = self.lock();
        let new = inner.haves.get(*cursor..).unwrap_or_default().to_vec();
        *cursor = inner.haves.len();
        new
    }

    pub fn have_count(&self) -> usize {
        self.lock().haves.len()
    }

    /// A good piece from `address` restores some trust.
    pub fn piece_verified(&self, address: SocketAddr) {
        let mut inner = self.lock();
        let trust = inner.trust.entry(identity(address)).or_insert(0);
        *trust = (*trust + 1).min(TRUST_MAX);
    }

    /// A corrupt piece from `address` costs trust. Returns whether that
    /// banned the address.
    pub fn piece_corrupt(&self, address: SocketAddr) -> bool {
        let mut inner = self.lock();
        let identity = identity(address);
        let trust = inner.trust.entry(identity).or_insert(0);
        *trust -= CORRUPT_PENALTY;
        if *trust <= TRUST_BAN && !inner.banned.contains(&identity) {
            inner.banned.push(identity);
        }
        inner.banned.contains(&identity)
    }

    /// Orders candidate addresses best first, without banned ones or
    /// those waiting after failed connections: those that sent us the
    /// most data, then untried ones, then those that failed most often.
    /// Ties go to the higher canonical peer priority (BEP 40), once our
    /// public address is known.
    pub fn rank(&self, addresses: &[SocketAddr]) -> Vec<SocketAddr> {
        self.rank_at(addresses, Instant::now())
    }

    fn rank_at(&self, addresses: &[SocketAddr], now: Instant) -> Vec<SocketAddr> {
        let inner = self.lock();
        let us = inner
            .external_ip()
            .map(|ip| SocketAddr::new(ip, inner.listen_port.unwrap_or(0)));
        let mut ranked: Vec<(SocketAddr, History, u32)> = addresses
            .iter()
            .filter(|address| !inner.banned.contains(&identity(**address)))
            .filter(|address| {
                inner
                    .history
                    .get(address)
                    .and_then(|history| history.retry_at)
                    .is_none_or(|retry_at| now >= retry_at)
            })
            .map(|address| {
                let priority = us
                    .and_then(|us| crate::priority::peer_priority(us, *address))
                    .unwrap_or(0);
                (
                    *address,
                    inner.history.get(address).copied().unwrap_or_default(),
                    priority,
                )
            })
            .collect();
        ranked.sort_by(|(_, a, a_priority), (_, b, b_priority)| {
            b.received
                .cmp(&a.received)
                .then(a.failures.cmp(&b.failures))
                .then(b_priority.cmp(a_priority))
        });
        ranked.into_iter().map(|(address, ..)| address).collect()
    }

    /// The port peers can connect to us on.
    pub fn set_listen_port(&self, port: u16) {
        self.lock().listen_port = Some(port);
    }

    pub fn listen_port(&self) -> Option<u16> {
        self.lock().listen_port
    }

    /// `voter` reports seeing us at `ip`.
    pub fn vote_external(&self, voter: SocketAddr, ip: IpAddr) {
        self.lock().votes.entry(ip).or_default().insert(voter.ip());
    }

    /// Our public address: the one most peers report.
    pub fn external_ip(&self) -> Option<IpAddr> {
        self.lock().external_ip()
    }

    /// Records the address other peers can reach `key` on, for exchange.
    pub fn set_pex(&self, key: PeerKey, address: SocketAddr, flags: u8) {
        let mut inner = self.lock();
        inner.known.insert(address);
        if let Some(peer) = inner.peers.get_mut(&key) {
            peer.pex = Some((address, flags));
        }
    }

    /// Sets or clears flags for `key`'s exchanged address.
    pub fn update_pex_flags(&self, key: PeerKey, set: u8, clear: u8) {
        if let Some((_, flags)) = self
            .lock()
            .peers
            .get_mut(&key)
            .and_then(|peer| peer.pex.as_mut())
        {
            *flags = (*flags | set) & !clear;
        }
    }

    /// The connected peers to tell `except` about.
    pub fn pex_contacts(&self, except: PeerKey) -> Vec<(SocketAddr, u8)> {
        let inner = self.lock();
        let mut seen = HashSet::new();
        inner
            .peers
            .iter()
            .filter(|(key, _)| **key != except)
            .filter_map(|(_, peer)| peer.pex)
            .filter(|(address, _)| seen.insert(*address))
            .collect()
    }

    /// Notes addresses learned elsewhere, so exchanges do not repeat them.
    pub fn note_known(&self, addresses: &[SocketAddr]) {
        self.lock().known.extend(addresses.iter().copied());
    }

    /// Takes in contacts from `source`'s peer exchange. Returns how many
    /// were new. Banned addresses, ones already known, repeats of an IP
    /// within the message, our own address, and anything beyond the
    /// per-source limit are skipped.
    pub fn discover(&self, source: SocketAddr, contacts: &[(SocketAddr, u8)]) -> usize {
        let mut inner = self.lock();
        let us = inner.external_ip().zip(inner.listen_port);
        let introducer = source;
        let source = identity(source);
        // Who can introduce us to the contacts that can be hole-punched.
        for (address, flags) in contacts {
            if flags & crate::pex::SUPPORTS_HOLEPUNCH != 0
                && (inner.introducers.len() < MAX_DISCOVERED
                    || inner.introducers.contains_key(address))
            {
                inner.introducers.insert(*address, introducer);
            }
        }
        let mut ips = HashSet::new();
        let mut added = 0;
        for (address, _) in contacts {
            let from_source = inner.from_source.get(&source).copied().unwrap_or(0);
            if from_source >= MAX_FROM_ONE_SOURCE || inner.known.len() >= MAX_DISCOVERED {
                break;
            }
            if !ips.insert(address.ip())
                || inner.banned.contains(&identity(*address))
                || us == Some((address.ip(), address.port()))
            {
                continue;
            }
            // Known from elsewhere, it is still a peer exchange contact.
            if inner.sources.len() < MAX_DISCOVERED || inner.sources.contains_key(address) {
                *inner.sources.entry(*address).or_default() |= SOURCE_PEX;
            }
            if !inner.known.insert(*address) {
                continue;
            }
            inner.discovered.push_back(*address);
            inner.exchanged.push(*address);
            *inner.from_source.entry(source).or_default() += 1;
            added += 1;
        }
        added
    }

    /// Queues addresses from trackers, the DHT, or local discovery to try,
    /// remembering where they came from (`SOURCE_*` flags) for peer lists.
    /// These come from sources we chose, so unlike peer exchange there is
    /// no per-source limit. Returns how many were new.
    pub fn offer_from(&self, addresses: &[SocketAddr], source: u8) -> usize {
        let mut inner = self.lock();
        let mut added = 0;
        for address in addresses {
            if inner.discovered.len() >= MAX_OFFERED {
                break;
            }
            if source != 0
                && (inner.sources.len() < MAX_DISCOVERED || inner.sources.contains_key(address))
            {
                *inner.sources.entry(*address).or_default() |= source;
            }
            if inner.banned.contains(&identity(*address)) || !inner.known.insert(*address) {
                continue;
            }
            inner.discovered.push_back(*address);
            added += 1;
        }
        added
    }

    /// Whether addresses are waiting to be tried.
    pub fn has_pending(&self) -> bool {
        !self.lock().discovered.is_empty()
    }

    /// Addresses from peer exchange not yet tried, best first.
    pub fn take_discovered(&self) -> Vec<SocketAddr> {
        let discovered: Vec<SocketAddr> = self.lock().discovered.drain(..).collect();
        self.rank(&discovered)
    }

    /// Every address learned through peer exchange.
    pub fn exchanged(&self) -> Vec<SocketAddr> {
        self.lock().exchanged.clone()
    }
}

impl Inner {
    fn external_ip(&self) -> Option<IpAddr> {
        self.votes
            .iter()
            .max_by_key(|(ip, voters)| (voters.len(), **ip))
            .map(|(ip, _)| *ip)
    }

    /// This swarm's interested peers as choking candidates for torrent
    /// `torrent`, rolling each peer's rates over to a new interval.
    fn candidates(&mut self, torrent: usize, facts: Facts, now: Instant) -> Vec<Candidate> {
        self.last_rechoke = Some(now);
        for peer in self.peers.values_mut() {
            peer.rate = peer.downloaded - peer.downloaded_before;
            peer.downloaded_before = peer.downloaded;
            peer.uploaded_round = peer.uploaded - peer.uploaded_before;
            peer.uploaded_before = peer.uploaded;
        }
        self.peers
            .iter()
            .filter(|(_, peer)| peer.interested && !peer.evicted)
            .map(|(key, peer)| Candidate {
                slot: (torrent, *key),
                rate: peer.rate,
                rank: facts.rank,
                uploaded_round: peer.uploaded_round,
                unchoked: peer.unchoked,
                quota_done: peer.unchoked
                    && facts.piece_length > 0
                    && peer.uploaded - peer.uploaded_at_unchoke
                        > facts.piece_length * SEEDING_PIECE_QUOTA
                    && now.saturating_duration_since(peer.last_unchoke) > QUOTA_TIME,
                leech_score: anti_leech_score(peer, facts),
                last_unchoke: peer.last_unchoke,
                new: now.saturating_duration_since(peer.joined) < NEW_PEER,
            })
            .collect()
    }

    /// Unchokes the peers `chosen` names for torrent `torrent` and chokes
    /// the rest. Returns how many changed each way.
    fn apply(&mut self, torrent: usize, chosen: &Chosen, now: Instant) -> (usize, usize) {
        self.optimistic = chosen
            .optimistic
            .iter()
            .find(|(owner, _)| *owner == torrent)
            .map(|(_, key)| *key);
        let (mut unchoked, mut choked) = (0, 0);
        for (key, peer) in &mut self.peers {
            let want = chosen.contains(&(torrent, *key));
            if want != peer.unchoked {
                if want {
                    peer.unchoke(now);
                } else {
                    peer.unchoked = false;
                }
                if want {
                    unchoked += 1;
                } else {
                    choked += 1;
                }
            }
        }
        (unchoked, choked)
    }
}

impl Peer {
    fn unchoke(&mut self, now: Instant) {
        if !self.unchoked {
            self.unchoked = true;
            self.last_unchoke = now;
            self.uploaded_at_unchoke = self.uploaded;
        }
    }
}

/// Round-robin: an unchoked peer has had its turn once sent this many
/// pieces and unchoked this long (libtorrent's `seeding_piece_quota`).
const SEEDING_PIECE_QUOTA: u64 = 20;
const QUOTA_TIME: Duration = Duration::from_secs(60);
/// Rate-based slots: a slot for each peer uploaded to faster than this,
/// rising by the step per slot (libtorrent's
/// `rate_choker_initial_threshold`).
const RATE_THRESHOLD: u64 = 1024;
const RATE_STEP: u64 = 2048;

/// What the choker needs to know of a torrent.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Facts {
    /// Its seed rank: how much its swarm needs us.
    pub(crate) rank: u32,
    pub(crate) piece_length: u64,
    pub(crate) total_size: u64,
}

impl Facts {
    pub(crate) fn of(torrent: &crate::engine::active::ActiveTorrent) -> Self {
        Self {
            rank: torrent.seed_rank(),
            piece_length: u64::from(torrent.info.piece_length()),
            total_size: torrent.info.total_length(),
        }
    }
}

/// How far a peer is from half done, 0 (half) to 1000 (nothing or all), as
/// libtorrent's `anti_leech_score`: what it has is what it said, or what we
/// sent it, counted up to half.
fn anti_leech_score(peer: &Peer, facts: Facts) -> u32 {
    let given = if facts.total_size > 0 {
        peer.uploaded.min(facts.total_size / 2) as f64 / facts.total_size as f64
    } else {
        0.0
    };
    let have = peer
        .pieces
        .filter(|(_, total)| *total > 0)
        .map_or(0.0, |(have, total)| have as f64 / total as f64);
    let share = given.max(have).min(1.0);
    ((share - 0.5).abs() * 2000.0) as u32
}

/// A peer in the choker's eyes: its torrent (any number unique among the
/// torrents choked together) and its key in that torrent's swarm.
type Slot = (usize, PeerKey);

/// An interested peer the choker may unchoke.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    slot: Slot,
    /// Bytes it sent us in the last interval.
    rate: u64,
    /// Its torrent's seed rank: how much the torrent's swarm needs us.
    rank: u32,
    /// Bytes we sent it in the last interval.
    uploaded_round: u64,
    unchoked: bool,
    /// Round-robin: it has had its turn.
    quota_done: bool,
    /// Anti-leech: how far from half done it is.
    leech_score: u32,
    last_unchoke: Instant,
    /// It connected recently, so it is likelier to get an optimistic slot.
    new: bool,
}

/// The choking settings a rechoke follows.
#[derive(Debug, Clone, Copy)]
struct Policy {
    /// Upload slots across all torrents; `None` for unlimited.
    global: Option<usize>,
    per_torrent: usize,
    choking: crate::SeedChoking,
    /// The global slots follow the upload rate instead.
    rate_based: bool,
}

/// Upload slots as the upload rate fills them, as libtorrent's
/// `rate_based_choker`: one for each peer uploaded to faster than a
/// threshold that rises with each, plus one.
fn rate_based_slots(candidates: &[Candidate], interval: Duration) -> usize {
    let millis = interval.as_millis().max(1) as u64;
    let mut rates: Vec<u64> = candidates
        .iter()
        .map(|candidate| candidate.uploaded_round.saturating_mul(1000) / millis)
        .collect();
    rates.sort_unstable_by(|a, b| b.cmp(a));
    let mut threshold = RATE_THRESHOLD;
    let mut slots = 0;
    for rate in rates {
        if rate < threshold {
            break;
        }
        slots += 1;
        threshold += RATE_STEP;
    }
    slots + 1
}

/// The order peers get regular slots in: those that sent us most, then
/// those of the torrent with the highest seed rank, then as `choking`
/// says, then the one unchoked longest ago (libtorrent's `unchoke_compare_*`).
fn unchoke_order(a: &Candidate, b: &Candidate, choking: crate::SeedChoking) -> std::cmp::Ordering {
    use crate::SeedChoking;
    let by_choking = match choking {
        SeedChoking::FastestUpload => b.uploaded_round.cmp(&a.uploaded_round),
        SeedChoking::RoundRobin => {
            let round = |candidate: &Candidate| {
                if candidate.unchoked {
                    candidate.uploaded_round
                } else {
                    0
                }
            };
            a.quota_done
                .cmp(&b.quota_done)
                .then(round(b).cmp(&round(a)))
        }
        SeedChoking::AntiLeech => b.leech_score.cmp(&a.leech_score),
    };
    b.rate
        .cmp(&a.rate)
        .then(b.rank.cmp(&a.rank))
        .then(by_choking)
        .then(a.last_unchoke.cmp(&b.last_unchoke))
        .then(a.slot.0.cmp(&b.slot.0))
        .then(a.slot.1.0.cmp(&b.slot.1.0))
}

/// The peers the choker unchokes.
#[derive(Debug, Default)]
struct Chosen {
    regular: HashSet<Slot>,
    optimistic: Vec<Slot>,
}

impl Chosen {
    fn contains(&self, slot: &Slot) -> bool {
        self.regular.contains(slot) || self.optimistic.contains(slot)
    }
}

/// Picks the peers to upload to, as libtorrent's session choker does
/// (`recalculate_unchoke_slots`, `recalculate_optimistic_unchoke_slots`).
///
/// Regular slots go in [`unchoke_order`]. A torrent takes at most
/// `per_torrent - 1` of them, keeping one for an optimistic unchoke. With a
/// `global` limit (or rate-based slots, counted over the last `interval`),
/// a fifth of it (at least one) is optimistic, the rest regular; without,
/// each torrent has one optimistic slot. Optimistic slots stay with their
/// peers until `rotate`, then go to new random picks, newcomers three times
/// as likely.
fn choose(
    mut candidates: Vec<Candidate>,
    policy: &Policy,
    interval: Duration,
    previous: &[Slot],
    rotate: bool,
    random: &mut u64,
) -> Chosen {
    let per_torrent = policy.per_torrent.max(1);
    let global = if policy.rate_based {
        Some(rate_based_slots(&candidates, interval))
    } else {
        policy.global
    };
    let optimistic_total = match global {
        None => usize::MAX,
        Some(global) if global > 1 => (global / 5).max(1),
        Some(_) => 0,
    };
    // One slot of each torrent's is for an optimistic unchoke, unless it
    // has only one.
    let (regular_per_torrent, optimistic_per_torrent) = if per_torrent > 1 {
        (per_torrent - 1, 1)
    } else {
        (1, 0)
    };
    let mut regular_left = global.map_or(usize::MAX, |global| global - optimistic_total);
    candidates.sort_by(|a, b| unchoke_order(a, b, policy.choking));
    let mut chosen = Chosen::default();
    let mut regular_count: HashMap<usize, usize> = HashMap::new();
    for candidate in &candidates {
        if regular_left == 0 {
            break;
        }
        let count = regular_count.entry(candidate.slot.0).or_default();
        if *count < regular_per_torrent {
            *count += 1;
            regular_left -= 1;
            chosen.regular.insert(candidate.slot);
        }
    }
    let mut optimistic_count: HashMap<usize, usize> = HashMap::new();
    let eligible = |slot: &Slot, optimistic_count: &HashMap<usize, usize>| {
        !chosen.regular.contains(slot)
            && optimistic_count.get(&slot.0).copied().unwrap_or(0) < optimistic_per_torrent
    };
    if !rotate {
        for slot in previous {
            if chosen.optimistic.len() < optimistic_total
                && candidates.iter().any(|candidate| candidate.slot == *slot)
                && eligible(slot, &optimistic_count)
            {
                *optimistic_count.entry(slot.0).or_default() += 1;
                chosen.optimistic.push(*slot);
            }
        }
    }
    while chosen.optimistic.len() < optimistic_total {
        let weighted: Vec<(Slot, u64)> = candidates
            .iter()
            .filter(|candidate| eligible(&candidate.slot, &optimistic_count))
            .map(|candidate| (candidate.slot, if candidate.new { 3 } else { 1 }))
            .collect();
        let total: u64 = weighted.iter().map(|(_, weight)| weight).sum();
        if total == 0 {
            break;
        }
        let mut pick = next_random(random) % total;
        let Some(slot) = weighted.into_iter().find_map(|(slot, weight)| {
            if pick < weight {
                Some(slot)
            } else {
                pick -= weight;
                None
            }
        }) else {
            break;
        };
        *optimistic_count.entry(slot.0).or_default() += 1;
        chosen.optimistic.push(slot);
    }
    chosen
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// The event loop's choking state across rechokes.
#[derive(Debug)]
pub(crate) struct Choker {
    last_rechoke: Option<Instant>,
    last_optimistic: Option<Instant>,
    optimistic: Vec<Slot>,
    random: u64,
}

impl Default for Choker {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_nanos() as u64)
            | 1;
        Self {
            last_rechoke: None,
            last_optimistic: None,
            optimistic: Vec::new(),
            random: seed,
        }
    }
}

impl Choker {
    /// Whether a rechoke is due.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.last_rechoke
            .is_none_or(|last| now.saturating_duration_since(last) >= RECHOKE_INTERVAL)
    }

    /// Decides who every torrent uploads to, for `torrents` (each swarm
    /// with its torrent's seed rank), under the shared `slots`. Every swarm
    /// is handed to the event loop's choking ([`Swarm::manage`]) first.
    pub(crate) fn rechoke_all(
        &mut self,
        torrents: &[(&Swarm, Facts)],
        slots: &std::sync::Arc<crate::limits::UploadSlots>,
        now: Instant,
    ) {
        let interval = self
            .last_rechoke
            .map_or(RECHOKE_INTERVAL, |last| now.saturating_duration_since(last));
        self.last_rechoke = Some(now);
        let mut candidates = Vec::new();
        for (swarm, facts) in torrents {
            swarm.manage(slots);
            candidates.extend(swarm.lock().candidates(Self::id(swarm), *facts, now));
        }
        let rotate = self
            .last_optimistic
            .is_none_or(|last| now.saturating_duration_since(last) >= OPTIMISTIC_INTERVAL);
        if rotate {
            self.last_optimistic = Some(now);
        }
        let policy = Policy {
            global: slots.global(),
            per_torrent: slots.per_torrent(),
            choking: slots.choking(),
            rate_based: slots.mode() == crate::UploadSlotsMode::RateBased,
        };
        let chosen = choose(
            candidates,
            &policy,
            interval,
            &self.optimistic,
            rotate,
            &mut self.random,
        );
        self.optimistic.clone_from(&chosen.optimistic);
        // A recount, so a count that drifted (a swarm dropped with peers
        // unchoked) is put right.
        let mut unchoked = 0;
        for (swarm, _) in torrents {
            let mut inner = swarm.lock();
            inner.apply(Self::id(swarm), &chosen, now);
            unchoked += inner.peers.values().filter(|peer| peer.unchoked).count();
        }
        slots.unchoked.store(unchoked, Ordering::Relaxed);
    }

    /// A torrent's id across rechokes: its swarm's address, which is
    /// stable while the swarm lives.
    fn id(swarm: &Swarm) -> usize {
        std::ptr::from_ref(swarm) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], 6881))
    }

    fn info() -> PeerInfo {
        PeerInfo {
            address: address(1),
            client: None,
            outgoing: true,
            utp: false,
            encrypted: false,
            handshake_encrypted: false,
            optimistic: false,
            snubbed: false,
            sources: 0,
            holepunched: false,
            progress: None,
            down_rate: 0.0,
            up_rate: 0.0,
            downloaded: 0,
            uploaded: 0,
            interested: false,
            unchoked: false,
            interesting: false,
            choking_us: true,
        }
    }

    fn letters(info: &PeerInfo) -> String {
        info.flags()
            .iter()
            .map(|(flag, _)| flag.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn shows_flags_with_qbittorrents_letters_in_its_order() {
        // Downloading from a DHT peer we dialed over uTP, with only the
        // handshake encrypted; it wants data but is choked.
        let downloading = PeerInfo {
            interesting: true,
            choking_us: false,
            interested: true,
            utp: true,
            handshake_encrypted: true,
            sources: SOURCE_DHT | SOURCE_TRACKER,
            ..info()
        };
        assert_eq!(letters(&downloading), "D u H e P");
        // An incoming leecher we upload to on the optimistic slot, snubbed,
        // fully encrypted.
        let uploading = PeerInfo {
            outgoing: false,
            interested: true,
            unchoked: true,
            optimistic: true,
            snubbed: true,
            encrypted: true,
            handshake_encrypted: true,
            ..info()
        };
        assert_eq!(letters(&uploading), "U O S I E");
        // Choking us while we want data; unchoked by us though it wants
        // nothing; found by peer exchange and local discovery.
        let idle = PeerInfo {
            interesting: true,
            unchoked: true,
            sources: SOURCE_PEX | SOURCE_LSD,
            ..info()
        };
        assert_eq!(letters(&idle), "d ? X L");
        assert_eq!(
            letters(&PeerInfo {
                choking_us: false,
                ..info()
            }),
            "K"
        );
        assert!(downloading.flags().contains(&('H', "Peer from DHT")));
    }

    #[test]
    fn introduces_only_peers_that_support_it() {
        use crate::holepunch::{Kind, NO_SELF, NO_SUPPORT, NOT_CONNECTED};
        let swarm = Swarm::new(4);
        let now = Instant::now();
        let public = |last: u8| SocketAddr::from(([203, 0, 113, last], 6881));
        let asker = swarm.join(public(1), now).unwrap();
        let target = swarm.join(public(2), now).unwrap();
        let plain = swarm.join(public(3), now).unwrap();
        let local = swarm
            .join(SocketAddr::from(([192, 168, 1, 4], 6881)), now)
            .unwrap();
        for key in [asker, target, local] {
            swarm.set_holepunch(key);
        }
        // Over TCP, the asker is introduced by its listen address.
        swarm.set_pex(asker, public(11), 0);
        assert_eq!(swarm.introduce(asker, public(2)), Ok(()));
        let sent = swarm.take_holepunch(asker);
        assert_eq!((sent[0].kind, sent[0].address), (Kind::Connect, public(2)));
        assert_eq!(swarm.take_holepunch(target)[0].address, public(11));
        assert!(swarm.take_holepunch(asker).is_empty(), "taken once");
        assert_eq!(swarm.introduce(asker, public(3)), Err(NO_SUPPORT));
        assert_eq!(swarm.introduce(asker, public(9)), Err(NOT_CONNECTED));
        assert_eq!(swarm.introduce(asker, public(11)), Err(NO_SELF));
        // A public peer is not introduced to a local one.
        assert_eq!(
            swarm.introduce(asker, SocketAddr::from(([192, 168, 1, 4], 6881))),
            Ok(())
        );
        assert!(swarm.take_holepunch(local).is_empty());
        let _ = plain;
    }

    #[test]
    fn asks_the_peer_that_named_an_unreachable_one_to_introduce_us() {
        use crate::holepunch::Kind;
        use crate::pex::SUPPORTS_HOLEPUNCH;
        let swarm = Swarm::new(4);
        let now = Instant::now();
        let public = |last: u8| SocketAddr::from(([203, 0, 113, last], 6881));
        let introducer = swarm.join(public(1), now).unwrap();
        swarm.set_holepunch(introducer);
        let silent = swarm.join(public(2), now).unwrap();
        // Peer exchange from each: one contact that can be hole-punched,
        // one that cannot.
        swarm.discover(
            public(1),
            &[(public(10), SUPPORTS_HOLEPUNCH), (public(11), 0)],
        );
        swarm.discover(public(2), &[(public(12), SUPPORTS_HOLEPUNCH)]);
        swarm.connect_failed(public(10));
        let asked = swarm.take_holepunch(introducer);
        assert_eq!(asked.len(), 1);
        assert_eq!(
            (asked[0].kind, asked[0].address),
            (Kind::Rendezvous, public(10))
        );
        // Once only, and only for contacts flagged by a peer that can relay.
        swarm.connect_failed(public(10));
        swarm.connect_failed(public(11));
        swarm.connect_failed(public(12));
        assert!(swarm.take_holepunch(introducer).is_empty());
        assert!(
            swarm.take_holepunch(silent).is_empty(),
            "it never offered BEP 55"
        );

        // Told to connect: queued once, not for peers already connected.
        swarm.queue_holepunch_dial(public(20));
        swarm.queue_holepunch_dial(public(20));
        swarm.queue_holepunch_dial(public(1));
        assert_eq!(swarm.take_holepunch_dials(), [public(20)]);
        assert!(swarm.take_holepunch_dials().is_empty());
        // A punched connection shows qBittorrent's `h`.
        let punched = swarm.join(public(20), now).unwrap();
        swarm.describe(punched, true, true, false, false);
        swarm.mark_holepunched(public(20));
        let peer = swarm
            .peers()
            .into_iter()
            .find(|peer| peer.address == public(20))
            .unwrap();
        assert!(
            peer.flags()
                .contains(&('h', "Peer is using NAT hole punching"))
        );
    }

    #[test]
    fn remembers_where_dialed_peers_came_from() {
        let swarm = Swarm::new(4);
        let now = Instant::now();
        swarm.offer_from(&[address(1)], SOURCE_DHT);
        swarm.discover(address(9), &[(address(1), 0), (address(2), 0)]);
        let dialed = swarm.join(address(1), now).unwrap();
        swarm.describe(dialed, true, false, false, false);
        // A peer that connected to us has no source, as in libtorrent.
        let incoming = swarm.join(address(2), now).unwrap();
        swarm.describe(incoming, false, false, false, false);
        let peer = |at: SocketAddr| {
            swarm
                .peers()
                .into_iter()
                .find(|peer| peer.address == at)
                .unwrap()
        };
        assert_eq!(peer(address(1)).sources, SOURCE_DHT | SOURCE_PEX);
        assert_eq!(peer(address(2)).sources, 0);
        swarm.set_snubbed(dialed, true);
        assert!(peer(address(1)).snubbed);
    }

    #[test]
    fn interested_peers_get_free_slots_at_once() {
        let swarm = Swarm::new(2);
        let now = Instant::now();
        let a = swarm.join(address(1), now).unwrap();
        let b = swarm.join(address(2), now).unwrap();
        let c = swarm.join(address(3), now).unwrap();
        swarm.is_unchoked(a, now); // first rechoke, with nobody interested
        for key in [a, b, c] {
            swarm.set_interested(key, true);
        }
        assert!(swarm.is_unchoked(a, now));
        assert!(swarm.is_unchoked(b, now));
        assert!(!swarm.is_unchoked(c, now), "only two slots");
        swarm.set_interested(a, false);
        assert!(!swarm.is_unchoked(a, now));
    }

    #[test]
    fn regular_slots_go_to_the_fastest_and_one_rotates() {
        let swarm = Swarm::new(3);
        let start = Instant::now();
        let peers: Vec<PeerKey> = (1..=6)
            .map(|last| swarm.join(address(last), start).unwrap())
            .collect();
        for key in &peers {
            swarm.set_interested(*key, true);
        }
        // Peers 0 and 1 send us the most.
        swarm.record_download(peers[0], 900_000);
        swarm.record_download(peers[1], 500_000);
        swarm.record_download(peers[2], 1_000);
        let mut optimistic = std::collections::HashSet::new();
        let mut now = start;
        for round in 0..30 {
            now += RECHOKE_INTERVAL + OPTIMISTIC_INTERVAL;
            if round == 0 {
                assert!(swarm.is_unchoked(peers[0], now));
            }
            let unchoked: Vec<usize> = (0..6)
                .filter(|index| swarm.is_unchoked(peers[*index], now))
                .collect();
            assert_eq!(unchoked.len(), 3, "two regular slots and one optimistic");
            optimistic.extend(unchoked.into_iter().filter(|index| *index >= 2));
            // Keep peers 0 and 1 fastest for every interval.
            swarm.record_download(peers[0], 900_000);
            swarm.record_download(peers[1], 500_000);
        }
        assert!(swarm.is_unchoked(peers[0], now) && swarm.is_unchoked(peers[1], now));
        assert!(
            optimistic.len() >= 3,
            "the optimistic slot rotated: {optimistic:?}"
        );
    }

    fn candidate(torrent: usize, key: u64, rate: u64, rank: u32) -> Candidate {
        Candidate {
            slot: (torrent, PeerKey(key)),
            rate,
            rank,
            uploaded_round: 0,
            unchoked: false,
            quota_done: false,
            leech_score: 0,
            last_unchoke: Instant::now(),
            new: false,
        }
    }

    fn fixed(global: usize, per_torrent: usize) -> Policy {
        Policy {
            global: Some(global),
            per_torrent,
            choking: crate::SeedChoking::FastestUpload,
            rate_based: false,
        }
    }

    /// libtorrent's session choker: 20 global slots, 16 regular and a
    /// fifth (4) optimistic, at most 4 per torrent.
    #[test]
    fn shares_global_upload_slots_among_torrents() {
        let candidates: Vec<Candidate> = (0..10)
            .flat_map(|torrent| (0..10).map(move |key| candidate(torrent, key, 0, 1000)))
            .collect();
        let mut random = 7;
        let chosen = choose(
            candidates,
            &fixed(20, 4),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(chosen.regular.len(), 16);
        assert_eq!(chosen.optimistic.len(), 4);
        for torrent in 0..10 {
            let regular = chosen
                .regular
                .iter()
                .filter(|slot| slot.0 == torrent)
                .count();
            let optimistic = chosen
                .optimistic
                .iter()
                .filter(|slot| slot.0 == torrent)
                .count();
            assert!(regular <= 3 && optimistic <= 1 && regular + optimistic <= 4);
        }
    }

    /// Peers that send us data come first; among equals, those of the
    /// torrent its swarm needs most (highest seed rank).
    #[test]
    fn ranks_peers_by_what_they_send_then_by_seed_rank() {
        let candidates = vec![
            candidate(0, 1, 0, 50),
            candidate(1, 2, 0, 9000),
            candidate(2, 3, 5000, 0),
            candidate(3, 4, 0, 400),
        ];
        let mut random = 7;
        // Two regular slots, no optimistic one.
        let chosen = choose(
            candidates,
            &fixed(1, 2),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(
            chosen.regular,
            HashSet::from([(2, PeerKey(3))]),
            "the sender"
        );
        let candidates = vec![
            candidate(0, 1, 0, 50),
            candidate(1, 2, 0, 9000),
            candidate(3, 4, 0, 400),
        ];
        let chosen = choose(
            candidates,
            &fixed(3, 1),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(
            chosen.regular,
            HashSet::from([(1, PeerKey(2)), (3, PeerKey(4))]),
            "the rarest-seeded torrents"
        );
    }

    /// Which seeding peer gets the one regular slot, by each algorithm.
    #[test]
    fn picks_seeding_peers_by_the_chosen_algorithm() {
        let now = Instant::now();
        let earlier = now - Duration::from_secs(120);
        // 1: uploaded to fastest, unchoked long ago, has had its quota.
        // 2: slower, unchoked a while ago, still within its turn.
        // 3: choked, never had a turn since connecting earlier still.
        let mut fast = candidate(0, 1, 0, 0);
        (fast.uploaded_round, fast.unchoked, fast.quota_done) = (900_000, true, true);
        fast.last_unchoke = earlier;
        fast.leech_score = 0; // half done
        let mut slow = candidate(0, 2, 0, 0);
        (slow.uploaded_round, slow.unchoked) = (10_000, true);
        slow.last_unchoke = now - Duration::from_secs(30);
        slow.leech_score = 900; // nearly done
        let mut waiting = candidate(0, 3, 0, 0);
        waiting.last_unchoke = earlier - Duration::from_secs(60);
        waiting.leech_score = 500;
        let pick = |choking| {
            let policy = Policy {
                global: Some(1),
                per_torrent: 2,
                choking,
                rate_based: false,
            };
            let mut random = 7;
            let chosen = choose(
                vec![fast, slow, waiting],
                &policy,
                RECHOKE_INTERVAL,
                &[],
                true,
                &mut random,
            );
            chosen.regular.into_iter().next().unwrap().1
        };
        assert_eq!(pick(crate::SeedChoking::FastestUpload), PeerKey(1));
        // The fast one had its turn; of the rest, the unchoked one keeps
        // going until its quota.
        assert_eq!(pick(crate::SeedChoking::RoundRobin), PeerKey(2));
        assert_eq!(pick(crate::SeedChoking::AntiLeech), PeerKey(2));
    }

    #[test]
    fn scores_peers_far_from_half_done_highest() {
        let now = Instant::now();
        let swarm = Swarm::new(4);
        let key = swarm.join(address(1), now).unwrap();
        let facts = Facts {
            rank: 0,
            piece_length: 1024,
            total_size: 100 * 1024,
        };
        let score = |have: usize| {
            swarm.set_pieces(key, have, 100);
            anti_leech_score(&swarm.lock().peers[&key], facts)
        };
        assert_eq!(score(0), 1000);
        assert_eq!(score(50), 0);
        assert_eq!(score(100), 1000);
        // Unknown pieces: what we sent it counts, up to half.
        swarm.set_pieces(key, 0, 0);
        swarm.record_upload(key, 25 * 1024);
        assert_eq!(anti_leech_score(&swarm.lock().peers[&key], facts), 500);
    }

    /// libtorrent's rate-based choker: a slot per peer above 1 KiB/s,
    /// then 3, 5... KiB/s, plus one.
    #[test]
    fn rate_based_slots_follow_the_upload() {
        let at = |bytes_per_second: u64| {
            let mut peer = candidate(0, bytes_per_second, 0, 0);
            peer.uploaded_round = bytes_per_second * 10;
            peer
        };
        let ten = Duration::from_secs(10);
        assert_eq!(rate_based_slots(&[], ten), 1);
        assert_eq!(rate_based_slots(&[at(500)], ten), 1);
        assert_eq!(rate_based_slots(&[at(2000), at(500)], ten), 2);
        assert_eq!(rate_based_slots(&[at(9000), at(4000), at(3000)], ten), 3);
        let policy = Policy {
            global: Some(100),
            per_torrent: 4,
            choking: crate::SeedChoking::FastestUpload,
            rate_based: true,
        };
        let candidates: Vec<Candidate> = (0..4)
            .map(|torrent| {
                let mut peer = at(1_000_000);
                peer.slot = (torrent, PeerKey(1));
                peer
            })
            .chain((0..4).map(|torrent| candidate(torrent, 2, 0, 0)))
            .collect();
        let mut random = 7;
        let chosen = choose(candidates, &policy, ten, &[], true, &mut random);
        // 4 fast peers: thresholds 1, 3, 5, 7 KiB/s passed, so 5 slots, not
        // the fixed 100: 4 regular and 1 optimistic.
        assert_eq!(chosen.regular.len() + chosen.optimistic.len(), 5);
    }

    #[test]
    fn keeps_optimistic_slots_until_they_rotate() {
        // Three peers in each of six torrents: one regular slot each, and
        // two optimistic ones between them.
        let candidates: Vec<Candidate> = (0..6)
            .flat_map(|torrent| (0..3).map(move |key| candidate(torrent, key, 0, 1000)))
            .collect();
        let mut random = 7;
        let first = choose(
            candidates.clone(),
            &fixed(10, 2),
            RECHOKE_INTERVAL,
            &[],
            true,
            &mut random,
        );
        assert_eq!(first.optimistic.len(), 2);
        let kept = choose(
            candidates.clone(),
            &fixed(10, 2),
            RECHOKE_INTERVAL,
            &first.optimistic,
            false,
            &mut random,
        );
        assert_eq!(kept.optimistic, first.optimistic);
        // A peer that left gives its slot to another.
        let fewer: Vec<Candidate> = candidates
            .iter()
            .filter(|candidate| candidate.slot != first.optimistic[0])
            .copied()
            .collect();
        let refilled = choose(
            fewer,
            &fixed(10, 2),
            RECHOKE_INTERVAL,
            &first.optimistic,
            false,
            &mut random,
        );
        assert_eq!(refilled.optimistic.len(), first.optimistic.len());
        assert!(!refilled.optimistic.contains(&first.optimistic[0]));
    }

    /// The event loop's choker over real swarms: the global limit holds,
    /// a peer becoming interested is unchoked at once only while a global
    /// slot is free, and leaving frees one.
    #[test]
    fn the_event_loop_keeps_to_the_global_slots() {
        let limits = crate::Limits::unlimited();
        limits.set_upload_slots(5);
        // The test reaches the shared counters through these limits.
        let shared = limits.slots();
        let swarms: Vec<Swarm> = (0..3).map(|_| Swarm::new(4)).collect();
        let now = Instant::now();
        let mut keys = Vec::new();
        for (index, swarm) in swarms.iter().enumerate() {
            for last in 0..4 {
                let key = swarm
                    .join(SocketAddr::from(([10, 0, index as u8, last], 6881)), now)
                    .unwrap();
                swarm.set_interested(key, true);
                keys.push((index, key));
            }
        }
        let mut choker = Choker::default();
        let ranked: Vec<(&Swarm, Facts)> = swarms
            .iter()
            .map(|swarm| (swarm, Facts::default()))
            .collect();
        choker.rechoke_all(&ranked, shared, now);
        let unchoked = || {
            keys.iter()
                .filter(|(index, key)| swarms[*index].is_unchoked(*key, now))
                .count()
        };
        assert_eq!(unchoked(), 5);
        assert_eq!(shared.unchoked.load(Ordering::Relaxed), 5);
        // No free global slot: a new interested peer waits.
        let late = swarms[0]
            .join(SocketAddr::from(([10, 0, 9, 9], 6881)), now)
            .unwrap();
        swarms[0].set_interested(late, true);
        assert!(!swarms[0].is_unchoked(late, now));
        // One unchoked peer leaves, so the next one in gets its slot.
        let (index, key) = *keys
            .iter()
            .find(|(index, key)| *index != 0 && swarms[*index].is_unchoked(*key, now))
            .unwrap();
        swarms[index].leave(key);
        assert_eq!(shared.unchoked.load(Ordering::Relaxed), 4);
        // Into a torrent with a slot of its own free.
        let roomy = swarms
            .iter()
            .find(|swarm| {
                swarm
                    .lock()
                    .peers
                    .values()
                    .filter(|peer| peer.unchoked)
                    .count()
                    < 4
            })
            .unwrap();
        let other = roomy
            .join(SocketAddr::from(([10, 0, 9, 10], 6881)), now)
            .unwrap();
        roomy.set_interested(other, true);
        assert!(
            roomy.is_unchoked(other, now),
            "a free global slot goes at once"
        );
        assert_eq!(shared.unchoked.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn repeated_corrupt_pieces_ban_an_address() {
        let swarm = Swarm::new(4);
        let bad = address(9);
        assert!(!swarm.piece_corrupt(bad));
        swarm.piece_verified(bad);
        assert!(!swarm.piece_corrupt(bad));
        assert!(!swarm.piece_corrupt(bad));
        assert!(swarm.piece_corrupt(bad));
        assert!(swarm.is_banned(bad));
        assert!(swarm.join(bad, Instant::now()).is_none());
        // Any port on the same address.
        assert!(
            swarm
                .join(SocketAddr::new(bad.ip(), 1), Instant::now())
                .is_none()
        );
    }

    #[test]
    fn loopback_bans_cover_one_port() {
        let swarm = Swarm::new(4);
        let bad = SocketAddr::from(([127, 0, 0, 1], 7000));
        for _ in 0..3 {
            swarm.piece_corrupt(bad);
        }
        assert!(swarm.is_banned(bad));
        assert!(!swarm.is_banned(SocketAddr::from(([127, 0, 0, 1], 7001))));
    }

    #[test]
    fn ranks_productive_peers_first_and_skips_banned() {
        let swarm = Swarm::new(4);
        let now = Instant::now();
        let (good, flaky, fresh, bad) = (address(1), address(2), address(3), address(4));
        let key = swarm.join(good, now).unwrap();
        swarm.record_download(key, 10);
        swarm.connect_failed_at(flaky, now);
        for _ in 0..3 {
            swarm.piece_corrupt(bad);
        }
        // A failed address waits a minute before it is tried again, then
        // comes after the untried.
        assert_eq!(
            swarm.rank_at(&[bad, flaky, fresh, good], now),
            [good, fresh]
        );
        let later = now + RECONNECT_DELAY;
        assert_eq!(
            swarm.rank_at(&[bad, flaky, fresh, good], later),
            [good, fresh, flaky]
        );
    }

    #[test]
    fn waits_longer_after_each_failed_connection_as_libtorrent_does() {
        let swarm = Swarm::new(4);
        let now = Instant::now();
        let refusing = address(5);
        let waits: Vec<Duration> = (1..=4)
            .map(|_| {
                swarm.connect_failed_at(refusing, now);
                let mut wait = Duration::ZERO;
                while swarm.rank_at(&[refusing], now + wait).is_empty() {
                    wait += Duration::from_secs(60);
                }
                wait
            })
            .collect();
        assert_eq!(
            waits,
            [60, 120, 1800, 1800].map(Duration::from_secs),
            "a minute per failure, then half an hour after three"
        );
        // Once it answers, it starts over.
        swarm.join(refusing, now).unwrap();
        assert_eq!(swarm.rank_at(&[refusing], now), [refusing]);
    }

    #[test]
    fn exchanges_connected_peers_except_the_recipient() {
        let swarm = Swarm::new(4);
        let now = Instant::now();
        let a = swarm.join(address(1), now).unwrap();
        let b = swarm.join(address(2), now).unwrap();
        let c = swarm.join(address(3), now).unwrap();
        swarm.set_pex(a, address(1), crate::pex::REACHABLE);
        swarm.set_pex(b, address(2), 0);
        // `c` connected to us and gave no listen port: nothing to share.
        let mut contacts = swarm.pex_contacts(c);
        contacts.sort();
        assert_eq!(
            contacts,
            [(address(1), crate::pex::REACHABLE), (address(2), 0)]
        );
        assert_eq!(swarm.pex_contacts(a), [(address(2), 0)]);
        swarm.update_pex_flags(b, crate::pex::SEED, 0);
        assert_eq!(swarm.pex_contacts(a), [(address(2), crate::pex::SEED)]);
        swarm.leave(b);
        assert!(swarm.pex_contacts(a).is_empty());
    }

    #[test]
    fn takes_exchanged_contacts_with_care() {
        let swarm = Swarm::new(4);
        let source = address(200);
        swarm.note_known(&[address(1)]);
        for _ in 0..3 {
            swarm.piece_corrupt(address(2));
        }
        swarm.vote_external(address(50), IpAddr::from([198, 51, 100, 7]));
        swarm.set_listen_port(6881);
        let us = SocketAddr::from(([198, 51, 100, 7], 6881));
        let same_ip = SocketAddr::from(([10, 0, 0, 3], 7000));
        let contacts = [address(1), address(2), us, address(3), same_ip, address(4)]
            .map(|address| (address, 0));
        assert_eq!(swarm.discover(source, &contacts), 2, "only 3 and 4 are new");
        let mut taken = swarm.take_discovered();
        taken.sort();
        assert_eq!(taken, [address(3), address(4)]);
        assert!(swarm.take_discovered().is_empty());
        assert_eq!(swarm.discover(source, &contacts), 0, "already known");
        assert_eq!(swarm.exchanged().len(), 2);

        // One source cannot fill the queue.
        let many: Vec<(SocketAddr, u8)> = (0..300u16)
            .map(|index| {
                (
                    SocketAddr::from(([10, 1, (index / 250) as u8, (index % 250) as u8], 1)),
                    0,
                )
            })
            .collect();
        assert_eq!(swarm.discover(address(201), &many), MAX_FROM_ONE_SOURCE);
    }

    #[test]
    fn learns_its_public_address_by_majority_and_ranks_by_bep_40() {
        let swarm = Swarm::new(4);
        let ours = IpAddr::from([123, 213, 32, 10]);
        swarm.vote_external(address(1), ours);
        swarm.vote_external(address(2), ours);
        swarm.vote_external(address(3), IpAddr::from([1, 1, 1, 1]));
        assert_eq!(swarm.external_ip(), Some(ours));
        swarm.set_listen_port(0);
        // Equal history, so BEP 40 decides: 0xec2d7224 beats 0x99568189.
        let far = SocketAddr::from(([98, 76, 54, 32], 0));
        let near = SocketAddr::from(([123, 213, 32, 234], 0));
        assert_eq!(swarm.rank(&[near, far]), [far, near]);
    }

    #[test]
    fn duplicate_connections_keep_the_one_the_greater_id_started() {
        let now = Instant::now();
        let (low, high) = ([1; 20], [9; 20]);
        // We have the greater id: our outgoing connection stays.
        let swarm = Swarm::new(4);
        let incoming = swarm.join(address(1), now).unwrap();
        assert!(swarm.claim_peer_id(incoming, low, high, false));
        let outgoing = swarm.join(address(1), now).unwrap();
        assert!(swarm.claim_peer_id(outgoing, low, high, true));
        assert!(swarm.is_evicted(incoming) && !swarm.is_evicted(outgoing));

        // They have the greater id: their connection to us stays.
        let swarm = Swarm::new(4);
        let incoming = swarm.join(address(1), now).unwrap();
        assert!(swarm.claim_peer_id(incoming, high, low, false));
        let outgoing = swarm.join(address(1), now).unwrap();
        assert!(!swarm.claim_peer_id(outgoing, high, low, true));
        assert!(!swarm.is_evicted(incoming));
    }

    #[test]
    fn announces_each_verified_piece_once_per_connection() {
        let swarm = Swarm::new(4);
        let mut cursor = 0;
        swarm.add_have(3);
        swarm.add_have(1);
        assert_eq!(swarm.haves_since(&mut cursor), [3, 1]);
        assert!(swarm.haves_since(&mut cursor).is_empty());
        swarm.add_have(7);
        assert_eq!(swarm.haves_since(&mut cursor), [7]);
    }
}
