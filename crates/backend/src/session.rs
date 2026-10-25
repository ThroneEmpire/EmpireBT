use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::active::{ActiveTorrent, Layout};
use crate::{
    AnnounceEvent, AnnounceRequest, PieceManager, Storage, TorrentInfo, TrackerClient,
    TransferPhase, TransferProgress,
};

/// Downloads one torrent: finds peers through trackers, the DHT, and peer
/// exchange, and connects to them. Connections run in a [`crate::Runtime`]
/// with the shared peer protocol (see [`crate::engine`]): the one given
/// with [`DownloadSession::with_runtime`], where peers can also reach us
/// and a finished download keeps seeding, or a private one for the length
/// of the download.
#[derive(Debug, Clone)]
pub struct DownloadSession {
    info: TorrentInfo,
    storage: Storage,
    peer_id: [u8; 20],
    timeout: Duration,
    progress: Arc<TransferProgress>,
    dht: Option<Arc<crate::Dht>>,
    lsd: Option<Arc<crate::Lsd>>,
    resume: Option<crate::ResumeData>,
    layout: Layout,
    connect: crate::ConnectOptions,
    swarm: Arc<crate::swarm::Swarm>,
    runtime: Option<Arc<crate::Runtime>>,
    limits: Arc<crate::Limits>,
}

impl DownloadSession {
    pub fn new(info: TorrentInfo, storage: Storage, peer_id: [u8; 20], timeout: Duration) -> Self {
        let layout = Layout::for_info(&info);
        let progress = Arc::new(TransferProgress::new());
        let session = Self {
            info,
            storage,
            peer_id,
            timeout,
            progress,
            dht: None,
            lsd: None,
            resume: None,
            layout,
            connect: crate::ConnectOptions::default(),
            swarm: Arc::new(crate::swarm::Swarm::new(crate::swarm::DEFAULT_UPLOAD_SLOTS)),
            runtime: None,
            limits: Arc::default(),
        };
        session
            .progress
            .set_totals(session.piece_count(), session.info.total_length());
        session
    }

    fn piece_count(&self) -> usize {
        self.layout.piece_count(&self.info)
    }

    fn piece_size(&self, index: usize) -> Option<u64> {
        self.layout.piece_size(&self.info, index)
    }

    fn verify(&self, index: usize, data: &[u8]) -> bool {
        self.layout.verify(&self.info, index, data)
    }

    fn read_piece(&self, index: usize) -> Result<Vec<u8>, crate::BackendError> {
        self.layout.read_piece(&self.info, &self.storage, index)
    }

    /// The shared state for a download starting from `pieces`.
    fn active(&self, pieces: PieceManager) -> Arc<ActiveTorrent> {
        ActiveTorrent::new(
            self.info.clone(),
            self.storage.clone(),
            self.peer_id,
            self.timeout,
            Arc::clone(&self.progress),
            Arc::clone(&self.swarm),
            pieces,
        )
    }

    /// Runs `work` with the torrent in a runtime, so peers can reach it and
    /// dialed connections can be handed over. A complete torrent stays in
    /// a shared runtime and seeds there; otherwise it leaves.
    fn sharing<T>(
        &self,
        torrent: &Arc<ActiveTorrent>,
        work: impl FnOnce(&crate::Runtime) -> Result<T, crate::BackendError>,
    ) -> Result<T, crate::BackendError> {
        // A runtime being started gets time to come up: its thread may be
        // slow to run on a busy machine, and settling for a private runtime
        // instead would leave the torrent unseeded once it completes.
        let shared = self
            .runtime
            .as_ref()
            .filter(|runtime| runtime.wait_serving(RUNTIME_START));
        let private = match shared {
            Some(_) => None,
            None => Some(PrivateRuntime::start(Arc::clone(&self.limits))?),
        };
        let runtime: &crate::Runtime = match (shared, &private) {
            (Some(runtime), _) => runtime,
            (None, Some(private)) => &private.runtime,
            (None, None) => unreachable!("one of the two exists"),
        };
        runtime.insert(Arc::clone(torrent))?;
        let result = work(runtime);
        if shared.is_some() && torrent.is_complete() {
            runtime.touch();
        } else {
            runtime.remove_exact(torrent);
        }
        result
    }

    /// Starts from saved verified-piece state instead of hashing every
    /// piece, when the files have not changed since it was captured.
    pub fn with_resume(mut self, resume: Option<crate::ResumeData>) -> Self {
        self.resume = resume;
        self
    }

    /// Shares connection and rate limits with the client's other
    /// transfers. A shared runtime brings its own; these also decide how
    /// many peers to dial.
    pub fn with_limits(mut self, limits: Arc<crate::Limits>) -> Self {
        self.limits = limits;
        self
    }

    /// Sets whether peer connections use Message Stream Encryption.
    pub fn with_encryption(mut self, encryption: crate::Encryption) -> Self {
        self.connect = self.connect.with_encryption(encryption);
        self
    }

    /// Sets how peers are reached: encryption and uTP.
    pub fn with_connect_options(mut self, options: crate::ConnectOptions) -> Self {
        self.connect = options;
        self
    }

    /// Peers this download learned about through peer exchange (BEP 11).
    pub fn peers_from_exchange(&self) -> Vec<SocketAddr> {
        self.swarm.exchanged()
    }

    /// Our public address as most peers report seeing it (BEP 10
    /// `yourip`), once any have.
    pub fn external_ip(&self) -> Option<std::net::IpAddr> {
        self.swarm.external_ip()
    }

    /// The port peers can connect to us on, told to peers we connect to so
    /// they can pass it on through peer exchange. Downloads through
    /// trackers set it from the announced port.
    pub fn with_listen_port(self, port: u16) -> Self {
        self.swarm.set_listen_port(port);
        self
    }

    /// Runs connections in `runtime`, the client's shared event loop: peers
    /// that connect to us reach this download there, and once complete the
    /// torrent stays there as a seed, on the same connections. Without one,
    /// or if it is not running, a private event loop serves the download.
    pub fn with_runtime(mut self, runtime: Option<Arc<crate::Runtime>>) -> Self {
        self.runtime = runtime;
        self
    }

    /// Also finds peers through the DHT, and announces us there.
    pub fn with_dht(mut self, dht: Option<Arc<crate::Dht>>) -> Self {
        self.dht = dht;
        self
    }

    /// Finds peers on the local network too (BEP 14). Never set this for
    /// private torrents.
    pub fn with_lsd(mut self, lsd: Option<Arc<crate::Lsd>>) -> Self {
        self.lsd = lsd;
        self
    }

    /// Reports into a caller-owned progress tracker, which can also be used
    /// to cancel tracker-driven downloads from another thread.
    pub fn with_progress(mut self, progress: Arc<TransferProgress>) -> Self {
        progress.set_totals(self.piece_count(), self.info.total_length());
        self.progress = progress;
        self
    }

    pub fn progress(&self) -> &Arc<TransferProgress> {
        &self.progress
    }

    /// Hashes every local piece, reporting incremental progress, and returns
    /// the verified-piece scheduler state.
    pub(crate) fn check_local_pieces(&self) -> Result<PieceManager, crate::BackendError> {
        self.storage.release_files(&self.info);
        self.progress.set_phase(TransferPhase::Checking);
        self.progress.clear_verified();
        let mut pieces = PieceManager::new(self.piece_count());
        // Fast resume: trust saved state while the files are untouched.
        if let Some(resume) = &self.resume
            && resume.is_current(&self.info, &self.storage)
        {
            for index in (0..self.piece_count()).filter(|index| resume.has_piece(*index)) {
                pieces.mark_complete(index);
                self.progress
                    .add_verified(index, self.piece_size(index).unwrap_or(0));
            }
            self.progress.mark_checked();
            return Ok(pieces);
        }
        for index in self.verify_pieces_in_parallel()? {
            pieces.mark_complete(index);
        }
        self.progress.mark_checked();
        Ok(pieces)
    }

    /// Reads every piece in order, so a hard drive reads sequentially, and
    /// hashes them on several threads, so a fast drive is not held back by
    /// one core. Returns the verified pieces.
    fn verify_pieces_in_parallel(&self) -> Result<Vec<usize>, crate::BackendError> {
        let hashers = std::thread::available_parallelism()
            .map_or(1, |count| count.get().saturating_sub(1))
            .clamp(1, MAX_HASHERS);
        // A few pieces queued per hasher keeps them busy without holding
        // much of a large torrent in memory.
        let (pieces_in, pieces_out) =
            std::sync::mpsc::sync_channel::<(usize, Vec<u8>)>(2 * hashers);
        let pieces_out = Mutex::new(pieces_out);
        let verified = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..hashers {
                scope.spawn(|| {
                    loop {
                        let next = pieces_out.lock().ok().and_then(|queue| queue.recv().ok());
                        let Some((index, data)) = next else {
                            return;
                        };
                        if self.verify(index, &data) {
                            self.progress.add_verified(index, data.len() as u64);
                            if let Ok(mut verified) = verified.lock() {
                                verified.push(index);
                            }
                        }
                    }
                });
            }
            let read = (0..self.piece_count()).try_for_each(|index| {
                self.progress.check_cancelled()?;
                let data = self.read_piece(index)?;
                pieces_in
                    .send((index, data))
                    .map_err(|_| crate::BackendError::Io("a hashing thread stopped".to_owned()))
            });
            // Closing the queue lets the hashers finish what is queued.
            drop(pieces_in);
            read
        })?;
        Ok(verified.into_inner().unwrap_or_default())
    }

    fn require_v1_pieces(&self) -> Result<(), crate::BackendError> {
        if matches!(self.layout, Layout::V1) && self.info.pieces().is_empty() {
            return Err(crate::BackendError::InvalidMetainfo(
                "torrent has no piece hashes".to_owned(),
            ));
        }
        Ok(())
    }

    /// Downloads all pieces available from one peer.
    ///
    /// Returns the number of pieces downloaded once the peer has nothing
    /// more to offer. Tracker discovery is intentionally separate: callers
    /// provide the peer address.
    pub fn download_from_peer(&self, address: SocketAddr) -> Result<usize, crate::BackendError> {
        self.require_v1_pieces()?;
        let torrent = self.active(self.check_local_pieces()?);
        if torrent.is_complete() {
            return Ok(0);
        }
        self.progress.set_phase(TransferPhase::Downloading);
        let (downloaded, error) = self.sharing(&torrent, |runtime| {
            self.run_peers(&torrent, runtime, &[address], 1)
        })?;
        match error {
            Some(error) if downloaded == 0 => Err(error),
            _ => Ok(downloaded),
        }
    }

    /// Returns whether every torrent piece is present and hash-verified.
    pub fn is_complete(&self) -> Result<bool, crate::BackendError> {
        if matches!(self.layout, Layout::V2(_)) {
            return Ok(self
                .storage
                .scan_complete_v2_files(&self.info)?
                .into_iter()
                .all(|complete| complete));
        }
        let v1_complete = self
            .storage
            .scan_complete_pieces(&self.info)?
            .into_iter()
            .all(|complete| complete);
        if !v1_complete {
            return Ok(false);
        }
        if self.info.format() == crate::TorrentFormat::Hybrid {
            return Ok(self
                .storage
                .scan_complete_v2_files(&self.info)?
                .into_iter()
                .all(|complete| complete));
        }
        Ok(true)
    }

    /// Announces to the configured HTTP trackers and tries each returned peer.
    ///
    /// This is the synchronous end-to-end v1 path: tracker discovery, peer
    /// handshake, piece download, verification, and disk writes.
    pub fn download_from_trackers(&self, port: u16) -> Result<usize, crate::BackendError> {
        self.download_from_trackers_with_limit(port, 4)
    }

    /// Announces to HTTP trackers and downloads with a bounded peer worker
    /// count.
    pub fn download_from_trackers_with_limit(
        &self,
        port: u16,
        max_connections: usize,
    ) -> Result<usize, crate::BackendError> {
        self.download_from_trackers_with_retries(port, max_connections, 1)
    }

    /// Downloads until every piece is verified or the round limit is
    /// reached. A zero round limit is rejected.
    ///
    /// Each round tries every peer learned so far. Trackers are re-announced
    /// only when their interval allows; between announces, known peers are
    /// retried after a short delay. Verified state is kept in memory across
    /// rounds, so local data is hashed once rather than every round.
    pub fn download_from_trackers_with_retries(
        &self,
        port: u16,
        max_connections: usize,
        max_rounds: usize,
    ) -> Result<usize, crate::BackendError> {
        self.require_v1_pieces()?;
        if max_rounds == 0 {
            return Err(crate::BackendError::InvalidConfiguration(
                "tracker retry rounds must be greater than zero".to_owned(),
            ));
        }
        if max_connections == 0 {
            return Err(crate::BackendError::InvalidConfiguration(
                "maximum peer connections must be greater than zero".to_owned(),
            ));
        }
        let trackers = TrackerClient::from_tiers(self.info.tracker_tiers().to_vec())?
            .with_binding(self.connect.binding());
        let torrent = self.active(self.check_local_pieces()?);
        if torrent.is_complete() {
            self.require_hybrid_roots()?;
            return Ok(0);
        }
        if trackers.trackers().is_empty() && self.dht.is_none() && self.lsd.is_none() {
            return Err(crate::BackendError::Tracker(
                "this torrent lists no trackers and the DHT is disabled".to_owned(),
            ));
        }
        self.sharing(&torrent, |runtime| {
            self.tracker_rounds(
                &torrent,
                runtime,
                &trackers,
                port,
                max_connections,
                max_rounds,
            )
        })
    }

    /// Announces and downloads in rounds; see
    /// [`DownloadSession::download_from_trackers_with_retries`].
    fn tracker_rounds(
        &self,
        torrent: &Arc<ActiveTorrent>,
        runtime: &crate::Runtime,
        trackers: &TrackerClient,
        port: u16,
        max_connections: usize,
        max_rounds: usize,
    ) -> Result<usize, crate::BackendError> {
        let hashes = self.info.swarm_hashes();
        let mut dht_searches: Vec<crate::dht::PeerSearch> = self
            .dht
            .iter()
            .flat_map(|dht| {
                hashes
                    .iter()
                    .map(|hash| crate::dht::PeerSearch::new(Arc::clone(dht), *hash, Some(port)))
            })
            .collect();
        self.swarm.set_listen_port(port);
        // DHT searches run alongside the rounds, so peers they find join
        // the running connections at once instead of waiting for a round
        // to end. They are also kept for later rounds to retry.
        let found = Arc::new(Mutex::new(Vec::new()));
        // Local peers arrive the same way, from the discovery thread.
        let _local = self.lsd.as_ref().map(|lsd| {
            let (swarm, found) = (Arc::clone(&self.swarm), Arc::clone(&found));
            lsd.join(
                &hashes,
                Arc::new(move |peers: &[SocketAddr]| {
                    swarm.offer_from(&swarm.rank(peers), crate::swarm::SOURCE_LSD);
                    if let Ok(mut found) = found.lock() {
                        remember_peers(&mut found, peers);
                    }
                }),
            )
        });
        let searching = AtomicBool::new(!dht_searches.is_empty());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while searching.load(Ordering::Acquire) {
                    for search in &mut dht_searches {
                        let peers = search.poll();
                        if !peers.is_empty() {
                            self.swarm
                                .offer_from(&self.swarm.rank(&peers), crate::swarm::SOURCE_DHT);
                            if let Ok(mut found) = found.lock() {
                                remember_peers(&mut found, &peers);
                            }
                        }
                    }
                    std::thread::sleep(DISCOVERY_POLL);
                }
            });
            let result = self.rounds(
                (torrent, runtime),
                trackers,
                (port, max_connections, max_rounds),
                &found,
            );
            searching.store(false, Ordering::Release);
            result
        })
    }

    /// The announce-and-download rounds of
    /// [`DownloadSession::tracker_rounds`]. `found` collects peers from the
    /// DHT.
    fn rounds(
        &self,
        (torrent, runtime): (&Arc<ActiveTorrent>, &crate::Runtime),
        trackers: &TrackerClient,
        (port, max_connections, max_rounds): (u16, usize, usize),
        found: &Mutex<Vec<SocketAddr>>,
    ) -> Result<usize, crate::BackendError> {
        let hashes = self.info.swarm_hashes();
        let mut peers: Vec<SocketAddr> = Vec::new();
        // Tracker replies join the running connections at once, as DHT
        // results do, and are kept for later rounds through `found`.
        let mut announcer = {
            let swarm = Arc::clone(&self.swarm);
            crate::discovery::Announcer::new(
                trackers,
                self.timeout,
                Arc::new(move |peers: &[SocketAddr]| {
                    swarm.offer_from(&swarm.rank(peers), crate::swarm::SOURCE_TRACKER);
                }),
            )
        };
        // Trackers count what this session downloads, not what was on disk
        // already (checked before this).
        let on_disk = self.progress.verified_bytes();
        let request = |hash, event| self.announce_request(hash, port, event, on_disk);
        let result = (|| {
            let mut downloaded = 0;
            let mut last_error = None;
            let collect = |announcer: &mut crate::discovery::Announcer,
                           last_error: &mut Option<crate::BackendError>| {
                announcer.start_due(&hashes, request);
                let collected = announcer.collect();
                if let Some(best) = &collected.best {
                    self.progress.set_swarm(best);
                }
                if let Some(error) = collected.error {
                    self.progress.record_error(&error);
                    *last_error = Some(error);
                }
            };
            if !announcer.is_empty() {
                // The first round waits for one tracker to answer, or all to
                // fail, so it has peers to try; the rest keep coming meanwhile.
                self.progress.set_phase(TransferPhase::Announcing);
                let deadline = Instant::now() + self.timeout + Duration::from_secs(1);
                loop {
                    collect(&mut announcer, &mut last_error);
                    if announcer.succeeded()
                        || announcer.all_answered()
                        || self.swarm.has_pending()
                        || Instant::now() >= deadline
                    {
                        break;
                    }
                    self.progress.sleep(TRACKER_POLL)?;
                }
            }
            for round in 0..max_rounds {
                collect(&mut announcer, &mut last_error);
                if let Ok(found) = found.lock() {
                    remember_peers(&mut peers, &found);
                }
                self.progress.check_cancelled()?;
                // Peers learned from trackers and through exchange are retried
                // like announced ones.
                remember_peers(&mut peers, &self.swarm.exchanged());
                remember_peers(&mut peers, &self.swarm.take_discovered());
                if !peers.is_empty() {
                    self.progress.set_phase(TransferPhase::Downloading);
                    let (count, error) =
                        self.run_peers(torrent, runtime, &peers, max_connections)?;
                    downloaded += count;
                    if let Some(error) = error {
                        self.progress.record_error(&error);
                        last_error = Some(error);
                    }
                    if torrent.is_complete() {
                        self.require_hybrid_roots()?;
                        if downloaded > 0 {
                            announcer.announce_completed(
                                hashes
                                    .iter()
                                    .map(|hash| {
                                        self.announce_request(
                                            *hash,
                                            port,
                                            AnnounceEvent::Completed,
                                            on_disk,
                                        )
                                    })
                                    .collect(),
                            );
                        }
                        return Ok(downloaded);
                    }
                }
                if round + 1 < max_rounds {
                    self.progress.set_phase(TransferPhase::Waiting);
                    let wait = if peers.is_empty() {
                        announcer
                            .next_due()
                            .map_or(PEER_RETRY_DELAY, |due| {
                                due.saturating_duration_since(Instant::now())
                            })
                            .max(TRACKER_POLL)
                    } else {
                        PEER_RETRY_DELAY
                    };
                    // New peers from trackers or the DHT end the wait early.
                    let deadline = Instant::now() + wait;
                    while Instant::now() < deadline && !self.swarm.has_pending() {
                        self.progress.sleep(DISCOVERY_POLL)?;
                        collect(&mut announcer, &mut last_error);
                    }
                }
            }
            Err(last_error.unwrap_or_else(|| {
                crate::BackendError::Tracker(
                    "no peers have provided the remaining pieces yet".to_owned(),
                )
            }))
        })();
        // Paused or quitting: trackers that know us hear we left, with the
        // final counts, so nothing uploaded is lost from the ratio.
        if matches!(result, Err(crate::BackendError::Cancelled)) {
            announcer.announce_stopped(
                hashes
                    .iter()
                    .map(|hash| request(*hash, AnnounceEvent::Stopped))
                    .collect(),
            );
        }
        result
    }

    /// An announce for `hash`; `on_disk` is what was verified before this
    /// session, which trackers have no business counting as downloaded.
    fn announce_request(
        &self,
        hash: [u8; 20],
        port: u16,
        event: AnnounceEvent,
        on_disk: u64,
    ) -> AnnounceRequest {
        let verified = self.progress.verified_bytes();
        AnnounceRequest {
            info_hash: hash,
            peer_id: self.peer_id,
            port,
            uploaded: self.progress.uploaded_bytes(),
            downloaded: verified.saturating_sub(on_disk),
            left: self.info.total_length().saturating_sub(verified),
            corrupt: self.progress.corrupt_bytes(),
            compact: true,
            event,
        }
    }

    /// Hybrid torrents must also match their v2 Merkle roots once every v1
    /// piece is verified.
    fn require_hybrid_roots(&self) -> Result<(), crate::BackendError> {
        if self.info.format() == crate::TorrentFormat::Hybrid
            && !self
                .storage
                .scan_complete_v2_files(&self.info)?
                .into_iter()
                .all(|complete| complete)
        {
            return Err(crate::BackendError::InvalidMetainfo(
                "hybrid v2 file roots do not match the verified v1 data".to_owned(),
            ));
        }
        Ok(())
    }

    /// Downloads from several peers concurrently with a bounded worker count.
    pub fn download_from_peers(
        &self,
        addresses: &[SocketAddr],
        max_connections: usize,
    ) -> Result<usize, crate::BackendError> {
        if max_connections == 0 {
            return Err(crate::BackendError::InvalidConfiguration(
                "maximum peer connections must be greater than zero".to_owned(),
            ));
        }
        let torrent = self.active(self.check_local_pieces()?);
        self.progress.set_phase(TransferPhase::Downloading);
        let (downloaded, last_error) = self.sharing(&torrent, |runtime| {
            self.run_peers(&torrent, runtime, addresses, max_connections)
        })?;
        if torrent.is_complete() {
            self.require_hybrid_roots()?;
            return Ok(downloaded);
        }
        Err(last_error.unwrap_or_else(|| {
            crate::BackendError::Tracker(
                "tracker peers did not provide all remaining torrent pieces".to_owned(),
            )
        }))
    }

    /// Connects to peers from `addresses`, then to peers learned along the
    /// way, keeping up to `max_connections` connections for this torrent
    /// in `runtime`. A few threads dial, since connecting blocks; once
    /// connected, the runtime's event loop runs each connection. Returns
    /// when every piece is verified, or when no candidate is left and no
    /// connection remains that might find more, with the pieces downloaded
    /// and the last connection error. Only cancellation is an error.
    fn run_peers(
        &self,
        torrent: &Arc<ActiveTorrent>,
        runtime: &crate::Runtime,
        addresses: &[SocketAddr],
        max_connections: usize,
    ) -> Result<(usize, Option<crate::BackendError>), crate::BackendError> {
        let before = self.progress.verified_pieces();
        // Productive peers first; banned ones are skipped.
        let ranked = self.swarm.rank(addresses);
        self.swarm.note_known(&ranked);
        let queue = Mutex::new(std::collections::VecDeque::from(ranked));
        let last_error = Mutex::new(None);
        let dialing = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..max_connections.min(MAX_DIALERS) {
                scope.spawn(|| {
                    loop {
                        if self.progress.is_cancelled() || torrent.is_complete() {
                            return;
                        }
                        // Nothing connects while the chosen network
                        // interface is down; candidates wait for it.
                        if let Err(error) = self.connect.binding().check() {
                            if dialing.load(Ordering::Acquire) == 0
                                && torrent.connection_count() == 0
                            {
                                if let Ok(mut last) = last_error.lock() {
                                    *last = Some(error);
                                }
                                return;
                            }
                            std::thread::sleep(POLL_INTERVAL);
                            continue;
                        }
                        // Only dial while there is room for the connection,
                        // for this torrent and across all of them.
                        let pending = dialing.load(Ordering::Acquire);
                        if torrent.connection_count() + pending >= max_connections
                            || !self.limits.has_room(pending)
                        {
                            std::thread::sleep(POLL_INTERVAL);
                            continue;
                        }
                        let next = queue.lock().ok().and_then(|mut queue| {
                            if queue.is_empty() {
                                queue.extend(self.swarm.take_discovered());
                            }
                            queue.pop_front()
                        });
                        let Some(address) = next else {
                            // Connected peers may still tell us of others.
                            if dialing.load(Ordering::Acquire) == 0
                                && torrent.connection_count() == 0
                            {
                                return;
                            }
                            std::thread::sleep(POLL_INTERVAL);
                            continue;
                        };
                        dialing.fetch_add(1, Ordering::AcqRel);
                        let result = self.connect(address, torrent, runtime);
                        dialing.fetch_sub(1, Ordering::AcqRel);
                        if let Err(error) = result
                            && let Ok(mut last) = last_error.lock()
                        {
                            *last = Some(error);
                        }
                    }
                });
            }
        });
        self.progress.check_cancelled()?;
        // A connection that failed in the event loop explains more than a
        // peer that could not be reached.
        let error = torrent
            .take_error()
            .or(last_error.into_inner().ok().flatten());
        Ok((
            self.progress.verified_pieces().saturating_sub(before),
            error,
        ))
    }

    /// Connects to one peer, admits it to the swarm, and hands the
    /// connection to the runtime.
    fn connect(
        &self,
        address: SocketAddr,
        torrent: &Arc<ActiveTorrent>,
        runtime: &crate::Runtime,
    ) -> Result<(), crate::BackendError> {
        self.progress.check_cancelled()?;
        runtime.connect(address, torrent, &self.connect, self.timeout)
    }
}

/// An event loop serving one download that has no shared runtime.
struct PrivateRuntime {
    runtime: Arc<crate::Runtime>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PrivateRuntime {
    fn start(limits: Arc<crate::Limits>) -> Result<Self, crate::BackendError> {
        let runtime = Arc::new(crate::Runtime::new(Vec::new(), usize::MAX)?.with_limits(limits));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            std::thread::spawn(move || {
                let _ = runtime.serve_adopted(&shutdown);
            })
        };
        if !runtime.wait_serving(Duration::from_secs(5)) {
            return Err(crate::BackendError::Io(
                "the download's event loop did not start".to_owned(),
            ));
        }
        Ok(Self {
            runtime,
            shutdown,
            thread: Some(thread),
        })
    }
}

impl Drop for PrivateRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Adds newly announced peers, keeping the list bounded.
fn remember_peers(known: &mut Vec<SocketAddr>, announced: &[SocketAddr]) {
    for peer in announced {
        if !known.contains(peer) {
            known.push(*peer);
        }
    }
    if known.len() > MAX_KNOWN_PEERS {
        known.drain(..known.len() - MAX_KNOWN_PEERS);
    }
}

/// Workers dialing beyond the connection limit at most.
/// Threads hashing pieces during a check, at most.
const MAX_HASHERS: usize = 4;
const MAX_DIALERS: usize = 30;
/// How long a session waits for a shared runtime that has not started
/// serving yet, before downloading in a private one.
const RUNTIME_START: Duration = Duration::from_secs(30);
/// How often running DHT searches are checked for peers.
const DISCOVERY_POLL: Duration = Duration::from_millis(250);
/// How often finished tracker announces are checked for.
const TRACKER_POLL: Duration = Duration::from_millis(50);
/// Delay before retrying known peers between tracker announces.
const PEER_RETRY_DELAY: Duration = Duration::from_secs(15);
const MAX_KNOWN_PEERS: usize = 200;

/// How long a connection waits for a message before checking for pieces to
/// announce, choke changes, and timeouts.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PeerConnection;
    use crate::{Handshake, PeerMessage};
    use sha1::Digest;
    use std::net::TcpListener;

    /// A 32 KiB single-piece torrent: two blocks per piece.
    fn two_block_torrent() -> (TorrentInfo, Vec<u8>) {
        let data: Vec<u8> = (0..32 * 1024).map(|byte| (byte % 251) as u8).collect();
        let hash: [u8; 20] = sha1::Sha1::digest(&data).into();
        let info = TorrentInfo::new(
            "blocks",
            data.len() as u32,
            vec![hash],
            vec![crate::TorrentFile::new("blocks", data.len() as u64)],
        )
        .unwrap();
        (info, data)
    }

    /// A distinct peer id for each fake peer, as real peers have.
    fn fake_peer_id() -> [u8; 20] {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let mut id = *b"-XX0000-fakepeer0000";
        let number = NEXT.fetch_add(1, Ordering::Relaxed) as u32;
        id[16..].copy_from_slice(&number.to_be_bytes());
        id
    }

    /// Accepts one downloader and hands the connection to `script`.
    fn fake_peer(
        info: &TorrentInfo,
        script: impl FnOnce(&mut PeerConnection) + Send + 'static,
    ) -> (SocketAddr, std::thread::JoinHandle<()>) {
        fake_peer_with(info, Some(vec![0x80]), true, script)
    }

    /// A peer that announces `bitfield` (if any) and optionally unchokes
    /// us before running `script`.
    fn fake_peer_with<T: Send + 'static>(
        info: &TorrentInfo,
        bitfield: Option<Vec<u8>>,
        unchoke: bool,
        script: impl FnOnce(&mut PeerConnection) -> T + Send + 'static,
    ) -> (SocketAddr, std::thread::JoinHandle<T>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handshake = Handshake::new(*info.info_hash(), fake_peer_id());
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut connection =
                PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
            if let Some(bits) = bitfield {
                connection.send(&PeerMessage::Bitfield(bits)).unwrap();
            }
            if unchoke {
                connection.send(&PeerMessage::Unchoke).unwrap();
            }
            script(&mut connection)
        });
        (address, thread)
    }

    /// A peer that negotiates the Fast Extension and sends nothing until
    /// `script` does.
    fn fast_peer<T: Send + 'static>(
        info: &TorrentInfo,
        script: impl FnOnce(&mut PeerConnection) -> T + Send + 'static,
    ) -> (SocketAddr, std::thread::JoinHandle<T>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handshake = Handshake::new(*info.info_hash(), fake_peer_id()).with_fast();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut connection =
                PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
            assert!(connection.supports_fast(), "our handshake offers BEP 6");
            script(&mut connection)
        });
        (address, thread)
    }

    /// A peer that speaks the extension protocol, offering `ut_pex` as
    /// message id 7, then runs `script`.
    fn pex_peer<T: Send + 'static>(
        info: &TorrentInfo,
        script: impl FnOnce(&mut PeerConnection) -> T + Send + 'static,
    ) -> (SocketAddr, std::thread::JoinHandle<T>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handshake = Handshake::new(*info.info_hash(), fake_peer_id()).with_extensions();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut connection =
                PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
            assert!(
                connection.supports_extensions(),
                "our handshake offers BEP 10"
            );
            let ours = crate::bencode::Value::dict([(
                &b"m"[..],
                crate::bencode::Value::dict([(&b"ut_pex"[..], crate::bencode::Value::Int(7))]),
            )]);
            connection
                .send(&PeerMessage::Extended {
                    id: 0,
                    payload: ours.encode(),
                })
                .unwrap();
            script(&mut connection)
        });
        (address, thread)
    }

    /// Reads messages until our extension handshake arrives.
    fn their_extension_handshake(connection: &mut PeerConnection) -> crate::bencode::Value {
        loop {
            if let PeerMessage::Extended { id: 0, payload } = connection.receive().unwrap() {
                return crate::bencode::decode(&payload).unwrap();
            }
        }
    }

    /// A 64 KiB torrent of four one-block pieces.
    fn four_piece_torrent() -> (TorrentInfo, Vec<u8>) {
        let data: Vec<u8> = (0..64 * 1024).map(|byte| (byte * 7 % 253) as u8).collect();
        let hashes = data
            .chunks(16 * 1024)
            .map(|piece| sha1::Sha1::digest(piece).into())
            .collect();
        let info = TorrentInfo::new(
            "four",
            16 * 1024,
            hashes,
            vec![crate::TorrentFile::new("four", data.len() as u64)],
        )
        .unwrap();
        (info, data)
    }

    /// Answers block requests until the downloader disconnects.
    fn serve_blocks(connection: &mut PeerConnection, data: &[u8]) {
        while let Ok(message) = connection.receive() {
            if let PeerMessage::Request {
                piece_index,
                begin,
                length,
            } = message
            {
                let block = data[begin as usize..(begin + length) as usize].to_vec();
                let reply = PeerMessage::Piece {
                    piece_index,
                    begin,
                    block,
                };
                if connection.send(&reply).is_err() {
                    return;
                }
            }
        }
    }

    /// Like [`serve_blocks`], waiting `delay` before each block.
    fn serve_blocks_slowly(connection: &mut PeerConnection, data: &[u8], delay: Duration) {
        while let Ok(message) = connection.receive() {
            if let PeerMessage::Request {
                piece_index,
                begin,
                length,
            } = message
            {
                std::thread::sleep(delay);
                let start = piece_index as usize * 16 * 1024 + begin as usize;
                let block = data[start..start + length as usize].to_vec();
                let reply = PeerMessage::Piece {
                    piece_index,
                    begin,
                    block,
                };
                if connection.send(&reply).is_err() {
                    return;
                }
            }
        }
    }

    fn session(info: &TorrentInfo, name: &str) -> (DownloadSession, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("empirebt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = Storage::new(&root).unwrap();
        (
            DownloadSession::new(info.clone(), storage, [6; 20], Duration::from_secs(5)),
            root,
        )
    }

    /// An HTTP tracker that answers every announce with `peer`.
    fn tracker_for(peer: SocketAddr) -> String {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let SocketAddr::V4(peer) = peer else {
            panic!("an IPv4 test peer")
        };
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.read(&mut [0; 2048]);
                let mut body = b"d8:intervali60e5:peers6:".to_vec();
                body.extend(peer.ip().octets());
                body.extend(peer.port().to_be_bytes());
                body.push(b'e');
                let _ = stream.write_all(&[b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &body].concat());
            }
        });
        url
    }

    #[test]
    fn downloads_from_a_peer_found_on_the_local_network() {
        let Some(lsd) = crate::lsd::tests::running(40010) else {
            eprintln!("skipped: multicast is not available here");
            return;
        };
        let (info, data) = two_block_torrent();
        // Local peers are reached on their network address, so the seeder
        // listens on every interface, as a real client does.
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handshake = Handshake::new(*info.info_hash(), fake_peer_id());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer =
                PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
            peer.send(&PeerMessage::Bitfield(vec![0x80])).unwrap();
            peer.send(&PeerMessage::Unchoke).unwrap();
            serve_blocks(&mut peer, &data);
        });
        // No trackers and no DHT: only local peer discovery.
        let (session, root) = session(&info, "lsd-download");
        let session = session.with_lsd(Some(lsd));
        let hash = *info.info_hash();
        std::thread::scope(|scope| {
            let download = scope.spawn(|| session.download_from_trackers_with_retries(0, 4, 3));
            // The seeder announces, as a client on the network would, until
            // the download has registered and heard it.
            while !download.is_finished() {
                crate::lsd::tests::announce_as_peer(address.port(), &[hash]);
                std::thread::sleep(Duration::from_millis(500));
            }
            download.join().unwrap().unwrap();
        });
        assert!(session.is_complete().unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn downloads_from_a_peer_over_ipv6() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            eprintln!("skipped: no IPv6 here");
            return;
        };
        let (info, data) = two_block_torrent();
        let address = listener.local_addr().unwrap();
        let handshake = Handshake::new(*info.info_hash(), fake_peer_id());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer =
                PeerConnection::from_stream(stream, &handshake, Duration::from_secs(5)).unwrap();
            peer.send(&PeerMessage::Bitfield(vec![0x80])).unwrap();
            peer.send(&PeerMessage::Unchoke).unwrap();
            serve_blocks(&mut peer, &data);
        });
        let (session, root) = session(&info, "ipv6-download");
        session.download_from_peers(&[address], 2).unwrap();
        assert!(session.is_complete().unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn downloads_only_through_the_chosen_network_interface() {
        let (info, data) = two_block_torrent();
        let (address, _peer) = fake_peer(&info, move |peer| serve_blocks(peer, &data));
        let (session, root) = session(&info, "bound-download");
        let session = session.with_connect_options(
            crate::ConnectOptions::new().with_binding(crate::Binding::interface("127.0.0.1")),
        );
        session.download_from_peers(&[address], 2).unwrap();
        assert!(session.is_complete().unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn connects_to_nobody_while_the_chosen_interface_is_down() {
        let (info, _) = two_block_torrent();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let tracker = tracker_for(listener.local_addr().unwrap());
        let info = info.with_tracker_urls(vec![tracker]);
        let (session, root) = session(&info, "interface-down");
        let session = session.with_connect_options(
            crate::ConnectOptions::new().with_binding(crate::Binding::interface("empirebt-vpn0")),
        );
        let error = session
            .download_from_peers(&[listener.local_addr().unwrap()], 1)
            .unwrap_err();
        assert!(error.to_string().contains("empirebt-vpn0"), "{error}");
        let error = session
            .download_from_trackers_with_retries(0, 4, 1)
            .unwrap_err();
        assert!(error.to_string().contains("empirebt-vpn0"), "{error}");
        // Neither the peer nor the tracker heard from us.
        assert!(listener.accept().is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// An HTTP tracker that answers with `peer`, if any, and forwards each
    /// request line.
    fn recording_tracker_for(
        peer: Option<SocketAddr>,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..size])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let mut body = b"d8:intervali60e5:peers".to_vec();
                match peer {
                    Some(SocketAddr::V4(peer)) => {
                        body.extend(b"6:");
                        body.extend(peer.ip().octets());
                        body.extend(peer.port().to_be_bytes());
                    }
                    _ => body.extend(b"0:"),
                }
                body.push(b'e');
                let _ = stream.write_all(&[b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &body].concat());
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        (url, receiver)
    }

    #[test]
    fn trackers_hear_only_what_this_session_downloaded() {
        const PIECE: usize = 16 * 1024;
        let data: Vec<u8> = (0..2 * PIECE).map(|byte| (byte * 7 % 251) as u8).collect();
        let hashes = data
            .chunks(PIECE)
            .map(|piece| sha1::Sha1::digest(piece).into())
            .collect();
        let info = TorrentInfo::new(
            "halfway",
            PIECE as u32,
            hashes,
            vec![crate::TorrentFile::new("halfway", data.len() as u64)],
        )
        .unwrap();
        let served = data.clone();
        let (address, _peer) = fake_peer_with(&info, Some(vec![0xc0]), true, move |peer| {
            while let Ok(message) = peer.receive() {
                if let PeerMessage::Request {
                    piece_index,
                    begin,
                    length,
                } = message
                {
                    let start = piece_index as usize * PIECE + begin as usize;
                    let block = served[start..start + length as usize].to_vec();
                    let reply = PeerMessage::Piece {
                        piece_index,
                        begin,
                        block,
                    };
                    if peer.send(&reply).is_err() {
                        return;
                    }
                }
            }
        });
        let (url, heard) = recording_tracker_for(Some(address));
        let info = info.with_tracker_urls(vec![url]);
        let (session, root) = session(&info, "halfway");
        // The first piece is on disk from an earlier run.
        Storage::new(&root)
            .unwrap()
            .write_piece(&info, 0, &data[..PIECE])
            .unwrap();
        session
            .download_from_trackers_with_retries(0, 4, 1)
            .unwrap();
        let started = heard.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(started.contains("event=started"), "{started}");
        assert!(
            started.contains(&format!("&downloaded=0&left={PIECE}&")),
            "data already on disk is not downloaded again: {started}"
        );
        let completed = std::iter::from_fn(|| heard.recv_timeout(Duration::from_secs(2)).ok())
            .find(|line| line.contains("event=completed"))
            .expect("a completed announce");
        assert!(
            completed.contains(&format!("&downloaded={PIECE}&left=0&corrupt=0&")),
            "{completed}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_paused_download_tells_its_trackers_it_stopped() {
        let (info, _data) = two_block_torrent();
        let (url, heard) = recording_tracker_for(None);
        let info = info.with_tracker_urls(vec![url]);
        let (session, root) = session(&info, "stopped");
        let progress = Arc::clone(session.progress());
        let download =
            std::thread::spawn(move || session.download_from_trackers_with_retries(0, 100, 1));
        let started = heard.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(started.contains("event=started"), "{started}");
        progress.cancel();
        assert_eq!(
            download.join().unwrap(),
            Err(crate::BackendError::Cancelled)
        );
        let stopped = heard.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(stopped.contains("event=stopped&numwant=0"), "{stopped}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_dead_first_tracker_does_not_delay_the_others() {
        let (info, data) = two_block_torrent();
        let (address, _peer) = fake_peer(&info, move |peer| serve_blocks(peer, &data));
        // Accepts connections and never answers.
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}/announce", silent.local_addr().unwrap());
        let info = info.with_tracker_urls(vec![dead, tracker_for(address)]);
        let (session, root) = session(&info, "dead-first-tracker");
        let started = Instant::now();
        session
            .download_from_trackers_with_retries(0, 4, 1)
            .unwrap();
        assert!(session.is_complete().unwrap());
        // Asking trackers one at a time would wait out the 5 s timeout first.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        drop(silent);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_downloads_under_the_download_limit() {
        const PIECE: usize = 64 * 1024;
        let data: Vec<u8> = (0..16 * PIECE)
            .map(|byte| (byte * 13 % 251) as u8)
            .collect();
        let hashes = data
            .chunks(PIECE)
            .map(|piece| sha1::Sha1::digest(piece).into())
            .collect();
        let info = TorrentInfo::new(
            "limited",
            PIECE as u32,
            hashes,
            vec![crate::TorrentFile::new("limited", data.len() as u64)],
        )
        .unwrap();
        let served = data.clone();
        let (address, _peer) = fake_peer_with(&info, Some(vec![0xff, 0xff]), true, move |peer| {
            while let Ok(message) = peer.receive() {
                if let PeerMessage::Request {
                    piece_index,
                    begin,
                    length,
                } = message
                {
                    let start = piece_index as usize * PIECE + begin as usize;
                    let block = served[start..start + length as usize].to_vec();
                    let reply = PeerMessage::Piece {
                        piece_index,
                        begin,
                        block,
                    };
                    if peer.send(&reply).is_err() {
                        return;
                    }
                }
            }
        });
        let (session, root) = session(&info, "download-limit");
        let limits = Arc::new(crate::Limits::unlimited());
        limits.set_download_rate(512 * 1024);
        let session = session.with_limits(limits);
        let started = Instant::now();
        session.download_from_peers(&[address], 1).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(session.progress().verified_pieces(), 16);
        // 1 MiB at 512 KiB/s, less the first burst.
        assert!(elapsed >= Duration::from_millis(1700), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(6), "{elapsed:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resumes_from_saved_state_without_hashing_unchanged_files() {
        let data: Vec<u8> = (0..64).collect();
        let hashes = data
            .chunks(16)
            .map(|piece| sha1::Sha1::digest(piece).into())
            .collect();
        let info = TorrentInfo::new(
            "fast",
            16,
            hashes,
            vec![crate::TorrentFile::new("fast", 64)],
        )
        .unwrap();
        let root = std::env::temp_dir().join(format!("empirebt-fast-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = Storage::new(&root).unwrap();
        storage.write_piece(&info, 0, &data[..16]).unwrap();
        storage.write_piece(&info, 1, &data[16..32]).unwrap();
        let resume = crate::ResumeData::capture(&info, &storage, vec![0b1100_0000]);
        let verified = |resume: Option<crate::ResumeData>| {
            let session = DownloadSession::new(
                info.clone(),
                storage.clone(),
                [6; 20],
                Duration::from_millis(100),
            )
            .with_resume(resume);
            let _ = session.download_from_peers(&[], 1);
            session.progress().verified_pieces()
        };
        assert_eq!(verified(Some(resume.clone())), 2);

        // Corrupt piece 0 but restore the timestamp: the fingerprint still
        // matches, so the saved state is trusted and nothing is re-hashed.
        let path = root.join("fast");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        storage.write_piece(&info, 0, &[0xff; 16]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(verified(Some(resume.clone())), 2, "trusted without hashing");

        // Once the file visibly changes, the full check catches it.
        std::thread::sleep(Duration::from_millis(20));
        storage.write_piece(&info, 0, &[0xff; 16]).unwrap();
        assert_eq!(verified(Some(resume)), 1);
        assert_eq!(verified(None), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn abandons_a_peer_that_only_sends_keep_alives() {
        let (info, data) = two_block_torrent();
        // Takes the request, then sends keep-alives forever without data,
        // which restarts a plain read timeout on every message.
        let (trickling, first) = fake_peer(&info, |connection| {
            while !matches!(connection.receive(), Ok(PeerMessage::Request { .. })) {}
            while connection.send(&PeerMessage::KeepAlive).is_ok() {
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let served = data.clone();
        let (healthy, second) =
            fake_peer(&info, move |connection| serve_blocks(connection, &served));
        let root = std::env::temp_dir().join(format!("empirebt-trickle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = DownloadSession::new(
            info.clone(),
            Storage::new(&root).unwrap(),
            [6; 20],
            Duration::from_secs(1),
        );

        let started = Instant::now();
        assert_eq!(
            session
                .download_from_peers(&[trickling, healthy], 1)
                .unwrap(),
            1
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(session);
        first.join().unwrap();
        second.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_a_peer_that_chokes_mid_piece() {
        let (info, data) = two_block_torrent();
        let served = data.clone();
        let (address, peer) = fake_peer(&info, move |connection| {
            // Answer one block, then choke and unchoke before serving the rest.
            loop {
                if let Ok(PeerMessage::Request {
                    piece_index,
                    begin,
                    length,
                }) = connection.receive()
                {
                    let block = served[begin as usize..(begin + length) as usize].to_vec();
                    connection
                        .send(&PeerMessage::Piece {
                            piece_index,
                            begin,
                            block,
                        })
                        .unwrap();
                    break;
                }
            }
            connection.send(&PeerMessage::Choke).unwrap();
            connection.send(&PeerMessage::Unchoke).unwrap();
            serve_blocks(connection, &served);
        });
        let (session, root) = session(&info, "choke");

        assert_eq!(session.download_from_peer(address).unwrap(), 1);
        assert!(session.is_complete().unwrap());
        drop(session);
        peer.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_peer_failing_mid_piece_releases_it_for_others() {
        let (info, data) = two_block_torrent();
        // The first peer takes the request and disconnects without replying.
        let (failing, first) = fake_peer(&info, |connection| {
            while !matches!(connection.receive(), Ok(PeerMessage::Request { .. })) {}
        });
        let served = data.clone();
        let (healthy, second) =
            fake_peer(&info, move |connection| serve_blocks(connection, &served));
        let (session, root) = session(&info, "release");

        // One worker tries the peers in order, so the second peer can only
        // finish if the first released its claim.
        assert_eq!(
            session.download_from_peers(&[failing, healthy], 1).unwrap(),
            1
        );
        assert_eq!(session.progress().verified_pieces(), 1);
        drop(session);
        first.join().unwrap();
        second.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn uploads_verified_pieces_while_downloading() {
        let (info, data) = four_piece_torrent();
        let (session, root) = session(&info, "upload-while-downloading");
        // We already have piece 0.
        session
            .storage
            .write_piece(&info, 0, &data[..16 * 1024])
            .unwrap();
        // A slow seed keeps the download running.
        let seed_data = data.clone();
        let (seed, _seed) = fake_peer_with(&info, Some(vec![0xf0]), true, move |connection| {
            serve_blocks_slowly(connection, &seed_data, Duration::from_millis(300))
        });
        // A peer with nothing asks us for piece 0 and notes our haves.
        let expected = data[..16 * 1024].to_vec();
        let (leecher, observed) = fake_peer_with(&info, None, false, move |connection| {
            let (mut bitfield, mut block, mut haves) = (None, None, Vec::new());
            while let Ok(message) = connection.receive() {
                match message {
                    PeerMessage::Bitfield(bits) => {
                        bitfield = Some(bits);
                        connection.send(&PeerMessage::Interested).unwrap();
                    }
                    PeerMessage::Unchoke => connection
                        .send(&PeerMessage::Request {
                            piece_index: 0,
                            begin: 0,
                            length: 16 * 1024,
                        })
                        .unwrap(),
                    PeerMessage::Piece {
                        piece_index: 0,
                        begin: 0,
                        block: data,
                    } => block = Some(data),
                    PeerMessage::Have { piece_index } => haves.push(piece_index),
                    _ => {}
                }
            }
            (bitfield, block, haves)
        });
        session.download_from_peers(&[seed, leecher], 2).unwrap();
        let (bitfield, block, haves) = observed.join().unwrap();
        assert_eq!(bitfield, Some(vec![0x80]), "our bitfield shows piece 0");
        assert!(block == Some(expected), "the leecher got piece 0 from us");
        assert!(!haves.is_empty(), "newly verified pieces were announced");
        assert!(haves.iter().all(|index| (1..4).contains(index)));
        assert_eq!(session.progress().uploaded_bytes(), 16 * 1024);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bans_a_peer_that_keeps_sending_corrupt_pieces() {
        let (info, data) = two_block_torrent();
        let corrupt = vec![0xaa; data.len()];
        let (bad, _bad) = fake_peer(&info, move |connection| serve_blocks(connection, &corrupt));
        let served = data.clone();
        let (good, _good) = fake_peer(&info, move |connection| serve_blocks(connection, &served));
        let (session, root) = session(&info, "ban-corrupt");
        session.download_from_peers(&[bad, good], 1).unwrap();
        assert!(session.swarm.is_banned(bad));
        assert!(!session.swarm.is_banned(good));
        assert_eq!(
            session.read_piece(0).unwrap(),
            data,
            "downloaded again elsewhere"
        );
        assert!(
            session
                .progress()
                .last_error()
                .is_some_and(|error| error.contains("corrupt")),
            "{:?}",
            session.progress().last_error()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn closes_connections_neither_side_needs() {
        let (info, _) = two_block_torrent();
        // The peer has nothing, and we have nothing it could want.
        let (peer, closed) = fake_peer_with(&info, Some(vec![0]), true, |connection| {
            while connection.receive().is_ok() {}
        });
        let (session, root) = session(&info, "idle");
        let started = Instant::now();
        assert!(session.download_from_peers(&[peer], 1).is_err());
        closed.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn downloads_allowed_fast_pieces_from_a_peer_that_never_unchokes() {
        let (info, data) = four_piece_torrent();
        let served = data.clone();
        let (peer, _peer) = fast_peer(&info, move |connection| {
            connection.send(&PeerMessage::HaveAll).unwrap();
            for piece_index in 0..4 {
                connection
                    .send(&PeerMessage::AllowedFast { piece_index })
                    .unwrap();
            }
            serve_blocks_slowly(connection, &served, Duration::ZERO);
        });
        let (session, root) = session(&info, "allowed-fast");
        session.download_from_peers(&[peer], 1).unwrap();
        for index in 0..4 {
            let piece = &data[index * 16 * 1024..(index + 1) * 16 * 1024];
            assert_eq!(session.read_piece(index).unwrap(), piece);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn answers_every_request_from_a_fast_peer() {
        let (info, data) = four_piece_torrent();
        let (session, root) = session(&info, "fast-uploads");
        session
            .storage
            .write_piece(&info, 0, &data[..16 * 1024])
            .unwrap();
        let seed_data = data.clone();
        let (seed, _seed) = fake_peer_with(&info, Some(vec![0xf0]), true, move |connection| {
            serve_blocks_slowly(connection, &seed_data, Duration::from_millis(300))
        });
        // A starting peer, choked because it never says it is interested.
        let expected = data[..16 * 1024].to_vec();
        let (leecher, observed) = fast_peer(&info, move |connection| {
            connection.send(&PeerMessage::HaveNone).unwrap();
            let (mut allowed, mut served, mut rejected) = (Vec::new(), None, Vec::new());
            while let Ok(message) = connection.receive() {
                match message {
                    // Tiny torrents grant every piece; ask for one we have
                    // and one we lack.
                    PeerMessage::AllowedFast { piece_index } => {
                        allowed.push(piece_index);
                        if allowed.len() == 1 {
                            for piece_index in [0, 3] {
                                let _ = connection.send(&PeerMessage::Request {
                                    piece_index,
                                    begin: 0,
                                    length: 16 * 1024,
                                });
                            }
                        }
                    }
                    PeerMessage::Piece {
                        piece_index: 0,
                        block,
                        ..
                    } => served = Some(block),
                    PeerMessage::RejectRequest { piece_index, .. } => rejected.push(piece_index),
                    _ => {}
                }
            }
            (allowed, served, rejected)
        });
        session.download_from_peers(&[seed, leecher], 2).unwrap();
        let (allowed, served, rejected) = observed.join().unwrap();
        // Piece 0 at once; the rest as they are verified.
        assert_eq!(allowed.first(), Some(&0), "{allowed:?}");
        assert!(
            served == Some(expected),
            "an allowed fast piece is served while choked"
        );
        assert!(
            rejected.contains(&3),
            "a piece we lack is rejected: {rejected:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn closes_on_fast_messages_without_negotiating_them() {
        let (info, _) = two_block_torrent();
        let (peer, _peer) = fake_peer_with(&info, None, false, |connection| {
            let _ = connection.send(&PeerMessage::HaveAll);
            while connection.receive().is_ok() {}
        });
        let (session, root) = session(&info, "fast-unnegotiated");
        let error = session.download_from_peers(&[peer], 1).unwrap_err();
        assert!(error.to_string().contains("Fast Extension"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn downloads_from_peers_learned_through_peer_exchange() {
        let (info, data) = two_block_torrent();
        let served = data.clone();
        let (seed, _seed) = fake_peer(&info, move |connection| serve_blocks(connection, &served));
        // This peer has nothing, but tells us about the seed.
        let (gossip, _gossip) = pex_peer(&info, move |connection| {
            let message = crate::pex::PexMessage {
                added: vec![(seed, crate::pex::REACHABLE | crate::pex::SEED)],
                dropped: Vec::new(),
            };
            their_extension_handshake(connection);
            connection
                .send(&PeerMessage::Extended {
                    id: crate::pex::UT_PEX,
                    payload: message.encode(),
                })
                .unwrap();
            while connection.receive().is_ok() {}
        });
        let (session, root) = session(&info, "pex-learn");
        session.download_from_peers(&[gossip], 2).unwrap();
        assert_eq!(session.read_piece(0).unwrap(), data);
        assert_eq!(session.swarm.exchanged(), [seed]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tells_peers_about_the_peers_it_is_connected_to() {
        let (info, data) = four_piece_torrent();
        let served = data.clone();
        let (seed, _seed) = fake_peer_with(&info, Some(vec![0xf0]), true, move |connection| {
            serve_blocks_slowly(connection, &served, Duration::from_millis(300))
        });
        let (listener, observed) = pex_peer(&info, |connection| {
            let handshake = their_extension_handshake(connection);
            // Our message ids are ours to choose; this peer's is 7.
            loop {
                match connection.receive() {
                    Ok(PeerMessage::Extended { id: 7, payload }) => {
                        return (handshake, crate::pex::PexMessage::decode(&payload).unwrap());
                    }
                    Ok(_) => {}
                    Err(error) => panic!("no peer exchange arrived: {error}"),
                }
            }
        });
        let (session, root) = session(&info, "pex-tell");
        let session = session.with_listen_port(51413);
        session.download_from_peers(&[seed, listener], 2).unwrap();
        let (handshake, message) = observed.join().unwrap();
        let extensions = handshake.get(b"m").unwrap();
        assert_eq!(
            extensions.get_int(b"ut_pex"),
            Some(crate::pex::UT_PEX.into())
        );
        assert_eq!(extensions.get_int(b"ut_metadata"), Some(1));
        assert_eq!(handshake.get_bytes(b"yourip"), Some(&[127, 0, 0, 1][..]));
        assert_eq!(handshake.get_int(b"p"), Some(51413));
        // The seed, which we connected to. Its seed flag depends on whether
        // its bitfield had arrived when the message went out.
        let [(address, flags)] = message.added[..] else {
            panic!("expected only the seed: {:?}", message.added);
        };
        assert_eq!(address, seed);
        assert!(flags & crate::pex::REACHABLE != 0, "{flags:#x}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn private_torrents_never_exchange_peers() {
        let data: Vec<u8> = (0..32 * 1024).map(|byte| (byte % 251) as u8).collect();
        let hash: [u8; 20] = sha1::Sha1::digest(&data).into();
        let mut torrent =
            b"d4:infod6:lengthi32768e4:name6:blocks12:piece lengthi32768e6:pieces20:".to_vec();
        torrent.extend(hash);
        torrent.extend(b"7:privatei1eee");
        let info = TorrentInfo::from_bytes(&torrent).unwrap();
        assert!(info.is_private());
        let served = data.clone();
        let (seed, _seed) = fake_peer(&info, move |connection| serve_blocks(connection, &served));
        let stranger = SocketAddr::from(([127, 0, 0, 1], 1));
        let (gossip, observed) = pex_peer(&info, move |connection| {
            let handshake = their_extension_handshake(connection);
            let message = crate::pex::PexMessage {
                added: vec![(stranger, 0)],
                dropped: Vec::new(),
            };
            let _ = connection.send(&PeerMessage::Extended {
                id: crate::pex::UT_PEX,
                payload: message.encode(),
            });
            while connection.receive().is_ok() {}
            handshake
        });
        let (session, root) = session(&info, "pex-private");
        session.download_from_peers(&[gossip, seed], 2).unwrap();
        let handshake = observed.join().unwrap();
        assert_eq!(handshake.get(b"m").unwrap().get_int(b"ut_pex"), None);
        assert!(session.swarm.exchanged().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_zero_tracker_retry_rounds() {
        let info = TorrentInfo::new(
            "example",
            16_384,
            vec![[0; 20]],
            vec![crate::TorrentFile::new("example", 1)],
        )
        .unwrap();
        let storage = Storage::new(std::env::temp_dir().join("empirebt-session-test")).unwrap();
        let session = DownloadSession::new(info, storage, [0; 20], Duration::from_millis(1));

        assert!(matches!(
            session.download_from_trackers_with_retries(6881, 1, 0),
            Err(crate::BackendError::InvalidConfiguration(_))
        ));
    }
}
