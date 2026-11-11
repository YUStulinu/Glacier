"""Prepares the two datasets.

    python -m glacier_eval.fetch [--projects-root C:/VSCode/ProiecteVSCode]

1. Linux kernel 6.0 to 6.7: the release tarballs from cdn.kernel.org, extracted to
   data/kernel/linux-6.N. Entries Windows cannot hold are skipped the same way in every version
   and counted: symlinks, names reserved by Windows (aux.c, con.h, ...), invalid characters,
   and names that differ only in case from an earlier one in the same directory.
2. Project history: every commit of every git repository directly under the projects folder,
   exported with `git archive` to data/projects/commits/ (one tar per commit) plus a JSON list
   in commit-time order. Only tracked files: build outputs, models and datasets are not part of
   it.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import tarfile
import urllib.request
from pathlib import Path

from .common import KERNEL, KERNEL_VERSIONS, PROJECTS

URL = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-{}.tar.xz"
UA = {"User-Agent": "glacier-eval-fetch"}
RESERVED = re.compile(r"^(con|prn|aux|nul|com[1-9]|lpt[1-9])(\..*)?$", re.I)
BAD = re.compile(r'[<>:"|?*\\]')


def windows_safe(members, stats):
    seen: dict[str, set[str]] = {}
    for m in members:
        parts = m.name.split("/")
        name = parts[-1]
        if m.issym() or m.islnk():
            stats["links"] += 1
            continue
        if any(RESERVED.match(p) or BAD.search(p) or p.endswith((".", " ")) for p in parts):
            stats["reserved_or_invalid"] += 1
            continue
        d = "/".join(parts[:-1]).lower()
        low = name.lower()
        s = seen.setdefault(d, set())
        if low in s and not m.isdir():
            stats["case_collisions"] += 1
            continue
        s.add(low)
        yield m


def fetch_kernel():
    KERNEL.mkdir(parents=True, exist_ok=True)
    summary = {}
    for v in KERNEL_VERSIONS:
        out = KERNEL / f"linux-{v}"
        tar = KERNEL / f"linux-{v}.tar.xz"
        if out.exists():
            continue
        if not tar.exists():
            req = urllib.request.Request(URL.format(v), headers=UA)
            with urllib.request.urlopen(req, timeout=600) as r, open(tar, "wb") as f:
                while b := r.read(1 << 20):
                    f.write(b)
        stats = {"links": 0, "reserved_or_invalid": 0, "case_collisions": 0}
        with tarfile.open(tar) as t:
            t.extractall(KERNEL, members=windows_safe(t, stats), filter="data")
        tar.unlink()
        summary[v] = stats
        print(f"linux-{v}: extracted, skipped {stats}")
    if summary:
        (KERNEL / "skipped.json").write_text(json.dumps(summary, indent=1))


def export_projects(root: Path):
    out = PROJECTS / "commits"
    out.mkdir(parents=True, exist_ok=True)
    commits = []
    for repo in sorted(p for p in root.iterdir() if (p / ".git").is_dir()):
        log = subprocess.run(["git", "-C", str(repo), "log", "--all", "--format=%H %ct"], capture_output=True, text=True).stdout.split()
        for sha, ts in zip(log[::2], log[1::2]):
            tar = out / f"{repo.name}-{sha[:12]}.tar"
            if not tar.exists():
                subprocess.run(["git", "-C", str(repo), "archive", "--format=tar", "-o", str(tar), sha], check=True)
            commits.append({"repo": repo.name, "sha": sha[:12], "time": int(ts), "tar": tar.name})
    commits.sort(key=lambda c: (c["time"], c["repo"]))
    (PROJECTS / "commits.json").write_text(json.dumps(commits, indent=1))
    print(f"{len(commits)} commits from {len({c['repo'] for c in commits})} repositories")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--projects-root", type=Path, default=Path(r"C:/VSCode/ProiecteVSCode"))
    a = ap.parse_args()
    export_projects(a.projects_root)
    fetch_kernel()


if __name__ == "__main__":
    main()
