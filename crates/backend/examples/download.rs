//! Downloads a real torrent for a while and prints the speed, for measuring
//! the download engine against a live swarm.
//!
//! ```text
//! cargo run --release -p empirebt-backend --example download -- \
//!     <file.torrent> <dir> [SECONDS] [MAX_PEERS] [PORT] [LIMIT_KIB]
//! ```
//!
//! Uses trackers, the DHT, uTP, and encryption as the client does. The data
//! written to `<dir>` is left behind. `LIMIT_KIB` caps the download speed
//! (KiB/s; 0, the default, is unlimited).

use empirebt_backend::{
    ConnectOptions, Dht, DownloadSession, Storage, TorrentInfo, UtpSocket, generate_peer_id,
};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: download <file.torrent> <dir> [SECONDS] [MAX_PEERS] [PORT] [LIMIT_KIB]";
    let torrent = args.get(1).expect(usage);
    let dir = args.get(2).expect(usage);
    let seconds: u64 = args.get(3).map_or(60, |value| value.parse().expect(usage));
    let max_peers: usize = args.get(4).map_or(8, |value| value.parse().expect(usage));
    let port: u16 = args
        .get(5)
        .map_or(56881, |value| value.parse().expect(usage));
    let limit_kib: u64 = args.get(6).map_or(0, |value| value.parse().expect(usage));

    let info = TorrentInfo::from_file(torrent).expect("a readable torrent");
    let dht = Dht::start(port, Dht::default_bootstrap(), None).expect("the DHT port is free");
    let utp = UtpSocket::on_dht(&dht).expect("uTP shares the DHT port");
    let storage = Storage::new(dir).expect("a usable directory");
    let session = DownloadSession::new(info, storage, generate_peer_id(), Duration::from_secs(10))
        .with_dht(Some(dht))
        .with_connect_options(ConnectOptions::new().with_utp(Some(utp)))
        .with_listen_port(port)
        .with_limits(std::sync::Arc::new({
            let limits = empirebt_backend::Limits::unlimited();
            limits.set_download_rate(limit_kib * 1024);
            limits
        }));
    let progress = std::sync::Arc::clone(session.progress());
    let worker = std::thread::spawn(move || {
        session.download_from_trackers_with_retries(port, max_peers, usize::MAX)
    });

    let started = Instant::now();
    let mut last = (0, Instant::now());
    while started.elapsed() < Duration::from_secs(seconds) && !worker.is_finished() {
        std::thread::sleep(Duration::from_secs(5));
        let received = progress.received_bytes();
        let rate = (received - last.0) as f64 / last.1.elapsed().as_secs_f64();
        last = (received, Instant::now());
        println!(
            "{:5.0}s  {:6.2} MB/s  peers={:3}  verified={} MB  phase={:?}  error={:?}",
            started.elapsed().as_secs_f64(),
            rate / 1e6,
            progress.active_peers(),
            progress.verified_bytes() / 1_000_000,
            progress.phase(),
            progress.last_error(),
        );
    }
    progress.cancel();
    let _ = worker.join();
}
