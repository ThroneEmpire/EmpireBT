use crate::{BackendError, ClientConfig, Dht, MagnetLink, Torrent, TorrentInfo};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Backend client shared by the console and future interfaces.
#[derive(Debug, Clone)]
pub struct Client {
    config: ClientConfig,
    torrents: Vec<Torrent>,
    dht: Option<Arc<Dht>>,
    lsd: Option<Arc<crate::Lsd>>,
    utp: Option<Arc<crate::UtpSocket>>,
    /// The shared event loop for every connection, once one is running.
    runtime: Option<Arc<crate::Runtime>>,
    /// Shared by every transfer and seeder, so they share one pool of open
    /// files.
    storage: crate::Storage,
    /// Torrents whose resume data changed since the session was last saved.
    resume_dirty: Arc<std::sync::Mutex<std::collections::HashSet<[u8; 20]>>>,
    /// Connection and rate limits for every connection of this client.
    limits: Arc<crate::Limits>,
    /// In anonymous mode, the peer ID public torrents use instead of the
    /// one they are given.
    anonymous_peer_id: Option<[u8; 20]>,
}

/// Clients compare by configuration and torrents; the DHT node is a
/// shared service, not part of a client's state.
impl PartialEq for Client {
    fn eq(&self, other: &Self) -> bool {
        self.config == other.config && self.torrents == other.torrents
    }
}

impl Eq for Client {}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Creates a client with the default configuration.
    pub fn new() -> Self {
        let config = ClientConfig::default();
        let storage = crate::Storage::new(config.download_directory())
            .expect("the default download directory is not empty");
        Self {
            torrents: Vec::new(),
            dht: None,
            lsd: None,
            utp: None,
            runtime: None,
            limits: Arc::new(crate::Limits::from_config(&config)),
            config,
            storage,
            resume_dirty: Arc::default(),
            anonymous_peer_id: None,
        }
    }

    /// Creates a client with a caller-provided configuration.
    pub fn with_config(config: ClientConfig) -> Result<Self, BackendError> {
        config.validate()?;
        let storage = crate::Storage::new(config.download_directory())?;
        Ok(Self {
            limits: Arc::new(crate::Limits::from_config(&config)),
            config,
            torrents: Vec::new(),
            dht: None,
            lsd: None,
            utp: None,
            runtime: None,
            storage,
            resume_dirty: Arc::default(),
            anonymous_peer_id: None,
        })
    }

    /// Anonymous mode: public torrents started from now on use `peer_id`
    /// (see [`crate::generate_anonymous_peer_id`]), and with it a generic
    /// User-Agent and no client name in their handshakes. Private
    /// torrents keep the peer ID they are given, as private trackers
    /// admit only clients they know. `None` ends anonymous mode.
    pub fn set_anonymous_peer_id(&mut self, peer_id: Option<[u8; 20]>) {
        self.anonymous_peer_id = peer_id;
    }

    /// The peer ID a torrent uses when given `peer_id`: the anonymous one
    /// in anonymous mode, unless the torrent is private. A magnet link
    /// without metadata cannot say, and is fetched anonymously.
    pub fn peer_id_for(&self, info: Option<&TorrentInfo>, peer_id: [u8; 20]) -> [u8; 20] {
        match self.anonymous_peer_id {
            Some(anonymous) if !info.is_some_and(TorrentInfo::is_private) => anonymous,
            _ => peer_id,
        }
    }

    /// Uses a DHT node to find peers for downloads and magnet links, and to
    /// announce torrents being downloaded.
    pub fn set_dht(&mut self, dht: Option<Arc<Dht>>) {
        self.dht = dht;
    }

    /// Finds peers on the local network (BEP 14) for public downloads and
    /// magnet links, and announces seeds of the shared runtime there.
    pub fn set_lsd(&mut self, lsd: Option<Arc<crate::Lsd>>) {
        if let Some(lsd) = &lsd {
            lsd.set_runtime(self.runtime.clone());
        }
        self.lsd = lsd;
    }

    pub fn lsd(&self) -> Option<&Arc<crate::Lsd>> {
        self.lsd.as_ref()
    }

    /// Uses a uTP socket for incoming uTP peers and, per the `utp`
    /// setting, outgoing connections.
    pub fn set_utp(&mut self, utp: Option<Arc<crate::UtpSocket>>) {
        self.utp = utp;
    }

    pub fn utp(&self) -> Option<&Arc<crate::UtpSocket>> {
        self.utp.as_ref()
    }

    /// How transfers reach peers, from the configuration.
    pub fn connect_options(&self) -> crate::ConnectOptions {
        let mode = self.config.utp();
        crate::ConnectOptions::new()
            .with_binding(self.config.binding())
            .with_encryption(self.config.encryption())
            .with_utp(
                self.utp
                    .clone()
                    .filter(|_| mode != crate::UtpMode::Disabled),
            )
            .prefer_utp(mode == crate::UtpMode::Preferred)
    }

    pub fn dht(&self) -> Option<&Arc<Dht>> {
        self.dht.as_ref()
    }

    /// Closes data files that no transfer or seeder has used for `idle`.
    /// Call periodically; files reopen on demand.
    pub fn close_idle_files(&self, idle: Duration) {
        self.storage.close_idle_files(idle);
    }

    /// Adds a torrent from a magnet link. Its metadata is fetched from peers
    /// by [`Client::spawn_metadata`] before it can download.
    pub fn add_magnet(&mut self, link: MagnetLink) -> usize {
        self.push(Torrent::from_magnet(link))
    }

    /// Adds a torrent saved in the download directory as it is now, so a
    /// later change of that setting leaves it where it is.
    fn push(&mut self, mut torrent: Torrent) -> usize {
        if torrent.save_dir().is_none() {
            torrent.set_save_dir(Some(self.config.download_directory().to_owned()));
        }
        self.torrents.push(torrent);
        self.torrents.len() - 1
    }

    /// The folder a torrent's files are in.
    pub fn save_dir(&self, index: usize) -> Option<&std::path::Path> {
        let torrent = self.torrents.get(index)?;
        Some(
            torrent
                .save_dir()
                .unwrap_or_else(|| self.config.download_directory()),
        )
    }

    /// Chooses the folder a torrent's files are in. Nothing is moved: call
    /// this before the torrent starts, or with its files already there.
    pub fn set_save_dir(
        &mut self,
        index: usize,
        dir: std::path::PathBuf,
    ) -> Result<(), BackendError> {
        if !dir.is_absolute() {
            return Err(BackendError::InvalidConfiguration(format!(
                "the save folder must be a full path, not {}",
                dir.display()
            )));
        }
        if dir.to_str().is_none() {
            return Err(BackendError::InvalidConfiguration(
                "the save folder's name must be valid text".to_owned(),
            ));
        }
        let torrent = self
            .torrents
            .get_mut(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        torrent.set_save_dir(Some(dir));
        if let Some(hash) = torrent.info_hash().copied()
            && let Ok(mut dirty) = self.resume_dirty.lock()
        {
            dirty.insert(hash);
        }
        Ok(())
    }

    /// Takes another client's word for which pieces are on disk, for an
    /// import: records them as resume data tied to the files as they are
    /// now (so later changes to the files still mean a full check), and
    /// sets the state: seeding when every piece is there and it was
    /// running, downloading when running, paused otherwise. Fails when the
    /// piece count does not match the torrent's.
    pub fn adopt_progress(
        &mut self,
        index: usize,
        verified: &[bool],
        running: bool,
    ) -> Result<(), BackendError> {
        let torrent = self
            .torrents
            .get(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        let info = torrent.info().ok_or_else(|| {
            BackendError::InvalidMetainfo("a torrent without metadata has no pieces".to_owned())
        })?;
        let pieces = if info.format() == crate::TorrentFormat::V2 {
            info.v2_piece_count()
        } else {
            info.piece_count()
        };
        if verified.len() != pieces {
            return Err(BackendError::InvalidMetainfo(format!(
                "the progress covers {} pieces, the torrent has {pieces}",
                verified.len()
            )));
        }
        let mut bitfield = vec![0u8; pieces.div_ceil(8)];
        for (piece, _) in verified
            .iter()
            .enumerate()
            .filter(|(_, verified)| **verified)
        {
            bitfield[piece / 8] |= 0x80 >> (piece % 8);
        }
        let resume = crate::ResumeData::capture(info, &self.storage_for(torrent), bitfield);
        let complete = verified.iter().all(|verified| *verified);
        let hash = torrent.info_hash().copied();
        let torrent = &mut self.torrents[index];
        torrent.set_resume(Some(resume));
        match (running, complete) {
            (true, true) => torrent.seed(),
            (true, false) => torrent.start(),
            (false, _) => torrent.pause(),
        }
        if let Some(hash) = hash
            && let Ok(mut dirty) = self.resume_dirty.lock()
        {
            dirty.insert(hash);
        }
        Ok(())
    }

    /// Storage rooted at a torrent's folder, finding its files under the
    /// names they have on disk.
    fn storage_for(&self, torrent: &Torrent) -> crate::Storage {
        let storage = match torrent.save_dir() {
            Some(dir) if dir != self.storage.root() => self.storage.at(dir),
            _ => self.storage.at(self.storage.root()),
        };
        storage.with_renames(torrent.renames().clone())
    }

    /// Chooses the name shown for a torrent; `None` shows its own again.
    /// Its files are not touched.
    pub fn rename_torrent(
        &mut self,
        index: usize,
        name: Option<String>,
    ) -> Result<(), BackendError> {
        let name = name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
        if let Some(name) = &name
            && (name.chars().any(char::is_control) || name.chars().count() > MAX_NAME)
        {
            return Err(BackendError::InvalidConfiguration(format!(
                "a torrent's name is at most {MAX_NAME} characters, without line breaks"
            )));
        }
        self.torrents
            .get_mut(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?
            .set_display_name(name);
        Ok(())
    }

    /// A torrent's files (padding left out), each with the torrent's path
    /// and the one it has on disk, relative to the torrent's folder.
    pub fn files(&self, index: usize) -> Vec<crate::TorrentFileName> {
        let Some(torrent) = self.torrents.get(index) else {
            return Vec::new();
        };
        let Some(info) = torrent.info() else {
            return Vec::new();
        };
        let renames = torrent.renames().table();
        info.files()
            .iter()
            .filter(|file| !file.is_padding())
            .map(|file| crate::TorrentFileName {
                original: file.path().to_owned(),
                path: renames
                    .get(file.path())
                    .cloned()
                    .unwrap_or_else(|| file.path().to_owned()),
                length: file.length(),
            })
            .collect()
    }

    /// Lays out a torrent's files before any are written: as the torrent
    /// says, inside a folder of its own, or without its top folder. A
    /// magnet link gets it once its metadata arrives.
    pub fn set_content_layout(
        &mut self,
        index: usize,
        layout: crate::ContentLayout,
    ) -> Result<(), BackendError> {
        let files = self.files(index);
        let torrent = self
            .torrents
            .get_mut(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        if torrent.info().is_none() {
            torrent.set_pending_layout(layout);
            return Ok(());
        }
        torrent.set_pending_layout(crate::ContentLayout::Original);
        let name = Path::new(torrent.name());
        let single = files.len() == 1 && files[0].original == name;
        let table: BTreeMap<PathBuf, PathBuf> = match layout {
            crate::ContentLayout::Original => BTreeMap::new(),
            // A single file goes in a folder named after it.
            crate::ContentLayout::Subfolder if single => {
                let folder = name
                    .file_stem()
                    .map_or_else(|| name.to_owned(), PathBuf::from);
                std::iter::once((name.to_owned(), folder.join(name))).collect()
            }
            crate::ContentLayout::Subfolder => BTreeMap::new(),
            crate::ContentLayout::NoSubfolder if single => BTreeMap::new(),
            // Files go straight in the save folder, without the top folder.
            crate::ContentLayout::NoSubfolder => files
                .iter()
                .filter_map(|file| {
                    let inside = file.original.strip_prefix(name).ok()?;
                    (!inside.as_os_str().is_empty())
                        .then(|| (file.original.clone(), inside.to_owned()))
                })
                .collect(),
        };
        torrent.renames().replace(table);
        Ok(())
    }

    /// Takes names on disk another client gave a torrent's files (for an
    /// import), without moving anything: the torrent's path to the one
    /// used instead. Each must stay inside the torrent's folder.
    pub fn adopt_names(
        &mut self,
        index: usize,
        table: BTreeMap<PathBuf, PathBuf>,
    ) -> Result<(), BackendError> {
        if let Some((_, path)) = table.iter().find(|(_, path)| !is_plain_relative(path)) {
            return Err(BackendError::InvalidConfiguration(format!(
                "{} is not a name inside the torrent's folder",
                path.display()
            )));
        }
        self.torrents
            .get(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?
            .renames()
            .replace(table);
        Ok(())
    }

    /// Renames a file or a folder of a torrent, by its path on disk
    /// relative to the torrent's folder, to `to`. Files already on disk
    /// are moved there (all of them, or none); the names apply at once to
    /// running transfers and seeding. Returns how many files moved.
    pub fn rename_path(
        &mut self,
        index: usize,
        from: &Path,
        to: &Path,
    ) -> Result<usize, BackendError> {
        let invalid = BackendError::InvalidConfiguration;
        // `a/./b` is `a/b`.
        let (from, to): (PathBuf, PathBuf) =
            (from.components().collect(), to.components().collect());
        let (from, to) = (from.as_path(), to.as_path());
        if !is_plain_relative(to) {
            return Err(invalid(format!(
                "{} is not a name inside the torrent's folder",
                to.display()
            )));
        }
        let files = self.files(index);
        let moving: Vec<(PathBuf, PathBuf, PathBuf)> = files
            .iter()
            .filter_map(|file| {
                let rest = file.path.strip_prefix(from).ok()?;
                let target = if rest.as_os_str().is_empty() {
                    to.to_owned()
                } else {
                    to.join(rest)
                };
                Some((file.original.clone(), file.path.clone(), target))
            })
            .collect();
        if moving.is_empty() {
            return Err(invalid(format!(
                "the torrent has no file or folder {}",
                from.display()
            )));
        }
        if moving.iter().all(|(_, path, target)| path == target) {
            return Ok(0);
        }
        // Two files may not end up with one name, nor one inside another.
        let staying: Vec<&Path> = files
            .iter()
            .filter(|file| {
                !moving
                    .iter()
                    .any(|(original, _, _)| *original == file.original)
            })
            .map(|file| file.path.as_path())
            .collect();
        for (_, _, target) in &moving {
            if staying
                .iter()
                .any(|path| path.starts_with(target) || target.starts_with(path))
            {
                return Err(invalid(format!(
                    "{} is already taken by another file of the torrent",
                    target.display()
                )));
            }
        }
        let torrent = self
            .torrents
            .get(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        let storage = self.storage_for(torrent);
        let root = storage.root().to_owned();
        // Move what is on disk, undoing it all if one move fails.
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
        let result = moving.iter().try_for_each(|(_, path, target)| {
            let (old, new) = (root.join(path), root.join(target));
            if !old.exists() {
                return Ok(());
            }
            if new.exists() {
                return Err(invalid(format!("{} already exists", new.display())));
            }
            if let Some(parent) = new.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| BackendError::Io(error.to_string()))?;
            }
            storage.forget(&old);
            std::fs::rename(&old, &new)
                .map_err(|error| BackendError::Io(format!("{}: {error}", old.display())))?;
            moved.push((old, new));
            Ok(())
        });
        if let Err(error) = result {
            for (old, new) in moved.iter().rev() {
                let _ = std::fs::rename(new, old);
            }
            return Err(error);
        }
        // Folders left empty by the move go too.
        for (old, _) in &moved {
            let mut folder = old.parent();
            while let Some(dir) = folder.filter(|dir| dir.starts_with(&root) && *dir != root) {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                folder = dir.parent();
            }
        }
        let mut table = (*torrent.renames().table()).clone();
        for (original, _, target) in &moving {
            if target == original {
                table.remove(original);
            } else {
                table.insert(original.clone(), target.clone());
            }
        }
        torrent.renames().replace(table);
        Ok(moved.len())
    }

    /// Where a torrent's files are on disk (padding left out): its folder,
    /// and each file's full path under it, renames followed.
    pub fn file_paths(&self, index: usize) -> (PathBuf, Vec<PathBuf>) {
        let Some(torrent) = self.torrents.get(index) else {
            return (PathBuf::new(), Vec::new());
        };
        let storage = self.storage_for(torrent);
        let paths = torrent
            .info()
            .map(|info| {
                info.files()
                    .iter()
                    .filter(|file| !file.is_padding())
                    .filter_map(|file| storage.path_for(file.path()))
                    .collect()
            })
            .unwrap_or_default();
        (storage.root().to_owned(), paths)
    }

    /// Closes open handles to `paths`, such as before deleting them, so
    /// their space is freed at once.
    pub fn release_files(&self, paths: &[PathBuf]) {
        for path in paths {
            self.storage.forget(path);
        }
    }

    fn storage_at(&self, index: usize) -> crate::Storage {
        match self.torrents.get(index) {
            Some(torrent) => self.storage_for(torrent),
            None => self.storage.clone(),
        }
    }

    /// Connection and rate limits shared by every connection of this
    /// client (and its clones). Changes apply at once.
    pub fn limits(&self) -> &Arc<crate::Limits> {
        &self.limits
    }

    /// Returns the configuration used by this client.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Uses `port` for transfers started from now on. The caller moves the
    /// listeners, the DHT, uTP, and local discovery to it.
    pub fn set_listen_port(&mut self, port: u16) {
        self.config = self.config.clone().with_listen_port(port);
    }

    /// Adds a torrent from a parsed metainfo file.
    pub fn add_torrent(&mut self, info: TorrentInfo) -> usize {
        self.push(Torrent::from_info(info))
    }

    /// Loads and adds a `.torrent` file.
    pub fn add_torrent_file(
        &mut self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<usize, BackendError> {
        let info = TorrentInfo::from_file(path)?;
        Ok(self.add_torrent(info))
    }

    /// Returns all torrents currently managed by this client.
    pub fn torrents(&self) -> &[Torrent] {
        &self.torrents
    }

    /// Builds the shared runtime, starting with every seeding torrent. It
    /// should be bound and served once, for the life of the client, and
    /// given to the client with [`Client::set_runtime`]; afterwards
    /// [`Client::sync_runtime`] keeps it up to date.
    ///
    /// A torrent only reaches the seeding state after its data was hashed
    /// and verified, so this trusts that verification and checks only that
    /// each file still exists with the right size. Rebuilding the runtime
    /// therefore costs no hashing, however much data is being seeded. Use
    /// [`Client::spawn_verify`] to re-check data that may have changed.
    pub fn seeding_runtime(
        &self,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<crate::Runtime, BackendError> {
        let mut v1_seeders = Vec::new();
        let mut v2_seeders = Vec::new();
        for torrent in &self.torrents {
            let Some(info) = torrent.info().cloned() else {
                continue;
            };
            let storage = self.storage_for(torrent);
            if !matches!(
                torrent.state(),
                crate::TransferState::Seeding | crate::TransferState::Complete
            ) || !storage.has_expected_files(&info)?
            {
                continue;
            }
            let peer_id = self.peer_id_for(Some(&info), peer_id);
            match info.format() {
                crate::TorrentFormat::V2 => v2_seeders.push(crate::V2Seeder::from_verified(
                    info,
                    storage.clone(),
                    peer_id,
                    timeout,
                )),
                crate::TorrentFormat::V1 | crate::TorrentFormat::Hybrid => v1_seeders.push(
                    crate::Seeder::from_verified(info, storage.clone(), peer_id, timeout),
                ),
            }
        }
        crate::Runtime::new_mixed(
            v1_seeders,
            v2_seeders,
            // The shared limits cap connections, and can change live.
            usize::MAX,
        )?
        .with_limits(Arc::clone(&self.limits))
        .with_binding(self.config.binding())
        .with_encryption(self.config.encryption())
        .with_utp(
            self.utp
                .clone()
                .filter(|_| self.config.utp() != crate::UtpMode::Disabled),
        )
        .with_handshake_timeout(timeout)
    }

    /// Uses `runtime` for every connection: downloads run in it, and
    /// finished downloads stay in it as seeds.
    pub fn set_runtime(&mut self, runtime: Option<Arc<crate::Runtime>>) {
        if let Some(lsd) = &self.lsd {
            lsd.set_runtime(runtime.clone());
        }
        self.runtime = runtime;
    }

    pub fn runtime(&self) -> Option<&Arc<crate::Runtime>> {
        self.runtime.as_ref()
    }

    /// Brings the shared runtime in line with the torrents' states:
    /// seeding torrents not yet in it join, and paused or removed ones
    /// leave. Downloads manage their own place in it. Connections of
    /// torrents that stay are untouched.
    pub fn sync_runtime(&self, peer_id: [u8; 20], timeout: Duration) -> Result<(), BackendError> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        let mut wanted = std::collections::HashSet::new();
        for torrent in &self.torrents {
            let Some(hash) = torrent.info_hash().copied() else {
                continue;
            };
            match torrent.state() {
                crate::TransferState::Paused => continue,
                crate::TransferState::Downloading => {
                    wanted.insert(hash);
                    continue;
                }
                crate::TransferState::Seeding | crate::TransferState::Complete => {}
            }
            let Some(info) = torrent.info().cloned() else {
                continue;
            };
            wanted.insert(hash);
            let storage = self.storage_for(torrent);
            if runtime.contains(&hash) || !storage.has_expected_files(&info)? {
                continue;
            }
            let peer_id = self.peer_id_for(Some(&info), peer_id);
            match info.format() {
                crate::TorrentFormat::V2 => runtime.add_v2_seeder(
                    crate::V2Seeder::from_verified(info, storage, peer_id, timeout),
                )?,
                crate::TorrentFormat::V1 | crate::TorrentFormat::Hybrid => runtime.add_seeder(
                    crate::Seeder::from_verified(info, storage, peer_id, timeout),
                )?,
            }
        }
        for hash in runtime.info_hashes() {
            if !wanted.contains(&hash) {
                runtime.remove(&hash);
            }
        }
        Ok(())
    }

    /// Starts a managed torrent by its index.
    pub fn start_torrent(&mut self, index: usize) -> Result<(), BackendError> {
        let info = self
            .torrents
            .get(index)
            .and_then(Torrent::info)
            .cloned()
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        let storage = self.storage_at(index);
        if info.format() == crate::TorrentFormat::V2 {
            let complete = storage
                .scan_complete_v2_files(&info)?
                .into_iter()
                .all(|file| file);
            if complete {
                self.torrents[index].set_state(crate::TransferState::Complete);
                return Ok(());
            }
            return Err(BackendError::UnsupportedFeature(
                "v2-only downloading requires the v2 peer transfer path".to_owned(),
            ));
        }
        let complete = !info.pieces().is_empty()
            && storage
                .scan_complete_pieces(&info)?
                .into_iter()
                .all(|piece| piece)
            && (info.format() != crate::TorrentFormat::Hybrid
                || storage
                    .scan_complete_v2_files(&info)?
                    .into_iter()
                    .all(|file| file));
        if complete {
            self.torrents[index].seed();
        } else {
            self.torrents[index].start();
        }
        Ok(())
    }

    /// Refreshes a managed torrent's state from verified on-disk pieces.
    pub fn refresh_torrent_state(&mut self, index: usize) -> Result<(), BackendError> {
        let info = self
            .torrents
            .get(index)
            .and_then(Torrent::info)
            .cloned()
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        let storage = self.storage_at(index);
        if info.format() == crate::TorrentFormat::V2 {
            if storage
                .scan_complete_v2_files(&info)?
                .into_iter()
                .all(|file| file)
            {
                self.torrents[index].set_state(crate::TransferState::Complete);
            }
            return Ok(());
        }
        if !info.pieces().is_empty()
            && storage
                .scan_complete_pieces(&info)?
                .into_iter()
                .all(|piece| piece)
            && (info.format() != crate::TorrentFormat::Hybrid
                || storage
                    .scan_complete_v2_files(&info)?
                    .into_iter()
                    .all(|file| file))
        {
            self.torrents[index].seed();
        }
        Ok(())
    }

    /// Marks a torrent to start, without checking its data now. An
    /// interface resumes such torrents with [`Client::spawn_download`] (or
    /// [`Client::spawn_metadata`] for magnets), which checks local data
    /// first and moves complete torrents straight to seeding.
    pub fn mark_started(&mut self, index: usize) -> Result<(), BackendError> {
        self.torrents
            .get_mut(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?
            .start();
        Ok(())
    }

    /// Pauses a managed torrent by its index.
    pub fn pause_torrent(&mut self, index: usize) -> Result<(), BackendError> {
        self.torrents
            .get_mut(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?
            .pause();
        Ok(())
    }

    /// Discovers peers through trackers (and the DHT, when set) and
    /// downloads a managed torrent of any format, blocking until done.
    pub fn download_torrent(
        &mut self,
        index: usize,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<usize, BackendError> {
        let info = self.torrent_info(index)?;
        let peer_id = self.peer_id_for(Some(&info), peer_id);
        let dht = self.dht.clone().filter(|_| !info.is_private());
        let lsd = self.lsd.clone().filter(|_| !info.is_private());
        let session = crate::DownloadSession::new(info, self.storage_at(index), peer_id, timeout)
            .with_dht(dht)
            .with_lsd(lsd)
            .with_connect_options(self.connect_options())
            .with_limits(Arc::clone(&self.limits))
            .with_runtime(self.runtime.clone());
        let downloaded = session.download_from_trackers_with_retries(
            self.config.listen_port(),
            self.config.max_peer_connections(),
            3,
        )?;
        self.torrents[index].seed();
        Ok(downloaded)
    }

    /// Creates an inbound seeder for a fully downloaded managed torrent.
    pub fn seeder(
        &self,
        index: usize,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<crate::Seeder, BackendError> {
        let info = self
            .torrents
            .get(index)
            .and_then(Torrent::info)
            .cloned()
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        if info.format() == crate::TorrentFormat::V2 {
            return Err(BackendError::UnsupportedFeature(
                "v2-only seeding requires the v2 peer transfer path".to_owned(),
            ));
        }
        let storage = self.storage_at(index);
        crate::Seeder::new(info, storage, peer_id, timeout)
    }

    /// Creates a v2-only seeder for fully verified local data.
    pub fn v2_seeder(
        &self,
        index: usize,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<crate::V2Seeder, BackendError> {
        let info = self
            .torrents
            .get(index)
            .and_then(Torrent::info)
            .cloned()
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        let storage = self.storage_at(index);
        crate::V2Seeder::new(info, storage, peer_id, timeout)
    }

    /// Downloads a managed torrent when necessary, then serves it to peers
    /// until `shutdown` is set: the complete synchronous lifecycle for a
    /// long-running backend process, for any torrent format.
    pub fn download_and_seed(
        &mut self,
        index: usize,
        peer_id: [u8; 20],
        timeout: Duration,
        shutdown: &AtomicBool,
    ) -> Result<usize, BackendError> {
        let info = self.torrent_info(index)?;
        let complete =
            crate::DownloadSession::new(info.clone(), self.storage_at(index), peer_id, timeout)
                .is_complete()?;
        let downloaded = if complete {
            self.torrents[index].seed();
            0
        } else {
            self.download_torrent(index, peer_id, timeout)?
        };
        let addresses = self
            .config
            .binding()
            .listen_addresses(self.config.listen_port())?;
        let limit = usize::MAX; // the shared limits cap connections
        let runtime = if info.format() == crate::TorrentFormat::V2 {
            let seeder = self.v2_seeder(index, peer_id, timeout)?;
            crate::Runtime::new_mixed(Vec::new(), vec![seeder], limit)?
        } else {
            let seeder = self.seeder(index, peer_id, timeout)?;
            crate::Runtime::new_mixed(vec![seeder], Vec::new(), limit)?
        }
        .with_limits(Arc::clone(&self.limits))
        .with_binding(self.config.binding())
        .with_encryption(self.config.encryption())
        .with_utp(
            self.utp
                .clone()
                .filter(|_| self.config.utp() != crate::UtpMode::Disabled),
        )
        .with_handshake_timeout(timeout)?;
        let listeners = runtime.bind_all(&addresses)?;
        runtime.serve_until_all(&listeners, shutdown)?;
        Ok(downloaded)
    }

    /// Removes a managed torrent and returns it. Downloaded data stays on
    /// disk. Callers should cancel any running transfer for it first.
    pub fn remove_torrent(&mut self, index: usize) -> Result<Torrent, BackendError> {
        if index >= self.torrents.len() {
            return Err(BackendError::TorrentNotFound(index.to_string()));
        }
        Ok(self.torrents.remove(index))
    }

    /// Finds a managed torrent by its v1 info hash.
    pub fn torrent_index(&self, info_hash: &[u8; 20]) -> Option<usize> {
        self.torrents
            .iter()
            .position(|torrent| torrent.info_hash() == Some(info_hash))
    }

    /// Hashes a managed torrent's local data on a background thread,
    /// reporting progress as pieces are checked.
    pub fn spawn_verify(&self, index: usize) -> Result<crate::TransferHandle, BackendError> {
        let info = self.torrent_info(index)?;
        let storage = self.storage_at(index);
        let progress = std::sync::Arc::new(crate::TransferProgress::new());
        let reporter = std::sync::Arc::clone(&progress);
        Ok(crate::TransferHandle::spawn(
            *info.info_hash(),
            crate::TransferKind::Verify,
            progress,
            move |_| verify_local_data(&info, &storage, &reporter),
        ))
    }

    /// Starts downloading a managed torrent on a background thread. The
    /// transfer re-announces until every piece is verified or it is
    /// cancelled through the returned handle.
    pub fn spawn_download(
        &mut self,
        index: usize,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<crate::TransferHandle, BackendError> {
        let info = self.torrent_info(index)?;
        let peer_id = self.peer_id_for(Some(&info), peer_id);
        let storage = self.storage_at(index);
        let port = self.config.listen_port();
        let max_connections = self.config.max_peer_connections();
        let options = self.connect_options();
        let runtime = self.runtime.clone();
        let limits = Arc::clone(&self.limits);
        let progress = std::sync::Arc::new(crate::TransferProgress::new());
        progress.set_totals(info.piece_count(), info.total_length());
        let reporter = std::sync::Arc::clone(&progress);
        // BEP 27: private torrents find peers only through their trackers.
        let dht = self.dht.clone().filter(|_| !info.is_private());
        let lsd = self.lsd.clone().filter(|_| !info.is_private());
        let resume = self.torrents[index].resume().cloned();
        self.torrents[index].start();
        Ok(crate::TransferHandle::spawn(
            *info.info_hash(),
            crate::TransferKind::Download,
            progress,
            move |_| {
                crate::DownloadSession::new(info, storage, peer_id, timeout)
                    .with_progress(reporter)
                    .with_dht(dht)
                    .with_lsd(lsd)
                    .with_resume(resume)
                    .with_connect_options(options)
                    .with_limits(limits)
                    .with_runtime(runtime)
                    .download_from_trackers_with_retries(port, max_connections, UNTIL_CANCELLED)
            },
        ))
    }

    /// Fetches a magnet link's metadata from peers found through its
    /// trackers, the peers listed in the link, and the DHT. Retries until it
    /// succeeds or is cancelled. Once collected with
    /// [`Client::finish_transfer`], the torrent can download normally.
    pub fn spawn_metadata(
        &mut self,
        index: usize,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<crate::TransferHandle, BackendError> {
        let link = self
            .torrents
            .get(index)
            .and_then(Torrent::magnet)
            .cloned()
            .ok_or_else(|| {
                BackendError::InvalidConfiguration(
                    "this torrent already has its metadata".to_owned(),
                )
            })?;
        if link.trackers().is_empty()
            && link.peers().is_empty()
            && self.dht.is_none()
            && self.lsd.is_none()
        {
            return Err(BackendError::InvalidConfiguration(
                "this magnet link has no trackers or peers, and the DHT is disabled".to_owned(),
            ));
        }
        let peer_id = self.peer_id_for(None, peer_id);
        let port = self.config.listen_port();
        let options = self.connect_options();
        let dht = self.dht.clone();
        let lsd = self.lsd.clone();
        let progress = Arc::new(crate::TransferProgress::new());
        let reporter = Arc::clone(&progress);
        self.torrents[index].start();
        Ok(crate::TransferHandle::spawn(
            *link.info_hash(),
            crate::TransferKind::Metadata,
            progress,
            move |output| {
                let metadata = fetch_magnet_metadata(
                    &link,
                    (dht, lsd),
                    port,
                    peer_id,
                    timeout,
                    &options,
                    &reporter,
                )?;
                // The complete .torrent: info dictionary plus any v2 piece
                // layers, with the link's trackers.
                let torrent = TorrentInfo::from_metadata_with_layers(
                    &metadata.info,
                    &metadata.piece_layers,
                    link.trackers(),
                )?;
                let size = metadata.info.len();
                if let Ok(mut output) = output.lock() {
                    *output = Some(torrent.source_bytes().to_vec());
                }
                Ok(size)
            },
        ))
    }

    /// Waits for a background transfer, then updates its torrent's state:
    /// completed data moves to seeding, a cancelled or failed download is
    /// paused, and a verify that finds missing pieces stops seeding.
    pub fn finish_transfer(
        &mut self,
        handle: crate::TransferHandle,
    ) -> Result<usize, BackendError> {
        let info_hash = *handle.info_hash();
        let kind = handle.kind();
        let progress = std::sync::Arc::clone(handle.progress());
        let (mut result, output) = handle.join();
        let index = self
            .torrent_index(&info_hash)
            .ok_or_else(|| BackendError::TorrentNotFound(hex(&info_hash)))?;
        // The thread has stopped writing, so this snapshot will still match
        // the files next time. Cancelled downloads keep their progress too,
        // but only once their check finished: a check cancelled part way
        // would record the pieces it never reached as missing, and the
        // next start would trust that and download them again.
        if kind != crate::TransferKind::Metadata
            && !matches!(result, Err(BackendError::Io(_)))
            && progress.total_pieces() > 0
            && progress.checked()
        {
            self.record_resume(index, &progress);
        }
        let torrent = &self.torrents[index];
        // No piece covers an empty file, so a finished download creates them.
        if kind == crate::TransferKind::Download
            && result.is_ok()
            && let Some(info) = torrent.info()
            && let Err(error) = self.storage_for(torrent).create_empty_files(info)
        {
            result = Err(error);
        }
        let torrent = &mut self.torrents[index];
        match (kind, &result) {
            (crate::TransferKind::Metadata, Ok(_)) => {
                match output
                    .ok_or_else(|| {
                        BackendError::Io("metadata transfer produced no data".to_owned())
                    })
                    .and_then(|bytes| TorrentInfo::from_bytes(&bytes))
                {
                    // The swarm hash is the v1 hash, or the truncated v2 hash
                    // for a v2-only link. Stays downloading, ready to start.
                    Ok(info) if info.peer_info_hash() == info_hash => torrent.set_info(info),
                    Ok(_) => {
                        torrent.pause();
                        result = Err(BackendError::InvalidMetainfo(
                            "fetched metadata does not match the magnet link".to_owned(),
                        ));
                    }
                    Err(error) => {
                        torrent.pause();
                        result = Err(error);
                    }
                }
            }
            (crate::TransferKind::Metadata, Err(_)) => torrent.pause(),
            (crate::TransferKind::Download, Ok(_)) => torrent.seed(),
            (crate::TransferKind::Download, Err(_)) => torrent.pause(),
            // A check reports progress; it never resumes a paused torrent.
            // It only stops seeding data that turned out to be incomplete.
            (crate::TransferKind::Verify, Ok(_)) => {
                let complete = progress.total_pieces() > 0
                    && progress.verified_pieces() == progress.total_pieces();
                if !complete
                    && matches!(
                        torrent.state(),
                        crate::TransferState::Seeding | crate::TransferState::Complete
                    )
                {
                    torrent.pause();
                }
            }
            (crate::TransferKind::Verify, Err(_)) => {}
        }
        if kind == crate::TransferKind::Metadata && result.is_ok() {
            self.apply_pending_layout(index);
        }
        result
    }

    /// Gives a magnet link's files the layout chosen for them, now that
    /// they are known.
    fn apply_pending_layout(&mut self, index: usize) {
        let layout = self.torrents[index].pending_layout();
        if layout != crate::ContentLayout::Original {
            let _ = self.set_content_layout(index, layout);
        }
    }

    /// Whether a torrent's resume data still matches its files, so its saved
    /// progress can be shown and trusted without re-checking.
    pub fn resume_is_current(&self, index: usize) -> bool {
        let Some(torrent) = self.torrents.get(index) else {
            return false;
        };
        match (torrent.info(), torrent.resume()) {
            (Some(info), Some(resume)) => resume.is_current(info, &self.storage_for(torrent)),
            _ => false,
        }
    }

    /// Saves the verified pieces of a running transfer as resume data, for
    /// a periodic checkpoint or before quitting without collecting it. If
    /// the transfer writes again afterwards, the next start notices and
    /// checks the data in full.
    pub fn checkpoint(&mut self, handle: &crate::TransferHandle) {
        // An unfinished check knows nothing of the pieces it has not reached.
        if handle.kind() == crate::TransferKind::Metadata
            || handle.progress().total_pieces() == 0
            || !handle.progress().checked()
        {
            return;
        }
        if let Some(index) = self.torrent_index(handle.info_hash()) {
            self.record_resume(index, handle.progress());
        }
    }

    fn record_resume(&mut self, index: usize, progress: &crate::TransferProgress) {
        let Some(info) = self.torrents[index].info() else {
            return;
        };
        let resume = crate::ResumeData::capture(
            info,
            &self.storage_for(&self.torrents[index]),
            progress.verified_bitfield(),
        );
        if let Some(hash) = self.torrents[index].info_hash().copied()
            && let Ok(mut dirty) = self.resume_dirty.lock()
        {
            dirty.insert(hash);
        }
        self.torrents[index].set_resume(Some(resume));
    }

    fn torrent_info(&self, index: usize) -> Result<TorrentInfo, BackendError> {
        let torrent = self
            .torrents
            .get(index)
            .ok_or_else(|| BackendError::TorrentNotFound(index.to_string()))?;
        match (torrent.info(), torrent.magnet()) {
            (Some(info), _) => Ok(info.clone()),
            (None, Some(_)) => Err(BackendError::InvalidConfiguration(
                "this magnet link's metadata has not been fetched yet".to_owned(),
            )),
            (None, None) => Err(BackendError::TorrentNotFound(index.to_string())),
        }
    }

    /// Saves torrent metadata, pending magnet links, and lifecycle states
    /// for the next process run.
    pub fn save_session(&self, path: impl AsRef<std::path::Path>) -> Result<(), BackendError> {
        let mut out = SESSION_MAGIC.to_vec();
        out.extend((self.torrents.len() as u32).to_be_bytes());
        for torrent in &self.torrents {
            let (kind, bytes) = match (torrent.info(), torrent.magnet()) {
                (Some(info), _) if !info.source_bytes().is_empty() => {
                    (RECORD_METAINFO, info.source_bytes().to_vec())
                }
                (None, Some(link)) => (RECORD_MAGNET, link.to_uri().into_bytes()),
                _ => {
                    return Err(BackendError::InvalidConfiguration(
                        "cannot persist metadata created without original .torrent bytes"
                            .to_owned(),
                    ));
                }
            };
            out.push(state_byte(torrent.state()));
            out.push(kind);
            out.extend((bytes.len() as u64).to_be_bytes());
            out.extend(bytes);
        }
        std::fs::write(path, out).map_err(|error| BackendError::Io(error.to_string()))
    }

    /// Loads a previously saved session and restores torrent states. Reads
    /// both the current format and the original metainfo-only format.
    pub fn load_session(
        config: ClientConfig,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, BackendError> {
        let bytes = std::fs::read(path).map_err(|error| BackendError::Io(error.to_string()))?;
        let has_kinds = match bytes.get(..7) {
            Some(magic) if magic == SESSION_MAGIC => true,
            Some(magic) if magic == b"EMPBT1\0" => false,
            _ => {
                return Err(BackendError::InvalidConfiguration(
                    "session file has an invalid format".to_owned(),
                ));
            }
        };
        let mut cursor = 7;
        let count = read_u32(&bytes, &mut cursor)? as usize;
        let mut client = Self::with_config(config)?;
        for _ in 0..count {
            let state = state_from_byte(read_u8(&bytes, &mut cursor)?)?;
            let kind = if has_kinds {
                read_u8(&bytes, &mut cursor)?
            } else {
                RECORD_METAINFO
            };
            let length = read_u64(&bytes, &mut cursor)? as usize;
            let end = cursor.checked_add(length).ok_or_else(|| {
                BackendError::InvalidConfiguration("session metadata length overflowed".to_owned())
            })?;
            let record = bytes.get(cursor..end).ok_or_else(|| {
                BackendError::InvalidConfiguration("session metadata is truncated".to_owned())
            })?;
            cursor = end;
            let index = match kind {
                RECORD_METAINFO => client.add_torrent(TorrentInfo::from_bytes(record)?),
                RECORD_MAGNET => {
                    let uri = std::str::from_utf8(record).map_err(|_| {
                        BackendError::InvalidConfiguration(
                            "session magnet link is not text".to_owned(),
                        )
                    })?;
                    client.add_magnet(MagnetLink::parse(uri)?)
                }
                _ => {
                    return Err(BackendError::InvalidConfiguration(
                        "session contains an unknown record type".to_owned(),
                    ));
                }
            };
            client.torrents[index].set_state(state);
        }
        if cursor != bytes.len() {
            return Err(BackendError::InvalidConfiguration(
                "session file has trailing data".to_owned(),
            ));
        }
        Ok(client)
    }
}

impl Client {
    /// Saves the session into a directory incrementally: each torrent's
    /// metadata is written once to its own file, and only a small index of
    /// order and states is rewritten. With thousands of torrents a change
    /// costs kilobytes instead of rewriting every .torrent.
    pub fn save_session_dir(&self, dir: impl AsRef<std::path::Path>) -> Result<(), BackendError> {
        let dir = dir.as_ref();
        let files = dir.join(SESSION_TORRENTS);
        std::fs::create_dir_all(&files).map_err(|error| BackendError::Io(error.to_string()))?;
        let mut index = SESSION_DIR_MAGIC.to_vec();
        index.extend((self.torrents.len() as u32).to_be_bytes());
        let mut keep = std::collections::HashSet::new();
        for (position, torrent) in self.torrents.iter().enumerate() {
            let hash = torrent.info_hash().ok_or_else(|| {
                BackendError::InvalidConfiguration(
                    "cannot persist a torrent without a hash".to_owned(),
                )
            })?;
            let (kind, extension) = match (torrent.info(), torrent.magnet()) {
                (Some(info), _) if !info.source_bytes().is_empty() => (RECORD_METAINFO, "torrent"),
                (None, Some(_)) => (RECORD_MAGNET, "magnet"),
                _ => {
                    return Err(BackendError::InvalidConfiguration(
                        "cannot persist metadata created without original .torrent bytes"
                            .to_owned(),
                    ));
                }
            };
            let name = format!("{}.{extension}", hex(hash));
            let path = files.join(&name);
            // Metadata never changes for a given hash, so existing files
            // are left alone.
            if !path.exists() {
                let bytes = match (torrent.info(), torrent.magnet()) {
                    (Some(info), _) if kind == RECORD_METAINFO => info.source_bytes().to_vec(),
                    (_, Some(link)) => link.to_uri().into_bytes(),
                    _ => unreachable!("kind was chosen from these fields"),
                };
                write_atomically(&path, &bytes)?;
            }
            keep.insert(name);
            index.push(state_byte(torrent.state()));
            index.push(kind);
            index.extend(hash);
            let folder = self
                .save_dir(position)
                .and_then(std::path::Path::to_str)
                .unwrap_or_default();
            let length = u16::try_from(folder.len()).map_err(|_| {
                BackendError::InvalidConfiguration("a save folder's path is too long".to_owned())
            })?;
            index.extend(length.to_be_bytes());
            index.extend(folder.as_bytes());
        }
        write_atomically(&dir.join(SESSION_INDEX), &index)?;
        // Removed torrents, and magnets whose metadata has since arrived.
        if let Ok(entries) = std::fs::read_dir(&files) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !keep.contains(&name) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        self.save_resume(&dir.join(SESSION_RESUME))?;
        self.save_names(&dir.join(SESSION_NAMES))
    }

    /// Writes each torrent's chosen name and renamed files, where it has
    /// any, and removes the files of torrents without them.
    fn save_names(&self, dir: &std::path::Path) -> Result<(), BackendError> {
        std::fs::create_dir_all(dir).map_err(|error| BackendError::Io(error.to_string()))?;
        let mut keep = std::collections::HashSet::new();
        for torrent in &self.torrents {
            let Some(hash) = torrent.info_hash() else {
                continue;
            };
            let table = torrent.renames().table();
            let layout = torrent.pending_layout();
            if torrent.chosen_name().is_none()
                && table.is_empty()
                && layout == crate::ContentLayout::Original
            {
                continue;
            }
            let mut text = format!("{NAMES_HEADER}\n");
            if let Some(name) = torrent.chosen_name() {
                text.push_str(&format!("name\t{name}\n"));
            }
            if layout != crate::ContentLayout::Original {
                text.push_str(&format!("layout\t{}\n", layout.key()));
            }
            for (original, path) in table.iter() {
                let slashed = |path: &Path| path.to_string_lossy().replace('\\', "/");
                text.push_str(&format!("file\t{}\t{}\n", slashed(original), slashed(path)));
            }
            let name = format!("{}.names", hex(hash));
            let path = dir.join(&name);
            if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                write_atomically(&path, text.as_bytes())?;
            }
            keep.insert(name);
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    /// Reads what [`Client::save_names`] saved into `torrent`; lines that
    /// do not hold safe names are skipped.
    fn load_names(torrent: &mut Torrent, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let mut lines = text.lines();
        if lines.next() != Some(NAMES_HEADER) {
            return;
        }
        let mut table = BTreeMap::new();
        for line in lines {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next(), fields.next()) {
                (Some("name"), Some(name), None) if !name.is_empty() => {
                    torrent.set_display_name(Some(name.to_owned()));
                }
                (Some("layout"), Some(layout), None) => {
                    if let Some(layout) = crate::ContentLayout::parse(layout) {
                        torrent.set_pending_layout(layout);
                    }
                }
                (Some("file"), Some(original), Some(renamed)) => {
                    let (original, renamed) = (PathBuf::from(original), PathBuf::from(renamed));
                    if is_plain_relative(&original) && is_plain_relative(&renamed) {
                        table.insert(original, renamed);
                    }
                }
                _ => {}
            }
        }
        torrent.renames().replace(table);
    }

    /// Writes resume files that changed since the last save, and removes
    /// those of torrents no longer in the session.
    fn save_resume(&self, dir: &std::path::Path) -> Result<(), BackendError> {
        std::fs::create_dir_all(dir).map_err(|error| BackendError::Io(error.to_string()))?;
        let dirty: Vec<[u8; 20]> = self
            .resume_dirty
            .lock()
            .map(|mut dirty| dirty.drain().collect())
            .unwrap_or_default();
        for hash in dirty {
            let Some(index) = self.torrent_index(&hash) else {
                continue;
            };
            if let Some(resume) = self.torrents[index].resume() {
                write_atomically(
                    &dir.join(format!("{}.resume", hex(&hash))),
                    &resume.encode(),
                )?;
            }
        }
        let keep: std::collections::HashSet<String> = self
            .torrents
            .iter()
            .filter(|torrent| torrent.resume().is_some())
            .filter_map(|torrent| torrent.info_hash())
            .map(|hash| format!("{}.resume", hex(hash)))
            .collect();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    /// Loads a session saved by [`Client::save_session_dir`]. A torrent
    /// whose file is missing or damaged is skipped and reported in the
    /// returned warnings rather than failing the whole session.
    pub fn load_session_dir(
        config: ClientConfig,
        dir: impl AsRef<std::path::Path>,
    ) -> Result<(Self, Vec<String>), BackendError> {
        let dir = dir.as_ref();
        let index = std::fs::read(dir.join(SESSION_INDEX))
            .map_err(|error| BackendError::Io(error.to_string()))?;
        // Version 3 had no save folders: everything was in the download
        // directory.
        let has_folders = match index.get(..7) {
            Some(magic) if magic == SESSION_DIR_MAGIC => true,
            Some(magic) if magic == b"EMPBT3\0" => false,
            _ => {
                return Err(BackendError::InvalidConfiguration(
                    "session index has an invalid format".to_owned(),
                ));
            }
        };
        let mut cursor = 7;
        let count = read_u32(&index, &mut cursor)? as usize;
        let mut client = Self::with_config(config)?;
        let mut warnings = Vec::new();
        for _ in 0..count {
            let state = state_from_byte(read_u8(&index, &mut cursor)?)?;
            let kind = read_u8(&index, &mut cursor)?;
            let end = cursor + 20;
            let hash: [u8; 20] = index
                .get(cursor..end)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| {
                    BackendError::InvalidConfiguration("session index is truncated".to_owned())
                })?;
            cursor = end;
            let folder = if has_folders {
                let length = read_u16(&index, &mut cursor)? as usize;
                let end = cursor + length;
                let text = index
                    .get(cursor..end)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .ok_or_else(|| {
                        BackendError::InvalidConfiguration(
                            "session index has a damaged save folder".to_owned(),
                        )
                    })?;
                cursor = end;
                (!text.is_empty()).then(|| std::path::PathBuf::from(text))
            } else {
                None
            };
            let extension = if kind == RECORD_MAGNET {
                "magnet"
            } else {
                "torrent"
            };
            let path = dir
                .join(SESSION_TORRENTS)
                .join(format!("{}.{extension}", hex(&hash)));
            let loaded = std::fs::read(&path)
                .map_err(|error| BackendError::Io(error.to_string()))
                .and_then(|bytes| match kind {
                    RECORD_METAINFO => TorrentInfo::from_bytes(&bytes).map(Torrent::from_info),
                    RECORD_MAGNET => std::str::from_utf8(&bytes)
                        .map_err(|_| {
                            BackendError::InvalidMetainfo("magnet file is not text".to_owned())
                        })
                        .and_then(MagnetLink::parse)
                        .map(Torrent::from_magnet),
                    _ => Err(BackendError::InvalidConfiguration(
                        "session contains an unknown record type".to_owned(),
                    )),
                })
                .and_then(|torrent| {
                    if torrent.info_hash() == Some(&hash) {
                        Ok(torrent)
                    } else {
                        Err(BackendError::InvalidMetainfo(
                            "file does not match its info hash".to_owned(),
                        ))
                    }
                });
            match loaded {
                Ok(mut torrent) => {
                    torrent.set_state(state);
                    torrent.set_save_dir(
                        folder.or_else(|| Some(client.config.download_directory().to_owned())),
                    );
                    // Missing or damaged resume data only means a full check.
                    let resume = dir
                        .join(SESSION_RESUME)
                        .join(format!("{}.resume", hex(&hash)));
                    torrent.set_resume(
                        std::fs::read(resume)
                            .ok()
                            .and_then(|bytes| crate::ResumeData::decode(&bytes).ok()),
                    );
                    let names = dir
                        .join(SESSION_NAMES)
                        .join(format!("{}.names", hex(&hash)));
                    Self::load_names(&mut torrent, &names);
                    client.torrents.push(torrent);
                }
                Err(error) => warnings.push(format!(
                    "skipped torrent {}: {error} ({})",
                    hex(&hash),
                    path.display()
                )),
            }
        }
        if cursor != index.len() {
            return Err(BackendError::InvalidConfiguration(
                "session index has trailing data".to_owned(),
            ));
        }
        Ok((client, warnings))
    }
}

fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> Result<(), BackendError> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)
        .and_then(|_| std::fs::rename(&temporary, path))
        .map_err(|error| BackendError::Io(format!("{}: {error}", path.display())))
}

/// Longest name shown for a torrent, in characters.
const MAX_NAME: usize = 255;

/// A path that stays inside the folder it is relative to: plain names only,
/// no `..`, no root, nothing empty or with control characters.
fn is_plain_relative(path: &Path) -> bool {
    path.components().count() > 0
        && path.components().all(|part| match part {
            std::path::Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| !name.is_empty() && !name.chars().any(char::is_control)),
            _ => false,
        })
}

const SESSION_DIR_MAGIC: &[u8; 7] = b"EMPBT4\0";
const SESSION_INDEX: &str = "index";
const SESSION_TORRENTS: &str = "torrents";
const SESSION_RESUME: &str = "resume";
/// Torrents' chosen names and renamed files.
const SESSION_NAMES: &str = "names";
const NAMES_HEADER: &str = "EmpireBT names 1";

const SESSION_MAGIC: &[u8; 7] = b"EMPBT2\0";
const RECORD_METAINFO: u8 = 0;
const RECORD_MAGNET: u8 = 1;

/// Delay between metadata attempts when no peer could provide it.
const METADATA_RETRY: Duration = Duration::from_secs(15);
/// Peers asked for metadata at the same time. Public swarms are full of
/// unreachable peers and the metadata is small, so cast a wide net.
const METADATA_PARALLELISM: usize = 24;
/// Per-peer timeout for metadata, shorter than for piece transfers so dead
/// peers are skipped quickly.
const METADATA_PEER_TIMEOUT: Duration = Duration::from_secs(5);

/// Discovers peers for a magnet link and fetches its info dictionary.
fn fetch_magnet_metadata(
    link: &MagnetLink,
    (dht, lsd): (Option<Arc<Dht>>, Option<Arc<crate::Lsd>>),
    port: u16,
    peer_id: [u8; 20],
    timeout: Duration,
    options: &crate::ConnectOptions,
    progress: &crate::TransferProgress,
) -> Result<crate::Metadata, BackendError> {
    let trackers =
        crate::TrackerClient::from_urls(link.trackers().to_vec())?.with_binding(options.binding());
    let mut search = dht.map(|dht| crate::dht::PeerSearch::new(dht, *link.info_hash(), Some(port)));
    let mut peers = link.peers();
    // Every tracker is asked, each on its own thread; their peers land here.
    let announced: Arc<std::sync::Mutex<Vec<SocketAddr>>> = Arc::default();
    let mut announcer = crate::discovery::Announcer::new(&trackers, timeout, {
        let announced = Arc::clone(&announced);
        Arc::new(move |found: &[SocketAddr]| {
            if let Ok(mut announced) = announced.lock() {
                announced.extend_from_slice(found);
            }
        })
    })
    // Fresh peers matter more than tracker etiquette while nobody we know
    // has the metadata.
    .with_max_interval(Duration::from_secs(60));
    // Local peers that have the torrent can hand over its metadata too.
    let _local = lsd.as_ref().map(|lsd| {
        let announced = Arc::clone(&announced);
        lsd.join(
            &[*link.info_hash()],
            Arc::new(move |found: &[SocketAddr]| {
                if let Ok(mut announced) = announced.lock() {
                    announced.extend_from_slice(found);
                }
            }),
        )
    });
    let request = |info_hash, event| crate::AnnounceRequest {
        info_hash,
        peer_id,
        port,
        uploaded: 0,
        downloaded: 0,
        // The size is unknown until the metadata arrives; any non-zero
        // value marks us as a leecher rather than a seeder.
        left: 1,
        corrupt: 0,
        compact: true,
        event,
    };
    let hashes = [*link.info_hash()];
    // Starts due announces and takes in their results and peers; returns
    // whether new peers arrived.
    let mut poll_trackers = |peers: &mut Vec<SocketAddr>| {
        announcer.start_due(&hashes, request);
        let collected = announcer.collect();
        if let Some(best) = &collected.best {
            progress.set_swarm(best);
        }
        if let Some(error) = &collected.error {
            progress.record_error(error);
        }
        let mut added = false;
        for peer in announced
            .lock()
            .map(|mut found| std::mem::take(&mut *found))
            .unwrap_or_default()
        {
            if !peers.contains(&peer) {
                peers.push(peer);
                added = true;
            }
        }
        added
    };
    let mut tried = 0;
    loop {
        progress.check_cancelled()?;
        if let Some(search) = &mut search {
            for peer in search.poll() {
                if !peers.contains(&peer) {
                    peers.push(peer);
                }
            }
        }
        poll_trackers(&mut peers);
        // Only ask peers not tried yet, until every known peer was tried.
        if tried < peers.len() {
            progress.set_phase(crate::TransferPhase::Metadata);
            let batch = &peers[tried..];
            tried = peers.len();
            match crate::fetch_metadata(
                batch,
                link,
                peer_id,
                timeout.min(METADATA_PEER_TIMEOUT),
                METADATA_PARALLELISM,
                progress,
                options,
            ) {
                Ok(info) => return Ok(info),
                Err(BackendError::Cancelled) => return Err(BackendError::Cancelled),
                Err(error) => progress.record_error(&error),
            }
            continue;
        }
        progress.set_phase(crate::TransferPhase::Waiting);
        let searching = search.as_ref().is_some_and(|search| search.searching());
        let wait = if searching {
            Duration::from_secs(1)
        } else {
            METADATA_RETRY
        };
        // Peers from a tracker end the wait early.
        let deadline = Instant::now() + wait;
        let mut arrived = false;
        while !arrived && Instant::now() < deadline {
            progress.sleep(Duration::from_millis(100))?;
            arrived = poll_trackers(&mut peers);
        }
        if !searching && !arrived {
            // Nobody new turned up; give every known peer another chance.
            tried = 0;
        }
    }
}

/// Tracker rounds for background downloads; they stop through cancellation
/// rather than by exhausting retries.
const UNTIL_CANCELLED: usize = usize::MAX;

/// Hashes local data for any torrent format, returning verified pieces.
fn verify_local_data(
    info: &TorrentInfo,
    storage: &crate::Storage,
    progress: &std::sync::Arc<crate::TransferProgress>,
) -> Result<usize, BackendError> {
    let session = crate::DownloadSession::new(
        info.clone(),
        storage.clone(),
        [0; 20],
        Duration::from_secs(1),
    )
    .with_progress(std::sync::Arc::clone(progress));
    let pieces = session.check_local_pieces()?;
    if pieces.is_complete()
        && info.format() == crate::TorrentFormat::Hybrid
        && !storage
            .scan_complete_v2_files(info)?
            .into_iter()
            .all(|file| file)
    {
        return Err(BackendError::InvalidMetainfo(
            "hybrid v2 file roots do not match the verified v1 data".to_owned(),
        ));
    }
    Ok(progress.verified_pieces())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn state_byte(state: crate::TransferState) -> u8 {
    match state {
        crate::TransferState::Paused => 0,
        crate::TransferState::Downloading => 1,
        crate::TransferState::Seeding => 2,
        crate::TransferState::Complete => 3,
    }
}

fn state_from_byte(value: u8) -> Result<crate::TransferState, BackendError> {
    match value {
        0 => Ok(crate::TransferState::Paused),
        1 => Ok(crate::TransferState::Downloading),
        2 => Ok(crate::TransferState::Seeding),
        3 => Ok(crate::TransferState::Complete),
        _ => Err(BackendError::InvalidConfiguration(
            "session contains an invalid torrent state".to_owned(),
        )),
    }
}

fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8, BackendError> {
    let value = *bytes.get(*cursor).ok_or_else(|| {
        BackendError::InvalidConfiguration("session file is truncated".to_owned())
    })?;
    *cursor += 1;
    Ok(value)
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, BackendError> {
    let value = bytes
        .get(*cursor..*cursor + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or_else(|| {
            BackendError::InvalidConfiguration("session file is truncated".to_owned())
        })?;
    *cursor += 2;
    Ok(value)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, BackendError> {
    let end = cursor.checked_add(4).ok_or_else(|| {
        BackendError::InvalidConfiguration("session cursor overflowed".to_owned())
    })?;
    let value = u32::from_be_bytes(
        bytes
            .get(*cursor..end)
            .ok_or_else(|| {
                BackendError::InvalidConfiguration("session file is truncated".to_owned())
            })?
            .try_into()
            .expect("slice length checked"),
    );
    *cursor = end;
    Ok(value)
}

fn read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, BackendError> {
    let end = cursor.checked_add(8).ok_or_else(|| {
        BackendError::InvalidConfiguration("session cursor overflowed".to_owned())
    })?;
    let value = u64::from_be_bytes(
        bytes
            .get(*cursor..end)
            .ok_or_else(|| {
                BackendError::InvalidConfiguration("session file is truncated".to_owned())
            })?
            .try_into()
            .expect("slice length checked"),
    );
    *cursor = end;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A torrent `album` with `album/a.flac` (3 bytes) and
    /// `album/disc/b.flac` (4 bytes) in one piece, and a client saving in
    /// `dir`, with both files written there.
    fn album_client(dir: &Path) -> (Client, usize) {
        use sha1::{Digest, Sha1};
        let _ = std::fs::remove_dir_all(dir);
        let hash: [u8; 20] = Sha1::digest(b"abcdefg").into();
        let mut bytes = b"d4:infod5:filesld6:lengthi3e4:pathl6:a.flaceed6:lengthi4e4:pathl4:disc6:b.flaceee4:name5:album12:piece lengthi16384e6:pieces20:".to_vec();
        bytes.extend(hash);
        bytes.extend(b"ee");
        let info = TorrentInfo::from_bytes(&bytes).unwrap();
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(dir)).unwrap();
        let index = client.add_torrent(info);
        std::fs::create_dir_all(dir.join("album/disc")).unwrap();
        std::fs::write(dir.join("album/a.flac"), b"abc").unwrap();
        std::fs::write(dir.join("album/disc/b.flac"), b"defg").unwrap();
        (client, index)
    }

    fn verified(client: &Client, index: usize) -> usize {
        let info = client.torrent_info(index).unwrap();
        let progress = std::sync::Arc::new(crate::TransferProgress::new());
        verify_local_data(&info, &client.storage_at(index), &progress).unwrap()
    }

    #[test]
    fn renaming_files_and_folders_moves_them_and_keeps_them_verifiable() {
        let dir = std::env::temp_dir().join(format!("empirebt-rename-{}", std::process::id()));
        let (mut client, index) = album_client(&dir);
        assert_eq!(verified(&client, index), 1);

        let moved = client
            .rename_path(
                index,
                Path::new("album/a.flac"),
                Path::new("album/01 - a.flac"),
            )
            .unwrap();
        assert_eq!(moved, 1);
        assert!(dir.join("album/01 - a.flac").exists());
        assert!(!dir.join("album/a.flac").exists());

        // A folder moves with everything in it; the emptied one goes.
        client
            .rename_path(index, Path::new("album"), Path::new("Album (2024)"))
            .unwrap();
        assert!(dir.join("Album (2024)/01 - a.flac").exists());
        assert!(dir.join("Album (2024)/disc/b.flac").exists());
        assert!(!dir.join("album").exists());
        let paths: Vec<_> = client
            .files(index)
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("Album (2024)/01 - a.flac"),
                PathBuf::from("Album (2024)/disc/b.flac")
            ]
        );
        assert_eq!(
            verified(&client, index),
            1,
            "the data is found under its new names"
        );

        // Renaming back to the torrent's own names leaves no table.
        client
            .rename_path(index, Path::new("Album (2024)"), Path::new("album"))
            .unwrap();
        client
            .rename_path(
                index,
                Path::new("album/01 - a.flac"),
                Path::new("album/a.flac"),
            )
            .unwrap();
        assert!(client.torrents()[index].renames().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn renames_that_would_clash_or_escape_are_refused() {
        let dir = std::env::temp_dir().join(format!("empirebt-rename-bad-{}", std::process::id()));
        let (mut client, index) = album_client(&dir);
        for to in [
            "album/disc/b.flac",
            "album/disc",
            "../outside",
            "/etc/a",
            "",
        ] {
            assert!(
                client
                    .rename_path(index, Path::new("album/a.flac"), Path::new(to))
                    .is_err(),
                "{to:?}"
            );
        }
        assert!(
            client
                .rename_path(index, Path::new("album/missing"), Path::new("album/x"))
                .is_err()
        );
        // A file in the way that is not the torrent's is not overwritten.
        std::fs::write(dir.join("album/other.txt"), b"mine").unwrap();
        assert!(
            client
                .rename_path(
                    index,
                    Path::new("album/a.flac"),
                    Path::new("album/other.txt")
                )
                .is_err()
        );
        assert_eq!(std::fs::read(dir.join("album/other.txt")).unwrap(), b"mine");
        assert!(dir.join("album/a.flac").exists());
        assert!(client.torrents()[index].renames().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_folder_rename_moves_nothing() {
        let dir = std::env::temp_dir().join(format!("empirebt-rename-undo-{}", std::process::id()));
        let (mut client, index) = album_client(&dir);
        // The second file's new place is taken on disk.
        std::fs::create_dir_all(dir.join("new/disc")).unwrap();
        std::fs::write(dir.join("new/disc/b.flac"), b"other").unwrap();
        assert!(
            client
                .rename_path(index, Path::new("album"), Path::new("new"))
                .is_err()
        );
        assert!(dir.join("album/a.flac").exists());
        assert!(dir.join("album/disc/b.flac").exists());
        assert!(!dir.join("new/a.flac").exists());
        assert!(client.torrents()[index].renames().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn content_layouts_place_files_with_or_without_the_top_folder() {
        let dir = std::env::temp_dir().join(format!("empirebt-layout-{}", std::process::id()));
        let (mut client, index) = album_client(&dir);
        client
            .set_content_layout(index, crate::ContentLayout::NoSubfolder)
            .unwrap();
        let paths: Vec<_> = client
            .files(index)
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(
            paths,
            [PathBuf::from("a.flac"), PathBuf::from("disc/b.flac")]
        );
        client
            .set_content_layout(index, crate::ContentLayout::Original)
            .unwrap();
        assert!(client.torrents()[index].renames().is_empty());

        let single = {
            let mut bytes =
                b"d4:infod6:lengthi4e4:name8:film.mkv12:piece lengthi4e6:pieces20:".to_vec();
            bytes.extend([3; 20]);
            bytes.extend(b"ee");
            client.add_torrent(TorrentInfo::from_bytes(&bytes).unwrap())
        };
        client
            .set_content_layout(single, crate::ContentLayout::Subfolder)
            .unwrap();
        assert_eq!(client.files(single)[0].path, PathBuf::from("film/film.mkv"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn anonymous_mode_hides_public_torrents_but_not_private_ones() {
        let metainfo = |private: bool| {
            let mut bytes =
                b"d4:infod6:lengthi4e4:name4:test12:piece lengthi4e6:pieces20:".to_vec();
            bytes.extend([3; 20]);
            if private {
                bytes.extend(b"7:privatei1e");
            }
            bytes.extend(b"ee");
            TorrentInfo::from_bytes(&bytes).unwrap()
        };
        let (public, private) = (metainfo(false), metainfo(true));
        assert!(private.is_private());
        let own = crate::generate_peer_id();
        let mut client = Client::new();
        assert_eq!(client.peer_id_for(Some(&public), own), own);
        let anonymous = crate::generate_anonymous_peer_id();
        client.set_anonymous_peer_id(Some(anonymous));
        assert_eq!(client.peer_id_for(Some(&public), own), anonymous);
        assert_eq!(client.peer_id_for(None, own), anonymous, "magnet links too");
        assert_eq!(
            client.peer_id_for(Some(&private), own),
            own,
            "private trackers admit only clients they know"
        );
        client.set_anonymous_peer_id(None);
        assert_eq!(client.peer_id_for(Some(&public), own), own);
    }

    #[test]
    fn a_magnet_links_layout_waits_for_its_metadata_across_restarts() {
        let dir = std::env::temp_dir().join(format!("empirebt-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut client = Client::new();
        let index = client.add_magnet(
            MagnetLink::parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a")
                .unwrap(),
        );
        client
            .set_content_layout(index, crate::ContentLayout::NoSubfolder)
            .unwrap();
        client.save_session_dir(&dir).unwrap();
        let (mut loaded, _) = Client::load_session_dir(ClientConfig::default(), &dir).unwrap();
        assert_eq!(
            loaded.torrents()[0].pending_layout(),
            crate::ContentLayout::NoSubfolder
        );

        // The metadata arrives: the files go without the top folder.
        let mut bytes = b"d4:infod5:filesld6:lengthi3e4:pathl5:a.txteed6:lengthi4e4:pathl5:b.txteee4:name5:album12:piece lengthi16384e6:pieces20:".to_vec();
        bytes.extend([7; 20]);
        bytes.extend(b"ee");
        loaded.torrents[0].set_info(TorrentInfo::from_bytes(&bytes).unwrap());
        loaded.apply_pending_layout(0);
        let paths: Vec<_> = loaded.files(0).into_iter().map(|file| file.path).collect();
        assert_eq!(paths, [PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
        assert_eq!(
            loaded.torrents()[0].pending_layout(),
            crate::ContentLayout::Original
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn chosen_names_and_renamed_files_survive_a_session_round_trip() {
        let dir = std::env::temp_dir().join(format!("empirebt-names-{}", std::process::id()));
        let data = dir.join("data");
        let session = dir.join("session");
        let (mut client, index) = album_client(&data);
        let _ = std::fs::remove_dir_all(&session);
        client
            .rename_torrent(index, Some("  My album  ".to_owned()))
            .unwrap();
        assert!(
            client
                .rename_torrent(index, Some("two\nlines".to_owned()))
                .is_err()
        );
        client
            .rename_path(index, Path::new("album/disc"), Path::new("album/CD 1"))
            .unwrap();
        client.save_session_dir(&session).unwrap();

        let config = ClientConfig::new().with_download_directory(&data);
        let (loaded, problems) = Client::load_session_dir(config.clone(), &session).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let torrent = &loaded.torrents()[0];
        assert_eq!(torrent.display_name(), "My album");
        assert_eq!(torrent.name(), "album");
        assert_eq!(loaded.files(0)[1].path, PathBuf::from("album/CD 1/b.flac"));
        assert_eq!(verified(&loaded, 0), 1);

        // Clearing both removes the saved names.
        client.rename_torrent(index, None).unwrap();
        client
            .rename_path(index, Path::new("album/CD 1"), Path::new("album/disc"))
            .unwrap();
        client.save_session_dir(&session).unwrap();
        assert_eq!(
            std::fs::read_dir(session.join(SESSION_NAMES))
                .unwrap()
                .count(),
            0
        );
        let (loaded, _) = Client::load_session_dir(config, &session).unwrap();
        assert_eq!(loaded.torrents()[0].display_name(), "album");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_client_has_default_configuration() {
        let client = Client::new();

        assert_eq!(client.config(), &ClientConfig::default());
    }

    #[test]
    fn custom_configuration_is_preserved() {
        let config = ClientConfig::new()
            .with_download_directory("data")
            .with_listen_port(51413);
        let client = Client::with_config(config.clone()).expect("configuration should be valid");

        assert_eq!(client.config(), &config);
    }

    #[test]
    fn empty_download_directory_is_rejected() {
        let config = ClientConfig::new().with_download_directory("");

        assert_eq!(
            Client::with_config(config),
            Err(BackendError::InvalidConfiguration(
                "download directory cannot be empty".to_owned()
            ))
        );
    }

    #[test]
    fn managed_torrents_can_be_started_and_paused() {
        let mut client = Client::new();
        let info = TorrentInfo::new(
            "example",
            16_384,
            Vec::new(),
            vec![crate::TorrentFile::new("example", 0)],
        )
        .expect("metadata should be valid");
        let index = client.add_torrent(info);

        client.start_torrent(index).expect("torrent should exist");
        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Downloading
        );

        client.pause_torrent(index).expect("torrent should exist");
        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Paused
        );
    }

    #[test]
    fn start_detects_completed_storage_and_enters_seeding() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-complete-{}", std::process::id()));
        let config = ClientConfig::new().with_download_directory(&root);
        let mut client = Client::with_config(config).unwrap();
        let data = b"complete";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let info = TorrentInfo::new(
            "complete",
            data.len() as u32,
            vec![hash],
            vec![crate::TorrentFile::new("complete", data.len() as u64)],
        )
        .unwrap();
        let index = client.add_torrent(info.clone());
        crate::Storage::new(&root)
            .unwrap()
            .write_piece(&info, 0, data)
            .unwrap();

        client.start_torrent(index).unwrap();

        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Seeding
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Each torrent can live in a folder of its own: checks and the saved
    /// session follow it, a later change of the download directory leaves
    /// it alone, and a version 3 session (one folder for everything) still
    /// loads, into the download directory.
    #[test]
    fn torrents_live_in_folders_of_their_own() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (downloads, elsewhere) = (root.join("downloads"), root.join("elsewhere"));
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(&downloads)).unwrap();
        let data = b"in a folder of its own";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let mut bytes = format!(
            "d4:infod6:lengthi{}e4:name3:own12:piece lengthi16384e6:pieces20:",
            data.len()
        )
        .into_bytes();
        bytes.extend(hash);
        bytes.extend(b"ee");
        let info = TorrentInfo::from_bytes(&bytes).unwrap();
        let index = client.add_torrent(info.clone());
        assert_eq!(client.save_dir(index), Some(downloads.as_path()));
        assert!(
            client
                .set_save_dir(index, std::path::PathBuf::from("relative"))
                .is_err()
        );
        client.set_save_dir(index, elsewhere.clone()).unwrap();
        crate::Storage::new(&elsewhere)
            .unwrap()
            .write_piece(&info, 0, data)
            .unwrap();
        let handle = client.spawn_verify(index).unwrap();
        assert_eq!(client.finish_transfer(handle), Ok(1));

        let session = root.join("session");
        client.save_session_dir(&session).unwrap();
        let other = ClientConfig::new().with_download_directory(root.join("other"));
        let (restored, warnings) = Client::load_session_dir(other.clone(), &session).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(restored.save_dir(0), Some(elsewhere.as_path()));

        // The same session as version 3 wrote it: no folders.
        let index_path = session.join(SESSION_INDEX);
        let current = std::fs::read(&index_path).unwrap();
        let mut old = b"EMPBT3\0".to_vec();
        old.extend(&current[7..11]);
        let mut cursor = 11;
        while cursor < current.len() {
            old.extend(&current[cursor..cursor + 22]);
            cursor += 22;
            let length = u16::from_be_bytes([current[cursor], current[cursor + 1]]) as usize;
            cursor += 2 + length;
        }
        std::fs::write(&index_path, old).unwrap();
        let (restored, _) = Client::load_session_dir(other, &session).unwrap();
        assert_eq!(
            restored.save_dir(0),
            Some(root.join("other").as_path()),
            "version 3 torrents were in the download directory"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A check cancelled part way must not become resume data: the next
    /// start would trust it and treat the pieces it never reached as
    /// missing, downloading data that is already on disk.
    #[test]
    fn cancelled_checks_leave_no_resume_data() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-cancelled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(&root)).unwrap();
        let data = vec![5u8; 64 * 16 * 1024];
        let mut pieces = Vec::new();
        for piece in data.chunks(16 * 1024) {
            use sha1::{Digest, Sha1};
            pieces.extend(Sha1::digest(piece));
        }
        let mut bytes = format!(
            "d4:infod6:lengthi{}e4:name4:many12:piece lengthi16384e6:pieces{}:",
            data.len(),
            pieces.len()
        )
        .into_bytes();
        bytes.extend(pieces);
        bytes.extend(b"ee");
        let info = TorrentInfo::from_bytes(&bytes).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("many"), &data).unwrap();
        let index = client.add_torrent(info);

        let handle = client.spawn_verify(index).unwrap();
        handle.cancel();
        let cancelled = client.finish_transfer(handle);
        if cancelled.is_err() {
            assert!(client.torrents()[index].resume().is_none());
        }
        // A check that finishes is recorded.
        let handle = client.spawn_verify(index).unwrap();
        assert_eq!(client.finish_transfer(handle), Ok(64));
        assert!(client.torrents()[index].resume().is_some());
        assert!(client.resume_is_current(index));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn background_verify_reports_progress_without_resuming() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-verify-{}", std::process::id()));
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(&root)).unwrap();
        let data = b"verified";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let info = TorrentInfo::new(
            "verified",
            data.len() as u32,
            vec![hash],
            vec![crate::TorrentFile::new("verified", data.len() as u64)],
        )
        .unwrap();
        let index = client.add_torrent(info.clone());
        crate::Storage::new(&root)
            .unwrap()
            .write_piece(&info, 0, data)
            .unwrap();

        let handle = client.spawn_verify(index).unwrap();
        let progress = std::sync::Arc::clone(handle.progress());
        assert_eq!(client.finish_transfer(handle), Ok(1));

        assert_eq!(progress.phase(), crate::TransferPhase::Finished);
        assert_eq!(progress.fraction(), 1.0);
        // Complete data does not resume a paused torrent.
        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Paused
        );

        // Seeding data that has gone missing stops seeding.
        client.torrents[index].seed();
        std::fs::remove_dir_all(&root).unwrap();
        let handle = client.spawn_verify(index).unwrap();
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(5));
        }
        client.finish_transfer(handle).unwrap();
        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Paused
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_background_download_is_paused() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Port 1 refuses connections, so the session backs off after the
        // failed announce, which is where cancellation must wake it.
        let metainfo = b"d8:announce27:http://127.0.0.1:1/announce4:infod6:lengthi4e\
4:name6:cancel12:piece lengthi4e6:pieces20:\
\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03ee";
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(&root)).unwrap();
        let index = client.add_torrent(TorrentInfo::from_bytes(metainfo).unwrap());

        let handle = client
            .spawn_download(index, [7; 20], Duration::from_millis(200))
            .unwrap();
        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Downloading
        );
        let started = std::time::Instant::now();
        while handle.progress().phase() != crate::TransferPhase::Waiting {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.progress().last_error().is_some());
        handle.cancel();

        assert_eq!(client.finish_transfer(handle), Err(BackendError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            client.torrents()[index].state(),
            crate::TransferState::Paused
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn magnet_links_fetch_metadata_from_a_seeding_runtime() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-magnet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = b"magnet payload";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let mut metainfo = format!(
            "d4:infod6:lengthi{}e4:name6:magnet12:piece lengthi{}e6:pieces20:",
            data.len(),
            data.len()
        )
        .into_bytes();
        metainfo.extend(hash);
        metainfo.extend(b"ee");
        let info = TorrentInfo::from_bytes(&metainfo).unwrap();
        let storage = crate::Storage::new(root.join("seed")).unwrap();
        storage.write_piece(&info, 0, data).unwrap();

        // A seeding runtime serves the metadata, as any EmpireBT seeder does.
        let seeder =
            crate::Seeder::new(info.clone(), storage, [1; 20], Duration::from_secs(5)).unwrap();
        let runtime = Arc::new(crate::Runtime::new(vec![seeder], 4).unwrap());
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            std::thread::spawn(move || runtime.serve_until(&listener, &shutdown))
        };

        let hex: String = info
            .info_hash()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let link = MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{hex}&dn=magnet&tr=http://127.0.0.1:1/announce&x.pe={address}"
        ))
        .unwrap();
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(root.join("get")))
                .unwrap();
        let index = client.add_magnet(link);
        assert!(client.torrents()[index].info().is_none());
        assert!(
            client
                .spawn_download(index, [2; 20], Duration::from_secs(1))
                .is_err()
        );

        let handle = client
            .spawn_metadata(index, [2; 20], Duration::from_secs(5))
            .unwrap();
        let started = Instant::now();
        while !handle.is_finished() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "metadata fetch stalled"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        client.finish_transfer(handle).unwrap();

        let torrent = &client.torrents()[index];
        assert_eq!(torrent.info().unwrap().info_hash(), info.info_hash());
        assert_eq!(torrent.info().unwrap().info_bytes(), info.info_bytes());
        // The magnet's tracker is kept on the fetched torrent.
        assert_eq!(
            torrent.info().unwrap().tracker_urls(),
            ["http://127.0.0.1:1/announce"]
        );
        assert!(torrent.magnet().is_none());
        assert_eq!(torrent.state(), crate::TransferState::Downloading);

        // Fetched metadata persists like any .torrent.
        let session = root.join("session.dat");
        client.save_session(&session).unwrap();
        let restored = Client::load_session(ClientConfig::default(), &session).unwrap();
        assert_eq!(restored.torrents()[0].info_hash(), Some(info.info_hash()));

        shutdown.store(true, std::sync::atomic::Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn v2_magnet_links_fetch_metadata_and_piece_layers() {
        use crate::bencode::Value;
        let root_dir =
            std::env::temp_dir().join(format!("empirebt-client-v2-magnet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root_dir);
        // Seven 16 KiB pieces, so the file needs a piece layer.
        let data: Vec<u8> = (0..100_000).map(|byte| (byte * 13 % 251) as u8).collect();
        let pieces_root = crate::v2::file_root(&data, 16_384);
        let layer = crate::v2::piece_roots(&data, 16_384);
        assert_eq!(layer.len(), 7);
        let info = Value::dict([
            (
                b"file tree",
                Value::dict([(
                    b"data.bin",
                    Value::dict([(
                        b"",
                        Value::dict([
                            (b"length", Value::Int(data.len() as i64)),
                            (b"pieces root", Value::bytes(pieces_root)),
                        ]),
                    )]),
                )]),
            ),
            (b"meta version", Value::Int(2)),
            (b"name", Value::bytes(*b"data.bin")),
            (b"piece length", Value::Int(16_384)),
        ])
        .encode();
        let torrent =
            TorrentInfo::from_metadata_with_layers(&info, &[(pieces_root, layer.clone())], &[])
                .unwrap();
        assert_eq!(torrent.format(), crate::TorrentFormat::V2);
        let storage = crate::Storage::new(root_dir.join("seed")).unwrap();
        storage.write("data.bin", &data).unwrap();

        let seeder =
            crate::V2Seeder::new(torrent.clone(), storage, [1; 20], Duration::from_secs(5))
                .unwrap();
        let runtime = Arc::new(crate::Runtime::new_mixed(Vec::new(), vec![seeder], 4).unwrap());
        let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server = {
            let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
            std::thread::spawn(move || runtime.serve_until(&listener, &shutdown))
        };

        let v2: String = torrent
            .v2_info_hash()
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let link =
            MagnetLink::parse(&format!("magnet:?xt=urn:btmh:1220{v2}&x.pe={address}")).unwrap();
        let metadata =
            crate::fetch_metadata_from_peer(address, &link, [2; 20], Duration::from_secs(5))
                .unwrap();
        assert_eq!(metadata.info, info);
        assert_eq!(metadata.piece_layers, vec![(pieces_root, layer)]);

        shutdown.store(true, std::sync::atomic::Ordering::Release);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn pending_magnets_survive_a_session_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "empirebt-client-magnet-session-{}.dat",
            std::process::id()
        ));
        let mut client = Client::new();
        let link = MagnetLink::parse(
            "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=Pending",
        )
        .unwrap();
        let index = client.add_magnet(link.clone());
        client.torrents[index].start();
        client.save_session(&path).unwrap();

        let restored = Client::load_session(ClientConfig::default(), &path).unwrap();
        assert_eq!(restored.torrents()[0].magnet(), Some(&link));
        assert_eq!(restored.torrents()[0].name(), "Pending");
        assert_eq!(
            restored.torrents()[0].state(),
            crate::TransferState::Downloading
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn session_directories_save_incrementally_and_skip_damaged_files() {
        let dir = std::env::temp_dir().join(format!("empirebt-session-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let metainfo = |name: &str| {
            let mut bytes = format!(
                "d4:infod6:lengthi4e4:name{}:{name}12:piece lengthi4e6:pieces20:",
                name.len()
            )
            .into_bytes();
            bytes.extend([3; 20]);
            bytes.extend(b"ee");
            TorrentInfo::from_bytes(&bytes).unwrap()
        };
        let mut client = Client::new();
        client.add_torrent(metainfo("first"));
        client.add_torrent(metainfo("second"));
        let magnet = client.add_magnet(
            MagnetLink::parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a")
                .unwrap(),
        );
        client.torrents[1].seed();
        client.torrents[magnet].start();
        client.save_session_dir(&dir).unwrap();
        let file_count = || {
            std::fs::read_dir(dir.join(SESSION_TORRENTS))
                .unwrap()
                .count()
        };
        assert_eq!(file_count(), 3);

        // A change rewrites only the index; metadata files are untouched.
        let first = dir.join(SESSION_TORRENTS).join(format!(
            "{}.torrent",
            hex(client.torrents[0].info_hash().unwrap())
        ));
        let written = std::fs::metadata(&first).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        client.torrents[0].start();
        client.remove_torrent(magnet).unwrap();
        client.save_session_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&first).unwrap().modified().unwrap(),
            written
        );
        assert_eq!(file_count(), 2, "the removed magnet's file is cleaned up");

        let (restored, warnings) = Client::load_session_dir(ClientConfig::default(), &dir).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(restored, client);

        // A damaged file loses one torrent, not the session.
        std::fs::write(&first, b"garbage").unwrap();
        let (restored, warnings) = Client::load_session_dir(ClientConfig::default(), &dir).unwrap();
        assert_eq!(restored.torrents().len(), 1);
        assert_eq!(restored.torrents()[0].name(), "second");
        assert_eq!(
            restored.torrents()[0].state(),
            crate::TransferState::Seeding
        );
        assert_eq!(warnings.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resume_data_is_recorded_saved_and_restored() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = b"resumedata";
        let hash: [u8; 20] = {
            use sha1::{Digest, Sha1};
            Sha1::digest(data).into()
        };
        let mut bytes = format!(
            "d4:infod6:lengthi{0}e4:name6:resume12:piece lengthi{0}e6:pieces20:",
            data.len()
        )
        .into_bytes();
        bytes.extend(hash);
        bytes.extend(b"ee");
        let info = TorrentInfo::from_bytes(&bytes).unwrap();
        let config = ClientConfig::new().with_download_directory(root.join("data"));
        let mut client = Client::with_config(config.clone()).unwrap();
        let index = client.add_torrent(info.clone());
        client.storage.write_piece(&info, 0, data).unwrap();

        let handle = client.spawn_verify(index).unwrap();
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(5));
        }
        client.finish_transfer(handle).unwrap();
        let resume = client.torrents()[index].resume().cloned().unwrap();
        assert_eq!(resume.verified_pieces(), 1);

        let dir = root.join("session");
        client.save_session_dir(&dir).unwrap();
        let (restored, _) = Client::load_session_dir(config, &dir).unwrap();
        let restored_resume = restored.torrents()[0].resume().unwrap();
        assert_eq!(restored_resume, &resume);
        assert!(restored_resume.is_current(&info, &client.storage));

        // Removing the torrent removes its resume file too.
        client.remove_torrent(index).unwrap();
        client.save_session_dir(&dir).unwrap();
        assert_eq!(
            std::fs::read_dir(dir.join(SESSION_RESUME)).unwrap().count(),
            0
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removes_torrents_by_index() {
        let mut client = Client::new();
        let info = TorrentInfo::new(
            "removed",
            16_384,
            Vec::new(),
            vec![crate::TorrentFile::new("removed", 0)],
        )
        .unwrap();
        let hash = *info.info_hash();
        let index = client.add_torrent(info);
        assert_eq!(client.torrent_index(&hash), Some(index));

        assert_eq!(client.remove_torrent(index).unwrap().name(), "removed");
        assert!(client.torrents().is_empty());
        assert!(client.remove_torrent(index).is_err());
    }

    #[test]
    fn session_round_trips_parsed_torrent_and_state() {
        let root =
            std::env::temp_dir().join(format!("empirebt-client-session-{}", std::process::id()));
        let torrent_path = root.join("example.torrent");
        let session_path = root.join("session.dat");
        std::fs::create_dir_all(&root).unwrap();
        let metainfo = b"d4:infod6:lengthi4e4:name4:test12:piece lengthi4e6:pieces20:\
\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03\x03e\
e";
        std::fs::write(&torrent_path, metainfo).unwrap();

        let mut client = Client::new();
        let index = client.add_torrent_file(&torrent_path).unwrap();
        client.start_torrent(index).unwrap();
        client.save_session(&session_path).unwrap();

        let restored = Client::load_session(ClientConfig::default(), &session_path).unwrap();
        assert_eq!(restored.torrents().len(), 1);
        assert_eq!(
            restored.torrents()[0].state(),
            crate::TransferState::Downloading
        );
        assert_eq!(
            restored.torrents()[0].info().unwrap().info_hash(),
            client.torrents()[0].info().unwrap().info_hash()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Needs network access: resolves a real trackerless magnet link (the
    /// Creative Commons film Sintel) through the public DHT, starting from
    /// an empty routing table. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn fetches_public_magnet_metadata_through_the_dht() {
        let port = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| socket.local_addr())
            .unwrap()
            .port();
        let dht = Dht::start(port, Dht::default_bootstrap(), None).unwrap();
        let mut client = Client::new();
        client.set_dht(Some(Arc::clone(&dht)));
        let index = client.add_magnet(
            MagnetLink::parse("magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10")
                .unwrap(),
        );
        let handle = client
            .spawn_metadata(index, crate::dht::random_bytes(), Duration::from_secs(10))
            .unwrap();
        let started = Instant::now();
        while !handle.is_finished() && started.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_millis(250));
        }
        handle.cancel();
        let result = client.finish_transfer(handle);
        dht.stop();
        result.unwrap();
        let info = client.torrents()[index].info().unwrap();
        assert_eq!(info.name(), "Sintel");
        assert_eq!(info.files().len(), 11);
    }
}
