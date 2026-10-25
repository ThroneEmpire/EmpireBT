//! Limits shared by every connection of a client: how many peer
//! connections may be open at once, and how fast data may be uploaded and
//! downloaded in total.
//!
//! Rates are enforced where bytes meet sockets, in the event loop: writes
//! take upload tokens and reads take download tokens from a token bucket.
//! A connection that finds its bucket empty waits and retries shortly. Not
//! reading from a TCP socket fills the peer's send window, so a download
//! limit slows the sending peers rather than buffering data here.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Tokens a bucket may hold, in seconds of its rate. A short burst keeps a
/// limit smooth; one block's worth at least avoids many tiny writes.
const BURST_SECONDS: f64 = 0.1;
const MIN_BURST: f64 = 16.0 * 1024.0;

/// Connection and rate limits, shared through an `Arc` by a client, its
/// runtimes, and its downloads. Every limit can change while running.
#[derive(Debug)]
pub struct Limits {
    max_connections: AtomicUsize,
    open: AtomicUsize,
    upload: Bucket,
    download: Bucket,
}

impl Default for Limits {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl Limits {
    /// No connection or rate limit.
    pub fn unlimited() -> Self {
        Self {
            max_connections: AtomicUsize::new(usize::MAX),
            open: AtomicUsize::new(0),
            upload: Bucket::new(0),
            download: Bucket::new(0),
        }
    }

    /// Limits from a client configuration.
    pub fn from_config(config: &crate::ClientConfig) -> Self {
        let limits = Self::unlimited();
        limits.set_max_connections(config.max_connections());
        limits.set_upload_rate(config.upload_limit());
        limits.set_download_rate(config.download_limit());
        limits
    }

    /// Peer connections allowed open at once, across every torrent,
    /// incoming and outgoing.
    pub fn set_max_connections(&self, connections: usize) {
        self.max_connections.store(connections, Ordering::Relaxed);
    }

    pub fn max_connections(&self) -> usize {
        self.max_connections.load(Ordering::Relaxed)
    }

    /// Peer connections open now, across every runtime using these limits.
    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }

    /// Whether another connection fits, with `pending` more already being
    /// dialed.
    pub fn has_room(&self, pending: usize) -> bool {
        self.open_connections().saturating_add(pending) < self.max_connections()
    }

    /// Total upload rate in bytes per second; 0 means unlimited.
    pub fn set_upload_rate(&self, bytes_per_second: u64) {
        self.upload.rate.store(bytes_per_second, Ordering::Relaxed);
    }

    pub fn upload_rate(&self) -> u64 {
        self.upload.rate.load(Ordering::Relaxed)
    }

    /// Total download rate in bytes per second; 0 means unlimited.
    pub fn set_download_rate(&self, bytes_per_second: u64) {
        self.download
            .rate
            .store(bytes_per_second, Ordering::Relaxed);
    }

    pub fn download_rate(&self) -> u64 {
        self.download.rate.load(Ordering::Relaxed)
    }

    /// Counts one open connection until the returned slot is dropped.
    pub(crate) fn open_slot(self: &Arc<Self>) -> Slot {
        self.open.fetch_add(1, Ordering::Relaxed);
        Slot(Arc::clone(self))
    }

    /// Up to `wanted` bytes that may be written now.
    pub(crate) fn take_upload(&self, wanted: usize) -> usize {
        self.upload.take(wanted, Instant::now())
    }

    /// Returns bytes taken for writing but not written.
    pub(crate) fn refund_upload(&self, bytes: usize) {
        self.upload.refund(bytes);
    }

    /// Up to `wanted` bytes that may be read now.
    pub(crate) fn take_download(&self, wanted: usize) -> usize {
        self.download.take(wanted, Instant::now())
    }

    /// Returns bytes taken for reading but not read.
    pub(crate) fn refund_download(&self, bytes: usize) {
        self.download.refund(bytes);
    }
}

/// One open connection, counted against [`Limits::max_connections`].
#[derive(Debug)]
pub(crate) struct Slot(Arc<Limits>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Bucket {
    /// Bytes per second; 0 means unlimited.
    rate: AtomicU64,
    state: Mutex<BucketState>,
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    refilled: Instant,
}

impl Bucket {
    fn new(rate: u64) -> Self {
        Self {
            rate: AtomicU64::new(rate),
            state: Mutex::new(BucketState {
                tokens: 0.0,
                refilled: Instant::now(),
            }),
        }
    }

    fn take(&self, wanted: usize, now: Instant) -> usize {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return wanted;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let rate = rate as f64;
        let burst = (rate * BURST_SECONDS).max(MIN_BURST);
        let elapsed = now.saturating_duration_since(state.refilled).as_secs_f64();
        state.tokens = (state.tokens + elapsed * rate).min(burst);
        state.refilled = now;
        let granted = (state.tokens.max(0.0) as usize).min(wanted);
        state.tokens -= granted as f64;
        granted
    }

    fn refund(&self, bytes: usize) {
        if bytes == 0 || self.rate.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.tokens += bytes as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unlimited_buckets_grant_everything() {
        let limits = Limits::unlimited();
        assert_eq!(limits.take_upload(1 << 30), 1 << 30);
        assert_eq!(limits.take_download(1 << 30), 1 << 30);
    }

    #[test]
    fn a_bucket_grants_its_rate_over_time() {
        let bucket = Bucket::new(100_000);
        let start = Instant::now();
        bucket.state.lock().unwrap().refilled = start;
        let mut granted = 0;
        for step in 1..=100 {
            granted += bucket.take(usize::MAX, start + Duration::from_millis(step * 10));
        }
        // One second at 100 kB/s.
        assert!((99_000..=101_000).contains(&granted), "{granted}");
    }

    #[test]
    fn a_bucket_holds_only_a_short_burst() {
        let bucket = Bucket::new(1_000_000);
        let start = Instant::now();
        bucket.state.lock().unwrap().refilled = start;
        // Idle for a minute, then only 100 ms worth is available.
        let granted = bucket.take(usize::MAX, start + Duration::from_secs(60));
        assert_eq!(granted, 100_000);
        assert_eq!(bucket.take(usize::MAX, start + Duration::from_secs(60)), 0);
        bucket.refund(500);
        assert_eq!(
            bucket.take(usize::MAX, start + Duration::from_secs(60)),
            500
        );
    }

    #[test]
    fn slots_count_open_connections() {
        let limits = Arc::new(Limits::unlimited());
        limits.set_max_connections(2);
        let first = limits.open_slot();
        assert!(limits.has_room(0));
        assert!(!limits.has_room(1));
        let second = limits.open_slot();
        assert!(!limits.has_room(0));
        drop((first, second));
        assert_eq!(limits.open_connections(), 0);
    }
}
