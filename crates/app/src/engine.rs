//! The engine thread: owns the client and everything around it, runs
//! commands from interfaces, and publishes snapshots.

use crate::labels::{Labels, clean_name};
use crate::snapshot::{
    DhtState, Level, Network, Notice, PeerView, RateMeter, SeedStatus, SettingView, Snapshot,
    Status, TorrentView,
};
use crate::{InfoHash, Store};
use empirebt_backend::{
    AnnounceStatus, BackendError, Client, ClientConfig, ConfigFile, ContentLayout, Dht, MagnetLink,
    Runtime, SeedAnnouncer, TorrentInfo, TransferHandle, TransferKind, TransferPhase,
    TransferProgress, TransferState, UtpSocket,
};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often background work advances and a snapshot is published.
const TICK: Duration = Duration::from_millis(250);
/// Notices kept in snapshots.
const NOTICES_KEPT: usize = 20;
/// How long quitting waits for transfers to stop and for best-effort
/// `stopped` announces to reach trackers.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// How long a port change waits on the spot for the old sockets to close,
/// so it usually shows when the command returns; past that, the engine
/// finishes it in the background and keeps answering meanwhile.
const REBIND_WAIT: Duration = Duration::from_millis(250);
/// Data files unused this long are closed.
const IDLE_FILE_CLOSE: Duration = Duration::from_secs(5 * 60);
/// Local-data checks running at once. More would make spinning disks seek.
const VERIFY_CONCURRENCY: usize = 2;
/// Time per tick spent moving the check queue along.
const VERIFY_BUDGET: Duration = Duration::from_millis(40);
/// A seeding runtime that failed to start (port taken, network interface
/// down) is tried again this often.
const SEED_RETRY: Duration = Duration::from_secs(10);
/// How often seeds that could not join the runtime (files missing or
/// unreadable, as on a drive that is not mounted yet) are tried again.
const LEFT_OUT_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(60)
};
/// How long one request may take when downloading a `.torrent` from a URL.
const TORRENT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// How often every torrent's trackers are scraped for swarm counts, as
/// libtorrent's `auto_scrape_interval`.
const SCRAPE_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// The first scrape waits for the first announces.
const FIRST_SCRAPE: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(60)
};
/// Scrape requests in flight at once.
const SCRAPE_WORKERS: usize = 8;
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(15);

/// How often `network.port_file` is checked for a new port.
const PORT_FILE_POLL: Duration = Duration::from_secs(5);
/// How soon a session save that failed (a full disk, say) is tried again.
const SAVE_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(30)
};

/// Something an interface asks the engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Adds a magnet link, a `.torrent` file by path, or a `.torrent` URL
    /// (downloaded in the background), and starts it unless the
    /// `console.start_on_add` setting is off.
    Add(String),
    /// Adds a `.torrent` file from its contents, such as an upload.
    AddTorrent(Vec<u8>),
    /// Adds a torrent with options, such as the folder to save it in.
    AddWith(AddRequest),
    Start(InfoHash),
    Pause(InfoHash),
    /// Pauses a running or seeding torrent, starts any other.
    Toggle(InfoHash),
    /// Queues a check of the torrent's local data.
    Recheck(InfoHash),
    /// Announces a running or seeding torrent to its trackers and the DHT
    /// now, rather than when next due, to find new peers.
    Reannounce(InfoHash),
    /// Removes the torrent from the list. Downloaded files stay on disk.
    Remove(InfoHash),
    /// Removes the torrent and deletes its downloaded files, once its
    /// transfer has stopped. Files another torrent in the list also uses
    /// stay, as do other files in its folders.
    RemoveWithFiles(InfoHash),
    /// Validates, stores, and saves a setting, applying it at once where it
    /// can be.
    ChangeSetting {
        key: String,
        value: String,
    },
    /// Gives a category, made if new, the folder its new torrents are saved
    /// in; `None` takes the folder away. Torrents already in it stay where
    /// they are.
    SetCategoryFolder {
        category: String,
        folder: Option<PathBuf>,
    },
    /// Puts a torrent in a category, made if new; `None` takes it out.
    SetCategory {
        torrent: InfoHash,
        category: Option<String>,
    },
    /// Replaces a torrent's tags; new ones are made.
    SetTags {
        torrent: InfoHash,
        tags: Vec<String>,
    },
    CreateCategory(String),
    /// Removes a category; its torrents are left without one.
    RemoveCategory(String),
    CreateTag(String),
    /// Removes a tag from the list and from every torrent.
    RemoveTag(String),
    /// The peers connected for a torrent, answered as [`Reply::Peers`].
    Peers(InfoHash),
    /// Chooses the name shown for a torrent; `None` shows its own again.
    /// Its files are not touched.
    RenameTorrent {
        torrent: InfoHash,
        name: Option<String>,
    },
    /// Renames or moves a file or folder of a torrent, by its path in the
    /// torrent's folder (as [`TorrentView::files`] gives it), moving what
    /// is on disk.
    RenamePath {
        torrent: InfoHash,
        from: PathBuf,
        to: PathBuf,
    },
}

/// A torrent to add, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRequest {
    pub source: AddSource,
    /// The folder to save it in: a full path (`~` is expanded). Unset, it
    /// is the category's folder, or else the download directory.
    pub save_dir: Option<PathBuf>,
    /// Its category, made if new.
    pub category: Option<String>,
    /// Its tags, made if new.
    pub tags: Vec<String>,
    /// Whether to start it; unset follows `console.start_on_add`.
    pub start: Option<bool>,
    /// How its files are laid out in the folder.
    pub layout: ContentLayout,
}

impl AddRequest {
    /// Adds `source` as usual: the default folder, no labels.
    pub fn new(source: AddSource) -> Self {
        Self {
            source,
            save_dir: None,
            category: None,
            tags: Vec::new(),
            start: None,
            layout: ContentLayout::Original,
        }
    }
}

/// Where and how a torrent being added goes, checked.
#[derive(Debug, Clone, Default)]
struct Placement {
    save_dir: Option<PathBuf>,
    category: Option<String>,
    tags: BTreeSet<String>,
    start: Option<bool>,
    layout: ContentLayout,
}

/// What [`AddRequest`] adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSource {
    /// A magnet link, a `.torrent` file by path, or a `.torrent` URL.
    Link(String),
    /// A `.torrent` file's contents.
    Torrent(Vec<u8>),
}

/// How a command went.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Done,
    /// A torrent was added.
    Added(InfoHash),
    /// A `.torrent` URL is being downloaded; it is added when done.
    Fetching,
    /// A torrent's connected peers, oldest connection first.
    Peers(Vec<PeerView>),
}

/// Why a command failed, for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    /// The torrent the failure is about, such as one already in the list.
    pub torrent: Option<InfoHash>,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            torrent: None,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

enum Message {
    Command(Command, mpsc::Sender<Result<Reply, Failure>>),
    Shutdown(mpsc::Sender<()>),
}

/// The latest snapshot, and a way to wait for the next.
struct Published {
    snapshot: Mutex<Arc<Snapshot>>,
    changed: Condvar,
}

/// How interfaces reach the engine. Cheap to clone and share between
/// threads; the engine runs until [`Handle::shutdown`] or until every handle
/// is dropped.
#[derive(Clone)]
pub struct Handle {
    messages: mpsc::Sender<Message>,
    published: Arc<Published>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Handle")
    }
}

impl Handle {
    /// Loads the session from `store`, starts networking (the DHT, uTP,
    /// local peer discovery) as `config` says, and starts the engine.
    /// `overridden` names the settings that command-line flags override
    /// for this run. Interrupted downloads resume, paused torrents with
    /// stale progress are re-checked, and seeds rejoin the runtime.
    pub fn start(
        store: Store,
        config_file: ConfigFile,
        config: ClientConfig,
        overridden: Vec<&'static str>,
    ) -> Result<Self, String> {
        // Every peer connection and pooled data file is an open file. Many
        // systems start processes with a soft limit of 1,024; raise it as
        // far as the system allows.
        let _ = rlimit::increase_nofile_limit(u64::MAX);
        // A port file that already names a port (a VPN's forwarded port)
        // decides it from the start.
        let config = match PortFile::read(&config_file) {
            PortFile::Port(port) => config.with_listen_port(port),
            _ => config,
        };
        let (mut client, warnings) = store.load(config.clone())?;
        let settings = Settings {
            peer_id: empirebt_backend::generate_peer_id(),
            anonymous: config.anonymous(),
            peer_timeout: config.peer_timeout(),
            binding: config.binding(),
        };
        client.set_anonymous_peer_id(
            settings
                .anonymous
                .then(empirebt_backend::generate_anonymous_peer_id),
        );
        let dht_state = start_networking(&mut client, &config, &settings, &store);
        let (labels, damaged) = match Labels::load(&store.labels_path()) {
            Ok(labels) => (labels, None),
            Err(error) => {
                // Kept for the user rather than overwritten.
                let path = store.labels_path();
                let _ = std::fs::rename(&path, path.with_extension("damaged"));
                (Labels::default(), Some(error))
            }
        };
        let geoip = if config_file.get("peers.countries").as_deref() == Some("false") {
            crate::geoip::GeoIp::default()
        } else {
            crate::geoip::GeoIp::start(
                store.data_dir().join(crate::geoip::FILE),
                settings.binding.clone(),
            )
        };
        let on_off = |on: bool| if on { "on" } else { "off" };
        tracing::info!(
            target: "engine",
            "EmpireBT {}, {}; {} Source: {}",
            env!("CARGO_PKG_VERSION"),
            crate::COPYRIGHT,
            crate::LICENSE_NOTICE,
            crate::SOURCE
        );
        tracing::info!(
            target: "engine",
            "EmpireBT {} starting: data in {}, downloads to {}, peers on port {}{}; DHT {}, uTP {}, \
             local discovery {}; {} torrents",
            env!("CARGO_PKG_VERSION"),
            store.data_dir().display(),
            config.download_directory().display(),
            client.config().listen_port(),
            settings
                .binding
                .name()
                .map(|name| format!(" through {name}"))
                .unwrap_or_default(),
            match &dht_state {
                DhtState::Running => "on".to_owned(),
                DhtState::Disabled => "off".to_owned(),
                DhtState::Failed(error) => format!("failed ({error})"),
            },
            on_off(client.utp().is_some()),
            on_off(client.lsd().is_some()),
            client.torrents().len(),
        );
        let mut engine = Engine::new(client, store, settings, dht_state, config_file, overridden);
        engine.labels = labels;
        engine.geoip = geoip;
        engine.report_skipped(&warnings);
        if let Some(error) = damaged {
            engine.notify(
                Level::Error,
                format!(
                    "{error}; categories and tags start empty (the old file was kept as \
                     labels.damaged)"
                ),
            );
        }
        let published = Arc::new(Published {
            snapshot: Mutex::new(Arc::new(engine.snapshot())),
            changed: Condvar::new(),
        });
        let (messages, inbox) = mpsc::channel();
        let thread = {
            let published = Arc::clone(&published);
            std::thread::Builder::new()
                .name("engine".to_owned())
                .spawn(move || run(engine, &inbox, &published))
                .map_err(|error| error.to_string())?
        };
        Ok(Self {
            messages,
            published,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }

    /// The latest snapshot.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&lock(&self.published.snapshot))
    }

    /// Waits until a snapshot newer than `generation` is published, or for
    /// at most `timeout`, and returns the latest.
    pub fn wait_for_change(&self, generation: u64, timeout: Duration) -> Arc<Snapshot> {
        let guard = lock(&self.published.snapshot);
        let (guard, _) = self
            .published
            .changed
            .wait_timeout_while(guard, timeout, |snapshot| snapshot.generation <= generation)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&guard)
    }

    /// Runs a command and returns how it went. Once this returns, the
    /// latest snapshot shows its effect.
    pub fn call(&self, command: Command) -> Result<Reply, Failure> {
        let (reply, result) = mpsc::channel();
        self.messages
            .send(Message::Command(command, reply))
            .map_err(|_| Failure::new("EmpireBT is shutting down"))?;
        result
            .recv()
            .unwrap_or_else(|_| Err(Failure::new("EmpireBT is shutting down")))
    }

    /// Saves the session with interrupted downloads still marked as
    /// downloading so the next run resumes them, then stops every thread.
    /// Waits for that; later calls return at once.
    pub fn shutdown(&self) {
        let (done, finished) = mpsc::channel();
        if self.messages.send(Message::Shutdown(done)).is_ok() {
            let _ = finished.recv();
        }
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// Starts the DHT, uTP, and local peer discovery as configured.
fn start_networking(
    client: &mut Client,
    config: &ClientConfig,
    settings: &Settings,
    store: &Store,
) -> DhtState {
    let dht_state = if !config.dht_enabled() {
        DhtState::Disabled
    } else {
        // The DHT shares the peer port number, over UDP.
        match Dht::start_on(
            &settings.binding,
            config.listen_port(),
            config.dht_bootstrap().to_vec(),
            Some(store.dht_path()),
        ) {
            Ok(dht) => {
                dht.set_anonymous(settings.anonymous);
                client.set_dht(Some(dht));
                DhtState::Running
            }
            Err(error) => DhtState::Failed(error.to_string()),
        }
    };
    // uTP shares the UDP port with the DHT, or binds it itself.
    if config.utp() != empirebt_backend::UtpMode::Disabled {
        let utp = match client.dht() {
            Some(dht) => UtpSocket::on_dht(dht),
            None => settings
                .binding
                .listen_addresses(config.listen_port())
                .and_then(|addresses| UtpSocket::bind_all(&addresses)),
        };
        client.set_utp(utp.ok());
    }
    // Local peer discovery is a bonus: without it, everything else still
    // works.
    if config.lsd_enabled()
        && let Ok(lsd) =
            empirebt_backend::Lsd::start_on(settings.binding.clone(), config.listen_port())
    {
        client.set_lsd(Some(lsd));
    }
    dht_state
}

/// The engine thread: commands as they come, background work every tick.
fn run(mut engine: Engine, inbox: &mpsc::Receiver<Message>, published: &Published) {
    let mut next_tick = Instant::now();
    loop {
        // Due ticks come first, so a steady stream of commands (a script
        // polling the API, several open web pages) never holds back
        // background work.
        if Instant::now() >= next_tick {
            engine.tick();
            publish(&mut engine, published);
            next_tick = Instant::now() + TICK;
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(wait) {
            Ok(Message::Command(command, reply)) => {
                let result = engine.handle(command);
                // Published first, so the caller sees the effect at once.
                publish(&mut engine, published);
                let _ = reply.send(result);
            }
            Ok(Message::Shutdown(done)) => {
                engine.shutdown();
                publish(&mut engine, published);
                let _ = done.send(());
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                engine.shutdown();
                return;
            }
        }
    }
}

fn publish(engine: &mut Engine, published: &Published) {
    let snapshot = Arc::new(engine.snapshot());
    *lock(&published.snapshot) = snapshot;
    published.changed.notify_all();
}

struct Settings {
    peer_id: [u8; 20],
    /// `network.anonymous`: public torrents don't say they are EmpireBT.
    anonymous: bool,
    peer_timeout: Duration,
    /// The network interface every connection uses.
    binding: empirebt_backend::Binding,
}

/// Torrents scraped in one request: the tracker, the User-Agent, and each
/// torrent with the hash the tracker knows it by.
type ScrapeBatch = (String, &'static str, Vec<ScrapedAs>);

/// A torrent, and the hash its tracker knows it by (a v2 torrent's is its
/// truncated v2 hash).
type ScrapedAs = (InfoHash, [u8; 20]);

/// A removed torrent's files, deleted once `job` (its transfer, if it had
/// one) has stopped.
struct Deletion {
    name: String,
    root: PathBuf,
    paths: Vec<PathBuf>,
    job: Option<TransferHandle>,
}

/// A size for notices: `1.5 GiB`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[derive(Default)]
struct Activity {
    job: Option<TransferHandle>,
    /// Counters of the current or most recent transfer.
    progress: Option<Arc<TransferProgress>>,
    error: Option<String>,
    down: RateMeter,
    up: RateMeter,
    queued_verify: bool,
    pausing: bool,
    start_after_verify: bool,
}

impl Activity {
    fn job_kind(&self) -> Option<TransferKind> {
        self.job.as_ref().map(TransferHandle::kind)
    }
}

/// Runs the backend's runtime, the one event loop for every connection,
/// on a background thread for the whole session. Downloads run in it and
/// stay in it as seeds when they finish; seeding torrents join and leave
/// as their state changes, without disturbing other connections. Trackers
/// hear about everything it seeds.
struct SeedService {
    port: u16,
    status: Arc<Mutex<SeedStatus>>,
    runtime: Arc<Mutex<Option<Arc<Runtime>>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    started: bool,
    /// When the runtime last started; a failed one is retried after a
    /// while, such as once a chosen network interface is back.
    last_start: Option<Instant>,
    /// Torrent states changed, so the runtime's torrents need syncing.
    resync: bool,
    /// When the runtime's torrents were last synced.
    last_sync: Instant,
    /// Seeds that could not join the runtime, and why.
    left_out: HashMap<InfoHash, String>,
    announcer: SeedAnnouncer,
    tracking: bool,
}

impl SeedService {
    fn new(port: u16, settings: &Settings, dht: Option<Arc<Dht>>) -> Self {
        Self {
            port,
            status: Arc::new(Mutex::new(SeedStatus::Idle)),
            runtime: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
            started: false,
            last_start: None,
            resync: true,
            last_sync: Instant::now(),
            left_out: HashMap::new(),
            announcer: SeedAnnouncer::start_on(
                settings.binding.clone(),
                port,
                settings.peer_timeout,
                dht,
            ),
            tracking: false,
        }
    }

    fn announce_status(&self, hash: &InfoHash) -> Option<AnnounceStatus> {
        self.announcer.status(hash)
    }

    fn live(&self) -> Option<Arc<Runtime>> {
        self.runtime.lock().ok().and_then(|runtime| runtime.clone())
    }

    fn status(&self) -> SeedStatus {
        let status = self
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or(SeedStatus::Idle);
        match (status, self.live()) {
            (SeedStatus::Listening { .. }, Some(runtime)) => SeedStatus::Listening {
                torrents: runtime.seeding_count(),
            },
            (status, _) => status,
        }
    }

    /// Peers currently connected, downloading or seeding.
    fn peers(&self) -> usize {
        self.live().map_or(0, |runtime| runtime.connections())
    }

    fn uploaded(&self) -> HashMap<InfoHash, u64> {
        self.live()
            .map(|runtime| runtime.uploads())
            .unwrap_or_default()
    }

    /// Starts the runtime once, gives it to the client when it is live,
    /// and syncs its torrents after state changes, and now and then while
    /// some could not join. Returns the seeds newly left out, with why.
    fn tick(&mut self, client: &mut Client, settings: &Settings) -> Vec<(InfoHash, String)> {
        let failed = matches!(self.status(), SeedStatus::Failed(_))
            && self.thread.as_ref().is_some_and(JoinHandle::is_finished);
        let retry = failed
            && settings.binding.is_up()
            && self
                .last_start
                .is_none_or(|started| started.elapsed() >= SEED_RETRY);
        if !self.started || retry {
            self.started = true;
            self.last_start = Some(Instant::now());
            self.start(client, settings);
        }
        let Some(runtime) = self.live() else {
            return Vec::new();
        };
        if client.runtime().is_none() {
            client.set_runtime(Some(Arc::clone(&runtime)));
        }
        if !self.tracking {
            self.tracking = true;
            self.announcer.track(Arc::clone(&runtime));
        }
        let retry = !self.left_out.is_empty() && self.last_sync.elapsed() >= LEFT_OUT_RETRY;
        if !std::mem::take(&mut self.resync) && !retry {
            return Vec::new();
        }
        self.last_sync = Instant::now();
        let left_out: HashMap<InfoHash, String> = client
            .sync_runtime(settings.peer_id, settings.peer_timeout)
            .into_iter()
            .collect();
        let new = left_out
            .iter()
            .filter(|(hash, reason)| self.left_out.get(*hash) != Some(*reason))
            .map(|(hash, reason)| (*hash, reason.clone()))
            .collect();
        self.left_out = left_out;
        new
    }

    fn start(&mut self, client: &Client, settings: &Settings) {
        let client = client.clone();
        let (peer_id, timeout, port) = (settings.peer_id, settings.peer_timeout, self.port);
        let (status, slot, shutdown) = (
            Arc::clone(&self.status),
            Arc::clone(&self.runtime),
            Arc::clone(&self.shutdown),
        );
        let set = move |value: SeedStatus| {
            if let Ok(mut status) = status.lock() {
                *status = value;
            }
        };
        set(SeedStatus::Starting);
        self.thread = Some(std::thread::spawn(move || {
            let runtime = match client.seeding_runtime(peer_id, timeout) {
                Ok(runtime) => Arc::new(runtime),
                Err(error) => return set(SeedStatus::Failed(error.to_string())),
            };
            // An IPv4 and an IPv6 listener, on the same port.
            let addresses = client.config().binding().listen_addresses(port);
            let listeners = match addresses.and_then(|addresses| runtime.bind_all(&addresses)) {
                Ok(listeners) => listeners,
                Err(error) => return set(SeedStatus::Failed(format!("port {port}: {error}"))),
            };
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(Arc::clone(&runtime));
            }
            set(SeedStatus::Listening { torrents: 0 });
            let result = runtime.serve_until_all(&listeners, &shutdown);
            if let Ok(mut slot) = slot.lock() {
                *slot = None;
            }
            match result {
                Ok(()) => set(SeedStatus::Idle),
                Err(error) => set(SeedStatus::Failed(error.to_string())),
            }
        }));
    }

    /// Stops serving and sends `stopped` announces; returns the threads to
    /// wait for.
    fn stop(&mut self) -> Vec<JoinHandle<()>> {
        self.shutdown.store(true, Ordering::Release);
        self.thread
            .take()
            .into_iter()
            .chain(self.announcer.stop())
            .collect()
    }
}

struct Engine {
    client: Client,
    store: Store,
    settings: Settings,
    activity: HashMap<InfoHash, Activity>,
    verify_queue: VecDeque<InfoHash>,
    /// `.torrent` files being downloaded from URLs.
    fetches: Vec<Fetch>,
    /// Transfers of removed torrents, kept until their threads exit.
    detached: Vec<TransferHandle>,
    /// Files of removed torrents to delete once their transfers stop.
    deletions: Vec<Deletion>,
    seeding: SeedService,
    last_file_sweep: Instant,
    dht_state: DhtState,
    config: ConfigFile,
    /// Settings that command-line flags override for this run.
    overridden: Vec<&'static str>,
    last_sample: Instant,
    session_dirty: bool,
    /// After a failed save: when to try again, and what went wrong, so it
    /// is reported once.
    save_retry_at: Instant,
    save_problem: Option<String>,
    notices: VecDeque<Notice>,
    next_notice: u64,
    generation: u64,
    labels: Labels,
    geoip: crate::geoip::GeoIp,
    last_port_check: Instant,
    /// Swarm counts from the latest tracker scrapes.
    scrapes: HashMap<InfoHash, empirebt_backend::ScrapeStats>,
    next_scrape: Instant,
    /// The scrape round running in the background, if one is.
    scraping: Option<JoinHandle<Vec<(InfoHash, empirebt_backend::ScrapeStats)>>>,
    /// The last problem reported with `network.port_file`, so it is
    /// reported once.
    port_file_problem: Option<String>,
    /// A move to a new port waiting for the old sockets to close.
    rebinding: Option<Rebind>,
    /// Downloads stopped to move to a new port, or started while moving,
    /// started once the new runtime is live.
    restart_after_rebind: Vec<Restart>,
    /// What the log last said about seeding and the network interface, so
    /// it speaks only when they change.
    logged_seeding: Option<String>,
    logged_interface_up: Option<bool>,
    /// The chosen network interface came back after being down.
    interface_returned: bool,
}

/// A move of every network service to `port`, begun by [`Engine::rebind`]:
/// waits for the old services to stop, for at most `deadline`.
struct Rebind {
    port: u16,
    notice: String,
    services: Vec<JoinHandle<()>>,
    deadline: Instant,
}

/// A download to start once a move to a new port is done.
struct Restart {
    hash: InfoHash,
    /// Its transfer stopped for the move; the new one starts once this has
    /// stopped writing.
    old: Option<TransferHandle>,
    /// Started by the user during the move (the others restart only if
    /// still downloading).
    requested: bool,
}

/// A `.torrent` URL being downloaded: the URL, where the torrent goes, and
/// the download.
type Fetch = (
    String,
    Placement,
    JoinHandle<Result<TorrentInfo, BackendError>>,
);

/// What `network.port_file` says.
#[derive(Debug, PartialEq, Eq)]
enum PortFile {
    /// No file is set, or it does not exist yet, or it is empty or 0 (as
    /// while a VPN reconnects): keep the current port.
    Nothing,
    Port(u16),
    Problem(String),
}

impl PortFile {
    fn read(config: &ConfigFile) -> Self {
        let path = config.get("network.port_file").unwrap_or_default();
        let path = path.trim();
        if path.is_empty() {
            return Self::Nothing;
        }
        match std::fs::read_to_string(path) {
            Ok(text) => match text.trim() {
                "" | "0" => Self::Nothing,
                port => match port.parse::<u16>() {
                    Ok(port) => Self::Port(port),
                    Err(_) => Self::Problem(format!(
                        "network.port_file ({path}) does not hold a port number"
                    )),
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Nothing,
            Err(error) => Self::Problem(format!("network.port_file ({path}): {error}")),
        }
    }
}

impl Engine {
    fn new(
        client: Client,
        store: Store,
        settings: Settings,
        dht_state: DhtState,
        config: ConfigFile,
        overridden: Vec<&'static str>,
    ) -> Self {
        let port = client.config().listen_port();
        let seeding = SeedService::new(port, &settings, client.dht().cloned());
        let mut engine = Self {
            client,
            store,
            settings,
            activity: HashMap::new(),
            verify_queue: VecDeque::new(),
            fetches: Vec::new(),
            detached: Vec::new(),
            deletions: Vec::new(),
            seeding,
            last_file_sweep: Instant::now(),
            dht_state,
            config,
            overridden,
            last_sample: Instant::now(),
            session_dirty: false,
            save_retry_at: Instant::now(),
            save_problem: None,
            notices: VecDeque::new(),
            next_notice: 0,
            generation: 0,
            labels: Labels::default(),
            geoip: crate::geoip::GeoIp::default(),
            last_port_check: Instant::now(),
            scrapes: HashMap::new(),
            next_scrape: Instant::now() + FIRST_SCRAPE,
            scraping: None,
            port_file_problem: None,
            rebinding: None,
            restart_after_rebind: Vec::new(),
            logged_seeding: None,
            logged_interface_up: None,
            interface_returned: false,
        };
        let saved: Vec<(InfoHash, TransferState)> = engine
            .client
            .torrents()
            .iter()
            .filter_map(|torrent| Some((*torrent.info_hash()?, torrent.state())))
            .collect();
        for (hash, state) in saved {
            match state {
                TransferState::Downloading => engine.start(hash),
                TransferState::Paused => {
                    // Saved progress that still matches the files is shown
                    // as is; only unknown or stale progress is re-checked.
                    let current = engine
                        .client
                        .torrent_index(&hash)
                        .is_some_and(|index| engine.client.resume_is_current(index));
                    if engine.has_metadata(&hash) && !current {
                        engine.queue_verify(hash);
                    }
                }
                TransferState::Seeding | TransferState::Complete => {}
            }
        }
        engine
    }

    /// Reports torrents that could not be restored from the saved session.
    fn report_skipped(&mut self, warnings: &[String]) {
        if let Some(first) = warnings.first() {
            let more = match warnings.len() {
                1 => String::new(),
                count => format!(" (and {} more)", count - 1),
            };
            self.notify(Level::Error, format!("{first}{more}"));
        }
    }

    fn handle(&mut self, command: Command) -> Result<Reply, Failure> {
        match command {
            Command::Add(source) => self.add(&source, Placement::default()),
            Command::AddTorrent(bytes) => self.add_bytes(&bytes, Placement::default()),
            Command::AddWith(request) => {
                let category = request.category.map(|name| clean(&name)).transpose()?;
                let save_dir = match request.save_dir {
                    Some(dir) => Some(save_folder(dir)?),
                    None => category
                        .as_ref()
                        .and_then(|category| self.labels.folders.get(category).cloned()),
                };
                let placement = Placement {
                    save_dir,
                    category,
                    tags: request
                        .tags
                        .iter()
                        .map(|name| clean(name))
                        .collect::<Result<_, _>>()?,
                    start: request.start,
                    layout: request.layout,
                };
                match request.source {
                    AddSource::Link(source) => self.add(&source, placement),
                    AddSource::Torrent(bytes) => self.add_bytes(&bytes, placement),
                }
            }
            Command::SetCategoryFolder { category, folder } => {
                let category = clean(&category)?;
                let folder = folder.map(save_folder).transpose()?;
                if let Some(folder) = &folder {
                    crate::labels::check_folder(folder).map_err(Failure::new)?;
                }
                self.labels.set_folder(category, folder);
                self.save_labels()
            }
            Command::Start(hash) => {
                self.start(hash);
                Ok(Reply::Done)
            }
            Command::Pause(hash) => {
                self.pause(hash);
                Ok(Reply::Done)
            }
            Command::Toggle(hash) => {
                self.toggle(hash);
                Ok(Reply::Done)
            }
            Command::Recheck(hash) => {
                if self.queue_verify(hash) {
                    self.notify(
                        Level::Info,
                        format!("Queued recheck of {}", self.name(&hash)),
                    );
                }
                Ok(Reply::Done)
            }
            Command::Reannounce(hash) => self.reannounce(hash),
            Command::Remove(hash) => {
                self.remove(hash, false);
                Ok(Reply::Done)
            }
            Command::RemoveWithFiles(hash) => {
                self.remove(hash, true);
                Ok(Reply::Done)
            }
            Command::ChangeSetting { key, value } => {
                self.change_setting(&key, &value).map(|()| Reply::Done)
            }
            Command::SetCategory { torrent, category } => {
                self.known(&torrent)?;
                let category = category.map(|name| clean(&name)).transpose()?;
                self.labels.set_category(torrent, category);
                self.save_labels()
            }
            Command::SetTags { torrent, tags } => {
                self.known(&torrent)?;
                let tags = tags
                    .iter()
                    .map(|name| clean(name))
                    .collect::<Result<_, _>>()?;
                self.labels.set_tags(torrent, tags);
                self.save_labels()
            }
            Command::CreateCategory(name) => {
                self.labels.categories.insert(clean(&name)?);
                self.save_labels()
            }
            Command::RemoveCategory(name) => {
                if !self.labels.remove_category(&name) {
                    return Err(Failure::new(format!("there is no category {name}")));
                }
                self.save_labels()
            }
            Command::CreateTag(name) => {
                self.labels.tags.insert(clean(&name)?);
                self.save_labels()
            }
            Command::RemoveTag(name) => {
                if !self.labels.remove_tag(&name) {
                    return Err(Failure::new(format!("there is no tag {name}")));
                }
                self.save_labels()
            }
            Command::RenameTorrent { torrent, name } => {
                let index = self.index(&torrent)?;
                let before = self.name(&torrent);
                self.client
                    .rename_torrent(index, name)
                    .map_err(|error| Failure::new(error.to_string()))?;
                self.session_dirty = true;
                tracing::info!(target: "torrent", "{before} is now shown as {}", self.name(&torrent));
                Ok(Reply::Done)
            }
            Command::RenamePath { torrent, from, to } => {
                let index = self.index(&torrent)?;
                // Writes or checks running meanwhile could recreate or miss
                // the files being moved; seeding only reads, and a read
                // that misses during the move is just retried by the peer.
                match self.activity.get(&torrent).and_then(Activity::job_kind) {
                    Some(TransferKind::Download) => {
                        return Err(Failure::new(format!(
                            "Pause {} before renaming its files",
                            self.name(&torrent)
                        )));
                    }
                    Some(TransferKind::Verify) => {
                        return Err(Failure::new(format!(
                            "{} is being checked; rename its files afterwards",
                            self.name(&torrent)
                        )));
                    }
                    _ => {}
                }
                self.client
                    .rename_path(index, &from, &to)
                    .map_err(|error| Failure::new(error.to_string()))?;
                self.session_dirty = true;
                tracing::info!(
                    target: "torrent",
                    "{}: renamed {} to {}",
                    self.name(&torrent),
                    from.display(),
                    to.display()
                );
                Ok(Reply::Done)
            }
            Command::Peers(torrent) => {
                self.known(&torrent)?;
                // A torrent not in the runtime (paused, or a magnet link
                // without metadata) has no connections.
                let peers = self
                    .client
                    .runtime()
                    .and_then(|runtime| runtime.peers(&torrent))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|info| PeerView {
                        country: self.geoip.lookup(info.address.ip()),
                        info,
                    })
                    .collect();
                Ok(Reply::Peers(peers))
            }
        }
    }

    /// Fails unless the torrent is in the list.
    fn known(&self, hash: &InfoHash) -> Result<(), Failure> {
        self.index(hash).map(drop)
    }

    fn index(&self, hash: &InfoHash) -> Result<usize, Failure> {
        self.client
            .torrent_index(hash)
            .ok_or_else(|| Failure::new("there is no such torrent"))
    }

    fn save_labels(&mut self) -> Result<Reply, Failure> {
        self.labels
            .save(&self.store.labels_path())
            .map(|()| Reply::Done)
            .map_err(Failure::new)
    }

    // ----- snapshots ----------------------------------------------------------

    fn snapshot(&mut self) -> Snapshot {
        self.generation += 1;
        let torrents: Vec<TorrentView> = (0..self.client.torrents().len())
            .filter_map(|index| self.torrent_view(index))
            .collect();
        let limits = self.client.limits();
        let totals = self
            .activity
            .values()
            .fold((0.0, 0.0), |(down, up), activity| {
                (down + activity.down.rate(), up + activity.up.rate())
            });
        let settings = self
            .config
            .settings()
            .into_iter()
            .map(|setting| SettingView {
                setting,
                value: self.config.get(setting.key).unwrap_or_default(),
                overridden: self.overridden.contains(&setting.key),
            })
            .collect();
        Snapshot {
            generation: self.generation,
            seeding_count: torrents
                .iter()
                .filter(|torrent| torrent.status == Status::Seeding)
                .count(),
            active_count: self
                .activity
                .values()
                .filter(|activity| activity.job_kind() == Some(TransferKind::Download))
                .count(),
            torrents,
            totals,
            rate_limits: (limits.download_rate(), limits.upload_rate()),
            network: Network {
                listen_port: self.client.config().listen_port(),
                seed_status: self.seeding.status(),
                seeding_peers: self.seeding.peers(),
                dht: self.dht_state.clone(),
                dht_nodes: self.client.dht().map(|dht| dht.node_count()),
                dht_ipv6_nodes: self.client.dht().map_or(0, |dht| dht.ipv6_node_count()),
                utp: self.client.utp().is_some(),
                lsd: self.client.lsd().is_some(),
                interface: self.settings.binding.name().map(str::to_owned),
                interface_up: self.settings.binding.is_up(),
            },
            download_dir: self.client.config().download_directory().to_owned(),
            allowed_dirs: std::iter::once(self.client.config().download_directory().to_owned())
                .chain(self.config.allowed_dirs())
                .chain(self.labels.folders.values().cloned())
                .collect(),
            category_folders: self.labels.folders.clone(),
            notices: self.notices.iter().cloned().collect(),
            settings,
            config_path: self.config.path().to_owned(),
            categories: self.labels.categories.iter().cloned().collect(),
            tags: self.labels.tags.iter().cloned().collect(),
        }
    }

    fn torrent_view(&self, index: usize) -> Option<TorrentView> {
        let torrent = self.client.torrents().get(index)?;
        let hash = *torrent.info_hash()?;
        let info = torrent.info();
        let activity = self.activity.get(&hash);
        let job = activity.and_then(Activity::job_kind);
        let progress = activity.and_then(|activity| activity.progress.as_deref());
        let seeding = matches!(
            torrent.state(),
            TransferState::Seeding | TransferState::Complete
        );

        // A seed whose files the runtime could not serve.
        let left_out = self.seeding.left_out.get(&hash).filter(|_| job.is_none());
        let status = match (job, progress) {
            _ if activity.is_some_and(|activity| activity.pausing) => Status::Pausing,
            (Some(TransferKind::Verify), _) => Status::Checking,
            (Some(TransferKind::Metadata), Some(progress))
                if progress.phase() == TransferPhase::Announcing =>
            {
                Status::Announcing
            }
            (Some(TransferKind::Metadata), _) => Status::Metadata,
            (Some(TransferKind::Download), Some(progress)) => match progress.phase() {
                TransferPhase::Checking => Status::Checking,
                TransferPhase::Announcing => Status::Announcing,
                TransferPhase::Waiting if progress.last_error().is_some() => Status::Retrying,
                TransferPhase::Waiting => Status::Waiting,
                TransferPhase::Downloading | TransferPhase::Finished | TransferPhase::Metadata => {
                    Status::Downloading
                }
            },
            (Some(TransferKind::Download), None) => Status::Downloading,
            (None, _) if activity.is_some_and(|activity| activity.queued_verify) => Status::Queued,
            (None, _) if self.waiting_for_rebind(&hash) => Status::Waiting,
            (None, _) if activity.is_some_and(|activity| activity.error.is_some()) => Status::Error,
            (None, _) if seeding && left_out.is_some() => Status::Error,
            (None, _) if seeding => Status::Seeding,
            (None, _) => Status::Paused,
        };
        let complete = seeding && job.is_none();
        // Live progress first, then progress saved by the last run.
        let saved = info.zip(torrent.resume());
        let fraction = if complete {
            Some(1.0)
        } else {
            progress
                .filter(|progress| progress.total_bytes() > 0)
                .map(TransferProgress::fraction)
                .or_else(|| {
                    saved.map(|(info, resume)| {
                        resume.verified_bytes(info) as f64 / info.total_length().max(1) as f64
                    })
                })
        };
        let verified_pieces = match info {
            Some(info) if complete => Some(info.piece_count().max(info.v2_piece_count())),
            Some(_) => progress
                .map(TransferProgress::verified_pieces)
                .or_else(|| torrent.resume().map(|resume| resume.verified_pieces())),
            None => None,
        };
        let announce = self.seeding.announce_status(&hash);
        let swarm = match (&announce, progress) {
            (Some(status), _) if seeding => (status.seeders, status.leechers),
            (_, Some(progress)) if job.is_some_and(|job| job != TransferKind::Verify) => {
                progress.swarm()
            }
            _ => (None, None),
        };
        // Announces are fresher; scrapes cover the rest, paused ones too.
        let scraped = self.scrapes.get(&hash);
        let swarm = (
            swarm.0.or(scraped.map(|stats| stats.seeders)),
            swarm.1.or(scraped.map(|stats| stats.leechers)),
        );
        let error = activity
            .and_then(|activity| {
                activity.error.clone().or_else(|| {
                    activity
                        .job
                        .as_ref()
                        .and_then(|job| job.progress().last_error())
                })
            })
            .or_else(|| left_out.filter(|_| seeding).cloned());
        let trackers = match (info, torrent.magnet()) {
            (Some(info), _) => info.tracker_urls(),
            (None, Some(link)) => link.trackers(),
            (None, None) => &[],
        };
        Some(TorrentView {
            hash,
            name: torrent.display_name().to_owned(),
            original_name: torrent.name().to_owned(),
            renames: torrent.renames().table(),
            save_dir: self
                .client
                .save_dir(index)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default(),
            info: info.cloned(),
            trackers: trackers.to_vec(),
            status,
            fraction,
            verified_pieces,
            down_rate: activity.map_or(0.0, |activity| activity.down.rate()),
            up_rate: activity.map_or(0.0, |activity| activity.up.rate()),
            peers: (job == Some(TransferKind::Download))
                .then(|| progress.map(TransferProgress::active_peers))
                .flatten(),
            swarm,
            completed: scraped.map(|stats| stats.downloaded),
            announce: announce.filter(|_| seeding),
            error,
            category: self
                .labels
                .of(&hash)
                .and_then(|assigned| assigned.category.clone()),
            tags: self
                .labels
                .of(&hash)
                .map(|assigned| assigned.tags.iter().cloned().collect())
                .unwrap_or_default(),
        })
    }

    // ----- background work ------------------------------------------------------

    /// Advances background work: collects finished transfers, samples
    /// rates, starts queued checks, and keeps the seeding runtime current.
    fn tick(&mut self) {
        self.collect_fetches();
        self.collect_finished();
        self.detached.retain(|handle| !handle.is_finished());
        self.sample_rates();
        self.start_next_verify();
        // Small torrents check in microseconds; keep the queue moving within
        // a tick instead of starting one batch per tick.
        let budget = Instant::now() + VERIFY_BUDGET;
        while !self.verify_queue.is_empty() && Instant::now() < budget {
            std::thread::sleep(Duration::from_millis(1));
            self.collect_finished();
            self.start_next_verify();
        }
        self.finish_rebind();
        if self.rebinding.is_none() {
            for (hash, reason) in self.seeding.tick(&mut self.client, &self.settings) {
                let name = self.name(&hash);
                self.notify(Level::Error, format!("{name} is not seeding: {reason}"));
            }
        }
        self.delete_removed_files(false);
        self.log_changes();
        if std::mem::take(&mut self.interface_returned) {
            // Most likely a VPN reconnected, perhaps at another address:
            // new sockets, and a new identity, so peers and the DHT cannot
            // link this address to the last one.
            let name = self.settings.binding.name().unwrap_or_default().to_owned();
            self.rebind(
                self.client.config().listen_port(),
                format!("{name} is back; reconnected with a new peer ID and DHT node ID"),
            );
        }
        self.restart_rebound_downloads();
        self.scrape_trackers();
        if self.last_port_check.elapsed() >= PORT_FILE_POLL {
            self.last_port_check = Instant::now();
            self.follow_port_file();
        }
        if self.last_file_sweep.elapsed() >= Duration::from_secs(60) {
            self.last_file_sweep = Instant::now();
            self.client.close_idle_files(IDLE_FILE_CLOSE);
            // Periodic resume checkpoints. Ongoing writes make them stale, in
            // which case the next start simply checks the data in full.
            let running: Vec<&TransferHandle> = self
                .activity
                .values()
                .filter_map(|activity| activity.job.as_ref())
                .filter(|job| job.kind() == TransferKind::Download)
                .collect();
            if !running.is_empty() {
                for job in running {
                    self.client.checkpoint(job);
                }
                self.session_dirty = true;
            }
        }
        if self.session_dirty && Instant::now() >= self.save_retry_at {
            self.save();
        }
    }

    /// Saves the session with interrupted downloads still marked as
    /// downloading so the next run resumes them, then stops every thread.
    fn shutdown(&mut self) {
        tracing::info!(target: "engine", "stopping: saving the session and leaving trackers");
        let detached = std::mem::take(&mut self.detached);
        let jobs: Vec<TransferHandle> = self
            .activity
            .values_mut()
            .filter_map(|activity| activity.job.take())
            .chain(
                std::mem::take(&mut self.restart_after_rebind)
                    .into_iter()
                    .filter_map(|restart| restart.old),
            )
            .collect();
        jobs.iter()
            .chain(&detached)
            .for_each(TransferHandle::cancel);
        let mut services = self.seeding.stop();
        if let Some(rebind) = self.rebinding.take() {
            services.extend(rebind.services);
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline
            && (jobs
                .iter()
                .chain(&detached)
                .any(|handle| !handle.is_finished())
                || services.iter().any(|thread| !thread.is_finished()))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Transfers have stopped writing, so their progress can be saved as
        // resume data that will still match the files next time. States are
        // left as they were, so interrupted downloads resume on next launch.
        for job in &jobs {
            self.client.checkpoint(job);
        }
        // Removed torrents' files go even if a transfer is slow to stop.
        self.delete_removed_files(true);
        self.save();
        // Saves known nodes so the next run joins the DHT quickly.
        if let Some(dht) = self.client.dht() {
            dht.stop();
        }
        tracing::info!(target: "engine", "stopped");
    }

    /// Validates, stores, and saves one setting, explaining when it applies.
    fn change_setting(&mut self, key: &str, input: &str) -> Result<(), Failure> {
        let setting = self
            .config
            .settings()
            .into_iter()
            .find(|setting| setting.key == key)
            .ok_or_else(|| Failure::new(format!("there is no setting {key}")))?;
        self.config
            .set(setting.key, input)
            .and_then(|()| self.config.save())
            .map_err(|error| Failure::new(error.to_string()))?;
        let value = self.config.get(setting.key).unwrap_or_default();
        let overridden = self.overridden.contains(&setting.key);
        if !overridden {
            self.apply_limit(setting.key);
        }
        let follows_file = PortFile::read(&self.config) != PortFile::Nothing;
        match setting.key {
            "network.port" if !overridden && !follows_file => {
                if let Ok(port) = value.parse() {
                    self.change_port(port);
                }
            }
            "network.anonymous" if !overridden => {
                let anonymous = value == "true";
                if anonymous != self.settings.anonymous {
                    self.settings.anonymous = anonymous;
                    // Running torrents take their new peer IDs at once.
                    self.rebind(
                        self.client.config().listen_port(),
                        if anonymous {
                            "Anonymous mode is on: public torrents no longer say they are EmpireBT"
                        } else {
                            "Anonymous mode is off"
                        }
                        .to_owned(),
                    );
                }
            }
            "network.port_file" => {
                self.port_file_problem = None;
                self.follow_port_file();
                // Without a file, network.port decides again.
                if !follows_file && let Ok(config) = self.config.client_config() {
                    self.change_port(config.listen_port());
                }
            }
            _ => {}
        }
        let when = if setting.key == "network.port" && follows_file {
            " — network.port_file decides the port while it names one"
        } else if overridden {
            " — a command-line flag or environment variable overrides it until you restart \
             without it"
        } else if setting.needs_restart {
            " — restart EmpireBT to apply"
        } else {
            ""
        };
        self.notify(
            Level::Success,
            format!("Saved {} = {value}{when}", setting.key),
        );
        Ok(())
    }

    /// Switches to the port `network.port_file` names, when it names a new
    /// one; reports a problem with the file once.
    fn follow_port_file(&mut self) {
        match PortFile::read(&self.config) {
            PortFile::Nothing => {}
            PortFile::Port(port) => {
                self.port_file_problem = None;
                self.change_port(port);
            }
            PortFile::Problem(problem) => {
                if self.port_file_problem.as_ref() != Some(&problem) {
                    self.notify(Level::Error, problem.clone());
                    self.port_file_problem = Some(problem);
                }
            }
        }
    }

    /// Moves everything that listens to `port`: the runtime's TCP
    /// listeners, the DHT's and uTP's UDP socket, and local discovery.
    /// Connections on the old port end. Running downloads stop with their
    /// progress saved and start again once the new runtime is live, so
    /// they announce the new port; seeds rejoin the new runtime, and its
    /// announcer tells trackers and the DHT at once.
    fn change_port(&mut self, port: u16) {
        let current = self
            .rebinding
            .as_ref()
            .map_or(self.client.config().listen_port(), |rebind| rebind.port);
        if port == current {
            return;
        }
        self.rebind(port, format!("Now taking peers on port {port}"));
    }

    /// Starts every network service again on `port`, as
    /// [`Engine::change_port`] describes, with new peer IDs and a new DHT
    /// node ID; `notice` says why. The old services are stopped here; the
    /// new ones start once those have closed their sockets, usually at
    /// once, else in a later tick ([`Engine::finish_rebind`]). A move
    /// already under way just changes where it goes.
    fn rebind(&mut self, port: u16, notice: String) {
        if let Some(rebind) = &mut self.rebinding {
            rebind.port = port;
            rebind.notice = notice;
            return;
        }
        for (hash, activity) in &mut self.activity {
            let networked = matches!(
                activity.job_kind(),
                Some(TransferKind::Download | TransferKind::Metadata)
            );
            if networked
                && !activity.pausing
                && let Some(job) = activity.job.take()
            {
                job.cancel();
                self.restart_after_rebind.push(Restart {
                    hash: *hash,
                    old: Some(job),
                    requested: false,
                });
            }
        }
        self.rebinding = Some(Rebind {
            port,
            notice,
            services: self.seeding.stop(),
            deadline: Instant::now() + SHUTDOWN_GRACE,
        });
        let quick = Instant::now() + REBIND_WAIT;
        while !self.rebind_ready() && Instant::now() < quick {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.finish_rebind();
    }

    /// The old services and the transfers a move stopped have all stopped.
    fn rebind_ready(&self) -> bool {
        self.rebinding.as_ref().is_some_and(|rebind| {
            rebind.services.iter().all(JoinHandle::is_finished)
                && self
                    .restart_after_rebind
                    .iter()
                    .filter_map(|restart| restart.old.as_ref())
                    .all(TransferHandle::is_finished)
        })
    }

    /// Finishes a move to a new port once the old services have stopped,
    /// or have had [`SHUTDOWN_GRACE`] to: starts the DHT, uTP, local
    /// discovery, and a new seeding runtime on it. Transfers still winding
    /// down are waited for by [`Engine::restart_rebound_downloads`].
    fn finish_rebind(&mut self) {
        let Some(deadline) = self.rebinding.as_ref().map(|rebind| rebind.deadline) else {
            return;
        };
        if !self.rebind_ready() && Instant::now() < deadline {
            return;
        }
        let Some(Rebind { port, notice, .. }) = self.rebinding.take() else {
            return;
        };
        self.client.set_runtime(None);
        if let Some(lsd) = self.client.lsd() {
            lsd.stop();
        }
        self.client.set_lsd(None);
        if let Some(utp) = self.client.utp() {
            utp.stop();
        }
        self.client.set_utp(None);
        if let Some(dht) = self.client.dht() {
            dht.stop();
        }
        self.client.set_dht(None);

        tracing::debug!(
            target: "port",
            "moving from port {} to {port}",
            self.client.config().listen_port()
        );
        self.client.set_listen_port(port);
        self.settings.peer_id = empirebt_backend::generate_peer_id();
        self.client.set_anonymous_peer_id(
            self.settings
                .anonymous
                .then(empirebt_backend::generate_anonymous_peer_id),
        );
        let config = self.client.config().clone();
        self.dht_state = start_networking(&mut self.client, &config, &self.settings, &self.store);
        // Seeds already reported as left out are not reported again.
        let left_out = std::mem::take(&mut self.seeding.left_out);
        self.seeding = SeedService::new(port, &self.settings, self.client.dht().cloned());
        self.seeding.left_out = left_out;
        self.session_dirty = true;
        self.notify(Level::Info, notice);
    }

    /// Logs the seeding listener and the network interface when their
    /// state changes.
    fn log_changes(&mut self) {
        let seeding = match self.seeding.status() {
            SeedStatus::Listening { .. } => Some(format!(
                "taking peers on port {}",
                self.client.config().listen_port()
            )),
            SeedStatus::Failed(error) => Some(format!("cannot take peers: {error}")),
            SeedStatus::Idle | SeedStatus::Starting => None,
        };
        if seeding.is_some() && seeding != self.logged_seeding {
            let text = seeding.clone().unwrap_or_default();
            if matches!(self.seeding.status(), SeedStatus::Failed(_)) {
                tracing::error!(target: "seeding", "{text}");
            } else {
                tracing::info!(target: "seeding", "{text}");
            }
            self.logged_seeding = seeding;
        }
        if let Some(name) = self.settings.binding.name() {
            let up = self.settings.binding.is_up();
            if self.logged_interface_up != Some(up) {
                if up {
                    tracing::info!(target: "network", "{name} is up");
                    if self.logged_interface_up == Some(false) {
                        self.interface_returned = true;
                    }
                } else {
                    tracing::warn!(target: "network", "{name} is down; nothing connects until it is back");
                }
                self.logged_interface_up = Some(up);
            }
        }
    }

    /// Starts the downloads [`Engine::change_port`] stopped, once the new
    /// runtime is live (or has failed) and each old transfer has stopped
    /// writing, unless they were paused or removed meanwhile; and those
    /// started during the move.
    fn restart_rebound_downloads(&mut self) {
        if self.restart_after_rebind.is_empty()
            || self.rebinding.is_some()
            || (self.seeding.live().is_none()
                && !matches!(self.seeding.status(), SeedStatus::Failed(_)))
        {
            return;
        }
        let (ready, waiting): (Vec<Restart>, Vec<Restart>) =
            std::mem::take(&mut self.restart_after_rebind)
                .into_iter()
                .partition(|restart| restart.old.as_ref().is_none_or(TransferHandle::is_finished));
        self.restart_after_rebind = waiting;
        for restart in ready {
            // Stopped writing, so its progress matches the files.
            if let Some(old) = &restart.old {
                self.client.checkpoint(old);
                self.session_dirty = true;
            }
            if restart.requested || self.downloading(&restart.hash) {
                self.start(restart.hash);
            }
        }
    }

    /// The torrent is set to download (a restart after a move waits for it).
    fn downloading(&self, hash: &InfoHash) -> bool {
        self.client.torrent_index(hash).is_some_and(|index| {
            self.client.torrents()[index].state() == TransferState::Downloading
        })
    }

    /// Waiting for a move to a new port before it starts.
    fn waiting_for_rebind(&self, hash: &InfoHash) -> bool {
        self.restart_after_rebind
            .iter()
            .any(|restart| restart.hash == *hash && (restart.requested || self.downloading(hash)))
    }

    /// Applies a changed connection or speed limit to the running client.
    fn apply_limit(&self, key: &str) {
        let Ok(config) = self.config.client_config() else {
            return;
        };
        let limits = self.client.limits();
        match key {
            "network.max_connections" => limits.set_max_connections(config.max_connections()),
            "network.upload_limit" => limits.set_upload_rate(config.upload_limit()),
            "network.download_limit" => limits.set_download_rate(config.download_limit()),
            _ => {}
        }
    }

    fn start_on_add(&self) -> bool {
        self.config.get("console.start_on_add").as_deref() != Some("false")
    }

    // ----- actions ------------------------------------------------------------

    /// Adds a magnet link, a `.torrent` file, or a `.torrent` URL and
    /// starts it. A URL downloads in the background and is added when done.
    fn add(&mut self, source: &str, placement: Placement) -> Result<Reply, Failure> {
        let text = source.trim();
        if text.is_empty() {
            return Err(Failure::new(
                "Type a .torrent file path or URL, or paste a magnet link",
            ));
        }
        if TorrentInfo::is_url(text) {
            let url = text.to_owned();
            let fetch = std::thread::spawn({
                let url = url.clone();
                let binding = self.settings.binding.clone();
                move || TorrentInfo::from_url_with(&url, TORRENT_DOWNLOAD_TIMEOUT, &binding)
            });
            self.notify(Level::Info, format!("Downloading {url}"));
            self.fetches.push((url, placement, fetch));
            return Ok(Reply::Fetching);
        }
        if text.starts_with("magnet:") {
            let link = MagnetLink::parse(text).map_err(|error| Failure::new(error.to_string()))?;
            let hash = *link.info_hash();
            if self.client.torrent_index(&hash).is_some() {
                return Err(Failure {
                    message: "this torrent is already in your list".to_owned(),
                    torrent: Some(hash),
                });
            }
            let index = self.client.add_magnet(link);
            let start = self.place(index, hash, placement)?;
            let name = self.name(&hash);
            if start {
                self.notify(Level::Success, format!("Added {name} — fetching metadata"));
                self.start(hash);
            } else {
                self.notify(
                    Level::Success,
                    format!("Added {name} (paused; press s to start)"),
                );
            }
            return Ok(Reply::Added(hash));
        }
        let path = expand_home(text);
        let info = TorrentInfo::from_file(&path).map_err(|error| {
            Failure::new(match error {
                BackendError::Io(message) => format!("Cannot read {}: {message}", path.display()),
                other => other.to_string(),
            })
        })?;
        self.add_info(info, placement)
    }

    /// Adds a `.torrent` file from its contents.
    fn add_bytes(&mut self, bytes: &[u8], placement: Placement) -> Result<Reply, Failure> {
        let info =
            TorrentInfo::from_bytes(bytes).map_err(|error| Failure::new(error.to_string()))?;
        self.add_info(info, placement)
    }

    /// Adds a torrent whose metadata is known and starts it.
    fn add_info(&mut self, info: TorrentInfo, placement: Placement) -> Result<Reply, Failure> {
        let hash = *info.info_hash();
        if self.client.torrent_index(&hash).is_some() {
            return Err(Failure {
                message: format!("{} is already in your list", info.name()),
                torrent: Some(hash),
            });
        }
        let name = info.name().to_owned();
        let index = self.client.add_torrent(info);
        if self.place(index, hash, placement)? {
            self.notify(Level::Success, format!("Added {name}"));
            self.start(hash);
        } else {
            // Still check for data already on disk, so progress is known.
            self.notify(
                Level::Success,
                format!("Added {name} (paused; press s to start)"),
            );
            self.queue_verify(hash);
        }
        Ok(Reply::Added(hash))
    }

    /// Puts a just-added torrent where `placement` says, with its labels;
    /// returns whether to start it.
    fn place(
        &mut self,
        index: usize,
        hash: InfoHash,
        placement: Placement,
    ) -> Result<bool, Failure> {
        if let Some(dir) = placement.save_dir {
            self.client
                .set_save_dir(index, dir)
                .map_err(|error| Failure::new(error.to_string()))?;
        }
        // A magnet link gets it once its metadata arrives.
        if placement.layout != ContentLayout::Original {
            self.client
                .set_content_layout(index, placement.layout)
                .map_err(|error| Failure::new(error.to_string()))?;
        }
        self.session_dirty = true;
        if placement.category.is_some() || !placement.tags.is_empty() {
            if placement.category.is_some() {
                self.labels.set_category(hash, placement.category);
            }
            if !placement.tags.is_empty() {
                self.labels.set_tags(hash, placement.tags);
            }
            if let Err(error) = self.labels.save(&self.store.labels_path()) {
                self.notify(Level::Error, error);
            }
        }
        Ok(placement.start.unwrap_or_else(|| self.start_on_add()))
    }

    fn start(&mut self, hash: InfoHash) {
        let Some(index) = self.client.torrent_index(&hash) else {
            return;
        };
        let name = self.name(&hash);
        let activity = self.activity.entry(hash).or_default();
        match activity.job_kind() {
            Some(TransferKind::Download | TransferKind::Metadata) => {
                if activity.pausing {
                    activity.start_after_verify = true;
                }
                return;
            }
            Some(TransferKind::Verify) => {
                activity.start_after_verify = true;
                activity.pausing = false;
                activity.job.as_ref().map(TransferHandle::cancel);
                return;
            }
            None => {}
        }
        if matches!(
            self.client.torrents()[index].state(),
            TransferState::Seeding | TransferState::Complete
        ) {
            self.notify(
                Level::Info,
                format!("{name} is already complete and seeding"),
            );
            return;
        }
        activity.queued_verify = false;
        activity.error = None;
        activity.down.idle();
        if self.rebinding.is_some() {
            // The old runtime is closing: start on the new one.
            match self
                .restart_after_rebind
                .iter_mut()
                .find(|restart| restart.hash == hash)
            {
                Some(restart) => restart.requested = true,
                None => self.restart_after_rebind.push(Restart {
                    hash,
                    old: None,
                    requested: true,
                }),
            }
            return;
        }
        let (peer_id, timeout) = (self.settings.peer_id, self.settings.peer_timeout);
        let spawned = if self.client.torrents()[index].info().is_some() {
            self.client.spawn_download(index, peer_id, timeout)
        } else {
            self.client.spawn_metadata(index, peer_id, timeout)
        };
        match spawned {
            Ok(handle) => {
                activity.progress = Some(Arc::clone(handle.progress()));
                activity.job = Some(handle);
                self.session_dirty = true;
            }
            Err(error) => {
                activity.error = Some(error.to_string());
                self.notify(Level::Error, format!("Cannot start {name}: {error}"));
            }
        }
    }

    fn pause(&mut self, hash: InfoHash) {
        let Some(index) = self.client.torrent_index(&hash) else {
            return;
        };
        let name = self.name(&hash);
        for restart in &mut self.restart_after_rebind {
            if restart.hash == hash {
                restart.requested = false;
            }
        }
        let activity = self.activity.entry(hash).or_default();
        activity.start_after_verify = false;
        if let Some(job) = &activity.job {
            // A cancelled check leaves the torrent as it was, so a seed
            // being checked is paused once the check has stopped.
            job.cancel();
            activity.pausing = true;
            return;
        }
        activity.queued_verify = false;
        let state = self.client.torrents()[index].state();
        if state == TransferState::Paused {
            return;
        }
        if self.client.pause_torrent(index).is_ok() {
            self.seeding.resync = true;
            self.session_dirty = true;
            self.notify(Level::Info, format!("Paused {name}"));
        }
    }

    fn toggle(&mut self, hash: InfoHash) {
        let running = self
            .activity
            .get(&hash)
            .is_some_and(|activity| activity.job_kind() == Some(TransferKind::Download));
        let seeding = self.client.torrent_index(&hash).is_some_and(|index| {
            matches!(
                self.client.torrents()[index].state(),
                TransferState::Seeding | TransferState::Complete
            )
        });
        if running || seeding {
            self.pause(hash);
        } else {
            self.start(hash);
        }
    }

    /// Queues a local data check. Returns false when the torrent is busy.
    fn queue_verify(&mut self, hash: InfoHash) -> bool {
        if !self.has_metadata(&hash) {
            return false;
        }
        let activity = self.activity.entry(hash).or_default();
        if activity.job.is_some() || activity.queued_verify {
            return false;
        }
        activity.queued_verify = true;
        activity.error = None;
        self.verify_queue.push_back(hash);
        true
    }

    /// Announces now: a download or metadata fetch through its own
    /// transfer, a seed through the seeding announcer.
    fn reannounce(&mut self, hash: InfoHash) -> Result<Reply, Failure> {
        self.known(&hash)?;
        let name = self.name(&hash);
        let transfer = self
            .activity
            .get(&hash)
            .and_then(|activity| activity.job.as_ref())
            .filter(|job| job.kind() != TransferKind::Verify);
        if let Some(job) = transfer {
            job.progress().request_reannounce();
        } else if !self.seeding.announcer.reannounce(&hash) {
            // A notice rather than a failure, so every interface shows it.
            self.notify(
                Level::Info,
                format!("{name} is not running; start it to announce it"),
            );
            return Ok(Reply::Done);
        }
        self.notify(Level::Info, format!("Reannouncing {name}"));
        Ok(Reply::Done)
    }

    /// Takes in a finished scrape round, and starts the next when due:
    /// every torrent with trackers, through the first of them that can be
    /// scraped, many torrents per request (BEP 48, BEP 15), in the
    /// background.
    fn scrape_trackers(&mut self) {
        if self.scraping.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(round) = self.scraping.take()
            && let Ok(results) = round.join()
        {
            self.scrapes.extend(results);
        }
        if self.scraping.is_some() || Instant::now() < self.next_scrape {
            return;
        }
        self.next_scrape = Instant::now() + SCRAPE_INTERVAL;
        // Torrents by the tracker that answers for them and the
        // User-Agent to say: anonymous mode hides it for public ones.
        let mut groups: HashMap<(String, &'static str), Vec<ScrapedAs>> = HashMap::new();
        for torrent in self.client.torrents() {
            let Some(hash) = torrent.info_hash().copied() else {
                continue;
            };
            let (urls, scraped_as, private): (Vec<String>, [u8; 20], bool) =
                match (torrent.info(), torrent.magnet()) {
                    (Some(info), _) => (
                        info.tracker_tiers().iter().flatten().cloned().collect(),
                        info.swarm_hashes()[0],
                        info.is_private(),
                    ),
                    (None, Some(link)) => (link.trackers().to_vec(), hash, false),
                    (None, None) => continue,
                };
            let Some(url) = urls.into_iter().find(|url| {
                empirebt_backend::Tracker::new(url.as_str())
                    .is_ok_and(|tracker| tracker.supports_scrape())
            }) else {
                continue;
            };
            let user_agent = if self.settings.anonymous && !private {
                empirebt_backend::ANONYMOUS_USER_AGENT
            } else {
                empirebt_backend::USER_AGENT
            };
            groups
                .entry((url, user_agent))
                .or_default()
                .push((hash, scraped_as));
        }
        if groups.is_empty() {
            return;
        }
        let batches: Vec<ScrapeBatch> = groups
            .into_iter()
            .flat_map(|((url, user_agent), torrents)| {
                torrents
                    .chunks(empirebt_backend::MAX_SCRAPE_HASHES)
                    .map(|chunk| (url.clone(), user_agent, chunk.to_vec()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let binding = self.settings.binding.clone();
        self.scraping = Some(std::thread::spawn(move || {
            let results = Mutex::new(Vec::new());
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..SCRAPE_WORKERS.min(batches.len()) {
                    scope.spawn(|| {
                        while let Some((url, user_agent, torrents)) =
                            batches.get(next.fetch_add(1, Ordering::Relaxed))
                        {
                            let Ok(client) =
                                empirebt_backend::TrackerClient::from_urls([url.clone()])
                            else {
                                continue;
                            };
                            let client = client.with_binding(&binding);
                            let hashes: Vec<[u8; 20]> =
                                torrents.iter().map(|(_, scraped_as)| *scraped_as).collect();
                            let Ok(stats) =
                                client.trackers()[0].scrape(&hashes, user_agent, SCRAPE_TIMEOUT)
                            else {
                                continue;
                            };
                            let found = torrents.iter().filter_map(|(hash, scraped_as)| {
                                stats.get(scraped_as).map(|stats| (*hash, *stats))
                            });
                            lock(&results).extend(found);
                        }
                    });
                }
            });
            results.into_inner().unwrap_or_default()
        }));
    }

    /// Removes a torrent; with `delete_files`, its files are deleted once
    /// its transfer has stopped writing (see
    /// [`Engine::delete_removed_files`]).
    fn remove(&mut self, hash: InfoHash, delete_files: bool) {
        let Some(index) = self.client.torrent_index(&hash) else {
            return;
        };
        let (root, paths) = self.client.file_paths(index);
        let mut job = self
            .activity
            .remove(&hash)
            .and_then(|activity| activity.job);
        if let Some(position) = self
            .restart_after_rebind
            .iter()
            .position(|restart| restart.hash == hash)
        {
            job = job.or(self.restart_after_rebind.remove(position).old);
        }
        if let Some(job) = &job {
            job.cancel();
        }
        self.verify_queue.retain(|queued| *queued != hash);
        self.scrapes.remove(&hash);
        let Ok(torrent) = self.client.remove_torrent(index) else {
            self.detached.extend(job);
            return;
        };
        if self.labels.forget(&hash) {
            let _ = self.save_labels();
        }
        self.seeding.resync = true;
        self.session_dirty = true;
        let name = torrent.display_name().to_owned();
        if delete_files && !paths.is_empty() {
            self.notify(Level::Info, format!("Removed {name}; deleting its files"));
            self.deletions.push(Deletion {
                name,
                root,
                paths,
                job,
            });
        } else {
            self.detached.extend(job);
            let kept = if paths.is_empty() {
                String::new()
            } else {
                format!(" — its files are still in {}", root.display())
            };
            self.notify(Level::Info, format!("Removed {name}{kept}"));
        }
    }

    /// Deletes the files of removed torrents whose transfers have stopped
    /// (all of them, with `now`), after the seeding runtime let go of them.
    /// A file another torrent in the list uses is kept.
    fn delete_removed_files(&mut self, now: bool) {
        if self.deletions.is_empty() {
            return;
        }
        let (ready, waiting): (Vec<Deletion>, Vec<Deletion>) = std::mem::take(&mut self.deletions)
            .into_iter()
            .partition(|deletion| {
                now || deletion
                    .job
                    .as_ref()
                    .is_none_or(TransferHandle::is_finished)
            });
        self.deletions = waiting;
        if ready.is_empty() {
            return;
        }
        let in_use: std::collections::HashSet<PathBuf> = (0..self.client.torrents().len())
            .flat_map(|index| self.client.file_paths(index).1)
            .collect();
        for deletion in ready {
            let listed = deletion.paths.len();
            let paths: Vec<PathBuf> = deletion
                .paths
                .into_iter()
                .filter(|path| !in_use.contains(path))
                .collect();
            let kept = listed - paths.len();
            self.client.release_files(&paths);
            let deleted = empirebt_backend::delete_files(&deletion.root, &paths);
            // A read that raced the deletion may have opened one again.
            self.client.release_files(&paths);
            tracing::info!(
                target: "torrent",
                "deleted {} files ({} bytes) of {}",
                deleted.files,
                deleted.bytes,
                deletion.name
            );
            let shared = match kept {
                0 => String::new(),
                1 => "; kept 1 file another torrent in the list uses".to_owned(),
                count => format!("; kept {count} files another torrent in the list uses"),
            };
            match deleted.failures.first() {
                None if deleted.files == 0 && kept > 0 => self.notify(
                    Level::Info,
                    format!(
                        "Kept the files of {}: another torrent in the list uses them",
                        deletion.name
                    ),
                ),
                None if deleted.files == 0 => self.notify(
                    Level::Info,
                    format!("{} had no files on disk to delete", deletion.name),
                ),
                None => self.notify(
                    Level::Success,
                    format!(
                        "Deleted the files of {} ({}){shared}",
                        deletion.name,
                        human_bytes(deleted.bytes)
                    ),
                ),
                Some(first) => {
                    let more = match deleted.failures.len() {
                        1 => String::new(),
                        count => format!(" (and {} more)", count - 1),
                    };
                    self.notify(
                        Level::Error,
                        format!(
                            "Could not delete every file of {}: {first}{more}",
                            deletion.name
                        ),
                    );
                }
            }
        }
    }

    // ----- background bookkeeping ------------------------------------------

    /// Adds torrents whose `.torrent` download finished.
    fn collect_fetches(&mut self) {
        let (done, pending) = std::mem::take(&mut self.fetches)
            .into_iter()
            .partition(|(_, _, fetch)| fetch.is_finished());
        self.fetches = pending;
        for (url, placement, fetch) in done {
            let result = match fetch.join() {
                Ok(Ok(info)) => self.add_info(info, placement),
                Ok(Err(error)) => Err(Failure::new(error.to_string())),
                Err(_) => Err(Failure::new(format!("downloading {url} failed"))),
            };
            if let Err(failure) = result {
                self.notify(Level::Error, failure.message);
            }
        }
    }

    fn collect_finished(&mut self) {
        let finished: Vec<InfoHash> = self
            .activity
            .iter()
            .filter(|(_, activity)| {
                activity
                    .job
                    .as_ref()
                    .is_some_and(TransferHandle::is_finished)
            })
            .map(|(hash, _)| *hash)
            .collect();
        for hash in finished {
            let name = self.name(&hash);
            let Some(activity) = self.activity.get_mut(&hash) else {
                continue;
            };
            let Some(handle) = activity.job.take() else {
                continue;
            };
            let kind = handle.kind();
            let was_seeding = self.client.torrent_index(&hash).is_some_and(|index| {
                self.client.torrents()[index].state() == TransferState::Seeding
            });
            let index = self.client.torrent_index(&hash);
            let result = self.client.finish_transfer(handle);
            // A v2-only magnet is known by its truncated v2 hash until its
            // metadata arrives, then by the torrent's own hash. Follow it.
            let hash = match index.and_then(|index| self.client.torrents()[index].info_hash()) {
                Some(new_hash) if *new_hash != hash => {
                    let new_hash = *new_hash;
                    if let Some(activity) = self.activity.remove(&hash) {
                        self.activity.insert(new_hash, activity);
                    }
                    if self.labels.rename(&hash, new_hash) {
                        let _ = self.save_labels();
                    }
                    new_hash
                }
                _ => hash,
            };
            let now_seeding = self.client.torrent_index(&hash).is_some_and(|index| {
                self.client.torrents()[index].state() == TransferState::Seeding
            });
            let Some(activity) = self.activity.get_mut(&hash) else {
                continue;
            };
            let was_pausing = std::mem::take(&mut activity.pausing);
            activity.down.idle();
            let mut start_next = std::mem::take(&mut activity.start_after_verify);
            let notice = match (kind, result) {
                (_, Err(BackendError::TorrentNotFound(_))) => None,
                (TransferKind::Metadata, Ok(_)) => {
                    // The magnet is now a full torrent; download it.
                    start_next = true;
                    Some((
                        Level::Success,
                        format!("Got metadata for {} — downloading", self.name(&hash)),
                    ))
                }
                (TransferKind::Metadata, Err(BackendError::Cancelled)) => {
                    Some((Level::Info, format!("Paused {name}")))
                }
                (TransferKind::Download, Ok(0)) => {
                    Some((Level::Success, format!("{name} is complete — now seeding")))
                }
                (TransferKind::Download, Ok(_)) => {
                    Some((Level::Success, format!("{name} finished — now seeding")))
                }
                (TransferKind::Download, Err(BackendError::Cancelled)) => {
                    Some((Level::Info, format!("Paused {name}")))
                }
                (TransferKind::Verify, Ok(_) | Err(BackendError::Cancelled)) => None,
                (_, Err(error)) => {
                    if let Some(activity) = self.activity.get_mut(&hash) {
                        activity.error = Some(error.to_string());
                    }
                    Some((Level::Error, format!("{name}: {error}")))
                }
            };
            if let Some((level, text)) = notice {
                self.notify(level, text);
            }
            self.seeding.resync |= was_seeding != now_seeding;
            self.session_dirty = true;
            if start_next {
                self.start(hash);
            } else if kind == TransferKind::Verify && was_pausing {
                self.pause(hash);
            }
        }
    }

    /// Starts queued local-data checks, a few at a time.
    fn start_next_verify(&mut self) {
        let mut verifying = self
            .activity
            .values()
            .filter(|activity| activity.job_kind() == Some(TransferKind::Verify))
            .count();
        while verifying < VERIFY_CONCURRENCY
            && let Some(hash) = self.verify_queue.pop_front()
        {
            let Some(activity) = self.activity.get_mut(&hash) else {
                continue;
            };
            if !activity.queued_verify || activity.job.is_some() {
                continue;
            }
            activity.queued_verify = false;
            let Some(index) = self.client.torrent_index(&hash) else {
                continue;
            };
            match self.client.spawn_verify(index) {
                Ok(handle) => {
                    activity.progress = Some(Arc::clone(handle.progress()));
                    activity.job = Some(handle);
                    verifying += 1;
                }
                Err(error) => activity.error = Some(error.to_string()),
            }
        }
    }

    fn sample_rates(&mut self) {
        let elapsed = self.last_sample.elapsed().as_secs_f64();
        self.last_sample = Instant::now();
        let uploaded = self.seeding.uploaded();
        for (hash, activity) in &mut self.activity {
            match &activity.job {
                Some(job) if job.kind() == TransferKind::Download => activity
                    .down
                    .sample(job.progress().received_bytes(), elapsed),
                _ => activity.down.idle(),
            }
            // A downloading torrent uploads through its own connections.
            let up = match &activity.job {
                Some(job) if job.kind() == TransferKind::Download => {
                    Some(job.progress().uploaded_bytes())
                }
                _ => uploaded.get(hash).copied(),
            };
            match up {
                Some(bytes) => activity.up.sample(bytes, elapsed),
                None => activity.up.idle(),
            }
        }
        // A torrent removed this tick may still be in the runtime until
        // it next syncs.
        for hash in uploaded.keys() {
            if self.client.torrent_index(hash).is_some() {
                self.activity.entry(*hash).or_default();
            }
        }
    }

    // ----- helpers ------------------------------------------------------------

    /// Saves the session. A failure keeps it unsaved, to be tried again
    /// a little later, and is reported once until a save works again.
    fn save(&mut self) {
        match self.store.save(&self.client) {
            Ok(()) => {
                self.session_dirty = false;
                if self.save_problem.take().is_some() {
                    self.notify(Level::Success, "Saved the session again");
                }
            }
            Err(error) => {
                self.save_retry_at = Instant::now() + SAVE_RETRY;
                if self.save_problem.as_ref() != Some(&error) {
                    self.notify(
                        Level::Error,
                        format!("{error}; trying again every {} s", SAVE_RETRY.as_secs()),
                    );
                    self.save_problem = Some(error);
                }
            }
        }
    }

    fn notify(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        // What the interfaces tell the user goes in the log too.
        match level {
            Level::Error => tracing::error!(target: "torrent", "{text}"),
            Level::Info | Level::Success => tracing::info!(target: "torrent", "{text}"),
        }
        self.next_notice += 1;
        self.notices.push_back(Notice {
            id: self.next_notice,
            level,
            text,
            at: Instant::now(),
        });
        while self.notices.len() > NOTICES_KEPT {
            self.notices.pop_front();
        }
    }

    fn has_metadata(&self, hash: &InfoHash) -> bool {
        self.client
            .torrent_index(hash)
            .is_some_and(|index| self.client.torrents()[index].info().is_some())
    }

    fn name(&self, hash: &InfoHash) -> String {
        self.client
            .torrent_index(hash)
            .map(|index| self.client.torrents()[index].display_name().to_owned())
            .unwrap_or_default()
    }
}

/// A category or tag name, checked.
fn clean(name: &str) -> Result<String, Failure> {
    clean_name(name).map_err(|error| Failure::new(format!("{name:?}: {error}")))
}

/// A typed path with a leading `~` expanded.
/// Checks a folder to save a torrent in: a full path, `~` expanded, whose
/// name is valid text.
fn save_folder(dir: PathBuf) -> Result<PathBuf, Failure> {
    let dir = match dir.to_str() {
        Some(text) => expand_home(text.trim()),
        None => return Err(Failure::new("the save folder's name must be valid text")),
    };
    if !dir.is_absolute() {
        return Err(Failure::new(format!(
            "the save folder must be a full path, such as /media/torrents, not {}",
            dir.display()
        )));
    }
    Ok(dir)
}

fn expand_home(text: &str) -> std::path::PathBuf {
    use std::path::is_separator;
    if let Some(rest) = text.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with(is_separator))
        && let Some(home) = crate::home_dir()
    {
        return home.join(rest.trim_start_matches(is_separator));
    }
    std::path::PathBuf::from(text)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine over a fresh data directory, offline, adding torrents
    /// paused.
    fn engine(name: &str) -> (Handle, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("empirebt-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (start_in(&root), root)
    }

    fn start_in(root: &std::path::Path) -> Handle {
        let store = Store::open(root.join("data")).unwrap();
        let mut config = store.config().unwrap();
        config.set("console.start_on_add", "false").unwrap();
        let client_config = config
            .client_config()
            .unwrap()
            .with_listen_port(0)
            .with_dht(false)
            .with_lsd(false)
            .with_download_directory(root.join("downloads"));
        Handle::start(store, config, client_config, Vec::new()).unwrap()
    }

    /// A port nothing listens on right now.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Whether a seed answers a plain BitTorrent handshake for `hash` on
    /// `port`.
    fn answers_handshake(port: u16, hash: &InfoHash) -> bool {
        use std::io::{Read, Write};
        let Ok(mut stream) = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(2),
        ) else {
            return false;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut handshake = vec![19];
        handshake.extend(b"BitTorrent protocol");
        handshake.extend([0; 8]);
        handshake.extend(hash);
        handshake.extend([b'x'; 20]);
        let mut reply = [0; 68];
        stream.write_all(&handshake).is_ok()
            && stream.read_exact(&mut reply).is_ok()
            && reply[28..48] == hash[..]
    }

    /// Waits up to `seconds` for `done` to hold for the latest snapshot.
    fn wait_for(handle: &Handle, seconds: u64, done: impl Fn(&Snapshot) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if done(&handle.snapshot()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// A VPN's forwarded port arrives in `network.port_file` (as gluetun
    /// writes it): everything moves to it while running, seeds included,
    /// and the old port stops answering. A file that holds no port is
    /// reported, and the port stays.
    #[test]
    fn moves_to_the_port_a_port_file_names_while_running() {
        use sha1::Digest;
        let root =
            std::env::temp_dir().join(format!("empirebt-app-port-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let content = b"hello from a forwarded port";
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        std::fs::write(root.join("downloads/hello.txt"), content).unwrap();
        let mut torrent = format!(
            "d4:infod6:lengthi{}e4:name9:hello.txt12:piece lengthi16384e6:pieces20:",
            content.len()
        )
        .into_bytes();
        torrent.extend(sha1::Sha1::digest(content));
        torrent.extend(b"ee");

        let (first, second) = (free_port(), free_port());
        let port_file = root.join("forwarded_port");
        let store = Store::open(root.join("data")).unwrap();
        let mut config = store.config().unwrap();
        config.set("console.start_on_add", "false").unwrap();
        config
            .set("network.port_file", &port_file.to_string_lossy())
            .unwrap();
        let client_config = config
            .client_config()
            .unwrap()
            .with_listen_port(first)
            .with_dht(false)
            .with_lsd(false)
            .with_download_directory(root.join("downloads"));
        let handle = Handle::start(store, config, client_config, Vec::new()).unwrap();

        let Ok(Reply::Added(hash)) = handle.call(Command::AddTorrent(torrent)) else {
            panic!("not added");
        };
        handle.call(Command::Start(hash)).unwrap();
        let seeding = |snapshot: &Snapshot| {
            snapshot.torrent(&hash).unwrap().status == Status::Seeding
                && matches!(
                    snapshot.network.seed_status,
                    SeedStatus::Listening { torrents: 1 }
                )
        };
        if !wait_for(&handle, 20, seeding) {
            let snapshot = handle.snapshot();
            let torrent = snapshot.torrent(&hash).unwrap();
            panic!(
                "never seeded: {:?} {:?} {:?}; notices {:?}",
                torrent.status,
                torrent.error,
                snapshot.network.seed_status,
                snapshot
                    .notices
                    .iter()
                    .map(|notice| &notice.text)
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(handle.snapshot().network.listen_port, first);
        assert!(answers_handshake(first, &hash));

        std::fs::write(&port_file, format!("{second}\n")).unwrap();
        assert!(
            wait_for(&handle, 20, |snapshot| snapshot.network.listen_port
                == second
                && seeding(snapshot)),
            "never moved to port {second}"
        );
        assert!(answers_handshake(second, &hash));
        assert!(!answers_handshake(first, &hash));

        std::fs::write(&port_file, "not a port").unwrap();
        assert!(wait_for(&handle, 20, |snapshot| {
            snapshot
                .notices
                .iter()
                .any(|notice| notice.text.contains("does not hold a port number"))
        }));
        assert_eq!(handle.snapshot().network.listen_port, second);
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// Changing `network.port` applies at once; transfers it interrupts
    /// start again by themselves on the new port.
    #[test]
    fn changes_port_live_and_restarts_interrupted_transfers() {
        let (handle, root) = engine("port-live");
        let (link, hash) = magnet(9);
        // A tracker that never answers keeps the fetch going.
        let link = format!(
            "{link}&tr=http%3A%2F%2F127.0.0.1%3A{}%2Fannounce",
            free_port()
        );
        handle.call(Command::Add(link)).unwrap();
        handle.call(Command::Start(hash)).unwrap();
        assert_eq!(
            handle.snapshot().torrent(&hash).unwrap().status,
            Status::Metadata
        );
        let port = free_port();
        handle
            .call(Command::ChangeSetting {
                key: "network.port".to_owned(),
                value: port.to_string(),
            })
            .unwrap();
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.network.listen_port, port);
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.text.contains(&format!("port {port}")))
        );
        assert!(
            wait_for(&handle, 20, |snapshot| matches!(
                snapshot.network.seed_status,
                SeedStatus::Listening { .. }
            ) && snapshot
                .torrent(&hash)
                .unwrap()
                .status
                == Status::Metadata),
            "the metadata fetch did not start again"
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A torrent added with a folder keeps it across restarts; a folder
    /// that is not a full path is refused before anything is added.
    #[test]
    fn adds_torrents_to_chosen_folders() {
        let (handle, root) = engine("folders");
        let (link, hash) = magnet(11);
        let add = |save_dir: &str| {
            handle.call(Command::AddWith(AddRequest {
                save_dir: Some(save_dir.into()),
                ..AddRequest::new(AddSource::Link(link.clone()))
            }))
        };
        let refused = add("relative/folder").unwrap_err();
        assert!(refused.message.contains("full path"), "{}", refused.message);
        assert!(handle.snapshot().torrents.is_empty());

        let folder = root.join("films");
        assert_eq!(add(&folder.to_string_lossy()), Ok(Reply::Added(hash)));
        assert_eq!(handle.snapshot().torrent(&hash).unwrap().save_dir, folder);
        handle.shutdown();
        let handle = start_in(&root);
        assert_eq!(handle.snapshot().torrent(&hash).unwrap().save_dir, folder);
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A category's folder is where its new torrents go unless one is
    /// chosen; labels and whether to start come with the request.
    #[test]
    fn adds_torrents_with_category_folders_and_labels() {
        let (handle, root) = engine("category-folders");
        let films = root.join("films");
        assert!(
            handle
                .call(Command::SetCategoryFolder {
                    category: "Films".into(),
                    folder: Some("relative".into()),
                })
                .is_err()
        );
        handle
            .call(Command::SetCategoryFolder {
                category: "Films".into(),
                folder: Some(films.clone()),
            })
            .unwrap();
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.category_folders.get("Films"), Some(&films));
        assert!(snapshot.allowed_dirs.contains(&films));

        let (first, first_hash) = magnet(21);
        handle
            .call(Command::AddWith(AddRequest {
                category: Some("Films".into()),
                tags: vec!["hd".into(), " new ".into()],
                start: Some(false),
                ..AddRequest::new(AddSource::Link(first))
            }))
            .unwrap();
        let (second, second_hash) = magnet(22);
        let elsewhere = root.join("elsewhere");
        handle
            .call(Command::AddWith(AddRequest {
                category: Some("Films".into()),
                save_dir: Some(elsewhere.clone()),
                ..AddRequest::new(AddSource::Link(second))
            }))
            .unwrap();
        let snapshot = handle.snapshot();
        let first = snapshot.torrent(&first_hash).unwrap();
        assert_eq!(first.save_dir, films);
        assert_eq!(first.category.as_deref(), Some("Films"));
        assert_eq!(first.tags, ["hd", "new"]);
        assert_eq!(first.status, Status::Paused);
        assert_eq!(snapshot.torrent(&second_hash).unwrap().save_dir, elsewhere);

        // Labels and folders survive a restart.
        handle.shutdown();
        let handle = start_in(&root);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.category_folders.get("Films"), Some(&films));
        assert_eq!(snapshot.torrent(&first_hash).unwrap().tags, ["hd", "new"]);
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    fn magnet(byte: u8) -> (String, InfoHash) {
        let hash = [byte; 20];
        let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        (format!("magnet:?xt=urn:btih:{hex}&dn=test-{byte}"), hash)
    }

    #[test]
    fn commands_take_effect_before_they_return() {
        let (handle, root) = engine("commands");
        let (link, hash) = magnet(1);
        assert_eq!(
            handle.call(Command::Add(link.clone())),
            Ok(Reply::Added(hash))
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.torrents.len(), 1);
        assert_eq!(snapshot.torrents[0].name, "test-1");
        assert_eq!(snapshot.torrents[0].status, Status::Paused);
        assert!(
            snapshot
                .notices
                .last()
                .unwrap()
                .text
                .contains("Added test-1")
        );

        let duplicate = handle.call(Command::Add(link)).unwrap_err();
        assert_eq!(duplicate.torrent, Some(hash));
        assert!(duplicate.message.contains("already in your list"));
        assert!(handle.call(Command::Add("  ".to_owned())).is_err());

        handle.call(Command::Remove(hash)).unwrap();
        let snapshot = handle.snapshot();
        assert!(snapshot.torrents.is_empty());
        assert!(
            snapshot
                .notices
                .last()
                .unwrap()
                .text
                .starts_with("Removed test-1")
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn adds_uploaded_torrent_files() {
        let (handle, root) = engine("upload");
        let mut torrent =
            b"d4:infod6:lengthi5e4:name9:hello.txt12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend([7; 20]);
        torrent.extend(b"ee");
        let Ok(Reply::Added(hash)) = handle.call(Command::AddTorrent(torrent)) else {
            panic!("not added");
        };
        assert_eq!(handle.snapshot().torrent(&hash).unwrap().name, "hello.txt");
        assert!(
            handle
                .call(Command::AddTorrent(b"not a torrent".to_vec()))
                .is_err()
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn renames_torrents_and_their_files_and_lays_out_new_ones() {
        let (handle, root) = engine("renames");
        let save = root.join("saved");
        let mut torrent = b"d4:infod5:filesld6:lengthi3e4:pathl5:a.txteed6:lengthi4e4:pathl5:b.txteee4:name5:album12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend([7; 20]);
        torrent.extend(b"ee");
        let mut request = AddRequest::new(AddSource::Torrent(torrent));
        request.save_dir = Some(save.clone());
        request.start = Some(false);
        request.layout = ContentLayout::NoSubfolder;
        let Ok(Reply::Added(hash)) = handle.call(Command::AddWith(request)) else {
            panic!("not added");
        };
        let paths = |handle: &Handle| -> Vec<PathBuf> {
            let snapshot = handle.snapshot();
            let torrent = snapshot.torrent(&hash).unwrap();
            torrent.files().into_iter().map(|file| file.path).collect()
        };
        assert_eq!(
            paths(&handle),
            [PathBuf::from("a.txt"), PathBuf::from("b.txt")]
        );

        handle
            .call(Command::RenameTorrent {
                torrent: hash,
                name: Some("My album".to_owned()),
            })
            .unwrap();
        let snapshot = handle.snapshot();
        let view = snapshot.torrent(&hash).unwrap();
        assert_eq!(
            (view.name.as_str(), view.original_name.as_str()),
            ("My album", "album")
        );

        std::fs::create_dir_all(&save).unwrap();
        std::fs::write(save.join("a.txt"), b"abc").unwrap();
        // The check queued on adding may still be running.
        let rename = || {
            handle.call(Command::RenamePath {
                torrent: hash,
                from: "a.txt".into(),
                to: "Disc 1/01 a.txt".into(),
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Err(failure) = rename() {
            assert!(failure.message.contains("checked"), "{failure}");
            assert!(Instant::now() < deadline, "the check never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read(save.join("Disc 1/01 a.txt")).unwrap(), b"abc");
        assert_eq!(paths(&handle)[0], PathBuf::from("Disc 1/01 a.txt"));
        assert!(
            handle
                .call(Command::RenamePath {
                    torrent: hash,
                    from: "b.txt".into(),
                    to: "../escape".into(),
                })
                .is_err()
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removes_a_torrent_with_its_files_but_nothing_else() {
        let (handle, root) = engine("delete");
        let save = root.join("saved");
        std::fs::create_dir_all(save.join("album/disc")).unwrap();
        std::fs::write(save.join("album/a.txt"), b"abc").unwrap();
        std::fs::write(save.join("album/disc/b.txt"), b"defg").unwrap();
        std::fs::write(save.join("album/cover.jpg"), b"mine").unwrap();
        let mut torrent = b"d4:infod5:filesld6:lengthi3e4:pathl5:a.txteed6:lengthi4e4:pathl4:disc5:b.txteee4:name5:album12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend([7; 20]);
        torrent.extend(b"ee");
        let mut request = AddRequest::new(AddSource::Torrent(torrent));
        request.save_dir = Some(save.clone());
        request.start = Some(false);
        let Ok(Reply::Added(hash)) = handle.call(Command::AddWith(request)) else {
            panic!("not added");
        };
        handle.call(Command::RemoveWithFiles(hash)).unwrap();
        assert!(handle.snapshot().torrent(&hash).is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while save.join("album/a.txt").exists() {
            assert!(Instant::now() < deadline, "the files were not deleted");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!save.join("album/disc").exists(), "an emptied folder goes");
        assert_eq!(
            std::fs::read(save.join("album/cover.jpg")).unwrap(),
            b"mine",
            "files that are not the torrent's stay, and so does their folder"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle
            .snapshot()
            .notices
            .iter()
            .any(|notice| notice.text.starts_with("Deleted the files of album"))
        {
            assert!(Instant::now() < deadline, "no notice");
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_files_another_torrent_uses() {
        let (handle, root) = engine("delete-shared");
        let save = root.join("saved");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::write(save.join("shared.bin"), b"abc").unwrap();
        // Two torrents (different hashes) of one file in one folder.
        let add = |fill: u8| {
            let mut torrent =
                b"d4:infod6:lengthi3e4:name10:shared.bin12:piece lengthi16384e6:pieces20:".to_vec();
            torrent.extend([fill; 20]);
            torrent.extend(b"ee");
            let mut request = AddRequest::new(AddSource::Torrent(torrent));
            request.save_dir = Some(save.clone());
            request.start = Some(false);
            match handle.call(Command::AddWith(request)) {
                Ok(Reply::Added(hash)) => hash,
                other => panic!("not added: {other:?}"),
            }
        };
        let (first, _second) = (add(1), add(2));
        handle.call(Command::RemoveWithFiles(first)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle
            .snapshot()
            .notices
            .iter()
            .any(|notice| notice.text.starts_with("Kept the files of shared.bin"))
        {
            assert!(Instant::now() < deadline, "no notice");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(save.join("shared.bin").exists());
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changes_settings_and_explains_bad_ones() {
        let (handle, root) = engine("settings");
        let change = |key: &str, value: &str| {
            handle.call(Command::ChangeSetting {
                key: key.to_owned(),
                value: value.to_owned(),
            })
        };
        change("network.upload_limit", "512").unwrap();
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.rate_limits.1, 512 * 1024, "applied at once");
        let shown = snapshot
            .settings
            .iter()
            .find(|item| item.setting.key == "network.upload_limit")
            .unwrap();
        assert_eq!(shown.value, "512");
        assert!(change("network.upload_limit", "lots").is_err());
        assert!(change("no.such.setting", "1").is_err());
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn anonymous_mode_switches_on_and_off_while_running() {
        let (handle, root) = engine("anonymous");
        let (link, hash) = magnet(3);
        handle.call(Command::Add(link)).unwrap();
        let change = |value: &str| {
            handle
                .call(Command::ChangeSetting {
                    key: "network.anonymous".to_owned(),
                    value: value.to_owned(),
                })
                .unwrap();
            handle.snapshot()
        };
        let snapshot = change("true");
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.text.starts_with("Anonymous mode is on")),
            "{:?}",
            snapshot.notices
        );
        assert!(snapshot.torrent(&hash).is_some(), "torrents stay");
        let snapshot = change("false");
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.text == "Anonymous mode is off")
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reannouncing_a_paused_torrent_says_to_start_it() {
        let (handle, root) = engine("reannounce");
        let (link, hash) = magnet(4);
        handle.call(Command::Add(link)).unwrap();
        handle.call(Command::Reannounce(hash)).unwrap();
        assert!(
            handle
                .snapshot()
                .notices
                .iter()
                .any(|notice| notice.text == "test-4 is not running; start it to announce it")
        );
        assert!(handle.call(Command::Reannounce([0; 20])).is_err());
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scrapes_trackers_for_paused_torrents_too() {
        use std::io::{Read, Write};
        let (handle, root) = engine("scrape");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let announce = format!("http://{}/announce", listener.local_addr().unwrap());
        let mut torrent = format!("d8:announce{}:{announce}4:infod6:lengthi5e4:name9:hello.txt12:piece lengthi16384e6:pieces20:", announce.len()).into_bytes();
        torrent.extend([7; 20]);
        torrent.extend(b"ee");
        let hash = *TorrentInfo::from_bytes(&torrent).unwrap().info_hash();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..size]).into_owned();
                if !request.starts_with("GET /scrape?") {
                    continue;
                }
                let mut body = b"d5:filesd20:".to_vec();
                body.extend(hash);
                body.extend(b"d8:completei12e10:downloadedi345e10:incompletei6eeee");
                let _ = stream.write_all(&[b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &body].concat());
            }
        });
        let mut request = AddRequest::new(AddSource::Torrent(torrent));
        request.start = Some(false);
        handle.call(Command::AddWith(request)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let view = loop {
            let snapshot = handle.snapshot();
            let view = snapshot.torrent(&hash).unwrap().clone();
            if view.completed.is_some() || Instant::now() > deadline {
                break view;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(view.status, Status::Paused);
        assert_eq!(view.swarm, (Some(12), Some(6)));
        assert_eq!(view.completed, Some(345));
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn publishes_snapshots_that_interfaces_can_wait_for() {
        let (handle, root) = engine("wait");
        let first = handle.snapshot();
        let next = handle.wait_for_change(first.generation, Duration::from_secs(5));
        assert!(next.generation > first.generation, "a tick publishes");
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn several_interfaces_can_drive_one_engine_at_once() {
        let (handle, root) = engine("shared");
        std::thread::scope(|scope| {
            for byte in 1..=8 {
                let handle = handle.clone();
                scope.spawn(move || handle.call(Command::Add(magnet(byte).0)).unwrap());
            }
        });
        assert_eq!(handle.snapshot().torrents.len(), 8);
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn organizes_torrents_with_categories_and_tags() {
        let (handle, root) = engine("labels");
        let (one, first) = magnet(1);
        let (two, second) = magnet(2);
        handle.call(Command::Add(one)).unwrap();
        handle.call(Command::Add(two)).unwrap();
        handle
            .call(Command::CreateCategory("Empty".to_owned()))
            .unwrap();
        handle
            .call(Command::SetCategory {
                torrent: first,
                category: Some(" Linux ".to_owned()),
            })
            .unwrap();
        handle
            .call(Command::SetTags {
                torrent: first,
                tags: vec!["iso".to_owned(), "favorite".to_owned(), "iso".to_owned()],
            })
            .unwrap();
        handle
            .call(Command::SetTags {
                torrent: second,
                tags: vec!["iso".to_owned()],
            })
            .unwrap();
        let snapshot = handle.snapshot();
        let shown = snapshot.torrent(&first).unwrap();
        assert_eq!(shown.category.as_deref(), Some("Linux"), "trimmed");
        assert_eq!(shown.tags, ["favorite", "iso"]);
        assert_eq!(snapshot.categories, ["Empty", "Linux"]);
        assert_eq!(snapshot.tags, ["favorite", "iso"]);

        // Bad names and unknown torrents are refused.
        let bad = |command| handle.call(command).unwrap_err().message;
        assert!(bad(Command::CreateTag("a\tb".to_owned())).contains("control"));
        assert!(
            bad(Command::SetCategory {
                torrent: [9; 20],
                category: None
            })
            .contains("no such torrent")
        );
        assert!(bad(Command::RemoveTag("missing".to_owned())).contains("no tag"));

        // Removing a tag takes it off every torrent; removing a torrent
        // forgets its labels.
        handle
            .call(Command::RemoveTag("favorite".to_owned()))
            .unwrap();
        handle.call(Command::Remove(second)).unwrap();
        handle.shutdown();

        let again = start_in(&root);
        let snapshot = again.snapshot();
        let shown = snapshot.torrent(&first).unwrap();
        assert_eq!(shown.category.as_deref(), Some("Linux"));
        assert_eq!(shown.tags, ["iso"]);
        assert_eq!(snapshot.categories, ["Empty", "Linux"]);
        assert_eq!(snapshot.tags, ["iso"]);
        again.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_a_damaged_labels_file_aside() {
        let root = std::env::temp_dir().join(format!(
            "empirebt-app-damaged-labels-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/labels"), "not a labels file").unwrap();
        let handle = start_in(&root);
        let notice = &handle.snapshot().notices[0];
        assert_eq!(notice.level, Level::Error);
        assert!(notice.text.contains("labels.damaged"), "{}", notice.text);
        assert!(root.join("data/labels.damaged").exists());
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A single-file `.torrent` for `content`, named `name`, in pieces
    /// of `piece_length`.
    fn single_file(name: &str, content: &[u8], piece_length: usize) -> Vec<u8> {
        use sha1::Digest;
        let mut torrent = format!(
            "d4:infod6:lengthi{}e4:name{}:{name}12:piece lengthi{piece_length}e6:pieces{}:",
            content.len(),
            name.len(),
            content.len().div_ceil(piece_length) * 20
        )
        .into_bytes();
        for piece in content.chunks(piece_length) {
            torrent.extend(sha1::Sha1::digest(piece));
        }
        torrent.extend(b"ee");
        torrent
    }

    /// Adds `torrent` saved in `folder` and starts it.
    fn add_started(handle: &Handle, torrent: Vec<u8>, folder: PathBuf) -> InfoHash {
        let mut request = AddRequest::new(AddSource::Torrent(torrent));
        request.save_dir = Some(folder);
        request.start = Some(true);
        let Ok(Reply::Added(hash)) = handle.call(Command::AddWith(request)) else {
            panic!("not added");
        };
        hash
    }

    fn seeds(snapshot: &Snapshot, count: usize) -> bool {
        matches!(snapshot.network.seed_status, SeedStatus::Listening { torrents } if torrents == count)
    }

    /// A seed whose files cannot be read (here a folder on the way is a
    /// file, as an unmounted drive or a lost permission would be) is shown
    /// as an error, and seeds after it in the list still seed; once its
    /// files are back, it seeds again by itself.
    #[test]
    fn one_unreadable_seed_does_not_stop_the_others() {
        let (handle, root) = engine("left-out");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/a.txt"), b"first seed").unwrap();
        std::fs::write(root.join("b/b.txt"), b"second seed").unwrap();
        let first = add_started(
            &handle,
            single_file("a.txt", b"first seed", 16384),
            root.join("a"),
        );
        let second = add_started(
            &handle,
            single_file("b.txt", b"second seed", 16384),
            root.join("b"),
        );
        assert!(
            wait_for(&handle, 20, |snapshot| seeds(snapshot, 2)),
            "never seeded both"
        );
        handle.shutdown();

        std::fs::remove_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a"), b"not a folder").unwrap();
        let handle = start_in(&root);
        assert!(
            wait_for(&handle, 20, |snapshot| seeds(snapshot, 1)
                && snapshot.torrent(&first).unwrap().status == Status::Error),
            "the second seed did not seed: {:?}",
            handle.snapshot().network.seed_status
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.torrent(&second).unwrap().status, Status::Seeding);
        let error = snapshot.torrent(&first).unwrap().error.clone().unwrap();
        assert!(error.contains("cannot read its files"), "{error}");
        assert!(
            snapshot
                .notices
                .iter()
                .any(|notice| notice.text.starts_with("a.txt is not seeding"))
        );

        std::fs::remove_file(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/a.txt"), b"first seed").unwrap();
        assert!(
            wait_for(&handle, 20, |snapshot| seeds(snapshot, 2)
                && snapshot.torrent(&first).unwrap().status == Status::Seeding),
            "the first seed did not come back"
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// Pausing a seed while it is being rechecked pauses it once the check
    /// has stopped (a cancelled check leaves a torrent as it was).
    #[test]
    fn pausing_a_seed_during_a_recheck_pauses_it() {
        let (handle, root) = engine("pause-check");
        // Large enough that the check is still going when the pause comes.
        let content = vec![0; 64 << 20];
        let file = std::fs::File::create(root.join("big.bin")).unwrap();
        file.set_len(content.len() as u64).unwrap();
        let hash = add_started(
            &handle,
            single_file("big.bin", &content, 4 << 20),
            root.clone(),
        );
        assert!(
            wait_for(&handle, 60, |snapshot| snapshot
                .torrent(&hash)
                .unwrap()
                .status
                == Status::Seeding
                && seeds(snapshot, 1)),
            "never seeded"
        );
        handle.call(Command::Recheck(hash)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.snapshot().torrent(&hash).unwrap().status != Status::Checking {
            assert!(Instant::now() < deadline, "the check never started");
            std::thread::sleep(Duration::from_millis(2));
        }
        handle.call(Command::Pause(hash)).unwrap();
        assert!(
            wait_for(&handle, 20, |snapshot| snapshot
                .torrent(&hash)
                .unwrap()
                .status
                == Status::Paused
                && seeds(snapshot, 0)),
            "still {:?}",
            handle.snapshot().torrent(&hash).unwrap().status
        );
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            handle.snapshot().torrent(&hash).unwrap().status,
            Status::Paused
        );
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// Commands arriving back to back from several interfaces never hold
    /// back the engine's background work (here, a queued check).
    #[test]
    fn background_work_goes_on_under_a_stream_of_commands() {
        let (handle, root) = engine("busy");
        std::fs::write(root.join("busy.txt"), b"some data").unwrap();
        let mut request = AddRequest::new(AddSource::Torrent(single_file(
            "busy.txt",
            b"some data",
            16384,
        )));
        request.save_dir = Some(root.clone());
        request.start = Some(false);
        let Ok(Reply::Added(hash)) = handle.call(Command::AddWith(request)) else {
            panic!("not added");
        };
        let stop = Arc::new(AtomicBool::new(false));
        let callers: Vec<_> = (0..32)
            .map(|_| {
                let (handle, stop) = (handle.clone(), Arc::clone(&stop));
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = handle.call(Command::Peers(hash));
                    }
                })
            })
            .collect();
        let checked = wait_for(&handle, 10, |snapshot| {
            snapshot.torrent(&hash).unwrap().fraction == Some(1.0)
        });
        stop.store(true, Ordering::Relaxed);
        for caller in callers {
            caller.join().unwrap();
        }
        assert!(checked, "the queued check never ran");
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A session save that fails (here its folder is a file) is reported
    /// once and tried again until it works.
    #[test]
    fn tries_a_failed_session_save_again() {
        let (handle, root) = engine("save-retry");
        let session = root.join("data").join("session");
        let _ = std::fs::remove_dir_all(&session);
        std::fs::write(&session, b"in the way").unwrap();
        let (link, hash) = magnet(3);
        handle.call(Command::Add(link)).unwrap();
        let failures = |snapshot: &Snapshot| {
            snapshot
                .notices
                .iter()
                .filter(|notice| notice.text.starts_with("cannot save session"))
                .count()
        };
        assert!(wait_for(&handle, 10, |snapshot| failures(snapshot) == 1));
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(failures(&handle.snapshot()), 1, "reported once");

        std::fs::remove_file(&session).unwrap();
        assert!(wait_for(&handle, 10, |snapshot| {
            snapshot
                .notices
                .iter()
                .any(|notice| notice.text == "Saved the session again")
        }));
        handle.shutdown();
        let handle = start_in(&root);
        assert!(handle.snapshot().torrent(&hash).is_some(), "it was saved");
        handle.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shutting_down_saves_the_session_for_the_next_run() {
        let (handle, root) = engine("restart");
        let (link, hash) = magnet(9);
        handle.call(Command::Add(link)).unwrap();
        handle.shutdown();
        // Later calls fail plainly instead of hanging.
        assert!(handle.call(Command::Pause(hash)).is_err());
        handle.shutdown();

        let again = start_in(&root);
        assert_eq!(again.snapshot().torrent(&hash).unwrap().name, "test-9");
        again.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }
}
