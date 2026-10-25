#!/usr/bin/env python3
"""Runs a real qBittorrent for EmpireBT's opt-in importer tests.

Starts qBittorrent (the linuxserver Docker image) with torrents in every
case the importer handles, prints one line of JSON once it is ready:

    {"url": "http://127.0.0.1:PORT", "username": "admin",
     "password": "...", "downloads": "/host/folder/for/downloads",
     "version": "v5.3.0"}

then keeps it running until its standard input closes, and stops it.
qBittorrent sees the data as /downloads/...; `downloads` is that folder
here, for `--map /downloads=<downloads>`.

    python3 interop/qbittorrent.py [VERSION]    # default 5.3.0

The tests in crates/app/tests/qbittorrent.rs run it (see TESTING.md).
Needs Docker, and libtorrent set up for interop (setup_libtorrent.py) to
make the .torrent files.
"""

import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from http.cookiejar import CookieJar
from pathlib import Path

HERE = Path(__file__).resolve().parent
VERSION = sys.argv[1] if len(sys.argv) > 1 else "5.3.0"
IMAGE = f"lscr.io/linuxserver/qbittorrent:{VERSION}"
# Docker installed as a snap cannot see /tmp; work under the home folder.
WORK = Path.home() / f".cache/empirebt-qbittorrent-{VERSION}"
PIECE = "16384"


def log(message):
    print(message, file=sys.stderr, flush=True)


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def make_torrent(source, out, kind="v1"):
    run(sys.executable, "-I", str(HERE / "lt_peer.py"), "make", str(source), str(out), kind, PIECE,
        stdout=subprocess.DEVNULL)


def pattern(size, seed):
    return bytes((index * seed + seed) % 251 for index in range(size))


class Api:
    def __init__(self, base):
        self.base = base
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(CookieJar()))

    def login(self, username, password):
        """Signs in; True when qBittorrent accepted it (200 "Ok." before
        5.2, 204 since)."""
        body = urllib.parse.urlencode({"username": username, "password": password}).encode()
        request = urllib.request.Request(self.base + "/api/v2/auth/login", data=body, headers={
            "Referer": self.base, "Content-Type": "application/x-www-form-urlencoded",
        })
        try:
            with self.opener.open(request, timeout=30) as response:
                return 200 <= response.status < 300 and not response.read().startswith(b"Fails")
        except urllib.error.HTTPError:
            return False

    def post(self, path, fields=None, files=None):
        headers = {"Referer": self.base}
        if files:
            boundary = uuid.uuid4().hex
            body = b""
            for name, value in (fields or {}).items():
                body += f"--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n".encode()
            for name, file in files.items():
                body += (
                    f"--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{Path(file).name}\"\r\n"
                    "Content-Type: application/x-bittorrent\r\n\r\n"
                ).encode() + Path(file).read_bytes() + b"\r\n"
            body += f"--{boundary}--\r\n".encode()
            headers["Content-Type"] = f"multipart/form-data; boundary={boundary}"
        else:
            body = urllib.parse.urlencode(fields or {}).encode()
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        request = urllib.request.Request(self.base + path, data=body, headers=headers)
        with self.opener.open(request, timeout=30) as response:
            return response.read().decode()

    def get(self, path):
        request = urllib.request.Request(self.base + path, headers={"Referer": self.base})
        with self.opener.open(request, timeout=30) as response:
            return response.read().decode()


def prepare_data():
    """Makes the data and .torrent files; returns (torrents, downloads)."""
    if WORK.exists():
        # qBittorrent's files belong to the container's user.
        run("docker", "run", "--rm", "-v", f"{WORK}:/work", "alpine", "rm", "-rf",
            "/work/config", "/work/downloads", stdout=subprocess.DEVNULL)
        shutil.rmtree(WORK, ignore_errors=True)
    sources, torrents, downloads = WORK / "sources", WORK / "torrents", WORK / "downloads"
    for folder in (sources, torrents, downloads / "linux", downloads / "complete", downloads / "flat",
                   downloads / "incomplete", WORK / "config"):
        folder.mkdir(parents=True, exist_ok=True)

    # 1. Complete, in the Linux category's folder, automatic management on.
    (sources / "mini.iso").write_bytes(pattern(40000, 3))
    make_torrent(sources / "mini.iso", torrents / "mini.torrent")
    shutil.copy(sources / "mini.iso", downloads / "linux/mini.iso")

    # 2. Complete multi-file album; one file, and the torrent's name, are
    #    renamed in qBittorrent.
    album = sources / "album"
    album.mkdir()
    for number, size in enumerate((20000, 30000, 25000), start=1):
        (album / f"{number:02}.flac").write_bytes(pattern(size, number + 10))
    make_torrent(album, torrents / "album.torrent")
    shutil.copytree(album, downloads / "complete/album")

    # 3. Half downloaded, stopped, in the incomplete folder.
    data = pattern(16384 * 4, 7)
    (sources / "partial.bin").write_bytes(data)
    make_torrent(sources / "partial.bin", torrents / "partial.torrent")
    (downloads / "incomplete/partial.bin").write_bytes(data[: 16384 * 2] + bytes(16384 * 2))

    # 4. Two files, one skipped; only one is on disk.
    pair = sources / "pair"
    pair.mkdir()
    (pair / "wanted.txt").write_bytes(pattern(16384 * 2, 5))
    (pair / "skipped.txt").write_bytes(pattern(16384 * 2, 9))
    make_torrent(pair, torrents / "pair.torrent")
    (downloads / "complete/pair").mkdir()
    shutil.copy(pair / "wanted.txt", downloads / "complete/pair/wanted.txt")

    # 5. A hybrid (v1 + v2) torrent, complete.
    (sources / "hybrid.dat").write_bytes(pattern(50000, 13))
    make_torrent(sources / "hybrid.dat", torrents / "hybrid.torrent", "hybrid")
    shutil.copy(sources / "hybrid.dat", downloads / "complete/hybrid.dat")

    # 6. Two files laid out without the torrent's folder, complete.
    flat = sources / "flat"
    flat.mkdir()
    (flat / "one.txt").write_bytes(pattern(20000, 17))
    (flat / "two.txt").write_bytes(pattern(30000, 19))
    make_torrent(flat, torrents / "flat.torrent")
    for name in ("one.txt", "two.txt"):
        shutil.copy(flat / name, downloads / "flat" / name)
    return torrents, downloads


def start(downloads, port):
    """Starts qBittorrent; returns (container, api, password)."""
    container = subprocess.run(
        ["docker", "run", "-d", "--rm", "-e", f"PUID={os.getuid()}", "-e", f"PGID={os.getgid()}",
         "-e", f"WEBUI_PORT={port}", "-p", f"127.0.0.1:{port}:{port}",
         "-v", f"{WORK / 'config'}:/config", "-v", f"{downloads}:/downloads", IMAGE],
        check=True, capture_output=True, text=True,
    ).stdout.strip()
    password, logs = None, ""
    for _ in range(90):
        time.sleep(1)
        logs = subprocess.run(["docker", "logs", container], capture_output=True, text=True).stdout
        found = re.search(r"temporary password is provided for this session: (\S+)", logs)
        if found:
            password = found.group(1)
            break
    if not password:
        stop(container)
        sys.exit("qBittorrent did not start:\n" + logs)
    api = Api(f"http://127.0.0.1:{port}")
    for _ in range(30):
        try:
            if api.login("admin", password):
                return container, api, password
        except OSError:
            pass
        time.sleep(1)
    stop(container)
    sys.exit("could not sign in to qBittorrent")


def add_torrents(api, torrents):
    api.post("/api/v2/app/setPreferences", {"json": json.dumps({
        "save_path": "/downloads/complete",
        "temp_path_enabled": True,
        "temp_path": "/downloads/incomplete",
        "auto_tmm_enabled": False,
    })})
    api.post("/api/v2/torrents/createCategory", {"category": "Linux", "savePath": "/downloads/linux"})
    api.post("/api/v2/torrents/createCategory", {
        "category": "Films", "savePath": "/downloads/films",
        "downloadPathEnabled": "true", "downloadPath": "/downloads/films-incomplete",
    })
    api.post("/api/v2/torrents/createCategory", {"category": "Empty"})
    api.post("/api/v2/torrents/createTags", {"tags": "iso,music,unused"})

    def add(name, **fields):
        fields = {key: str(value).lower() if isinstance(value, bool) else value for key, value in fields.items()}
        api.post("/api/v2/torrents/add", fields, {"torrents": torrents / f"{name}.torrent"})

    add("mini", category="Linux", tags="iso", autoTMM=True)
    add("album", savepath="/downloads/complete", tags="music", autoTMM=False)
    add("partial", savepath="/downloads/complete", stopped=True, autoTMM=False)
    add("pair", savepath="/downloads/complete", autoTMM=False, stopped=True)
    add("hybrid", category="Films", savepath="/downloads/complete", autoTMM=False)
    add("flat", savepath="/downloads/flat", contentLayout="NoSubfolder", autoTMM=False)
    api.post("/api/v2/torrents/add", {
        "urls": "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=never-resolved"
                "&tr=udp%3A%2F%2Ftracker.example%3A6969%2Fannounce",
        "stopped": "true", "savepath": "/downloads/complete",
    })
    hashes = {}
    for _ in range(30):
        time.sleep(1)
        hashes = {torrent["name"]: torrent["hash"] for torrent in json.loads(api.get("/api/v2/torrents/info"))}
        if len(hashes) == 7:
            break
    api.post("/api/v2/torrents/renameFile", {
        "hash": hashes["album"], "oldPath": "album/02.flac", "newPath": "album/02 - renamed.flac",
    })
    api.post("/api/v2/torrents/rename", {"hash": hashes["album"], "name": "Album (renamed)"})
    api.post("/api/v2/torrents/filePrio", {"hash": hashes["pair"], "id": "0", "priority": "0"})
    # qBittorrent keeps trackers in its resume data, not its .torrent copy.
    api.post("/api/v2/torrents/addTrackers", {
        "hash": hashes["mini.iso"], "urls": "http://tracker.example/announce",
    })
    # Torrents added stopped are not checked; check these, as their
    # progress would be in real use.
    api.post("/api/v2/torrents/recheck", {"hashes": f"{hashes['partial.bin']}|{hashes['pair']}"})
    # Settled: complete ones seeding, the half-done one checked.
    for _ in range(60):
        time.sleep(1)
        info = {torrent["name"]: torrent for torrent in json.loads(api.get("/api/v2/torrents/info"))}
        if (info["mini.iso"]["progress"] == 1 and info["hybrid.dat"]["progress"] == 1
                and info["Album (renamed)"]["progress"] == 1 and info["flat"]["progress"] == 1
                and info["partial.bin"]["progress"] == 0.5 and not info["pair"]["state"].startswith("checking")):
            return
    log("qBittorrent did not settle; the test may fail")


def stop(container):
    subprocess.run(["docker", "stop", "-t", "10", container], capture_output=True)


def main():
    torrents, downloads = prepare_data()
    container, api, password = start(downloads, free_port())
    try:
        add_torrents(api, torrents)
        print(json.dumps({
            "url": api.base, "username": "admin", "password": password,
            "downloads": str(downloads), "version": api.get("/api/v2/app/version").strip(),
        }), flush=True)
        # Until the test is done.
        sys.stdin.read()
    finally:
        stop(container)


if __name__ == "__main__":
    main()
