//! The settings file (`config.toml`) shared by every EmpireBT interface.
//!
//! The backend owns the `[storage]`, `[network]`, and `[dht]` sections.
//! Interfaces can register settings of their own (the console uses
//! `[console]`). Comments and formatting in the file are preserved when
//! settings are changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use crate::{BackendError, ClientConfig};

/// The kind of value a setting holds, which decides how input is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Path,
    Port,
    /// A whole number of at least 1.
    Count,
    Seconds,
    /// KiB per second; 0 means unlimited.
    Rate,
    Bool,
    /// Comma-separated `host:port` entries.
    HostList,
    /// One of a fixed list of words.
    Choice(&'static [&'static str]),
    /// A network interface name or IPv4 address; empty for any.
    Interface,
    /// An IPv4 or IPv6 address.
    Address,
    /// Free text, possibly empty.
    Text,
}

/// One setting: its dotted key (`section.name`), what it means, and when a
/// change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    pub kind: SettingKind,
    pub description: &'static str,
    /// Whether a change only applies after restarting EmpireBT.
    pub needs_restart: bool,
}

/// Settings owned by the backend.
pub const BACKEND_SETTINGS: &[Setting] = &[
    Setting {
        key: "storage.download_dir",
        kind: SettingKind::Path,
        description: "Where downloaded files are saved. Existing downloads are not moved.",
        needs_restart: true,
    },
    Setting {
        key: "storage.allowed_dirs",
        kind: SettingKind::Text,
        description: "Folders the web UI may save torrents in besides storage.download_dir, \
                      comma-separated; folders inside them are allowed too. The dashboard can \
                      save anywhere.",
        needs_restart: false,
    },
    Setting {
        key: "network.port",
        kind: SettingKind::Port,
        description: "Port other peers connect to, over TCP and UDP (uTP and the DHT). \
                      Changes apply at once; network.port_file takes precedence.",
        needs_restart: false,
    },
    Setting {
        key: "network.port_file",
        kind: SettingKind::Text,
        description: "A file holding the port to use instead of network.port, checked every \
                      few seconds, such as the port a VPN forwards (gluetun writes it). Empty \
                      for none.",
        needs_restart: false,
    },
    Setting {
        key: "network.max_peers",
        kind: SettingKind::Count,
        description: "Peers to download one torrent from at the same time.",
        needs_restart: true,
    },
    Setting {
        key: "network.max_connections",
        kind: SettingKind::Count,
        description: "Peer connections open at once across all torrents, downloading and \
                      seeding.",
        needs_restart: false,
    },
    Setting {
        key: "network.upload_limit",
        kind: SettingKind::Rate,
        description: "Total upload speed in KiB/s, across all torrents. 0 means unlimited.",
        needs_restart: false,
    },
    Setting {
        key: "network.download_limit",
        kind: SettingKind::Rate,
        description: "Total download speed in KiB/s, across all torrents. 0 means unlimited.",
        needs_restart: false,
    },
    Setting {
        key: "network.peer_timeout",
        kind: SettingKind::Seconds,
        description: "Seconds to wait on an unresponsive peer or tracker.",
        needs_restart: true,
    },
    Setting {
        key: "network.encryption",
        kind: SettingKind::Choice(&["enabled", "required", "disabled"]),
        description: "Encrypt peer connections (MSE). enabled: try encryption, fall back to \
                      plain text. required: refuse unencrypted peers. disabled: never encrypt.",
        needs_restart: true,
    },
    Setting {
        key: "network.utp",
        kind: SettingKind::Choice(&["enabled", "preferred", "disabled"]),
        description: "uTP, BitTorrent over UDP. enabled: accept uTP peers, connect over TCP \
                      first. preferred: connect over uTP first. disabled: TCP only.",
        needs_restart: true,
    },
    Setting {
        key: "network.interface",
        kind: SettingKind::Interface,
        description: "Network interface for every connection, by name (wg0, tun0) or IPv4 \
                      address; empty for any. Usually a VPN's: while it is down, nothing \
                      connects.",
        needs_restart: true,
    },
    Setting {
        key: "network.lsd",
        kind: SettingKind::Bool,
        description: "Find peers on the local network (BEP 14). Private torrents never use it.",
        needs_restart: true,
    },
    Setting {
        key: "network.anonymous",
        kind: SettingKind::Bool,
        description: "Anonymous mode: don't tell peers, trackers, or the DHT that this is \
                      EmpireBT (a random peer ID, a generic User-Agent, no client name). \
                      Private torrents still say so, as private trackers allow only clients \
                      they know. It hides the client, not your IP address: use a VPN for that.",
        needs_restart: false,
    },
    Setting {
        key: "dht.enabled",
        kind: SettingKind::Bool,
        description: "Find peers without trackers. Private torrents never use the DHT.",
        needs_restart: true,
    },
    Setting {
        key: "dht.bootstrap",
        kind: SettingKind::HostList,
        description: "Nodes used to join the DHT network (host:port, comma-separated).",
        needs_restart: true,
    },
];

const HEADER: &str = "\
# EmpireBT settings
#
# Change these from the dashboard (press `c`), with
# `empirebt config set <key> <value>`, or by editing this file while
# EmpireBT is closed. Command-line flags override them for a single run.
";

/// A loaded settings file.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    path: PathBuf,
    document: DocumentMut,
    /// Settings registered by the interface, in addition to the backend's.
    extra: Vec<Setting>,
    /// Values set for this run only (from environment variables), by key:
    /// read in place of the file's, never saved.
    overrides: HashMap<String, Item>,
}

impl ConfigFile {
    /// Loads the settings file, creating it with documented defaults when it
    /// does not exist. `default_download_dir` seeds a new file.
    pub fn load_or_create(
        path: impl Into<PathBuf>,
        default_download_dir: &Path,
    ) -> Result<Self, BackendError> {
        let path = path.into();
        let mut file = match std::fs::read_to_string(&path) {
            Ok(text) => Self {
                document: text.parse::<DocumentMut>().map_err(|error| {
                    invalid(&path, format!("is not valid TOML: {}", error.message()))
                })?,
                path,
                extra: Vec::new(),
                overrides: HashMap::new(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = Self {
                    document: DocumentMut::new(),
                    path,
                    extra: Vec::new(),
                    overrides: HashMap::new(),
                };
                file.fill_missing_backend_settings(default_download_dir);
                // A comment before the first section heads the file.
                if let Some(storage) = file
                    .document
                    .get_mut("storage")
                    .and_then(Item::as_table_mut)
                {
                    storage.decor_mut().set_prefix(format!("{HEADER}\n"));
                }
                file.save()?;
                file
            }
            Err(error) => return Err(BackendError::Io(format!("{}: {error}", path.display()))),
        };
        // Files from older versions gain any settings added since.
        let before = file.document.to_string();
        file.migrate();
        file.fill_missing_backend_settings(default_download_dir);
        if file.document.to_string() != before {
            file.save()?;
        }
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Adds an interface's own settings, writing `defaults` for any that are
    /// missing from the file. Returns whether the file changed.
    pub fn register(
        &mut self,
        settings: &[Setting],
        defaults: &[(&str, &str)],
    ) -> Result<bool, BackendError> {
        self.extra.extend_from_slice(settings);
        let mut changed = false;
        for (key, default) in defaults {
            if self.item(key).is_none() {
                let setting = self.setting(key)?;
                let parsed =
                    parse(&setting, default).map_err(|message| invalid(&self.path, message))?;
                let comment = format!("# {}\n", setting.description);
                self.insert(key, parsed, &comment);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Every known setting: the backend's, then the interface's.
    pub fn settings(&self) -> Vec<Setting> {
        BACKEND_SETTINGS
            .iter()
            .copied()
            .chain(self.extra.iter().copied())
            .collect()
    }

    /// A setting's current value as text, as a user would type it.
    pub fn get(&self, key: &str) -> Option<String> {
        let item = self.item(key)?;
        Some(match item.as_value()? {
            Value::String(text) => text.value().clone(),
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string().trim().to_owned(),
        })
    }

    /// Checks and stores a setting. Call [`ConfigFile::save`] to write it.
    pub fn set(&mut self, key: &str, input: &str) -> Result<(), BackendError> {
        let setting = self.setting(key)?;
        let parsed = parse(&setting, input)
            .map_err(|message| BackendError::InvalidConfiguration(format!("{key}: {message}")))?;
        self.insert(key, parsed, "");
        Ok(())
    }

    /// Checks a setting and uses it for this run only, over the file's
    /// value: [`ConfigFile::get`] and [`ConfigFile::client_config`] see it,
    /// [`ConfigFile::save`] never writes it, and it outlasts later
    /// [`ConfigFile::set`] calls.
    pub fn set_override(&mut self, key: &str, input: &str) -> Result<(), BackendError> {
        let setting = self.setting(key)?;
        let parsed = parse(&setting, input)
            .map_err(|message| BackendError::InvalidConfiguration(format!("{key}: {message}")))?;
        self.overrides.insert(setting.key.to_owned(), value(parsed));
        Ok(())
    }

    /// Whether [`ConfigFile::set_override`] set `key` for this run.
    pub fn is_overridden(&self, key: &str) -> bool {
        self.overrides.contains_key(key)
    }

    /// Writes the file atomically: a temporary file renamed into place.
    pub fn save(&self) -> Result<(), BackendError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| BackendError::Io(error.to_string()))?;
        }
        let temporary = self.path.with_extension("toml.tmp");
        std::fs::write(&temporary, self.document.to_string())
            .and_then(|_| std::fs::rename(&temporary, &self.path))
            .map_err(|error| BackendError::Io(format!("{}: {error}", self.path.display())))
    }

    /// The folders `storage.allowed_dirs` names, with `~` expanded; ones
    /// that are not full paths are left out.
    pub fn allowed_dirs(&self) -> Vec<PathBuf> {
        self.get("storage.allowed_dirs")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(expand_home)
            .filter(|path| path.is_absolute())
            .collect()
    }

    /// Builds the backend configuration from the file.
    pub fn client_config(&self) -> Result<ClientConfig, BackendError> {
        let defaults = ClientConfig::default();
        let path = |key: &str| -> Result<Option<PathBuf>, BackendError> {
            self.typed(key)
                .map(|value| value.map(|text: String| expand_home(&text)))
        };
        let number = |key: &str| -> Result<Option<i64>, BackendError> { self.typed(key) };
        let count = |key: &str, default: usize| -> Result<usize, BackendError> {
            Ok(number(key)?.map_or(default, |value| value as usize))
        };
        // Rates are stored in KiB/s.
        let rate = |key: &str| -> Result<Option<u64>, BackendError> {
            Ok(number(key)?.map(|kib| kib as u64 * 1024))
        };
        let config = defaults
            .clone()
            .with_download_directory(
                path("storage.download_dir")?
                    .unwrap_or_else(|| defaults.download_directory().to_owned()),
            )
            .with_listen_port(
                number("network.port")?.map_or(defaults.listen_port(), |port| port as u16),
            )
            .with_max_peer_connections(count("network.max_peers", defaults.max_peer_connections())?)
            .with_max_connections(count(
                "network.max_connections",
                defaults.max_connections(),
            )?)
            .with_upload_limit(rate("network.upload_limit")?.unwrap_or(defaults.upload_limit()))
            .with_download_limit(
                rate("network.download_limit")?.unwrap_or(defaults.download_limit()),
            )
            .with_peer_timeout(
                number("network.peer_timeout")?.map_or(defaults.peer_timeout(), |seconds| {
                    Duration::from_secs(seconds as u64)
                }),
            )
            .with_encryption(match self.typed::<String>("network.encryption")? {
                None => defaults.encryption(),
                Some(text) => crate::Encryption::parse(&text).ok_or_else(|| {
                    invalid(
                        &self.path,
                        format!(
                            "network.encryption must be enabled, required, or disabled, \
                             not \"{text}\""
                        ),
                    )
                })?,
            })
            .with_utp(match self.typed::<String>("network.utp")? {
                None => defaults.utp(),
                Some(text) => crate::UtpMode::parse(&text).ok_or_else(|| {
                    invalid(
                        &self.path,
                        format!(
                            "network.utp must be enabled, preferred, or disabled, not \"{text}\""
                        ),
                    )
                })?,
            })
            .with_dht(
                self.typed::<bool>("dht.enabled")?
                    .unwrap_or(defaults.dht_enabled()),
            )
            .with_lsd(
                self.typed::<bool>("network.lsd")?
                    .unwrap_or(defaults.lsd_enabled()),
            )
            .with_anonymous(
                self.typed::<bool>("network.anonymous")?
                    .unwrap_or(defaults.anonymous()),
            )
            .with_interface(self.typed::<String>("network.interface")?)
            .with_dht_bootstrap(
                self.typed::<Vec<String>>("dht.bootstrap")?
                    .unwrap_or_else(|| defaults.dht_bootstrap().to_vec()),
            );
        config
            .validate()
            .map_err(|error| invalid(&self.path, error.to_string()))?;
        Ok(config)
    }

    /// Reads a setting, checking it the same way `set` would.
    fn typed<T: FromSetting>(&self, key: &str) -> Result<Option<T>, BackendError> {
        let Some(text) = self.get(key) else {
            return Ok(None);
        };
        let setting = self.setting(key)?;
        let parsed = parse(&setting, &text)
            .map_err(|message| invalid(&self.path, format!("{key}: {message}")))?;
        Ok(T::from_value(&parsed))
    }

    fn setting(&self, key: &str) -> Result<Setting, BackendError> {
        self.settings()
            .into_iter()
            .find(|setting| setting.key == key)
            .ok_or_else(|| {
                BackendError::InvalidConfiguration(format!(
                    "unknown setting \"{key}\"; known settings: {}",
                    self.settings()
                        .iter()
                        .map(|setting| setting.key)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    fn item(&self, key: &str) -> Option<&Item> {
        if let Some(item) = self.overrides.get(key) {
            return Some(item);
        }
        let (section, name) = key.split_once('.')?;
        self.document.get(section)?.as_table()?.get(name)
    }

    fn insert(&mut self, key: &str, parsed: Value, comment: &str) {
        let (section, name) = key.split_once('.').expect("setting keys are dotted");
        let table = self
            .document
            .entry(section)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("settings sections are tables");
        if let Some(item) = table.get_mut(name) {
            // Replacing only the value keeps comments attached to the key.
            *item = value(parsed);
            return;
        }
        table.insert(name, value(parsed));
        if !comment.is_empty()
            && let Some(mut key) = table.key_mut(name)
        {
            key.leaf_decor_mut().set_prefix(comment);
        }
    }

    /// Renames settings from older versions, keeping their values.
    fn migrate(&mut self) {
        // The seeding-only connection limit became the limit for all
        // connections; its old default was the same.
        let Some(network) = self
            .document
            .get_mut("network")
            .and_then(Item::as_table_mut)
        else {
            return;
        };
        if let Some(old) = network.remove("max_inbound")
            && !network.contains_key("max_connections")
        {
            let setting = BACKEND_SETTINGS
                .iter()
                .find(|setting| setting.key == "network.max_connections")
                .expect("a backend setting");
            if let Some(value) = old.as_value().cloned() {
                self.insert(
                    "network.max_connections",
                    value,
                    &format!("# {}\n", setting.description),
                );
            }
        }
    }

    fn fill_missing_backend_settings(&mut self, download_dir: &Path) {
        let defaults = ClientConfig::default();
        let values: [(&str, Value); 16] = [
            (
                "storage.download_dir",
                Value::from(download_dir.to_string_lossy().as_ref()),
            ),
            (
                "network.port",
                Value::from(i64::from(defaults.listen_port())),
            ),
            (
                "network.max_peers",
                Value::from(defaults.max_peer_connections() as i64),
            ),
            (
                "network.max_connections",
                Value::from(defaults.max_connections() as i64),
            ),
            (
                "network.upload_limit",
                Value::from((defaults.upload_limit() / 1024) as i64),
            ),
            (
                "network.download_limit",
                Value::from((defaults.download_limit() / 1024) as i64),
            ),
            (
                "network.peer_timeout",
                Value::from(defaults.peer_timeout().as_secs() as i64),
            ),
            (
                "network.encryption",
                Value::from(defaults.encryption().as_str()),
            ),
            ("network.utp", Value::from(defaults.utp().as_str())),
            ("network.lsd", Value::from(defaults.lsd_enabled())),
            ("network.anonymous", Value::from(defaults.anonymous())),
            ("network.interface", Value::from("")),
            ("network.port_file", Value::from("")),
            ("storage.allowed_dirs", Value::from("")),
            ("dht.enabled", Value::from(defaults.dht_enabled())),
            (
                "dht.bootstrap",
                Value::Array(defaults.dht_bootstrap().iter().collect::<Array>()),
            ),
        ];
        for (key, default) in values {
            if self.item(key).is_none() {
                let setting = BACKEND_SETTINGS
                    .iter()
                    .find(|setting| setting.key == key)
                    .expect("defaults cover backend settings");
                self.insert(key, default, &format!("# {}\n", setting.description));
            }
        }
    }
}

/// Converts a checked TOML value into the type a caller wants.
trait FromSetting: Sized {
    fn from_value(value: &Value) -> Option<Self>;
}

impl FromSetting for String {
    fn from_value(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_owned)
    }
}

impl FromSetting for i64 {
    fn from_value(value: &Value) -> Option<Self> {
        value.as_integer()
    }
}

impl FromSetting for bool {
    fn from_value(value: &Value) -> Option<Self> {
        value.as_bool()
    }
}

impl FromSetting for Vec<String> {
    fn from_value(value: &Value) -> Option<Self> {
        Some(
            value
                .as_array()?
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
        )
    }
}

/// Checks user input for a setting and converts it to a TOML value.
fn parse(setting: &Setting, input: &str) -> Result<Value, String> {
    let input = input.trim();
    let whole = |minimum: i64, maximum: i64, what: &str| {
        input
            .parse::<i64>()
            .ok()
            .filter(|number| (minimum..=maximum).contains(number))
            .map(Value::from)
            .ok_or_else(|| format!("expected {what} from {minimum} to {maximum}, got \"{input}\""))
    };
    match setting.kind {
        SettingKind::Path if input.is_empty() => Err("a folder path is required".to_owned()),
        SettingKind::Path => Ok(Value::from(input)),
        SettingKind::Port => whole(1, 65_535, "a port number"),
        SettingKind::Count => whole(1, 10_000, "a whole number"),
        SettingKind::Seconds => whole(1, 3_600, "a number of seconds"),
        SettingKind::Rate => whole(0, 10_000_000, "a speed in KiB/s (0 for unlimited)"),
        SettingKind::Bool => match input.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Value::from(true)),
            "false" | "no" | "off" | "0" => Ok(Value::from(false)),
            _ => Err(format!("expected true or false, got \"{input}\"")),
        },
        SettingKind::HostList => {
            let hosts: Vec<&str> = input
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .collect();
            if let Some(bad) = hosts.iter().find(|host| {
                host.rsplit_once(':')
                    .is_none_or(|(name, port)| name.is_empty() || port.parse::<u16>().is_err())
            }) {
                return Err(format!("expected host:port entries, got \"{bad}\""));
            }
            Ok(Value::Array(hosts.into_iter().collect()))
        }
        SettingKind::Interface | SettingKind::Text => Ok(Value::from(input)),
        SettingKind::Address => input
            .parse::<std::net::IpAddr>()
            .map(|address| Value::from(address.to_string()))
            .map_err(|_| format!("expected an IP address such as 127.0.0.1, got \"{input}\"")),
        SettingKind::Choice(choices) => {
            let lower = input.to_ascii_lowercase();
            choices
                .iter()
                .find(|choice| **choice == lower)
                .map(|choice| Value::from(*choice))
                .ok_or_else(|| format!("expected one of {}, got \"{input}\"", choices.join(", ")))
        }
    }
}

fn expand_home(text: &str) -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    match (text.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(text),
    }
}

fn invalid(path: &Path, message: impl std::fmt::Display) -> BackendError {
    BackendError::InvalidConfiguration(format!("{}: {message}", path.display()))
}

#[cfg(test)]
mod tests {
    /// Overrides (from environment variables) win for this run but never
    /// reach the file.
    #[test]
    fn overrides_apply_for_the_run_but_are_never_saved() {
        let dir = std::env::temp_dir().join(format!("empirebt-override-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mut file = ConfigFile::load_or_create(&path, &dir.join("downloads")).unwrap();
        file.set_override("network.upload_limit", "300").unwrap();
        file.set_override("storage.download_dir", "/downloads")
            .unwrap();
        assert!(file.set_override("network.port", "not a port").is_err());
        assert!(file.set_override("no.such", "1").is_err());
        assert!(file.is_overridden("network.upload_limit"));
        assert_eq!(file.get("network.upload_limit").as_deref(), Some("300"));
        let config = file.client_config().unwrap();
        assert_eq!(config.upload_limit(), 300 * 1024);
        assert_eq!(config.download_directory(), Path::new("/downloads"));

        // A change to the file keeps the override in force, and saving
        // writes the change but not the override.
        file.set("network.upload_limit", "50").unwrap();
        file.save().unwrap();
        assert_eq!(file.get("network.upload_limit").as_deref(), Some("300"));
        let saved = ConfigFile::load_or_create(&path, &dir.join("downloads")).unwrap();
        assert_eq!(saved.get("network.upload_limit").as_deref(), Some("50"));
        assert_ne!(
            saved.get("storage.download_dir").as_deref(),
            Some("/downloads")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("empirebt-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.toml")
    }

    #[test]
    fn creates_a_documented_default_file() {
        let path = temp_path("defaults");
        let file = ConfigFile::load_or_create(&path, Path::new("/data/torrents")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# EmpireBT settings"));
        assert!(text.contains("# Port other peers connect to"));
        assert!(text.contains("port = 6881"));

        let config = file.client_config().unwrap();
        assert_eq!(config.download_directory(), Path::new("/data/torrents"));
        assert_eq!(config.listen_port(), 6881);
        assert!(config.dht_enabled());
        assert_eq!(config.dht_bootstrap().len(), 3);
        assert_eq!(config.encryption(), crate::Encryption::Enabled);
        assert!(text.contains("encryption = \"enabled\""));
        assert_eq!(config.utp(), crate::UtpMode::Enabled);
        assert!(text.contains("utp = \"enabled\""));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rates_are_kib_per_second_and_zero_is_unlimited() {
        let path = temp_path("rates");
        let mut file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        let config = file.client_config().unwrap();
        assert_eq!((config.upload_limit(), config.download_limit()), (0, 0));
        file.set("network.upload_limit", "512").unwrap();
        file.set("network.download_limit", "0").unwrap();
        let config = file.client_config().unwrap();
        assert_eq!(
            (config.upload_limit(), config.download_limit()),
            (512 * 1024, 0)
        );
        let error = file.set("network.upload_limit", "-1").unwrap_err();
        assert!(error.to_string().contains("KiB/s"), "{error}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_network_interface_is_any_until_one_is_chosen() {
        let path = temp_path("interface");
        let mut file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        assert_eq!(file.client_config().unwrap().interface(), None);
        file.set("network.interface", "wg0").unwrap();
        let config = file.client_config().unwrap();
        assert_eq!(config.interface(), Some("wg0"));
        assert_eq!(config.binding().name(), Some("wg0"));
        file.set("network.interface", "").unwrap();
        assert_eq!(
            file.client_config().unwrap().binding(),
            crate::Binding::any()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_old_seeding_connection_limit_becomes_the_global_one() {
        let path = temp_path("migrate");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[network]\n# mine\nmax_inbound = 1200\n").unwrap();
        let file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        assert_eq!(file.client_config().unwrap().max_connections(), 1200);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("max_inbound"), "{text}");
        assert!(text.contains("max_connections = 1200"), "{text}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn encryption_accepts_only_known_policies() {
        let path = temp_path("encryption");
        let mut file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        file.set("network.encryption", "Required").unwrap();
        assert_eq!(file.get("network.encryption").as_deref(), Some("required"));
        assert_eq!(
            file.client_config().unwrap().encryption(),
            crate::Encryption::Required
        );
        let error = file.set("network.encryption", "rc4").unwrap_err();
        assert!(
            error.to_string().contains("enabled, required, disabled"),
            "{error}"
        );

        std::fs::write(&path, "[network]\nencryption = \"maybe\"\n").unwrap();
        let error = ConfigFile::load_or_create(&path, Path::new("/data"))
            .and_then(|file| file.client_config())
            .unwrap_err();
        assert!(error.to_string().contains("network.encryption"), "{error}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn changes_persist_and_keep_user_comments() {
        let path = temp_path("edit");
        ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("port = 6881", "# my router forwards this\nport = 6881"),
        )
        .unwrap();
        let mut file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();

        file.set("network.port", "51413").unwrap();
        file.set("dht.enabled", "no").unwrap();
        file.set("dht.bootstrap", "a.example:1, b.example:2")
            .unwrap();
        file.save().unwrap();

        let reloaded = ConfigFile::load_or_create(&path, Path::new("/elsewhere")).unwrap();
        let config = reloaded.client_config().unwrap();
        assert_eq!(config.listen_port(), 51413);
        assert!(!config.dht_enabled());
        assert_eq!(config.dht_bootstrap(), ["a.example:1", "b.example:2"]);
        // The existing download folder is not replaced by the new default.
        assert_eq!(config.download_directory(), Path::new("/data"));
        assert_eq!(
            reloaded.get("dht.bootstrap").unwrap(),
            "a.example:1, b.example:2"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# my router forwards this")
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_bad_values_with_clear_messages() {
        let path = temp_path("bad");
        let mut file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        let message = |result: Result<(), BackendError>| result.unwrap_err().to_string();
        assert!(message(file.set("network.port", "99999")).contains("port number from 1 to 65535"));
        assert!(message(file.set("network.max_peers", "0")).contains("whole number"));
        assert!(message(file.set("dht.enabled", "maybe")).contains("true or false"));
        assert!(message(file.set("dht.bootstrap", "nohost")).contains("host:port"));
        assert!(message(file.set("network.speed", "1")).contains("unknown setting"));

        // A hand-edited file with a bad value names the file and the key.
        std::fs::write(&path, "[network]\nport = \"fast\"\n").unwrap();
        let error = ConfigFile::load_or_create(&path, Path::new("/data"))
            .unwrap()
            .client_config()
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("config.toml") && error.contains("network.port"),
            "{error}"
        );
        std::fs::write(&path, "[network\n").unwrap();
        assert!(ConfigFile::load_or_create(&path, Path::new("/data")).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn interfaces_register_their_own_settings() {
        let path = temp_path("register");
        let mut file = ConfigFile::load_or_create(&path, Path::new("/data")).unwrap();
        const CONSOLE: &[Setting] = &[Setting {
            key: "console.start_on_add",
            kind: SettingKind::Bool,
            description: "Start downloading as soon as a torrent is added.",
            needs_restart: false,
        }];
        assert!(
            file.register(CONSOLE, &[("console.start_on_add", "true")])
                .unwrap()
        );
        file.save().unwrap();
        assert_eq!(file.get("console.start_on_add").as_deref(), Some("true"));
        file.set("console.start_on_add", "off").unwrap();
        assert_eq!(file.get("console.start_on_add").as_deref(), Some("false"));
        assert!(
            file.settings()
                .iter()
                .any(|setting| setting.key == "console.start_on_add")
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
