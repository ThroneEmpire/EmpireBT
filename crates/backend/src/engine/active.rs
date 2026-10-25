//! A torrent being shared: downloading, seeding, or both at once.
//!
//! [`ActiveTorrent`] holds what every connection for one torrent shares:
//! - the piece layout, and reading and writing data
//! - which pieces are verified and which are claimed by a connection
//! - the swarm state: choking, peer exchange, trust and bans
//! - the transfer counters
//!
//! A seeding torrent is simply one whose pieces are all verified. The same
//! connections and the same protocol code serve both cases.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sha1::Digest;

use super::block::{BlockManager, BlockRequest};
use crate::{Handshake, PeerMessage, PieceManager, Storage, TorrentInfo};

/// Where each piece lives and how it is verified. v1 (and hybrid) pieces
/// may span files and are checked with SHA-1; v2 pieces never cross a file
/// boundary and are checked against BEP 52 Merkle hashes. Pieces are
/// numbered the same way on the wire for both.
#[derive(Debug, Clone)]
pub(crate) enum Layout {
    V1,
    V2(Arc<[V2Piece]>),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct V2Piece {
    /// Index into the torrent's v2 files.
    file: usize,
    /// Piece index within that file.
    local: usize,
    size: u64,
    /// The piece-layer hash, or the file root for a single-piece file.
    expected: [u8; 32],
    single: bool,
}

impl Layout {
    pub(crate) fn for_info(info: &TorrentInfo) -> Self {
        if info.format() != crate::TorrentFormat::V2 {
            return Self::V1;
        }
        let piece_length = u64::from(info.piece_length());
        let mut pieces = Vec::with_capacity(info.v2_piece_count());
        for (file_index, file) in info.v2_files().iter().enumerate() {
            let count = file.piece_count(info.piece_length());
            let layer = info
                .v2_piece_layers()
                .iter()
                .find(|layer| layer.root() == file.pieces_root())
                .map(|layer| layer.pieces());
            for local in 0..count {
                let expected = match (count, layer) {
                    (1, _) => *file.pieces_root(),
                    (_, Some(layer)) => layer.get(local).copied().unwrap_or([0; 32]),
                    // Metadata validation requires layers for multi-piece
                    // files, so this never matches a real piece.
                    (_, None) => [0; 32],
                };
                pieces.push(V2Piece {
                    file: file_index,
                    local,
                    size: (file.length() - local as u64 * piece_length).min(piece_length),
                    expected,
                    single: count == 1,
                });
            }
        }
        Self::V2(pieces.into())
    }

    pub(crate) fn piece_count(&self, info: &TorrentInfo) -> usize {
        match self {
            Self::V1 => info.piece_count(),
            Self::V2(pieces) => pieces.len(),
        }
    }

    pub(crate) fn piece_size(&self, info: &TorrentInfo, index: usize) -> Option<u64> {
        match self {
            Self::V1 => info.piece_size(index),
            Self::V2(pieces) => pieces.get(index).map(|piece| piece.size),
        }
    }

    pub(crate) fn verify(&self, info: &TorrentInfo, index: usize, data: &[u8]) -> bool {
        if self.piece_size(info, index) != Some(data.len() as u64) {
            return false;
        }
        match self {
            Self::V1 => {
                let actual: [u8; 20] = sha1::Sha1::digest(data).into();
                info.pieces().get(index) == Some(&actual)
            }
            Self::V2(pieces) => {
                pieces[index].expected
                    == crate::v2::piece_hash_for(data, info.piece_length(), pieces[index].single)
            }
        }
    }

    pub(crate) fn read_piece(
        &self,
        info: &TorrentInfo,
        storage: &Storage,
        index: usize,
    ) -> Result<Vec<u8>, crate::BackendError> {
        match self {
            Self::V1 => storage.read_piece(info, index),
            Self::V2(pieces) => {
                let piece = pieces[index];
                storage.read_v2_piece(
                    &info.v2_files()[piece.file],
                    info.piece_length(),
                    piece.local,
                )
            }
        }
    }
}

/// How checking a completed piece turned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checked {
    /// Verified and written; new to us.
    Stored,
    /// Verified, but another connection completed it first (end-game).
    AlreadyHad,
    /// Failed its hash check.
    Corrupt,
}

/// A piece claimed by one connection. Dropping it without settling it
/// releases the piece for others, so a connection that fails mid-piece
/// never leaves it claimed forever.
pub(crate) struct Claim {
    pieces: Arc<Mutex<PieceManager>>,
    index: usize,
    settled: bool,
}

impl Claim {
    pub(crate) fn settle(&mut self) {
        self.settled = true;
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.settled
            && let Ok(mut pieces) = self.pieces.lock()
        {
            pieces.reset(self.index);
        }
    }
}

/// A piece being downloaded from one peer.
pub(crate) struct Download {
    pub(crate) index: usize,
    pub(crate) blocks: BlockManager,
    pub(crate) unsent: std::collections::VecDeque<BlockRequest>,
    pub(crate) outstanding: Vec<BlockRequest>,
    /// Released if the piece is abandoned; `None` for end-game duplicates.
    pub(crate) claim: Option<Claim>,
}

/// Leaf (16 KiB block) hash layers by file root.
type LeafLayer = Arc<Vec<[u8; 32]>>;

pub(crate) struct ActiveTorrent {
    pub(crate) info: TorrentInfo,
    pub(crate) storage: Storage,
    pub(crate) layout: Layout,
    pub(crate) peer_id: [u8; 20],
    /// How long a peer may go without sending requested blocks.
    pub(crate) timeout: Duration,
    pub(crate) progress: Arc<crate::TransferProgress>,
    pub(crate) swarm: Arc<crate::swarm::Swarm>,
    pieces: Arc<Mutex<PieceManager>>,
    /// Open peer connections, counted by their protocol state.
    connections: std::sync::atomic::AtomicUsize,
    /// Why the last connection that failed did so.
    last_error: Mutex<Option<crate::BackendError>>,
    /// Computed on the first leaf-layer hash request for a file, so
    /// repeated requests cannot force it to be re-hashed each time.
    leaf_layers: Mutex<HashMap<[u8; 32], LeafLayer>>,
}

impl std::fmt::Debug for ActiveTorrent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveTorrent")
            .field("name", &self.info.name())
            .finish()
    }
}

impl ActiveTorrent {
    pub(crate) fn new(
        info: TorrentInfo,
        storage: Storage,
        peer_id: [u8; 20],
        timeout: Duration,
        progress: Arc<crate::TransferProgress>,
        swarm: Arc<crate::swarm::Swarm>,
        pieces: PieceManager,
    ) -> Arc<Self> {
        Arc::new(Self {
            layout: Layout::for_info(&info),
            info,
            storage,
            peer_id,
            timeout,
            progress,
            swarm,
            pieces: Arc::new(Mutex::new(pieces)),
            connections: std::sync::atomic::AtomicUsize::new(0),
            last_error: Mutex::new(None),
            leaf_layers: Mutex::new(HashMap::new()),
        })
    }

    /// A torrent whose data is all verified, ready to seed.
    pub(crate) fn seeding(
        info: TorrentInfo,
        storage: Storage,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Arc<Self> {
        let count = Layout::for_info(&info).piece_count(&info);
        let mut pieces = PieceManager::new(count);
        for index in 0..count {
            pieces.mark_complete(index);
        }
        let progress = Arc::new(crate::TransferProgress::new());
        progress.set_totals(count, info.total_length());
        let swarm = Arc::new(crate::swarm::Swarm::new(crate::swarm::DEFAULT_UPLOAD_SLOTS));
        Self::new(info, storage, peer_id, timeout, progress, swarm, pieces)
    }

    pub(crate) fn pieces(&self) -> Result<MutexGuard<'_, PieceManager>, crate::BackendError> {
        self.pieces
            .lock()
            .map_err(|_| invalid_peer("piece scheduler lock was poisoned"))
    }

    pub(crate) fn piece_count(&self) -> usize {
        self.layout.piece_count(&self.info)
    }

    pub(crate) fn piece_size(&self, index: usize) -> Option<u64> {
        self.layout.piece_size(&self.info, index)
    }

    /// Notes why a connection failed. Ends of connections the protocol
    /// finished on purpose are not errors.
    pub(crate) fn note_error(&self, error: crate::BackendError) {
        if error.to_string().contains("connection finished") {
            return;
        }
        if let Ok(mut last) = self.last_error.lock() {
            *last = Some(error);
        }
    }

    /// Why the last failed connection failed, once.
    pub(crate) fn take_error(&self) -> Option<crate::BackendError> {
        self.last_error.lock().ok().and_then(|mut last| last.take())
    }

    /// Open peer connections for this torrent.
    pub(crate) fn connection_count(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn connection_opened(&self) {
        self.connections
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.progress.add_peer();
    }

    pub(crate) fn connection_closed(&self) {
        self.connections
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.progress.remove_peer();
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.pieces().is_ok_and(|pieces| pieces.is_complete())
    }

    pub(crate) fn has_piece(&self, index: usize) -> bool {
        self.pieces()
            .is_ok_and(|pieces| pieces.state(index) == Some(crate::PieceState::Complete))
    }

    /// The handshake for this torrent's swarm: the v1 hash, or for v2-only
    /// torrents the truncated v2 hash.
    pub(crate) fn handshake(&self) -> Handshake {
        self.handshake_for(self.info.peer_info_hash())
    }

    /// Our answer to a peer that connected using `hash`, which it expects
    /// echoed. A hybrid torrent's truncated v2 hash makes a v2 connection.
    pub(crate) fn handshake_for(&self, hash: [u8; 20]) -> Handshake {
        let handshake = Handshake::new(hash, self.peer_id)
            .with_fast()
            .with_extensions();
        if matches!(self.layout, Layout::V2(_)) || hash != *self.info.info_hash() {
            handshake.with_v2_support()
        } else {
            handshake
        }
    }

    /// Chooses the next piece to download from a peer with `bits`: the
    /// first of `preferred` it has that is unclaimed, then (unless
    /// `only_preferred`) the rarest unclaimed one, or in end-game one still
    /// in flight elsewhere.
    pub(crate) fn pick_piece(
        &self,
        bits: &[u8],
        tie_breaker: usize,
        preferred: &[u32],
        only_preferred: bool,
    ) -> Result<Option<Download>, crate::BackendError> {
        let (index, claim) = {
            let mut pieces = self.pieces()?;
            let chosen = preferred
                .iter()
                .map(|index| *index as usize)
                .find(|index| has_bit(bits, *index) && pieces.claim(*index));
            let rarest = match chosen {
                Some(index) => Some(index),
                None if only_preferred => return Ok(None),
                None => pieces.claim_rarest(bits, tie_breaker),
            };
            match rarest {
                Some(index) => (
                    index,
                    Some(Claim {
                        pieces: Arc::clone(&self.pieces),
                        index,
                        settled: false,
                    }),
                ),
                None => match pieces.end_game_candidate(bits, tie_breaker) {
                    // End-game duplicates are not claimed: whichever peer
                    // delivers first completes the piece.
                    Some(index) => (index, None),
                    None => return Ok(None),
                },
            }
        };
        let length = self.piece_size(index).expect("claimed pieces exist") as u32;
        let blocks = BlockManager::start(index as u32, length)?;
        Ok(Some(Download {
            index,
            unsent: blocks.missing_requests().into(),
            blocks,
            outstanding: Vec::new(),
            claim,
        }))
    }

    /// Whether a peer's block request is for verified data we can send.
    pub(crate) fn valid_upload(&self, piece_index: u32, begin: u32, length: u32) -> bool {
        let index = piece_index as usize;
        let Some(size) = self.piece_size(index) else {
            return false;
        };
        length > 0
            && length <= MAX_UPLOAD_BLOCK
            && u64::from(begin) + u64::from(length) <= size
            && self.has_piece(index)
    }

    /// Reads one block of a verified piece for a peer.
    pub(crate) fn read_block(
        &self,
        index: usize,
        begin: u32,
        length: u32,
    ) -> Result<Vec<u8>, crate::BackendError> {
        let block = match &self.layout {
            Layout::V1 => {
                let offset = index as u64 * u64::from(self.info.piece_length()) + u64::from(begin);
                self.storage
                    .read_range(&self.info, offset, u64::from(length))?
            }
            Layout::V2(pieces) => {
                let piece = pieces[index];
                self.storage.read_v2_block(
                    &self.info.v2_files()[piece.file],
                    self.info.piece_length(),
                    piece.local,
                    begin,
                    length,
                )?
            }
        };
        if block.len() != length as usize {
            return Err(crate::BackendError::Io(
                "verified piece data is missing from disk".to_owned(),
            ));
        }
        self.progress.add_uploaded(u64::from(length));
        Ok(block)
    }

    /// Verifies a downloaded piece and, if it is good and new, writes it
    /// and marks it verified. A good piece settles the download's claim.
    pub(crate) fn check_piece(
        &self,
        download: &mut Download,
        data: &[u8],
    ) -> Result<Checked, crate::BackendError> {
        let index = download.index;
        if !self.layout.verify(&self.info, index, data) {
            return Ok(Checked::Corrupt);
        }
        if let Some(claim) = &mut download.claim {
            claim.settle();
        }
        if self.has_piece(index) {
            return Ok(Checked::AlreadyHad);
        }
        // Write before marking complete, so complete always means on disk.
        self.write_piece(index, data)?;
        let mut pieces = self.pieces()?;
        if pieces.state(index) == Some(crate::PieceState::Complete) {
            return Ok(Checked::AlreadyHad);
        }
        pieces.mark_complete(index);
        self.progress.add_verified(index, data.len() as u64);
        Ok(Checked::Stored)
    }

    fn write_piece(&self, index: usize, data: &[u8]) -> Result<(), crate::BackendError> {
        match &self.layout {
            Layout::V1 => self.storage.write_piece(&self.info, index, data),
            Layout::V2(pieces) => {
                let piece = pieces[index];
                self.storage.write_v2_piece(
                    &self.info.v2_files()[piece.file],
                    self.info.piece_length(),
                    piece.local,
                    data,
                )
            }
        }
    }

    /// Answers a BEP 52 hash request: piece-layer hashes from the
    /// metadata, or leaf-layer hashes for a file we have in full.
    /// Anything else is rejected.
    pub(crate) fn answer_hashes(
        &self,
        request: PeerMessage,
    ) -> Result<PeerMessage, crate::BackendError> {
        let PeerMessage::HashRequest {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
        } = request
        else {
            return Err(invalid_peer("not a hash request"));
        };
        let reject = PeerMessage::HashReject {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
        };
        if self.info.v2_info_hash().is_none() {
            return Ok(reject);
        }
        crate::v2::answer_hash_request(
            &self.info,
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
            |file| self.leaf_layer(file),
        )
    }

    /// A file's leaf hashes, computed once, if the file is complete.
    fn leaf_layer(&self, file: &crate::V2File) -> Result<Option<LeafLayer>, crate::BackendError> {
        if let Some(layer) = self
            .leaf_layers
            .lock()
            .ok()
            .and_then(|layers| layers.get(file.pieces_root()).cloned())
        {
            return Ok(Some(layer));
        }
        if !self.is_complete() {
            return Ok(None);
        }
        let mut leaves = Vec::new();
        for piece_index in 0..file.piece_count(self.info.piece_length()) {
            let piece = self
                .storage
                .read_v2_piece(file, self.info.piece_length(), piece_index)?;
            leaves.extend(crate::v2::leaf_hashes(&piece));
        }
        let layer = Arc::new(leaves);
        if let Ok(mut layers) = self.leaf_layers.lock() {
            layers.insert(*file.pieces_root(), Arc::clone(&layer));
        }
        Ok(Some(layer))
    }
}

/// The largest block a peer may request; 16 KiB is the standard size.
const MAX_UPLOAD_BLOCK: u32 = 16 * 1024;

pub(crate) fn has_bit(bits: &[u8], index: usize) -> bool {
    bits.get(index / 8)
        .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
}

pub(crate) fn invalid_peer(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::InvalidPeerMessage(message.into())
}
