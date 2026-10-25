#!/usr/bin/env python3
"""Downloads the libtorrent Python bindings used by the interop tests.

Fetches the official wheel for this Python version and platform from PyPI,
or from libtorrent's GitHub release when PyPI does not have that version
(2.1.2 was released only on GitHub), checks it against the SHA-256 that
PyPI or GitHub publishes, and unpacks it into interop/.libtorrent/<version>/
(ignored by git). Nothing is installed into
the system or any Python environment.

    python3 interop/setup_libtorrent.py            # the pinned version
    python3 interop/setup_libtorrent.py --version 2.0.11
"""
import argparse
import hashlib
import json
import platform
import sys
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

# The version the interop suite is known to pass against.
PINNED_VERSION = "2.1.2"
HERE = Path(__file__).resolve().parent
TARGET = HERE / ".libtorrent"


def platform_tags():
    """Substrings a wheel filename must contain to run here."""
    machine = platform.machine().lower()
    if sys.platform.startswith("linux"):
        arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
        return ["manylinux", arch]
    if sys.platform == "darwin":
        return ["macosx", "arm64" if machine == "arm64" else "x86_64"]
    if sys.platform == "win32":
        return ["win_amd64" if machine in ("amd64", "x86_64") else "win32"]
    sys.exit(f"Unsupported platform: {sys.platform} {machine}")


def release_files(version):
    """A release's files, each with its download URL and published SHA-256:
    from PyPI, or else from libtorrent's GitHub release."""
    try:
        with urllib.request.urlopen(f"https://pypi.org/pypi/libtorrent/{version}/json") as response:
            release = json.load(response)
        return [
            {"filename": entry["filename"], "url": entry["url"],
             "sha256": entry["digests"]["sha256"], "source": "PyPI"}
            for entry in release["urls"]
        ]
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
    url = f"https://api.github.com/repos/arvidn/libtorrent/releases/tags/v{version}"
    try:
        with urllib.request.urlopen(url) as response:
            release = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            sys.exit(f"libtorrent {version} is neither on PyPI nor a GitHub release")
        raise
    # GitHub publishes each file's SHA-256 as "sha256:<hex>"; files
    # without one cannot be checked, so they are not offered.
    return [
        {"filename": asset["name"], "url": asset["browser_download_url"],
         "sha256": asset["digest"].removeprefix("sha256:"), "source": "GitHub"}
        for asset in release.get("assets", [])
        if (asset.get("digest") or "").startswith("sha256:")
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", default=PINNED_VERSION)
    version = parser.parse_args().version
    destination = TARGET / version
    if (destination / "libtorrent").exists():
        print(f"libtorrent {version} is already set up in {destination}")
        return

    python_tag = f"cp{sys.version_info.major}{sys.version_info.minor}"
    files = release_files(version)
    tags = platform_tags()
    wheels = [
        entry for entry in files
        if entry["filename"].endswith(".whl")
        and f"-{python_tag}-" in entry["filename"]
        and all(tag in entry["filename"] for tag in tags)
    ]
    if not wheels:
        available = sorted({entry["filename"].split("-")[2] for entry in files
                            if entry["filename"].endswith(".whl")})
        sys.exit(
            f"libtorrent {version} has no wheel for Python {python_tag} on {' '.join(tags)}.\n"
            f"Wheels exist for: {', '.join(available)}. Try another Python or --version."
        )
    wheel = wheels[0]
    print(f"Downloading {wheel['filename']} from {wheel['source']}")
    with urllib.request.urlopen(wheel["url"]) as response:
        data = response.read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != wheel["sha256"]:
        sys.exit(f"Checksum mismatch for {wheel['filename']}: got {digest}")

    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / wheel["filename"]
    archive.write_bytes(data)
    with zipfile.ZipFile(archive) as contents:
        contents.extractall(destination)
    archive.unlink()
    (TARGET / "current").write_text(version + "\n")
    print(f"libtorrent {version} is ready in {destination}")


if __name__ == "__main__":
    main()
