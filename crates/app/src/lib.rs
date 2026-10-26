//! The EmpireBT application, shared by every interface: the console's
//! terminal dashboard and the web UI.
//!
//! An engine thread owns the client and everything around it: the saved
//! session and settings, the seeding runtime, transfers, the queue of local
//! data checks, transfer rates, and notices. Interfaces hold a [`Handle`]:
//! they send [`Command`]s and draw from [`Snapshot`]s, immutable views of
//! the whole state that the engine publishes every tick and after every
//! command. Any number of interfaces can do so at once.

mod engine;
mod geoip;
mod labels;
pub mod qbittorrent;
mod snapshot;
pub mod store;

pub use empirebt_backend::ContentLayout;
pub use engine::{AddRequest, AddSource, Command, Failure, Handle, Reply};
pub use geoip::ATTRIBUTION as GEOIP_ATTRIBUTION;

/// The program's copyright notice, shown in its interfaces (the GPL's
/// "Appropriate Legal Notices"): versions others make must keep it.
pub const COPYRIGHT: &str = "Copyright (C) 2026 ThroneEmpire";
/// Its license, and that it comes with no warranty.
pub const LICENSE_NOTICE: &str = "Free software under the GNU General Public License, version 3 or \
                                   later, with no warranty.";
/// Where its source is.
pub const SOURCE: &str = env!("CARGO_PKG_REPOSITORY");
pub use labels::{MAX_NAME, clean_name};
pub use snapshot::{
    DhtState, FileView, Level, Network, Notice, PeerView, RateMeter, SeedStatus, SettingView,
    Snapshot, Status, TorrentView,
};
pub use store::{Store, default_data_dir, home_dir};

/// A torrent's identity: its info hash (the truncated v2 hash for v2-only
/// magnet links until their metadata arrives).
pub type InfoHash = [u8; 20];
