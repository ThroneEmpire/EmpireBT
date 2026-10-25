use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What a background transfer is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    /// Hashing local data to find verified pieces.
    Checking,
    /// Contacting trackers for peers.
    Announcing,
    /// Exchanging pieces with peers.
    Downloading,
    /// Waiting for the next tracker announce interval.
    Waiting,
    /// Fetching torrent metadata from peers for a magnet link.
    Metadata,
    /// The transfer thread has stopped.
    Finished,
}

impl TransferPhase {
    fn to_byte(self) -> u8 {
        match self {
            Self::Checking => 0,
            Self::Announcing => 1,
            Self::Downloading => 2,
            Self::Waiting => 3,
            Self::Finished => 4,
            Self::Metadata => 5,
        }
    }

    fn from_byte(value: u8) -> Self {
        match value {
            0 => Self::Checking,
            1 => Self::Announcing,
            2 => Self::Downloading,
            3 => Self::Waiting,
            5 => Self::Metadata,
            _ => Self::Finished,
        }
    }
}

/// Live counters for one transfer, readable from any thread while the
/// transfer runs.
#[derive(Debug)]
pub struct TransferProgress {
    phase: AtomicU8,
    total_pieces: AtomicUsize,
    verified_pieces: AtomicUsize,
    total_bytes: AtomicU64,
    verified_bytes: AtomicU64,
    received_bytes: AtomicU64,
    corrupt_bytes: AtomicU64,
    uploaded_bytes: AtomicU64,
    active_peers: AtomicUsize,
    cancelled: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// Swarm counts from the latest tracker response; `UNKNOWN` until
    /// a tracker reports them.
    seeders: AtomicU64,
    leechers: AtomicU64,
    /// Which pieces are verified, most significant bit first, so the state
    /// can be saved as resume data.
    verified_bits: Mutex<Vec<u8>>,
    /// Whether a check of the local data ran to the end, so the verified
    /// pieces say something about every piece, not just those reached.
    checked: AtomicBool,
}

const UNKNOWN: u64 = u64::MAX;

/// Delay before re-announcing after a tracker request fails outright.
pub(crate) const ANNOUNCE_FAILURE_DELAY: Duration = Duration::from_secs(15);

impl Default for TransferProgress {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(TransferPhase::Checking.to_byte()),
            total_pieces: AtomicUsize::new(0),
            verified_pieces: AtomicUsize::new(0),
            total_bytes: AtomicU64::new(0),
            verified_bytes: AtomicU64::new(0),
            received_bytes: AtomicU64::new(0),
            corrupt_bytes: AtomicU64::new(0),
            uploaded_bytes: AtomicU64::new(0),
            active_peers: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            last_error: Mutex::new(None),
            seeders: AtomicU64::new(UNKNOWN),
            leechers: AtomicU64::new(UNKNOWN),
            verified_bits: Mutex::new(Vec::new()),
            checked: AtomicBool::new(false),
        }
    }
}

impl TransferProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> TransferPhase {
        TransferPhase::from_byte(self.phase.load(Ordering::Acquire))
    }

    pub fn total_pieces(&self) -> usize {
        self.total_pieces.load(Ordering::Acquire)
    }

    /// Pieces present on disk and hash-verified.
    pub fn verified_pieces(&self) -> usize {
        self.verified_pieces.load(Ordering::Acquire)
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Acquire)
    }

    /// Bytes covered by verified pieces.
    pub fn verified_bytes(&self) -> u64 {
        self.verified_bytes.load(Ordering::Acquire)
    }

    /// Raw block payload bytes received from peers, including pieces that
    /// later fail verification. Suitable for rate display.
    pub fn received_bytes(&self) -> u64 {
        self.received_bytes.load(Ordering::Acquire)
    }

    /// Bytes of pieces that failed verification, which trackers are told
    /// as `corrupt`.
    pub fn corrupt_bytes(&self) -> u64 {
        self.corrupt_bytes.load(Ordering::Acquire)
    }

    /// Block payload bytes sent to peers.
    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes.load(Ordering::Acquire)
    }

    /// Peers with an open connection for this transfer.
    pub fn active_peers(&self) -> usize {
        self.active_peers.load(Ordering::Acquire)
    }

    /// Fraction of verified bytes in `0.0..=1.0`.
    pub fn fraction(&self) -> f64 {
        let total = self.total_bytes();
        if total == 0 {
            return 0.0;
        }
        (self.verified_bytes() as f64 / total as f64).clamp(0.0, 1.0)
    }

    /// Asks the transfer to stop. Workers notice at their next message or
    /// piece boundary, so stopping can take up to the peer timeout.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// The most recent recoverable error, such as a failed announce or a
    /// dropped peer, while the transfer keeps retrying.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|error| error.clone())
    }

    /// Seeders and leechers in the swarm, as last reported by a tracker.
    pub fn swarm(&self) -> (Option<u64>, Option<u64>) {
        let read =
            |value: &AtomicU64| Some(value.load(Ordering::Acquire)).filter(|v| *v != UNKNOWN);
        (read(&self.seeders), read(&self.leechers))
    }

    pub(crate) fn set_swarm(&self, response: &crate::TrackerResponse) {
        if let Some(seeders) = response.seeders() {
            self.seeders.store(seeders, Ordering::Release);
        }
        if let Some(leechers) = response.leechers() {
            self.leechers.store(leechers, Ordering::Release);
        }
    }

    pub(crate) fn record_error(&self, error: &crate::BackendError) {
        if let Ok(mut last) = self.last_error.lock() {
            *last = Some(error.to_string());
        }
    }

    pub(crate) fn set_phase(&self, phase: TransferPhase) {
        self.phase.store(phase.to_byte(), Ordering::Release);
    }

    pub(crate) fn set_totals(&self, pieces: usize, bytes: u64) {
        self.total_pieces.store(pieces, Ordering::Release);
        self.total_bytes.store(bytes, Ordering::Release);
        if let Ok(mut bits) = self.verified_bits.lock() {
            bits.resize(pieces.div_ceil(8), 0);
        }
    }

    /// Clears verified state before a fresh check.
    pub(crate) fn clear_verified(&self) {
        self.checked.store(false, Ordering::Release);
        self.verified_pieces.store(0, Ordering::Release);
        self.verified_bytes.store(0, Ordering::Release);
        if let Ok(mut bits) = self.verified_bits.lock() {
            bits.fill(0);
        }
    }

    /// Records that a check of the local data finished.
    pub(crate) fn mark_checked(&self) {
        self.checked.store(true, Ordering::Release);
    }

    /// Whether a check of the local data finished: only then do the
    /// verified pieces describe the whole torrent. A check cancelled part
    /// way knows nothing about the pieces it never reached.
    pub fn checked(&self) -> bool {
        self.checked.load(Ordering::Acquire)
    }

    /// Marks one piece verified. Counting happens only the first time.
    pub(crate) fn add_verified(&self, index: usize, bytes: u64) {
        let Ok(mut bits) = self.verified_bits.lock() else {
            return;
        };
        let (byte, mask) = (index / 8, 0x80_u8 >> (index % 8));
        if byte >= bits.len() {
            bits.resize(byte + 1, 0);
        }
        if bits[byte] & mask != 0 {
            return;
        }
        bits[byte] |= mask;
        self.verified_pieces.fetch_add(1, Ordering::AcqRel);
        self.verified_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    /// Verified pieces as a bitfield (most significant bit first).
    pub fn verified_bitfield(&self) -> Vec<u8> {
        self.verified_bits
            .lock()
            .map(|bits| bits.clone())
            .unwrap_or_default()
    }

    pub(crate) fn add_uploaded(&self, bytes: u64) {
        self.uploaded_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    pub(crate) fn add_corrupt(&self, bytes: u64) {
        self.corrupt_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    pub(crate) fn add_received(&self, bytes: u64) {
        self.received_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    pub(crate) fn add_peer(&self) {
        self.active_peers.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn remove_peer(&self) {
        self.active_peers.fetch_sub(1, Ordering::AcqRel);
    }

    /// Returns `Cancelled` when cancellation has been requested.
    pub(crate) fn check_cancelled(&self) -> Result<(), crate::BackendError> {
        if self.is_cancelled() {
            return Err(crate::BackendError::Cancelled);
        }
        Ok(())
    }

    /// Sleeps for `duration`, waking early when cancellation is requested.
    pub(crate) fn sleep(&self, duration: Duration) -> Result<(), crate::BackendError> {
        let deadline = Instant::now() + duration;
        loop {
            self.check_cancelled()?;
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
        }
    }
}

/// What a background transfer was started to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    /// Hash local data without contacting peers.
    Verify,
    /// Discover peers and download missing pieces.
    Download,
    /// Fetch a magnet link's metadata from peers.
    Metadata,
}

/// A transfer running on a background thread. Pass it back to
/// [`crate::Client::finish_transfer`] once [`TransferHandle::is_finished`]
/// reports true to collect the result and update the torrent state.
#[derive(Debug)]
pub struct TransferHandle {
    info_hash: [u8; 20],
    kind: TransferKind,
    progress: Arc<TransferProgress>,
    thread: JoinHandle<Result<usize, crate::BackendError>>,
    /// Data the transfer produced, such as fetched metadata.
    output: Arc<Mutex<Option<Vec<u8>>>>,
}

impl TransferHandle {
    pub(crate) fn spawn(
        info_hash: [u8; 20],
        kind: TransferKind,
        progress: Arc<TransferProgress>,
        work: impl FnOnce(&Mutex<Option<Vec<u8>>>) -> Result<usize, crate::BackendError>
        + Send
        + 'static,
    ) -> Self {
        let finished = Arc::clone(&progress);
        let output = Arc::new(Mutex::new(None));
        let produced = Arc::clone(&output);
        let thread = std::thread::spawn(move || {
            let result = work(&produced);
            finished.set_phase(TransferPhase::Finished);
            result
        });
        Self {
            info_hash,
            kind,
            progress,
            thread,
            output,
        }
    }

    /// The v1 info hash of the torrent this transfer belongs to.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    pub fn kind(&self) -> TransferKind {
        self.kind
    }

    pub fn progress(&self) -> &Arc<TransferProgress> {
        &self.progress
    }

    pub fn cancel(&self) {
        self.progress.cancel();
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Waits for the thread and returns its result and any output.
    pub(crate) fn join(self) -> (Result<usize, crate::BackendError>, Option<Vec<u8>>) {
        let result = self.thread.join().unwrap_or_else(|_| {
            Err(crate::BackendError::Io(
                "transfer thread panicked".to_owned(),
            ))
        });
        let output = self.output.lock().ok().and_then(|mut output| output.take());
        (result, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_connected_peers() {
        let progress = TransferProgress::new();
        progress.add_peer();
        progress.add_peer();
        assert_eq!(progress.active_peers(), 2);
        progress.remove_peer();
        progress.remove_peer();
        assert_eq!(progress.active_peers(), 0);
    }

    #[test]
    fn cancelled_sleep_returns_early() {
        let progress = TransferProgress::new();
        progress.cancel();
        let started = Instant::now();
        assert_eq!(
            progress.sleep(Duration::from_secs(30)),
            Err(crate::BackendError::Cancelled)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn fraction_handles_empty_totals() {
        let progress = TransferProgress::new();
        assert_eq!(progress.fraction(), 0.0);
        progress.set_totals(4, 400);
        progress.add_verified(2, 100);
        progress.add_verified(2, 100);
        assert_eq!(progress.verified_pieces(), 1, "a piece counts once");
        assert_eq!(progress.verified_bitfield(), [0b0010_0000]);
        assert!((progress.fraction() - 0.25).abs() < f64::EPSILON);
    }
}
