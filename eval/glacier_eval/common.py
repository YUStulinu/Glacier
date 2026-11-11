"""Paths, the glacier binary, and helpers shared by the evaluation scripts."""
from __future__ import annotations

import hashlib
import json
import os
import secrets
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / "data"
RESULTS = ROOT / "results"
DOCS = ROOT / "docs"
EXE = ".exe" if sys.platform == "win32" else ""
GLACIER = ROOT / "core" / "target" / "release" / f"glacier{EXE}"
KERNEL_VERSIONS = [f"6.{i}" for i in range(8)]
KERNEL = DATA / "kernel"
PROJECTS = DATA / "projects"


def password() -> str:
    """A generated test password for the evaluation repositories (kept in data/, never committed)."""
    p = DATA / "test-password.txt"
    if not p.exists():
        DATA.mkdir(exist_ok=True)
        p.write_text(secrets.token_urlsafe(24))
    return p.read_text().strip()


def env(**extra) -> dict:
    e = dict(os.environ)
    e["GLACIER_PASSWORD"] = password()
    e.update(extra)
    return e


def glacier(*args, check=True, **kw) -> subprocess.CompletedProcess:
    return subprocess.run([str(GLACIER), *map(str, args)], capture_output=True, text=True, env=kw.pop("env", env()), check=check, **kw)


def glacier_json(*args, **kw):
    out = glacier(*args, "--json", **kw).stdout
    return json.loads(out)


def timed(fn, *a, **kw):
    t0 = time.perf_counter()
    r = fn(*a, **kw)
    return r, time.perf_counter() - t0


def dir_size(root: Path) -> tuple[int, int]:
    files = size = 0
    for dp, _, fs in os.walk(root):
        for f in fs:
            files += 1
            size += os.path.getsize(os.path.join(dp, f))
    return files, size


def tree_digest(root: Path) -> dict[str, str]:
    """Relative path -> SHA-256 of every file under root."""
    out = {}
    for dp, _, fs in os.walk(root):
        for f in fs:
            p = Path(dp) / f
            out[p.relative_to(root).as_posix()] = hashlib.sha256(p.read_bytes()).hexdigest()
    return out


def save(name: str, obj) -> None:
    RESULTS.mkdir(exist_ok=True)
    (RESULTS / name).write_text(json.dumps(obj, indent=1))
