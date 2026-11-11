"""Integrity: deliberately corrupted repositories are detected, and never restored silently.

    python -m glacier_eval.corrupt [--trials 200]

A repository holds a few snapshots of the project history. Each trial flips one random bit in
one random repository file (a pack, an index, a snapshot, the key file or the config), then:

1. `glacier check --read-data` must fail;
2. a full restore of the newest snapshot must not produce a file with wrong content: each file
   is either identical to the source or reported as an error (and not written).

The bit is flipped back after each trial. Writes results/corrupt.json.
"""
from __future__ import annotations

import argparse
import json
import random
import shutil
import tarfile
from collections import Counter

from .common import DATA, PROJECTS, glacier, glacier_json, save, tree_digest

WORK = DATA / "corrupt"


def build() -> tuple:
    shutil.rmtree(WORK, ignore_errors=True)
    src = WORK / "src"
    src.mkdir(parents=True)
    repo = WORK / "repo"
    glacier("init", "--repo", repo, "--kdf-mem-mib", "8", "--kdf-time", "1")
    commits = json.loads((PROJECTS / "commits.json").read_text())
    # A handful of states of a few repositories: enough files of all kinds, fast to check.
    picked = [c for c in commits if c["repo"] in {c2["repo"] for c2 in commits[-40:]}][-12:]
    for c in picked:
        d = src / c["repo"]
        shutil.rmtree(d, ignore_errors=True)
        d.mkdir()
        with tarfile.open(PROJECTS / "commits" / c["tar"]) as t:
            t.extractall(d, filter="data")
        glacier_json("backup", "--repo", repo, src)
    return src, repo


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trials", type=int, default=200)
    a = ap.parse_args()
    src, repo = build()
    want = tree_digest(src)
    files = sorted(p for p in repo.rglob("*") if p.is_file() and "locks" not in p.parts)
    rng = random.Random(11)
    kinds = Counter()
    detected = Counter()
    silent_wrong = 0
    restore_reported = 0
    for i in range(a.trials):
        f = rng.choice(files)
        kind = f.relative_to(repo).parts[0]
        data = bytearray(f.read_bytes())
        k = rng.randrange(len(data))
        bit = 1 << rng.randrange(8)
        data[k] ^= bit
        f.write_bytes(bytes(data))
        try:
            c = glacier("check", "--repo", repo, "--read-data", check=False)
            kinds[kind] += 1
            detected[kind] += c.returncode != 0
            out = WORK / "out"
            shutil.rmtree(out, ignore_errors=True)
            r = glacier("restore", "latest", "--repo", repo, "--target", out, check=False)
            got = tree_digest(out / src.name) if (out / src.name).exists() else {}
            wrong = sum(1 for p, h in got.items() if want.get(p) != h)
            silent_wrong += wrong
            restore_reported += r.returncode != 0
            if c.returncode == 0 or wrong:
                print(f"trial {i}: {f.relative_to(repo)} byte {k}: check exit {c.returncode}, {wrong} wrong files")
        finally:
            data[k] ^= bit
            f.write_bytes(bytes(data))
    assert glacier("check", "--repo", repo, "--read-data", check=False).returncode == 0
    summary = {"trials": a.trials, "detected": sum(detected.values()), "by_file_kind": {k: {"trials": kinds[k], "detected": detected[k]} for k in sorted(kinds)},
               "restores_with_wrong_content": silent_wrong, "restores_reporting_errors": restore_reported, "repository_files": len(files)}
    print(summary)
    save("corrupt.json", summary)
    shutil.rmtree(WORK)


if __name__ == "__main__":
    main()
