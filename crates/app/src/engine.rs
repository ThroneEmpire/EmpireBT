//! The engine thread: owns the client and everything around it, runs
//! commands from interfaces, and publishes snapshots.

use crate::labels::{Labels, clean_name};
use crate::snapshot::{
    DhtState, Level, Network, Notice, PeerView, RateMeter, SeedStatus, SettingView, Snapshot,
    Status, TorrentView,
};
use crate::{InfoHash, Store};
use empirebt_backend::{
    AnnounceStatus, BackendError, Client, ClientConfig, ConfigFile, Dht, MagnetLink, Runtime,
    SeedAnnouncer, TorrentInfo, TransferHandle, TransferKind, TransferPhase, TransferProgress,
    TransferState, UtpSocket,
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
/// Data files unused this long are closed.
const IDLE_FILE_CLOSE: Duration = Duration::from_secs(5 * 60);
/// Local-data checks running at once. More would make spinning disks seek.
const VERIFY_CONCURRENCY: usize = 2;
/// Time per tick spent moving the check queue along.
const VERIFY_BUDGET: Duration = Duration::from_millis(40);
/// A seeding runtime that failed to start (port taken, network interface
/// down) is tried again this often.
const SEED_RETRY: Duration = Duration::from_secs(10);
/// How long one request may take when downloading a `.torrent` from a URL.
const TORRENT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// How often `network.port_file` is checked for a new port.
const PORT_FILE_POLL: Duration = Duration::from_secs(5);

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
    /// Removes the torrent from the list. Downloaded files stay on disk.
    Remove(InfoHash),
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
            peer_timeout: config.peer_timeout(),
            binding: config.binding(),
        };
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
            Err(mpsc::RecvTimeoutError::Timeout) => {
                engine.tick();
                publish(&mut engine, published);
                next_tick = Instant::now() + TICK;
            }
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
    peer_timeout: Duration,
    /// The network interface every connection uses.
    binding: empirebt_backend::Binding,
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
            announcer: SeedAnnouncer::start_on(
                settings.binding.clone(),
                settings.peer_id,
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
    /// and syncs its torrents after state changes.
    fn tick(&mut self, client: &mut Client, settings: &Settings) {
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
            return;
        };
        if client.runtime().is_none() {
            client.set_runtime(Some(Arc::clone(&runtime)));
        }
        if !self.tracking {
            self.tracking = true;
            self.announcer.track(Arc::clone(&runtime));
        }
        if std::mem::take(&mut self.resync)
            && let Err(error) = client.sync_runtime(settings.peer_id, settings.peer_timeout)
            && let Ok(mut status) = self.status.lock()
        {
            *status = SeedStatus::Failed(error.to_string());
        }
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
    seeding: SeedService,
    last_file_sweep: Instant,
    dht_state: DhtState,
    config: ConfigFile,
    /// Settings that command-line flags override for this run.
    overridden: Vec<&'static str>,
    last_sample: Instant,
    session_dirty: bool,
    notices: VecDeque<Notice>,
    next_notice: u64,
    generation: u64,
    labels: Labels,
    geoip: crate::geoip::GeoIp,
    last_port_check: Instant,
    /// The last problem reported with `network.port_file`, so it is
    /// reported once.
    port_file_problem: Option<String>,
    /// Downloads stopped to move to a new port, started again once the
    /// new runtime is live.
    restart_after_rebind: Vec<InfoHash>,
    /// What the log last said about seeding and the network interface, so
    /// it speaks only when they change.
    logged_seeding: Option<String>,
    logged_interface_up: Option<bool>,
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
            seeding,
            last_file_sweep: Instant::now(),
            dht_state,
            config,
            overridden,
            last_sample: Instant::now(),
            session_dirty: false,
            notices: VecDeque::new(),
            next_notice: 0,
            generation: 0,
            labels: Labels::default(),
            geoip: crate::geoip::GeoIp::default(),
            last_port_check: Instant::now(),
            port_file_problem: None,
            restart_after_rebind: Vec::new(),
            logged_seeding: None,
            logged_interface_up: None,
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
            Command::Remove(hash) => {
                self.remove(hash);
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
        match self.client.torrent_index(hash) {
            Some(_) => Ok(()),
            None => Err(Failure::new("there is no such torrent")),
        }
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
            (None, _) if activity.is_some_and(|activity| activity.error.is_some()) => Status::Error,
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
        let error = activity.and_then(|activity| {
            activity.error.clone().or_else(|| {
                activity
                    .job
                    .as_ref()
                    .and_then(|job| job.progress().last_error())
            })
        });
        let trackers = match (info, torrent.magnet()) {
            (Some(info), _) => info.tracker_urls(),
            (None, Some(link)) => link.trackers(),
            (None, None) => &[],
        };
        Some(TorrentView {
            hash,
            name: torrent.name().to_owned(),
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
        self.seeding.tick(&mut self.client, &self.settings);
        self.log_changes();
        self.restart_rebound_downloads();
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
        if self.session_dirty {
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
            .collect();
        jobs.iter()
            .chain(&detached)
            .for_each(TransferHandle::cancel);
        let services = self.seeding.stop();
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
        if port == self.client.config().listen_port() {
            return;
        }
        let mut jobs = Vec::new();
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
                jobs.push(job);
                self.restart_after_rebind.push(*hash);
            }
        }
        let services = self.seeding.stop();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline
            && (jobs.iter().any(|job| !job.is_finished())
                || services.iter().any(|thread| !thread.is_finished()))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        for job in &jobs {
            self.client.checkpoint(job);
        }
        // Any still winding down finish on their own.
        self.detached.extend(jobs);
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
        let config = self.client.config().clone();
        self.dht_state = start_networking(&mut self.client, &config, &self.settings, &self.store);
        self.seeding = SeedService::new(port, &self.settings, self.client.dht().cloned());
        self.session_dirty = true;
        self.notify(Level::Info, format!("Now taking peers on port {port}"));
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
                } else {
                    tracing::warn!(target: "network", "{name} is down; nothing connects until it is back");
                }
                self.logged_interface_up = Some(up);
            }
        }
    }

    /// Starts the downloads [`Engine::change_port`] stopped, once the new
    /// runtime is live (or has failed), unless they were paused or removed
    /// meanwhile.
    fn restart_rebound_downloads(&mut self) {
        if self.restart_after_rebind.is_empty()
            || (self.seeding.live().is_none()
                && !matches!(self.seeding.status(), SeedStatus::Failed(_)))
        {
            return;
        }
        for hash in std::mem::take(&mut self.restart_after_rebind) {
            let downloading = self.client.torrent_index(&hash).is_some_and(|index| {
                self.client.torrents()[index].state() == TransferState::Downloading
            });
            if downloading {
                self.start(hash);
            }
        }
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
        let activity = self.activity.entry(hash).or_default();
        activity.start_after_verify = false;
        if let Some(job) = &activity.job {
            job.cancel();
            if job.kind() != TransferKind::Verify {
                activity.pausing = true;
            }
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

    fn remove(&mut self, hash: InfoHash) {
        let Some(index) = self.client.torrent_index(&hash) else {
            return;
        };
        if let Some(job) = self
            .activity
            .remove(&hash)
            .and_then(|activity| activity.job)
        {
            job.cancel();
            self.detached.push(job);
        }
        self.verify_queue.retain(|queued| *queued != hash);
        let Ok(torrent) = self.client.remove_torrent(index) else {
            return;
        };
        if self.labels.forget(&hash) {
            let _ = self.save_labels();
        }
        self.seeding.resync = true;
        self.session_dirty = true;
        self.notify(
            Level::Info,
            format!(
                "Removed {} — downloaded files are still in {}",
                torrent.name(),
                self.client.config().download_directory().display()
            ),
        );
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
            activity.pausing = false;
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
        for hash in uploaded.keys() {
            self.activity.entry(*hash).or_default();
        }
    }

    // ----- helpers ------------------------------------------------------------

    fn save(&mut self) {
        match self.store.save(&self.client) {
            Ok(()) => self.session_dirty = false,
            Err(error) => {
                self.session_dirty = false;
                self.notify(Level::Error, error);
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
            .map(|index| self.client.torrents()[index].name().to_owned())
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
