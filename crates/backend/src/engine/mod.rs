//! The peer engine: one implementation of the peer protocol for every
//! connection, downloading and seeding alike.
//!
//! - [`active::ActiveTorrent`] is the state one torrent's connections
//!   share.
//! - [`block::BlockManager`] splits a piece into 16 KiB requests and
//!   assembles the blocks.
//! - [`wire::PeerWire`] is the protocol for one connection, as a state
//!   machine that asks for I/O rather than doing it.
//! - [`runtime::Runtime`] runs every connection's `PeerWire` in one event
//!   loop ([`reactor`]), and [`session`] drives a download.
//! - [`swarm::Swarm`] tracks one torrent's peers: who is connected, trust
//!   and bans, and peer exchange.
//! - [`choker`] decides which peers we upload to, across all torrents.
//! - [`limits::Limits`] holds the connection, upload-slot and rate limits
//!   every connection shares.
//! - [`seeder`] makes a complete torrent ready to serve.

pub(crate) mod active;
pub(crate) mod block;
pub(crate) mod choker;
pub(crate) mod limits;
pub(crate) mod piece_manager;
pub(crate) mod reactor;
pub(crate) mod runtime;
pub(crate) mod seeder;
pub(crate) mod session;
pub(crate) mod swarm;
pub(crate) mod wire;
