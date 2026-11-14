//! Core BitTorrent functionality shared by the console and future interfaces.

mod client;
mod config;
mod config_file;
mod discovery;
mod engine;
mod error;
mod identity;
mod net;
mod progress;
mod protocol;
mod storage;
mod torrent;

pub use client::Client;
pub use config::ClientConfig;
pub use config_file::{BACKEND_SETTINGS, ConfigFile, Setting, SettingKind};
pub use discovery::announcer::{AnnounceStatus, SeedAnnouncer};
pub use discovery::dht::Dht;
pub use discovery::lsd::Lsd;
pub use discovery::tracker::{
    AnnounceEvent, AnnounceRequest, MAX_SCRAPE_HASHES, ScrapeStats, Tracker, TrackerClient,
    TrackerResponse, scrape_url,
};
pub use engine::limits::{Limits, SeedChoking, UploadSlotsMode};
pub use engine::piece_manager::{PieceManager, PieceState};
pub use engine::runtime::Runtime;
pub use engine::seeder::Seeder;
pub use engine::seeder::V2Seeder;
pub use engine::session::DownloadSession;
pub use engine::swarm::PeerInfo;
pub use error::BackendError;
pub use identity::{
    ANONYMOUS_USER_AGENT, CLIENT_CODE, Identity, PEER_ID_PREFIX, QBITTORRENT_PEER_ID_PREFIX,
    QBITTORRENT_USER_AGENT, USER_AGENT, client_name, disguised, generate_anonymous_peer_id,
    generate_peer_id, generate_qbittorrent_peer_id, identifies,
};
pub use net::Binding;
pub use net::http::{HttpResponse, download, http_request};
pub use net::utp::{UtpMode, UtpSocket, UtpStream};
pub use progress::{TransferHandle, TransferKind, TransferPhase, TransferProgress};
pub use protocol::connection::{ConnectOptions, Handshake, PeerConnection, PeerMessage};
pub use protocol::magnet::MagnetLink;
pub use protocol::metadata::{
    Metadata, fetch_metadata, fetch_metadata_from_peer, fetch_metadata_from_peer_with,
};
pub use protocol::metainfo::{TorrentFile, TorrentFormat, TorrentInfo, V2File, V2PieceLayer};
pub use protocol::mse::Encryption;
pub use protocol::v2::verify_hashes_response;
pub use storage::resume::ResumeData;
pub use storage::{Deleted, Renames, Storage, delete_files};
pub use torrent::TransferState;
pub use torrent::{ContentLayout, Torrent, TorrentFileName};
