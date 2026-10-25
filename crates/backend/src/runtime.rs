//! The peer runtime: one event loop that serves every torrent we share.
//!
//! Torrents join and leave while it runs: seeds when their data is
//! verified, downloads while they download. A download that completes
//! simply stays, so its connections carry on as a seed's. Peers connecting
//! to us are routed by the info hash in their handshake. Connections that
//! downloads dial on their own threads are handed over with
//! [`Runtime::adopt`], after which the event loop runs them like any other.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::engine::active::ActiveTorrent;
use crate::{Handshake, PeerConnection, Seeder, V2Seeder};

/// The shared event loop for every torrent being shared.
#[derive(Debug)]
pub struct Runtime {
    routes: Arc<Routes>,
    max_connections: usize,
    handshake_timeout: Duration,
    encryption: crate::Encryption,
    utp: Option<Arc<crate::UtpSocket>>,
    limits: Arc<crate::Limits>,
    /// The network interface outgoing connections leave from.
    binding: crate::Binding,
    connections: Arc<AtomicUsize>,
    /// Outgoing connections being made for torrents in the runtime.
    dialing: Arc<AtomicUsize>,
    /// Set while serving: how other threads hand connections in.
    control: Mutex<Option<crate::reactor::Control>>,
    /// Set once serving has ended, so nobody waits for it to start.
    stopped: AtomicBool,
}

/// Which torrent each handshake hash reaches, shared with the event loop.
#[derive(Debug, Default)]
pub(crate) struct Routes {
    table: RwLock<RouteTable>,
    /// Changes whenever torrents join, leave, or finish downloading.
    revision: AtomicU64,
    /// The port peers reach us on, told to torrents as they join.
    listen_port: Mutex<Option<u16>>,
}

#[derive(Debug, Default)]
struct RouteTable {
    /// By the hashes peers use in handshakes: the v1 hash and, for v2 and
    /// hybrid torrents, the truncated v2 hash.
    by_hash: HashMap<[u8; 20], Arc<ActiveTorrent>>,
    /// By each torrent's own info hash.
    by_key: HashMap<[u8; 20], Arc<ActiveTorrent>>,
    /// `SHA1("req2", hash)` to handshake hash, for encrypted handshakes.
    obfuscated: HashMap<[u8; 20], [u8; 20]>,
}

impl Runtime {
    pub fn new(
        seeders: impl IntoIterator<Item = Seeder>,
        max_connections: usize,
    ) -> Result<Self, crate::BackendError> {
        Self::new_mixed(seeders, Vec::new(), max_connections)
    }

    /// Creates a runtime for both v1 and v2 seeders; more torrents can join
    /// later.
    pub fn new_mixed(
        v1_seeders: impl IntoIterator<Item = Seeder>,
        v2_seeders: impl IntoIterator<Item = V2Seeder>,
        max_connections: usize,
    ) -> Result<Self, crate::BackendError> {
        if max_connections == 0 {
            return Err(crate::BackendError::InvalidConfiguration(
                "maximum runtime connections must be greater than zero".to_owned(),
            ));
        }
        let runtime = Self {
            routes: Arc::default(),
            max_connections,
            handshake_timeout: Duration::from_secs(10),
            encryption: crate::Encryption::default(),
            utp: None,
            limits: Arc::default(),
            binding: crate::Binding::any(),
            connections: Arc::new(AtomicUsize::new(0)),
            dialing: Arc::default(),
            control: Mutex::new(None),
            stopped: AtomicBool::new(false),
        };
        for seeder in v1_seeders {
            runtime.add_seeder(seeder)?;
        }
        for seeder in v2_seeders {
            runtime.add_v2_seeder(seeder)?;
        }
        Ok(runtime)
    }

    /// Sets the maximum time allowed to receive an inbound handshake.
    pub fn with_handshake_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<Self, crate::BackendError> {
        if timeout.is_zero() {
            return Err(crate::BackendError::InvalidConfiguration(
                "runtime handshake timeout must be greater than zero".to_owned(),
            ));
        }
        self.handshake_timeout = timeout;
        Ok(self)
    }

    /// Sets which peers are accepted: plain text, encrypted (MSE), or both.
    pub fn with_encryption(mut self, encryption: crate::Encryption) -> Self {
        self.encryption = encryption;
        self
    }

    /// Also accepts peers connecting over uTP on `socket`.
    pub fn with_utp(mut self, socket: Option<Arc<crate::UtpSocket>>) -> Self {
        self.utp = socket;
        self
    }

    /// Shares connection and rate limits with other runtimes and downloads
    /// using the same `limits`. Without, nothing beyond this runtime's own
    /// connection cap is limited.
    pub fn with_limits(mut self, limits: Arc<crate::Limits>) -> Self {
        self.limits = limits;
        self
    }

    /// Makes outgoing connections through this network interface only.
    pub fn with_binding(mut self, binding: crate::Binding) -> Self {
        self.binding = binding;
        self
    }

    pub fn limits(&self) -> &Arc<crate::Limits> {
        &self.limits
    }

    /// Starts seeding a verified v1 or hybrid torrent.
    pub fn add_seeder(&self, seeder: Seeder) -> Result<(), crate::BackendError> {
        self.add_new(Arc::clone(seeder.torrent()))
    }

    /// Starts seeding a verified v2 torrent.
    pub fn add_v2_seeder(&self, seeder: V2Seeder) -> Result<(), crate::BackendError> {
        self.add_new(Arc::clone(seeder.torrent()))
    }

    fn add_new(&self, torrent: Arc<ActiveTorrent>) -> Result<(), crate::BackendError> {
        if self.contains(torrent.info.info_hash()) {
            return Err(crate::BackendError::InvalidConfiguration(
                "this torrent is already in the runtime".to_owned(),
            ));
        }
        self.insert(torrent)
    }

    /// Adds a torrent, replacing any with the same info hash: the old
    /// one's connections close.
    pub(crate) fn insert(&self, torrent: Arc<ActiveTorrent>) -> Result<(), crate::BackendError> {
        self.routes.insert(torrent)
    }

    /// Stops sharing a torrent; its connections close. Returns whether it
    /// was there.
    pub fn remove(&self, info_hash: &[u8; 20]) -> bool {
        self.routes.remove(info_hash)
    }

    /// Removes `torrent` only if it is still the one registered.
    pub(crate) fn remove_exact(&self, torrent: &Arc<ActiveTorrent>) {
        if self.routes.is_current(torrent) {
            self.routes.remove(torrent.info.info_hash());
        }
    }

    pub fn contains(&self, info_hash: &[u8; 20]) -> bool {
        self.routes.read().by_key.contains_key(info_hash)
    }

    /// The info hash of every torrent in the runtime.
    pub fn info_hashes(&self) -> Vec<[u8; 20]> {
        self.routes.read().by_key.keys().copied().collect()
    }

    /// Torrents in the runtime, downloading or seeding.
    pub fn torrent_count(&self) -> usize {
        self.routes.read().by_key.len()
    }

    /// Torrents whose data is complete.
    pub fn seeding_count(&self) -> usize {
        self.routes
            .read()
            .by_key
            .values()
            .filter(|torrent| torrent.is_complete())
            .count()
    }

    /// The metadata of every complete torrent.
    pub fn seeding_torrents(&self) -> Vec<crate::TorrentInfo> {
        self.seeding_with_peer_ids()
            .into_iter()
            .map(|(info, _)| info)
            .collect()
    }

    /// Complete torrents with the peer ID each uses, which trackers are
    /// told too.
    pub fn seeding_with_peer_ids(&self) -> Vec<(crate::TorrentInfo, [u8; 20])> {
        self.routes
            .read()
            .by_key
            .values()
            .filter(|torrent| torrent.is_complete())
            .map(|torrent| (torrent.info.clone(), torrent.peer_id))
            .collect()
    }

    /// Payload bytes uploaded per torrent since it joined.
    pub fn uploads(&self) -> HashMap<[u8; 20], u64> {
        self.routes
            .read()
            .by_key
            .iter()
            .map(|(key, torrent)| (*key, torrent.progress.uploaded_bytes()))
            .collect()
    }

    /// Changes whenever torrents join, leave, or finish downloading, so
    /// watchers can tell when to look again.
    pub fn revision(&self) -> u64 {
        self.routes.revision.load(Ordering::Acquire)
    }

    /// Notes that a torrent finished downloading.
    pub(crate) fn touch(&self) {
        self.routes.revision.fetch_add(1, Ordering::AcqRel);
    }

    pub fn bind(&self, address: SocketAddr) -> Result<TcpListener, crate::BackendError> {
        crate::net::listen_socket(address, socket2::Type::STREAM)
            .and_then(|socket| {
                socket.listen(1024)?;
                Ok(socket.into())
            })
            .map_err(|error| crate::BackendError::Io(format!("{address}: {error}")))
    }

    /// Binds a listener on each of `addresses` (from
    /// [`crate::Binding::listen_addresses`]), for [`Runtime::serve_until_all`].
    /// IPv4 must work; IPv6 is left out where the system has none.
    pub fn bind_all(
        &self,
        addresses: &[SocketAddr],
    ) -> Result<Vec<TcpListener>, crate::BackendError> {
        let mut listeners: Vec<TcpListener> = Vec::new();
        for address in addresses {
            // Port 0 picks a free port once; the other families share it.
            let mut address = *address;
            if address.port() == 0
                && let Some(port) = listeners
                    .first()
                    .and_then(|listener| listener.local_addr().ok())
            {
                address.set_port(port.port());
            }
            match self.bind(address) {
                Ok(listener) => listeners.push(listener),
                Err(error) if address.is_ipv4() || addresses.len() == 1 => return Err(error),
                Err(_) => {}
            }
        }
        Ok(listeners)
    }

    /// Serves peers until shutdown is requested, routing each to its torrent
    /// by handshake info hash. One event-driven thread handles every
    /// connection, with a few worker threads reading from disk, so the
    /// number of peers we accept is bounded only by `max_connections`.
    pub fn serve_until(
        &self,
        listener: &TcpListener,
        shutdown: &AtomicBool,
    ) -> Result<(), crate::BackendError> {
        self.serve_until_all(std::slice::from_ref(listener), shutdown)
    }

    /// Like [`Runtime::serve_until`], accepting on several listeners: one
    /// per address family, all on the same port.
    pub fn serve_until_all(
        &self,
        listeners: &[TcpListener],
        shutdown: &AtomicBool,
    ) -> Result<(), crate::BackendError> {
        let listeners = listeners
            .iter()
            .map(TcpListener::try_clone)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        if let Some(address) = listeners
            .first()
            .and_then(|listener| listener.local_addr().ok())
        {
            self.routes.set_listen_port(address.port());
        }
        self.serve(listeners, shutdown)
    }

    /// Serves only connections handed over with [`Runtime::adopt`].
    pub(crate) fn serve_adopted(&self, shutdown: &AtomicBool) -> Result<(), crate::BackendError> {
        self.serve(Vec::new(), shutdown)
    }

    fn serve(
        &self,
        listeners: Vec<TcpListener>,
        shutdown: &AtomicBool,
    ) -> Result<(), crate::BackendError> {
        self.stopped.store(false, Ordering::Release);
        let result = crate::reactor::serve(
            &self.routes,
            listeners,
            crate::reactor::Settings {
                max_connections: self.max_connections,
                handshake_timeout: self.handshake_timeout,
                encryption: self.encryption,
                utp: self.utp.as_deref(),
                limits: Arc::clone(&self.limits),
            },
            shutdown,
            &self.connections,
            &self.control,
        );
        self.stopped.store(true, Ordering::Release);
        result
    }

    /// Whether the event loop is running and can adopt connections.
    pub(crate) fn is_serving(&self) -> bool {
        self.control.lock().is_ok_and(|control| control.is_some())
    }

    /// Waits up to `timeout` for the event loop to start; not at all once
    /// it has stopped.
    pub(crate) fn wait_serving(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !self.is_serving() {
            if self.stopped.load(Ordering::Acquire) || std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// Hands a connection we dialed, with its handshakes done and its
    /// protocol state created, to the event loop.
    pub(crate) fn adopt(
        &self,
        connection: PeerConnection,
        address: SocketAddr,
        torrent: Arc<ActiveTorrent>,
        wire: Box<crate::engine::wire::PeerWire>,
    ) -> Result<(), crate::BackendError> {
        let control = self
            .control
            .lock()
            .map_err(|_| crate::BackendError::Io("runtime lock poisoned".to_owned()))?;
        let Some(control) = control.as_ref() else {
            return Err(crate::BackendError::Io(
                "the runtime is not running".to_owned(),
            ));
        };
        control.adopt(crate::reactor::Adopted {
            connection,
            address,
            torrent,
            wire,
        })
    }

    /// The peers connected for a torrent in the runtime, by its info hash.
    pub fn peers(&self, info_hash: &[u8; 20]) -> Option<Vec<crate::PeerInfo>> {
        let torrent = self.routes.read().by_key.get(info_hash).cloned()?;
        Some(torrent.swarm.peers())
    }

    /// Peers currently connected to this runtime.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Acquire)
    }

    /// Connects to `address` for `torrent`, admits the peer to its swarm,
    /// and hands the connection to the event loop.
    pub(crate) fn connect(
        &self,
        address: SocketAddr,
        torrent: &Arc<ActiveTorrent>,
        options: &crate::ConnectOptions,
        timeout: Duration,
    ) -> Result<(), crate::BackendError> {
        let invalid = |message: &str| crate::BackendError::InvalidPeerMessage(message.to_owned());
        // A peer of an address family the chosen interface lacks is not the
        // peer's fault.
        if !options.binding().reaches(&address) {
            options.binding().check()?;
            return Err(crate::BackendError::Io(format!(
                "{address} cannot be reached from network interface {}",
                options.binding().name().unwrap_or_default()
            )));
        }
        let connection =
            PeerConnection::connect_with(address, &torrent.handshake(), timeout, options)
                .inspect_err(|_| torrent.swarm.connect_failed(address))?;
        let key = torrent
            .swarm
            .join(address, std::time::Instant::now())
            .ok_or_else(|| invalid("peer is banned for sending corrupt data"))?;
        if !torrent
            .swarm
            .claim_peer_id(key, *connection.peer_id(), torrent.peer_id, true)
        {
            torrent.swarm.leave(key);
            return Err(invalid("already connected to this peer"));
        }
        let link = crate::engine::wire::Link {
            address,
            key,
            outgoing: true,
            fast: connection.supports_fast(),
            extensions: connection.supports_extensions(),
            utp: connection.is_utp(),
            encrypted: connection.is_encrypted(),
            obfuscated: connection.is_obfuscated(),
        };
        // Created here, so the connection counts as open before the event
        // loop takes it. Dropping it leaves the swarm.
        let wire = Box::new(crate::engine::wire::PeerWire::new(
            Arc::clone(torrent),
            link,
            std::time::Instant::now(),
        ));
        self.adopt(connection, address, Arc::clone(torrent), wire)
    }

    /// Connects to a peer for one of the runtime's torrents in the
    /// background, such as a downloader on the local network asking for a
    /// torrent we seed. Returns whether a connection is being attempted: not
    /// when the torrent is unknown, the peer is already connected, or
    /// there is no room for another connection.
    pub(crate) fn dial(self: &Arc<Self>, address: SocketAddr, info_hash: &[u8; 20]) -> bool {
        let Some(torrent) = self.routes.read().by_hash.get(info_hash).cloned() else {
            return false;
        };
        let pending = self.dialing.load(Ordering::Acquire);
        if pending >= MAX_DIALS
            || !self.limits.has_room(pending)
            || torrent.swarm.is_connected_to(address)
            || torrent.swarm.is_banned(address)
            || !self.is_serving()
        {
            return false;
        }
        self.dialing.fetch_add(1, Ordering::AcqRel);
        let runtime = Arc::clone(self);
        std::thread::spawn(move || {
            let options = crate::ConnectOptions::new()
                .with_binding(runtime.binding.clone())
                .with_encryption(runtime.encryption)
                .with_utp(runtime.utp.clone());
            let _ = runtime.connect(address, &torrent, &options, torrent.timeout);
            runtime.dialing.fetch_sub(1, Ordering::AcqRel);
        });
        true
    }
}

/// Background connections [`Runtime::dial`] makes at once at most.
const MAX_DIALS: usize = 8;

impl Routes {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, RouteTable> {
        self.table
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, RouteTable> {
        self.table
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn insert(&self, torrent: Arc<ActiveTorrent>) -> Result<(), crate::BackendError> {
        let key = *torrent.info.info_hash();
        let hashes = torrent.info.swarm_hashes();
        if let Some(port) = *self.listen_port.lock().unwrap_or_else(|p| p.into_inner()) {
            torrent.swarm.set_listen_port(port);
        }
        let mut table = self.write();
        // A handshake hash must lead to one torrent.
        if hashes.iter().any(|hash| {
            table
                .by_hash
                .get(hash)
                .is_some_and(|other| *other.info.info_hash() != key)
        }) {
            return Err(crate::BackendError::InvalidConfiguration(
                "another torrent in the runtime uses the same handshake hash".to_owned(),
            ));
        }
        if let Some(old) = table.by_key.remove(&key) {
            for hash in old.info.swarm_hashes() {
                table.by_hash.remove(&hash);
                table.obfuscated.remove(&crate::mse::obfuscated_hash(&hash));
            }
        }
        for hash in hashes {
            table.by_hash.insert(hash, Arc::clone(&torrent));
            table
                .obfuscated
                .insert(crate::mse::obfuscated_hash(&hash), hash);
        }
        table.by_key.insert(key, torrent);
        drop(table);
        self.revision.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn remove(&self, info_hash: &[u8; 20]) -> bool {
        let mut table = self.write();
        let Some(torrent) = table.by_key.remove(info_hash) else {
            return false;
        };
        for hash in torrent.info.swarm_hashes() {
            table.by_hash.remove(&hash);
            table.obfuscated.remove(&crate::mse::obfuscated_hash(&hash));
        }
        drop(table);
        self.revision.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Whether `torrent` is still the one registered for its info hash.
    pub(crate) fn is_current(&self, torrent: &Arc<ActiveTorrent>) -> bool {
        self.read()
            .by_key
            .get(torrent.info.info_hash())
            .is_some_and(|current| Arc::ptr_eq(current, torrent))
    }

    /// The handshake hash an encrypted handshake asked for, from the
    /// `SHA1("req2", hash)` it sent.
    pub(crate) fn unobfuscate(&self, req2: &[u8; 20]) -> Option<[u8; 20]> {
        self.read().obfuscated.get(req2).copied()
    }

    /// The torrent a peer's handshake asks for, by the info hash it used.
    /// The v2 handshake bit is not required: it only hints that a v1
    /// connection to a hybrid torrent could be upgraded.
    pub(crate) fn route(&self, handshake: &Handshake) -> Option<Arc<ActiveTorrent>> {
        self.read().by_hash.get(handshake.info_hash()).cloned()
    }

    /// Tells every torrent, and those that join later, the port peers
    /// reach us on.
    fn set_listen_port(&self, port: u16) {
        *self.listen_port.lock().unwrap_or_else(|p| p.into_inner()) = Some(port);
        for torrent in self.read().by_key.values() {
            torrent.swarm.set_listen_port(port);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PeerMessage;
    use std::net::TcpStream;
    use std::thread;

    /// Sessions wait for a shared runtime whose thread starts late (as on
    /// a busy machine) instead of downloading where nothing seeds the
    /// result; once it has stopped, nobody waits.
    #[test]
    fn waits_for_a_late_runtime_but_not_a_stopped_one() {
        let runtime = Arc::new(Runtime::new(Vec::new(), 4).unwrap());
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(1500));
                runtime.serve_adopted(&shutdown)
            })
        };
        assert!(!runtime.is_serving());
        assert!(runtime.wait_serving(Duration::from_secs(10)));
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();

        let started = std::time::Instant::now();
        assert!(!runtime.wait_serving(Duration::from_secs(10)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_silent_peer_does_not_block_other_peers() {
        use std::io::Write;
        let root =
            std::env::temp_dir().join(format!("empirebt-runtime-silent-{}", std::process::id()));
        let storage = crate::Storage::new(&root).unwrap();
        let data = b"runtime";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let info = crate::TorrentInfo::new(
            "runtime",
            data.len() as u32,
            vec![hash],
            vec![crate::TorrentFile::new("runtime", data.len() as u64)],
        )
        .unwrap();
        storage.write_piece(&info, 0, data).unwrap();
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(
            Runtime::new(vec![seeder], 4)
                .unwrap()
                .with_handshake_timeout(Duration::from_secs(5))
                .unwrap(),
        );
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            thread::spawn(move || runtime.serve_until(&listener, &shutdown))
        };

        // One peer connects and stays silent; another sends its handshake
        // split across two writes.
        let _silent = TcpStream::connect(address).unwrap();
        let started = std::time::Instant::now();
        let mut split = TcpStream::connect(address).unwrap();
        let handshake = Handshake::new(*info.info_hash(), [4; 20]).encode();
        split.write_all(&handshake[..10]).unwrap();
        thread::sleep(Duration::from_millis(50));
        split.write_all(&handshake[10..]).unwrap();
        let mut reply = [0; 68];
        std::io::Read::read_exact(&mut split, &mut reply).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            Handshake::decode(&reply).unwrap().info_hash(),
            info.info_hash()
        );

        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn serves_peers_that_pipeline_past_every_limit() {
        use std::io::{Read, Write};
        let root =
            std::env::temp_dir().join(format!("empirebt-runtime-pipeline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = crate::Storage::new(&root).unwrap();
        let data: Vec<u8> = (0..64 * 1024).map(|byte| (byte * 7 % 251) as u8).collect();
        let hashes = data
            .chunks(16 * 1024)
            .map(|piece| {
                use sha1::{Digest, Sha1};
                Sha1::digest(piece).into()
            })
            .collect();
        let info = crate::TorrentInfo::new(
            "pipeline",
            16 * 1024,
            hashes,
            vec![crate::TorrentFile::new("pipeline", data.len() as u64)],
        )
        .unwrap();
        for (index, piece) in data.chunks(16 * 1024).enumerate() {
            storage.write_piece(&info, index, piece).unwrap();
        }
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 64).unwrap());
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            thread::spawn(move || runtime.serve_until(&listener, &shutdown))
        };

        // Each peer asks to download, waits to be unchoked, then sends a
        // full queue of requests (the `reqq` of 250) before reading
        // anything: about 4 MiB of replies each, the output limit, so
        // writing must pause and resume as the peer reads.
        const REQUESTS: usize = 250;
        let peers: Vec<_> = (0..4)
            .map(|peer| {
                let (hash, data) = (*info.info_hash(), data.clone());
                thread::spawn(move || {
                    let mut stream = TcpStream::connect(address).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(20)))
                        .unwrap();
                    stream
                        .write_all(&Handshake::new(hash, [peer; 20]).encode())
                        .unwrap();
                    stream
                        .write_all(&PeerMessage::Interested.encode().unwrap())
                        .unwrap();
                    let mut handshake = [0; 68];
                    stream.read_exact(&mut handshake).unwrap();
                    let read_message = |stream: &mut TcpStream| {
                        let mut prefix = [0; 4];
                        stream.read_exact(&mut prefix).unwrap();
                        let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
                        stream.read_exact(&mut body).unwrap();
                        let mut frame = prefix.to_vec();
                        frame.extend(body);
                        PeerMessage::decode(&frame).unwrap()
                    };
                    while read_message(&mut stream) != PeerMessage::Unchoke {}
                    let mut requests = Vec::new();
                    for index in 0..REQUESTS {
                        let message = PeerMessage::Request {
                            piece_index: (index % 4) as u32,
                            begin: 0,
                            length: 16 * 1024,
                        };
                        requests.extend(message.encode().unwrap());
                    }
                    stream.write_all(&requests).unwrap();
                    let mut pieces = 0;
                    while pieces < REQUESTS {
                        if let PeerMessage::Piece {
                            piece_index, block, ..
                        } = read_message(&mut stream)
                        {
                            let start = piece_index as usize * 16 * 1024;
                            assert_eq!(block, data[start..start + 16 * 1024]);
                            pieces += 1;
                        }
                    }
                    pieces
                })
            })
            .collect();
        for peer in peers {
            assert_eq!(peer.join().unwrap(), REQUESTS);
        }
        assert_eq!(
            runtime.uploads()[info.info_hash()],
            (4 * REQUESTS * 16 * 1024) as u64
        );
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A runtime seeding `size` bytes in 16 KiB pieces, served on a local
    /// port until the returned flag is set.
    #[test]
    fn shows_an_encrypted_peer_as_qbittorrent_would() {
        let limits = Arc::new(crate::Limits::unlimited());
        let (info, _, shutdown, server, root) = serving("peer-flags", 64 * 1024, limits);
        let storage = crate::Storage::new(&root).unwrap();
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(
            Runtime::new(vec![seeder], 8)
                .unwrap()
                .with_encryption(crate::Encryption::Enabled),
        );
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let serving = {
            let (runtime, stop) = (Arc::clone(&runtime), Arc::clone(&stop));
            thread::spawn(move || runtime.serve_until(&listener, &stop))
        };
        let options = crate::ConnectOptions::new().with_encryption(crate::Encryption::Enabled);
        let connection = PeerConnection::connect_with(
            address,
            &Handshake::new(*info.info_hash(), *b"-qB4650-000000000001"),
            Duration::from_secs(5),
            &options,
        )
        .unwrap();
        assert!(connection.is_obfuscated() && connection.is_encrypted());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let flags = loop {
            if let Some(peer) = runtime.peers(info.info_hash()).unwrap().first() {
                break peer
                    .flags()
                    .iter()
                    .map(|(flag, _)| *flag)
                    .collect::<String>();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the peer never showed"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(flags.contains('I') && flags.contains('E'), "{flags}");
        assert!(!flags.contains('e'), "{flags}");
        drop(connection);
        stop.store(true, Ordering::Release);
        serving.join().unwrap().unwrap();
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    fn serving(
        name: &str,
        size: usize,
        limits: Arc<crate::Limits>,
    ) -> (
        crate::TorrentInfo,
        SocketAddr,
        Arc<AtomicBool>,
        thread::JoinHandle<Result<(), crate::BackendError>>,
        std::path::PathBuf,
    ) {
        let root =
            std::env::temp_dir().join(format!("empirebt-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = crate::Storage::new(&root).unwrap();
        let data: Vec<u8> = (0..size).map(|byte| (byte * 7 % 251) as u8).collect();
        let hashes = data
            .chunks(16 * 1024)
            .map(|piece| {
                use sha1::{Digest, Sha1};
                Sha1::digest(piece).into()
            })
            .collect();
        let info = crate::TorrentInfo::new(
            name,
            16 * 1024,
            hashes,
            vec![crate::TorrentFile::new(name, size as u64)],
        )
        .unwrap();
        for (index, piece) in data.chunks(16 * 1024).enumerate() {
            storage.write_piece(&info, index, piece).unwrap();
        }
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 64).unwrap().with_limits(limits));
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            thread::spawn(move || runtime.serve_until(&listener, &shutdown))
        };
        (info, address, shutdown, server, root)
    }

    /// Connects, completes the handshake, and asks to download.
    fn interested_peer(address: SocketAddr, info: &crate::TorrentInfo, id: u8) -> TcpStream {
        use std::io::{Read, Write};
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(&Handshake::new(*info.info_hash(), [id; 20]).encode())
            .unwrap();
        stream
            .write_all(&PeerMessage::Interested.encode().unwrap())
            .unwrap();
        let mut handshake = [0; 68];
        stream.read_exact(&mut handshake).unwrap();
        stream
    }

    fn read_message(stream: &mut TcpStream) -> std::io::Result<PeerMessage> {
        use std::io::Read;
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix)?;
        let mut frame = prefix.to_vec();
        frame.resize(4 + u32::from_be_bytes(prefix) as usize, 0);
        stream.read_exact(&mut frame[4..])?;
        Ok(PeerMessage::decode(&frame).unwrap())
    }

    #[test]
    fn a_seed_dials_downloaders_announcing_on_the_local_network() {
        let Some(lsd) = crate::lsd::tests::running(40011) else {
            eprintln!("skipped: multicast is not available here");
            return;
        };
        let limits = Arc::new(crate::Limits::unlimited());
        let (info, _address, shutdown, server, root) = serving("lsd-seed", 16 * 1024, limits);
        // `serving` keeps its runtime to itself; seed the same torrent here.
        let storage = crate::Storage::new(&root).unwrap();
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 8).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let adopting = {
            let (runtime, stop) = (Arc::clone(&runtime), Arc::clone(&stop));
            thread::spawn(move || runtime.serve_adopted(&stop))
        };
        assert!(runtime.wait_serving(Duration::from_secs(5)));
        lsd.set_runtime(Some(Arc::clone(&runtime)));

        // A downloader on the network announces; the seed connects to it.
        let downloader = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = downloader.local_addr().unwrap().port();
        downloader.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let stream = loop {
            crate::lsd::tests::announce_as_peer(port, &[*info.info_hash()]);
            thread::sleep(Duration::from_millis(500));
            if let Ok((stream, _)) = downloader.accept() {
                break stream;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the seed never dialed"
            );
        };
        stream.set_nonblocking(false).unwrap();
        let handshake = Handshake::new(*info.info_hash(), [9; 20]);
        let connection =
            PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
        assert_eq!(connection.peer_id(), &[3; 20]);

        stop.store(true, Ordering::Release);
        adopting.join().unwrap().unwrap();
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn serves_peers_over_ipv4_and_ipv6_on_one_port() {
        let limits = Arc::new(crate::Limits::unlimited());
        let (info, _, shutdown, server, root) = serving("dual-stack", 16 * 1024, limits);
        let storage = crate::Storage::new(&root).unwrap();
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 8).unwrap());
        let both: [SocketAddr; 2] = ["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()];
        let listeners = runtime.bind_all(&both).unwrap();
        if listeners.len() < 2 {
            eprintln!("skipped: no IPv6 here");
        } else {
            let port = listeners[0].local_addr().unwrap().port();
            assert_eq!(listeners[1].local_addr().unwrap().port(), port);
            let stop = Arc::new(AtomicBool::new(false));
            let serving = {
                let (runtime, stop) = (Arc::clone(&runtime), Arc::clone(&stop));
                thread::spawn(move || runtime.serve_until_all(&listeners, &stop))
            };
            for address in [
                SocketAddr::from(([127, 0, 0, 1], port)),
                SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
            ] {
                let mut peer = interested_peer(address, &info, address.is_ipv6() as u8 + 1);
                while read_message(&mut peer).unwrap() != PeerMessage::Unchoke {}
            }
            stop.store(true, Ordering::Release);
            serving.join().unwrap().unwrap();
        }
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lists_the_peers_connected_for_a_torrent() {
        use std::io::Write;
        let limits = Arc::new(crate::Limits::unlimited());
        let (info, _, shutdown, server, root) = serving("peer-list", 64 * 1024, limits);
        let storage = crate::Storage::new(&root).unwrap();
        let seeder = Seeder::new(info.clone(), storage, [3; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 8).unwrap());
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let serving = {
            let (runtime, stop) = (Arc::clone(&runtime), Arc::clone(&stop));
            thread::spawn(move || runtime.serve_until(&listener, &stop))
        };
        let mut stream = TcpStream::connect(address).unwrap();
        let mut id = *b"-qB4650-000000000000";
        id[19] = b'7';
        stream
            .write_all(&Handshake::new(*info.info_hash(), id).encode())
            .unwrap();
        let mut handshake = [0; 68];
        std::io::Read::read_exact(&mut stream, &mut handshake).unwrap();
        // It has one of the four pieces and wants ours.
        stream
            .write_all(&PeerMessage::Bitfield(vec![0b1000_0000]).encode().unwrap())
            .unwrap();
        stream
            .write_all(&PeerMessage::Interested.encode().unwrap())
            .unwrap();
        while read_message(&mut stream).unwrap() != PeerMessage::Unchoke {}

        let peers = runtime.peers(info.info_hash()).unwrap();
        assert_eq!(peers.len(), 1);
        let peer = &peers[0];
        assert_eq!(peer.address.ip(), address.ip());
        assert_eq!(peer.client.as_deref(), Some("qBittorrent 4.6.5"));
        assert!(!peer.outgoing && !peer.utp);
        assert_eq!(peer.progress, Some(0.25));
        assert!(peer.interested && peer.unchoked);
        // As qBittorrent shows it: uploading to it, it connected to us,
        // plain TCP. (One peer may also hold the optimistic slot.)
        let flags: String = peer.flags().iter().map(|(flag, _)| *flag).collect();
        assert!(flags.starts_with("U") && flags.contains('I'), "{flags}");
        assert!(!flags.contains(['E', 'e', 'P', 'H', 'X', 'L']), "{flags}");
        assert!(runtime.peers(&[9; 20]).is_none());

        stop.store(true, Ordering::Release);
        serving.join().unwrap().unwrap();
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_uploads_under_the_upload_limit() {
        use std::io::Write;
        const SIZE: usize = 1024 * 1024;
        let limits = Arc::new(crate::Limits::unlimited());
        limits.set_upload_rate(512 * 1024);
        let (info, address, shutdown, server, root) = serving("upload-limit", SIZE, limits);
        let mut stream = interested_peer(address, &info, 1);
        while read_message(&mut stream).unwrap() != PeerMessage::Unchoke {}
        let started = std::time::Instant::now();
        let mut requests = Vec::new();
        for piece in 0..(SIZE / (16 * 1024)) as u32 {
            let request = PeerMessage::Request {
                piece_index: piece,
                begin: 0,
                length: 16 * 1024,
            };
            requests.extend(request.encode().unwrap());
        }
        stream.write_all(&requests).unwrap();
        let mut received = 0;
        while received < SIZE {
            if let PeerMessage::Piece { block, .. } = read_message(&mut stream).unwrap() {
                received += block.len();
            }
        }
        // 1 MiB at 512 KiB/s, less the first burst.
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(1700), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_peers_beyond_the_shared_connection_limit() {
        let limits = Arc::new(crate::Limits::unlimited());
        limits.set_max_connections(2);
        let (info, address, shutdown, server, root) =
            serving("connection-limit", 16 * 1024, Arc::clone(&limits));
        let first = interested_peer(address, &info, 1);
        let _second = interested_peer(address, &info, 2);
        assert_eq!(limits.open_connections(), 2);
        // A third is closed at once, before any handshake.
        let mut third = TcpStream::connect(address).unwrap();
        third
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(matches!(
            std::io::Read::read(&mut third, &mut [0; 1]),
            Ok(0) | Err(_)
        ));
        // Once one leaves, there is room again.
        drop(first);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while limits.open_connections() > 1 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _fourth = interested_peer(address, &info, 4);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        assert_eq!(limits.open_connections(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_an_empty_connection_limit() {
        assert!(Runtime::new(Vec::new(), 0).is_err());
    }

    #[test]
    fn rejects_zero_handshake_timeout() {
        let runtime = Runtime::new(Vec::new(), 1).unwrap();
        assert!(runtime.with_handshake_timeout(Duration::ZERO).is_err());
    }
}
