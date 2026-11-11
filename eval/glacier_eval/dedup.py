"""How much space each chunking strategy needs on the same data (`glacier analyze`).

    python -m glacier_eval.dedup

Datasets:
- kernel: Linux 6.0 to 6.7 side by side (eight versions of one code base);
- projects: every commit of every project repository, each extracted to its own directory
  (data/projects/trees/<repo>-<sha>).

Strategies: FastCDC with averages of 16 KiB, 64 KiB (Glacier's default), 256 KiB, 1 MiB
(restic's average chunk size) and 2 MiB (Borg's default), fixed 64 KiB chunks, and whole-file
deduplication. "Stored" is the zstd-compressed size of the unique chunks (encryption adds 41
bytes per chunk).

Writes results/dedup.json.
"""
from __future__ import annotations

import json
import tarfile

from .common import GLACIER, KERNEL, KERNEL_VERSIONS, PROJECTS, glacier, save

STRATEGIES = [("--avg", "16KiB"), ("--avg", "64KiB"), ("--avg", "256KiB"), ("--avg", "1MiB"), ("--avg", "2MiB"), ("--fixed", "64KiB"), ("--whole-file",)]


def project_trees():
    out = PROJECTS / "trees"
    commits = json.loads((PROJECTS / "commits.json").read_text())
    for c in commits:
        d = out / f"{c['repo']}-{c['sha']}"
        if not d.exists():
            d.mkdir(parents=True)
            with tarfile.open(PROJECTS / "commits" / c["tar"]) as t:
                t.extractall(d, filter="data")
    return sorted(out.iterdir())


def run(roots) -> list[dict]:
    rows = []
    for s in STRATEGIES:
        r = json.loads(glacier("analyze", *roots, *s, env=None).stdout)
        rows.append(r)
        print(f"  {r['strategy']:16s} logical {r['logical_bytes'] / 2**20:9.1f} MiB  unique {r['unique_bytes'] / 2**20:8.1f} MiB  "
              f"stored {r['unique_compressed_bytes'] / 2**20:8.1f} MiB  dedup x{r['dedup_ratio']:.2f}  total x{r['total_ratio']:.2f}  "
              f"{r['unique_chunks']} chunks  {r['seconds']:.1f} s")
    return rows


def main():
    assert GLACIER.exists(), "build the release binary first"
    out = {}
    print("kernel 6.0-6.7")
    out["kernel"] = run([KERNEL / f"linux-{v}" for v in KERNEL_VERSIONS])
    print("project history")
    out["projects"] = run(project_trees())
    save("dedup.json", out)


if __name__ == "__main__":
    main()
