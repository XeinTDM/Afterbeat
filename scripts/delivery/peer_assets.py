"""Fetch and stage only the publisher artifacts pinned in peers.lock.json."""

import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import tempfile
from urllib.request import Request, urlopen
from zipfile import ZipFile


LOCK = Path(__file__).with_name("peers.lock.json")
PEERS = json.loads(LOCK.read_text(encoding="utf-8"))


def verify(path, peer):
    with path.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != peer["sha256"]:
        raise ValueError(f"SHA-256 mismatch for {path}: expected {peer['sha256']}, got {actual}.")


def download_release(path, peer):
    headers = {"User-Agent": "Afterbeat delivery benchmark"}
    requests = [Request(peer["url"], headers=headers)]
    if "github_asset" in peer:
        requests.insert(0, Request(peer["github_asset"], headers={
            **headers, "Accept": "application/octet-stream", "X-GitHub-Api-Version": "2022-11-28",
        }))
    errors = []
    for request in requests:
        try:
            with urlopen(request, timeout=60) as response, path.open("wb") as stream:
                shutil.copyfileobj(response, stream)
            verify(path, peer)
            return
        except (OSError, ValueError) as error:
            errors.append(f"{request.full_url}: {error}")
    raise RuntimeError("\n".join(errors))


def fetch(cache, selected):
    cache.mkdir(parents=True, exist_ok=True)
    failures = []
    for key in selected:
        peer = PEERS[key]
        path = cache / peer["file"]
        try:
            if not path.exists():
                with tempfile.TemporaryDirectory(dir=cache) as temporary:
                    download = Path(temporary) / path.name
                    download_release(download, peer)
                    download.rename(path)
            verify(path, peer)
            print(f"Verified {peer['name']} {peer['version']}: {path}", flush=True)
        except (OSError, RuntimeError, ValueError) as error:
            failures.append(
                f"{peer['name']}: {error}\n"
                f"  Obtain {peer['url']} and save it as {path}. Expected SHA-256: {peer['sha256']}."
            )
    if failures:
        raise RuntimeError("\n".join(failures))


def cached(cache, key):
    peer = PEERS[key]
    archive = cache / peer["file"]
    if not archive.is_file():
        raise ValueError(f"Missing {archive}. Run fetch-peers with the same --cache.")
    verify(archive, peer)
    return archive


def stage(archive, key, destination):
    peer = PEERS[key]
    executable = destination / peer["entry"]
    destination.mkdir(parents=True)
    if archive.suffix == ".zip":
        with ZipFile(archive) as package:
            for member in package.infolist():
                path = PurePosixPath(member.filename)
                if path.is_absolute() or ".." in path.parts or any(
                    ":" in part or "\\" in part for part in path.parts
                ):
                    raise ValueError(f"Invalid package entry: {member.filename}")
            package.extractall(destination)
    else:
        shutil.copyfile(archive, executable)
    if not executable.is_file():
        raise ValueError(f"{archive} did not contain {peer['entry']}.")
    return executable
