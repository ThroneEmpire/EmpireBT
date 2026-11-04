//! Moving a torrent's files to another folder, and the folder torrents
//! downloading in `storage.incomplete_dir` move to once complete.
//!
//! A move renames each file when the folders are on one file system, and
//! otherwise copies it under a temporary name, renames the copy into
//! place, and only then deletes the original. So at every moment each file
//! is whole in one folder or the other. Moves under way are written down
//! before they start (the `moves` file in the data folder), and one cut
//! short by a crash or by quitting carries on at the next start: a file
//! already in the new folder is taken as moved.
//!
//! A move that fails (a full disk, no permission) moves back what it moved,
//! as libtorrent's `move_storage` does, and leaves the torrent where it was.
//!
//! The file:
//!
//! ```text
//! EmpireBT moves 1
//! complete<TAB><info hash in hex><TAB><folder>
//! move<TAB><info hash in hex><TAB><from><TAB><to><TAB><start: 0 or 1>
//! ```
//!
//! Paths have `\`, tabs, and line breaks escaped as `\\`, `\t`, `\n`, `\r`.

use crate::InfoHash;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

const HEADER: &str = "EmpireBT moves 1";
/// A copy checks whether it should stop after this much.
const COPY_CHUNK: u64 = 64 * 1024 * 1024;
/// Appended to a copy's name until it is whole.
const PARTIAL: &str = ".empirebt-move";

/// A move written down before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Planned {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Start downloading once moved: it was downloading when the move
    /// began, or was started meanwhile.
    pub start: bool,
}

/// What the `moves` file holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Journal {
    /// Torrents downloading in the incomplete folder: the folder each
    /// moves to once complete.
    pub complete: HashMap<InfoHash, PathBuf>,
    /// Moves under way.
    pub moving: HashMap<InfoHash, Planned>,
}

impl Journal {
    /// Follows a torrent to its new hash (a v2-only magnet link, once its
    /// metadata arrives).
    pub fn rename(&mut self, from: &InfoHash, to: InfoHash) {
        if let Some(folder) = self.complete.remove(from) {
            self.complete.insert(to, folder);
        }
        if let Some(planned) = self.moving.remove(from) {
            self.moving.insert(to, planned);
        }
    }

    /// Reads the file; a missing one is empty, and lines it does not know
    /// are skipped.
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
            return Err(format!("{} is not an EmpireBT moves file", path.display()));
        }
        let mut journal = Self::default();
        for line in lines {
            let fields: Vec<&str> = line.split('\t').collect();
            let hash = fields.get(1).and_then(|hex| unhex(hex));
            let folder = |index: usize| {
                fields
                    .get(index)
                    .map(|text| PathBuf::from(unescape(text)))
                    .filter(|path| path.is_absolute())
            };
            match (fields.first(), hash) {
                (Some(&"complete"), Some(hash)) => {
                    if let Some(folder) = folder(2) {
                        journal.complete.insert(hash, folder);
                    }
                }
                (Some(&"move"), Some(hash)) => {
                    if let (Some(from), Some(to)) = (folder(2), folder(3)) {
                        let start = fields.get(4) == Some(&"1");
                        journal.moving.insert(hash, Planned { from, to, start });
                    }
                }
                _ => {}
            }
        }
        Ok(journal)
    }

    /// Writes the file atomically, and to disk before returning: a move
    /// starts only once it is written down.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut text = format!("{HEADER}\n");
        let mut complete: Vec<_> = self.complete.iter().collect();
        complete.sort_by_key(|(hash, _)| **hash);
        for (hash, folder) in complete {
            text.push_str(&format!("complete\t{}\t{}\n", hex(hash), escape(folder)));
        }
        let mut moving: Vec<_> = self.moving.iter().collect();
        moving.sort_by_key(|(hash, _)| **hash);
        for (hash, planned) in moving {
            text.push_str(&format!(
                "move\t{}\t{}\t{}\t{}\n",
                hex(hash),
                escape(&planned.from),
                escape(&planned.to),
                u8::from(planned.start)
            ));
        }
        let temporary = path.with_extension("tmp");
        std::fs::File::create(&temporary)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|error| format!("cannot save {}: {error}", path.display()))
    }
}

/// How far a move is.
#[derive(Debug, Default)]
pub(crate) struct MoveProgress {
    pub moved: AtomicU64,
    pub total: AtomicU64,
}

/// What a finished move did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Moved {
    /// Files that were moved.
    pub files: usize,
    /// Some files were already in the new folder (put there by hand) and
    /// were used as they are: the torrent needs a check.
    pub adopted: bool,
}

/// Why a move did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveError {
    /// Stopped when asked (EmpireBT is quitting); it carries on next time.
    Stopped,
    /// It failed, and what it moved was moved back.
    Failed(String),
}

/// A move running on a thread of its own.
pub(crate) struct Mover {
    pub progress: Arc<MoveProgress>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<Moved, MoveError>>,
}

impl Mover {
    /// Moves `files` (paths relative to the torrent's folder) from `from`
    /// to `to`. A first try refuses before moving anything if a file is in
    /// both folders; `resuming` an interrupted move takes a file already
    /// in `to` as moved.
    pub fn start(from: PathBuf, to: PathBuf, files: Vec<PathBuf>, resuming: bool) -> Self {
        let progress = Arc::new(MoveProgress::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (progress, stop) = (Arc::clone(&progress), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("move".to_owned())
                .spawn(move || relocate(&from, &to, &files, resuming, &progress, &stop))
                .expect("the system can start a thread")
        };
        Self {
            progress,
            stop,
            thread,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Asks it to stop after the piece of a file it is copying.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn join(self) -> Result<Moved, MoveError> {
        self.thread.join().unwrap_or_else(|_| {
            Err(MoveError::Failed(
                "the move stopped unexpectedly".to_owned(),
            ))
        })
    }

    /// The fraction moved, by size.
    pub fn fraction(&self) -> f64 {
        let total = self.progress.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.progress.moved.load(Ordering::Relaxed) as f64 / total as f64
    }
}

/// Whether any of `files` is in `folder`.
pub(crate) fn any_exist(folder: &Path, files: &[PathBuf]) -> bool {
    files.iter().any(|file| folder.join(file).exists())
}

fn relocate(
    from: &Path,
    to: &Path,
    files: &[PathBuf],
    resuming: bool,
    progress: &MoveProgress,
    stop: &AtomicBool,
) -> Result<Moved, MoveError> {
    let failed = |error: String| MoveError::Failed(error);
    // Plan first, so a conflict stops it before anything moves.
    let mut adopted = false;
    let mut total = 0;
    // Files that a run before this one already moved: moved back too if
    // this one fails.
    let mut moved_before = Vec::new();
    for file in files {
        let (source, target) = (from.join(file), to.join(file));
        let source_meta = std::fs::symlink_metadata(&source).ok();
        let target_exists = std::fs::symlink_metadata(&target).is_ok();
        match (source_meta, target_exists) {
            (Some(_), true) if !resuming => {
                return Err(failed(format!("{} already exists", target.display())));
            }
            (Some(meta), false) => total += meta.len(),
            (None, true) if resuming => moved_before.push(file.clone()),
            (None, true) => adopted = true,
            _ => {}
        }
    }
    progress.total.store(total, Ordering::Relaxed);

    let mut moved: Vec<&PathBuf> = Vec::new();
    for file in files {
        let (source, target) = (from.join(file), to.join(file));
        let Ok(meta) = std::fs::symlink_metadata(&source) else {
            continue;
        };
        if std::fs::symlink_metadata(&target).is_ok() {
            // Only while resuming (checked above): the copy was whole and
            // renamed into place, and the original was not deleted yet.
            remove_file(&source).map_err(failed)?;
            continue;
        }
        match move_file(&source, &target, &meta, progress, stop) {
            Ok(()) => moved.push(file),
            Err(error) => {
                let mut back: Vec<&PathBuf> = moved_before.iter().collect();
                back.extend(moved);
                let problem = match &error {
                    MoveError::Stopped => return Err(MoveError::Stopped),
                    MoveError::Failed(problem) => problem.clone(),
                };
                let undone = move_back(from, to, &back);
                return Err(failed(match undone {
                    Ok(()) => problem,
                    Err(undo) => format!("{problem}; moving back failed too: {undo}"),
                }));
            }
        }
    }
    remove_empty_folders(from, files);
    Ok(Moved {
        files: moved.len() + moved_before.len(),
        adopted,
    })
}

/// Moves files that reached `to` back to `from`, after a failure.
fn move_back(from: &Path, to: &Path, files: &[&PathBuf]) -> Result<(), String> {
    let never = AtomicBool::new(false);
    let progress = MoveProgress::default();
    let mut first = Ok(());
    for file in files.iter().rev() {
        let (back, moved) = (from.join(file), to.join(file));
        let Ok(meta) = std::fs::symlink_metadata(&moved) else {
            continue;
        };
        if std::fs::symlink_metadata(&back).is_ok() {
            continue;
        }
        if let Err(error) = move_file(&moved, &back, &meta, &progress, &never)
            && first.is_ok()
        {
            first = Err(match error {
                MoveError::Failed(text) => text,
                MoveError::Stopped => "stopped".to_owned(),
            });
        }
    }
    let files: Vec<PathBuf> = files.iter().map(|file| (*file).clone()).collect();
    remove_empty_folders(to, &files);
    first
}

/// Moves one file: a rename, or else a copy under a temporary name that
/// is renamed into place once whole, then the original deleted.
fn move_file(
    source: &Path,
    target: &Path,
    meta: &std::fs::Metadata,
    progress: &MoveProgress,
    stop: &AtomicBool,
) -> Result<(), MoveError> {
    let failed = |path: &Path, error: std::io::Error| {
        MoveError::Failed(format!("{}: {error}", path.display()))
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| failed(parent, error))?;
    }
    if std::fs::rename(source, target).is_ok() {
        progress.moved.fetch_add(meta.len(), Ordering::Relaxed);
        return Ok(());
    }
    // Another file system (or a rename refused for another reason, which
    // the copy then reports).
    let mut name = target.as_os_str().to_owned();
    name.push(PARTIAL);
    let partial = PathBuf::from(name);
    let copied = copy(source, &partial, meta, progress, stop);
    let copied = copied
        .and_then(|()| std::fs::rename(&partial, target).map_err(|error| failed(target, error)));
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    remove_file(source).map_err(MoveError::Failed)
}

fn copy(
    source: &Path,
    target: &Path,
    meta: &std::fs::Metadata,
    progress: &MoveProgress,
    stop: &AtomicBool,
) -> Result<(), MoveError> {
    let failed = |path: &Path, error: std::io::Error| {
        MoveError::Failed(format!("{}: {error}", path.display()))
    };
    let mut input = std::fs::File::open(source).map_err(|error| failed(source, error))?;
    let mut output = std::fs::File::create(target).map_err(|error| failed(target, error))?;
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(MoveError::Stopped);
        }
        // `io::copy` of a file into a file lets the kernel copy
        // (`copy_file_range`), a piece at a time.
        let copied = std::io::copy(&mut (&mut input).take(COPY_CHUNK), &mut output)
            .map_err(|error| failed(target, error))?;
        progress.moved.fetch_add(copied, Ordering::Relaxed);
        if copied < COPY_CHUNK {
            break;
        }
    }
    output.sync_all().map_err(|error| failed(target, error))?;
    // The same permissions and time: resume data recognizes the files by
    // size and modification time.
    let _ = output.set_permissions(meta.permissions());
    if let Ok(modified) = meta.modified() {
        let _ = output.set_modified(modified);
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Removes folders under `root` that held `files` and are empty now.
fn remove_empty_folders(root: &Path, files: &[PathBuf]) {
    for file in files {
        let mut folder = root.join(file);
        while folder.pop() && folder.starts_with(root) && folder != root {
            if std::fs::remove_dir(&folder).is_err() {
                break;
            }
        }
    }
}

fn escape(path: &Path) -> String {
    let mut text = String::new();
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => text.push_str("\\\\"),
            '\t' => text.push_str("\\t"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            other => text.push(other),
        }
    }
    text
}

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
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

    fn folder(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("empirebt-moves-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn run(from: &Path, to: &Path, files: &[&str], resuming: bool) -> Result<Moved, MoveError> {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        Mover::start(from.to_owned(), to.to_owned(), files, resuming).join()
    }

    #[test]
    fn journal_round_trip_with_awkward_paths() {
        let root = folder("journal");
        let path = root.join("moves");
        let mut journal = Journal::default();
        journal
            .complete
            .insert([1; 20], PathBuf::from("/media/done\tfolder"));
        journal.moving.insert(
            [2; 20],
            Planned {
                from: PathBuf::from("/a\\b\nc"),
                to: PathBuf::from("/x y"),
                start: true,
            },
        );
        journal.save(&path).unwrap();
        assert_eq!(Journal::load(&path).unwrap(), journal);
        assert_eq!(
            Journal::load(&root.join("missing")).unwrap(),
            Journal::default()
        );
        std::fs::write(&path, "something else\n").unwrap();
        assert!(Journal::load(&path).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn moves_files_and_tidies_the_old_folder() {
        let root = folder("move");
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(from.join("show/season")).unwrap();
        std::fs::write(from.join("show/season/one.mkv"), b"one").unwrap();
        std::fs::write(from.join("show/two.mkv"), b"two").unwrap();
        let moved = run(
            &from,
            &to,
            &["show/season/one.mkv", "show/two.mkv", "show/missing"],
            false,
        )
        .unwrap();
        assert_eq!(
            moved,
            Moved {
                files: 2,
                adopted: false
            }
        );
        assert_eq!(
            std::fs::read(to.join("show/season/one.mkv")).unwrap(),
            b"one"
        );
        assert_eq!(std::fs::read(to.join("show/two.mkv")).unwrap(), b"two");
        assert!(!from.join("show").exists(), "emptied folders go");
        assert!(from.exists(), "the save folder itself stays");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn copies_with_the_same_time_when_renaming_cannot() {
        let root = folder("copy");
        let source = root.join("a.bin");
        let data = vec![7u8; 100_000];
        std::fs::write(&source, &data).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let target = root.join("b.bin");
        let progress = MoveProgress::default();
        copy(&source, &target, &meta, &progress, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), data);
        assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), old);
        assert_eq!(progress.moved.load(Ordering::Relaxed), 100_000);
        // Asked to stop, it stops before copying.
        let stopped = copy(
            &source,
            &root.join("c.bin"),
            &meta,
            &progress,
            &AtomicBool::new(true),
        );
        assert_eq!(stopped, Err(MoveError::Stopped));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_to_overwrite_and_adopts_files_already_there() {
        let root = folder("conflict");
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("a"), b"mine").unwrap();
        std::fs::write(to.join("a"), b"someone else's").unwrap();
        let refused = run(&from, &to, &["a"], false);
        assert!(matches!(refused, Err(MoveError::Failed(text)) if text.contains("already exists")));
        assert_eq!(
            std::fs::read(from.join("a")).unwrap(),
            b"mine",
            "nothing moved"
        );
        assert_eq!(std::fs::read(to.join("a")).unwrap(), b"someone else's");

        // Moved by hand already: used as is.
        std::fs::remove_file(from.join("a")).unwrap();
        std::fs::write(from.join("b"), b"b").unwrap();
        let moved = run(&from, &to, &["a", "b"], false).unwrap();
        assert_eq!(
            moved,
            Moved {
                files: 1,
                adopted: true
            }
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn carries_on_an_interrupted_move() {
        let root = folder("resume");
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        // One moved, one copied but not yet deleted, one with a partial
        // copy, one not started.
        std::fs::write(to.join("moved"), b"1").unwrap();
        std::fs::write(from.join("copied"), b"2").unwrap();
        std::fs::write(to.join("copied"), b"2").unwrap();
        std::fs::write(from.join("partial"), b"3").unwrap();
        std::fs::write(to.join(format!("partial{PARTIAL}")), b"").unwrap();
        std::fs::write(from.join("waiting"), b"4").unwrap();
        let moved = run(&from, &to, &["moved", "copied", "partial", "waiting"], true).unwrap();
        assert!(!moved.adopted);
        for (name, data) in [
            ("moved", b"1"),
            ("copied", b"2"),
            ("partial", b"3"),
            ("waiting", b"4"),
        ] {
            assert_eq!(std::fs::read(to.join(name)).unwrap(), data, "{name}");
            assert!(!from.join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_move_moves_back_what_it_moved() {
        let root = folder("fail");
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(from.join("blocked")).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("first"), b"1").unwrap();
        std::fs::write(from.join("blocked/second"), b"2").unwrap();
        // A file where the folder must go: no one can move into it.
        std::fs::write(to.join("blocked"), b"").unwrap();
        let result = run(&from, &to, &["first", "blocked/second"], false);
        assert!(matches!(result, Err(MoveError::Failed(_))), "{result:?}");
        assert_eq!(
            std::fs::read(from.join("first")).unwrap(),
            b"1",
            "moved back"
        );
        assert_eq!(std::fs::read(from.join("blocked/second")).unwrap(), b"2");
        assert!(!to.join("first").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Across file systems (here the temporary folder and `/dev/shm`, a
    /// memory file system, where there is one): copied, the copy whole
    /// under its own name, with the original's time, then the original
    /// deleted.
    #[cfg(target_os = "linux")]
    #[test]
    fn moves_across_file_systems() {
        use std::os::unix::fs::MetadataExt;
        let root = folder("across");
        let shm = Path::new("/dev/shm");
        let Ok(memory) = std::fs::metadata(shm) else {
            return;
        };
        if memory.dev() == std::fs::metadata(&root).unwrap().dev() {
            return;
        }
        let to = shm.join(format!("empirebt-moves-across-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&to);
        let data: Vec<u8> = (0..(COPY_CHUNK as usize + 12_345))
            .map(|byte| byte as u8)
            .collect();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/big.bin"), &data).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        std::fs::File::options()
            .write(true)
            .open(root.join("nested/big.bin"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mover = Mover::start(
            root.clone(),
            to.clone(),
            vec![PathBuf::from("nested/big.bin")],
            false,
        );
        let progress = Arc::clone(&mover.progress);
        assert_eq!(
            mover.join(),
            Ok(Moved {
                files: 1,
                adopted: false
            })
        );
        assert_eq!(progress.moved.load(Ordering::Relaxed), data.len() as u64);
        assert_eq!(std::fs::read(to.join("nested/big.bin")).unwrap(), data);
        assert_eq!(
            std::fs::metadata(to.join("nested/big.bin"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
        assert!(!to.join(format!("nested/big.bin{PARTIAL}")).exists());
        assert!(!root.join("nested").exists());
        let _ = std::fs::remove_dir_all(&to);
        let _ = std::fs::remove_dir_all(root);
    }
}
