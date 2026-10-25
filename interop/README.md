# Interop tests

These tests check that EmpireBT works with
[libtorrent](https://libtorrent.org), the reference BitTorrent implementation
used by qBittorrent and Deluge. Each test runs a real libtorrent peer next to
EmpireBT's backend on `127.0.0.1` and transfers a real torrent between them.

## Running

```bash
python3 interop/setup_libtorrent.py   # once
cargo test -p empirebt-interop
```

`setup_libtorrent.py` downloads the official libtorrent Python wheel for your
Python version and platform from PyPI. It checks the SHA-256 that PyPI
publishes and unpacks the wheel into `interop/.libtorrent/<version>/`, which
git ignores. It installs nothing into your system or Python environment, and
running it again does nothing. Use `--version 2.0.11` to test against another
release; the pinned version is the one the suite is known to pass against.

If libtorrent is not set up, every test prints `skipped: …` and passes, so
`cargo test --workspace` works on any machine. In CI, set
`EMPIREBT_INTEROP=require` to turn a missing libtorrent into a failure. Set
`PYTHON` to use an interpreter other than `python3`.

## What is tested

| Test | What happens |
| --- | --- |
| `downloads_{v1,hybrid,v2}_from_libtorrent` | libtorrent seeds; EmpireBT downloads the `.torrent` |
| `seeds_{v1,hybrid,v2}_to_libtorrent` | EmpireBT seeds; libtorrent downloads the `.torrent` |
| `resolves_*_magnet_from_libtorrent` | EmpireBT fetches metadata (and v2 piece layers) from libtorrent, then downloads |
| `serves_*_magnet_to_libtorrent` | libtorrent resolves a magnet from EmpireBT, then downloads |
| `libtorrent_decodes_our_peer_id`, `libtorrent_shows_empirebt_as_the_client` | libtorrent names us correctly from our peer ID (BEP 20) and extension handshake (BEP 10) |
| `*encrypted*`, `*plain_text*`, `encrypts_connections_with_libtorrent_defaults` | Encryption (MSE) against libtorrent set to forced, default, and disabled, both directions |
| `*utp*` | uTP (BEP 29): libtorrent's uTP-first connection, uTP-only transfers both ways, magnets over uTP, and falling back from TCP |
| `*_through_peer_exchange` | Peer exchange (BEP 11) both ways: EmpireBT learns peers from libtorrent, and libtorrent learns peers from EmpireBT |
| `keeps_seeding_to_its_peers_after_finishing_a_download` | A finished download stays in the runtime and serves a leecher that can reach only EmpireBT |
| `relays_pieces_to_peers_*_while_downloading` | A libtorrent leecher reachable only through EmpireBT gets the pieces EmpireBT downloads from a libtorrent seed, while it downloads |
| `verifies_libtorrent_hashes_for_many_file_sizes` | EmpireBT verifies libtorrent's v2 and hybrid hashes for files around block and piece boundaries |

The magnet tests cover v2-only (`btmh`), hybrid with both hashes, and hybrid
with only the v1 `btih` hash. Each transfer uses a small album with an empty
file, a tiny file, odd-sized multi-piece files, and a subdirectory. Each test
then checks that the received files match byte for byte, that empty files
exist, and that no padding files reached the disk.

The test peers use libtorrent's defaults (uTP before TCP, encryption
enabled) unless a test sets `LT_TRANSPORT` to `utp` or `tcp`, or
`LT_ENCRYPTION` to `forced` or `disabled`. EmpireBT's seeders listen on the
same port over TCP and uTP, as the client does.

The two local peer discovery tests set `LT_LSD=1`: libtorrent announces on
the BEP 14 multicast group and listens on every interface, and neither side
is told the other's address. One checks that libtorrent hears our announces
and connects to us; the other that we hear libtorrent's and our seed
connects to it. They skip themselves where multicast is unavailable.

Two IPv6 tests set `LT_LISTEN=[::1]`: EmpireBT downloads from a libtorrent
seed over IPv6, and libtorrent downloads from an EmpireBT seed listening on
`[::1]`. They skip themselves where IPv6 is off.

## Layout

```text
setup_libtorrent.py   Downloads libtorrent into .libtorrent/
lt_peer.py            Command-line libtorrent peer: make, magnet, seed, download, identify
tests/libtorrent.rs   The tests, which drive lt_peer.py as a subprocess
```

`lt_peer.py` can also be run by hand when you are debugging:

```bash
python3 interop/lt_peer.py make some/dir out.torrent hybrid
python3 interop/lt_peer.py seed out.torrent some 6881
python3 interop/lt_peer.py download out.torrent /tmp/get 6882 127.0.0.1:6881 60
```

## Adding a test

Write the case in `tests/libtorrent.rs` using the helpers there:
`album_torrent` builds the test torrent, and `Libtorrent::seed` and
`EmpireSeeder::start` start seeders, which stop when dropped.
`empire_download` and `Libtorrent::download` run the downloads, and
`assert_same_files` compares the results. Give each test its own `Scratch`
directory and use `free_port()` for ports, so tests can run in parallel.

## qBittorrent

`qbittorrent.py` runs a real qBittorrent (the linuxserver Docker image)
with torrents in every case the importer handles, for the opt-in tests in
`crates/app/tests/qbittorrent.rs`; see [TESTING.md](../TESTING.md).
