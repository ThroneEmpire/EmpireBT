use std::fmt;

/// Errors returned by the backend.
#[derive(Debug, PartialEq, Eq)]
pub enum BackendError {
    /// A client configuration value is not usable.
    InvalidConfiguration(String),
    /// Torrent metadata is malformed or missing required fields.
    InvalidMetainfo(String),
    /// Reading torrent metadata from disk failed.
    Io(String),
    /// A requested torrent index or identifier does not exist.
    TorrentNotFound(String),
    /// A peer-wire message is malformed or violates protocol limits.
    InvalidPeerMessage(String),
    /// A tracker request or response could not be processed.
    Tracker(String),
    /// The metadata is understood but the requested protocol feature is not
    /// implemented yet.
    UnsupportedFeature(String),
    /// A background transfer stopped because cancellation was requested.
    Cancelled,
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(formatter, "invalid configuration: {message}")
            }
            Self::InvalidMetainfo(message) => {
                write!(formatter, "invalid torrent metadata: {message}")
            }
            Self::Io(message) => write!(formatter, "I/O error: {message}"),
            Self::TorrentNotFound(identifier) => {
                write!(formatter, "torrent not found: {identifier}")
            }
            Self::InvalidPeerMessage(message) => {
                write!(formatter, "invalid peer message: {message}")
            }
            Self::Tracker(message) => write!(formatter, "tracker error: {message}"),
            Self::UnsupportedFeature(message) => {
                write!(formatter, "unsupported feature: {message}")
            }
            Self::Cancelled => write!(formatter, "transfer was cancelled"),
        }
    }
}

impl std::error::Error for BackendError {}
