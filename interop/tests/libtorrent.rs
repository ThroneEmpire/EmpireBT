//! Interoperability with libtorrent, the reference BitTorrent implementation
//! (used by qBittorrent and Deluge).
//!
//! Each test runs real libtorrent peers (through `lt_peer.py`) against
//! EmpireBT's backend in both directions, for v1, hybrid, and v2-only
//! torrents, from `.torrent` files and magnet links. Peers find each other
//! by address, so no tracker or DHT is involved.
//!
//! Set up libtorrent once with `python3 interop/setup_libtorrent.py`, then
//! run `cargo test -p empirebt-interop`. Without libtorrent the tests skip;
//! set `EMPIREBT_INTEROP=require` to make that a failure instead.

use empirebt_backend::{
    BackendError, ConnectOptions, DownloadSession, Encryption, Lsd, MagnetLink, PEER_ID_PREFIX,
    Runtime, Seeder, Storage, TorrentFormat, TorrentInfo, UtpSocket, V2Seeder,
    generate_anonymous_peer_id, generate_peer_id,
};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// How long libtorrent may take to finish a download.
const DOWNLOAD_SECONDS: u32 = 60;

// ----- libtorrent -----------------------------------------------------------

/// Locates libtorrent, or returns `None` (skipping the test) when it is not
/// set up, unless `EMPIREBT_INTEROP=require`.
fn libtorrent() -> Option<Libtorrent> {
    let python = std::env::var("PYTHON")
        .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.to_owned());
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("lt_peer.py");
    let ready = Command::new(&python)
        .arg("-I")
        .arg(&script)
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success());
    if ready {
        return Some(Libtorrent {
            python,
            script,
            encryption: None,
            transport: None,
            upload_limit: None,
            lsd: false,
            ipv6: false,
        });
    }
    let message = "libtorrent is not set up: run `python3 interop/setup_libtorrent.py`";
    if std::env::var("EMPIREBT_INTEROP").as_deref() == Ok("require") {
        panic!("{message}");
    }
    eprintln!("skipped: {message}");
    None
}

#[derive(Clone)]
struct Libtorrent {
    python: String,
    script: PathBuf,
    /// `LT_ENCRYPTION` for lt_peer.py: "forced", "disabled", or unset for
    /// libtorrent's defaults.
    encryption: Option<&'static str>,
    /// `LT_TRANSPORT`: "utp", "tcp", or unset for both, uTP first.
    transport: Option<&'static str>,
    /// `LT_UPLOAD_LIMIT` in bytes per second.
    upload_limit: Option<u32>,
    /// `LT_LSD=1`: local peer discovery on, listening on every interface.
    lsd: bool,
    /// `LT_LISTEN=[::1]`: listening over IPv6.
    ipv6: bool,
}

impl Libtorrent {
    /// The same libtorrent with an encryption policy instead of its default.
    fn with_encryption(&self, mode: &'static str) -> Self {
        Self {
            encryption: Some(mode),
            ..self.clone()
        }
    }

    /// The same libtorrent using only one transport.
    fn with_transport(&self, mode: &'static str) -> Self {
        Self {
            transport: Some(mode),
            ..self.clone()
        }
    }

    /// The same libtorrent uploading at most `bytes_per_second`.
    fn with_upload_limit(&self, bytes_per_second: u32) -> Self {
        Self {
            upload_limit: Some(bytes_per_second),
            ..self.clone()
        }
    }

    /// The same libtorrent finding peers on the local network (BEP 14).
    fn with_lsd(&self) -> Self {
        Self {
            lsd: true,
            ..self.clone()
        }
    }

    /// The same libtorrent listening on IPv6 loopback.
    fn with_ipv6(&self) -> Self {
        Self {
            ipv6: true,
            ..self.clone()
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.python);
        command.arg("-I").arg(&self.script).args(args);
        if let Some(mode) = self.encryption {
            command.env("LT_ENCRYPTION", mode);
        }
        if let Some(mode) = self.transport {
            command.env("LT_TRANSPORT", mode);
        }
        if let Some(limit) = self.upload_limit {
            command.env("LT_UPLOAD_LIMIT", limit.to_string());
        }
        if self.lsd {
            command.env("LT_LSD", "1");
        }
        if self.ipv6 {
            command.env("LT_LISTEN", "[::1]");
        }
        command
    }

    /// Runs a command to completion, failing the test with its output.
    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().expect("python runs");
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "lt_peer.py {args:?} failed:\n{text}"
        );
        text
    }

    fn make(&self, source: &Path, out: &Path, format: Format, piece_size: u32) {
        self.make_with(source, out, format, piece_size, false);
    }

    fn make_with(&self, source: &Path, out: &Path, format: Format, piece_size: u32, private: bool) {
        self.run(&[
            "make",
            path(source),
            path(out),
            format.name(),
            &piece_size.to_string(),
            if private { "private" } else { "public" },
        ]);
    }

    fn magnet(&self, torrent: &Path) -> String {
        self.run(&["magnet", path(torrent)]).trim().to_owned()
    }

    /// Starts a seeder and waits until it has checked its data.
    fn seed(&self, torrent: &Path, save: &Path) -> LtSeeder {
        let port = free_port();
        self.spawn(
            &["seed", path(torrent), path(save), &port.to_string()],
            port,
            true,
        )
    }

    /// Starts a download from `peer` in the background, listening on a
    /// port of its own.
    fn download_in_background(&self, torrent: &Path, save: &Path, peer: SocketAddr) -> LtSeeder {
        let port = free_port();
        self.spawn(
            &[
                "download",
                path(torrent),
                path(save),
                &port.to_string(),
                &peer.to_string(),
                &DOWNLOAD_SECONDS.to_string(),
            ],
            port,
            false,
        )
    }

    /// Runs `lt_peer.py` in the background, collecting its output; with
    /// `wait_ready`, until it prints READY.
    fn spawn(&self, args: &[&str], port: u16, wait_ready: bool) -> LtSeeder {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("python runs");
        let (ready, wait) = mpsc::channel();
        let log = Arc::new(Mutex::new(String::new()));
        let output = BufReader::new(child.stdout.take().unwrap());
        let lines = Arc::clone(&log);
        std::thread::spawn(move || {
            for line in output.lines().map_while(Result::ok) {
                if line == "READY" {
                    let _ = ready.send(());
                }
                lines.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
        let host: std::net::IpAddr = if self.ipv6 {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        };
        let seeder = LtSeeder {
            child,
            address: SocketAddr::new(host, port),
            log,
        };
        if wait_ready && wait.recv_timeout(Duration::from_secs(30)).is_err() {
            panic!("libtorrent seeder never became ready:\n{}", seeder.log());
        }
        seeder
    }

    /// Downloads from `peer` until complete, failing the test otherwise.
    /// Returns libtorrent's log.
    fn download(&self, source: &str, save: &Path, peer: SocketAddr) -> String {
        let output = self.run_status(&[
            "download",
            source,
            path(save),
            &free_port().to_string(),
            &peer.to_string(),
            &DOWNLOAD_SECONDS.to_string(),
        ]);
        assert!(
            output.contains("COMPLETE"),
            "libtorrent did not finish:\n{output}"
        );
        output
    }

    /// Connects to `peer` and reports what libtorrent saw: the client names
    /// it showed, as qBittorrent's and Deluge's peer lists display them
    /// (first decoded from the peer ID, then taken from the extension
    /// handshake), and the connection's encryption. `None` if libtorrent
    /// could not finish a handshake within `seconds`.
    fn identify_peer(
        &self,
        torrent: &Path,
        save: &Path,
        peer: SocketAddr,
        seconds: u32,
    ) -> Option<PeerReport> {
        let output = self.run_status(&[
            "identify-peer",
            path(torrent),
            path(save),
            &free_port().to_string(),
            &peer.to_string(),
            &seconds.to_string(),
        ]);
        (!output.contains("TIMEOUT")).then(|| PeerReport {
            clients: tagged(&output, "client: "),
            encryption: tagged(&output, "encryption: "),
            transport: tagged(&output, "transport: "),
        })
    }

    fn run_status(&self, args: &[&str]) -> String {
        let output = self.command(args).output().expect("python runs");
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr)
    }
}

struct PeerReport {
    clients: Vec<String>,
    /// "rc4", "plaintext" (only the handshake was encrypted), or "none".
    encryption: Vec<String>,
    /// "utp" or "tcp".
    transport: Vec<String>,
}

/// The values of log lines starting with `tag`.
fn tagged(log: &str, tag: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| line.strip_prefix(tag))
        .map(str::to_owned)
        .collect()
}

/// A running libtorrent seeder, stopped when dropped.
struct LtSeeder {
    child: Child,
    address: SocketAddr,
    log: Arc<Mutex<String>>,
}

impl LtSeeder {
    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// Waits up to `limit` for it to finish downloading.
    fn wait_complete(&self, limit: Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while std::time::Instant::now() < deadline {
            if self.log().lines().any(|line| line == "COMPLETE") {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// The highest download progress it has printed, in percent.
    fn progress(&self) -> f64 {
        self.log()
            .lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .nth(1)?
                    .strip_suffix('%')?
                    .parse()
                    .ok()
            })
            .fold(0.0, f64::max)
    }
}

impl Drop for LtSeeder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ----- EmpireBT -------------------------------------------------------------

/// An EmpireBT seeding runtime on a local port, stopped when dropped.
struct EmpireSeeder {
    address: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmpireSeeder {
    fn start(info: TorrentInfo, data: &Path) -> Self {
        Self::start_with(info, data, Encryption::default())
    }

    fn start_with(info: TorrentInfo, data: &Path, encryption: Encryption) -> Self {
        Self::start_at(info, data, encryption, "127.0.0.1:0")
    }

    /// Seeds on `address`, such as `[::1]:0` for IPv6.
    fn start_at(info: TorrentInfo, data: &Path, encryption: Encryption, address: &str) -> Self {
        Self::start_as(info, data, encryption, address, generate_peer_id())
    }

    /// Seeds with `peer_id`, such as an anonymous one.
    fn start_as(
        info: TorrentInfo,
        data: &Path,
        encryption: Encryption,
        address: &str,
        peer_id: [u8; 20],
    ) -> Self {
        let storage = Storage::new(data).unwrap();
        let (v1, v2) = if info.format() == TorrentFormat::V2 {
            let seeder = V2Seeder::new(info, storage, peer_id, TIMEOUT).unwrap();
            (Vec::new(), vec![seeder])
        } else {
            let seeder = Seeder::new(info, storage, peer_id, TIMEOUT).unwrap();
            (vec![seeder], Vec::new())
        };
        // Peers reach the same port over TCP and uTP, as in the client.
        let listener = TcpListener::bind(address).unwrap();
        let address = listener.local_addr().unwrap();
        let utp = UtpSocket::bind(address).unwrap();
        let runtime = Arc::new(
            Runtime::new_mixed(v1, v2, 16)
                .unwrap()
                .with_encryption(encryption)
                .with_utp(Some(utp)),
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            runtime.serve_until(&listener, &stop).unwrap();
        });
        Self {
            address,
            shutdown,
            thread: Some(thread),
        }
    }
}

impl Drop for EmpireSeeder {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Downloads with EmpireBT from one peer, as a finished download would end:
/// every piece verified and empty files created.
fn empire_download(info: &TorrentInfo, save: &Path, peer: SocketAddr) {
    try_empire_download(info, save, peer, ConnectOptions::new())
        .unwrap_or_else(|error| panic!("EmpireBT download failed: {error}"));
}

fn try_empire_download(
    info: &TorrentInfo,
    save: &Path,
    peer: SocketAddr,
    options: ConnectOptions,
) -> Result<(), BackendError> {
    let storage = Storage::new(save).unwrap();
    let session = DownloadSession::new(info.clone(), storage.clone(), generate_peer_id(), TIMEOUT)
        .with_connect_options(options);
    session.download_from_peers(&[peer], 4)?;
    assert!(session.is_complete().unwrap());
    storage.create_empty_files(info).unwrap();
    Ok(())
}

/// Connection options that use uTP over a fresh local socket.
fn over_utp(prefer: bool) -> ConnectOptions {
    let socket = UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    ConnectOptions::new()
        .with_utp(Some(socket))
        .prefer_utp(prefer)
}

// ----- test data ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    V1,
    Hybrid,
    V2,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::Hybrid => "hybrid",
            Self::V2 => "v2",
        }
    }

    fn expected(self) -> TorrentFormat {
        match self {
            Self::V1 => TorrentFormat::V1,
            Self::Hybrid => TorrentFormat::Hybrid,
            Self::V2 => TorrentFormat::V2,
        }
    }
}

/// A scratch directory for one test, removed afterwards.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("empirebt-interop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn join(&self, part: &str) -> PathBuf {
        self.0.join(part)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes deterministic files shaped to catch layout bugs: an empty file, a
/// tiny one, multi-piece files with odd sizes, and a subdirectory. In hybrid
/// torrents these need padding files between them.
fn write_album(root: &Path) -> PathBuf {
    let album = root.join("album");
    let files: [(&str, usize); 5] = [
        ("extras/empty.txt", 0),
        ("extras/notes.txt", 5_000),
        ("tiny.txt", 37),
        ("track1.flac", 1_000_000),
        ("track2.flac", 2_500_123),
    ];
    for (index, (name, length)) in files.into_iter().enumerate() {
        write_file(&album.join(name), length, index as u64);
    }
    album
}

fn write_file(path: &Path, length: usize, seed: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let bytes: Vec<u8> = (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    std::fs::write(path, bytes).unwrap();
}

/// Creates a torrent for the album with libtorrent and loads it in EmpireBT.
fn album_torrent(lt: &Libtorrent, scratch: &Scratch, format: Format) -> (PathBuf, TorrentInfo) {
    album_torrent_with(lt, scratch, format, false)
}

/// The album as a private torrent (BEP 27), so no peer exchange happens:
/// peers only meet whom they are told to.
fn private_album_torrent(
    lt: &Libtorrent,
    scratch: &Scratch,
    format: Format,
) -> (PathBuf, TorrentInfo) {
    let (torrent, info) = album_torrent_with(lt, scratch, format, true);
    assert!(info.is_private());
    (torrent, info)
}

fn album_torrent_with(
    lt: &Libtorrent,
    scratch: &Scratch,
    format: Format,
    private: bool,
) -> (PathBuf, TorrentInfo) {
    let album = write_album(&scratch.join("source"));
    let torrent = scratch.join("album.torrent");
    lt.make_with(&album, &torrent, format, 64 * 1024, private);
    let info = TorrentInfo::from_file(&torrent).unwrap_or_else(|error| {
        panic!("EmpireBT cannot read libtorrent's {format:?} torrent: {error}")
    });
    assert_eq!(info.format(), format.expected());
    (torrent, info)
}

/// Asserts that `downloaded` holds exactly the source files: same names,
/// same bytes, empty files present, and no padding files written to disk.
fn assert_same_files(source: &Path, downloaded: &Path) {
    let expected = files_under(source);
    let actual = files_under(downloaded);
    assert!(
        actual
            .keys()
            .all(|name| !name.starts_with(".pad") && !name.contains("/.pad")),
        "padding files were written to disk: {:?}",
        actual.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        expected.keys().collect::<Vec<_>>(),
        actual.keys().collect::<Vec<_>>(),
        "different files"
    );
    for (name, bytes) in &expected {
        assert!(actual[name] == *bytes, "{name} differs");
    }
}

fn files_under(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn path(path: &Path) -> &str {
    path.to_str().expect("test paths are UTF-8")
}

// ----- .torrent files -------------------------------------------------------

fn downloads_from_libtorrent(format: Format) {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new(&format!("get-{}", format.name()));
    let (torrent, info) = album_torrent(&lt, &scratch, format);
    let seeder = lt.seed(&torrent, &scratch.join("source"));
    empire_download(&info, &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
}

/// A libtorrent seed (qBittorrent's engine) watches our progress grow as
/// it uploads to us: we announce every verified piece, even to peers that
/// have it. A seed that saw only our starting bitfield would think we claim
/// less than we downloaded, which PeerBanHelper bans as a fake.
#[test]
fn a_libtorrent_seed_sees_our_progress_grow() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("progress-seen");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);
    // Slow enough for the seed to see the progress move.
    let seeder = lt
        .with_upload_limit(400_000)
        .seed(&torrent, &scratch.join("source"));
    empire_download(&info, &scratch.join("get"), seeder.address);
    let log = seeder.log();
    let seen: Vec<u32> = tagged(&log, "peer progress: ")
        .iter()
        .filter_map(|percent| percent.parse().ok())
        .collect();
    let highest = seen.iter().copied().max().unwrap_or(0);
    assert!(highest >= 90, "libtorrent saw our progress as {seen:?}");
    assert!(seen.len() > 3, "it moved in steps: {seen:?}");
}

/// Over TCP; the uTP tests below cover uTP, which libtorrent prefers.
fn seeds_to_libtorrent(format: Format) {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_transport("tcp");
    let scratch = Scratch::new(&format!("serve-{}", format.name()));
    let (torrent, info) = album_torrent(&lt, &scratch, format);
    let seeder = EmpireSeeder::start(info, &scratch.join("source"));
    let log = lt.download(path(&torrent), &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
    assert_eq!(tagged(&log, "transport: "), ["tcp"]);
}

#[test]
fn downloads_v1_from_libtorrent() {
    downloads_from_libtorrent(Format::V1);
}

#[test]
fn downloads_hybrid_from_libtorrent() {
    downloads_from_libtorrent(Format::Hybrid);
}

#[test]
fn downloads_v2_from_libtorrent() {
    downloads_from_libtorrent(Format::V2);
}

#[test]
fn seeds_v1_to_libtorrent() {
    seeds_to_libtorrent(Format::V1);
}

#[test]
fn seeds_hybrid_to_libtorrent() {
    seeds_to_libtorrent(Format::Hybrid);
}

#[test]
fn seeds_v2_to_libtorrent() {
    seeds_to_libtorrent(Format::V2);
}

/// Without the extension handshake, libtorrent decodes our BEP 20 peer ID
/// to our client code and version, not to another client's name.
#[test]
fn libtorrent_decodes_our_peer_id() {
    let Some(lt) = libtorrent() else { return };
    let id = generate_peer_id();
    let name = lt.run(&["identify", std::str::from_utf8(&id).unwrap()]);
    let [_, a, b, major, minor, patch, ..] = PEER_ID_PREFIX.map(char::from);
    assert_eq!(name.trim(), format!("{a}{b} {major}.{minor}.{patch}"));
}

/// libtorrent names a connected peer's client from its BEP 20 peer ID
/// until the BEP 10 extension handshake arrives, then from its `v` string.
/// Neither may read as another client (`-EB` peer IDs show as "EBit").
#[test]
fn libtorrent_shows_empirebt_as_the_client() {
    let Some(lt) = libtorrent() else { return };
    let [_, a, b, major, minor, patch, ..] = PEER_ID_PREFIX.map(char::from);
    let from_peer_id = format!("{a}{b} {major}.{minor}.{patch}");
    let from_extensions = format!("EmpireBT {major}.{minor}.{patch}");
    for format in [Format::V1, Format::V2] {
        let scratch = Scratch::new(&format!("identify-{}", format.name()));
        let (torrent, info) = album_torrent(&lt, &scratch, format);
        let seeder = EmpireSeeder::start(info, &scratch.join("source"));
        let names = lt
            .identify_peer(&torrent, &scratch.join("get"), seeder.address, 20)
            .expect("libtorrent connects")
            .clients;
        let last = names.last().cloned().unwrap_or_default();
        assert_eq!(
            last, from_extensions,
            "{format:?}: libtorrent showed {names:?}"
        );
        assert!(
            names
                .iter()
                .all(|name| *name == from_peer_id || *name == from_extensions),
            "{format:?}: libtorrent showed {names:?}"
        );
    }
}

/// In anonymous mode neither the peer ID nor the extension handshake
/// gives EmpireBT away, and libtorrent still downloads.
#[test]
fn libtorrent_cannot_tell_an_anonymous_empirebt() {
    let Some(lt) = libtorrent() else { return };
    let [_, a, b, ..] = PEER_ID_PREFIX.map(char::from);
    let code = format!("{a}{b} ");
    let scratch = Scratch::new("identify-anonymous");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);
    let seeder = EmpireSeeder::start_as(
        info,
        &scratch.join("source"),
        Encryption::default(),
        "127.0.0.1:0",
        generate_anonymous_peer_id(),
    );
    // With no client name in the extension handshake, nothing ever
    // replaces the name from the peer ID, so the probe runs out its time.
    let output = lt.run_status(&[
        "identify-peer",
        path(&torrent),
        path(&scratch.join("get")),
        &free_port().to_string(),
        &seeder.address.to_string(),
        "8",
    ]);
    assert!(output.lines().any(|line| line == "connected"), "{output}");
    let names = tagged(&output, "client: ");
    assert!(
        names
            .iter()
            .all(|name| !name.contains("EmpireBT") && !name.starts_with(&code)),
        "libtorrent showed {names:?}"
    );
}

// ----- encryption (MSE) -----------------------------------------------------

/// libtorrent set to require full RC4 encryption seeds; EmpireBT, also
/// requiring it, downloads.
#[test]
fn downloads_encrypted_from_libtorrent_that_requires_it() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_encryption("forced");
    let scratch = Scratch::new("mse-get-forced");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::Hybrid);
    let seeder = lt.seed(&torrent, &scratch.join("source"));
    try_empire_download(
        &info,
        &scratch.join("get"),
        seeder.address,
        ConnectOptions::new().with_encryption(Encryption::Required),
    )
    .unwrap_or_else(|error| panic!("{error}\n{}", seeder.log()));
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
    assert_eq!(tagged(&seeder.log(), "encryption: "), ["rc4"]);
}

/// EmpireBT requiring encryption seeds to libtorrent that requires it too.
#[test]
fn seeds_encrypted_to_libtorrent_that_requires_it() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_encryption("forced");
    let scratch = Scratch::new("mse-serve-forced");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V2);
    let seeder = EmpireSeeder::start_with(info, &scratch.join("source"), Encryption::Required);
    let report = lt
        .identify_peer(&torrent, &scratch.join("probe"), seeder.address, 20)
        .expect("libtorrent connects with encryption");
    assert_eq!(report.encryption, ["rc4"]);
    lt.download(path(&torrent), &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
}

/// With everyone on default settings, connections are encrypted both ways.
/// As the responder EmpireBT picks full RC4. libtorrent picks plain text
/// after an encrypted handshake, its `prefer_rc4 = false` default.
#[test]
fn encrypts_connections_with_libtorrent_defaults() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("mse-defaults");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);

    let ours = EmpireSeeder::start(info.clone(), &scratch.join("source"));
    let report = lt
        .identify_peer(&torrent, &scratch.join("probe"), ours.address, 20)
        .expect("libtorrent connects");
    assert_eq!(report.encryption, ["rc4"]);

    let theirs = lt.seed(&torrent, &scratch.join("source"));
    empire_download(&info, &scratch.join("get"), theirs.address);
    assert_eq!(tagged(&theirs.log(), "encryption: "), ["plaintext"]);
}

/// Peers with encryption turned off still work with EmpireBT's default.
#[test]
fn falls_back_to_plain_text_for_peers_without_encryption() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_encryption("disabled");
    let scratch = Scratch::new("mse-fallback");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::Hybrid);

    let theirs = lt.seed(&torrent, &scratch.join("source"));
    empire_download(&info, &scratch.join("get"), theirs.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
    assert_eq!(tagged(&theirs.log(), "encryption: "), ["none"]);

    let ours = EmpireSeeder::start(info, &scratch.join("source"));
    lt.download(path(&torrent), &scratch.join("get2"), ours.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get2/album"));
}

/// With encryption required, plain-text peers are refused both ways.
#[test]
fn refuses_plain_text_peers_when_encryption_is_required() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_encryption("disabled");
    let scratch = Scratch::new("mse-refuse");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);

    let theirs = lt.seed(&torrent, &scratch.join("source"));
    let result = try_empire_download(
        &info,
        &scratch.join("get"),
        theirs.address,
        ConnectOptions::new().with_encryption(Encryption::Required),
    );
    assert!(result.is_err(), "downloaded from a plain-text peer");

    let ours = EmpireSeeder::start_with(info, &scratch.join("source"), Encryption::Required);
    let report = lt.identify_peer(&torrent, &scratch.join("probe"), ours.address, 3);
    assert!(report.is_none(), "a plain-text peer connected");
}

// ----- uTP (BEP 29) ---------------------------------------------------------

/// libtorrent at its defaults tries uTP first. EmpireBT answers at once,
/// with encryption on top.
#[test]
fn libtorrent_reaches_empirebt_over_utp_first() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("utp-first");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::Hybrid);
    let seeder = EmpireSeeder::start(info, &scratch.join("source"));
    let started = std::time::Instant::now();
    let report = lt
        .identify_peer(&torrent, &scratch.join("probe"), seeder.address, 20)
        .expect("libtorrent connects");
    assert_eq!(report.transport, ["utp"]);
    assert_eq!(report.encryption, ["rc4"]);
    // A TCP fallback would take seconds.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

/// libtorrent with TCP turned off downloads every format from EmpireBT.
#[test]
fn seeds_over_utp_to_libtorrent() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_transport("utp");
    for format in [Format::V1, Format::Hybrid, Format::V2] {
        let scratch = Scratch::new(&format!("utp-serve-{}", format.name()));
        let (torrent, info) = album_torrent(&lt, &scratch, format);
        let seeder = EmpireSeeder::start(info, &scratch.join("source"));
        let log = lt.download(path(&torrent), &scratch.join("get"), seeder.address);
        assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
        assert_eq!(tagged(&log, "transport: "), ["utp"], "{format:?}");
    }
}

/// EmpireBT preferring uTP downloads from libtorrent that only takes uTP.
#[test]
fn downloads_over_utp_from_libtorrent() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_transport("utp");
    for format in [Format::V1, Format::Hybrid, Format::V2] {
        let scratch = Scratch::new(&format!("utp-get-{}", format.name()));
        let (torrent, info) = album_torrent(&lt, &scratch, format);
        let seeder = lt.seed(&torrent, &scratch.join("source"));
        try_empire_download(&info, &scratch.join("get"), seeder.address, over_utp(true))
            .unwrap_or_else(|error| panic!("{format:?}: {error}\n{}", seeder.log()));
        assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
        assert_eq!(tagged(&seeder.log(), "transport: "), ["utp"], "{format:?}");
    }
}

/// EmpireBT's default connects over TCP and falls back to uTP when a peer
/// takes only uTP.
#[test]
fn falls_back_to_utp_when_tcp_fails() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_transport("utp");
    let scratch = Scratch::new("utp-fallback");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);
    let seeder = lt.seed(&torrent, &scratch.join("source"));
    try_empire_download(&info, &scratch.join("get"), seeder.address, over_utp(false))
        .unwrap_or_else(|error| panic!("{error}\n{}", seeder.log()));
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
    assert_eq!(tagged(&seeder.log(), "transport: "), ["utp"]);
}

/// Magnet links resolve over uTP, metadata and piece layers included.
#[test]
fn resolves_magnet_over_utp() {
    let Some(lt) = libtorrent() else { return };
    let lt = lt.with_transport("utp");
    let scratch = Scratch::new("utp-magnet");
    let (torrent, original) = album_torrent(&lt, &scratch, Format::Hybrid);
    let link = MagnetLink::parse(&lt.magnet(&torrent)).unwrap();
    let seeder = lt.seed(&torrent, &scratch.join("source"));
    let metadata = empirebt_backend::fetch_metadata_from_peer_with(
        seeder.address,
        &link,
        generate_peer_id(),
        TIMEOUT,
        &over_utp(true),
    )
    .unwrap_or_else(|error| panic!("{error}\n{}", seeder.log()));
    let info = TorrentInfo::from_metadata_with_layers(&metadata.info, &metadata.piece_layers, &[])
        .unwrap();
    assert_eq!(info.info_hash(), original.info_hash());
    assert_eq!(info.v2_piece_layers(), original.v2_piece_layers());
}

// ----- uploading while downloading -----------------------------------------

/// A libtorrent leecher that can reach only EmpireBT must get the pieces
/// EmpireBT downloads from a libtorrent seed before EmpireBT has finished.
/// Here the leecher connects to EmpireBT's listener (over uTP, encrypted,
/// at libtorrent's defaults), which hands it to the download.
#[test]
fn relays_pieces_to_peers_that_connect_while_downloading() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("relay-incoming");
    // Private, so peer exchange cannot introduce the leecher to the seed.
    let (torrent, info) = private_album_torrent(&lt, &scratch, Format::Hybrid);
    let seed = lt
        .with_upload_limit(600_000)
        .seed(&torrent, &scratch.join("source"));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let runtime = Arc::new(
        Runtime::new_mixed(Vec::new(), Vec::new(), 16)
            .unwrap()
            .with_utp(Some(UtpSocket::bind(address).unwrap())),
    );
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = {
        let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || runtime.serve_until(&listener, &shutdown).unwrap())
    };
    let leecher = lt.download_in_background(&torrent, &scratch.join("leecher"), address);

    let storage = Storage::new(scratch.join("get")).unwrap();
    let session = DownloadSession::new(info, storage, generate_peer_id(), TIMEOUT)
        .with_runtime(Some(Arc::clone(&runtime)))
        .with_listen_port(address.port());
    session
        .download_from_peers(&[seed.address], 4)
        .unwrap_or_else(|error| panic!("{error}\n{}", seed.log()));
    let relayed = leecher.progress();
    shutdown.store(true, Ordering::Release);
    server.join().unwrap();
    assert!(
        relayed >= 50.0,
        "the leecher got only {relayed}% from us:\n{}",
        leecher.log()
    );
    assert_relayed_most(&session);
}

/// EmpireBT uploaded at least half the torrent: the leecher's data came
/// through it, not around it.
fn assert_relayed_most(session: &DownloadSession) {
    let progress = session.progress();
    assert!(
        progress.uploaded_bytes() * 2 >= progress.total_bytes(),
        "uploaded only {} of {} bytes",
        progress.uploaded_bytes(),
        progress.total_bytes()
    );
}

/// A finished download keeps its connections and seeds on them: the
/// leecher, which can reach only EmpireBT, finishes after EmpireBT does.
/// Before downloads shared the runtime, finishing closed every connection.
#[test]
fn keeps_seeding_to_its_peers_after_finishing_a_download() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("keep-seeding");
    // Private, so peer exchange cannot introduce the leecher to the seed.
    let (torrent, info) = private_album_torrent(&lt, &scratch, Format::V1);
    let seed = lt.seed(&torrent, &scratch.join("source"));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let runtime = Arc::new(Runtime::new_mixed(Vec::new(), Vec::new(), 16).unwrap());
    let shutdown = Arc::new(AtomicBool::new(false));
    // The event loop's thread starts late, as on a busy machine; the
    // session must still use this runtime rather than one of its own,
    // which nothing would seed from once the download completes.
    let server = {
        let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            runtime.serve_until(&listener, &shutdown).unwrap()
        })
    };
    let leecher = lt.download_in_background(&torrent, &scratch.join("leecher"), address);

    let storage = Storage::new(scratch.join("get")).unwrap();
    let session = DownloadSession::new(info.clone(), storage, generate_peer_id(), TIMEOUT)
        .with_runtime(Some(Arc::clone(&runtime)))
        .with_listen_port(address.port());
    session
        .download_from_peers(&[seed.address], 4)
        .unwrap_or_else(|error| panic!("{error}\n{}", seed.log()));
    assert!(
        runtime.contains(info.info_hash()),
        "the torrent stays in the runtime"
    );
    assert_eq!(runtime.seeding_count(), 1);
    // Only we can serve it from here on.
    drop(seed);
    assert!(
        leecher.wait_complete(Duration::from_secs(40)),
        "the leecher never finished from us:\n{}",
        leecher.log()
    );
    shutdown.store(true, Ordering::Release);
    server.join().unwrap();
}

/// The same, with EmpireBT connecting to the leecher itself.
#[test]
fn relays_pieces_to_peers_it_connects_to_while_downloading() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("relay-outgoing");
    let (torrent, info) = private_album_torrent(&lt, &scratch, Format::V1);
    let seed = lt
        .with_upload_limit(600_000)
        .seed(&torrent, &scratch.join("source"));
    // The leecher's only other peer does not exist.
    let nowhere = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let leecher = lt.download_in_background(&torrent, &scratch.join("leecher"), nowhere);
    let leecher_address = SocketAddr::from(([127, 0, 0, 1], leecher.address.port()));
    // Give it a moment to start listening.
    std::thread::sleep(Duration::from_secs(1));

    let storage = Storage::new(scratch.join("get")).unwrap();
    let session = DownloadSession::new(info, storage, generate_peer_id(), TIMEOUT);
    session
        .download_from_peers(&[seed.address, leecher_address], 4)
        .unwrap_or_else(|error| panic!("{error}\n{}", seed.log()));
    let relayed = leecher.progress();
    assert!(
        relayed >= 50.0,
        "the leecher got only {relayed}% from us:\n{}",
        leecher.log()
    );
    assert_relayed_most(&session);
}

// ----- peer exchange (BEP 11) ----------------------------------------------

/// EmpireBT, given only a libtorrent leecher, learns of that leecher's seed
/// through peer exchange and downloads from it too.
#[test]
fn learns_peers_from_libtorrent_through_peer_exchange() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("pex-learn");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);
    let seed = lt
        .with_upload_limit(400_000)
        .seed(&torrent, &scratch.join("source"));
    let leecher = lt.download_in_background(&torrent, &scratch.join("leecher"), seed.address);
    let leecher_address = SocketAddr::from(([127, 0, 0, 1], leecher.address.port()));
    // Let the leecher connect to the seed, so it has someone to name.
    let started = std::time::Instant::now();
    while leecher.progress() == 0.0 && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }

    let storage = Storage::new(scratch.join("get")).unwrap();
    let session = DownloadSession::new(info, storage, generate_peer_id(), TIMEOUT);
    session
        .download_from_peers(&[leecher_address], 4)
        .unwrap_or_else(|error| panic!("{error}\n{}", leecher.log()));
    let learned = session.peers_from_exchange();
    assert!(
        learned
            .iter()
            .any(|peer| peer.port() == seed.address.port()),
        "libtorrent's peer exchange named {learned:?}, not the seed at {}",
        seed.address
    );
}

/// A libtorrent leecher that knows only EmpireBT learns of EmpireBT's seed
/// through peer exchange, and finishes from it after EmpireBT has left.
#[test]
fn tells_libtorrent_about_peers_through_peer_exchange() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("pex-tell");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::V1);
    let seed = lt
        .with_upload_limit(600_000)
        .seed(&torrent, &scratch.join("source"));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let runtime = Arc::new(
        Runtime::new_mixed(Vec::new(), Vec::new(), 16)
            .unwrap()
            .with_utp(Some(UtpSocket::bind(address).unwrap())),
    );
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = {
        let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || runtime.serve_until(&listener, &shutdown).unwrap())
    };
    let leecher = lt.download_in_background(&torrent, &scratch.join("leecher"), address);

    let storage = Storage::new(scratch.join("get")).unwrap();
    let session = DownloadSession::new(info, storage, generate_peer_id(), TIMEOUT)
        .with_runtime(Some(Arc::clone(&runtime)))
        .with_listen_port(address.port());
    session
        .download_from_peers(&[seed.address], 4)
        .unwrap_or_else(|error| panic!("{error}\n{}", seed.log()));
    // We have left; the leecher can only finish from the seed it heard of.
    shutdown.store(true, Ordering::Release);
    server.join().unwrap();
    assert!(
        leecher.wait_complete(Duration::from_secs(40)),
        "the leecher never finished:\n{}",
        leecher.log()
    );
}

// ----- magnet links ---------------------------------------------------------

/// Which hashes a magnet link carries.
#[derive(Debug, Clone, Copy)]
enum MagnetKind {
    V1Only,
    V2Only,
    HybridBothHashes,
    HybridV1Only,
}

fn magnet_for(lt: &Libtorrent, torrent: &Path, kind: MagnetKind) -> String {
    let magnet = lt.magnet(torrent);
    match kind {
        // libtorrent writes btih first, then btmh; keep only the btih part.
        MagnetKind::HybridV1Only => magnet
            .split('&')
            .filter(|part| !part.starts_with("xt=urn:btmh:"))
            .collect::<Vec<_>>()
            .join("&"),
        MagnetKind::V1Only | MagnetKind::V2Only | MagnetKind::HybridBothHashes => magnet,
    }
}

fn format_of(kind: MagnetKind) -> Format {
    match kind {
        MagnetKind::V1Only => Format::V1,
        MagnetKind::V2Only => Format::V2,
        MagnetKind::HybridBothHashes | MagnetKind::HybridV1Only => Format::Hybrid,
    }
}

fn resolves_magnet_from_libtorrent(kind: MagnetKind) {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new(&format!("magnet-get-{kind:?}"));
    let (torrent, original) = album_torrent(&lt, &scratch, format_of(kind));
    let link = MagnetLink::parse(&magnet_for(&lt, &torrent, kind)).unwrap();
    let seeder = lt.seed(&torrent, &scratch.join("source"));

    let metadata = empirebt_backend::fetch_metadata_from_peer(
        seeder.address,
        &link,
        generate_peer_id(),
        TIMEOUT,
    )
    .unwrap_or_else(|error| panic!("fetching metadata failed: {error}\n{}", seeder.log()));
    let info = TorrentInfo::from_metadata_with_layers(&metadata.info, &metadata.piece_layers, &[])
        .unwrap();
    assert_eq!(info.info_hash(), original.info_hash());
    assert_eq!(
        info.v2_piece_layers(),
        original.v2_piece_layers(),
        "piece layers differ"
    );
    empire_download(&info, &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
}

fn serves_magnet_to_libtorrent(kind: MagnetKind) {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new(&format!("magnet-serve-{kind:?}"));
    let (torrent, info) = album_torrent(&lt, &scratch, format_of(kind));
    let magnet = magnet_for(&lt, &torrent, kind);
    let seeder = EmpireSeeder::start(info, &scratch.join("source"));
    lt.download(&magnet, &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
}

#[test]
fn resolves_v1_magnet_from_libtorrent() {
    resolves_magnet_from_libtorrent(MagnetKind::V1Only);
}

#[test]
fn resolves_v2_magnet_from_libtorrent() {
    resolves_magnet_from_libtorrent(MagnetKind::V2Only);
}

#[test]
fn resolves_hybrid_magnet_from_libtorrent() {
    resolves_magnet_from_libtorrent(MagnetKind::HybridBothHashes);
}

#[test]
fn resolves_hybrid_v1_magnet_from_libtorrent() {
    resolves_magnet_from_libtorrent(MagnetKind::HybridV1Only);
}

/// The common case: we are the only seeder and a magnet user has nothing
/// but the info hash, so the metadata can only come from us.
#[test]
fn serves_v1_magnet_to_libtorrent() {
    serves_magnet_to_libtorrent(MagnetKind::V1Only);
}

#[test]
fn serves_v2_magnet_to_libtorrent() {
    serves_magnet_to_libtorrent(MagnetKind::V2Only);
}

#[test]
fn serves_hybrid_magnet_to_libtorrent() {
    serves_magnet_to_libtorrent(MagnetKind::HybridBothHashes);
}

#[test]
fn serves_hybrid_v1_magnet_to_libtorrent() {
    serves_magnet_to_libtorrent(MagnetKind::HybridV1Only);
}

// ----- hashing --------------------------------------------------------------

/// EmpireBT must accept libtorrent's hashes for files of every awkward size:
/// around block and piece boundaries, single-block, and many-piece files.
/// That checks v2 Merkle roots, piece layers, and padding, plus v1 hashes
/// across hybrid padding files.
#[test]
fn verifies_libtorrent_hashes_for_many_file_sizes() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("sizes");
    let sizes = [
        1, 100, 16_383, 16_384, 16_385, 32_768, 65_535, 65_536, 65_537, 131_072, 200_000, 1_048_577,
    ];
    let source = scratch.join("source/sizes");
    for (index, size) in sizes.into_iter().enumerate() {
        write_file(
            &source.join(format!("file-{index:02}-{size}.bin")),
            size,
            100 + index as u64,
        );
    }
    let storage = Storage::new(scratch.join("source")).unwrap();
    for piece_size in [16 * 1024, 64 * 1024] {
        for format in [Format::V2, Format::Hybrid] {
            let torrent = scratch.join(&format!("sizes-{}-{piece_size}.torrent", format.name()));
            lt.make(&source, &torrent, format, piece_size);
            let info = TorrentInfo::from_file(&torrent).unwrap();
            let v2 = storage.scan_complete_v2_files(&info).unwrap();
            assert!(
                v2.iter().all(|ok| *ok),
                "{format:?} with {piece_size}-byte pieces: v2 files failed verification: {v2:?}"
            );
            if format == Format::Hybrid {
                let v1 = storage.scan_complete_pieces(&info).unwrap();
                assert!(
                    v1.iter().all(|ok| *ok),
                    "hybrid v1 pieces failed verification"
                );
            }
        }
    }
}

// ----- local peer discovery (BEP 14) ----------------------------------------

/// A torrent of one file with content no other test shares. Tests here
/// all meet on the same multicast group, so a shared torrent would let
/// them serve each other and pass for the wrong reason.
fn unique_torrent(lt: &Libtorrent, scratch: &Scratch) -> (PathBuf, TorrentInfo) {
    let source = scratch.join("source");
    std::fs::create_dir_all(&source).unwrap();
    let mut data = format!("{}", std::process::id()).into_bytes();
    data.extend(scratch.join("x").to_string_lossy().bytes());
    data.extend((0..300_000).map(|byte: u32| (byte * 31 % 251) as u8));
    std::fs::write(source.join("lsd.bin"), &data).unwrap();
    let torrent = scratch.join("lsd.torrent");
    lt.make(&source.join("lsd.bin"), &torrent, Format::V1, 64 * 1024);
    (torrent.clone(), TorrentInfo::from_file(&torrent).unwrap())
}

/// Starts local peer discovery, or `None` where multicast is not available.
fn local_discovery(port: u16) -> Option<Arc<Lsd>> {
    let lsd = Lsd::start(port).ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while lsd.interface_count() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if lsd.interface_count() == 0 {
        eprintln!("skipped: multicast is not available here");
        return None;
    }
    Some(lsd)
}

/// libtorrent hears our announce and connects to us: we are given no
/// address, no tracker, and no DHT.
#[test]
fn finds_a_libtorrent_seed_on_the_local_network() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("lsd-from-libtorrent");
    let (torrent, info) = unique_torrent(&lt, &scratch);
    let seeder = lt.with_lsd().seed(&torrent, &scratch.join("source"));

    // Our side listens, as the client does, so a peer that heard our
    // announce can connect.
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let Some(lsd) = local_discovery(port) else {
        return;
    };
    let runtime = Arc::new(Runtime::new(Vec::new(), 16).unwrap());
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = {
        let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || runtime.serve_until(&listener, &shutdown))
    };
    let storage = Storage::new(scratch.join("get")).unwrap();
    let session = DownloadSession::new(info, storage, generate_peer_id(), TIMEOUT)
        .with_lsd(Some(lsd))
        .with_runtime(Some(Arc::clone(&runtime)));
    let result = session.download_from_trackers_with_retries(port, 8, 4);
    shutdown.store(true, Ordering::Release);
    server.join().unwrap().unwrap();
    result.unwrap_or_else(|error| panic!("no download: {error}\n{}", seeder.log()));
    assert!(session.is_complete().unwrap());
    assert_same_files(&scratch.join("source"), &scratch.join("get"));
}

/// We hear libtorrent's announce and our seed connects to it: libtorrent
/// is given no address, no tracker, and no DHT.
#[test]
fn serves_a_libtorrent_download_found_on_the_local_network() {
    let Some(lt) = libtorrent() else { return };
    let scratch = Scratch::new("lsd-to-libtorrent");
    let (torrent, info) = unique_torrent(&lt, &scratch);
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let Some(lsd) = local_discovery(port) else {
        return;
    };
    let storage = Storage::new(scratch.join("source")).unwrap();
    let seeder = Seeder::new(info, storage, generate_peer_id(), TIMEOUT).unwrap();
    let runtime = Arc::new(Runtime::new(vec![seeder], 16).unwrap());
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = {
        let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || runtime.serve_until(&listener, &shutdown))
    };
    lsd.set_runtime(Some(Arc::clone(&runtime)));

    let output = lt.with_lsd().run_status(&[
        "download",
        path(&torrent),
        path(&scratch.join("get")),
        &free_port().to_string(),
        "none",
        &DOWNLOAD_SECONDS.to_string(),
    ]);
    shutdown.store(true, Ordering::Release);
    server.join().unwrap().unwrap();
    assert!(
        output.contains("COMPLETE"),
        "libtorrent did not finish:\n{output}"
    );
    assert!(lsd.peers_found() >= 1);
    assert_same_files(&scratch.join("source"), &scratch.join("get"));
}

// ----- IPv6 ------------------------------------------------------------------

fn ipv6_available() -> bool {
    let available = TcpListener::bind("[::1]:0").is_ok();
    if !available {
        eprintln!("skipped: no IPv6 here");
    }
    available
}

#[test]
fn downloads_from_libtorrent_over_ipv6() {
    let Some(lt) = libtorrent() else { return };
    if !ipv6_available() {
        return;
    }
    let scratch = Scratch::new("ipv6-from-libtorrent");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::Hybrid);
    let seeder = lt.with_ipv6().seed(&torrent, &scratch.join("source"));
    assert!(seeder.address.is_ipv6());
    empire_download(&info, &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
}

#[test]
fn serves_libtorrent_over_ipv6() {
    let Some(lt) = libtorrent() else { return };
    if !ipv6_available() {
        return;
    }
    let scratch = Scratch::new("ipv6-to-libtorrent");
    let (torrent, info) = album_torrent(&lt, &scratch, Format::Hybrid);
    let seeder = EmpireSeeder::start_at(
        info,
        &scratch.join("source"),
        Encryption::default(),
        "[::1]:0",
    );
    assert!(seeder.address.is_ipv6());
    lt.with_ipv6()
        .download(path(&torrent), &scratch.join("get"), seeder.address);
    assert_same_files(&scratch.join("source/album"), &scratch.join("get/album"));
}
