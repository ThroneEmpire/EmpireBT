//! The BitTorrent protocol's formats and messages: bencoding, torrent
//! files (v1, v2, hybrid) and magnet links, the peer wire handshake and
//! messages, encryption (MSE), and the extensions we speak: the Fast
//! Extension, extension handshake and metadata exchange, peer exchange,
//! hole punching, and canonical peer priority (BEP 40).

pub(crate) mod bencode;
pub(crate) mod connection;
pub(crate) mod fast;
pub(crate) mod holepunch;
pub(crate) mod magnet;
pub(crate) mod metadata;
pub(crate) mod metainfo;
pub(crate) mod mse;
pub(crate) mod pex;
pub(crate) mod priority;
pub(crate) mod v2;
