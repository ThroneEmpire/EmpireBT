//! Finding peers: trackers (HTTP and UDP, every tier, [`tiers`]), the
//! mainline DHT, local service discovery, and the announcer that keeps
//! every seeded torrent announced.

pub(crate) mod announcer;
pub(crate) mod dht;
pub(crate) mod dht_id;
pub(crate) mod lsd;
pub(crate) mod tiers;
pub(crate) mod tracker;
