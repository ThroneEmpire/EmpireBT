//! Each torrent's history and totals, kept across runs: when it was added
//! and completed, the payload downloaded and uploaded, and how long it has
//! been active and seeding, as qBittorrent keeps them (and private
//! trackers' users watch). Also the totals over every torrent ever, which
//! go on after a torrent is removed.
//!
//! They are saved in a small text file of their own, beside the session:
//!
//! ```text
//! EmpireBT stats 1
//! total<TAB><downloaded><TAB><uploaded>
//! torrent<TAB><info hash in hex><TAB><added><TAB><completed><TAB><downloaded><TAB><uploaded><TAB><active><TAB><seeding>
//! ```
//!
//! Times are Unix seconds, 0 when unknown; byte counts and durations
//! (in seconds) are whole numbers.

use crate::InfoHash;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEADER: &str = "EmpireBT stats 1";
/// The highest ratio shown, as qBittorrent's `MAX_RATIO`: a torrent with
/// uploads and no downloads (one we made) would otherwise be infinite.
pub const MAX_RATIO: f64 = 9999.0;

/// One torrent's history and totals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TorrentStats {
    /// When it was added, in Unix seconds; 0 when unknown.
    pub added: u64,
    /// When it finished downloading; 0 when it has not, or unknown.
    pub completed: u64,
    /// Payload received from peers, over every run.
    pub downloaded: u64,
    /// Payload sent to peers, over every run.
    pub uploaded: u64,
    /// Time it was running (downloading or seeding).
    pub active: Duration,
    /// Time it was seeding.
    pub seeding: Duration,
    /// Payload received and sent this run only.
    pub downloaded_session: u64,
    pub uploaded_session: u64,
}

impl TorrentStats {
    /// Uploaded over downloaded, as qBittorrent's `realRatio`: a torrent
    /// whose downloads went unrecorded (its data came with it, or an older
    /// version ran it) counts the data it has as downloaded, unless that is
    /// under 1% of it. At most [`MAX_RATIO`].
    pub fn ratio(&self, have: u64) -> f64 {
        let downloaded = if (self.downloaded as f64) < have as f64 * 0.01 {
            have
        } else {
            self.downloaded
        };
        if downloaded == 0 {
            return if self.uploaded == 0 { 0.0 } else { MAX_RATIO };
        }
        (self.uploaded as f64 / downloaded as f64).min(MAX_RATIO)
    }
}

/// Every torrent's [`TorrentStats`], and the totals over all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    torrents: HashMap<InfoHash, TorrentStats>,
    /// Payload received and sent by every torrent, removed ones included.
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    /// This run's share of the totals.
    pub session_downloaded: u64,
    pub session_uploaded: u64,
    /// Changed since the last save.
    dirty: bool,
}

/// Now, in Unix seconds.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

impl Stats {
    pub fn of(&self, hash: &InfoHash) -> Option<&TorrentStats> {
        self.torrents.get(hash)
    }

    fn entry(&mut self, hash: InfoHash) -> &mut TorrentStats {
        self.dirty = true;
        self.torrents.entry(hash).or_default()
    }

    /// Records that a torrent was added at `when`, unless it already has
    /// a date.
    pub fn added(&mut self, hash: InfoHash, when: u64) {
        let stats = self.entry(hash);
        if stats.added == 0 {
            stats.added = when;
        }
    }

    /// Records that a torrent finished at `when`, unless it already had.
    pub fn completed(&mut self, hash: InfoHash, when: u64) {
        let stats = self.entry(hash);
        if stats.completed == 0 {
            stats.completed = when;
        }
    }

    /// Counts payload a torrent received and sent.
    pub fn traffic(&mut self, hash: InfoHash, downloaded: u64, uploaded: u64) {
        if downloaded == 0 && uploaded == 0 {
            return;
        }
        let stats = self.entry(hash);
        stats.downloaded = stats.downloaded.saturating_add(downloaded);
        stats.uploaded = stats.uploaded.saturating_add(uploaded);
        stats.downloaded_session = stats.downloaded_session.saturating_add(downloaded);
        stats.uploaded_session = stats.uploaded_session.saturating_add(uploaded);
        self.total_downloaded = self.total_downloaded.saturating_add(downloaded);
        self.total_uploaded = self.total_uploaded.saturating_add(uploaded);
        self.session_downloaded = self.session_downloaded.saturating_add(downloaded);
        self.session_uploaded = self.session_uploaded.saturating_add(uploaded);
    }

    /// Counts time a torrent was running, and of that, seeding.
    pub fn time(&mut self, hash: InfoHash, elapsed: Duration, seeding: bool) {
        // Only whole seconds reach the file, so this is no reason to save.
        let dirty = self.dirty;
        let stats = self.entry(hash);
        stats.active += elapsed;
        if seeding {
            stats.seeding += elapsed;
        }
        self.dirty = dirty || elapsed >= Duration::from_secs(1);
    }

    /// Takes a torrent's history from another client, as an import brings
    /// it over: it replaces what was known, and the totals count it.
    pub fn import(&mut self, hash: InfoHash, stats: TorrentStats) {
        self.total_downloaded = self.total_downloaded.saturating_add(stats.downloaded);
        self.total_uploaded = self.total_uploaded.saturating_add(stats.uploaded);
        *self.entry(hash) = TorrentStats {
            downloaded_session: 0,
            uploaded_session: 0,
            ..stats
        };
    }

    /// Forgets a removed torrent; the totals keep what it moved.
    pub fn forget(&mut self, hash: &InfoHash) {
        if self.torrents.remove(hash).is_some() {
            self.dirty = true;
        }
    }

    /// Moves a torrent's history to its new hash (a v2-only magnet link,
    /// once its metadata arrives).
    pub fn rename(&mut self, from: &InfoHash, to: InfoHash) {
        if let Some(stats) = self.torrents.remove(from) {
            self.torrents.insert(to, stats);
            self.dirty = true;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Reads the file; a missing one is empty. Lines it does not know (from
    /// a later version) are skipped.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!("{} is not an EmpireBT stats file", path.display()));
        }
        let mut stats = Self::default();
        for line in lines {
            let fields: Vec<&str> = line.split('\t').collect();
            let number = |index: usize| {
                fields
                    .get(index)
                    .and_then(|field| field.parse::<u64>().ok())
            };
            match fields.first() {
                Some(&"total") => {
                    stats.total_downloaded = number(1).unwrap_or(0);
                    stats.total_uploaded = number(2).unwrap_or(0);
                }
                Some(&"torrent") => {
                    let Some(hash) = fields.get(1).and_then(|hex| unhex(hex)) else {
                        continue;
                    };
                    stats.torrents.insert(
                        hash,
                        TorrentStats {
                            added: number(2).unwrap_or(0),
                            completed: number(3).unwrap_or(0),
                            downloaded: number(4).unwrap_or(0),
                            uploaded: number(5).unwrap_or(0),
                            active: Duration::from_secs(number(6).unwrap_or(0)),
                            seeding: Duration::from_secs(number(7).unwrap_or(0)),
                            ..TorrentStats::default()
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(stats)
    }

    /// Writes the file atomically, so a crash never leaves it truncated.
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        let mut text = format!(
            "{HEADER}\ntotal\t{}\t{}\n",
            self.total_downloaded, self.total_uploaded
        );
        let mut torrents: Vec<_> = self.torrents.iter().collect();
        torrents.sort_by_key(|(hash, _)| **hash);
        for (hash, stats) in torrents {
            text.push_str(&format!(
                "torrent\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                hex(hash),
                stats.added,
                stats.completed,
                stats.downloaded,
                stats.uploaded,
                stats.active.as_secs(),
                stats.seeding.as_secs()
            ));
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|error| format!("cannot save {}: {error}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<InfoHash> {
    if text.len() != 40 {
        return None;
    }
    let mut hash = [0; 20];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_history_and_totals_across_saves() {
        let path = std::env::temp_dir().join(format!("empirebt-stats-{}", std::process::id()));
        let mut stats = Stats::default();
        stats.added([1; 20], 1_700_000_000);
        stats.added([1; 20], 1_800_000_000);
        stats.traffic([1; 20], 1000, 2500);
        stats.time([1; 20], Duration::from_secs(90), false);
        stats.time([1; 20], Duration::from_secs(30), true);
        stats.completed([1; 20], 1_700_000_500);
        stats.traffic([2; 20], 10, 0);
        stats.forget(&[2; 20]);
        stats.save(&path).unwrap();
        assert!(!stats.is_dirty());

        let loaded = Stats::load(&path).unwrap();
        let torrent = loaded.of(&[1; 20]).unwrap();
        assert_eq!(torrent.added, 1_700_000_000, "the first date stays");
        assert_eq!(torrent.completed, 1_700_000_500);
        assert_eq!((torrent.downloaded, torrent.uploaded), (1000, 2500));
        assert_eq!(torrent.active, Duration::from_secs(120));
        assert_eq!(torrent.seeding, Duration::from_secs(30));
        assert_eq!(
            (torrent.downloaded_session, torrent.uploaded_session),
            (0, 0),
            "a new run starts its own counts"
        );
        assert!(loaded.of(&[2; 20]).is_none());
        assert_eq!(
            (loaded.total_downloaded, loaded.total_uploaded),
            (1010, 2500),
            "totals keep what removed torrents moved"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn works_out_the_ratio_as_qbittorrent_does() {
        let stats = |downloaded, uploaded| TorrentStats {
            downloaded,
            uploaded,
            ..TorrentStats::default()
        };
        assert_eq!(stats(1000, 2500).ratio(1000), 2.5);
        assert_eq!(stats(0, 0).ratio(0), 0.0);
        // Its data came with it: what it has counts as downloaded.
        assert_eq!(stats(0, 500).ratio(1000), 0.5);
        // Nothing to divide by.
        assert_eq!(stats(0, 500).ratio(0), MAX_RATIO);
        assert_eq!(stats(1, 1_000_000_000).ratio(100), MAX_RATIO);
    }

    #[test]
    fn skips_lines_it_does_not_know() {
        let path =
            std::env::temp_dir().join(format!("empirebt-stats-lines-{}", std::process::id()));
        std::fs::write(
            &path,
            "EmpireBT stats 1\nsomething\tnew\ntorrent\tnot-a-hash\t1\ntorrent\t0101010101010101010101010101010101010101\t5\n",
        )
        .unwrap();
        let loaded = Stats::load(&path).unwrap();
        assert_eq!(loaded.of(&[1; 20]).unwrap().added, 5);
        std::fs::write(&path, "something else\n").unwrap();
        assert!(Stats::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
