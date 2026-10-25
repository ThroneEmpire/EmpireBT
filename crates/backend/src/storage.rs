use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::{TorrentInfo, V2File};

/// Open files kept for reuse. Seeding thousands of torrents would otherwise
/// open and close a file for every 16 KiB block, and could exceed the
/// operating system's limit on open files.
const DEFAULT_OPEN_FILES: usize = 512;

/// Local filesystem location used for torrent data. Clones share one pool
/// of open files.
#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
    files: Arc<FilePool>,
    /// A torrent's files whose names on disk differ from the torrent's.
    renames: Option<Renames>,
}

/// The names a torrent's files have on disk where they differ from the
/// torrent's own (renamed files, or another content layout): the
/// torrent's relative path to the one used instead. One table is shared
/// by the torrent's transfers, seeders, and the client, so a rename
/// applies to all of them at once.
#[derive(Debug, Clone, Default)]
pub struct Renames(Arc<RwLock<Arc<BTreeMap<PathBuf, PathBuf>>>>);

impl Renames {
    pub fn new(table: BTreeMap<PathBuf, PathBuf>) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(table))))
    }

    /// The table as it is now.
    pub fn table(&self) -> Arc<BTreeMap<PathBuf, PathBuf>> {
        self.0
            .read()
            .map(|table| Arc::clone(&table))
            .unwrap_or_default()
    }

    pub fn replace(&self, table: BTreeMap<PathBuf, PathBuf>) {
        if let Ok(mut current) = self.0.write() {
            *current = Arc::new(table);
        }
    }

    /// Where a file of the torrent is, relative to its folder.
    pub fn resolve(&self, original: &Path) -> PathBuf {
        self.table()
            .get(original)
            .cloned()
            .unwrap_or_else(|| original.to_owned())
    }

    pub fn is_empty(&self) -> bool {
        self.table().is_empty()
    }
}

impl PartialEq for Renames {
    fn eq(&self, other: &Self) -> bool {
        self.table() == other.table()
    }
}

impl Eq for Renames {}

impl PartialEq for Storage {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for Storage {}

/// A bounded cache of open files, closing the least recently used first.
/// Handles are shared, so every access through them must be positional.
///
/// The cache is split into independently locked shards, so threads reading
/// different files rarely wait on each other and each eviction scans only a
/// small shard.
#[derive(Debug)]
struct FilePool {
    shard_capacity: usize,
    shards: Vec<Mutex<Shard>>,
}

#[derive(Debug)]
struct Shard {
    files: HashMap<PathBuf, Slot>,
    clock: u64,
    last_sweep: Instant,
}

impl Default for Shard {
    fn default() -> Self {
        Self {
            files: HashMap::new(),
            clock: 0,
            last_sweep: Instant::now(),
        }
    }
}

#[derive(Debug)]
struct Slot {
    file: Arc<File>,
    writable: bool,
    last_used: u64,
    used_at: Instant,
}

const POOL_SHARDS: usize = 16;
/// Handles unused this long are closed, so an idle client does not keep
/// hundreds of files open (on Windows, open files cannot be moved or
/// deleted).
const IDLE_CLOSE: Duration = Duration::from_secs(5 * 60);
const SWEEP_EVERY: Duration = Duration::from_secs(30);

impl FilePool {
    fn new(capacity: usize) -> Self {
        let shards = POOL_SHARDS.min(capacity.max(1));
        Self {
            shard_capacity: capacity.max(1).div_ceil(shards),
            shards: (0..shards).map(|_| Mutex::new(Shard::default())).collect(),
        }
    }

    fn shard(&self, path: &Path) -> &Mutex<Shard> {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(path);
        &self.shards[hash as usize % self.shards.len()]
    }

    /// Returns an open handle, opening the file if needed. A read-only
    /// handle is upgraded when writing is requested.
    fn get(&self, path: &Path, writable: bool) -> std::io::Result<Arc<File>> {
        let shard = self.shard(path);
        let mut read_only = None;
        if let Ok(mut state) = shard.lock() {
            state.clock += 1;
            let now = state.clock;
            match state.files.get_mut(path) {
                Some(slot) if slot.writable || !writable => {
                    slot.last_used = now;
                    slot.used_at = Instant::now();
                    return Ok(Arc::clone(&slot.file));
                }
                // Becoming writable: the read-only handle leaves the pool
                // first (below), so it is closed before the file is opened
                // again. Some file systems (FUSE passthrough, Linux 6.9+)
                // refuse a read-write open while a read-only one is open.
                Some(_) => read_only = state.files.remove(path),
                None => {}
            }
        }
        // Closed now unless a reader still holds it; then the open below
        // may fail on such file systems as before, and is retried later.
        drop(read_only);
        // Opened without holding the lock, so a slow open never blocks
        // other files.
        let file = Arc::new(if writable {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Pieces land at arbitrary offsets, so existing data must survive.
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path)?
        } else {
            File::open(path)?
        });
        let evicted = shard.lock().ok().map(|mut state| {
            state.clock += 1;
            let slot = Slot {
                file: Arc::clone(&file),
                writable,
                last_used: state.clock,
                used_at: Instant::now(),
            };
            state.files.insert(path.to_owned(), slot);
            let mut closing = Vec::new();
            if state.last_sweep.elapsed() >= SWEEP_EVERY {
                state.last_sweep = Instant::now();
                let idle: Vec<PathBuf> = state
                    .files
                    .iter()
                    .filter(|(_, slot)| slot.used_at.elapsed() >= IDLE_CLOSE)
                    .map(|(path, _)| path.clone())
                    .collect();
                closing.extend(idle.iter().filter_map(|path| state.files.remove(path)));
            }
            if state.files.len() > self.shard_capacity
                && let Some(oldest) = state
                    .files
                    .iter()
                    .min_by_key(|(_, slot)| slot.last_used)
                    .map(|(path, _)| path.clone())
            {
                closing.extend(state.files.remove(&oldest));
            }
            closing
        });
        // Closing a file is a system call; do it after releasing the lock.
        // In-flight users keep their own reference until done.
        drop(evicted);
        Ok(file)
    }

    /// Drops a cached handle, for files replaced as a whole.
    fn forget(&self, path: &Path) {
        let removed = self
            .shard(path)
            .lock()
            .ok()
            .and_then(|mut state| state.files.remove(path));
        drop(removed);
    }

    /// Closes handles that have not been used recently.
    fn close_idle(&self, idle: Duration) {
        for shard in &self.shards {
            let closing: Vec<Slot> = match shard.lock() {
                Ok(mut state) => {
                    state.last_sweep = Instant::now();
                    let stale: Vec<PathBuf> = state
                        .files
                        .iter()
                        .filter(|(_, slot)| slot.used_at.elapsed() >= idle)
                        .map(|(path, _)| path.clone())
                        .collect();
                    stale
                        .iter()
                        .filter_map(|path| state.files.remove(path))
                        .collect()
                }
                Err(_) => Vec::new(),
            };
            drop(closing);
        }
    }

    fn open_count(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.lock().map_or(0, |state| state.files.len()))
            .sum()
    }
}

impl Storage {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, crate::BackendError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(crate::BackendError::InvalidConfiguration(
                "storage root cannot be empty".to_owned(),
            ));
        }
        Ok(Self {
            root,
            files: Arc::new(FilePool::new(DEFAULT_OPEN_FILES)),
            renames: None,
        })
    }

    /// The same storage rooted at `root`, sharing this one's pool of open
    /// files: each torrent can live in a folder of its own.
    pub fn at(&self, root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            files: Arc::clone(&self.files),
            renames: None,
        }
    }

    /// The same storage, finding a torrent's files under the names
    /// `renames` gives them.
    pub fn with_renames(mut self, renames: Renames) -> Self {
        self.renames = Some(renames);
        self
    }

    /// Closes a cached handle to the file at `path`, for files moved or
    /// renamed on disk.
    pub(crate) fn forget(&self, path: &Path) {
        self.files.forget(path);
    }

    /// Changes how many files may stay open at once.
    pub fn with_open_file_limit(mut self, limit: usize) -> Self {
        self.files = Arc::new(FilePool::new(limit));
        self
    }

    /// Closes open files unused for at least `idle`. Files also close on
    /// their own after five minutes without use, whenever the pool is used.
    pub fn close_idle_files(&self, idle: Duration) {
        self.files.close_idle(idle);
    }

    /// Files currently held open for reuse.
    pub fn open_files(&self) -> usize {
        self.files.open_count()
    }

    /// Closes cached handles to a torrent's files, so the next access
    /// reopens them by path. Checks call this first: a cached handle can
    /// keep a deleted or replaced file readable, and a check must see what
    /// is on disk now.
    pub fn release_files(&self, info: &TorrentInfo) {
        for file in info.files() {
            if let Some(path) = self.path_for(file.path()) {
                self.files.forget(&path);
            }
        }
    }

    /// Opens a file for reading through the pool. `Ok(None)` means it does
    /// not exist yet.
    fn reader(&self, path: &Path) -> Result<Option<Arc<File>>, crate::BackendError> {
        match self.files.get(path, false) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(crate::BackendError::Io(error.to_string())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, relative_path: impl AsRef<Path>) -> Option<PathBuf> {
        let relative_path = relative_path.as_ref();
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return None;
        }
        match &self.renames {
            Some(renames) => Some(self.root.join(renames.resolve(relative_path))),
            None => Some(self.root.join(relative_path)),
        }
    }

    /// Reads a verified-safe relative path from storage.
    pub fn read(&self, relative_path: impl AsRef<Path>) -> Result<Vec<u8>, crate::BackendError> {
        let path = self.path_for(relative_path).ok_or_else(|| {
            crate::BackendError::InvalidConfiguration(
                "storage path must be relative and cannot contain '..'".to_owned(),
            )
        })?;
        std::fs::read(path).map_err(|error| crate::BackendError::Io(error.to_string()))
    }

    /// Writes a file under storage, creating parent directories as needed.
    pub fn write(
        &self,
        relative_path: impl AsRef<Path>,
        data: &[u8],
    ) -> Result<(), crate::BackendError> {
        let path = self.path_for(relative_path).ok_or_else(|| {
            crate::BackendError::InvalidConfiguration(
                "storage path must be relative and cannot contain '..'".to_owned(),
            )
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        }
        self.files.forget(&path);
        std::fs::write(path, data).map_err(|error| crate::BackendError::Io(error.to_string()))
    }

    /// Writes one complete piece across the torrent's file boundaries.
    pub fn write_piece(
        &self,
        info: &TorrentInfo,
        piece_index: usize,
        data: &[u8],
    ) -> Result<(), crate::BackendError> {
        let expected = info
            .piece_size(piece_index)
            .ok_or_else(|| invalid_storage("piece index is out of range"))?;
        if data.len() as u64 != expected {
            return Err(invalid_storage("piece data has an invalid length"));
        }

        let mut global_offset = piece_index as u64 * info.piece_length() as u64;
        let mut remaining = data;
        for file in info.files() {
            if global_offset >= file.length() {
                global_offset -= file.length();
                continue;
            }
            let amount = remaining
                .len()
                .min((file.length() - global_offset) as usize);
            // Padding is implied zeros, never stored.
            if !file.is_padding() {
                self.write_at(file.path(), global_offset, &remaining[..amount])?;
            }
            remaining = &remaining[amount..];
            global_offset = 0;
            if remaining.is_empty() {
                break;
            }
        }
        debug_assert!(remaining.is_empty());
        Ok(())
    }

    /// Reads one complete piece across the torrent's file boundaries.
    pub fn read_piece(
        &self,
        info: &TorrentInfo,
        piece_index: usize,
    ) -> Result<Vec<u8>, crate::BackendError> {
        let expected = info
            .piece_size(piece_index)
            .ok_or_else(|| invalid_storage("piece index is out of range"))?;
        self.read_range(
            info,
            piece_index as u64 * info.piece_length() as u64,
            expected,
        )
    }

    /// Reads `length` bytes starting at a torrent-wide byte offset, across
    /// file boundaries. Returns an empty buffer when a covered file does not
    /// exist yet.
    pub fn read_range(
        &self,
        info: &TorrentInfo,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, crate::BackendError> {
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= info.total_length())
            .ok_or_else(|| invalid_storage("read range is outside the torrent"))?;
        let mut data = Vec::with_capacity((end - offset) as usize);
        let mut global_offset = offset;
        for file in info.files() {
            if global_offset >= file.length() {
                global_offset -= file.length();
                continue;
            }
            let amount = (file.length() - global_offset).min(length - data.len() as u64);
            if file.is_padding() {
                data.resize(data.len() + amount as usize, 0);
                global_offset = 0;
                if data.len() as u64 == length {
                    break;
                }
                continue;
            }
            let path = self
                .path_for(file.path())
                .ok_or_else(|| invalid_storage("unsafe file path"))?;
            let Some(handle) = self.reader(&path)? else {
                return Ok(Vec::new());
            };
            if !covers(&handle, global_offset, amount)? {
                return Ok(Vec::new());
            }
            let mut bytes = vec![0; amount as usize];
            read_exact_at(&handle, &mut bytes, global_offset)?;
            data.extend(bytes);
            global_offset = 0;
            if data.len() as u64 == length {
                break;
            }
        }
        Ok(data)
    }

    /// Finds already downloaded pieces by reading and checking their SHA-1 hashes.
    pub fn scan_complete_pieces(
        &self,
        info: &TorrentInfo,
    ) -> Result<Vec<bool>, crate::BackendError> {
        self.release_files(info);
        let mut complete = Vec::with_capacity(info.piece_count());
        for (index, expected) in info.pieces().iter().enumerate() {
            let data = self.read_piece(info, index)?;
            let actual: [u8; 20] = Sha1::digest(&data).into();
            complete
                .push(data.len() as u64 == info.piece_size(index).unwrap() && actual == *expected);
        }
        Ok(complete)
    }

    /// Finds v2 files whose BEP 52 Merkle roots and piece layers match the
    /// bytes currently stored on disk.
    pub fn scan_complete_v2_files(
        &self,
        info: &TorrentInfo,
    ) -> Result<Vec<bool>, crate::BackendError> {
        self.release_files(info);
        info.v2_files()
            .iter()
            .map(|file| self.verify_v2_file(info, file))
            .collect()
    }

    /// Whether every torrent file exists with exactly its expected length.
    /// A cheap check that previously verified data is still in place,
    /// without re-hashing it.
    pub fn has_expected_files(&self, info: &TorrentInfo) -> Result<bool, crate::BackendError> {
        for file in info.files().iter().filter(|file| !file.is_padding()) {
            let path = self
                .path_for(file.path())
                .ok_or_else(|| invalid_storage("unsafe file path"))?;
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.len() == file.length() => {}
                Ok(_) => return Ok(false),
                // Empty files hold no data to serve.
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound && file.length() == 0 => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(crate::BackendError::Io(error.to_string())),
            }
        }
        Ok(true)
    }

    /// Creates the torrent's empty files, which no piece covers, so a
    /// finished download contains every file.
    pub fn create_empty_files(&self, info: &TorrentInfo) -> Result<(), crate::BackendError> {
        for file in info.files() {
            if file.length() != 0 || file.is_padding() {
                continue;
            }
            let path = self
                .path_for(file.path())
                .ok_or_else(|| invalid_storage("unsafe file path"))?;
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| crate::BackendError::Io(error.to_string()))?;
                }
                std::fs::File::create(&path)
                    .map_err(|error| crate::BackendError::Io(error.to_string()))?;
            }
        }
        Ok(())
    }

    /// Reads one block of a v2 piece without reading the rest of the piece.
    pub fn read_v2_block(
        &self,
        file: &crate::V2File,
        piece_length: u32,
        piece_index: usize,
        begin: u32,
        length: u32,
    ) -> Result<Vec<u8>, crate::BackendError> {
        let piece_offset = (piece_index as u64)
            .checked_mul(u64::from(piece_length))
            .ok_or_else(|| invalid_storage("v2 piece offset overflowed"))?;
        let piece_size = file
            .length()
            .checked_sub(piece_offset)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| invalid_storage("v2 piece index is out of range"))?
            .min(u64::from(piece_length));
        if u64::from(begin) + u64::from(length) > piece_size {
            return Err(invalid_storage("v2 block is outside the piece"));
        }
        let path = self
            .path_for(file.path())
            .ok_or_else(|| invalid_storage("unsafe v2 file path"))?;
        let handle = self.reader(&path)?.ok_or_else(|| {
            crate::BackendError::Io("verified v2 data is missing from disk".to_owned())
        })?;
        let offset = piece_offset + u64::from(begin);
        if !covers(&handle, offset, u64::from(length))? {
            return Err(crate::BackendError::Io(
                "verified v2 data is missing from disk".to_owned(),
            ));
        }
        let mut data = vec![0; length as usize];
        read_exact_at(&handle, &mut data, offset)?;
        Ok(data)
    }

    /// Reads one piece-length region from a v2 file.
    pub fn read_v2_piece(
        &self,
        file: &crate::V2File,
        piece_length: u32,
        piece_index: usize,
    ) -> Result<Vec<u8>, crate::BackendError> {
        let offset = (piece_index as u64)
            .checked_mul(u64::from(piece_length))
            .ok_or_else(|| invalid_storage("v2 piece offset overflowed"))?;
        if offset >= file.length() {
            return Err(invalid_storage("v2 piece index is out of range"));
        }
        let length = (file.length() - offset).min(u64::from(piece_length));
        let path = self
            .path_for(file.path())
            .ok_or_else(|| invalid_storage("unsafe v2 file path"))?;
        let Some(handle) = self.reader(&path)? else {
            return Ok(Vec::new());
        };
        if !covers(&handle, offset, length)? {
            return Ok(Vec::new());
        }
        let mut data = vec![0; length as usize];
        read_exact_at(&handle, &mut data, offset)?;
        Ok(data)
    }

    /// Writes one piece-length region to a v2 file.
    pub fn write_v2_piece(
        &self,
        file: &crate::V2File,
        piece_length: u32,
        piece_index: usize,
        data: &[u8],
    ) -> Result<(), crate::BackendError> {
        let offset = (piece_index as u64)
            .checked_mul(u64::from(piece_length))
            .ok_or_else(|| invalid_storage("v2 piece offset overflowed"))?;
        if offset >= file.length() {
            return Err(invalid_storage("v2 piece index is out of range"));
        }
        let expected = (file.length() - offset).min(u64::from(piece_length)) as usize;
        if data.len() != expected {
            return Err(invalid_storage("v2 piece data has an invalid length"));
        }
        self.write_at(file.path(), offset, data)
    }

    fn verify_v2_file(
        &self,
        info: &TorrentInfo,
        file: &V2File,
    ) -> Result<bool, crate::BackendError> {
        // Empty files have no hashes to check.
        if file.length() == 0 {
            return Ok(true);
        }
        let data = self.read_file(file.path(), file.length())?;
        if data.len() as u64 != file.length() {
            return Ok(false);
        }

        let roots = crate::v2::piece_roots(&data, info.piece_length());
        if roots.len() > 1 {
            let Some(layer) = info
                .v2_piece_layers()
                .iter()
                .find(|layer| layer.root() == file.pieces_root())
            else {
                return Ok(false);
            };
            if layer.pieces() != roots.as_slice() {
                return Ok(false);
            }
        }
        Ok(crate::v2::file_root(&data, info.piece_length()) == *file.pieces_root())
    }

    fn read_file(
        &self,
        relative_path: impl AsRef<Path>,
        length: u64,
    ) -> Result<Vec<u8>, crate::BackendError> {
        let path = self.path_for(relative_path).ok_or_else(|| {
            invalid_storage("storage path must be relative and cannot contain '..'")
        })?;
        let Some(file) = self.reader(&path)? else {
            return Ok(Vec::new());
        };
        // Positional reads only: pooled handles are shared between threads,
        // so the file cursor must never be used.
        let size = file
            .metadata()
            .map_err(|error| crate::BackendError::Io(error.to_string()))?
            .len()
            .min(length);
        let size = usize::try_from(size)
            .map_err(|_| invalid_storage("v2 file is too large for this platform"))?;
        let mut data = vec![0; size];
        read_exact_at(&file, &mut data, 0)?;
        Ok(data)
    }

    fn write_at(
        &self,
        relative_path: impl AsRef<Path>,
        offset: u64,
        data: &[u8],
    ) -> Result<(), crate::BackendError> {
        let path = self
            .path_for(relative_path)
            .ok_or_else(|| invalid_storage("unsafe file path"))?;
        let file = self
            .files
            .get(&path, true)
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        write_all_at(&file, data, offset)
    }
}

fn invalid_storage(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::InvalidConfiguration(message.into())
}

/// Whether a file is long enough to hold `length` bytes at `offset`. A
/// partially downloaded file is shorter than the torrent says, and the
/// missing tail must read as absent data rather than as an I/O failure.
fn covers(file: &std::fs::File, offset: u64, length: u64) -> Result<bool, crate::BackendError> {
    let size = file
        .metadata()
        .map_err(|error| crate::BackendError::Io(error.to_string()))?
        .len();
    Ok(offset.saturating_add(length) <= size)
}

#[cfg(unix)]
fn read_exact_at(
    file: &std::fs::File,
    buffer: &mut [u8],
    offset: u64,
) -> Result<(), crate::BackendError> {
    use std::os::unix::fs::FileExt;

    let mut position = 0;
    while position < buffer.len() {
        let read = file
            .read_at(&mut buffer[position..], offset + position as u64)
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        if read == 0 {
            return Err(crate::BackendError::Io(
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string(),
            ));
        }
        position += read;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(
    file: &std::fs::File,
    buffer: &mut [u8],
    offset: u64,
) -> Result<(), crate::BackendError> {
    use std::os::windows::fs::FileExt;

    let mut position = 0;
    while position < buffer.len() {
        let read = file
            .seek_read(&mut buffer[position..], offset + position as u64)
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        if read == 0 {
            return Err(crate::BackendError::Io(
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string(),
            ));
        }
        position += read;
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(
    file: &std::fs::File,
    buffer: &[u8],
    offset: u64,
) -> Result<(), crate::BackendError> {
    use std::os::unix::fs::FileExt;

    let mut position = 0;
    while position < buffer.len() {
        let written = file
            .write_at(&buffer[position..], offset + position as u64)
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        if written == 0 {
            return Err(crate::BackendError::Io(
                std::io::Error::from(std::io::ErrorKind::WriteZero).to_string(),
            ));
        }
        position += written;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(
    file: &std::fs::File,
    buffer: &[u8],
    offset: u64,
) -> Result<(), crate::BackendError> {
    use std::os::windows::fs::FileExt;

    let mut position = 0;
    while position < buffer.len() {
        let written = file
            .seek_write(&buffer[position..], offset + position as u64)
            .map_err(|error| crate::BackendError::Io(error.to_string()))?;
        if written == 0 {
            return Err(crate::BackendError::Io(
                std::io::Error::from(std::io::ErrorKind::WriteZero).to_string(),
            ));
        }
        position += written;
    }
    Ok(())
}

/// What [`delete_files`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deleted {
    pub files: usize,
    pub bytes: u64,
    /// Files that could not be deleted, with why.
    pub failures: Vec<String>,
}

/// Deletes `paths` (files inside `root`) and then the folders under `root`
/// they leave empty; `root` itself stays. Files already gone are fine.
pub fn delete_files(root: &Path, paths: &[PathBuf]) -> Deleted {
    let mut deleted = Deleted::default();
    for path in paths.iter().filter(|path| path.starts_with(root)) {
        let size = std::fs::symlink_metadata(path).map(|meta| meta.len());
        match std::fs::remove_file(path) {
            Ok(()) => {
                deleted.files += 1;
                deleted.bytes += size.unwrap_or(0);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => deleted
                .failures
                .push(format!("{}: {error}", path.display())),
        }
    }
    // Deepest folders first, so a parent empties once its children go.
    let mut folders: Vec<&Path> = paths
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|folder| folder.starts_with(root) && *folder != root)
        .collect();
    folders.sort_by_key(|folder| std::cmp::Reverse(folder.components().count()));
    folders.dedup();
    for folder in folders {
        // Only empty ones go; anything else in them stays.
        let _ = std::fs::remove_dir(folder);
    }
    deleted
}

#[cfg(test)]
mod tests {
    /// A file read first and written later: its read-only handle leaves the
    /// pool before the read-write one opens (some file systems refuse both
    /// at once), the write lands, and reads then share the writable handle.
    #[test]
    fn upgrades_a_read_only_handle_by_closing_it_first() {
        let dir = std::env::temp_dir().join(format!("empirebt-pool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data");
        std::fs::write(&path, b"old data").unwrap();
        let pool = FilePool::new(8);

        let reader = pool.get(&path, false).unwrap();
        assert_eq!(Arc::strong_count(&reader), 2, "the pool holds it too");
        drop(reader);
        let writer = pool.get(&path, true).unwrap();
        assert_eq!(pool.open_count(), 1, "only the writable handle stays");
        write_all_at(&writer, b"new", 0).unwrap();
        let again = pool.get(&path, false).unwrap();
        assert!(
            Arc::ptr_eq(&writer, &again),
            "reads reuse the writable handle"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new data");

        // A reader still holding the old handle keeps it open, but the pool
        // lets go of it all the same.
        let other = dir.join("other");
        std::fs::write(&other, b"x").unwrap();
        let held = pool.get(&other, false).unwrap();
        let upgraded = pool.get(&other, true).unwrap();
        assert!(!Arc::ptr_eq(&held, &upgraded));
        assert_eq!(Arc::strong_count(&held), 1, "only the reader holds it now");
        let _ = std::fs::remove_dir_all(dir);
    }

    use super::*;

    #[test]
    fn writes_and_reads_relative_file() {
        let root = std::env::temp_dir().join(format!("empirebt-storage-{}", std::process::id()));
        let storage = Storage::new(&root).unwrap();
        storage.write("nested/piece", b"data").unwrap();
        assert_eq!(storage.read("nested/piece").unwrap(), b"data");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn writes_and_scans_piece_across_files() {
        let root = std::env::temp_dir().join(format!("empirebt-piece-{}", std::process::id()));
        let storage = Storage::new(&root).unwrap();
        let first = crate::TorrentFile::new("root/a", 3);
        let second = crate::TorrentFile::new("root/b", 2);
        let data = b"abcde";
        let hash: [u8; 20] = Sha1::digest(data).into();
        let info = TorrentInfo::new("root", 5, vec![hash], vec![first, second]).unwrap();

        storage.write_piece(&info, 0, data).unwrap();
        assert_eq!(storage.read_piece(&info, 0).unwrap(), data);
        assert_eq!(storage.scan_complete_pieces(&info).unwrap(), vec![true]);
        assert_eq!(storage.read_range(&info, 2, 2).unwrap(), b"cd");
        assert!(storage.read_range(&info, 4, 2).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reuses_open_files_within_a_limit() {
        let root = std::env::temp_dir().join(format!("empirebt-pool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = Storage::new(&root).unwrap().with_open_file_limit(3);
        let infos: Vec<TorrentInfo> = (0..5)
            .map(|index| {
                let data = [index as u8; 4];
                let hash: [u8; 20] = Sha1::digest(data).into();
                let name = format!("file-{index}");
                TorrentInfo::new(
                    &name,
                    4,
                    vec![hash],
                    vec![crate::TorrentFile::new(&name, 4)],
                )
                .unwrap()
            })
            .collect();
        for (index, info) in infos.iter().enumerate() {
            storage.write_piece(info, 0, &[index as u8; 4]).unwrap();
        }
        assert!(storage.open_files() <= 3, "never more than the limit");
        // Clones share the pool, and reads see data written through it.
        let clone = storage.clone();
        for (index, info) in infos.iter().enumerate() {
            assert_eq!(clone.read_piece(info, 0).unwrap(), [index as u8; 4]);
        }
        assert!(storage.open_files() <= 3);
        storage.close_idle_files(Duration::ZERO);
        assert_eq!(storage.open_files(), 0);
        // Whole-file rewrites are not hidden behind a stale handle.
        storage.write("file-4", b"zzzz").unwrap();
        assert_eq!(storage.read_range(&infos[4], 0, 4).unwrap(), b"zzzz");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn partially_written_files_scan_as_missing_pieces() {
        let root = std::env::temp_dir().join(format!("empirebt-partial-{}", std::process::id()));
        let storage = Storage::new(&root).unwrap();
        let data = b"abcdef";
        let hashes = data
            .chunks(2)
            .map(|piece| Sha1::digest(piece).into())
            .collect();
        let info = TorrentInfo::new(
            "partial",
            2,
            hashes,
            vec![crate::TorrentFile::new("partial", 6)],
        )
        .unwrap();

        // Only the first piece has been written, so the file is short.
        storage.write_piece(&info, 0, b"ab").unwrap();
        assert_eq!(
            storage.scan_complete_pieces(&info).unwrap(),
            vec![true, false, false]
        );
        assert!(storage.read_piece(&info, 2).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scans_verified_v2_file_roots() {
        let root = std::env::temp_dir().join(format!("empirebt-v2-{}", std::process::id()));
        let storage = Storage::new(&root).unwrap();
        let data = b"data";
        let pieces_root = crate::v2::file_root(data, 16_384);
        let mut bytes = b"d4:infod4:name4:test12:piece lengthi16384e9:file treed3:foo\
d0:d6:lengthi4e11:pieces root32:"
            .to_vec();
        bytes.extend(pieces_root);
        bytes.extend_from_slice(b"eee12:meta versioni2eee");
        let info = TorrentInfo::from_bytes(&bytes).unwrap();

        storage.write("test/foo", data).unwrap();
        assert_eq!(storage.scan_complete_v2_files(&info).unwrap(), vec![true]);
        assert_eq!(
            storage
                .read_v2_piece(&info.v2_files()[0], info.piece_length(), 0)
                .unwrap(),
            data
        );

        storage.write("test/foo", b"bad").unwrap();
        assert_eq!(storage.scan_complete_v2_files(&info).unwrap(), vec![false]);
        storage
            .write_v2_piece(&info.v2_files()[0], info.piece_length(), 0, data)
            .unwrap();
        assert_eq!(storage.read("test/foo").unwrap(), data);
        let _ = std::fs::remove_dir_all(root);
    }
}
