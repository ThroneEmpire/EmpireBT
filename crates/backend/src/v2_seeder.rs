//! Seeding a v2-only torrent: a complete [`ActiveTorrent`] that a
//! [`crate::Runtime`] serves to peers with the shared peer protocol,
//! including piece-layer and leaf-layer hash requests.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::active::ActiveTorrent;
use crate::{Storage, TorrentFormat, TorrentInfo};

/// A verified v2-only torrent ready to serve.
#[derive(Debug)]
pub struct V2Seeder {
    torrent: Arc<ActiveTorrent>,
}

impl V2Seeder {
    pub fn new(
        info: TorrentInfo,
        storage: Storage,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<Self, crate::BackendError> {
        if info.format() != TorrentFormat::V2 {
            return Err(crate::BackendError::InvalidConfiguration(
                "v2 seeder requires v2-only torrent metadata".to_owned(),
            ));
        }
        if storage
            .scan_complete_v2_files(&info)?
            .into_iter()
            .any(|complete| !complete)
        {
            return Err(crate::BackendError::InvalidConfiguration(
                "cannot seed a v2 torrent with missing or corrupt files".to_owned(),
            ));
        }
        Ok(Self::from_verified(info, storage, peer_id, timeout))
    }

    /// Creates a v2 seeder for data the caller has already verified.
    pub(crate) fn from_verified(
        info: TorrentInfo,
        storage: Storage,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Self {
        Self {
            torrent: ActiveTorrent::seeding(info, storage, peer_id, timeout),
        }
    }

    pub fn info(&self) -> &TorrentInfo {
        &self.torrent.info
    }

    /// Payload bytes uploaded to peers.
    pub fn uploaded_bytes(&self) -> u64 {
        self.torrent.progress.uploaded_bytes()
    }

    pub fn bind(&self, address: SocketAddr) -> Result<TcpListener, crate::BackendError> {
        TcpListener::bind(address).map_err(|error| crate::BackendError::Io(error.to_string()))
    }

    pub(crate) fn torrent(&self) -> &Arc<ActiveTorrent> {
        &self.torrent
    }
}
