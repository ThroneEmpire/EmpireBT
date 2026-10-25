use std::path::{Path, PathBuf};

use crate::{MagnetLink, Renames, ResumeData, TorrentInfo, TransferState};

/// A torrent managed by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Torrent {
    name: String,
    state: TransferState,
    info: Option<TorrentInfo>,
    /// The magnet link this torrent was added from, kept until its metadata
    /// has been fetched from peers.
    magnet: Option<MagnetLink>,
    /// Which pieces were verified when last checked or downloaded.
    resume: Option<ResumeData>,
    /// The folder its files are in; the client's download directory when
    /// unset.
    save_dir: Option<PathBuf>,
    /// A name chosen for it, shown instead of the torrent's own.
    display_name: Option<String>,
    /// Files whose names on disk differ from the torrent's.
    renames: Renames,
    /// The layout to give a magnet link's files once its metadata arrives.
    pending_layout: ContentLayout,
}

impl Torrent {
    /// Creates a torrent in the paused state.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: TransferState::Paused,
            info: None,
            magnet: None,
            resume: None,
            save_dir: None,
            display_name: None,
            renames: Renames::default(),
            pending_layout: ContentLayout::Original,
        }
    }

    /// Creates a paused torrent from a magnet link. Its metadata must be
    /// fetched from peers before it can download.
    pub fn from_magnet(link: MagnetLink) -> Self {
        let name = link.name().map(str::to_owned).unwrap_or_else(|| {
            link.info_hash()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        });
        Self {
            name,
            state: TransferState::Paused,
            info: None,
            magnet: Some(link),
            resume: None,
            save_dir: None,
            display_name: None,
            renames: Renames::default(),
            pending_layout: ContentLayout::Original,
        }
    }

    /// Creates a torrent backed by parsed metadata.
    pub fn from_info(info: TorrentInfo) -> Self {
        Self {
            name: info.name().to_owned(),
            state: TransferState::Paused,
            info: Some(info),
            magnet: None,
            resume: None,
            save_dir: None,
            display_name: None,
            renames: Renames::default(),
            pending_layout: ContentLayout::Original,
        }
    }

    /// Returns the display name of the torrent.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The name shown for it: one chosen for it, or the torrent's own.
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// A name chosen for it, if any.
    pub fn chosen_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub fn set_display_name(&mut self, name: Option<String>) {
        self.display_name = name;
    }

    /// Its files' names on disk where they differ from the torrent's.
    pub fn renames(&self) -> &Renames {
        &self.renames
    }

    /// The layout a magnet link's files get once its metadata arrives.
    pub fn pending_layout(&self) -> ContentLayout {
        self.pending_layout
    }

    pub(crate) fn set_pending_layout(&mut self, layout: ContentLayout) {
        self.pending_layout = layout;
    }

    /// The folder its files are in, when one was chosen for it.
    pub fn save_dir(&self) -> Option<&Path> {
        self.save_dir.as_deref()
    }

    pub fn set_save_dir(&mut self, dir: Option<PathBuf>) {
        self.save_dir = dir;
    }

    /// Returns the current lifecycle state.
    pub fn state(&self) -> TransferState {
        self.state
    }

    /// Returns the parsed metadata, when this torrent was created from it.
    pub fn info(&self) -> Option<&TorrentInfo> {
        self.info.as_ref()
    }

    /// The v1 info hash, from the metadata or the magnet link.
    pub fn info_hash(&self) -> Option<&[u8; 20]> {
        self.info
            .as_ref()
            .map(TorrentInfo::info_hash)
            .or_else(|| self.magnet.as_ref().map(MagnetLink::info_hash))
    }

    /// Saved verified-piece state, if any. Check
    /// [`ResumeData::is_current`] before trusting it.
    pub fn resume(&self) -> Option<&ResumeData> {
        self.resume.as_ref()
    }

    pub(crate) fn set_resume(&mut self, resume: Option<ResumeData>) {
        self.resume = resume;
    }

    /// The magnet link, while metadata is still missing.
    pub fn magnet(&self) -> Option<&MagnetLink> {
        self.magnet.as_ref()
    }

    /// Replaces a magnet placeholder with fetched metadata.
    pub(crate) fn set_info(&mut self, info: TorrentInfo) {
        self.name = info.name().to_owned();
        self.info = Some(info);
        self.magnet = None;
    }

    /// Marks the torrent as ready to download.
    pub fn start(&mut self) {
        self.state = TransferState::Downloading;
    }

    /// Pauses the torrent.
    pub fn pause(&mut self) {
        self.state = TransferState::Paused;
    }

    /// Marks a fully downloaded torrent as available for uploading.
    pub fn seed(&mut self) {
        self.state = TransferState::Seeding;
    }

    pub(crate) fn set_state(&mut self, state: TransferState) {
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_torrent_starts_paused() {
        let torrent = Torrent::new("example");

        assert_eq!(torrent.name(), "example");
        assert_eq!(torrent.state(), TransferState::Paused);
    }

    #[test]
    fn torrent_can_start_and_pause() {
        let mut torrent = Torrent::new("example");

        torrent.start();
        assert_eq!(torrent.state(), TransferState::Downloading);

        torrent.pause();
        assert_eq!(torrent.state(), TransferState::Paused);
    }
}

/// A torrent's file: its path in the torrent, and the one it has on disk,
/// both relative to the torrent's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFileName {
    pub original: PathBuf,
    pub path: PathBuf,
    pub length: u64,
}

/// How a torrent's files are laid out in its folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContentLayout {
    /// As the torrent says.
    #[default]
    Original,
    /// A single-file torrent goes in a folder named after it.
    Subfolder,
    /// A multi-file torrent's files go straight in the folder, without
    /// the torrent's top folder.
    NoSubfolder,
}

impl ContentLayout {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "" | "original" => Some(Self::Original),
            "subfolder" => Some(Self::Subfolder),
            "nosubfolder" | "no-subfolder" | "no_subfolder" => Some(Self::NoSubfolder),
            _ => None,
        }
    }

    /// The name [`ContentLayout::parse`] reads.
    pub fn key(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Subfolder => "subfolder",
            Self::NoSubfolder => "nosubfolder",
        }
    }
}
