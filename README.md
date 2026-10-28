# EmpireBT

EmpireBT is a BitTorrent client written in Rust, built to download fast and
to seed thousands of torrents. It comes with a full-screen terminal
dashboard and a web UI that also works on phones. Both are built on a
reusable backend library.

It handles BitTorrent v1, v2 (BEP 52) and hybrid torrents, from `.torrent`
files, `.torrent` URLs, or magnet links. Every format is tested against
libtorrent, the engine behind qBittorrent and Deluge, in both directions.

## Highlights

- **Fast:** about 100 MB/s on a busy public swarm within 20 seconds,
  against about 33 MB/s for libtorrent with default settings on the same
  connection ([Speed](#speed)).
- **Built for scale:** 2,000 torrents seeded and announced within a
  minute, 5,000 peers served at once on four threads, and live web updates
  that stay small with thousands of torrents ([Scale](#scale)).
- **A good swarm member:** it uploads while downloading and uses
  tit-for-tat choking. It supports the Fast Extension, peer exchange, uTP,
  and encryption on by default, so other clients connect to it easily.
- **Finds peers everywhere:** HTTP, HTTPS and UDP trackers, the DHT, peer
  exchange, and local peer discovery, over IPv4 and IPv6.
- **Stays inside your VPN:** it can bind every connection to one network
  interface, and connects nothing while that interface is down. An
  optional anonymous mode stops it telling peers and trackers it is
  EmpireBT ([Privacy](#privacy)).
- **Two interfaces, one engine:** a terminal dashboard and a secure web UI
  can run in the same process, showing the same torrents. Torrents can be
  organized with categories and tags, and each peer is shown with its
  country.
- **Your names, on disk:** rename a torrent, or its files and folders,
  even while it seeds. When adding a torrent, choose to put it in a folder
  of its own or leave out its top folder.

## Getting started

Build and install the `empirebt` binary (Rust 1.89 or newer):

```bash
cargo install --path crates/console
```

Or run it straight from the source tree:

```bash
cargo run --release -p empirebt-console -- [COMMAND]
```

| Command | What it does |
| --- | --- |
| `empirebt` | Opens the terminal dashboard |
| `empirebt ui --web` | Opens the dashboard and serves the web UI too |
| `empirebt serve` | Serves the web UI without the dashboard, until Ctrl-C |
| `empirebt add <files, URLs, or magnets…>` | Adds torrents without opening anything (`-d <folder>` saves them there; `--layout nosubfolder` leaves out the top folder, `--layout subfolder` puts a single file in a folder) |
| `empirebt list` / `empirebt remove <number>` | Lists or removes torrents (files stay on disk; `--delete-files` deletes them) |
| `empirebt config` / `empirebt config set <key> <value>` | Shows or changes settings |
| `empirebt web password` / `token <name>` / `revoke-tokens` | Manages access to the web UI (works while EmpireBT runs) |
| `empirebt health` | Succeeds if a running EmpireBT's web UI answers (for health checks) |
| `empirebt import qbittorrent` | Brings torrents over from a running qBittorrent (see [Moving from qBittorrent](#moving-from-qbittorrent)) |

To run it in Docker instead, see [Docker](#docker).

Your torrent list is saved after every change. When EmpireBT starts again,
finished torrents go straight back to seeding. Unfinished downloads resume
where they stopped, without re-hashing, and paused torrents stay paused.
Torrents are shared only while EmpireBT is running.

## Moving from qBittorrent

With qBittorrent running and its Web UI on (Options → Web UI), and
EmpireBT closed:

```bash
empirebt import qbittorrent --url http://127.0.0.1:8080 --username admin --dry-run
empirebt import qbittorrent --url http://127.0.0.1:8080 --username admin
```

The import only reads from qBittorrent's Web API and never changes
anything in qBittorrent. It asks for the password, shows what it will do,
and imports once you confirm.

**What comes over:**
- **Every torrent, where its files are now.** That includes unfinished ones
  in qBittorrent's incomplete folder.
- **Labels and state:** each torrent's category, tags, and whether it was
  stopped or running.
- **Progress, without a recheck.** qBittorrent's verified pieces are
  trusted; use `--recheck` to check everything instead.
- **Categories (with their folders) and tags.**
- **Names:** files renamed or moved in qBittorrent, torrents laid out
  without their top folder (or in a folder of their own), and names given
  to torrents.
- **Magnet links whose metadata never arrived.**

**What doesn't, yet:**
- **Torrents with skipped files come over paused,** because EmpireBT would
  download every file. Pieces that touch a skipped file are downloaded
  again, since qBittorrent may keep part of them outside the files.
- **Share limits are noted but not kept.**

If qBittorrent sees the files under other paths than this computer does,
as in Docker, add `--map /downloads=/srv/torrents`. Once imported, stop
those torrents in qBittorrent (or close it) before opening EmpireBT.

## Terminal dashboard

The dashboard lists every torrent with live progress, speeds, peers and
time left. A detail pane has four tabs, switched with `1`–`4`: overview,
files, peers (address, country, client, connection, speeds) and trackers.

| Key | Action |
| --- | --- |
| `a` | Add a `.torrent` file, URL, or magnet link, and choose its folder and content layout (↑↓ switch fields, Tab completes paths, ←→ change the layout; pasting works) |
| `s` / `p` | Start / pause |
| `v` | Recheck the downloaded data |
| `A` | Reannounce now: ask trackers and the DHT for peers instead of waiting |
| `x` | Remove; `d` in the dialog deletes the downloaded files too |
| `C` / `t` | Set the category / edit the tags (Tab completes known names) |
| `R` | Rename the torrent (only the name shown; empty shows its own again) |
| `r` | Rename a file or folder on disk: pick it, then type its new name (`/` moves it into a folder) |
| `f` / `Esc` | Show one category or tag / show everything again |
| `S` | Sort the list: by upload or download speed, size, progress, peers, time left, name |
| `c` | Settings |
| `?` | Every shortcut |

## Web UI

`empirebt serve` or `empirebt ui --web` serves a browser interface. By
default it listens on `127.0.0.1:8080`, on this computer only. It prints a
one-time login link, and on the very first start a password.

The web UI shows the same torrents and actions as the dashboard. It sorts
and filters thousands of torrents smoothly, shows a peer list with country
flags, and lets you choose its theme and accent colour. Its Files tab
shows a torrent's folders and files; the ✎ next to each renames it, and
the one by the name renames the torrent. On a phone it
switches to a touch layout.

Signing in is always required. Any address other than loopback is served
over HTTPS only, and a browser session can't reach the file system.

- [WEB_UI.md](WEB_UI.md) covers remote access, passwords, and how the web
  UI is protected.
- [WEB_API.md](WEB_API.md) is the JSON API, for scripts with an API token.

## Docker

```bash
mkdir -p data downloads
docker compose up -d
docker compose logs empirebt    # the sign-in link and first password
```

The image is small (about 33 MB: our binary on distroless, run as a
non-root user) and checks its own health. Any setting can be passed as an
environment variable, such as `EMPIREBT_NETWORK_UPLOAD_LIMIT=500`. There's
also a ready-made setup for running behind a VPN with gluetun, which
follows the VPN's forwarded port. See
[DOCKER.md](DOCKER.md) for ports, folders, reverse proxies and the VPN.

## Settings

Settings live in `config.toml` in the data directory
(`~/.local/share/empirebt/config.toml` by default). The file is created
with comments on first run. Change settings in any of three ways:

- In the dashboard: press `c`.
- From the command line, with `empirebt config set network.port 51413`.
- By editing the file while EmpireBT is closed. Your comments are kept.

| Setting | Default | Purpose |
| --- | --- | --- |
| `storage.download_dir` | `~/Downloads/EmpireBT` | The folder new torrents are saved in, unless you choose another when adding (existing ones stay where they are) |
| `storage.allowed_dirs` | empty | Extra folders the web UI may save torrents in (comma-separated); the dashboard can save anywhere |
| `network.port` | `6881` | Incoming peer port, over TCP and UDP (uTP and the DHT); changes apply at once |
| `network.port_file` | empty | A file holding the port to use instead, followed while running, such as a VPN's forwarded port ([DOCKER.md](DOCKER.md#port-forwarding)) |
| `network.max_peers` | `50` | Peers to download one torrent from at once |
| `network.max_connections` | `500` | Peer connections across all torrents, downloading and seeding (applies live) |
| `network.upload_limit` | `0` | Total upload speed in KiB/s; `0` is unlimited (applies live) |
| `network.download_limit` | `0` | Total download speed in KiB/s; `0` is unlimited (applies live) |
| `network.peer_timeout` | `10` | Seconds before an unresponsive peer or tracker is dropped |
| `network.utp` | `enabled` | uTP: `enabled` (TCP first), `preferred` (uTP first), or `disabled` |
| `network.encryption` | `enabled` | Peer encryption: `enabled` (falls back to plain), `required`, or `disabled` |
| `network.interface` | empty (any) | Bind every connection to one interface, by name (`wg0`) or address; nothing connects while it is down |
| `network.lsd` | `true` | Find peers on the local network (never for private torrents) |
| `network.anonymous` | `false` | Anonymous mode: public torrents don't say they are EmpireBT ([Privacy](#privacy)); applies live |
| `dht.enabled` | `true` | Find peers without trackers (never for private torrents) |
| `dht.bootstrap` | public routers | Nodes used to join the DHT |
| `peers.countries` | `true` | Show peers' countries, from the free DB-IP Lite database, downloaded monthly (applies after restart) |
| `console.start_on_add` | `true` | Start downloading as soon as a torrent is added |
| `log.level` | `info` | How much to log: `error`, `warn`, `info`, `debug`, `trace`, or per area such as `info,tracker=debug`. The dashboard logs to `empirebt.log` in the data directory (10 MiB, then it starts over, keeping one old file); `empirebt serve` to its output |
| `web.*` | | Web UI address, port, HTTPS, certificate, and host names; see [WEB_UI.md](WEB_UI.md#settings) |

Network and storage changes apply after a restart, unless marked live.

Command-line flags override settings for one run without saving them:
- Transfer settings: `--port`, `--download-dir`, `--max-peers`,
  `--max-connections`, `--upload-limit`, `--download-limit` and
  `--peer-timeout`.
- Network settings: `--interface`, `--no-dht`, `--no-lsd` and
  `--dht-bootstrap`.

Environment variables named `EMPIREBT_<SECTION>_<NAME>` override settings
the same way, for example `EMPIREBT_NETWORK_UPLOAD_LIMIT=500`. This is
mainly for [Docker](DOCKER.md#settings).

`--data-dir` (or `EMPIREBT_DATA_DIR`) chooses a different data directory.
Only one EmpireBT can use a data directory at a time.

## Features

### Torrent formats

- **v1, v2 and hybrid torrents share one download engine.** Many peers at
  once, rarest-first, end-game, resume and stall detection all apply to
  every format.
- **v2 pieces are verified against BEP 52 Merkle trees.** The checks were
  validated against reference values computed by libtorrent. Hybrid
  torrents must pass both verifications.
- **Magnet links work for every format.** They take `urn:btih:`,
  `urn:btmh:` or both, and fetch the metadata from peers (BEP 9/10). For v2
  and hybrid torrents, the piece layers come with BEP 52 hash requests.
  EmpireBT serves both to other peers.
- **Hybrid torrents are reachable under both info hashes.** They are
  announced to trackers and the DHT under both, and answer peers using
  either. Their padding files are virtual and never written to disk.

### Peers

Every connection, downloading or seeding, runs the same peer protocol code
in one event loop. A download that finishes keeps seeding on the
connections it already has.

- **Uploading while downloading.** Peers hear about each piece as it is
  verified and can download it. Peers that connect for an unfinished
  torrent are handed to its download.
- **Tit-for-tat choking (BEP 3).** It uploads to the three peers sending it
  the most, rechecked every 10 seconds. A fourth, optimistic slot rotates
  every 30 seconds and favours new peers.
- **The Fast Extension (BEP 6).** Seeds announce `have_all`, and every
  request gets a block or an explicit reject. New peers can download their
  allowed fast pieces while choked.
- **Peer exchange (BEP 11)** with untrusted contacts. Repeats, banned
  addresses and our own are dropped, and no peer can supply more than 100.
  It is never used for private torrents.
- **Banning bad peers.** Each piece comes from one peer, so a corrupt piece
  names its sender; three corrupt pieces ban that address. Peers that
  delivered data are tried first, in BEP 40 canonical priority.
- **Snubbing and idle timeouts.** A peer that stops sending requested
  blocks is snubbed, and its piece goes elsewhere. Connections neither side
  needs close after 15 seconds, and silent peers are dropped after 3
  minutes.
- **A peer list in both interfaces.** It shows each peer's address,
  country, client name (from its handshake or peer ID), progress and
  speeds, and the connection's flags as qBittorrent shows them (`D U I E
  P` and so on; hover in the web UI, or `?` in the dashboard, for what
  each means).

### Finding peers

- **Hole punching through NATs (BEP 55).** When EmpireBT cannot reach a
  peer that peer exchange said can be hole-punched, it asks the peer that
  told it of that one for an introduction; both then connect over uTP at
  the same moment, opening both NATs. It also introduces peers to each
  other for libtorrent clients (qBittorrent and others). Never for
  private torrents.
- **Seeds connect out.** A seeding torrent connects to the downloaders
  trackers and the DHT return, as libtorrent's seeds do, so it serves
  them even when they cannot reach it (behind a NAT, or a VPN without a
  forwarded port).
- **Trackers.** EmpireBT announces to every tier of HTTP, HTTPS and UDP
  trackers at once, each on its own interval, and within a tier to one
  tracker, moving to the next if it fails (BEP 12, as qBittorrent does by
  default). A dead tracker never delays the others, and an unreachable
  tracker host is skipped for every torrent for a while.
- **Swarm sizes for every torrent.** Trackers are scraped every 30
  minutes (BEP 48 over HTTP, BEP 15 over UDP), many torrents per request,
  so paused torrents and unresolved magnet links show their seeders and
  leechers too, along with how many have completed the torrent.
- **Honest counts for private trackers.** Trackers hear what this session
  uploaded and downloaded: data already on disk is never counted as
  downloaded, a finished download's uploads are not reported again once it
  seeds, and backup URLs in one tier never each get the same upload.
  Pausing or quitting sends `stopped` with the final counts. Announces
  carry the same parameters as libtorrent's (`corrupt`, `key`, `numwant`,
  `no_peer_id`).
- **The mainline DHT** (BEP 5, and BEP 32 for IPv6). Lookups are fast at
  first, then slow gradually to every 15 minutes for a torrent that finds
  no new peers, without ever stopping. Known nodes are saved between runs,
  but the node ID is new every start. Once other nodes agree on our
  external address, the ID is derived from it (BEP 42, as libtorrent
  does), and our replies tell other nodes theirs. Private torrents (BEP
  27) stay off the DHT. It only finds and announces peers: it stores
  nothing but peer addresses (from their own IPs, limited and expiring),
  not BEP 44's arbitrary data, and ignores an address that sends more
  than 5 messages a second for 5 minutes, as libtorrent does, so it
  cannot be used to flood anyone.
- **Local peer discovery (BEP 14).** It announces on every network
  interface, so a VPN doesn't hide the LAN, and shares UDP port 6771 with
  other clients on the machine.
- **IPv6 throughout.** Peers, uTP, the DHT, trackers and local discovery
  all work over both IPv4 and IPv6 on the same port. Systems without IPv6
  simply use IPv4.

### Privacy

Every peer in a swarm sees your IP address, whatever client you use: a
VPN is what hides it (see `network.interface`). EmpireBT also avoids
making itself easy to follow:
- **A new DHT node ID every start,** and new peer IDs and DHT node ID
  whenever the chosen network interface comes back after being down (a
  VPN reconnecting, often at a new address) or the port changes. The DHT
  node ID also changes whenever other nodes agree our address changed
  (BEP 42 ties it to the address; its random part is new each time). A node
  ID kept across addresses would let anyone watching the DHT follow you
  from one VPN address to the next.
- **Anonymous mode** (`network.anonymous`), like qBittorrent's but
  further. For public torrents EmpireBT then sends a random peer ID that
  names no client (peers see "Unknown"), leaves its name out of the
  extension handshake and DHT messages, and announces to trackers as
  `curl/7.81.0`, the User-Agent libtorrent's anonymous mode uses.
  qBittorrent's anonymous mode still sends its own peer ID. Private
  torrents keep saying they are EmpireBT, because private trackers allow
  only clients they know.

Anonymous mode hides which client you use, not who you are. A client
that names itself "Unknown" is still unusual, and clients can be told
apart by how they behave.

### Connections

- **uTP (BEP 29)** runs on the same port as TCP and the DHT, so clients
  that try uTP first connect immediately. Its congestion control backs off
  as soon as it sees queuing, so seeding leaves room for everything else.
- **Message Stream Encryption** is tried first, with a fallback to plain
  text, as in qBittorrent. It works over TCP and uTP, and is tested against
  libtorrent in every combination.
- **Speed and connection limits** apply across all torrents, and change
  live without reconnecting.
- **Interface binding.** Peers, trackers, the DHT, uTP, local discovery and
  `.torrent` downloads all use the chosen interface. Nothing falls back to
  another interface while it is down, so traffic can't leak outside a VPN.

## Speed

On the Ubuntu 26.04 desktop ISO swarm, EmpireBT reaches about 100 MB/s
within 15–25 seconds of starting. libtorrent with default settings reaches
about 33 MB/s on the same connection. Three things make the difference:

- Peers found by the DHT join a download the moment a lookup returns.
- Each peer keeps about two seconds' worth of requests in flight at its
  current rate, so fast peers are never left idle.
- A download uses up to 50 peers at once. Peers that don't answer are given
  up after 5 seconds, while spare workers dial new ones.

`crates/backend/examples/download.rs` repeats the measurement (see
[TESTING.md](TESTING.md)).

Measured over localhost with nothing else in the way
(`crates/backend/examples/throughput.rs`), one connection moves about
300 MiB/s over TCP and about 115 MiB/s over uTP. A recheck hashes about
3.9 GiB/s from cached data, on several threads.

## Scale

A benchmark seeds 2,000 local torrents with realistic (~40 KB) metadata.
Every torrent also lists a dead tracker, and the DHT bootstrap list
includes a silent node:

```bash
cargo run --release -p empirebt-backend --example scale -- 2000
```

| Measure (2,000 torrents) | Result |
| --- | --- |
| Announced to trackers | all within 30 s |
| Announced to the DHT | all within 40 s |
| Data written per session change | 43 KiB |
| Memory (session and seeding runtime) | 88 MB |
| Random 16 KiB reads, 200 active torrents | 2.5 M/s |
| Peers served at once | 5,000 on 4 threads, 0 failures, 174 MiB/s |

What makes that possible:

- **Seeding is event-driven.** One thread multiplexes every connection
  (epoll, kqueue or IOCP through `mio`), and four threads read from disk.
  Per-connection queues are bounded, so slow or greedy peers can't exhaust
  memory.
- **Announces run concurrently** and are spread out. DHT lookups stop
  waiting for silent nodes once most have answered.
- **The session is one small file per torrent.** A change rewrites
  kilobytes, and a damaged file skips one torrent instead of losing the
  session.
- **Open files are kept in a bounded pool.** Reads and writes are
  positional (`pread`/`pwrite`), like libtorrent's default disk I/O.
  Memory-mapping is avoided, because a file changed by another program
  would be undefined behaviour.
- **Resume data records verified pieces** with each file's size and
  modification time. Restarts skip re-hashing unless a file changed.

## Workspace

```text
crates/backend   The BitTorrent library: protocol, storage, trackers, DHT, uTP,
                 and the event loop every connection runs in
crates/app       The application shared by both interfaces: an engine thread that
                 owns the session, settings and seeding, driven by commands
crates/console   The `empirebt` binary and terminal dashboard
crates/web       The web UI: HTTP API, live updates, security, and the browser
                 files, built into the binary
interop          Interoperability tests against libtorrent
```

## Build and test

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

The interoperability tests run real libtorrent peers against EmpireBT. They
skip themselves until libtorrent is set up:

```bash
python3 interop/setup_libtorrent.py   # once
cargo test -p empirebt-interop
```

See [TESTING.md](TESTING.md) for how testing works, and
[interop/README.md](interop/README.md) for the interop suite.

To update the Rust dependencies, then build, lint, and test everything:

```bash
scripts/update-dependencies.sh          # within the versions Cargo.toml allows
scripts/update-dependencies.sh --major  # new major versions too (needs cargo-edit)
```

## Limitations

- **No automatic port forwarding (UPnP or NAT-PMP).** Behind a router,
  forward the listen port by hand so other peers can connect to you.
- **No per-torrent rules yet:** no seed ratio or seeding time limits, and
  no per-torrent speed limits. Peers on the local network count against
  the speed limits.
- **No web seeds (BEP 19).** Nor tracker exchange (BEP 28), which
  libtorrent, and so qBittorrent, removed long ago.
- **Hybrid connections aren't upgraded from v1 to v2** mid-handshake. That
  upgrade is an optional part of BEP 52; peers connect with either hash
  instead.
- **Tested on Linux.** macOS and Windows builds are not regularly tested.

The detailed roadmap is in [CHECKLIST.md](CHECKLIST.md).

## Releasing

1. Set the new version in `Cargo.toml`, under `[workspace.package]`.
2. Run `cargo build`, which updates `Cargo.lock`, and commit and push both.
3. On GitHub: **Actions → Release → Run workflow**.

The workflow releases the version `Cargo.toml` names: it runs the tests,
builds Linux binaries for x86-64 and ARM, pushes the Docker image to
`ghcr.io/throneempire/empirebt` (tagged with the version and `latest`), and
publishes the GitHub release with notes, binaries, and checksums. It stops
before doing anything if that version is already released or `Cargo.lock`
was not updated, and nothing is released if a test fails.

Versions follow semantic versioning: before 1.0, the last number for fixes
(`0.1.1`), the middle one for new features or changed behavior (`0.2.0`).
A version with a suffix (`0.2.0-beta.1`) is published as a pre-release and
does not move `latest`.

## License

Copyright (C) 2026 ThroneEmpire

EmpireBT is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your option)
any later version. It is distributed in the hope that it will be useful,
but without any warranty; see [LICENSE](LICENSE) for the full terms.

The country database it downloads is DB-IP's IP to Country Lite, under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/); the web UI and
dashboard credit it.
