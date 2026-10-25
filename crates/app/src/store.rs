//! Locations of the session and settings files, and crash-safe saving.

use empirebt_backend::{Client, ClientConfig, ConfigFile, Setting, SettingKind};
use std::path::PathBuf;

/// The session: an index plus one metadata file per torrent.
const SESSION_DIR: &str = "session";
/// The single-file session written by earlier versions.
const LEGACY_SESSION_FILE: &str = "session.dat";
/// Remembered download folder from versions before `config.toml`.
const LEGACY_DOWNLOAD_DIR_FILE: &str = "download-dir";
const DHT_FILE: &str = "dht.dat";
const CONFIG_FILE: &str = "config.toml";
/// Held locked while a process uses the data directory.
const LOCK_FILE: &str = "lock";
/// Categories and tags.
const LABELS_FILE: &str = "labels";

/// Settings of the application, in the `[console]` section (named for the
/// first interface; the web UI shares them).
pub const CONSOLE_SETTINGS: &[Setting] = &[Setting {
    key: "console.start_on_add",
    kind: SettingKind::Bool,
    description: "Start downloading as soon as a torrent is added.",
    needs_restart: false,
}];
const CONSOLE_DEFAULTS: &[(&str, &str)] = &[("console.start_on_add", "true")];

/// Settings about peers, in the `[peers]` section.
pub const PEER_SETTINGS: &[Setting] = &[Setting {
    key: "peers.countries",
    kind: SettingKind::Bool,
    description: "Show peers' countries. Downloads DB-IP's free country database (about \
                  5 MB) once a month, through the chosen network interface.",
    needs_restart: true,
}];
const PEER_DEFAULTS: &[(&str, &str)] = &[("peers.countries", "true")];

/// Settings of the web UI, in the `[web]` section. None can be changed
/// from the web UI itself.
pub const WEB_SETTINGS: &[Setting] = &[
    Setting {
        key: "web.address",
        kind: SettingKind::Address,
        description: "Address the web UI listens on. 127.0.0.1 is this computer only; any \
                      other address requires HTTPS (a certificate is made if none is set).",
        needs_restart: true,
    },
    Setting {
        key: "web.port",
        kind: SettingKind::Port,
        description: "Port of the web UI.",
        needs_restart: true,
    },
    Setting {
        key: "web.https",
        kind: SettingKind::Choice(&["auto", "off"]),
        description: "auto serves HTTPS on any address but 127.0.0.1. off serves plain HTTP \
                      everywhere: only behind a reverse proxy that adds HTTPS (Caddy, Traefik, \
                      nginx), never exposed directly.",
        needs_restart: true,
    },
    Setting {
        key: "web.tls_cert",
        kind: SettingKind::Text,
        description: "HTTPS certificate (PEM file) for the web UI; empty for plain HTTP on \
                      127.0.0.1, or a generated certificate elsewhere.",
        needs_restart: true,
    },
    Setting {
        key: "web.tls_key",
        kind: SettingKind::Text,
        description: "Private key (PEM file) for web.tls_cert.",
        needs_restart: true,
    },
    Setting {
        key: "web.allowed_hosts",
        kind: SettingKind::Text,
        description: "Extra host names the web UI answers to, comma-separated (for example \
                      nas.local). Requests for other names are refused.",
        needs_restart: true,
    },
];
const WEB_DEFAULTS: &[(&str, &str)] = &[
    ("web.address", "127.0.0.1"),
    ("web.port", "8080"),
    ("web.https", "auto"),
    ("web.tls_cert", ""),
    ("web.tls_key", ""),
    ("web.allowed_hosts", ""),
];

/// Per-user data directory: `$XDG_DATA_HOME/empirebt`,
/// `~/.local/share/empirebt`, or `%APPDATA%\EmpireBT` on Windows.
pub fn default_data_dir() -> PathBuf {
    if cfg!(windows)
        && let Some(app_data) = std::env::var_os("APPDATA")
    {
        return PathBuf::from(app_data).join("EmpireBT");
    }
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(data_home).join("empirebt");
    }
    match home_dir() {
        Some(home) => home.join(".local").join("share").join("empirebt"),
        None => PathBuf::from(".empirebt"),
    }
}

fn default_download_dir() -> PathBuf {
    home_dir()
        .map(|home| home.join("Downloads").join("EmpireBT"))
        .unwrap_or_else(|| PathBuf::from("downloads"))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn open(data_dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&data_dir).map_err(|error| {
            format!(
                "cannot create data directory {}: {error}",
                data_dir.display()
            )
        })?;
        Ok(Self { data_dir })
    }

    /// Locks the data directory for this process, so two never use one
    /// session at once. The lock lasts until the returned file is dropped,
    /// and the system releases it if the process dies.
    pub fn lock(&self) -> Result<std::fs::File, String> {
        let path = self.data_dir.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "EmpireBT is already running with the data directory {}; close it first, \
                 or choose another with --data-dir",
                self.data_dir.display()
            )),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(format!("cannot lock {}: {error}", path.display()))
            }
        }
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn session_path(&self) -> PathBuf {
        self.data_dir.join(SESSION_DIR)
    }

    /// The web UI's password hash and API token hashes.
    pub fn web_auth_path(&self) -> PathBuf {
        self.data_dir.join("web-auth")
    }

    /// Where a generated web UI certificate and its key are kept.
    pub fn web_certificate_paths(&self) -> (PathBuf, PathBuf) {
        (
            self.data_dir.join("web-cert.pem"),
            self.data_dir.join("web-key.pem"),
        )
    }

    /// Where categories and tags are saved.
    pub fn labels_path(&self) -> PathBuf {
        self.data_dir.join(LABELS_FILE)
    }

    /// Where the DHT keeps its node id and known nodes between runs.
    pub fn dht_path(&self) -> PathBuf {
        self.data_dir.join(DHT_FILE)
    }

    /// Opens `config.toml`, creating it on first run. A download folder
    /// remembered by an older version seeds the new file and is then retired.
    pub fn config(&self) -> Result<ConfigFile, String> {
        let legacy = self.data_dir.join(LEGACY_DOWNLOAD_DIR_FILE);
        let default_download_dir = std::fs::read_to_string(&legacy)
            .ok()
            .map(|saved| saved.trim().to_owned())
            .filter(|saved| !saved.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_download_dir);
        let mut config =
            ConfigFile::load_or_create(self.data_dir.join(CONFIG_FILE), &default_download_dir)
                .map_err(|error| error.to_string())?;
        let console = config
            .register(CONSOLE_SETTINGS, CONSOLE_DEFAULTS)
            .map_err(|error| error.to_string())?;
        let peers = config
            .register(PEER_SETTINGS, PEER_DEFAULTS)
            .map_err(|error| error.to_string())?;
        let web = config
            .register(WEB_SETTINGS, WEB_DEFAULTS)
            .map_err(|error| error.to_string())?;
        if console || peers || web {
            config.save().map_err(|error| error.to_string())?;
        }
        let _ = std::fs::remove_file(legacy);
        Ok(config)
    }

    /// Loads the saved session, or starts empty when none exists yet. A
    /// session from an older version is converted on the way. Torrents whose
    /// files are damaged are skipped and described in the returned warnings;
    /// a damaged index is reported instead of being silently replaced.
    pub fn load(&self, config: ClientConfig) -> Result<(Client, Vec<String>), String> {
        let dir = self.session_path();
        let legacy = self.data_dir.join(LEGACY_SESSION_FILE);
        if !dir.join("index").exists() {
            if !legacy.exists() {
                let client = Client::with_config(config).map_err(|error| error.to_string())?;
                return Ok((client, Vec::new()));
            }
            let client = Client::load_session(config, &legacy).map_err(|error| {
                format!(
                    "cannot load session {}: {error}\nMove the file aside to start fresh.",
                    legacy.display()
                )
            })?;
            self.save(&client)?;
            // Kept, renamed, in case anything goes wrong with the new format.
            let _ = std::fs::rename(&legacy, legacy.with_extension("dat.migrated"));
            return Ok((client, Vec::new()));
        }
        Client::load_session_dir(config, &dir).map_err(|error| {
            format!(
                "cannot load session {}: {error}\nMove the folder aside to start fresh.",
                dir.display()
            )
        })
    }

    /// Saves the session incrementally: unchanged torrents cost nothing, and
    /// files are replaced atomically so a crash never leaves them truncated.
    pub fn save(&self, client: &Client) -> Result<(), String> {
        client
            .save_session_dir(self.session_path())
            .map_err(|error| format!("cannot save session: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_the_remembered_download_directory_into_the_config() {
        let root = std::env::temp_dir().join(format!("empirebt-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(root.clone()).unwrap();
        std::fs::write(root.join(LEGACY_DOWNLOAD_DIR_FILE), "/media/torrents\n").unwrap();

        let config = store.config().unwrap();
        assert_eq!(
            config.client_config().unwrap().download_directory(),
            std::path::Path::new("/media/torrents")
        );
        assert_eq!(config.get("console.start_on_add").as_deref(), Some("true"));
        assert!(!root.join(LEGACY_DOWNLOAD_DIR_FILE).exists());
        // Later runs read the config file itself.
        assert_eq!(
            store
                .config()
                .unwrap()
                .get("storage.download_dir")
                .as_deref(),
            Some("/media/torrents")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_one_process_uses_a_data_directory() {
        let root = std::env::temp_dir().join(format!("empirebt-store-lock-{}", std::process::id()));
        let store = Store::open(root.clone()).unwrap();
        let held = store.lock().unwrap();
        let error = Store::open(root.clone()).unwrap().lock().unwrap_err();
        assert!(error.contains("already running"), "{error}");
        drop(held);
        store.lock().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_session_starts_empty() {
        let root =
            std::env::temp_dir().join(format!("empirebt-store-empty-{}", std::process::id()));
        let store = Store::open(root.clone()).unwrap();

        let (client, warnings) = store.load(ClientConfig::default()).unwrap();
        assert!(client.torrents().is_empty() && warnings.is_empty());
        store.save(&client).unwrap();
        assert!(store.session_path().join("index").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn converts_a_single_file_session() {
        let root =
            std::env::temp_dir().join(format!("empirebt-store-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(root.clone()).unwrap();
        let mut old = Client::new();
        old.add_magnet(
            empirebt_backend::MagnetLink::parse(
                "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=Old",
            )
            .unwrap(),
        );
        old.save_session(root.join(LEGACY_SESSION_FILE)).unwrap();

        let (client, _) = store.load(ClientConfig::default()).unwrap();
        assert_eq!(client.torrents()[0].name(), "Old");
        assert!(root.join("session.dat.migrated").exists());
        // The next run reads the converted session.
        let (again, _) = store.load(ClientConfig::default()).unwrap();
        assert_eq!(again.torrents().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
