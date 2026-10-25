//! Seeds thousands of local torrents and measures what limits scale:
//! startup, session saves, memory, open files, block-read throughput, and
//! how many torrents trackers and the DHT actually hear about.
//!
//! ```text
//! cargo run --release -p empirebt-backend --example scale -- [TORRENTS] [DIR] [PEERS]
//! ```
//!
//! Every torrent lists a working local tracker and a "black hole" tracker
//! that accepts connections but never answers, as dead trackers in real
//! torrents do. The DHT bootstrap list likewise includes a node that never
//! replies, so lookups take realistic time.

use empirebt_backend::{Client, ClientConfig, Dht, SeedAnnouncer, Storage, TransferState};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FILE_SIZE: usize = 32 * 1024;
const PIECE: usize = 16 * 1024;
const COVERAGE_WINDOW: Duration = Duration::from_secs(60);
/// Real .torrent files average tens of KB, mostly piece hashes. Each test
/// torrent carries this much extra metadata so memory, session, and clone
/// costs are realistic while the data files stay small.
const METADATA_PADDING: usize = 40 * 1024;

fn main() {
    // Load-only mode, run in a fresh process so memory figures are not
    // inflated by allocations from earlier phases.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--load-client") {
        return load_client(&args[2..]);
    }
    if args.get(1).map(String::as_str) == Some("--measure-load") {
        let config = ClientConfig::new().with_download_directory(&args[3]);
        let client = Client::load_session(config, &args[2]).unwrap();
        let runtime = client
            .seeding_runtime([7; 20], Duration::from_secs(10))
            .unwrap();
        println!("{}", rss());
        drop((client, runtime));
        return;
    }
    let mut args = std::env::args().skip(1);
    let count: usize = args
        .next()
        .map_or(2_000, |arg| arg.parse().expect("torrent count"));
    let root = args.next().map_or_else(
        || std::env::temp_dir().join("empirebt-scale"),
        std::path::PathBuf::from,
    );
    let peers: usize = args
        .next()
        .map_or(LOAD_PEERS, |arg| arg.parse().expect("peer count"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();

    let live = start_tracker();
    let dead = start_black_hole_tracker();
    println!(
        "EmpireBT scale benchmark: {count} torrents in {}\n",
        root.display()
    );

    let started = Instant::now();
    let torrents: Vec<Vec<u8>> = (0..count)
        .map(|index| make_torrent(&data, index, &live.url, &dead))
        .collect();
    report("create files and .torrent metadata", started.elapsed(), "");

    let config = ClientConfig::new().with_download_directory(&data);
    let mut client = Client::with_config(config.clone()).unwrap();
    for bytes in &torrents {
        client.add_torrent(empirebt_backend::TorrentInfo::from_bytes(bytes).unwrap());
    }
    let started = Instant::now();
    for index in 0..count {
        client.start_torrent(index).unwrap();
    }
    let seeding = client
        .torrents()
        .iter()
        .filter(|torrent| torrent.state() == TransferState::Seeding)
        .count();
    report(
        "verify all local data",
        started.elapsed(),
        &format!("{seeding} seeding"),
    );

    let session = root.join("session.dat");
    let started = Instant::now();
    client.save_session(&session).unwrap();
    let size = std::fs::metadata(&session).map(|m| m.len()).unwrap_or(0);
    report(
        "save session (every change rewrites this)",
        started.elapsed(),
        &format!("{:.1} MiB written", size as f64 / 1_048_576.0),
    );
    save_session_dir_benchmark(&client, &root);

    // Measure memory with only the loaded session alive.
    // Room for the connection load test.
    let config = client.config().clone().with_max_connections(10_000);
    drop(client);
    drop(torrents);
    let started = Instant::now();
    let client = Client::load_session(config, &session).unwrap();
    report("load session at startup", started.elapsed(), "");
    let measured = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--measure-load",
            session.to_str().unwrap(),
            data.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    report(
        "memory: loaded session + seeding runtime",
        Duration::ZERO,
        String::from_utf8_lossy(&measured.stdout).trim(),
    );

    let started = Instant::now();
    let copy = client.clone();
    report("clone client (each runtime rebuild)", started.elapsed(), "");
    drop(copy);
    let started = Instant::now();
    let runtime = Arc::new(
        client
            .seeding_runtime([7; 20], Duration::from_secs(10))
            .unwrap(),
    );
    report(
        "build seeding runtime",
        started.elapsed(),
        &format!("{} seeders", runtime.torrent_count()),
    );

    read_benchmark(&data, &client);
    connection_benchmark(&runtime, count, peers);
    coverage_benchmark(&client, runtime, &live);
    let _ = std::fs::remove_dir_all(&root);
}

/// Measures incremental session saves when the backend supports them.
fn save_session_dir_benchmark(client: &Client, root: &std::path::Path) {
    let dir = root.join("session");
    let started = Instant::now();
    client.save_session_dir(&dir).unwrap();
    report("save session directory (first save)", started.elapsed(), "");
    let started = Instant::now();
    client.save_session_dir(&dir).unwrap();
    let index = std::fs::metadata(dir.join("index"))
        .map(|m| m.len())
        .unwrap_or(0);
    report(
        "save session directory (a later change)",
        started.elapsed(),
        &format!("{:.1} KiB index rewritten", index as f64 / 1024.0),
    );
    let started = Instant::now();
    let (loaded, warnings) = Client::load_session_dir(client.config().clone(), &dir).unwrap();
    assert_eq!(loaded.torrents().len(), client.torrents().len());
    assert!(warnings.is_empty(), "{warnings:?}");
    report("load session directory", started.elapsed(), "");
}

/// Random 16 KiB block reads from 8 threads: across every torrent (the
/// worst case for a file cache), and across a "hot" set of 200 torrents,
/// closer to real seeding where peers concentrate on popular torrents.
fn read_benchmark(data: &std::path::Path, client: &Client) {
    let infos: Vec<_> = client
        .torrents()
        .iter()
        .filter_map(|torrent| torrent.info().cloned())
        .collect();
    read_pass(data, &infos, "random 16 KiB reads, all torrents");
    read_pass(
        data,
        &infos[..200.min(infos.len())],
        "random 16 KiB reads, 200 hot torrents",
    );
}

fn read_pass(data: &std::path::Path, infos: &[empirebt_backend::TorrentInfo], label: &str) {
    let storage = Storage::new(data).unwrap();
    let stop = AtomicBool::new(false);
    let reads = AtomicU64::new(0);
    let peak_fds = AtomicUsize::new(open_fds());
    let duration = Duration::from_secs(3);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                peak_fds.fetch_max(open_fds(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        for worker in 0..8_u64 {
            let (storage, stop, reads) = (&storage, &stop, &reads);
            scope.spawn(move || {
                let mut seed = worker * 7_919 + 1;
                while !stop.load(Ordering::Relaxed) {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let info = &infos[(seed >> 33) as usize % infos.len()];
                    let block = (seed >> 13) % (FILE_SIZE / PIECE) as u64;
                    let bytes = storage
                        .read_range(info, block * PIECE as u64, PIECE as u64)
                        .unwrap();
                    assert_eq!(bytes.len(), PIECE);
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        std::thread::sleep(duration);
        stop.store(true, Ordering::Relaxed);
    });
    let per_second = reads.load(Ordering::Relaxed) as f64 / duration.as_secs_f64();
    report(
        label,
        duration,
        &format!(
            "{:.0} reads/s, peak {} open files",
            per_second,
            peak_fds.load(Ordering::Relaxed)
        ),
    );
}

/// Peers connected at once in the load test, unless given on the command
/// line.
const LOAD_PEERS: usize = 2_000;
const LOAD_SECONDS: u64 = 15;

/// Serves many simultaneous peers, each repeatedly requesting blocks. The
/// peers run in a separate process, so this process's thread count is the
/// server's alone.
fn connection_benchmark(runtime: &Arc<empirebt_backend::Runtime>, torrents: usize, peers: usize) {
    let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let threads_before = threads();
    let server = {
        let (runtime, shutdown) = (Arc::clone(runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || runtime.serve_until(&listener, &shutdown))
    };
    let hashes: Vec<String> = runtime
        .info_hashes()
        .iter()
        .take(torrents)
        .map(|hash| hash.iter().map(|b| format!("{b:02x}")).collect())
        .collect();
    let uploaded = || -> u64 { runtime.uploads().values().sum() };
    let before = uploaded();
    let started = Instant::now();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--load-client")
        .arg(port.to_string())
        .arg(peers.to_string())
        .arg(LOAD_SECONDS.to_string())
        .arg(hashes.join(","))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (mut peak_connections, mut peak_threads) = (0, 0);
    while child.try_wait().unwrap().is_none() {
        peak_connections = peak_connections.max(runtime.connections());
        peak_threads = peak_threads.max(threads());
        std::thread::sleep(Duration::from_millis(50));
    }
    let elapsed = started.elapsed();
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    shutdown.store(true, Ordering::Release);
    server.join().unwrap().unwrap();
    let sent = uploaded() - before;
    println!();
    report(
        &format!("serve {peers} peers requesting blocks"),
        elapsed,
        output.trim(),
    );
    report(
        "  peak simultaneous connections",
        Duration::ZERO,
        &peak_connections.to_string(),
    );
    report(
        "  server threads (excluding the benchmark's)",
        Duration::ZERO,
        &format!("{}", peak_threads.saturating_sub(threads_before + 1)),
    );
    report(
        "  upload throughput",
        Duration::ZERO,
        &format!(
            "{:.0} MiB/s",
            sent as f64 / 1_048_576.0 / elapsed.as_secs_f64()
        ),
    );
}

/// Load-test peers: each connects, then requests blocks until time is up.
fn load_client(args: &[String]) {
    let port: u16 = args[0].parse().unwrap();
    let peers: usize = args[1].parse().unwrap();
    let seconds: u64 = args[2].parse().unwrap();
    let hashes: Vec<[u8; 20]> = args[3]
        .split(',')
        .map(|hex| std::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    // Failures by stage: connecting, waiting for unchoke, requesting.
    let (connected, failed, blocks) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new([const { AtomicUsize::new(0) }; 3]),
        Arc::new(AtomicU64::new(0)),
    );
    let workers: Vec<_> = (0..peers)
        .map(|index| {
            let (connected, failed, blocks, hash) = (
                Arc::clone(&connected),
                Arc::clone(&failed),
                Arc::clone(&blocks),
                hashes[index % hashes.len()],
            );
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    // Spread connection attempts over the first two seconds,
                    // as peers arriving from a swarm would.
                    std::thread::sleep(Duration::from_millis((index % 2_000) as u64));
                    let mut peer_id = [0; 20];
                    peer_id[..8].copy_from_slice(&(index as u64).to_be_bytes());
                    let address = format!("127.0.0.1:{port}").parse().unwrap();
                    let handshake = empirebt_backend::Handshake::new(hash, peer_id);
                    // Patient like a remote client: a burst of thousands of
                    // connections overflows the accept queue, and dropped
                    // SYNs are retried by the kernel after 1, 3, 7 s...
                    let options = empirebt_backend::ConnectOptions::new()
                        .with_dial_timeout(Duration::from_secs(20));
                    let Ok(mut connection) = empirebt_backend::PeerConnection::connect_with(
                        address,
                        &handshake,
                        Duration::from_secs(20),
                        &options,
                    ) else {
                        failed[0].fetch_add(1, Ordering::Relaxed);
                        return;
                    };
                    connected.fetch_add(1, Ordering::Relaxed);
                    // Like a real peer: ask, then wait to be unchoked.
                    if connection
                        .send(&empirebt_backend::PeerMessage::Interested)
                        .is_err()
                    {
                        failed[1].fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    loop {
                        match connection.receive() {
                            Ok(empirebt_backend::PeerMessage::Unchoke) => break,
                            Ok(_) => {}
                            Err(_) => {
                                failed[1].fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                    let mut piece = 0;
                    while Instant::now() < deadline {
                        piece = (piece + 1) % 2;
                        let request = empirebt_backend::PeerMessage::Request {
                            piece_index: piece,
                            begin: 0,
                            length: PIECE as u32,
                        };
                        if connection.send(&request).is_err() {
                            failed[2].fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        loop {
                            match connection.receive() {
                                Ok(empirebt_backend::PeerMessage::Piece { block, .. }) => {
                                    assert_eq!(block.len(), PIECE);
                                    blocks.fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    failed[2].fetch_add(1, Ordering::Relaxed);
                                    return;
                                }
                            }
                        }
                    }
                })
                .unwrap()
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    let failed: Vec<usize> = failed
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .collect();
    let total: usize = failed.iter().sum();
    let stages = if total == 0 {
        String::new()
    } else {
        format!(
            " ({} connecting, {} awaiting unchoke, {} requesting)",
            failed[0], failed[1], failed[2]
        )
    };
    println!(
        "{} connected, {total} failed{stages}, {} blocks served",
        connected.load(Ordering::Relaxed),
        blocks.load(Ordering::Relaxed)
    );
}

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |entries| entries.count())
}

/// How many torrents trackers and the DHT hear about within a minute.
fn coverage_benchmark(client: &Client, runtime: Arc<empirebt_backend::Runtime>, live: &Tracker) {
    let boot_port = free_udp_port();
    let boot = Dht::start(boot_port, Vec::new(), None).unwrap();
    // Bound but never read: queries to it simply go unanswered.
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let dht_port = free_udp_port();
    let dht = Dht::start(
        dht_port,
        vec![
            format!("127.0.0.1:{boot_port}"),
            silent.local_addr().unwrap().to_string(),
        ],
        None,
    )
    .unwrap();
    let mut announcer =
        SeedAnnouncer::start(6_881, Duration::from_secs(10), Some(Arc::clone(&dht)));
    announcer.track(runtime);
    let hashes: Vec<[u8; 20]> = client
        .torrents()
        .iter()
        .filter_map(|torrent| torrent.info_hash().copied())
        .collect();
    let started = Instant::now();
    println!("\n  announce coverage, of {} torrents:", hashes.len());
    println!("  {:>6}  {:>10}  {:>10}", "time", "trackers", "DHT");
    while started.elapsed() < COVERAGE_WINDOW {
        std::thread::sleep(Duration::from_secs(10));
        let tracker = live.announced.lock().unwrap().len();
        let in_dht = hashes
            .iter()
            .filter(|hash| {
                announcer
                    .status(hash)
                    .is_some_and(|status| status.dht_announced.is_some())
            })
            .count();
        println!(
            "  {:>5}s  {:>10}  {:>10}",
            started.elapsed().as_secs(),
            tracker,
            in_dht
        );
    }
    drop(announcer.stop());
    dht.stop();
    boot.stop();
}

struct Tracker {
    url: String,
    announced: Arc<Mutex<HashSet<String>>>,
}

/// A minimal HTTP tracker that records which info hashes announced.
fn start_tracker() -> Tracker {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let announced = Arc::new(Mutex::new(HashSet::new()));
    let record = Arc::clone(&announced);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let record = Arc::clone(&record);
            std::thread::spawn(move || {
                let mut request = [0; 2048];
                let size = stream.read(&mut request).unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..size]);
                if let Some(hash) = text
                    .split(['?', '&', ' '])
                    .find_map(|part| part.strip_prefix("info_hash="))
                {
                    record.lock().unwrap().insert(hash.to_owned());
                }
                let body = b"d8:intervali1800e5:peers0:e";
                let _ = stream.write_all(
                    format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                );
                let _ = stream.write_all(body);
            });
        }
    });
    Tracker { url, announced }
}

/// Accepts connections and never answers.
fn start_black_hole_tracker() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    url
}

fn make_torrent(data: &std::path::Path, index: usize, live: &str, dead: &str) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let name = format!("torrent-{index:05}.bin");
    let content: Vec<u8> = (0..FILE_SIZE)
        .map(|byte| (byte.wrapping_mul(31) ^ index.wrapping_mul(131)) as u8)
        .collect();
    std::fs::write(data.join(&name), &content).unwrap();
    let pieces: Vec<u8> = content
        .chunks(PIECE)
        .flat_map(|piece| Sha1::digest(piece).to_vec())
        .collect();
    let text = |value: &str| format!("{}:{value}", value.len());
    let mut bytes = format!(
        "d8:announce{}13:announce-listl{}{}e4:infod6:lengthi{FILE_SIZE}e4:name{}12:piece lengthi{PIECE}e6:pieces{}:",
        text(live),
        format_args!("l{}e", text(live)),
        format_args!("l{}e", text(dead)),
        text(&name),
        pieces.len()
    )
    .into_bytes();
    bytes.extend(pieces);
    // Unknown info keys are allowed; this one stands in for the piece
    // hashes of a multi-gigabyte torrent.
    bytes.extend(format!("9:x-padding{METADATA_PADDING}:").as_bytes());
    bytes.extend((0..METADATA_PADDING).map(|byte| (byte ^ index) as u8));
    bytes.extend(b"ee");
    bytes
}

fn report(what: &str, elapsed: Duration, detail: &str) {
    let time = if elapsed.is_zero() {
        String::new()
    } else if elapsed < Duration::from_millis(10) {
        format!("{:.2} ms", elapsed.as_secs_f64() * 1000.0)
    } else {
        format!("{:.0} ms", elapsed.as_secs_f64() * 1000.0)
    };
    println!("  {what:<44} {time:>10}  {detail}");
}

fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmRSS:"))
                .map(|line| line.trim_start_matches("VmRSS:").trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |entries| entries.count())
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
