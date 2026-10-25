//! The peer engine: one implementation of the peer protocol for every
//! connection, downloading and seeding alike.
//!
//! - [`active::ActiveTorrent`] is the state one torrent's connections
//!   share.
//! - [`block::BlockManager`] splits a piece into 16 KiB requests and
//!   assembles the blocks.
//! - [`wire::PeerWire`] is the protocol for one connection, as a state
//!   machine that asks for I/O rather than doing it.
//! - [`crate::Runtime`] runs every connection's `PeerWire` in one event
//!   loop.

pub(crate) mod active;
pub(crate) mod block;
pub(crate) mod wire;
