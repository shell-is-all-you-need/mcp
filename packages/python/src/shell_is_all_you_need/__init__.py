"""Launcher for the shell-is-all-you-need release binary."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import platform
import sys
import tempfile
from urllib.request import Request, urlopen

__version__ = "0.1.2"
_REPOSITORY = os.environ.get(
    "SHELL_IS_ALL_YOU_NEED_GITHUB_REPOSITORY", "shell-is-all-you-need/mcp"
)


def _target() -> tuple[str, str]:
    system = platform.system().lower()
    machine = platform.machine().lower()
    aliases = {"amd64": "x86_64", "x64": "x86_64", "arm64": "aarch64"}
    machine = aliases.get(machine, machine)
    targets = {
        ("linux", "x86_64"): ("x86_64-unknown-linux-musl", ""),
        ("linux", "aarch64"): ("aarch64-unknown-linux-musl", ""),
        ("darwin", "x86_64"): ("x86_64-apple-darwin", ""),
        ("darwin", "aarch64"): ("aarch64-apple-darwin", ""),
        ("windows", "x86_64"): ("x86_64-pc-windows-msvc", ".exe"),
        ("windows", "aarch64"): ("aarch64-pc-windows-msvc", ".exe"),
    }
    try:
        return targets[(system, machine)]
    except KeyError as error:
        raise RuntimeError(f"unsupported platform: {system}/{machine}") from error


def _cache_root() -> Path:
    override = os.environ.get("SHELL_IS_ALL_YOU_NEED_CACHE_DIR")
    if override:
        return Path(override).expanduser()
    system = platform.system().lower()
    if system == "windows" and os.environ.get("LOCALAPPDATA"):
        return Path(os.environ["LOCALAPPDATA"]) / "shell-is-all-you-need"
    if system == "darwin":
        return Path.home() / "Library" / "Caches" / "shell-is-all-you-need"
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "shell-is-all-you-need"


def _download(url: str) -> bytes:
    request = Request(url, headers={"User-Agent": f"shell-is-all-you-need-pypi/{__version__}"})
    with urlopen(request, timeout=60) as response:
        return response.read()


def _binary() -> Path:
    override = os.environ.get("SHELL_IS_ALL_YOU_NEED_BINARY")
    if override:
        return Path(override).expanduser().resolve()

    triple, extension = _target()
    asset = f"shell-is-all-you-need-{triple}{extension}"
    destination = _cache_root() / __version__ / asset
    if destination.is_file():
        return destination

    destination.parent.mkdir(parents=True, exist_ok=True)
    base = f"https://github.com/{_REPOSITORY}/releases/download/v{__version__}/{asset}"
    binary = _download(base)
    checksum_text = _download(base + ".sha256").decode("ascii").strip()
    expected = checksum_text.split()[0] if checksum_text else ""
    if len(expected) != 64 or any(ch not in "0123456789abcdefABCDEF" for ch in expected):
        raise RuntimeError("release checksum has an invalid format")
    actual = hashlib.sha256(binary).hexdigest()
    if actual.lower() != expected.lower():
        raise RuntimeError(f"SHA-256 mismatch for {asset}")

    fd, temporary_name = tempfile.mkstemp(prefix=asset + ".", dir=destination.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(binary)
        if os.name != "nt":
            temporary.chmod(0o755)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def main() -> None:
    binary = _binary()
    os.execv(binary, [str(binary), *sys.argv[1:]])
