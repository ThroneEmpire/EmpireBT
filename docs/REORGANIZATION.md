# Backend reorganization

Grouping `crates/backend/src` by what each module is for. Moves only:
no behavior changes, and the public API (`lib.rs` re-exports) stays the
same. Files are moved with `git mv` so their history follows them.

Started 2026-11-13, after the upload-slot work (commit `6fbcd9f`).

## Target layout

| Folder | Modules | What it is |
|---|---|---|
| `protocol/` | bencode, metainfo, magnet, metadata, v2, fast, pex, holepunch, mse, priority, connection (was `peer_manager`) | Formats and messages of the BitTorrent protocol and its extensions |
| `net/` | mod (was `net.rs`), utp, http | Sockets, interface binding, uTP, HTTP |
| `discovery/` | tracker, tiers (was `discovery.rs`), dht, dht_id, lsd, announcer | Finding peers: trackers, the DHT, local discovery |
| `engine/` | active, block, wire, runtime, reactor, session, swarm, choker (out of `swarm.rs`), limits, seeder (with `v2_seeder`), piece_manager | Moving data: connections, choking, limits |
| `storage/` | mod (was `storage.rs`), resume, names | Files on disk |
| top level | lib, client, config, config_file, error, identity, progress, torrent (with `transfer`) | The client and its settings |

## Progress

- [x] `protocol/`
- [x] `net/`
- [x] `discovery/` (rename `discovery.rs` to `tiers.rs`)
- [x] `engine/` moves
- [x] `storage/`
- [x] Merge `seeder.rs` + `v2_seeder.rs`, and `transfer.rs` into `torrent.rs`
- [x] Split the choker out of `swarm.rs` into `engine/choker.rs`
- [x] Update doc comments and docs/TESTING.md paths
- [ ] Later, separately: split `client.rs` (~2,000 lines of code)

After each step: `cargo build`, then the full suite, clippy, and fmt at
the end.

## Notes

- Moved with a script that ran `git mv` and rewrote `crate::module` paths
  to `crate::group::module`; the public API is unchanged (`lib.rs`
  re-exports).
- `PeerKey`'s number is visible to the rest of `engine/` (`pub(super)`),
  since the choker sorts and builds keys; nothing outside sees it.
- The choker reaches swarms only through `Swarm::choking_candidates` and
  `Swarm::apply_choice`; a swarm's peers stay private to `swarm.rs`.
- docs/CHECKLIST.md keeps the old file names in its history entries on
  purpose: they describe the code as it was then.
