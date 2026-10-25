//! Announcing one swarm to every tracker a torrent lists.
//!
//! Torrents usually list several trackers, often run by different people
//! and knowing different peers, so we announce to all of them (as
//! qBittorrent does by default) instead of stopping at the first that
//! answers. Each tracker is announced to on a short-lived thread of its
//! own and keeps its own interval, so a slow or dead tracker never holds
//! up the others, and the peers each one returns are handed over the
//! moment it replies.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{AnnounceEvent, AnnounceRequest, BackendError, TrackerClient, TrackerResponse};

/// Re-announce interval when a tracker does not specify one.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Upper bound on a tracker-requested interval.
const MAX_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Retry delay after a failed announce.
const FAILURE_DELAY: Duration = crate::progress::ANNOUNCE_FAILURE_DELAY;

/// Called with the peers from each successful announce, on the thread that
/// made it.
pub(crate) type OnPeers = Arc<dyn Fn(&[SocketAddr]) + Send + Sync>;

/// One tracker's schedule.
#[derive(Debug)]
/// One tier of trackers (BEP 12), announced to as one: its trackers in
/// order until one accepts.
struct Schedule {
    tracker: TrackerClient,
    next: Instant,
    in_flight: bool,
    /// Whether it has accepted our `started` event.
    started: bool,
    /// Whether any announce to it has finished, successfully or not.
    answered: bool,
}

/// A finished announce to one tracker, for every hash of the swarm.
struct Reply {
    tracker: usize,
    /// The primary hash's result decides the schedule.
    primary: Result<TrackerResponse, BackendError>,
}

/// What finished announces reported, for the download to show.
#[derive(Debug, Default)]
pub(crate) struct Collected {
    /// The largest swarm a tracker reported.
    pub best: Option<TrackerResponse>,
    /// Why announcing failed, when no tracker has succeeded yet.
    pub error: Option<BackendError>,
}

pub(crate) struct Announcer {
    schedules: Vec<Schedule>,
    replies: Arc<Mutex<Vec<Reply>>>,
    on_peers: OnPeers,
    timeout: Duration,
    /// Caps tracker-requested intervals, for callers that want fresh peers
    /// more often (metadata for a magnet link).
    max_interval: Duration,
    succeeded: bool,
}

impl Announcer {
    pub fn new(trackers: &TrackerClient, timeout: Duration, on_peers: OnPeers) -> Self {
        let now = Instant::now();
        Self {
            schedules: trackers
                .tiers()
                .into_iter()
                .map(|tracker| Schedule {
                    tracker,
                    next: now,
                    in_flight: false,
                    started: false,
                    answered: false,
                })
                .collect(),
            replies: Arc::default(),
            on_peers,
            timeout,
            max_interval: MAX_INTERVAL,
            succeeded: false,
        }
    }

    pub fn with_max_interval(mut self, interval: Duration) -> Self {
        self.max_interval = interval;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.schedules.is_empty()
    }

    /// Whether some tracker has answered successfully.
    pub fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// Whether every tracker has answered at least once.
    pub fn all_answered(&self) -> bool {
        self.schedules.iter().all(|schedule| schedule.answered)
    }

    /// When the next announce is due, unless every tracker is busy.
    pub fn next_due(&self) -> Option<Instant> {
        self.schedules
            .iter()
            .filter(|schedule| !schedule.in_flight)
            .map(|schedule| schedule.next)
            .min()
    }

    /// Starts an announce to every tracker that is due, without waiting.
    /// `request` builds the request for one of the swarm's `hashes`; the
    /// first hash is the primary one, whose replies set the schedule.
    pub fn start_due(
        &mut self,
        hashes: &[[u8; 20]],
        request: impl Fn([u8; 20], AnnounceEvent) -> AnnounceRequest,
    ) {
        let now = Instant::now();
        for (index, schedule) in self.schedules.iter_mut().enumerate() {
            if schedule.in_flight || now < schedule.next {
                continue;
            }
            schedule.in_flight = true;
            let event = if schedule.started {
                AnnounceEvent::None
            } else {
                AnnounceEvent::Started
            };
            let requests: Vec<AnnounceRequest> =
                hashes.iter().map(|hash| request(*hash, event)).collect();
            let tracker = schedule.tracker.clone();
            let (replies, on_peers, timeout) = (
                Arc::clone(&self.replies),
                Arc::clone(&self.on_peers),
                self.timeout,
            );
            std::thread::spawn(move || {
                let mut primary = None;
                for request in &requests {
                    let result = tracker.announce(request, timeout);
                    if let Ok(response) = &result {
                        on_peers(response.peers());
                    }
                    primary.get_or_insert(result);
                }
                let primary = primary
                    .unwrap_or_else(|| Err(BackendError::Tracker("no hash to announce".into())));
                lock(&replies).push(Reply {
                    tracker: index,
                    primary,
                });
            });
        }
    }

    /// Takes in finished announces and schedules each tracker's next one.
    pub fn collect(&mut self) -> Collected {
        let replies = std::mem::take(&mut *lock(&self.replies));
        let mut collected = Collected::default();
        let now = Instant::now();
        for reply in replies {
            let schedule = &mut self.schedules[reply.tracker];
            schedule.in_flight = false;
            schedule.answered = true;
            match reply.primary {
                Ok(response) => {
                    schedule.started = true;
                    self.succeeded = true;
                    let interval = response
                        .min_interval()
                        .or(response.interval())
                        .map_or(DEFAULT_INTERVAL, Duration::from_secs)
                        .clamp(Duration::from_secs(1), self.max_interval);
                    schedule.next = now + interval;
                    let size = |response: &TrackerResponse| {
                        response.seeders().unwrap_or(0) + response.leechers().unwrap_or(0)
                    };
                    if collected
                        .best
                        .as_ref()
                        .is_none_or(|best| size(&response) > size(best))
                    {
                        collected.best = Some(response);
                    }
                }
                Err(error) => {
                    schedule.next = now + FAILURE_DELAY;
                    collected.error = Some(error);
                }
            }
        }
        // A dead tracker among working ones is not worth reporting.
        if self.succeeded {
            collected.error = None;
        }
        collected
    }

    /// Tells every tracker that the download finished, in the background:
    /// trackers count completed downloads, but none of them should delay
    /// the switch to seeding.
    pub fn announce_completed(&self, requests: Vec<AnnounceRequest>) {
        let tiers: Vec<TrackerClient> = self
            .schedules
            .iter()
            .map(|schedule| schedule.tracker.clone())
            .collect();
        let timeout = self.timeout;
        std::thread::spawn(move || announce_each(&tiers, &requests, timeout));
    }

    /// Tells the trackers that know us that we are leaving, with the
    /// final counts, and waits for them briefly: quitting allows only a
    /// few seconds.
    pub fn announce_stopped(&self, requests: Vec<AnnounceRequest>) {
        let tiers: Vec<TrackerClient> = self
            .schedules
            .iter()
            // Including those whose answer to `started` is still on its way.
            .filter(|schedule| schedule.started || schedule.in_flight)
            .map(|schedule| schedule.tracker.clone())
            .collect();
        announce_each(&tiers, &requests, self.timeout.min(STOPPED_TIMEOUT));
    }
}

/// How long a `stopped` announce may take.
const STOPPED_TIMEOUT: Duration = Duration::from_secs(2);

/// Sends each of `requests` to every tier at once, and waits for them.
fn announce_each(tiers: &[TrackerClient], requests: &[AnnounceRequest], timeout: Duration) {
    std::thread::scope(|scope| {
        for tier in tiers {
            scope.spawn(move || {
                for request in requests {
                    let _ = tier.announce(request, timeout);
                }
            });
        }
    });
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// An HTTP tracker that answers every announce with `peer`, after
    /// `delay`, until the test ends.
    fn tracker(peer: [u8; 6], delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    let _ = stream.read(&mut [0; 2048]);
                    std::thread::sleep(delay);
                    let mut body = b"d8:intervali60e5:peers6:".to_vec();
                    body.extend(peer);
                    body.push(b'e');
                    let mut response = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
                    response.extend(body);
                    let _ = stream.write_all(&response);
                });
            }
        });
        format!("http://{address}/announce")
    }

    /// Accepts connections and never answers, like a dead tracker.
    fn silent_tracker() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let _held: Vec<_> = listener.incoming().collect();
        });
        format!("http://{address}/announce?silent")
    }

    fn request(hash: [u8; 20], event: AnnounceEvent) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: hash,
            peer_id: [1; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1,
            corrupt: 0,
            compact: true,
            event,
        }
    }

    #[test]
    fn gathers_peers_from_every_tracker_without_waiting_for_dead_ones() {
        let urls = vec![
            silent_tracker(),
            tracker([10, 0, 0, 1, 0x1a, 0xe1], Duration::ZERO),
            tracker([10, 0, 0, 2, 0x1a, 0xe1], Duration::from_millis(300)),
        ];
        let trackers = TrackerClient::from_urls(urls).unwrap();
        let found = Arc::new(Mutex::new(Vec::new()));
        let on_peers: OnPeers = {
            let found = Arc::clone(&found);
            Arc::new(move |peers| lock(&found).extend_from_slice(peers))
        };
        let mut announcer = Announcer::new(&trackers, Duration::from_secs(5), on_peers);
        let started = Instant::now();
        announcer.start_due(&[[7; 20]], request);
        while lock(&found).len() < 2 && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut found = lock(&found).clone();
        found.sort();
        assert_eq!(
            found,
            vec![
                "10.0.0.1:6881".parse().unwrap(),
                "10.0.0.2:6881".parse().unwrap()
            ]
        );
        // The silent tracker is still pending, and did not hold up the others.
        assert!(started.elapsed() < Duration::from_secs(2));
        let collected = announcer.collect();
        assert!(announcer.succeeded());
        assert!(!announcer.all_answered());
        assert!(collected.best.is_some());
        assert!(collected.error.is_none());
        // Working trackers wait for their interval; nothing is due again yet.
        assert!(announcer.next_due().unwrap() > Instant::now() + Duration::from_secs(50));
    }
}
