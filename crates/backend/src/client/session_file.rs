//! Saving and loading the session: which torrents there are, their
//! states, names and resume data, so a restart picks up where it left off.
//! A session is a directory (`index`, `torrents/`, `resume/`, `names/`),
//! rewritten incrementally; the older single-file format is still read.

use super::{Client, hex, is_plain_relative};
use crate::{BackendError, ClientConfig, MagnetLink, Torrent, TorrentInfo};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SESSION_DIR_MAGIC: &[u8; 7] = b"EMPBT4\0";
pub(super) const SESSION_INDEX: &str = "index";
const SESSION_TORRENTS: &str = "torrents";
const SESSION_RESUME: &str = "resume";
/// Torrents' chosen names and renamed files.
pub(super) const SESSION_NAMES: &str = "names";
const NAMES_HEADER: &str = "EmpireBT names 1";

const SESSION_MAGIC: &[u8; 7] = b"EMPBT2\0";
const RECORD_METAINFO: u8 = 0;
const RECORD_MAGNET: u8 = 1;

impl Client {
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
        for position in 0..client.torrents.len() {
            client.adopt_legacy_names(position);
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
    use std::time::Duration;

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
}
