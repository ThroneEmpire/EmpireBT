//! Resume data: which pieces were verified, so restarting does not mean
//! re-hashing every torrent.
//!
//! Each record carries a fingerprint (length and modification time) of every
//! file. Resume data is only trusted while the files still match: anything
//! that touched them since, including a crash mid-download, causes a full
//! check instead.

use std::time::UNIX_EPOCH;

use crate::{BackendError, Storage, TorrentInfo};

const MAGIC: &[u8; 8] = b"EBRESUM1";

/// A file's length and modification time, or `None` if it was missing.
type Stamp = Option<(u64, u128)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeData {
    piece_count: usize,
    bitfield: Vec<u8>,
    stamps: Vec<Stamp>,
    /// Verified pieces, counted once so displays can read it every frame.
    verified: usize,
}

impl ResumeData {
    /// Records `bitfield` together with the current state of the torrent's
    /// files. Capture after writes have stopped, or the fingerprint will not
    /// match next time and a full check runs instead.
    pub fn capture(info: &TorrentInfo, storage: &Storage, bitfield: Vec<u8>) -> Self {
        let piece_count = piece_count(info);
        let mut bitfield = bitfield;
        bitfield.resize(piece_count.div_ceil(8), 0);
        Self::new(piece_count, bitfield, stamps(info, storage))
    }

    fn new(piece_count: usize, mut bitfield: Vec<u8>, stamps: Vec<Stamp>) -> Self {
        // Bits past the last piece are never meaningful.
        if !piece_count.is_multiple_of(8)
            && let Some(last) = bitfield.last_mut()
        {
            *last &= 0xff << (8 - piece_count % 8);
        }
        let verified = bitfield.iter().map(|byte| byte.count_ones() as usize).sum();
        Self {
            piece_count,
            bitfield,
            stamps,
            verified,
        }
    }

    /// Whether the files are exactly as they were when this was captured.
    pub fn is_current(&self, info: &TorrentInfo, storage: &Storage) -> bool {
        self.piece_count == piece_count(info) && self.stamps == stamps(info, storage)
    }

    pub fn has_piece(&self, index: usize) -> bool {
        index < self.piece_count
            && self
                .bitfield
                .get(index / 8)
                .is_some_and(|byte| byte & (0x80 >> (index % 8)) != 0)
    }

    pub fn verified_pieces(&self) -> usize {
        self.verified
    }

    /// Bytes covered by verified pieces.
    pub fn verified_bytes(&self, info: &TorrentInfo) -> u64 {
        if info.format() != crate::TorrentFormat::V2 {
            // Every piece is full length except possibly the last.
            let piece_length = u64::from(info.piece_length());
            let mut bytes = self.verified as u64 * piece_length;
            if self.piece_count > 0 && self.has_piece(self.piece_count - 1) {
                let last = info
                    .piece_size(self.piece_count - 1)
                    .unwrap_or(piece_length);
                bytes -= piece_length - last;
            }
            return bytes;
        }
        // v2 pieces never span files, so each file's last piece is short.
        (0..self.piece_count)
            .filter(|index| self.has_piece(*index))
            .filter_map(|index| info.v2_piece_location(index))
            .map(|(file, local)| {
                let start = local as u64 * u64::from(info.piece_length());
                (file.length() - start).min(u64::from(info.piece_length()))
            })
            .sum()
    }

    pub fn piece_count(&self) -> usize {
        self.piece_count
    }

    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend((self.piece_count as u64).to_be_bytes());
        bytes.extend(&self.bitfield);
        bytes.extend((self.stamps.len() as u32).to_be_bytes());
        for stamp in &self.stamps {
            match stamp {
                Some((length, modified)) => {
                    bytes.push(1);
                    bytes.extend(length.to_be_bytes());
                    bytes.extend(modified.to_be_bytes());
                }
                None => bytes.push(0),
            }
        }
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BackendError> {
        let invalid = || BackendError::InvalidConfiguration("resume data is damaged".to_owned());
        let rest = bytes.strip_prefix(MAGIC).ok_or_else(invalid)?;
        let mut cursor = 0;
        let mut take = |count: usize| -> Result<&[u8], BackendError> {
            let slice = rest.get(cursor..cursor + count).ok_or_else(invalid)?;
            cursor += count;
            Ok(slice)
        };
        let piece_count = u64::from_be_bytes(take(8)?.try_into().unwrap()) as usize;
        if piece_count > 1 << 32 {
            return Err(invalid());
        }
        let bitfield = take(piece_count.div_ceil(8))?.to_vec();
        let file_count = u32::from_be_bytes(take(4)?.try_into().unwrap()) as usize;
        let mut stamps = Vec::with_capacity(file_count.min(1 << 16));
        for _ in 0..file_count {
            stamps.push(match take(1)?[0] {
                1 => Some((
                    u64::from_be_bytes(take(8)?.try_into().unwrap()),
                    u128::from_be_bytes(take(16)?.try_into().unwrap()),
                )),
                _ => None,
            });
        }
        if cursor != rest.len() {
            return Err(invalid());
        }
        Ok(Self::new(piece_count, bitfield, stamps))
    }
}

fn piece_count(info: &TorrentInfo) -> usize {
    if info.format() == crate::TorrentFormat::V2 {
        info.v2_piece_count()
    } else {
        info.piece_count()
    }
}

fn stamps(info: &TorrentInfo, storage: &Storage) -> Vec<Stamp> {
    info.files()
        .iter()
        .map(|file| {
            let metadata = std::fs::metadata(storage.path_for(file.path())?).ok()?;
            let modified = metadata
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            Some((metadata.len(), modified))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};

    #[test]
    fn trusts_resume_data_only_while_files_are_unchanged() {
        let root = std::env::temp_dir().join(format!("empirebt-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let storage = Storage::new(&root).unwrap();
        let data = b"abcdefgh";
        let hashes = data
            .chunks(4)
            .map(|piece| Sha1::digest(piece).into())
            .collect();
        let info = TorrentInfo::new(
            "resume",
            4,
            hashes,
            vec![crate::TorrentFile::new("resume", 8)],
        )
        .unwrap();
        storage.write_piece(&info, 0, b"abcd").unwrap();

        let resume = ResumeData::capture(&info, &storage, vec![0b1000_0000]);
        assert!(resume.is_current(&info, &storage));
        assert!(resume.has_piece(0) && !resume.has_piece(1));
        assert_eq!(
            (resume.verified_pieces(), resume.verified_bytes(&info)),
            (1, 4)
        );
        assert_eq!(ResumeData::decode(&resume.encode()).unwrap(), resume);

        // Any write after the capture invalidates it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        storage.write_piece(&info, 1, b"efgh").unwrap();
        assert!(!resume.is_current(&info, &storage));
        // So does deleting the data.
        let fresh = ResumeData::capture(&info, &storage, vec![0b1100_0000]);
        assert!(fresh.is_current(&info, &storage));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!fresh.is_current(&info, &storage));
        assert!(ResumeData::decode(b"EBRESUM1\x00").is_err());
    }
}
