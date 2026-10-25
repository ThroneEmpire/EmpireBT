#!/usr/bin/env python3
"""A libtorrent peer for interop tests, driven by interop/tests/libtorrent.rs.

    lt_peer.py version
    lt_peer.py identify <peer_id>
    lt_peer.py make <source> <out.torrent> <v1|v2|hybrid> [piece_size] [private]
    lt_peer.py magnet <torrent>
    lt_peer.py seed <torrent> <save_dir> <port>
    lt_peer.py download <torrent|magnet> <save_dir> <port> <peer_host:port> <seconds>
    lt_peer.py identify-peer <torrent|magnet> <save_dir> <port> <peer_host:port> <seconds>

`seed` prints READY once its data is checked and it is listening, then runs
until killed. Both `seed` and `download` print `encryption: rc4`,
`encryption: plaintext` (encrypted handshake only), or `encryption: none`
for each kind of peer connection they see, and `transport: utp` or
`transport: tcp`. Set LT_ENCRYPTION=forced to allow only fully
RC4-encrypted connections, or LT_ENCRYPTION=disabled for plain text only.
Set LT_TRANSPORT=utp or LT_TRANSPORT=tcp to use only that transport.
Set LT_UPLOAD_LIMIT to cap uploads, in bytes per second.
Set LT_LSD=1 to turn on local peer discovery (BEP 14); the session then
listens on every interface, since LSD peers connect to network addresses.
`download` with `none` as the peer relies on discovery alone.
Set LT_LISTEN to listen on another address, such as `[::1]` for IPv6;
peers may be given as `[::1]:port`.
Otherwise libtorrent's defaults apply: encryption enabled, and uTP tried
before TCP. `download` connects to the given peer and exits 0 once
complete, 1 on timeout. Progress lines go to stdout for test failure logs.
"""
import os
import sys
import time
import warnings
from pathlib import Path

HERE = Path(__file__).resolve().parent
_marker = HERE / ".libtorrent" / "current"
if _marker.exists():
    sys.path.insert(0, str(HERE / ".libtorrent" / _marker.read_text().strip()))
warnings.simplefilter("ignore", DeprecationWarning)
import libtorrent as lt  # noqa: E402


def encryption_settings():
    mode = os.environ.get("LT_ENCRYPTION", "default")
    if mode == "forced":
        return {
            "out_enc_policy": int(lt.enc_policy.forced),
            "in_enc_policy": int(lt.enc_policy.forced),
            "allowed_enc_level": int(lt.enc_level.rc4),
        }
    if mode == "disabled":
        return {
            "out_enc_policy": int(lt.enc_policy.disabled),
            "in_enc_policy": int(lt.enc_policy.disabled),
        }
    assert mode == "default", f"unknown LT_ENCRYPTION {mode!r}"
    return {}


def transport_settings():
    mode = os.environ.get("LT_TRANSPORT", "default")
    if mode == "utp":
        return {"enable_outgoing_tcp": False, "enable_incoming_tcp": False}
    if mode == "tcp":
        return {"enable_outgoing_utp": False, "enable_incoming_utp": False}
    assert mode == "default", f"unknown LT_TRANSPORT {mode!r}"
    return {}


def session(port):
    limit = int(os.environ.get("LT_UPLOAD_LIMIT", "0"))
    lsd = os.environ.get("LT_LSD") == "1"
    return lt.session({
        **encryption_settings(),
        **transport_settings(),
        "upload_rate_limit": limit,
        "listen_interfaces": f"{os.environ.get('LT_LISTEN') or ('0.0.0.0' if lsd else '127.0.0.1')}:{port}",
        "enable_dht": False,
        "enable_lsd": lsd,
        "enable_upnp": False,
        "enable_natpmp": False,
        # Every test peer is on 127.0.0.1.
        "allow_multiple_connections_per_ip": True,
        "alert_mask": lt.alert.category_t.error_notification
        | lt.alert.category_t.peer_notification
        | lt.alert.category_t.connect_notification,
    })


def make(source, out, kind, piece_size="65536", private="public"):
    files = lt.file_storage()
    lt.add_files(files, source)
    flags = {"v1": lt.create_torrent.v1_only, "v2": lt.create_torrent.v2_only, "hybrid": 0}[kind]
    torrent = lt.create_torrent(files, int(piece_size), flags=flags)
    # Private torrents (BEP 27) turn off peer exchange and the DHT.
    torrent.set_priv(private == "private")
    lt.set_piece_hashes(torrent, os.path.dirname(os.path.abspath(source)))
    Path(out).write_bytes(lt.bencode(torrent.generate()))


def add(ses, source, save, flags=None):
    if source.startswith("magnet:"):
        params = lt.parse_magnet_uri(source)
    else:
        params = lt.add_torrent_params()
        params.ti = lt.torrent_info(source)
    params.save_path = save
    if flags is not None:
        params.flags |= flags
    handle = ses.add_torrent(params)
    # Session limits skip local-network peers; per-torrent limits do not.
    limit = int(os.environ.get("LT_UPLOAD_LIMIT", "0"))
    if limit:
        handle.set_upload_limit(limit)
    return handle


ENCRYPTION_SEEN = set()
TRANSPORT_SEEN = set()
# peer_info::utp_socket, which the Python bindings do not name.
UTP_SOCKET = 1 << 17


def note_encryption(peer):
    """Prints how a connected peer's stream is carried and protected, once
    per kind."""
    if peer.flags & (lt.peer_info.connecting | lt.peer_info.handshake):
        return
    transport = "utp" if peer.flags & UTP_SOCKET else "tcp"
    if transport not in TRANSPORT_SEEN:
        TRANSPORT_SEEN.add(transport)
        print("transport:", transport, flush=True)
    if peer.flags & lt.peer_info.rc4_encrypted:
        kind = "rc4"
    elif peer.flags & lt.peer_info.plaintext_encrypted:
        kind = "plaintext"
    else:
        kind = "none"
    if kind not in ENCRYPTION_SEEN:
        ENCRYPTION_SEEN.add(kind)
        print("encryption:", kind, flush=True)


def report(ses, handle, last):
    for alert in ses.pop_alerts():
        if isinstance(alert, (lt.peer_error_alert, lt.torrent_error_alert,
                              lt.peer_disconnected_alert)):
            print("alert:", alert.message(), flush=True)
    for peer in handle.get_peer_info():
        note_encryption(peer)
    status = handle.status()
    line = f"{status.state} {status.progress * 100:.1f}% peers={status.num_peers}"
    if line != last:
        print(line, flush=True)
    return status, line


def seed(torrent, save, port):
    ses = session(int(port))
    handle = add(ses, torrent, save)
    last, ready = "", False
    # Each peer's progress as libtorrent sees it (from its bitfield and
    # `have` messages), printed as `peer progress: <percent>` when it moves:
    # what qBittorrent's peer list and PeerBanHelper go by.
    seen = {}
    while True:
        status, last = report(ses, handle, last)
        if not ready and status.is_seeding:
            print("READY", flush=True)
            ready = True
        for peer in handle.get_peer_info():
            shown = f"{peer.progress * 100:.0f}"
            if seen.get(peer.ip) != shown:
                seen[peer.ip] = shown
                print(f"peer progress: {shown}", flush=True)
        # Poll often enough to see short-lived local connections.
        time.sleep(0.02)


def download(source, save, port, peer, seconds):
    ses = session(int(port))
    handle = add(ses, source, save)
    host, peer_port = peer.rsplit(":", 1) if peer != "none" else ("", "0")
    host = host.strip("[]")
    deadline = time.time() + int(seconds)
    last, next_connect = "", 0.0
    while time.time() < deadline:
        # Reconnect periodically in case an attempt failed or was dropped.
        if peer != "none" and time.time() >= next_connect:
            handle.connect_peer((host, int(peer_port)))
            next_connect = time.time() + 3
        status, last = report(ses, handle, last)
        if status.is_seeding:
            report(ses, handle, last)
            print("COMPLETE", flush=True)
            return 0
        time.sleep(0.2)
    print("TIMEOUT", flush=True)
    return 1


def identify_peer(source, save, port, peer, seconds):
    """Prints each client name libtorrent shows for the peer: first from its
    peer ID, then from its extension handshake once that arrives. Also
    prints the connection's encryption."""
    ses = session(int(port))
    handle = add(ses, source, save)
    # Throttle so the connection outlives the extension handshake; a fast
    # local download would finish and drop the seed first.
    handle.set_download_limit(1024)
    host, peer_port = peer.rsplit(":", 1)
    host = host.strip("[]")
    deadline = time.time() + int(seconds)
    next_connect, seen = 0.0, []
    while time.time() < deadline:
        if time.time() >= next_connect:
            handle.connect_peer((host, int(peer_port)))
            next_connect = time.time() + 3
        for info in handle.get_peer_info():
            note_encryption(info)
            if "connected" not in seen:
                seen.append("connected")
                print("connected", flush=True)
            client = info.client.decode() if isinstance(info.client, bytes) else info.client
            if client and client not in seen:
                seen.append(client)
                print(f"client: {client}", flush=True)
            if client and client != lt.identify_client(info.pid):
                return 0
        time.sleep(0.05)
    print("TIMEOUT", flush=True)
    return 1

if __name__ == "__main__":
    command, args = sys.argv[1], sys.argv[2:]
    if command == "version":
        print(lt.__version__)
    elif command == "identify":
        print(lt.identify_client(lt.sha1_hash(args[0].encode())))
    elif command == "make":
        make(*args)
    elif command == "magnet":
        print(lt.make_magnet_uri(lt.torrent_info(args[0])))
    elif command == "seed":
        seed(*args)
    elif command == "download":
        sys.exit(download(*args))
    elif command == "identify-peer":
        sys.exit(identify_peer(*args))
    else:
        sys.exit(f"unknown command {command!r}")
