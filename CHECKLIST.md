# EmpireBT Development Checklist

This checklist tracks backend, console, and web UI work.

## Project foundation

- [x] Set up the Cargo workspace
- [x] Create the `backend` library crate
- [x] Create the `console` binary crate
- [x] Make the console depend on the backend
- [x] Add a project README with build, capabilities, and limitations
- [x] One version for the whole workspace (`[workspace.package]` in the root `Cargo.toml`, with the edition): it is the release other clients see in our peer ID (`-EM0100-`), extension handshake (`EmpireBT 0.1.0`), DHT `v`, and tracker User-Agent, so a release bumps it once. Checked: setting it to 0.2.0 changed `empirebt --version`, the peer ID, and libtorrent's name for us
- [ ] Register the `EM` client code with libtorrent (`identify_client.cpp`), Transmission (`clients.cc`), and the BEP 20 client list, so peer-ID-only views (tracker peer lists, private tracker whitelists) show EmpireBT
- [ ] Add formatting and linting checks
- [ ] Add continuous integration
- [ ] Establish supported Rust version and platform targets
- [ ] Define public API stability and compatibility policy

## Backend

- [x] Define the public backend API and error types
- [x] Add client configuration
- [x] Implement torrent metainfo parsing
- [ ] Validate canonical bencode dictionary ordering
- [ ] Validate duplicate metainfo keys
- [ ] Support UTF-8 and non-UTF-8 metadata safely
- [ ] Expose torrent size and piece geometry
- [x] Define torrent state and piece tracking
- [ ] Add torrent identity and stable IDs
- [ ] Add torrent statistics and transfer counters
- [x] Persist torrent metadata and lifecycle state across client restarts
- [x] Define local data storage
- [x] Map pieces across single-file and multi-file torrents
- [x] Scan existing files for verified pieces
- [ ] Add sparse-file and preallocation support
- [ ] Add atomic piece-file writes
- [ ] Add filesystem permission and disk-space checks
- [ ] Add storage path normalization tests on Windows and Unix
- [x] Implement peer wire protocol support
- [x] Validate peer bitfield lengths and unused bits
- [x] Implement extension handshake messages
- [x] Implement metadata exchange messages
- [x] Implement upload request handling
- [x] Add torrents from `http(s)://` `.torrent` URLs (dashboard and `empirebt add`; shared HTTP client in `http.rs`, follows redirects)
- [x] Implement peer request cancellation (end-game `cancel` for pieces another peer finished; queued uploads dropped on `cancel`, answered with `reject` under BEP 6)
- [x] Define peer connection management
- [x] Implement synchronous peer TCP message transport
- [x] Implement inbound peer listener
- [x] Implement peer connection limits
- [x] Add bounded concurrent outbound peer workers
- [x] Add shared piece ownership across concurrent peer workers
- [x] Implement connection retry and backoff
- [x] Implement tracker HTTP failover
- [x] Add bounded tracker reannounce and peer retry rounds
- [x] Implement idle peer detection (connections neither side needs close after 15 s; keep-alives every 90 s; silent peers dropped after 3 min; stalled peers snubbed)
- [x] Implement peer scoring and banning (trust per address; productive peers tried first; banned addresses skipped and refused)
- [x] Define tracker communication
- [x] Parse tracker tiers and validate tracker URLs
- [x] Implement HTTP tracker announces
- [x] Build HTTP tracker announce query parameters
- [x] Report verified resume progress in tracker announces
- [x] Implement UDP tracker announces
- [x] Fail over between HTTP and UDP trackers
- [x] Implement tracker response bencode parsing
- [x] Accept valid tracker responses with no currently available peers
- [x] Implement compact IPv4/IPv6 tracker peer decoding
- [x] Implement dictionary-form tracker peer decoding
- [x] Implement peer TCP handshakes
- [x] Implement peer connection lifecycle and timeouts
- [x] Implement single-peer synchronous download session
- [x] Wire tracker discovery into the download session
- [x] Continue across discovered peers until all pieces are verified
- [x] Refuse to enter seeding state after an incomplete download
- [x] Implement choke/interest state transitions
- [x] Implement piece selection and downloading
- [x] Implement request pipelining and block assembly
- [x] Implement disk-backed piece writes
- [x] Implement resume detection and existing-piece verification
- [x] Register verified resume pieces with the download scheduler
- [x] Resume completed torrents directly into seeding
- [x] Refresh torrent lifecycle state from verified storage
- [x] Retry an in-flight piece after a peer choke
- [x] Implement end-game duplicate requests
- [x] Implement rarest-piece selection
- [x] Implement optimistic unchoking (one rotating slot every 30 s; new peers weighted 3x)
- [x] Implement request timeout recovery
- [x] Implement corrupt-piece retry and peer penalties (the sender is known because each piece comes from one peer; banned after three corrupt pieces)
- [x] Upload while downloading: bitfield on connect, `have` for every verified piece, serve requests from unchoked peers, keep connections either side needs
- [x] Tit-for-tat choking while downloading: 4 upload slots to the peers sending us most, rechecked every 10 s
- [x] Accept incoming peers for torrents still downloading (the seeding listener hands them to the download, TCP or uTP, encrypted or not)
- [x] Report upload totals to trackers and show upload rates while downloading
- [x] One peer engine for downloading and seeding (`engine/`): the protocol is written once, as a state machine without I/O (`PeerWire`), over one shared torrent state (`ActiveTorrent`)
  - [x] Seeding runs the same protocol as downloading: choking, PEX, keep-alives, full Fast Extension, `yourip`, idle cleanup, hash requests
  - [x] Every connection, downloading or seeding, runs in one event loop (`Runtime`); downloads dial on a few threads and hand connections over
  - [x] A finished download stays in the runtime and keeps seeding on the same connections
  - [x] Torrents join and leave the running runtime; the console no longer rebuilds it (which dropped every seeding connection on any state change)
  - [x] Removed: per-message seeder code, the thread-per-peer seeding path, the download hand-off registry, the blocking peer loop
  - [x] Removed the old engine's leftovers: `download/` (block assembly now lives in `engine/block.rs`), `DownloadManager`, the v2 piece and block managers, the `PeerManager`/`PeerState`/`PeerId` placeholders, unused `PieceManager` methods
- [ ] Make upload slots configurable (4 per torrent today, 3 by tit-for-tat plus 1 optimistic)
- [x] Bind to one network interface (`network.interface`, `--interface`): by name or IPv4 address, for peers (in and out), HTTP/UDP trackers, the DHT, uTP, LSD, `.torrent` URLs, and seeds dialing local peers
  - [x] Fails closed: while the interface is missing nothing connects and nothing falls back; trackers are not marked dead for it
  - [x] Outgoing sockets also tied to the device (Linux `SO_BINDTODEVICE`, macOS `IP_BOUND_IF`); listening sockets by address, so they work again when a VPN reconnects with the same address
  - [x] The seeding listener retries every 10 s after failing (port taken, interface down); the header shows the interface and when it is down
  - [ ] Rebind the listener, DHT, and uTP when the interface comes back with a different address (outgoing connections already follow it)
  - [x] IPv6 binding: a name binds the interface's IPv4 and (non-link-local) IPv6 address; it is down only with neither; peers of a family it lacks are skipped, not blamed
- [x] A connection limit across all torrents, as libtorrent has (`network.max_connections`, default 500, shared by every runtime and download through `Limits`; the old seeding-only `network.max_inbound` migrates to it)
- [x] Download speed on a real swarm (Ubuntu ISO): from ~0 to ~100 MB/s, about 3x libtorrent's default on the same link
  - [x] DHT results feed running downloads at once (they were only read between rounds), and thin results retry within seconds instead of minutes
  - [x] Stalled torrents back off: DHT retries stay fast (2 s doubling to 1 min) for the first 5 minutes without a new peer, then double to every 15 minutes; a new peer brings them back to a minute. Searches returning only peers already seen count as finding nothing
  - [x] Request pipeline sized to ~2 s at each peer's rate (16 to 250 blocks, within its `reqq`), spanning several pieces so it never drains between them
  - [x] 50 peers per download by default (was 8)
  - [x] 5 s connect and handshake deadline; no fallback retries for peers that never answer; extra workers dial while slots are busy
  - [x] `examples/download.rs` measures the engine against a live swarm
- [ ] Faster DHT cold start (first peers arrive after 5-10 s; libtorrent is similar)
- [ ] Migrate settings files that still hold an old default (for example `max_peers = 8`)
- [x] Fast Extension (BEP 6), checked against the BEP's allowed-fast test vectors and live with libtorrent
  - [x] `have_all` / `have_none` sent and accepted in place of bitfields (seeders send `have_all`)
  - [x] Every request answered with a block or `reject`: while choking, for missing pieces, on cancel, on lost interest
  - [x] A choke keeps outstanding requests; rejected blocks are re-requested after unchoke
  - [x] Allowed fast set (k = 10) granted to starting peers, and peers' allowed fast pieces downloaded while choked
  - [x] Suggested pieces tried first; fast messages without negotiation close the connection
  - [ ] Send `suggest_piece` (useful for super-seeding)
- [x] Peer exchange (BEP 11), live with libtorrent in both directions
  - [x] Extension handshake while downloading: `ut_pex`, `ut_metadata` (downloaders serve metadata too), `yourip`, listen port `p`
  - [x] Send connected peers (flags: reachable, seed, uTP, encryption): full list first, then changes at most once a minute, 50 per list
  - [x] Take untrusted contacts carefully: no repeats, banned, connected, or own addresses; one IP per message; at most 100 per source
  - [x] Connect to discovered peers during a download, and retry them in later rounds
  - [x] Never for private torrents (BEP 27)
- [x] Canonical peer priority (BEP 40) as the ranking tie-break, checked against the BEP's and libtorrent's test vectors
- [x] Learn our public IP from peers' `yourip` (majority of distinct voters)
- [x] Resolve duplicate connections to one peer the way libtorrent does (the side with the greater peer ID keeps the connection it started)
- [x] Peer exchange from seeding torrents too
- [x] Local peer discovery (BEP 14), `network.lsd` (default on), `--no-lsd`
  - [x] Announce format with several info hashes per packet (≤ 1,400 bytes), cookie to drop our own; tolerant parsing (LF, header case, unknown headers)
  - [x] Every IPv4 interface joins 239.192.152.143:6771 (rescanned every 2 min), so a VPN's default route does not hide the LAN; port shared with other clients (SO_REUSEADDR/SO_REUSEPORT)
  - [x] At most one announce a minute, downloads first, seeds with the remaining room; private torrents never announced
  - [x] Downloads get local peers like DHT peers; seeds dial local downloaders that announce (`Runtime::dial`)
  - [x] Per-client rate limit and repeat suppression for incoming announces
  - [x] Interop with libtorrent's LSD in both directions
  - [x] IPv6 LSD (`ff15::efc0:988f`) on every IPv6 interface; link-local peers keep the interface they were heard on
  - [ ] Exempt local peers from the speed limits
- [x] Implement basic seeding from verified storage
- [x] Add one-call download-to-seeding lifecycle orchestration
- [x] Use bounded concurrent seeding in the complete lifecycle entry point
- [x] Keep the seeder accepting peers after an individual peer disconnects
- [x] Serve multiple inbound peers concurrently with a connection limit
- [x] Route multiple torrent seeders through one shared listener
- [x] Add graceful seeder shutdown support
- [x] Track uploaded payload bytes
- [x] Implement separate download and upload rate limits (`network.upload_limit` / `network.download_limit` in KiB/s; token buckets enforced on socket reads and writes in the event loop, so they cover downloads and seeding alike; change live from the dashboard)
  - [ ] Exempt local-network peers from the limits, as libtorrent does by default
  - [ ] Per-torrent limits
- [ ] Implement connection and request bandwidth budgets
- [ ] Implement seed ratio and seeding time policies
- [ ] Add backend unit and integration tests
- [ ] Add local tracker/peer test fixtures
- [x] Logging (2026-10-25): `tracing` events under short areas (engine, torrent, tracker, peer, seeding, network, port, web, geoip, import); `log.level` (`EMPIREBT_LOG_LEVEL` in Docker) takes a level or `info,tracker=debug`, and a typo is reported instead of silently matching nothing. `empirebt serve` logs to standard error (`docker logs`), the dashboard to `empirebt.log` (10 MiB, then it starts over keeping `.1`), one-off commands only warnings. Logged: startup summary, every notice the UIs show (added, finished, paused, errors, port changes, settings), seeding listener up or failed, interface down or up, tracker hosts going down and coming back (once each, host only: passkeys stay out), web UI address, sign-ins and failed sign-ins (with address) and lockouts, country database updates, imports, shutdown; at debug, peers dropped with the reason, port moves. Never logged: passwords, tokens, login links, tracker paths and queries; peer addresses only at debug
- [ ] Diagnostics: `empirebt doctor` (data folder writable, ports bindable, interface up, DHT reaching nodes, disk space, clock), and a Logs view in the web UI
- [ ] More debug logging where problems hide: DHT bootstrap and lookups, tracker replies, uTP, local discovery
- [x] Private-tracker accounting (2026-10-25), audited against libtorrent's announce code:
  - `downloaded` counted data already on disk (verified bytes in total), so a resumed download was charged again: now only bytes verified this session (libtorrent: payload received minus failed and redundant bytes)
  - Uploads during a download were reported twice: the finished download stays in the runtime as a seed with its counter, and the seed announcer's first `started` repeated it. Seed entries now count from the counter when they begin (`uploaded_base`)
  - Tiers were flattened and every URL announced to, so backup URLs of one tracker each got the full upload: tiers kept (`TorrentInfo::tracker_tiers`, `announce-list` replacing `announce` per BEP 12), one tracker per tier in order until one accepts, every tier (qBittorrent's default); `TrackerClient::from_tiers`, `tiers()`; status counts tiers
  - Paused or quitting downloads never sent `stopped`: now sent (2 s at most) to every tier sent `started`, even one whose reply is still on its way
  - Announces now carry libtorrent's parameters in its order: `corrupt` (bytes of pieces that failed verification), `key` (follows the peer ID, so it links nothing beyond it), `numwant=200` (0 when stopping), `no_peer_id=1`; UDP sends the key and peers wanted instead of a random key and -1
  - Tests: exact query, key, numwant, tiers (one per tier, fallback, repeats), `announce-list` replacing `announce`, a resumed download announcing `downloaded=0` then only the new piece, `stopped` on pause, seed upload baseline
- [ ] Hybrid torrents announce both hashes with full counts (libtorrent does the same); a private tracker that maps both to one torrent would count them twice
- [ ] `supportcrypto=1` in announces when encryption is on, as libtorrent sends
- [x] Privacy (2026-10-25): the DHT node ID is new every start (known nodes still saved), and the engine rebinds with new peer IDs and a new DHT node ID when the chosen interface comes back after being down (a VPN reconnecting) as well as on port changes, so nothing stable links one address to the next
- [x] Anonymous mode `network.anonymous` (applies live, by rebinding): public torrents use a random peer ID without a client code (libtorrent shows "Unknown"), no `v` in the extension handshake, no `v` in DHT queries, and `curl/7.81.0` as tracker User-Agent (libtorrent's anonymous-mode one). Private torrents keep EmpireBT's identity. One rule ties it together: a torrent whose peer ID does not start with `-EM` is anonymous everywhere (`identity::identifies`); `Client::peer_id_for` picks per torrent, and the seed announcer now announces each torrent with its own runtime peer ID. Compared with qBittorrent's (libtorrent 2.x: generic User-Agent, no handshake version, no local IPs or announce_ip to trackers), EmpireBT never sends local IPs or `ip=`, and also hides the peer ID. Tests: identity, handshake, per-torrent choice, tracker User-Agent per peer ID, DHT id per start, engine toggle, and libtorrent end to end (it cannot name an anonymous seed)
- [x] Add cancellation and shutdown signals for the seeder
- [x] Add concurrency/resource limits for seeding
- [x] Configure maximum outbound peer connections
- [ ] Add runtime/session event notifications
- [x] Run cancellable background transfers with live progress counters
- [x] Keep several block requests in flight per peer
- [x] Ignore unrelated peer messages without re-requesting blocks
- [x] Treat partially written files as missing pieces on resume
- [x] Serve only the requested block instead of re-reading whole pieces
- [x] Release a failed peer's in-flight piece back to the scheduler
- [x] Keep verified state in memory across tracker rounds
- [x] Build the seeding runtime from verified state without re-hashing
- [x] Route inbound handshakes off the accept loop
- [x] Wait for handshakes split across TCP segments
- [x] Separate the inbound connection limit from per-download peers
- [x] Track v2 upload bytes and serve v2 blocks without whole-piece reads
- [x] Cache v2 leaf layers instead of re-hashing files per request
- [x] Bound v2 Have indexes from peers
- [x] Add basic uploaded-byte metrics
- [ ] Add configuration validation diagnostics

### Backend runtime and concurrency

- [ ] Choose synchronous or asynchronous runtime strategy
- [ ] Define worker ownership and shutdown ordering
- [ ] Add bounded peer and tracker task queues
- [x] Add thread-safe session access for console and future web UI (the `empirebt-app` engine thread; see Web UI)
- [x] Add shared multi-torrent inbound runtime
- [x] Add backpressure through bounded peer workers
- [ ] Add panic-free handling for malformed remote input
- [ ] Add deterministic clock abstraction for timeout tests

### Tracker and discovery

- [x] Implement announce URL query encoding (binary info hash and peer id percent-encoded; private-tracker passkeys in the URL kept)
- [x] Implement compact IPv4 peer decoding
- [x] Implement compact IPv6 peer decoding (HTTP `peers6`, UDP trackers over IPv6)
- [x] Implement tracker failure and warning responses
- [x] Implement tracker interval and min-interval scheduling
- [x] Implement HTTPS tracker announces
- [x] Decode chunked HTTP tracker responses
- [x] Send started, completed, and stopped tracker events
- [x] Announce seeding torrents to every tracker with upload totals
- [x] Report tracker seeder and leecher counts
- [x] Honor tracker announce intervals during bounded download retries
- [x] Implement tracker tiers and failover policy: announce to every tracker at once, as qBittorrent does by default, each on its own thread and interval (`discovery.rs`); a dead tracker no longer delays the others, and peers join the running download as each tracker replies. Downloads and magnet links (re-announcing every 60 s at most until the metadata arrives) both do this; seeding already did
- [x] Add DHT node identity
- [x] Add DHT routing table
- [x] Add DHT announce/get_peers
- [x] Answer DHT queries with rotating announce tokens
- [x] Persist the DHT node id and routing table
- [x] Announce downloading and seeding torrents to the DHT
- [x] Parse magnet links (hex and base32 hashes, trackers, direct peers)
- [x] Persist pending magnet links in the session
- [x] Serve metadata from seeders to magnet users
- [x] Tolerate unknown peer message ids instead of disconnecting
- [x] Abandon peers that stop delivering requested blocks
- [x] Announce to trackers and the DHT concurrently, with jittered intervals
- [x] Skip unreachable tracker hosts across torrents with backoff
- [x] Stop DHT lookups waiting on silent nodes
- [x] Store the session as an index plus one file per torrent
- [x] Pool open data files (bounded, sharded, idle ones closed)
- [x] Share torrent metadata instead of copying it
- [x] Add a 2,000-torrent scale benchmark
- [x] Persist verified-piece state (resume data) to skip rechecks of paused torrents
  - [x] Track verified pieces as a bitfield in transfer progress
  - [x] Save resume files (bitfield + file size/mtime fingerprints) with the session
  - [x] Resume downloads without re-hashing when files are unchanged
  - [x] Show progress for paused torrents from resume data at startup
  - [x] Capture resume data on quit, and periodically during downloads
- [x] Serve thousands of peers at once
  - [x] Event-driven inbound server (one I/O thread, small disk thread pool)
  - [x] Bounded per-connection queues, backpressure, and idle timeouts
  - [x] Raise the open-file limit at startup
  - [x] Load test with 1,000+ concurrent peers
- [x] Event-driven outbound connections (dialing uses a few threads; established connections run in the event loop)
- [x] Upload slots / seed choking (tit-for-tat and optimistic unchoke for seeds as for downloads)
- [x] Interoperate with libtorrent for v1, hybrid, and v2-only torrents (both directions)
  - [x] Accept empty files in v2 file trees (no pieces root, zero pieces)
  - [x] Place v2 file-tree paths under the torrent's directory
  - [x] Treat hybrid padding files as virtual zeros, never written to disk
  - [x] Create empty files when a download completes
  - [x] Compute BEP 52 Merkle roots, piece layers, and padding correctly
  - [x] Generate correct Merkle proofs for trees of any size
  - [x] Stop requiring the v2 handshake bit on v2-hash connections
  - [x] Serve hybrid torrents to peers using either info hash
  - [x] Accept `have` and `bitfield` messages from downloaders while seeding
- [x] Download v2-only torrents from many peers at once (rarest-first, end-game, resume data)
- [x] Announce hybrid torrents under both info hashes (trackers and DHT)
- [x] Support v2-only (btmh) magnet links
- [x] Fetch v2 and hybrid piece layers for magnet links with BEP 52 hash requests
- [x] Serve metadata from v2-only seeders and piece layers from hybrid seeders
- [x] Test v2 Merkle hashing against libtorrent reference values
- [x] Permanent libtorrent interop suite in `interop/` (25 tests: uTP, encryption, client identification, v1/hybrid/v2 files and magnets, both directions, Merkle hashing across awkward file sizes)
- [x] Interop pinned to libtorrent 2.1.2 (2026-10-25; all 36 pass). 2.1.2 is only on GitHub, not PyPI, so `setup_libtorrent.py` falls back to the GitHub release, checking the SHA-256 GitHub publishes for each file
- [ ] Find the test that failed once in a full `cargo test --workspace` run on 2026-10-25, before publishing (its output was not kept; 14 clean runs followed). Loop the suite with each run's log kept, as TESTING.md describes
- [ ] Run the interop suite in CI (`EMPIREBT_INTEROP=require`)
- [x] Fix the race behind the one-off `keeps_seeding_to_its_peers_after_finishing_a_download` failure (2026-10-24; 60 looped runs never reproduced it). A session waited only 1 s for a shared runtime whose event-loop thread had not started yet, then silently downloaded in a private runtime of its own, so nothing seeded the finished torrent. That happens on a busy machine (the failure was in a full parallel run), and the app had the same window between publishing its runtime and serving. Now sessions wait up to 30 s for a runtime that has not started, and not at all for one that has stopped (it records that). The interop test now starts its event loop 1.5 s late on purpose: with the old 1 s wait it fails exactly this way, and it passes with the fix. Plus the unit test `waits_for_a_late_runtime_but_not_a_stopped_one`
- [ ] Extend interop tests: multiple peers at once, resume after restart, private torrents, trackers
- [ ] Upgrade hybrid connections from v1 to v2 (optional part of BEP 52)
- [x] IPv6, one of everything per address family (as libtorrent does); IPv4 keeps working where the system has no IPv6
  - [x] Peer listeners for IPv4 and IPv6 on one port (IPv6-only sockets, so no IPv4-mapped addresses reach the swarm; mapped ones are normalized anyway)
  - [x] uTP over both families, each datagram through the socket of its peer's family
  - [x] DHT over IPv6 (BEP 32): a second node with its own socket, routing table, and saved state (`dht6.dat`); `nodes6`, 18-byte peers; lookups run on both nodes and merge
  - [x] Trackers asked over IPv4 and IPv6 at once, peers merged, so each learns both our addresses; a silent family gets 2 s after the other answers
  - [x] Interop with libtorrent over IPv6 in both directions
  - [x] Dashboard shows how many DHT nodes are IPv6
  - [ ] Send our IPv6 address in the extension handshake (`ipv6`) and rely less on dual announces
- [x] Add peer exchange (BEP 11)
- [x] Add local peer discovery (BEP 14; see above)

### Protocol compatibility

- [x] Support BitTorrent v1 metainfo
- [x] Support magnet metadata acquisition
- [x] Support BEP 5 DHT peer discovery
- [x] Support BEP 10 extension protocol
- [x] Support BEP 9 metadata exchange
- [x] Support BEP 27 private torrents (no DHT for private torrents)
- [x] Support BEP 12 multi-tracker metadata (`announce-list` tiers parsed; every tracker is announced to rather than one per tier, which finds more peers)
- [x] Support BEP 15 UDP tracker protocol
- [ ] Map the listen port on the router (UPnP IGD and NAT-PMP/PCP), so peers behind NAT can reach us without manual forwarding
- [ ] Support BEP 19 web seeds
- [ ] Support BEP 23 tracker exchange
- [x] Support BEP 29 uTP transport, `network.utp = enabled | preferred | disabled`
  - [x] Packets, selective ACK, wrapping sequence numbers, handshake numbering as libutp/libtorrent
  - [x] RTT/timeouts, fast retransmit (3 dup acks / 3 later SACKed), paced resend after timeouts
  - [x] LEDBAT congestion control (100 ms target) with slow start and ssthresh
  - [x] Shares the UDP port with the DHT (DHT messages start with `d`)
  - [x] Incoming uTP in the event-driven seeding server; MSE works over uTP
  - [x] Outgoing: TCP first then uTP (or uTP first when preferred); retries the other transport on I/O failure
  - [x] Interop-tested: libtorrent uTP-first, uTP-only both directions, magnets over uTP, fallback
- [x] uTP throughput, 11 → ~115 MiB/s plain and 12 → ~83 MiB/s encrypted per connection on localhost (2026-10-24, `examples/throughput.rs`). Cause: the default ~200 KiB UDP receive buffer overflowed as the window grew, so packets dropped, the window halved, and lost retransmits waited out 500 ms timeouts (139 losses and 19 timeouts in 12 s). Fixes: 8 MiB UDP buffers on every DHT and uTP socket (`net::udp_sockets`; the kernel caps them at `net.core.rmem_max`), and batched receiving (`net::receive_batches`: wait with mio, read everything queued, then send acks/data and wake readers once per connection per batch, for uTP's own socket and the DHT's)
- [ ] uTP: fewer system calls still (`sendmmsg`/`recvmmsg` or GSO need `unsafe`/libc; the backend has no `unsafe` today), and fewer allocations per packet
- [ ] uTP path MTU discovery (payload is fixed at 1380 bytes) and delayed ACKs
- [x] uTP and the DHT over IPv6
- [x] Show each peer's transport (TCP/uTP) in the console (Peers tab, `3`)
- [x] BEP 20 peer IDs: `-EM` + version from the crate version (`-EB` was EBit's code); libtorrent shows us as "EmpireBT <version>"
- [x] Refuse connections to ourselves (own peer ID in the handshake)
- [x] Support MSE/PE encryption (Vuze spec, not a BEP): RC4 and plain-text modes, both directions, `network.encryption = enabled | required | disabled`
  - [x] Outgoing: try encrypted first, fall back to plain text (libtorrent's behaviour), or require RC4
  - [x] Incoming: detect plain vs encrypted handshakes in the reactor; obfuscated info-hash lookup across all seeded torrents
  - [x] Interop-tested against libtorrent forced, default, and disabled encryption in both directions
- [ ] Move RC4 for seeding off the reactor thread if encrypted upload becomes a bottleneck (measured: ~440 MiB/s per core, the cipher's own limit; writing it with locals gained 2%)
- [x] Show each connection's encryption in the console's peer view (Peers tab, `3`)
- [x] Show each peer's client name (from BEP 10 `v`, else BEP 20 peer ID) in the console (Peers tab, `3`)
- [x] Support IPv6 tracker peers (BEP 7 `peers6`; listed under BEP 32 here before)
- [x] Detect BitTorrent v2 and hybrid metainfo
- [x] Parse BEP 52 v2 file trees and piece roots
- [x] Parse BEP 52 piece layers
- [x] Verify existing v2 files with 16 KiB Merkle leaves and piece layers
- [x] Encode and decode BEP 52 hash request/response messages
- [x] Recognize locally complete v2-only data without routing it through v1
- [x] Select the correct v1/v2 truncated handshake identifier
- [x] Advertise and inspect the BEP 52 v2 handshake capability bit
- [x] Require both v1 and v2 verification before completing a hybrid torrent
- [x] Add per-file v2 piece geometry and Merkle verification state
- [x] Add v2 piece-level storage reads and writes
- [x] Add v2 block request scheduling and assembly
- [x] Restore verified v2 piece state from disk
- [x] Expose global v2 piece counts and file locations
- [x] Add a verified v2 upload surface using v2 handshake identity
- [x] Serve v2 piece-layer hash ranges with Merkle proofs
- [x] Expose v2 seeder creation through the client API
- [x] Support downloading v2-only torrents
- [x] Add a direct-peer v2 download session
- [x] Add tracker-driven v2 peer discovery and retries
- [x] Route v2 downloads into the combined download-and-seed lifecycle
- [x] Serve v2 leaf-layer Merkle proofs
- [x] Request and validate remote v2 piece-layer proofs
- [x] Bound concurrent v2 seeding connections
- [x] Enforce v2 capability negotiation for inbound peers
- [x] Validate v2 Merkle-compatible piece lengths during metainfo parsing
- [x] Route v1 and v2 seeders through one bounded shared runtime
- [x] Index shared-runtime routes by handshake info hash
- [x] Reject duplicate shared-runtime handshake routes
- [x] Bound shared-runtime handshake reads
- [x] Apply client peer timeouts to shared-runtime handshakes
- [x] Honor bounded tracker retry intervals for v2 announces
- [x] Expose one client-level runtime for all verified seeders
- [x] Skip incomplete torrents when constructing the shared seeding runtime
- [x] Use positional storage I/O for concurrent piece access
- [x] Support hybrid v1/v2 torrents in the scheduler
- [x] Reject unsupported v2-only download/seeding paths explicitly (superseded: v2 is fully supported)

## Console

- [x] Add command-line argument parsing
- [x] Add commands for adding and removing torrents
- [x] Add commands for starting, pausing, and resuming torrents
- [x] Add torrent status and progress output
- [x] Add configuration options
- [x] Add a shared `config.toml` settings file (backend sections plus `[console]`)
- [x] Add `empirebt config` and a dashboard settings screen
- [x] Migrate the remembered download folder into the settings file
- [x] Add graceful shutdown handling
- [ ] Add console integration tests
- [x] Add persistent session state
- [x] Add interactive status refresh
- [x] Add a full-screen dashboard with detail tabs and dialogs
- [x] Add download directory and listen-port flags
- [x] Add clear exit codes and command help

## Quality and release

- [ ] Document the backend API
- [ ] Document the console commands
- [ ] Define configuration and data-directory compatibility rules
- [ ] Add end-to-end tests with a local test swarm
- [ ] Add versioning and release build checks
- [ ] Add benchmark coverage for bencoding and piece verification
- [ ] Add fuzz tests for bencode and peer messages

## Web UI

Decided (2026-10-24): one process for both UIs; a no-build frontend; local
first with remote access opt-in; categories are labels only (no per-category
save folders for now).

- [x] Design the web UI (layers, process model, security; decisions above)
- [x] Extract `empirebt-app` (crates/app): the application layer that was inside `console/src/app.rs` (client, settings, session store, seeding service, check queue, rates, notices, URL fetches, network startup), on one engine thread; UIs send `Command`s through a cloneable `Handle` (`call` returns once the next snapshot shows the effect) and read immutable `Snapshot`s (`wait_for_change` for live updates); also `AddTorrent` for uploads
  - [x] Move the console onto it with no behavior change (existing tests pass; the console keeps only TUI state)
  - [x] Data-directory lock (`lock` file, OS lock released if the process dies) so two processes never share a session; `add`/`list`/`remove` respect it
- [x] Tags (many per torrent) and categories (one label per torrent), in `empirebt-app`: commands `SetCategory`, `SetTags`, `Create`/`RemoveCategory`, `Create`/`RemoveTag`; snapshots carry each torrent's labels and the full lists
  - [x] Saved in the data directory's `labels` text file (atomic writes); names trimmed, 1–64 characters, no control characters; labels follow a v2 magnet's identity change and leave with a removed torrent; a damaged file is kept as `labels.damaged` and reported
  - [x] Console: `C` category, `t` tags (Tab completes known names), `f` filter picker (category, no category, tag, no tags), `Esc` shows all; labels beside names and in the detail pane
  - [x] Wide table layout (Up, Peers, ETA columns) from 130 columns instead of 100, so names keep room
  - [ ] Rename a category or tag (remove and re-add for now)
- [x] `empirebt-web` (crates/web): axum on its own tokio runtime (hyper connection loop of our own), JSON API, Server-Sent Events (at most once a second), frontend (HTML/CSS/JS modules, no build step) embedded in the binary
  - [x] Run modes: `empirebt ui --web` (TUI + web; a dialog shows the address, one-time link, first password, certificate fingerprint), `empirebt serve` (headless; Ctrl-C or SIGTERM saves and stops); `empirebt web password|token <name>|revoke-tokens`
  - [x] Settings `[web]`: `web.address` (127.0.0.1), `web.port` (8080), `web.tls_cert`/`web.tls_key`, `web.allowed_hosts`
  - [x] Security: 127.0.0.1 by default; any other address serves HTTPS only (rustls; your PEM certificate, or a generated self-signed one in the data directory, key 0600, SHA-256 fingerprint printed)
  - [x] Login always required, even on localhost; first start generates a password (shown once); one-time login links (10 min, single use, token in the URL fragment so it never reaches logs); Argon2id hash in `web-auth` (0600)
  - [x] Sessions: random 256-bit cookie tokens stored hashed (HttpOnly, SameSite=Strict, Secure under TLS), 24 h idle / 7 day limit, at most 64; API tokens (Bearer, stored hashed) for scripts
  - [x] Failed-login limits: 5 failures from an address mean a 5-minute wait (429 + Retry-After, even for the right password); at most 2 password checks at once
  - [x] CSRF and DNS rebinding: Host must be an IP address, `localhost`, or in `web.allowed_hosts` (421 otherwise); writes with a cookie need `X-EmpireBT` and a matching Origin (login too); Bearer tokens exempt
  - [x] Headers on every response, errors included: CSP `default-src 'none'` (no inline scripts or styles), frame-ancestors 'none', X-Frame-Options, nosniff, no-referrer, COOP/CORP, Permissions-Policy, no-store on the API, HSTS under TLS
  - [x] No file-system reach from a browser: torrents only by magnet link, URL, or upload (never a server path); storage, network-interface, and web settings locked (shown read-only)
  - [x] Limits: 64 KiB request bodies, 16 MiB uploads, 16 live-update streams, 256 connections, 10 s TLS handshake and request-header timeouts (slowloris), 3 s graceful stop
  - [x] Frontend: sign-in, header (speeds, limits, network), sidebar filters (status, categories, tags; create/remove), search, table with labels, detail tabs, dialogs (add by link/URL/upload/drop, remove, category, tags, settings), toasts; light and dark; phone layout; text only into the DOM
  - [x] Tests: 7 unit (hashing, auth file permissions, sessions, links, bans, host names, locked settings) and 9 end to end over HTTP and HTTPS (strangers, sign-in, lockout, CSRF, engine control, tokens, size limits, live updates, HTTPS only beyond loopback)
  - [x] Thousands of torrents: live updates are deltas. Each stream remembers what it sent: the header every time, then only changed torrents, removed hashes, the order when it changed, and categories/tags/settings when they changed (`/api/state` gives the same shape, full). With 1000 idle torrents an update carries a few hundred bytes, not the whole list. The browser keeps one row element per torrent and rewrites only cells whose text changed, re-sorts and re-filters in memory, counts every filter in one pass, rebuilds the sidebar only when its labels change, and off-screen rows skip layout (`content-visibility`)
  - [x] Fixed: the event stream spun a CPU core while nothing changed (its sleep deadline was already past); it now checks again a second later. The first update on a connection always goes out
  - [x] Redesign for feel (2026-10-24, after "taps feel dead, screens are abrupt, looks dated"): surfaces separated by tone instead of lines, SVG icons, tinted status pills and label chips, segmented tabs, switches for on/off settings; every button and row has a pressed state, hover styles only on devices with a pointer (on iOS they stick after a tap), Start/Pause/Recheck spin until the server answers; the phone detail sheet slides in and can be swiped away to the right, dialogs rise as bottom sheets with a grabber (drag down, tap outside, or Escape to close), toasts slide in, tab changes fade; all motion off under reduced motion
  - [x] Appearance per browser (Settings → Appearance, saved in localStorage): theme system/dark/light, accent plain (default; white on neutral grays) or 8 presets or any color, text on the accent picked for contrast; neutral gray palette with no blue tint
  - [x] Sorting by any column (click a header; a menu on phones); ties keep the order torrents were added in
  - [x] Phones (iPhone): torrents as cards (name, progress and size, status and speeds, ETA); filters as a sideways-scrolling chip bar; details open as a full-screen sheet with Back (the back gesture works too) and the actions in reach; dialogs as bottom sheets; 16 px fields (no iOS zoom on focus); 40–44 px tap targets; safe-area insets and `100dvh`
  - [x] Peers tab (web and console `3`; Trackers moves to `4`): each connected peer's country flag, address, client, direction/transport/encryption, progress, speeds, and totals; refreshed every 2 s (web) or 1 s (console) while open. `GET /api/torrents/{hash}/peers`, `Command::Peers`, `Runtime::peers`, `swarm::PeerInfo` (rates over windows of at least a second; client name from the extension handshake `v`, else decoded from the peer ID, Azureus or Mainline style)
  - [x] Countries from the DB-IP Lite country database (CC BY 4.0, credited under the peer list), as qBittorrent does: downloaded in the background through the bound interface to `ip-country.csv.gz` in the data directory, refreshed monthly, looked up by binary search over ranges; `peers.countries = false` turns it off
  - [x] Seeing who we upload to (2026-10-25): the web Peers tab sorts by any column (click a header; on phones a menu above the list, since headers are hidden there), opens by upload speed, splits the totals into Downloaded and Uploaded, sums up "N peers · sending to N · getting from N", and shows peers we send to in green (a tinted card on phones, whose entries now show both totals). Dashboard: `S` sorts the torrent list (added, name, size, progress, download or upload speed, peers, time left; the selection stays on its torrent), and the Peers tab lists who gets the most from us first, with a Sent column and green rows for peers we send to
  - [ ] One view of everyone we upload to, across all torrents (today: per torrent)
  - [ ] Rebind the web UI when its settings change without a restart
  - [ ] Pick up API tokens made while the web UI runs (today: restart)
  - [ ] Per-file progress and file priorities in the detail view

## Docker

Decided (2026-10-24): build with the official Rust image (glibc, as on a desktop), run on distroless `cc-debian12` as a non-root user; no Alpine, musl, or allocator swap.

- [x] `Dockerfile` (two stages, cargo caches, `--locked`; ~33 MB image), `.dockerignore`, `docker-compose.yml` (bind mounts `./data` and `./downloads`, `user: PUID:PGID`, ports 8080 and 6881 TCP+UDP, 30 s stop grace), `docker-compose.vpn.yml` (gluetun, bound to `tun0`), [DOCKER.md](DOCKER.md)
- [x] Settings from `EMPIREBT_<SECTION>_<NAME>` environment variables: run-only overrides (`ConfigFile::set_override`, never saved), shown as overridden with their real values, unknown names warned about, bad values an error naming the variable; `EMPIREBT_DATA_DIR`
- [x] `web.https = auto|off`: plain HTTP on every address only when a reverse proxy adds HTTPS (an error together with `web.tls_cert`)
- [x] Web UI password from `EMPIREBT_WEB_PASSWORD` or `EMPIREBT_WEB_PASSWORD_FILE` (Docker secrets), set on every start
- [x] `empirebt health` (connects to the web UI's port) as the image's `HEALTHCHECK`
- [x] `empirebt web password|token|revoke-tokens` work while EmpireBT runs (they no longer take the session lock; they only touch `web-auth`)
- [x] Tested for real: built, run as uid 1000 with bind mounts (files owned by the user), healthy, HTTPS sign-in, environment limit applied and not saved, DHT joined, plain HTTP refused, `web.https = off`, password from the environment, `docker stop` in ~3 s
- [x] VPN port forwarding: `network.port_file` names a file holding the port (gluetun's `VPN_PORT_FORWARDING_UP_COMMAND` writes it), read at start and every 5 s; a missing, empty, or `0` file keeps the port, anything else is reported once. `docker-compose.vpn.yml` is set up for ProtonVPN; tested in a real container by writing ports by hand (a real ProtonVPN session is untested here)
- [x] Change the listen port while running (`Engine::change_port`, also behind `network.port`, now live): downloads stop with progress saved, the runtime, DHT, uTP, and LSD are rebuilt on the new port, seeds rejoin, downloads restart once the new runtime is live (so they never fall back to a private one), and the new announcer announces everything at once. Tests: a seed moves to the port a port file names (handshake answered on the new port, refused on the old), and a live `network.port` change restarts an interrupted transfer
- [ ] Publish images (GHCR) for amd64 and arm64 from CI
- [ ] Login lockout behind a reverse proxy counts the proxy's address for everyone; trust `X-Forwarded-For` from configured proxies (`web.trusted_proxies`)
- [ ] Mark cookies `Secure` behind an HTTPS proxy (`X-Forwarded-Proto` from trusted proxies)

## Save locations and qBittorrent import

Planned (2026-10-24) in four steps, each useful alone: per-torrent folders, category folders with automatic mode, moving files (incomplete folder, move on completion, "move to…"), then a read-only qBittorrent importer (`BT_backup` `.torrent` + `.fastresume`, `categories.json`, `Session\Tags`; dry-run report; trusted progress by default).

- [x] Step 1: a folder per torrent. `Torrent::save_dir`, recorded when added (the download directory at that moment, so changing `storage.download_dir` later leaves existing torrents where they are, as its description always promised); `Storage::at` shares one pool of open files; checks, downloads, seeding, resume data, and empty-file creation follow it. Session index `EMPBT4` stores each folder; `EMPBT3` sessions load with everything in the download directory. `Command::AddWith(AddRequest { source, save_dir })` (folders must be full paths, `~` expanded); dashboard add dialog has a "Save in" field (↑↓); `empirebt add -d <folder>`; web Add dialog "Save in" with suggestions; API `savePath` on add and upload, and on torrents and details. Browsers may save only in the download folder, `storage.allowed_dirs`, or folders inside them (403 otherwise, `..` refused). Tests: backend folders and session versions, engine add/restart and relative refusal, web allowed folders (unit and end to end), console dialog
- [x] Step 2: categories with folders. Saved in `labels` as a third field (older files load, and older versions ignore it); `Command::SetCategoryFolder`; adding with a category uses its folder unless one is chosen. `AddRequest` also carries `category`, `tags`, and `start`, so one call adds a torrent fully placed (the importer needs this). Category folders are folders browsers may save in. Web: category, tags and folder in the Add dialog (picking a category moves "Save in" to its folder), a folder when creating a category, ✎ to change it; API `PUT /api/categories/folder`, `category`/`tags` on add and upload, `lists.categoryFolders`. Tests: labels file, engine (folder, override, labels, paused, restart), web end to end
- [ ] Dashboard: set a category's folder (today only from the web UI or the importer)
- [ ] Automatic mode (a torrent's folder follows its category) and an incomplete-downloads folder: both need moving files, so they come with step 3
- [ ] Step 3: moving files: an incomplete-downloads folder moved on completion, "move to…", category moves; same-drive renames, cross-drive copy then verify then delete, resumable after a crash, progress, full-disk handling
- [x] Step 4 (done before step 3, which it does not need): `empirebt import qbittorrent --url --username [--password] [--map FROM=TO] [--recheck] [--dry-run] [--yes]`, through qBittorrent's Web API, read only (`torrents/info`, `categories`, `tags`, `files`, `pieceStates`, `export`). Each torrent comes over where its files are now (`content_path`, so unfinished ones from the incomplete folder), with category, tags, stopped/running state, and qBittorrent's verified pieces as resume data (`Client::adopt_progress`; files' current sizes and times, so later changes still mean a check); the exported .torrent brings trackers and v2 piece layers. Categories with folders, all tags, magnets without metadata (their magnet URI, trackers included). Renamed files, content layouts and torrents' shown names come over (see Renaming). Paused with a warning: skipped files, errors, missing files, content not found at the (mapped) path. Noted: share limits. Shows the plan, asks (or `--yes`), then imports offline with the data-directory lock
  - Decided (2026-10-25): the Web API, not qBittorrent's files (a first version read `BT_backup` directly and was removed): a documented interface, independent of qBittorrent's storage (fastresume or SQLite) and OS, and its export carries what qBittorrent strips from its own .torrent copies
  - Tests: opt in (`--ignored`), against live qBittorrents in Docker (`interop/qbittorrent.py`; 5.1.2 and 5.3.0 by default, `EMPIREBT_QBITTORRENT_VERSIONS` for others), covering every case above end to end; always on, the sign-in for both answer formats against a stand-in. (Recorded answers were tried first and dropped: 86 fixture files.) Also checked by hand with the real command (dry run, import, list, wrong password)
  - qBittorrent 5.2+ (checked against 5.3.0): a sign-in answers 204 (refused: 401) instead of 200 "Ok." (refused: 200 "Fails."), and the session cookie is `QBT_SID_<port>` instead of `SID`; both generations are handled and tested. Everything else 5.3.0 changed is additive (new fields; categories gained share limits and a nullable `download_path`)
  - Fixed on the way: the HTTP client waited for servers to close the connection; qBittorrent sometimes keeps it open, so responses now end at their Content-Length or last chunk (affects tracker requests too)
- [x] Renaming (2026-10-25), like qBittorrent's Content tab:
  - Backend: one rename table per torrent (`Renames`, the torrent's path to the one on disk), shared by every `Storage` of the torrent, so renames apply at once to transfers and seeding; `Storage::path_for` resolves through it. `Client::rename_torrent` (the name shown; files untouched), `rename_path` (file or folder; moves files on disk all-or-none with rollback, refuses clashes with the torrent's other files or files already on disk and paths outside the folder, closes pooled handles first, removes emptied folders, drops entries renamed back), `set_content_layout` (Original / Subfolder for a single file / NoSubfolder for a multi-file torrent), `adopt_names` (the importer), `files`. Saved per torrent in `session/names/<hash>.names` (text: `name`, `layout`, `file` lines; only torrents with names have one; stale ones removed). Resume data stays valid across a rename (sizes and times don't change)
  - Engine: `Command::RenameTorrent`, `Command::RenamePath` (refused while downloading or checking: writes or checks during the move could recreate or miss files; seeding only reads), `AddRequest.layout` (magnets: applied by the client when their metadata arrives), `TorrentView.name` is the shown name, plus `original_name`, `renames`, `files()`
  - Web: Files tab as a folder tree with a ✎ per file and folder (34 px tap targets), the torrent's name with ✎ at the top; Add dialog "Content layout"; API `PUT /api/torrents/{hash}/name`, `POST /api/torrents/{hash}/rename`, `layout` on add and upload, details' files with `original` and `originalName`
  - Dashboard: `R` renames the torrent, `r` picks a file or folder and renames it; the Files tab is a tree with renamed files marked
  - Importer: qBittorrent's file names become the rename table (renamed files, NoSubfolder/Subfolder layouts), the folder they are relative to is found from `content_path`, and a name given in qBittorrent becomes the shown name; nothing is skipped for renames anymore
  - Tests: backend (file and folder renames on disk then verified through the new names, refusals, rollback, layouts, session round trip), engine (layout on add, both commands), web end to end, console (tree, `R`, `r`), importer folder finding, and the live qBittorrent test (a renamed file and torrent, a NoSubfolder torrent, all verified after import)
- [x] Content layout everywhere a torrent is added: the dashboard's add dialog (a third field, ←→), `empirebt add --layout`. A magnet link's layout is kept on the torrent (`Torrent::pending_layout`), saved as a `layout` line in its names file, and applied by the client when its metadata arrives, so it survives restarts and works offline (tests: backend round trip and application, console dialog)
- [ ] Renaming while downloading (pause first, for now)
- [x] Seeds dial downloaders (2026-10-25, found in real use: an only-seed in WSL2's NAT mode served nobody): seeds dialed only local-discovery peers; peers from tracker and DHT announces were counted for status and dropped. Now up to 50 per announce are dialed (`Runtime::dial` skips connected, banned, and over-limit ones), as libtorrent's seeds do. Test: a tracker returns a listening-only downloader and the seed connects to it.
- [ ] Tell the user when no peer has ever connected in (like qBittorrent's "firewalled" status), so an unreachable setup is obvious
- [ ] A seed with no peers learns of new downloaders only at its next announce (up to 30 minutes for trackers, 15 for the DHT); consider announcing more often while it has none
- [x] PeerBanHelper banned EmpireBT for fake progress (2026-10-25, found in real use): `have` messages went only to peers lacking the piece, so seeds never heard our progress and saw the bitfield from when we connected while uploading to us ("client progress 58.54%, calculated minimal 89.98%"). Every verified piece is now announced to every peer, as libtorrent's default `send_redundant_have` does; allowed-fast hints still only to peers lacking the piece. Interop test: a rate-limited libtorrent seed watches our progress climb to 100% (it saw 0% throughout before the fix, checked by reverting it)
- [x] Peer flags as qBittorrent shows them (2026-10-25): `PeerInfo::flags()` follows qBittorrent's `determineFlags()` letter for letter and in order (D d U u K ? O S I H X L E e P) with its descriptions; the web UI shows the letters with the descriptions as tooltip (API `flags`, `flagsDescription`), the dashboard the letters with a legend under `?`. New tracking for it: optimistic slot, snubbed (set and cleared with the snub), where a dialed address came from (`Swarm::offer_from` with tracker, DHT and local discovery; peer exchange contacts tagged even when already known), and whether the encryption handshake was used when the stream went on in plain text (`PeerConnection::is_obfuscated`, through the reactor's accepted and adopted connections). Tests: letters and order for every case, sources, snubbed, a plain incoming peer (`U … I`), an encrypted one through the reactor (`I E`)
- [x] Remove with files (2026-10-25): `Command::RemoveWithFiles`. The torrent leaves at once; its files (renames followed, padding left out) are deleted once its transfer thread has stopped, so a last write cannot recreate one, and after the seeding runtime let go; open pool handles are closed first so space frees at once. Folders it empties go (never the save folder itself); other files and files another torrent in the list uses stay; notices say what was deleted, kept, or failed. Web: Remove dialog with a red "Also delete the downloaded files" switch (whole row tappable), API `DELETE …?deleteFiles=true`; dashboard: `d` in the remove dialog; `empirebt remove N --delete-files`. Tests: engine (files and emptied folders gone, other files stay; a file another torrent uses stays), web end to end, console dialog; CLI checked by hand
- [x] Import: pieces that cross into a file skipped in qBittorrent are not trusted (2026-10-25). libtorrent keeps such a piece's bytes for the skipped file in its part file, not in the file, so qBittorrent shows the piece complete while the file lacks those bytes; trusted, EmpireBT would seed it wrong and never download it. Every piece in a skipped file's `piece_range` is downloaded again (no range: nothing trusted). The live test's fixture skipped the wrong file (by index, and the torrent's file order differs from the order made); it now skips by name and checks that exactly the wanted half is trusted
- [ ] Import: skipped files once EmpireBT has file priorities
- [ ] Import from qBittorrent in the web UI (today the command line)

## Speed work (2026-10-24)

Measured with `examples/throughput.rs` (EmpireBT to EmpireBT over localhost, 1 GiB, 1 MiB pieces; WSL2, 16 cores; runs vary about 15%):

| | Before | After |
| --- | --- | --- |
| Recheck | ~1.7 GiB/s | ~3.9 GiB/s |
| 1 downloader, uTP, plain | 11 MiB/s | ~115 MiB/s |
| 1 downloader, uTP, encrypted | 12 MiB/s | ~83 MiB/s |
| 1 downloader, TCP, plain / encrypted | ~340 / ~170 MiB/s | unchanged |
| 4 downloaders at once (seed's upload) | ~290 MiB/s | unchanged |

- [x] `examples/throughput.rs`: recheck, one seed to one downloader (TCP/uTP, plain/encrypted), and one seed to several downloaders; `ONLY=<part of a name>` runs one test for profiling
- [x] uTP: bigger UDP buffers and batched receiving (see the uTP item above)
- [x] Parallel rechecks: one thread reads pieces in order (sequential for hard drives), up to 4 threads hash them (`DownloadSession::verify_pieces_in_parallel`)
- [x] Fixed: a check cancelled part way (a recheck interrupted by starting or pausing the torrent, or quitting during a download's first check) saved its partial results as resume data; the next start trusted them and treated unreached pieces as missing, downloading data already on disk. Now only a finished check is recorded (`TransferProgress::checked`); test `cancelled_checks_leave_no_resume_data` fails without the fix
- [x] Tried and dropped: LTO with one codegen unit (no measurable gain; the profiles are 60-70% kernel time: TCP stack, copies, page cache), and a rewritten RC4 loop (2%)
- [x] From libtorrent's commits (2026-10-25): its file pool opened a file read-write while still holding it read-only, which FUSE passthrough (Linux 6.9+, e.g. mergerfs) refuses with ETXTBSY (arvidn/libtorrent dd3fd78). Ours did the same when a file read during a check is later written; the read-only handle now leaves the pool, and is closed, before the read-write open (best effort: a reader still using it keeps it open, as in libtorrent)
- [x] Checked, not affected: arvidn/libtorrent f90c324 (each sent block erased its request from the front of a vector); our upload queue is a `VecDeque` taken with `pop_front`
- [ ] Maybe, measure first: merge a peer's adjacent block requests into one disk read (`preadv`), as arvidn/libtorrent 46255b2 does; each 16 KiB block is one `pread` today, from the page cache
- [ ] TCP and seeding are bound by the kernel on localhost, not our code (our own code is under 10% of samples besides SHA-1); reusing piece and block buffers might save a few percent (page faults show in profiles)
