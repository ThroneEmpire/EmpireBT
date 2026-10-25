//! Measures how fast EmpireBT moves data on this machine, with no network
//! in the way: hashing (a recheck), one seed to one downloader over
//! localhost (TCP or uTP, plain or encrypted), and one seed to several
//! downloaders at once. Everything is EmpireBT, so the numbers show what
//! our own code costs.
//!
//! ```text
//! cargo run --release -p empirebt-backend --example throughput -- [MIB] [PIECE_KIB] [DOWNLOADERS]
//! ```
//!
//! `ONLY=utp` (or any part of a test's name) runs just the tests whose
//! names contain it, for profiling one at a time.

use empirebt_backend::{
    Client, ClientConfig, ConnectOptions, DownloadSession, Encryption, Runtime, Seeder, Storage,
    TorrentInfo, UtpSocket,
};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|arg| arg.parse().expect("arguments are numbers"))
        .collect();
    let mib = args.first().copied().unwrap_or(1024);
    let piece = args.get(1).copied().unwrap_or(1024) * 1024;
    let downloaders = args.get(2).copied().unwrap_or(4);
    let root = std::env::temp_dir().join(format!("empirebt-throughput-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let seed_dir = root.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();

    let started = Instant::now();
    let info = make_torrent(&seed_dir, mib * 1024 * 1024, piece);
    println!(
        "{mib} MiB in {} KiB pieces, written in {:.1} s\n",
        piece / 1024,
        started.elapsed().as_secs_f64()
    );

    let only = std::env::var("ONLY").unwrap_or_default().to_lowercase();
    let wanted = |name: &str| name.to_lowercase().contains(&only);

    // A recheck: every piece read and hashed.
    if wanted("recheck") {
        let mut client =
            Client::with_config(ClientConfig::new().with_download_directory(&seed_dir)).unwrap();
        let index = client.add_torrent(info.clone());
        let started = Instant::now();
        let handle = client.spawn_verify(index).unwrap();
        let verified = client.finish_transfer(handle).unwrap();
        assert_eq!(verified, info.piece_count(), "the seed's data is complete");
        report("recheck", mib, started.elapsed());
    }

    for (name, encryption, utp) in [
        ("TCP, plain", Encryption::Disabled, false),
        ("TCP, encrypted", Encryption::Required, false),
        ("uTP, plain", Encryption::Disabled, true),
        ("uTP, encrypted", Encryption::Required, true),
    ] {
        if !wanted(&format!("1 downloader, {name}")) {
            continue;
        }
        let elapsed = transfer(&root, &seed_dir, &info, encryption, utp, 1);
        report(&format!("1 downloader, {name}"), mib, elapsed);
    }
    let name = format!("{downloaders} downloaders at once, TCP, plain (seed's upload)");
    if wanted(&name) {
        let elapsed = transfer(
            &root,
            &seed_dir,
            &info,
            Encryption::Disabled,
            false,
            downloaders,
        );
        report(&name, mib * downloaders, elapsed);
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn report(what: &str, mib: usize, elapsed: Duration) {
    println!(
        "{what:<55} {:>8.0} MiB/s  ({:.2} s)",
        mib as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );
}

/// Writes deterministic data and returns a single-file torrent for it.
fn make_torrent(dir: &Path, size: usize, piece: usize) -> TorrentInfo {
    use sha1::{Digest, Sha1};
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut data = vec![0u8; size];
    for chunk in data.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    std::fs::write(dir.join("data.bin"), &data).unwrap();
    let mut pieces = Vec::with_capacity(size / piece * 20 + 20);
    for chunk in data.chunks(piece) {
        pieces.extend(Sha1::digest(chunk));
    }
    let mut bytes = format!(
        "d4:infod6:lengthi{size}e4:name8:data.bin12:piece lengthi{piece}e6:pieces{}:",
        pieces.len()
    )
    .into_bytes();
    bytes.extend(pieces);
    bytes.extend(b"ee");
    TorrentInfo::from_bytes(&bytes).unwrap()
}

/// Seeds `info` from `seed_dir` and times `downloaders` downloads of it,
/// all at once, each into a fresh folder.
fn transfer(
    root: &Path,
    seed_dir: &Path,
    info: &TorrentInfo,
    encryption: Encryption,
    utp: bool,
    downloaders: usize,
) -> Duration {
    let seeder = Seeder::new(
        info.clone(),
        Storage::new(seed_dir).unwrap(),
        [1; 20],
        TIMEOUT,
    )
    .unwrap();
    let runtime = Runtime::new(vec![seeder], 1024)
        .unwrap()
        .with_encryption(encryption);
    let listener = runtime.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let runtime = Arc::new(if utp {
        runtime.with_utp(Some(UtpSocket::bind(address).unwrap()))
    } else {
        runtime
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = {
        let (runtime, shutdown) = (Arc::clone(&runtime), Arc::clone(&shutdown));
        std::thread::spawn(move || runtime.serve_until(&listener, &shutdown))
    };

    let started = Instant::now();
    std::thread::scope(|scope| {
        for number in 0..downloaders {
            let dir = root.join(format!("download-{number}"));
            let _ = std::fs::remove_dir_all(&dir);
            scope.spawn(move || {
                let mut options = ConnectOptions::new().with_encryption(encryption);
                if utp {
                    options = options
                        .with_utp(Some(
                            UtpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap(),
                        ))
                        .prefer_utp(true);
                }
                let mut peer_id = [2; 20];
                peer_id[0] = number as u8;
                let session = DownloadSession::new(
                    info.clone(),
                    Storage::new(&dir).unwrap(),
                    peer_id,
                    TIMEOUT,
                )
                .with_connect_options(options);
                let pieces = session.download_from_peers(&[address], 1).unwrap();
                assert_eq!(pieces, info.piece_count(), "downloaded everything");
                let _ = std::fs::remove_dir_all(&dir);
            });
        }
    });
    let elapsed = started.elapsed();
    shutdown.store(true, std::sync::atomic::Ordering::Release);
    server.join().unwrap().unwrap();
    elapsed
}
