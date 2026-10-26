# Testing EmpireBT

EmpireBT has four kinds of tests. Only the first three run in
`cargo test --workspace`.

| Kind | Where | Count | Needs |
| --- | --- | --- | --- |
| Backend unit tests | `crates/backend/src/*.rs` (`mod tests`) | ~215 | nothing |
| Application tests | `crates/app/src/*.rs` (`mod tests`), `crates/app/tests/qbittorrent.rs` | 34 + 1 (+1 opt in) | nothing |
| Console unit tests | `crates/console/src/*.rs` (`mod tests`) | 21 | nothing |
| Web UI tests | `crates/web/src/*.rs` and `crates/web/tests/web.rs` | 9 + 10 | nothing |
| libtorrent interop tests | `interop/tests/libtorrent.rs` | 38 | libtorrent (skipped without it) |
| Scale benchmark | `crates/backend/examples/scale.rs` | — | run by hand, release build |

There are also 5 unit tests marked `#[ignore = "requires network access"]`.
They join the public DHT, fetch a public magnet, reach a real HTTPS
server, download a real `.torrent` from a URL, and download this month's
DB-IP country database (`cargo test -p empirebt-app -- --ignored`).

## Everyday commands

```bash
cargo fmt --all                          # format first
cargo test --workspace                   # everything that runs offline
cargo test -p empirebt-backend           # backend only
cargo test -p empirebt-backend v2::      # one module (filter by test path)
cargo test -p empirebt-backend -- --ignored   # the network tests
cargo clippy --workspace --all-targets   # lint
```

Add `-- --nocapture` to see a test's printed output, such as interop skip
messages or libtorrent logs.

Before calling work done, run `cargo fmt --all`, then
`cargo test --workspace`, with libtorrent set up so the interop suite really
runs. Clippy currently reports some old style warnings in the backend
(`is_multiple_of`, `chunks_exact`, `len` without `is_empty`). New code should
add no new ones.

## Unit tests

Each backend module tests itself in a `#[cfg(test)] mod tests` at the bottom
of its file. They are fast (about 1.3 s for the whole backend) and run
entirely on the local machine:

- **Real sockets on 127.0.0.1.** Peers, trackers, DHT nodes, and seeders bind
  `127.0.0.1:0` so the OS picks a free port. They usually run on a thread
  inside the test. For example, `session.rs` tests misbehaving peers (one
  that only sends keep-alives, one that chokes mid-piece), and `client.rs`
  runs whole downloads, magnets, resume, and session reloads.
- **Temporary directories** go under `std::env::temp_dir()`, named
  `empirebt-<what>-<pid>`, so parallel test runs never collide. Tests remove
  them afterwards.
- **IPv6 tests use `::1`** (and IPv6 multicast on link-local addresses)
  and skip themselves where IPv6 is off. No IPv6 internet is needed. No
  hostname resolves to both families here, so tests of both-family tracker
  announces call the merging code with explicit addresses.
- **Multicast tests skip themselves** where multicast is unavailable (they
  print `skipped:`). Every process on the machine shares the LSD group, so
  LSD tests use torrents no other test uses: torrents built with
  `TorrentInfo::new` get a real info hash from their fields, and the
  interop LSD tests write a unique file. Test peers that LSD should reach
  must listen on `0.0.0.0`, since LSD hands out network addresses.
- **Encrypted by default.** Test peers built on `PeerConnection::from_stream`
  accept encrypted handshakes, so every unit-test download also exercises MSE.
- **Reference values.** `v2.rs` checks Merkle roots, piece layers, and proofs
  against hashes computed by libtorrent and pasted into the tests. If those
  fail, the hashing is wrong, not the test.

Key areas and where they are tested:

| Area | File |
| --- | --- |
| Bencode | `bencode.rs` |
| .torrent parsing (v1/v2/hybrid, padding, empty files) | `metainfo.rs` |
| BEP 52 Merkle trees and hash requests | `v2.rs` |
| Peer protocol (shared by downloads and seeds): stalls, choking, uploads, bans, idle peers, Fast Extension, PEX | `engine/wire.rs`, `engine/active.rs`, tested through `session.rs` |
| Downloads: finding and dialing peers, resume | `session.rs`, `piece_manager.rs`, `peer_manager.rs`, `engine/block.rs` |
| Choking, optimistic unchoke, trust and bans, peer ranking | `swarm.rs` |
| Fast Extension (BEP 6): allowed fast sets (BEP test vectors), `have_all`, rejects, choked downloads | `fast.rs`, `session.rs` |
| Peer exchange (BEP 11): message format, what we send and accept, private torrents | `pex.rs`, `swarm.rs`, `session.rs` |
| Canonical peer priority (BEP 40) and CRC-32C (BEP and libtorrent test vectors) | `priority.rs` |
| Disk I/O, file pool, padding | `storage.rs` |
| Magnets and metadata exchange | `magnet.rs`, `metadata.rs` |
| Trackers (HTTP and UDP) | `tracker.rs`, `announcer.rs` |
| Announcing a download to every tracker at once (dead trackers don't delay others) | `discovery.rs`, `session.rs` |
| HTTP client (redirects, chunked bodies), `.torrent` URLs | `http.rs`, `metainfo.rs` |
| Local peer discovery (BEP 14): format, schedule, filtering, real multicast between instances, downloads and seeds finding local peers | `lsd.rs`, `session.rs`, `runtime.rs`, interop |
| IPv6: dual-stack listeners, uTP, downloads from IPv6 peers, the IPv6 DHT (BEP 32 encodings, a local IPv6 swarm), trackers over both families, LSD over IPv6, libtorrent both ways | `net.rs`, `runtime.rs`, `utp.rs`, `session.rs`, `dht.rs`, `tracker.rs`, `lsd.rs`, interop |
| Web UI: who gets in (password, one-time links, sessions, tokens, lockout), CSRF and Host checks, security headers, locked settings, size limits, live updates, HTTPS beyond loopback, driving the engine | `web/src/auth.rs`, `web/src/api.rs`, `web/tests/web.rs` (real HTTP and HTTPS against a real engine) |
| Network interface binding: name and address lookup, source address of connections, device tie-in, nothing connecting while it is down, trackers not marked dead | `net.rs`, `session.rs`, `tracker.rs` |
| Connection and speed limits: token buckets, the global connection cap, capped uploads and downloads end to end | `limits.rs`, `runtime.rs`, `session.rs` |
| DHT | `dht.rs` |
| Event loop: handshakes, adopting dialed connections, torrents joining and leaving | `runtime.rs`, `reactor.rs` |
| Peer ID (BEP 20), handshakes, self-connections | `identity.rs`, `peer_manager.rs` |
| Encryption (MSE): DH, RC4, both handshake sides | `mse.rs` (RC4 checked against RFC 6229, DH against Python's `pow`) |
| uTP: packets, retransmission, congestion, sockets | `utp.rs` (two cores over a simulated link with 10% loss, duplicates, and reordering; real loopback sockets) |
| Client, session save/load, resume data | `client.rs`, `resume.rs` |
| Config file | `config_file.rs`, `config.rs` |
| Application engine: commands seen in the next snapshot, failures, settings, waiting for snapshots, several interfaces at once, restart; session store and data-directory lock | `app/src/engine.rs`, `app/src/store.rs` |
| Categories and tags: assignment, the labels file, names, restart, a damaged file | `app/src/labels.rs`, `app/src/engine.rs` |
| Console: key input, formatting, UI, adding from a URL, labels and filters (driven by keys, checked on screens drawn to ratatui's `TestBackend`) | `console/src/input.rs`, `format.rs`, `ui.rs`, `app.rs` |

## libtorrent interop tests

These prove that EmpireBT works with other clients, not just with itself.
Self-tests can't catch a bug that the downloader and seeder share. The interop
suite caught a dozen such bugs in the v2 work.

```bash
python3 interop/setup_libtorrent.py   # once per machine
cargo test -p empirebt-interop        # ~6 s
```

How it works:

1. `setup_libtorrent.py` downloads the pinned libtorrent Python wheel
   (2.1.2, from GitHub, as it was not published on PyPI) into
   `interop/.libtorrent/`, which git ignores, after checking its SHA-256.
2. `lt_peer.py` is a small libtorrent client with the commands `make`,
   `magnet`, `seed`, `download`, `identify` (decode a peer ID), and
   `identify-peer` (show the client name, encryption, and transport
   libtorrent sees for a peer). By default it uses libtorrent's defaults:
   uTP tried before TCP, and encryption enabled with plain text allowed.
   `LT_TRANSPORT=utp` or `LT_TRANSPORT=tcp` limits it to one transport.
   `LT_ENCRYPTION=forced` requires RC4, and `LT_ENCRYPTION=disabled` allows
   plain text only. `LT_UPLOAD_LIMIT` caps its upload rate in bytes per
   second, per torrent, because libtorrent's session-wide limits skip local
   peers. `make … private` creates a private torrent; the relay tests use
   one, so peer exchange cannot route around EmpireBT. `LT_LSD=1` turns on
   local peer discovery and listens on every interface; `download` with
   `none` as the peer then relies on discovery alone. `LT_LISTEN=[::1]`
   makes it listen over IPv6.
3. `tests/libtorrent.rs` runs `lt_peer.py` as a subprocess:
   - A libtorrent seeder prints `READY` when it is seeding, and the test
     kills it on drop.
   - EmpireBT seeds through `Runtime`, stopped on drop.
   - The test peers connect directly by address, with no tracker or DHT.
4. Every transfer uses a test album with an empty file, a tiny file,
   odd-sized multi-piece files, and a subdirectory. It must arrive identical
   byte for byte, with empty files created and no `.pad` files on disk.

The cases:

- v1, hybrid, and v2 `.torrent` files in both directions.
- Three magnet kinds in both directions: v2 `btmh`, hybrid with both hashes,
  and hybrid with `btih` only.
- A hashing check across file sizes near block and piece boundaries.
- Encryption (MSE): fully encrypted when libtorrent requires it, encrypted
  with libtorrent's defaults, falling back to plain text for peers with
  encryption off, and refusing plain-text peers when EmpireBT requires
  encryption. All are checked in both directions against the encryption
  libtorrent reports for the connection.
- uTP (BEP 29): libtorrent at its defaults reaches EmpireBT over uTP
  first, encrypted and without a TCP fallback delay. Every format works
  with libtorrent uTP-only in both directions, magnet metadata resolves over
  uTP, and EmpireBT's TCP-first default falls back to uTP. The original
  seeding tests are pinned to TCP, so both transports stay covered.
- Peer exchange: EmpireBT, given only a libtorrent leecher, learns of the
  leecher's seed through PEX. A libtorrent leecher that knows only EmpireBT
  learns of EmpireBT's seed and finishes from it after EmpireBT leaves.
- Seeding after finishing: a download keeps its connections when it
  completes. A libtorrent leecher that can reach only EmpireBT finishes
  after EmpireBT does, using a private torrent so PEX cannot route around
  it.
- Uploading while downloading: a libtorrent leecher whose only source is
  EmpireBT gets most of the torrent from it while EmpireBT is still
  downloading from a rate-limited libtorrent seed (over 90% in practice;
  the test requires 50%). This runs both with the leecher connecting to
  EmpireBT's listener, which hands it to the download, and with EmpireBT
  connecting to the leecher.
- Client identification: libtorrent decodes our BEP 20 peer ID as
  `EM <version>`, never as another client. Once our extension handshake
  arrives, it shows us as `EmpireBT <version>`.
- Progress others see: a libtorrent seed (qBittorrent's engine, and what
  PeerBanHelper reads) watches our progress climb while it uploads to us.
- Anonymous mode: libtorrent connects to an anonymous EmpireBT seed and
  never shows it as EmpireBT or `EM` (it shows "Unknown").

Without libtorrent each test prints `skipped: …` and passes. Set
`EMPIREBT_INTEROP=require` (as CI should) to make that a failure. Set
`PYTHON` to use another interpreter.

**Debugging a failure:** the panic message includes libtorrent's log,
including its peer errors. To reproduce a failure by hand:

```bash
python3 interop/lt_peer.py make some/dir /tmp/t.torrent hybrid
python3 interop/lt_peer.py seed /tmp/t.torrent some 6881
python3 interop/lt_peer.py download /tmp/t.torrent /tmp/get 6882 127.0.0.1:6881 60
```

See [interop/README.md](interop/README.md) for how to add a case.

## Real-swarm speed check

`examples/download.rs` runs the download engine against a live swarm, with
trackers, the DHT, uTP, and encryption as the client uses them, and prints
the speed every 5 seconds:

```bash
cargo run --release -p empirebt-backend --example download -- \
    ubuntu.iso.torrent /tmp/dl 30 50
# arguments: <file.torrent> <dir> [SECONDS] [MAX_PEERS] [PORT] [LIMIT_KIB]
```

Pass `LIMIT_KIB` (for example `5120`) to check the download speed limit
against real peers. Use a current release's torrent: Ubuntu's tracker
refuses retired ones ("not authorized"), leaving only the DHT, which makes
the first 20 seconds slow.

A popular Linux ISO is a good, legal test. Compare with libtorrent on the
same connection to tell engine problems from network ones. Delete the
output directory afterwards: a fast run writes gigabytes.

## qBittorrent import tests (opt in)

The importer is tested against real qBittorrents in Docker. These tests
are opt in: they need Docker and libtorrent set up for interop, and take
about 15 seconds per version.

```
cargo test -p empirebt-app --test qbittorrent -- --ignored
EMPIREBT_QBITTORRENT_VERSIONS=5.3.0 cargo test -p empirebt-app --test qbittorrent -- --ignored
```

The default versions are 5.1.2 (before qBittorrent changed its sign-in) and
5.3.0. For each version, `interop/qbittorrent.py` starts the linuxserver
image and adds torrents in every case the importer handles:
- complete, in a category with automatic management;
- a renamed file, in a torrent given a name of its own;
- files laid out without the torrent's folder;
- half done and stopped, in the incomplete folder;
- a skipped file;
- a hybrid;
- a magnet that never resolved.

The test then imports them and checks the result. When a new qBittorrent
comes out, run the test with its version.

The sign-in itself, for both answer formats (before and since 5.2), is
covered on every run by a stand-in server in the same file.

## Throughput benchmark

`examples/throughput.rs` measures what our own code costs, with no
network in the way:
- a recheck;
- one EmpireBT seed to one EmpireBT downloader over localhost, by TCP
  and uTP, plain and encrypted;
- one seed to several downloaders at once.

Run it before and after changes to the peer protocol, uTP, storage or
checking:

```
cargo run --release -p empirebt-backend --example throughput -- [MIB] [PIECE_KIB] [DOWNLOADERS]
ONLY=utp cargo run --release -p empirebt-backend --example throughput   # just the uTP tests
```

Results vary about 15% between runs on WSL; compare several runs. To
profile one test: `ONLY="utp, plain" perf record -g ./target/release/examples/throughput 256`.
This `perf` cannot name Rust symbols, so map addresses with `nm -C`. To
compare with older code, build it in a separate `git worktree`, never by
stashing the working tree.

## Scale benchmark

The benchmark seeds thousands of local torrents and reports:

- how many torrents reach the trackers and the DHT within 60 s
- session save size and memory use
- open files and block-read throughput
- how many concurrent peers it can serve

Every torrent also lists a dead tracker, so the results reflect real
conditions.

```bash
cargo run --release -p empirebt-backend --example scale -- 2000
# arguments: [TORRENTS] [DIR] [PEERS], e.g. -- 2000 /tmp/scale 5000
```

Peer failures are broken down by stage (connecting, awaiting unchoke,
requesting). The load peers dial with a 20 s deadline, like a patient
remote client: with the default 5 s (`CONNECT_TIMEOUT`), the burst of
connections overflows the kernel's accept queue and some dropped SYNs are
not retried in time, which shows up as "connecting" failures that are not
the server's fault.

Run it after changing announcing, the session store, storage, or the
seeding runtime, and compare with the table in [README.md](README.md#scale).

## Docker image

Not part of `cargo test`. After changing the Dockerfile or startup, build
and run it (with folders under your home directory: Docker installed as a
snap cannot see `/tmp`):

```
docker build -t empirebt .
mkdir -p ~/ebt/data ~/ebt/downloads && sudo chown -R 1000:1000 ~/ebt
docker run -d --name ebt --user 1000:1000 -p 18080:8080 \
  -v ~/ebt/data:/data -v ~/ebt/downloads:/downloads empirebt
docker logs ebt; docker inspect --format '{{.State.Health.Status}}' ebt
```

Check: the sign-in link works over HTTPS, the container turns `healthy`,
files in `~/ebt/data` belong to uid 1000, and `docker stop ebt` takes a
few seconds, not the full grace period.

## Web UI frontend

The browser files (`crates/web/assets`) have no automated tests in the
suite; the user tests the web UI by hand. `app.js` has no build step, so
syntax errors only show in the browser: when changing it, check it with
`node --check` (copy it to a `.mjs` file first). Node.js is not installed
system-wide here; the copy bundled with VS Code's server works:
`~/.vscode-server/bin/*/node --check app.mjs`. Never drive the UI with
Puppeteer or a headless browser.

What the Rust tests do cover for the frontend's sake: the update protocol
(`updates_carry_only_what_changed` in `web/src/api.rs`, with 1000
torrents) and the shapes of `/api/state`, the event stream, and
`/api/torrents/{hash}/peers` (`web/tests/web.rs`).

## Known intermittent failures

None known. `keeps_seeding_to_its_peers_after_finishing_a_download` failed
once (2026-10-24) because of a startup race, since fixed: a session gave a
shared runtime only 1 s to start serving before downloading in a private
one (see CHECKLIST.md). The test now starts its event loop late on
purpose. If any test fails only sometimes, loop it with its output kept
rather than rerunning until it passes:

```
for i in $(seq 60); do cargo test -p empirebt-interop --test libtorrent NAME \
  > run-$i.log 2>&1 || echo "run $i failed"; done
```

## Writing new tests

- Put unit tests in the module they test. Put tests that need another
  client in `interop/`.
- Never depend on the internet in a normal test. If a test must use the
  network, mark it `#[ignore = "requires network access"]`.
- Bind `127.0.0.1:0` for ports, use a per-test temp directory containing the
  process id, and generate deterministic data instead of using random bytes.
- Name tests as sentences describing the behaviour, for example
  `keeps_a_peer_that_chokes_mid_piece`.
- When fixing a bug, add the test that would have caught it.

## Gaps

These are tracked in [CHECKLIST.md](CHECKLIST.md):

- There is no CI yet. It should run fmt, clippy, the workspace tests, and the
  interop suite with `EMPIREBT_INTEROP=require`.
- No fuzz tests for bencode or peer messages.
- Console tests cover input, state, and formatting, but not full end-to-end
  sessions.
- Interop tests don't yet cover several peers at once, resume after a
  restart, private torrents, or trackers.
