//! Where log lines go and how many there are.
//!
//! The backend, the engine, and the web UI describe what happens with
//! `tracing` events, each under a short area name (`torrent`, `tracker`,
//! `dht`, `peer`, `web`, …). This sets up the one subscriber that writes
//! them: to standard error for `empirebt serve` (so `docker logs` shows
//! them), or to a size-capped file while the dashboard has the terminal.
//! `log.level` decides how much: a level, or directives such as
//! `info,dht=debug`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;

/// A log file grows to this size, then starts over, keeping the previous
/// one as `<name>.1`.
const MAX_LOG_FILE: u64 = 10 << 20;

/// Where the log goes.
pub enum Destination<'a> {
    /// Standard error: `empirebt serve`, and one-off commands.
    Stderr,
    /// A file: the dashboard owns the terminal.
    File(&'a Path),
}

/// Starts logging; returns a warning to show when `level` is not usable
/// (logging then uses `info`).
pub fn start(level: &str, destination: Destination<'_>) -> Option<String> {
    let (filter, problem) = match filter(level) {
        Ok(filter) => (filter, None),
        Err(error) => (
            filter("info").expect("info is a valid filter"),
            Some(format!(
                "log.level {level:?} is not usable ({error}); logging at info"
            )),
        ),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);
    let started = match destination {
        Destination::Stderr => {
            use std::io::IsTerminal;
            builder
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr)
                .try_init()
        }
        Destination::File(path) => match LogFile::open(path) {
            Ok(file) => builder.with_ansi(false).with_writer(file).try_init(),
            Err(error) => {
                return Some(format!("cannot write the log {}: {error}", path.display()));
            }
        },
    };
    // Already started (tests) is fine.
    drop(started);
    problem
}

/// `log.level` as a filter: a level for everything, then `area=level`
/// for areas that should say more or less. A bare word must be a level,
/// so a typo is reported instead of being taken for an area's name.
fn filter(level: &str) -> Result<EnvFilter, String> {
    const LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];
    let level = level.trim();
    let level = if level.is_empty() { "info" } else { level };
    for directive in level.split(',').map(str::trim) {
        let word = directive.rsplit('=').next().unwrap_or_default();
        if !LEVELS.contains(&word.to_ascii_lowercase().as_str()) {
            return Err(format!(
                "{directive} is not a level ({})",
                LEVELS.join(", ")
            ));
        }
    }
    EnvFilter::builder()
        .parse(level)
        .map_err(|error| error.to_string())
}

/// A log file that starts over at [`MAX_LOG_FILE`], keeping one old copy.
#[derive(Clone)]
pub struct LogFile {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    path: PathBuf,
    file: File,
    size: u64,
    limit: u64,
}

impl LogFile {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Self::with_limit(path, MAX_LOG_FILE)
    }

    fn with_limit(path: &Path, limit: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                path: path.to_owned(),
                file,
                size,
                limit,
            })),
        })
    }
}

impl Inner {
    /// Moves the full log aside and starts a new one.
    fn roll(&mut self) -> std::io::Result<()> {
        let mut old = self.path.clone().into_os_string();
        old.push(".1");
        std::fs::rename(&self.path, old)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

impl Write for LogFile {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.size > 0 && inner.size + data.len() as u64 > inner.limit {
            // A log that cannot roll keeps growing rather than failing.
            let _ = inner.roll();
        }
        let written = inner.file.write(data)?;
        inner.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .file
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFile {
    type Writer = LogFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_levels_and_area_directives() {
        assert!(filter("info").is_ok());
        assert!(filter("").is_ok());
        assert!(filter("warn,dht=debug,tracker=trace").is_ok());
        assert!(filter("infoo").is_err(), "a typo is not taken for an area");
        assert!(filter("dht=nonsense").is_err());
    }

    #[test]
    fn the_log_file_starts_over_when_full() {
        let dir = std::env::temp_dir().join(format!("empirebt-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("empirebt.log");
        let mut log = LogFile::with_limit(&path, 100).unwrap();
        for line in 0..10 {
            writeln!(log, "line {line:02} padding padding padding").unwrap();
        }
        let current = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::read_to_string(dir.join("empirebt.log.1")).unwrap();
        assert!(current <= 100, "{current}");
        assert!(!old.is_empty() && old.len() <= 100);
        assert!(std::fs::read_to_string(&path).unwrap().contains("line 09"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
