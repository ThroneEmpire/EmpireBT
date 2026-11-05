//! What interfaces draw from: an immutable view of the application.

use crate::InfoHash;
use empirebt_backend::{AnnounceStatus, Setting, TorrentInfo};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// The whole application at one moment. Cheap to share: interfaces hold it
/// through an `Arc` while they draw.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Increases with every published snapshot.
    pub generation: u64,
    /// Every torrent, in the order they were added.
    pub torrents: Vec<TorrentView>,
    /// Total download and upload rates, in bytes per second.
    pub totals: (f64, f64),
    /// Payload downloaded and uploaded by every torrent ever (removed ones
    /// too), and this run alone.
    pub transferred: Transferred,
    /// Download and upload limits in bytes per second; 0 means unlimited.
    pub rate_limits: (u64, u64),
    /// Torrents downloading now.
    pub active_count: usize,
    pub seeding_count: usize,
    pub network: Network,
    pub download_dir: PathBuf,
    /// Folders the web UI may save torrents in (and folders inside them):
    /// the download directory, `storage.allowed_dirs`, category folders,
    /// and `storage.incomplete_dir`.
    pub allowed_dirs: Vec<PathBuf>,
    /// `storage.incomplete_dir`, when set: where new torrents download
    /// unless told otherwise.
    pub incomplete_dir: Option<PathBuf>,
    /// The folder each category saves new torrents in, where it has one.
    pub category_folders: std::collections::BTreeMap<String, PathBuf>,
    /// The latest notices, oldest first.
    pub notices: Vec<Notice>,
    /// Every setting with its current value.
    pub settings: Vec<SettingView>,
    /// Where the settings are saved.
    pub config_path: PathBuf,
    /// Every category and tag, sorted, whether or not a torrent uses it.
    pub categories: Vec<String>,
    pub tags: Vec<String>,
}

impl Snapshot {
    pub fn torrent(&self, hash: &InfoHash) -> Option<&TorrentView> {
        self.torrents.iter().find(|torrent| torrent.hash == *hash)
    }

    pub fn position(&self, hash: &InfoHash) -> Option<usize> {
        self.torrents
            .iter()
            .position(|torrent| torrent.hash == *hash)
    }
}

/// The state of the connections that every torrent shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub listen_port: u16,
    pub seed_status: SeedStatus,
    /// Peers connected now, downloading or seeding.
    pub seeding_peers: usize,
    pub dht: DhtState,
    pub dht_nodes: Option<usize>,
    /// Of `dht_nodes`, those reached over IPv6.
    pub dht_ipv6_nodes: usize,
    /// Whether peers can connect over uTP as well as TCP.
    pub utp: bool,
    pub lsd: bool,
    /// The network interface connections are bound to, if one was chosen,
    /// and whether it is up.
    pub interface: Option<String>,
    pub interface_up: bool,
}

/// Why the DHT is or is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DhtState {
    Disabled,
    Failed(String),
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedStatus {
    Idle,
    Starting,
    Listening { torrents: usize },
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Error,
}

/// Something worth telling the user, such as a finished download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Increases with every notice, so interfaces can tell new ones.
    pub id: u64,
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

/// A setting and its current value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingView {
    pub setting: Setting,
    pub value: String,
    /// A command-line flag overrides it for this run.
    pub overridden: bool,
}

/// What a torrent is doing right now, combining the saved lifecycle state
/// with any running transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Metadata,
    Checking,
    Announcing,
    Downloading,
    Waiting,
    Retrying,
    Pausing,
    /// Its files are moving to another folder.
    Moving,
    Seeding,
    Paused,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Metadata => "Metadata",
            Self::Checking => "Checking",
            Self::Announcing => "Announcing",
            Self::Downloading => "Downloading",
            Self::Waiting => "Waiting",
            Self::Retrying => "Retrying",
            Self::Pausing => "Pausing…",
            Self::Moving => "Moving",
            Self::Seeding => "Seeding",
            Self::Paused => "Paused",
            Self::Error => "Error",
        }
    }
}

/// Everything shown for one torrent.
#[derive(Debug, Clone)]
pub struct TorrentView {
    pub hash: InfoHash,
    /// The name shown: the one chosen for it, or else its own.
    pub name: String,
    /// The torrent's own name.
    pub original_name: String,
    /// Files with names on disk other than the torrent's: its path to the
    /// one used instead.
    pub renames: Arc<BTreeMap<PathBuf, PathBuf>>,
    /// The folder its files are in.
    pub save_dir: PathBuf,
    /// Downloading in the incomplete folder (`save_dir`): the folder it
    /// moves to once complete.
    pub complete_dir: Option<PathBuf>,
    /// Its files are moving to another folder.
    pub moving: Option<MoveView>,
    /// Metadata, which a magnet link only has once peers provided it.
    pub info: Option<TorrentInfo>,
    pub trackers: Vec<String>,
    pub status: Status,
    /// Verified fraction, unknown until local data has been checked.
    pub fraction: Option<f64>,
    pub verified_pieces: Option<usize>,
    pub down_rate: f64,
    pub up_rate: f64,
    pub peers: Option<usize>,
    /// Seeders and leechers reported by trackers.
    pub swarm: (Option<u64>, Option<u64>),
    /// How many finished downloading it, from tracker scrapes.
    pub completed: Option<u64>,
    /// Tracker announce state while seeding.
    pub announce: Option<AnnounceStatus>,
    pub error: Option<String>,
    pub category: Option<String>,
    /// Sorted.
    pub tags: Vec<String>,
    /// When it was added and completed, its totals, and its time active
    /// and seeding.
    pub history: crate::TorrentStats,
    /// Uploaded over downloaded, as qBittorrent shows it.
    pub ratio: f64,
}

/// A move of a torrent's files under way.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveView {
    pub to: PathBuf,
    /// How much has moved, by size; 0 while the torrent stops for it.
    pub fraction: f64,
}

impl TorrentView {
    /// The folder its files belong in: where they are, or for a torrent
    /// downloading in the incomplete folder, where they go once complete
    /// (qBittorrent's save path).
    pub fn final_dir(&self) -> &std::path::Path {
        self.complete_dir.as_deref().unwrap_or(&self.save_dir)
    }
}

/// Payload moved, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transferred {
    pub downloaded: u64,
    pub uploaded: u64,
    pub downloaded_session: u64,
    pub uploaded_session: u64,
}

/// A file of a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    /// Its path in the torrent.
    pub original: PathBuf,
    /// Its path on disk, relative to the torrent's folder.
    pub path: PathBuf,
    pub length: u64,
}

impl TorrentView {
    /// Its files, padding left out, with their paths on disk; empty until
    /// a magnet link's metadata arrives.
    pub fn files(&self) -> Vec<FileView> {
        let Some(info) = &self.info else {
            return Vec::new();
        };
        info.files()
            .iter()
            .filter(|file| !file.is_padding())
            .map(|file| FileView {
                original: file.path().to_owned(),
                path: self
                    .renames
                    .get(file.path())
                    .cloned()
                    .unwrap_or_else(|| file.path().to_owned()),
                length: file.length(),
            })
            .collect()
    }

    pub fn size(&self) -> Option<u64> {
        self.info.as_ref().map(TorrentInfo::total_length)
    }

    pub fn remaining(&self) -> u64 {
        let total = self.size().unwrap_or(0);
        let done = self
            .fraction
            .map_or(0.0, |fraction| fraction * total as f64);
        total.saturating_sub(done as u64)
    }
}

/// A connected peer and its country.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerView {
    pub info: empirebt_backend::PeerInfo,
    /// ISO 3166 code, such as `DE`, once the country database is loaded.
    pub country: Option<[u8; 2]>,
}

impl PeerView {
    pub fn country_code(&self) -> Option<&str> {
        self.country
            .as_ref()
            .and_then(|code| std::str::from_utf8(code).ok())
    }
}

/// Exponentially smoothed rate of a monotonically growing byte counter.
#[derive(Debug, Default)]
pub struct RateMeter {
    last: Option<u64>,
    rate: f64,
}

impl RateMeter {
    pub(crate) fn sample(&mut self, total: u64, elapsed: f64) {
        if let Some(last) = self.last
            && elapsed > 0.0
        {
            let instant = total.saturating_sub(last) as f64 / elapsed;
            self.rate = 0.35 * instant + 0.65 * self.rate;
            if self.rate < 1.0 {
                self.rate = 0.0;
            }
        }
        self.last = Some(total);
    }

    pub(crate) fn idle(&mut self) {
        *self = Self::default();
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }
}
