use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration used to create a backend client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    download_directory: PathBuf,
    listen_port: u16,
    max_peer_connections: usize,
    max_connections: usize,
    upload_limit: u64,
    download_limit: u64,
    peer_timeout: Duration,
    dht_enabled: bool,
    lsd_enabled: bool,
    interface: Option<String>,
    dht_bootstrap: Vec<String>,
    encryption: crate::Encryption,
    utp: crate::UtpMode,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            download_directory: PathBuf::from("downloads"),
            listen_port: 6881,
            max_peer_connections: 50,
            max_connections: 500,
            upload_limit: 0,
            download_limit: 0,
            peer_timeout: Duration::from_secs(10),
            dht_enabled: true,
            lsd_enabled: true,
            interface: None,
            dht_bootstrap: crate::Dht::default_bootstrap(),
            encryption: crate::Encryption::default(),
            utp: crate::UtpMode::default(),
        }
    }
}

impl ClientConfig {
    /// Creates a configuration with sensible local defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the directory where downloaded content will be stored.
    pub fn with_download_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_directory = path.into();
        self
    }

    /// Sets the TCP port used for incoming peer connections.
    pub fn with_listen_port(mut self, port: u16) -> Self {
        self.listen_port = port;
        self
    }

    /// Sets the maximum number of simultaneous outbound peer connections.
    pub fn with_max_peer_connections(mut self, connections: usize) -> Self {
        self.max_peer_connections = connections;
        self
    }

    /// Sets the maximum number of peer connections open at once across
    /// every torrent, downloading and seeding, incoming and outgoing.
    pub fn with_max_connections(mut self, connections: usize) -> Self {
        self.max_connections = connections;
        self
    }

    /// Sets the total upload rate in bytes per second; 0 means unlimited.
    pub fn with_upload_limit(mut self, bytes_per_second: u64) -> Self {
        self.upload_limit = bytes_per_second;
        self
    }

    /// Sets the total download rate in bytes per second; 0 means unlimited.
    pub fn with_download_limit(mut self, bytes_per_second: u64) -> Self {
        self.download_limit = bytes_per_second;
        self
    }

    /// Sets how long to wait on an unresponsive peer or tracker.
    pub fn with_peer_timeout(mut self, timeout: Duration) -> Self {
        self.peer_timeout = timeout;
        self
    }

    /// Enables or disables finding peers through the DHT.
    pub fn with_dht(mut self, enabled: bool) -> Self {
        self.dht_enabled = enabled;
        self
    }

    /// Enables or disables finding peers on the local network (BEP 14).
    pub fn with_lsd(mut self, enabled: bool) -> Self {
        self.lsd_enabled = enabled;
        self
    }

    pub fn lsd_enabled(&self) -> bool {
        self.lsd_enabled
    }

    /// Uses only this network interface, by name (`wg0`) or IPv4 address,
    /// for every connection; `None` (or an empty name) for any.
    pub fn with_interface(mut self, interface: Option<String>) -> Self {
        self.interface = interface.filter(|name| !name.trim().is_empty());
        self
    }

    pub fn interface(&self) -> Option<&str> {
        self.interface.as_deref()
    }

    /// The network interface every socket is bound to.
    pub fn binding(&self) -> crate::Binding {
        crate::Binding::interface(self.interface.as_deref().unwrap_or(""))
    }

    /// Sets the `host:port` nodes used to join the DHT.
    pub fn with_dht_bootstrap(mut self, nodes: Vec<String>) -> Self {
        self.dht_bootstrap = nodes;
        self
    }

    /// Sets whether peer connections use Message Stream Encryption.
    pub fn with_encryption(mut self, encryption: crate::Encryption) -> Self {
        self.encryption = encryption;
        self
    }

    pub fn encryption(&self) -> crate::Encryption {
        self.encryption
    }

    /// Sets whether peer connections may use uTP.
    pub fn with_utp(mut self, utp: crate::UtpMode) -> Self {
        self.utp = utp;
        self
    }

    pub fn utp(&self) -> crate::UtpMode {
        self.utp
    }

    pub fn peer_timeout(&self) -> Duration {
        self.peer_timeout
    }

    pub fn dht_enabled(&self) -> bool {
        self.dht_enabled
    }

    pub fn dht_bootstrap(&self) -> &[String] {
        &self.dht_bootstrap
    }

    /// Returns the configured download directory.
    pub fn download_directory(&self) -> &Path {
        &self.download_directory
    }

    /// Returns the configured incoming peer port.
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    pub fn max_peer_connections(&self) -> usize {
        self.max_peer_connections
    }

    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// Total upload rate in bytes per second; 0 means unlimited.
    pub fn upload_limit(&self) -> u64 {
        self.upload_limit
    }

    /// Total download rate in bytes per second; 0 means unlimited.
    pub fn download_limit(&self) -> u64 {
        self.download_limit
    }

    pub(crate) fn validate(&self) -> Result<(), crate::BackendError> {
        if self.download_directory.as_os_str().is_empty() {
            return Err(crate::BackendError::InvalidConfiguration(
                "download directory cannot be empty".to_owned(),
            ));
        }
        if self.max_peer_connections == 0 {
            return Err(crate::BackendError::InvalidConfiguration(
                "maximum peer connections must be greater than zero".to_owned(),
            ));
        }
        if self.peer_timeout.is_zero() {
            return Err(crate::BackendError::InvalidConfiguration(
                "peer timeout must be greater than zero".to_owned(),
            ));
        }
        if self.max_connections == 0 {
            return Err(crate::BackendError::InvalidConfiguration(
                "maximum connections must be greater than zero".to_owned(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_peer_connections() {
        let config = ClientConfig::new().with_max_peer_connections(0);
        assert!(config.validate().is_err());
    }
}
