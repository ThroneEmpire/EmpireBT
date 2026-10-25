//! Seeding a v1 or hybrid torrent: a complete [`ActiveTorrent`] that a
//! [`crate::Runtime`] serves to peers with the shared peer protocol.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::active::ActiveTorrent;
use crate::{Storage, TorrentInfo};

/// A verified v1 or hybrid torrent ready to serve.
#[derive(Debug)]
pub struct Seeder {
    torrent: Arc<ActiveTorrent>,
}

impl Seeder {
    /// Creates a seeder only when every piece is present and hash-verified.
    pub fn new(
        info: TorrentInfo,
        storage: Storage,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<Self, crate::BackendError> {
        if info.pieces().is_empty() {
            return Err(crate::BackendError::UnsupportedFeature(
                "v2-only seeding requires Merkle piece verification".to_owned(),
            ));
        }
        let complete = storage.scan_complete_pieces(&info)?;
        if complete.iter().any(|piece| !piece) {
            return Err(crate::BackendError::InvalidConfiguration(
                "cannot seed a torrent with missing or corrupt pieces".to_owned(),
            ));
        }
        if info.format() == crate::TorrentFormat::Hybrid
            && storage
                .scan_complete_v2_files(&info)?
                .into_iter()
                .any(|complete| !complete)
        {
            return Err(crate::BackendError::InvalidConfiguration(
                "cannot seed a hybrid torrent with invalid v2 file roots".to_owned(),
            ));
        }
        Ok(Self::from_verified(info, storage, peer_id, timeout))
    }

    /// Creates a seeder for data the caller has already verified, skipping
    /// the full re-hash `new` performs.
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

    /// Payload bytes served since this seeder was created.
    pub fn uploaded_bytes(&self) -> u64 {
        self.torrent.progress.uploaded_bytes()
    }

    /// Binds the configured listen address for incoming peers.
    pub fn bind(&self, address: SocketAddr) -> Result<TcpListener, crate::BackendError> {
        TcpListener::bind(address).map_err(|error| crate::BackendError::Io(error.to_string()))
    }

    pub(crate) fn torrent(&self) -> &Arc<ActiveTorrent> {
        &self.torrent
    }
}
