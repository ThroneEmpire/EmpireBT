//! Keeps trackers informed about torrents being seeded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{AnnounceEvent, AnnounceRequest, Dht, Runtime, TrackerClient};

/// Regular re-announce interval when trackers do not specify one.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MIN_INTERVAL: Duration = Duration::from_secs(60);
const MAX_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// First retry delay after every tracker failed; doubles up to the maximum.
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
const POLL: Duration = Duration::from_millis(250);
/// DHT peers expire after about 30 minutes, so announces repeat sooner.
const DHT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Tracker announces running at once. Each can wait the full timeout on a
/// dead tracker, so many must overlap to keep thousands of torrents current.
const TRACKER_WORKERS: usize = 32;
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
    pub fn start(peer_id: [u8; 20], port: u16, timeout: Duration, dht: Option<Arc<Dht>>) -> Self {
        Self::start_on(crate::Binding::any(), peer_id, port, timeout, dht)
    }

    /// Like [`Self::start`], announcing to trackers through one network
    /// interface only.
    pub fn start_on(
        binding: crate::Binding,
        peer_id: [u8; 20],
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
                peer_id,
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
    trackers: Arc<TrackerClient>,
    next_announce: Instant,
    started: bool,
    in_flight: bool,
    failures: u32,
    /// Bytes uploaded by runtimes that have since been replaced.
    uploaded_before: u64,
    /// Latest counter value from the current runtime.
    uploaded_now: u64,
}

impl Entry {
    fn uploaded(&self) -> u64 {
        self.uploaded_before + self.uploaded_now
    }
}

/// One finished tracker announce, sent back from a worker thread.
struct Outcome {
    key: [u8; 20],
    results: Vec<Result<crate::TrackerResponse, crate::BackendError>>,
}

/// A seeded torrent as the announce loops see it.
struct Target {
    key: [u8; 20],
    announce_hashes: Vec<[u8; 20]>,
    trackers: Vec<String>,
    private: bool,
}

/// Lists the runtime's complete torrents. Called only when the runtime or
/// its revision changes, so polling never copies metadata. Downloads
/// announce themselves until they complete.
fn targets(runtime: &Runtime) -> Vec<Target> {
    runtime
        .seeding_torrents()
        .iter()
        .map(|info| Target {
            key: *info.info_hash(),
            announce_hashes: info.swarm_hashes(),
            trackers: info.tracker_urls().to_vec(),
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
    peer_id: [u8; 20],
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
        // Leave every swarm; stop announces go out concurrently.
        let leaving: Vec<[u8; 20]> = self.entries.keys().copied().collect();
        let mut senders = Vec::new();
        for key in leaving {
            senders.extend(self.remove(&key));
        }
        for sender in senders {
            let _ = sender.join();
        }
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
            for target in current {
                if self.entries.contains_key(&target.key) {
                    continue;
                }
                // Torrents without usable trackers are not announced to
                // trackers (the DHT loop still covers public ones).
                let Ok(trackers) = TrackerClient::from_urls(target.trackers)
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
                        trackers: Arc::new(trackers),
                        next_announce: Instant::now(),
                        started: false,
                        in_flight: false,
                        failures: 0,
                        uploaded_before: 0,
                        uploaded_now: 0,
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

    /// Starts announces for due torrents, up to the worker limit.
    fn announce_due(&mut self) {
        let now = Instant::now();
        let mut due: Vec<([u8; 20], Instant)> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.in_flight && entry.next_announce <= now)
            .map(|(key, entry)| (*key, entry.next_announce))
            .collect();
        // Longest-waiting first, so no torrent starves behind others.
        due.sort_by_key(|(_, at)| *at);
        for (key, _) in due {
            if self.in_flight >= TRACKER_WORKERS {
                return;
            }
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            let event = if entry.started {
                AnnounceEvent::None
            } else {
                AnnounceEvent::Started
            };
            let requests: Vec<AnnounceRequest> = entry
                .announce_hashes
                .iter()
                .map(|hash| request(*hash, self.peer_id, self.port, entry.uploaded(), event))
                .collect();
            let (trackers, timeout, sender) = (
                Arc::clone(&entry.trackers),
                self.timeout,
                self.outcomes.0.clone(),
            );
            entry.in_flight = true;
            self.in_flight += 1;
            std::thread::spawn(move || {
                let results = trackers.announce_all(&requests[0], timeout);
                // A hybrid's other hash, so peers searching by it find us.
                for request in &requests[1..] {
                    let _ = trackers.announce_all(request, timeout);
                }
                let _ = sender.send(Outcome { key, results });
            });
        }
    }

    fn collect_outcomes(&mut self) {
        while let Ok(Outcome { key, results }) = self.outcomes.1.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            // The torrent may have stopped seeding while the announce ran.
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            entry.in_flight = false;
            let accepted: Vec<_> = results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .collect();
            let last_error = results
                .iter()
                .find_map(|result| result.as_ref().err())
                .map(ToString::to_string);
            if accepted.is_empty() {
                let backoff = FAILURE_BACKOFF
                    .saturating_mul(2_u32.saturating_pow(entry.failures))
                    .min(MAX_FAILURE_BACKOFF);
                entry.failures = entry.failures.saturating_add(1);
                entry.next_announce = Instant::now() + jittered(backoff);
            } else {
                entry.started = true;
                entry.failures = 0;
                let interval = accepted
                    .iter()
                    .filter_map(|response| response.interval())
                    .min()
                    .map_or(DEFAULT_INTERVAL, Duration::from_secs)
                    .clamp(MIN_INTERVAL, MAX_INTERVAL);
                entry.next_announce = Instant::now() + jittered(interval);
            }
            let status = AnnounceStatus {
                trackers_ok: accepted.len(),
                trackers_total: results.len(),
                seeders: accepted
                    .iter()
                    .filter_map(|response| response.seeders())
                    .max(),
                leechers: accepted
                    .iter()
                    .filter_map(|response| response.leechers())
                    .max(),
                last_error,
                next_announce: entry.next_announce,
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
    fn remove(&mut self, key: &[u8; 20]) -> Option<JoinHandle<()>> {
        if let Ok(mut statuses) = self.shared.status.lock() {
            statuses.remove(key);
        }
        let entry = self.entries.remove(key)?;
        if !entry.started {
            return None;
        }
        let requests: Vec<AnnounceRequest> = entry
            .announce_hashes
            .iter()
            .map(|hash| {
                request(
                    *hash,
                    self.peer_id,
                    self.port,
                    entry.uploaded(),
                    AnnounceEvent::Stopped,
                )
            })
            .collect();
        let timeout = self.timeout;
        Some(std::thread::spawn(move || {
            for request in &requests {
                let _ = entry.trackers.announce_all(request, timeout);
            }
        }))
    }
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
            std::thread::spawn(move || {
                for hash in &hashes {
                    dht.announce(hash, port);
                }
                let _ = sender.send(key);
            });
        }
        std::thread::sleep(POLL);
    }
}

fn request(
    announce_hash: [u8; 20],
    peer_id: [u8; 20],
    port: u16,
    uploaded: u64,
    event: AnnounceEvent,
) -> AnnounceRequest {
    AnnounceRequest {
        info_hash: announce_hash,
        peer_id,
        port,
        uploaded,
        downloaded: 0,
        left: 0,
        compact: true,
        event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// Serves tracker announces, forwarding each request line.
    fn recording_tracker() -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..size])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
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

        let mut announcer = SeedAnnouncer::start([9; 20], 7777, Duration::from_secs(1), None);
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
}
