//! Keeps trackers informed about torrents being seeded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{AnnounceEvent, AnnounceRequest, Dht, Runtime, TrackerClient};

const POLL: Duration = Duration::from_millis(250);
/// DHT peers expire after about 30 minutes, so announces repeat sooner.
const DHT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Tracker announces running at once. Each can wait the full timeout on a
/// dead tracker, so many must overlap to keep thousands of torrents current.
const TRACKER_WORKERS: usize = 32;
/// How long quitting gives trackers to hear `stopped`, all of them:
/// libtorrent's `stop_tracker_timeout`.
const STOP_DEADLINE: Duration = Duration::from_secs(5);
/// DHT lookups running at once; each takes seconds on the public network.
const DHT_WORKERS: usize = 16;

/// The latest announce outcome for one seeded torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceStatus {
    /// Trackers that accepted the most recent announce.
    pub trackers_ok: usize,
    pub trackers_total: usize,
    /// Swarm size from the largest tracker report, when trackers report it.
    pub seeders: Option<u64>,
    pub leechers: Option<u64>,
    /// The first failure from the most recent announce, if any tracker failed.
    pub last_error: Option<String>,
    pub next_announce: Instant,
    /// Total payload bytes uploaded for this torrent since the announcer
    /// started, across seeding runtime rebuilds.
    pub uploaded: u64,
    /// When the torrent was last announced to the DHT.
    pub dht_announced: Option<Instant>,
}

#[derive(Debug, Default)]
struct Shared {
    runtime: Mutex<Option<Arc<Runtime>>>,
    status: Mutex<HashMap<[u8; 20], AnnounceStatus>>,
    dht_announced: Mutex<HashMap<[u8; 20], Instant>>,
    shutdown: AtomicBool,
    /// Torrents to announce now, to trackers and to the DHT.
    reannounce_trackers: Mutex<std::collections::HashSet<[u8; 20]>>,
    reannounce_dht: Mutex<std::collections::HashSet<[u8; 20]>>,
}

/// Announces every torrent served by a seeding [`Runtime`] to all of its
/// trackers: `started` when seeding begins, regular re-announces on each
/// tracker's interval, and `stopped` when seeding ends or the announcer is
/// stopped. Without this, trackers never learn that we have the data, so
/// new peers cannot find us.
///
/// Statuses are keyed by each torrent's [`TorrentInfo::info_hash`].
#[derive(Debug)]
pub struct SeedAnnouncer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    dht_thread: Option<JoinHandle<()>>,
}

impl SeedAnnouncer {
    /// Starts the announcer thread. `port` is the listening port reported
    /// to trackers and the DHT. With `dht`, seeded torrents are also
    /// announced there, so peers without a working tracker can find us.
    pub fn start(port: u16, timeout: Duration, dht: Option<Arc<Dht>>) -> Self {
        Self::start_on(crate::Binding::any(), port, timeout, dht)
    }

    /// Like [`Self::start`], announcing to trackers through one network
    /// interface only.
    pub fn start_on(
        binding: crate::Binding,
        port: u16,
        timeout: Duration,
        dht: Option<Arc<Dht>>,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            AnnounceLoop {
                shared: worker,
                binding,
                port,
                timeout,
                entries: HashMap::new(),
                tracked_runtime: None,
                in_flight: 0,
                outcomes: mpsc::channel(),
            }
            .run();
        });
        let dht_thread = dht.map(|dht| {
            let worker = Arc::clone(&shared);
            std::thread::spawn(move || dht_loop(&worker, &dht, port))
        });
        Self {
            shared,
            thread: Some(thread),
            dht_thread,
        }
    }

    /// Announces the torrents served by `runtime`, replacing any previous
    /// runtime. Torrents no longer served are announced as stopped.
    pub fn track(&self, runtime: Arc<Runtime>) {
        if let Ok(mut current) = self.shared.runtime.lock() {
            *current = Some(runtime);
        }
    }

    /// Stops announcing every torrent, for when nothing is seeded.
    pub fn clear(&self) {
        if let Ok(mut current) = self.shared.runtime.lock() {
            *current = None;
        }
    }

    /// Announces a seeded torrent to its trackers and the DHT now rather
    /// than when next due, to find new downloaders. Returns whether the
    /// torrent is being seeded.
    pub fn reannounce(&self, info_hash: &[u8; 20]) -> bool {
        let seeded = self
            .shared
            .runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.clone())
            .is_some_and(|runtime| {
                runtime
                    .seeding_torrents()
                    .iter()
                    .any(|info| info.info_hash() == info_hash)
            });
        if seeded {
            for set in [
                &self.shared.reannounce_trackers,
                &self.shared.reannounce_dht,
            ] {
                if let Ok(mut set) = set.lock() {
                    set.insert(*info_hash);
                }
            }
        }
        seeded
    }

    pub fn status(&self, info_hash: &[u8; 20]) -> Option<AnnounceStatus> {
        let dht_announced = self
            .shared
            .dht_announced
            .lock()
            .ok()
            .and_then(|announced| announced.get(info_hash).copied());
        let tracker = self
            .shared
            .status
            .lock()
            .ok()
            .and_then(|status| status.get(info_hash).cloned());
        match (tracker, dht_announced) {
            (Some(status), dht_announced) => Some(AnnounceStatus {
                dht_announced,
                ..status
            }),
            // A torrent without trackers, announced only through the DHT.
            (None, Some(at)) => Some(AnnounceStatus {
                trackers_ok: 0,
                trackers_total: 0,
                seeders: None,
                leechers: None,
                last_error: None,
                next_announce: at + DHT_INTERVAL,
                uploaded: 0,
                dht_announced: Some(at),
            }),
            (None, None) => None,
        }
    }

    /// Asks the announcer to send `stopped` announces and exit. The thread
    /// can take up to the tracker timeout to finish.
    pub fn stop(&mut self) -> Option<JoinHandle<()>> {
        self.shared.shutdown.store(true, Ordering::Release);
        // The DHT thread ends at its next poll; only tracker `stopped`
        // announces are worth waiting for.
        drop(self.dht_thread.take());
        self.thread.take()
    }
}

impl Drop for SeedAnnouncer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
    }
}

struct Entry {
    /// Every hash the swarm is known by (two for hybrid torrents); the
    /// first decides the status shown.
    announce_hashes: Vec<[u8; 20]>,
    /// The peer ID the torrent uses in the runtime.
    peer_id: [u8; 20],
    /// Each tier of trackers (BEP 12) on a schedule of its own, as
    /// libtorrent keeps one per tracker: one that answers is asked again
    /// on its interval, one that fails sooner, on libtorrent's backoff,
    /// whatever the others do.
    tiers: Vec<Tier>,
    /// A private torrent (BEP 27).
    private: bool,
    /// Bytes uploaded by runtimes that have since been replaced.
    uploaded_before: u64,
    /// Latest counter value from the current runtime.
    uploaded_now: u64,
    /// The counter when announcing began. A finished download stays in the
    /// runtime as a seed with its counter, and its own announces already
    /// reported those bytes; counting them again would report uploads
    /// twice, which private trackers take for cheating.
    uploaded_base: u64,
}

/// One tier of a seeded torrent's trackers.
struct Tier {
    trackers: Arc<TrackerClient>,
    next_announce: Instant,
    /// It accepted an announce, so it heard `started`: later ones carry no
    /// event, and it hears `stopped` when we leave. One that never
    /// answered gets `started` with its first announce that works.
    started: bool,
    in_flight: bool,
    failures: u32,
    /// Its latest answer: the swarm's size, or why not.
    last: Option<Result<SwarmCount, String>>,
}

/// Seeders and leechers, as a tracker reported them.
type SwarmCount = (Option<u64>, Option<u64>);

impl Tier {
    fn new(trackers: TrackerClient) -> Self {
        Self {
            trackers: Arc::new(trackers),
            next_announce: Instant::now(),
            started: false,
            in_flight: false,
            failures: 0,
            last: None,
        }
    }
}

/// How long a tier that failed `failures` times in a row waits:
/// libtorrent's backoff (see [`crate::tracker::retry_delay`]), quicker in
/// tests.
fn tier_retry(failures: u32) -> Duration {
    if cfg!(test) {
        Duration::from_millis(300)
    } else {
        crate::tracker::retry_delay(failures)
    }
}

impl Entry {
    /// Bytes uploaded since this entry's `started` announce.
    fn uploaded(&self) -> u64 {
        (self.uploaded_before + self.uploaded_now).saturating_sub(self.uploaded_base)
    }
}

/// What a worker thread sends back.
enum Outcome {
    /// One finished announce to one tier.
    Tier {
        key: [u8; 20],
        tier: usize,
        result: Result<crate::TrackerResponse, crate::BackendError>,
    },
    /// Every tier it took has answered: its slot is free.
    Done,
}

/// A seeded torrent as the announce loops see it.
struct Target {
    key: [u8; 20],
    announce_hashes: Vec<[u8; 20]>,
    peer_id: [u8; 20],
    trackers: Vec<Vec<String>>,
    private: bool,
}

/// Lists the runtime's complete torrents. Called only when the runtime or
/// its revision changes, so polling never copies metadata. Downloads
/// announce themselves until they complete.
fn targets(runtime: &Runtime) -> Vec<Target> {
    runtime
        .seeding_with_peer_ids()
        .iter()
        .map(|(info, peer_id)| Target {
            key: *info.info_hash(),
            announce_hashes: info.swarm_hashes(),
            peer_id: *peer_id,
            trackers: info.tracker_tiers().to_vec(),
            private: info.is_private(),
        })
        .collect()
}

/// Whether `runtime` is the one last looked at, at the same revision.
fn unchanged(runtime: &Option<Arc<Runtime>>, tracked: &Option<(Arc<Runtime>, u64)>) -> bool {
    match (runtime, tracked) {
        (Some(current), Some((previous, revision))) => {
            Arc::ptr_eq(current, previous) && current.revision() == *revision
        }
        (None, None) => true,
        _ => false,
    }
}

/// Spreads re-announces by up to ±10% so thousands of torrents added at
/// once do not stay due at the same moment.
fn jittered(interval: Duration) -> Duration {
    let [byte] = crate::dht::random_bytes::<1>();
    interval.mul_f64(0.9 + f64::from(byte) / 255.0 * 0.2)
}

struct AnnounceLoop {
    shared: Arc<Shared>,
    binding: crate::Binding,
    port: u16,
    timeout: Duration,
    entries: HashMap<[u8; 20], Entry>,
    tracked_runtime: Option<(Arc<Runtime>, u64)>,
    in_flight: usize,
    outcomes: (mpsc::Sender<Outcome>, mpsc::Receiver<Outcome>),
}

impl AnnounceLoop {
    fn run(mut self) {
        while !self.shared.shutdown.load(Ordering::Acquire) {
            self.sync_targets();
            self.collect_outcomes();
            self.announce_due();
            std::thread::sleep(POLL);
        }
        // Leave every swarm: `stopped` to the trackers that know us, a
        // few dozen at a time (not a thread per torrent, which with
        // thousands of seeds would open thousands of connections to the
        // same trackers at once), and for a few seconds at most.
        let deadline = Instant::now() + STOP_DEADLINE;
        let leaving: Vec<[u8; 20]> = self.entries.keys().copied().collect();
        let stops: Vec<Stopped> = leaving.iter().filter_map(|key| self.leave(key)).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let timeout = self.timeout;
        std::thread::scope(|scope| {
            for _ in 0..TRACKER_WORKERS.min(stops.len()) {
                scope.spawn(|| {
                    while let Some((trackers, requests)) =
                        stops.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        for request in requests {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                return;
                            }
                            announce_tiers(trackers, request, timeout.min(left));
                        }
                    }
                });
            }
        });
    }

    /// Mirrors the runtime's torrents into announce entries.
    fn sync_targets(&mut self) {
        let runtime = self
            .shared
            .runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.clone());
        if !unchanged(&runtime, &self.tracked_runtime) {
            let replaced = match (&runtime, &self.tracked_runtime) {
                (Some(current), Some((previous, _))) => !Arc::ptr_eq(current, previous),
                _ => true,
            };
            // A new runtime starts its upload counters from zero.
            if replaced {
                for entry in self.entries.values_mut() {
                    entry.uploaded_before += entry.uploaded_now;
                    entry.uploaded_now = 0;
                }
            }
            self.tracked_runtime = runtime
                .as_ref()
                .map(|runtime| (Arc::clone(runtime), runtime.revision()));
            let current = runtime.as_deref().map(targets).unwrap_or_default();
            let wanted: std::collections::HashSet<[u8; 20]> =
                current.iter().map(|target| target.key).collect();
            let removed: Vec<[u8; 20]> = self
                .entries
                .keys()
                .filter(|key| !wanted.contains(*key))
                .copied()
                .collect();
            for key in removed {
                // Stop announces finish on their own threads.
                drop(self.remove(&key));
            }
            let counters = runtime
                .as_ref()
                .map(|runtime| runtime.uploads())
                .unwrap_or_default();
            for target in current {
                if self.entries.contains_key(&target.key) {
                    continue;
                }
                let counter = counters.get(&target.key).copied().unwrap_or(0);
                // Torrents without usable trackers are not announced to
                // trackers (the DHT loop still covers public ones).
                let Ok(trackers) = TrackerClient::from_tiers(target.trackers)
                    .map(|trackers| trackers.with_binding(&self.binding))
                else {
                    continue;
                };
                if trackers.trackers().is_empty() {
                    continue;
                }
                self.entries.insert(
                    target.key,
                    Entry {
                        announce_hashes: target.announce_hashes,
                        peer_id: target.peer_id,
                        tiers: trackers.tiers().into_iter().map(Tier::new).collect(),
                        private: target.private,
                        uploaded_before: 0,
                        uploaded_now: counter,
                        uploaded_base: counter,
                    },
                );
            }
        }
        if let Some(runtime) = &runtime {
            for (key, uploaded) in runtime.uploads() {
                if let Some(entry) = self.entries.get_mut(&key) {
                    // A torrent that rejoined counts from zero again.
                    if uploaded < entry.uploaded_now {
                        entry.uploaded_before += entry.uploaded_now;
                    }
                    entry.uploaded_now = uploaded;
                }
            }
        }
    }

    /// Starts announces for due tiers, up to the worker limit.
    fn announce_due(&mut self) {
        let now = Instant::now();
        let forced = self
            .shared
            .reannounce_trackers
            .lock()
            .map(|mut forced| std::mem::take(&mut *forced))
            .unwrap_or_default();
        for key in forced {
            if let Some(entry) = self.entries.get_mut(&key) {
                for tier in &mut entry.tiers {
                    tier.next_announce = tier.next_announce.min(now);
                }
            }
        }
        // A torrent's due tiers go out together, in one worker slot, so a
        // dead tracker beside a working one costs no extra slot.
        let mut due: Vec<([u8; 20], Vec<usize>, Instant)> = self
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                let tiers: Vec<(usize, Instant)> = entry
                    .tiers
                    .iter()
                    .enumerate()
                    .filter(|(_, tier)| !tier.in_flight && tier.next_announce <= now)
                    .map(|(index, tier)| (index, tier.next_announce))
                    .collect();
                let oldest = tiers.iter().map(|(_, at)| *at).min()?;
                Some((
                    *key,
                    tiers.into_iter().map(|(index, _)| index).collect(),
                    oldest,
                ))
            })
            .collect();
        // Longest-waiting first, so no torrent starves behind others.
        due.sort_by_key(|(_, _, at)| *at);
        for (key, indexes, _) in due {
            if self.in_flight >= TRACKER_WORKERS {
                return;
            }
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            let uploaded = entry.uploaded();
            let (peer_id, private, port) = (entry.peer_id, entry.private, self.port);
            let hashes = entry.announce_hashes.clone();
            let mut work = Vec::new();
            for index in indexes {
                let Some(tier) = entry.tiers.get_mut(index) else {
                    continue;
                };
                let event = if tier.started {
                    AnnounceEvent::None
                } else {
                    AnnounceEvent::Started
                };
                let requests: Vec<AnnounceRequest> = hashes
                    .iter()
                    .map(|hash| request(*hash, peer_id, port, uploaded, event, private))
                    .collect();
                tier.in_flight = true;
                work.push((index, Arc::clone(&tier.trackers), requests));
            }
            let (timeout, sender) = (self.timeout, self.outcomes.0.clone());
            self.in_flight += 1;
            std::thread::spawn(move || {
                std::thread::scope(|scope| {
                    for (index, trackers, requests) in &work {
                        let sender = sender.clone();
                        scope.spawn(move || {
                            let result = trackers.announce(&requests[0], timeout);
                            // A hybrid's other hash, so peers searching by
                            // it find us.
                            for request in &requests[1..] {
                                let _ = trackers.announce(request, timeout);
                            }
                            let _ = sender.send(Outcome::Tier {
                                key,
                                tier: *index,
                                result,
                            });
                        });
                    }
                });
                // The slot is free once every tier has answered.
                let _ = sender.send(Outcome::Done);
            });
        }
    }

    fn collect_outcomes(&mut self) {
        while let Ok(outcome) = self.outcomes.1.try_recv() {
            let (key, tier, result) = match outcome {
                Outcome::Done => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    continue;
                }
                Outcome::Tier { key, tier, result } => (key, tier, result),
            };
            // The torrent may have stopped seeding while the announce ran.
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            let Some(state) = entry.tiers.get_mut(tier) else {
                continue;
            };
            state.in_flight = false;
            let now = Instant::now();
            match result {
                Ok(response) => {
                    state.started = true;
                    state.failures = 0;
                    // Its interval, but never sooner than it allows.
                    let interval = response.next_announce_in().max(
                        response
                            .min_interval()
                            .map_or(Duration::ZERO, Duration::from_secs),
                    );
                    state.next_announce = now + jittered(interval);
                    state.last = Some(Ok((response.seeders(), response.leechers())));
                    // Downloaders that cannot reach us (we may be behind a
                    // NAT or a VPN without a forwarded port) still get the
                    // data if we connect to them, as libtorrent's seeds do.
                    if let Some((runtime, _)) = &self.tracked_runtime {
                        dial_peers(runtime, &key, response.peers().iter());
                    }
                }
                Err(error) => {
                    state.failures = state.failures.saturating_add(1);
                    state.next_announce = now + jittered(tier_retry(state.failures));
                    state.last = Some(Err(error.to_string()));
                }
            }
            let answers = || entry.tiers.iter().filter_map(|tier| tier.last.as_ref());
            let counts = || answers().filter_map(|answer| answer.as_ref().ok());
            let status = AnnounceStatus {
                trackers_ok: counts().count(),
                trackers_total: entry.tiers.len(),
                seeders: counts().filter_map(|(seeders, _)| *seeders).max(),
                leechers: counts().filter_map(|(_, leechers)| *leechers).max(),
                last_error: answers().find_map(|answer| answer.as_ref().err().cloned()),
                next_announce: entry
                    .tiers
                    .iter()
                    .map(|tier| tier.next_announce)
                    .min()
                    .unwrap_or(now),
                uploaded: entry.uploaded(),
                dht_announced: None,
            };
            if let Ok(mut statuses) = self.shared.status.lock() {
                statuses.insert(key, status);
            }
        }
    }

    /// Forgets a torrent and, if trackers knew about it, tells them we left
    /// on a background thread, which is returned.
    /// Stops announcing a torrent that left the runtime, telling its
    /// trackers in the background.
    fn remove(&mut self, key: &[u8; 20]) -> Option<JoinHandle<()>> {
        let (trackers, requests) = self.leave(key)?;
        let timeout = self.timeout;
        Some(std::thread::spawn(move || {
            for request in &requests {
                announce_tiers(&trackers, request, timeout);
            }
        }))
    }

    /// Forgets a torrent; returns the `stopped` announces for the tiers
    /// that heard `started`, if any did.
    fn leave(&mut self, key: &[u8; 20]) -> Option<Stopped> {
        if let Ok(mut statuses) = self.shared.status.lock() {
            statuses.remove(key);
        }
        let entry = self.entries.remove(key)?;
        let tiers: Vec<Arc<TrackerClient>> = entry
            .tiers
            .iter()
            .filter(|tier| tier.started)
            .map(|tier| Arc::clone(&tier.trackers))
            .collect();
        if tiers.is_empty() {
            return None;
        }
        let requests: Vec<AnnounceRequest> = entry
            .announce_hashes
            .iter()
            .map(|hash| {
                request(
                    *hash,
                    entry.peer_id,
                    self.port,
                    entry.uploaded(),
                    AnnounceEvent::Stopped,
                    entry.private,
                )
            })
            .collect();
        Some((tiers, requests))
    }
}

/// The tiers of a torrent that heard `started`, and the `stopped`
/// announces for them.
type Stopped = (Vec<Arc<TrackerClient>>, Vec<AnnounceRequest>);

/// Sends `request` to each of `tiers` at once, and waits for them.
fn announce_tiers(tiers: &[Arc<TrackerClient>], request: &AnnounceRequest, timeout: Duration) {
    std::thread::scope(|scope| {
        for tier in tiers {
            scope.spawn(move || {
                let _ = tier.announce(request, timeout);
            });
        }
    });
}

/// Announces every public seeded torrent to the DHT on a fixed interval,
/// several lookups at a time.
fn dht_loop(shared: &Shared, dht: &Arc<Dht>, port: u16) {
    let mut seeded: Vec<Target> = Vec::new();
    let mut tracked: Option<(Arc<Runtime>, u64)> = None;
    let mut next: HashMap<[u8; 20], Instant> = HashMap::new();
    let mut in_flight: std::collections::HashSet<[u8; 20]> = std::collections::HashSet::new();
    let (sender, finished) = mpsc::channel::<[u8; 20]>();
    while !shared.shutdown.load(Ordering::Acquire) {
        let runtime = shared
            .runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.clone());
        if !unchanged(&runtime, &tracked) {
            // BEP 27: private torrents are never announced to the DHT.
            seeded = runtime
                .as_deref()
                .map(targets)
                .unwrap_or_default()
                .into_iter()
                .filter(|target| !target.private)
                .collect();
            let keys: std::collections::HashSet<[u8; 20]> =
                seeded.iter().map(|target| target.key).collect();
            next.retain(|key, _| keys.contains(key));
            if let Ok(mut announced) = shared.dht_announced.lock() {
                announced.retain(|key, _| keys.contains(key));
            }
            tracked = runtime.map(|runtime| {
                let revision = runtime.revision();
                (runtime, revision)
            });
        }
        if let Ok(mut forced) = shared.reannounce_dht.lock() {
            for key in forced.drain() {
                next.remove(&key);
            }
        }
        while let Ok(key) = finished.try_recv() {
            in_flight.remove(&key);
            next.insert(key, Instant::now() + jittered(DHT_INTERVAL));
            if let Ok(mut announced) = shared.dht_announced.lock() {
                announced.insert(key, Instant::now());
            }
        }
        let now = Instant::now();
        for target in &seeded {
            if in_flight.len() >= DHT_WORKERS {
                break;
            }
            if in_flight.contains(&target.key) || next.get(&target.key).is_some_and(|at| now < *at)
            {
                continue;
            }
            in_flight.insert(target.key);
            let (dht, sender, key, hashes) = (
                Arc::clone(dht),
                sender.clone(),
                target.key,
                target.announce_hashes.clone(),
            );
            let runtime = tracked.as_ref().map(|(runtime, _)| Arc::clone(runtime));
            std::thread::spawn(move || {
                for hash in &hashes {
                    let peers = dht.announce(hash, port);
                    if let Some(runtime) = &runtime {
                        dial_peers(runtime, &key, peers.iter());
                    }
                }
                let _ = sender.send(key);
            });
        }
        std::thread::sleep(POLL);
    }
}

/// Peers dialed at most per announce; the runtime's own limits on dials
/// and connections apply too.
const DIALS_PER_ANNOUNCE: usize = 50;

/// Connects to peers a tracker or the DHT returned for a torrent we seed.
/// Ones already connected, banned, or beyond the limits are skipped.
fn dial_peers<'a>(
    runtime: &Arc<Runtime>,
    key: &[u8; 20],
    peers: impl Iterator<Item = &'a std::net::SocketAddr>,
) {
    let mut dialed = 0;
    for peer in peers {
        if dialed >= DIALS_PER_ANNOUNCE {
            break;
        }
        if runtime.dial(*peer, key) {
            dialed += 1;
        }
    }
}

fn request(
    announce_hash: [u8; 20],
    peer_id: [u8; 20],
    port: u16,
    uploaded: u64,
    event: AnnounceEvent,
    private: bool,
) -> AnnounceRequest {
    AnnounceRequest {
        info_hash: announce_hash,
        peer_id,
        port,
        uploaded,
        downloaded: 0,
        left: 0,
        corrupt: 0,
        compact: true,
        event,
        private,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// Serves tracker announces, forwarding each request (line and
    /// headers).
    fn recording_tracker() -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..size]).into_owned();
                let body = b"d8:completei3e10:incompletei1e8:intervali900e5:peers0:e";
                let _ = stream.write_all(
                    format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                );
                let _ = stream.write_all(body);
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        (url, receiver)
    }

    /// A runtime seeding one small torrent announced at `url`, with
    /// `peer_id`, in `root`.
    fn seeding(url: &str, root: &std::path::Path, peer_id: [u8; 20]) -> Arc<Runtime> {
        let storage = crate::Storage::new(root).unwrap();
        let data = b"seeded";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let metainfo = format!(
            "d8:announce{}:{url}4:infod6:lengthi6e4:name6:seeded12:piece lengthi6e6:pieces20:",
            url.len()
        );
        let mut bytes = metainfo.into_bytes();
        bytes.extend(hash);
        bytes.extend(b"ee");
        let info = crate::TorrentInfo::from_bytes(&bytes).unwrap();
        storage.write_piece(&info, 0, data).unwrap();
        let seeder = crate::Seeder::new(info, storage, peer_id, Duration::from_secs(1)).unwrap();
        Arc::new(Runtime::new(vec![seeder], 1).unwrap())
    }

    #[test]
    fn announces_with_each_torrents_peer_id_and_matching_user_agent() {
        for (peer_id, user_agent) in [
            (crate::generate_peer_id(), crate::USER_AGENT),
            (
                crate::generate_anonymous_peer_id(),
                crate::ANONYMOUS_USER_AGENT,
            ),
        ] {
            let (url, requests) = recording_tracker();
            let root = std::env::temp_dir().join(format!(
                "empirebt-announcer-agent-{}-{}",
                std::process::id(),
                peer_id[0]
            ));
            let mut announcer = SeedAnnouncer::start(7777, Duration::from_secs(1), None);
            announcer.track(seeding(&url, &root, peer_id));
            let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            let peer_id = std::str::from_utf8(&peer_id).unwrap();
            assert!(request.contains(&format!("peer_id={peer_id}")), "{request}");
            assert!(
                request.contains(&format!("User-Agent: {user_agent}\r\n")),
                "{request}"
            );
            announcer.clear();
            if let Some(thread) = announcer.stop() {
                thread.join().unwrap();
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn reannounces_a_seed_at_once_when_asked() {
        let (url, requests) = recording_tracker();
        let root =
            std::env::temp_dir().join(format!("empirebt-announcer-again-{}", std::process::id()));
        let runtime = seeding(&url, &root, crate::generate_peer_id());
        let hash = *runtime.seeding_torrents()[0].info_hash();
        let mut announcer = SeedAnnouncer::start(7777, Duration::from_secs(1), None);
        assert!(!announcer.reannounce(&hash), "nothing is tracked yet");
        announcer.track(runtime);
        let started = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(started.contains("event=started"));
        // The tracker asked for 900 s; asking again does not wait for it.
        assert!(
            requests.recv_timeout(Duration::from_millis(500)).is_err(),
            "no announce before it is due"
        );
        assert!(announcer.reannounce(&hash));
        let again = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!again.contains("event="), "a regular announce: {again}");
        assert!(!announcer.reannounce(&[9; 20]));
        announcer.clear();
        if let Some(thread) = announcer.stop() {
            thread.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_seed_dials_the_downloaders_its_tracker_returns() {
        // A downloader that only listens: it cannot reach us, we reach it.
        let downloader = TcpListener::bind("127.0.0.1:0").unwrap();
        let downloader_address = downloader.local_addr().unwrap();
        let tracker = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", tracker.local_addr().unwrap());
        std::thread::spawn(move || {
            let std::net::SocketAddr::V4(peer) = downloader_address else {
                return;
            };
            for mut stream in tracker.incoming().flatten() {
                let _ = stream.read(&mut [0; 2048]);
                let mut body = b"d8:intervali900e5:peers6:".to_vec();
                body.extend(peer.ip().octets());
                body.extend(peer.port().to_be_bytes());
                body.push(b'e');
                let _ = stream.write_all(&[b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &body].concat());
            }
        });
        let root =
            std::env::temp_dir().join(format!("empirebt-announcer-dial-{}", std::process::id()));
        let runtime = seeding(&url, &root, crate::generate_peer_id());
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let serving = {
            let (runtime, stop) = (Arc::clone(&runtime), Arc::clone(&stop));
            std::thread::spawn(move || runtime.serve_until(&listener, &stop))
        };
        let mut announcer = SeedAnnouncer::start(7777, Duration::from_secs(2), None);
        announcer.track(Arc::clone(&runtime));
        downloader
            .set_nonblocking(false)
            .and_then(|()| {
                let (mut stream, _) = downloader.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                // Its handshake, encrypted by default.
                let mut opening = [0; 20];
                stream.read_exact(&mut opening)?;
                Ok(())
            })
            .expect("the seed connected to the downloader");
        announcer.clear();
        if let Some(thread) = announcer.stop() {
            thread.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        let _ = serving.join();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_finished_downloads_uploads_are_not_reported_twice() {
        // The download reported 500 bytes; seeding starts from there.
        let mut entry = Entry {
            announce_hashes: Vec::new(),
            peer_id: [0; 20],
            tiers: Vec::new(),
            private: false,
            uploaded_before: 0,
            uploaded_now: 500,
            uploaded_base: 500,
        };
        assert_eq!(entry.uploaded(), 0);
        entry.uploaded_now = 800;
        assert_eq!(entry.uploaded(), 300);
        // A new runtime counts from zero; nothing is lost or repeated.
        entry.uploaded_before += entry.uploaded_now;
        entry.uploaded_now = 50;
        assert_eq!(entry.uploaded(), 350);
    }

    /// Each tier keeps a schedule of its own, as libtorrent does (its
    /// f969a0d fixed one shared timer): a tier that failed is asked again
    /// soon, still with `started`, rather than when the working tier is
    /// next due half an hour later; both then count in the status, and
    /// both hear `stopped`.
    #[test]
    fn a_failing_tier_is_retried_on_its_own_schedule() {
        use std::sync::atomic::AtomicUsize;
        // Tier 0 refuses its first announce, then works.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let flaky = format!("http://{}/announce", listener.local_addr().unwrap());
        let (sender, flaky_requests) = mpsc::channel::<String>();
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut request = [0; 2048];
                    let size = stream.read(&mut request).unwrap_or(0);
                    let line = String::from_utf8_lossy(&request[..size]).into_owned();
                    let body: &[u8] = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        b"d14:failure reason4:downe"
                    } else {
                        b"d8:completei5e10:incompletei2e8:intervali1800e5:peers0:e"
                    };
                    let _ = stream.write_all(
                        format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                            .as_bytes(),
                    );
                    let _ = stream.write_all(body);
                    if sender.send(line).is_err() {
                        return;
                    }
                }
            });
        }
        let (steady, steady_requests) = recording_tracker();
        let root =
            std::env::temp_dir().join(format!("empirebt-announcer-tiers-{}", std::process::id()));
        let storage = crate::Storage::new(&root).unwrap();
        let data = b"seeded";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let list = format!("ll{}:{flaky}el{}:{steady}ee", flaky.len(), steady.len());
        let mut bytes = format!(
            "d8:announce{}:{flaky}13:announce-list{list}4:infod6:lengthi6e4:name6:seeded12:piece lengthi6e6:pieces20:",
            flaky.len()
        )
        .into_bytes();
        bytes.extend(hash);
        bytes.extend(b"ee");
        let info = crate::TorrentInfo::from_bytes(&bytes).unwrap();
        storage.write_piece(&info, 0, data).unwrap();
        let seeder =
            crate::Seeder::new(info.clone(), storage, [9; 20], Duration::from_secs(1)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 1).unwrap());

        let mut announcer = SeedAnnouncer::start(7777, Duration::from_secs(1), None);
        announcer.track(runtime);
        let first = steady_requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(first.contains("event=started"), "{first}");
        let refused = flaky_requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(refused.contains("event=started"), "{refused}");
        // The retry comes on tier 0's own backoff, and still says started:
        // it never heard it.
        let retried = flaky_requests
            .recv_timeout(Duration::from_secs(5))
            .expect("the failing tier was not asked again on its own schedule");
        assert!(retried.contains("event=started"), "{retried}");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = announcer.status(info.info_hash())
                && status.trackers_ok == 2
            {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "{:?}",
                announcer.status(info.info_hash())
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.trackers_total, 2);
        assert_eq!((status.seeders, status.leechers), (Some(5), Some(2)));
        assert_eq!(status.last_error, None);
        // The working tier was not asked again meanwhile.
        assert!(steady_requests.try_recv().is_err());

        announcer.clear();
        let stopped = steady_requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(stopped.contains("event=stopped"), "{stopped}");
        let stopped = flaky_requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(stopped.contains("event=stopped"), "{stopped}");
        if let Some(thread) = announcer.stop() {
            thread.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn announces_started_and_stopped_for_seeded_torrents() {
        let (url, requests) = recording_tracker();
        let root = std::env::temp_dir().join(format!("empirebt-announcer-{}", std::process::id()));
        let storage = crate::Storage::new(&root).unwrap();
        let data = b"seeded";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let metainfo = format!(
            "d8:announce{}:{url}4:infod6:lengthi6e4:name6:seeded12:piece lengthi6e6:pieces20:",
            url.len()
        );
        let mut bytes = metainfo.into_bytes();
        bytes.extend(hash);
        bytes.extend(b"ee");
        let info = crate::TorrentInfo::from_bytes(&bytes).unwrap();
        storage.write_piece(&info, 0, data).unwrap();
        let seeder =
            crate::Seeder::new(info.clone(), storage, [9; 20], Duration::from_secs(1)).unwrap();
        let runtime = Arc::new(Runtime::new(vec![seeder], 1).unwrap());

        let mut announcer = SeedAnnouncer::start(7777, Duration::from_secs(1), None);
        announcer.track(runtime);
        let started = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(started.contains("event=started"), "{started}");
        assert!(started.contains("left=0"));
        assert!(started.contains("port=7777"));

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = announcer.status(info.info_hash()) {
                break status;
            }
            assert!(Instant::now() < deadline, "no announce status");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!((status.trackers_ok, status.trackers_total), (1, 1));
        assert_eq!((status.seeders, status.leechers), (Some(3), Some(1)));

        announcer.clear();
        let stopped = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(stopped.contains("event=stopped"), "{stopped}");
        if let Some(thread) = announcer.stop() {
            thread.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// Quitting with many seeds: `stopped` goes out a few dozen at a time,
    /// never a thread and connection per torrent at once, and a tracker
    /// that never answers holds the stop up for the deadline at most.
    #[test]
    fn leaves_many_swarms_a_few_at_a_time_and_within_the_deadline() {
        use std::sync::atomic::AtomicUsize;
        const TORRENTS: usize = 100;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let (started, stopping, peak) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        {
            let (started, stopping, peak) = (
                Arc::clone(&started),
                Arc::clone(&stopping),
                Arc::clone(&peak),
            );
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let (started, stopping, peak) = (
                        Arc::clone(&started),
                        Arc::clone(&stopping),
                        Arc::clone(&peak),
                    );
                    std::thread::spawn(move || {
                        let mut request = [0; 2048];
                        let size = stream.read(&mut request).unwrap_or(0);
                        let line = String::from_utf8_lossy(&request[..size]).into_owned();
                        if line.contains("event=stopped") {
                            // Never answered: the announcer must not wait.
                            let now = stopping.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_secs(30));
                            return;
                        }
                        let body = b"d8:intervali900e5:peers0:e";
                        let _ = stream.write_all(
                            format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                                .as_bytes(),
                        );
                        let _ = stream.write_all(body);
                        started.fetch_add(1, Ordering::SeqCst);
                    });
                }
            });
        }
        let root =
            std::env::temp_dir().join(format!("empirebt-announcer-many-{}", std::process::id()));
        let storage = crate::Storage::new(&root).unwrap();
        let seeders: Vec<crate::Seeder> = (0..TORRENTS)
            .map(|index| {
                let data = format!("seeded {index:04}").into_bytes();
                let hash: [u8; 20] = {
                    use sha1::{Digest, Sha1};
                    Sha1::digest(&data).into()
                };
                let name = format!("seeded-{index:04}");
                let metainfo = format!(
                    "d8:announce{}:{url}4:infod6:lengthi{}e4:name{}:{name}12:piece lengthi16384e6:pieces20:",
                    url.len(),
                    data.len(),
                    name.len()
                );
                let mut bytes = metainfo.into_bytes();
                bytes.extend(hash);
                bytes.extend(b"ee");
                let info = crate::TorrentInfo::from_bytes(&bytes).unwrap();
                storage.write_piece(&info, 0, &data).unwrap();
                crate::Seeder::new(info, storage.clone(), [9; 20], Duration::from_secs(1)).unwrap()
            })
            .collect();
        let runtime = Arc::new(Runtime::new(seeders, 1).unwrap());
        // A long tracker timeout: only the deadline can end the stop early.
        let mut announcer = SeedAnnouncer::start(7777, Duration::from_secs(30), None);
        announcer.track(runtime);
        let deadline = Instant::now() + Duration::from_secs(20);
        while started.load(Ordering::SeqCst) < TORRENTS {
            assert!(Instant::now() < deadline, "not every torrent was announced");
            std::thread::sleep(Duration::from_millis(20));
        }

        let stopping_at = Instant::now();
        if let Some(thread) = announcer.stop() {
            thread.join().unwrap();
        }
        let took = stopping_at.elapsed();
        assert!(
            took < STOP_DEADLINE + Duration::from_secs(2),
            "took {took:?}"
        );
        let peak = peak.load(Ordering::SeqCst);
        assert!(
            (1..=TRACKER_WORKERS).contains(&peak),
            "{peak} stopped announces at once"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
