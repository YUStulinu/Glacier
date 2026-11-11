"""Backup and restore speed, and repository size, against a full archive every time.

    python -m glacier_eval.bench

1. Kernel (every version is read once beforehand, so both tools find it in the OS cache):
   back up linux-6.0 ... linux-6.7 into one repository (each version is a new folder, so
   every file is read; only new chunks are stored). Then back up linux-6.7 again (nothing
   changed: the incremental path), restore linux-6.7 and compare every file's SHA-256.
   Baseline: a tar + zstd -3 archive of each version (Python's tarfile and compression.zstd),
   and extracting the last one.
2. Project history: rebuild the projects folder as it was after each of its commits, in time
   order, and back it up after each one (like a daily backup of a folder that keeps changing).
   Baseline: a tar + zstd -3 archive of the folder after each commit.

Writes results/bench.json.
"""
from __future__ import annotations

import json
import os
import shutil
import tarfile
import time
from pathlib import Path

from .common import DATA, KERNEL, KERNEL_VERSIONS, PROJECTS, dir_size, glacier, glacier_json, save, timed, tree_digest

BENCH = DATA / "bench"


def tar_zst(src: Path, out: Path) -> float:
    t0 = time.perf_counter()
    with tarfile.open(out, "w:zst", level=3) as t:
        t.add(src, arcname=src.name)
    return time.perf_counter() - t0


def untar_zst(arc: Path, dest: Path) -> float:
    t0 = time.perf_counter()
    with tarfile.open(arc, "r:zst") as t:
        t.extractall(dest, filter="data")
    return time.perf_counter() - t0


def repo_bytes(repo: Path) -> int:
    return dir_size(repo)[1]


def kernel() -> dict:
    repo = BENCH / "kernel-repo"
    shutil.rmtree(BENCH, ignore_errors=True)
    BENCH.mkdir(parents=True)
    glacier("init", "--repo", repo)
    rows = []
    for v in KERNEL_VERSIONS:
        src = KERNEL / f"linux-{v}"
        files, size = dir_size(src)
        # Read everything once first, so both Glacier and tar find the files in the OS cache.
        glacier("analyze", src, "--whole-file", env=None)
        r, secs = timed(glacier_json, "backup", "--repo", repo, src)
        arc = BENCH / f"linux-{v}.tar.zst"
        tsecs = tar_zst(src, arc)
        rows.append({"version": v, "files": files, "bytes": size, "seconds": secs, "stored_new": r["summary"]["stored_new"],
                     "repo_bytes": repo_bytes(repo), "tar_zst_seconds": tsecs, "tar_zst_bytes": arc.stat().st_size})
        print(f"linux-{v}: {size / 2**20:.0f} MiB, {files} files; glacier {secs:.1f} s, +{r['summary']['stored_new'] / 2**20:.1f} MiB; "
              f"tar.zst {tsecs:.1f} s, {arc.stat().st_size / 2**20:.0f} MiB")
    last = KERNEL / f"linux-{KERNEL_VERSIONS[-1]}"
    r, inc = timed(glacier_json, "backup", "--repo", repo, last)
    print(f"incremental backup of an unchanged linux-{KERNEL_VERSIONS[-1]}: {inc:.2f} s ({r['summary']['files_unchanged']} files unchanged)")
    out = BENCH / "restore"
    snap = glacier_json("snapshots", "--repo", repo)[-1]["id"]
    rs, rsecs = timed(glacier_json, "restore", snap, "--repo", repo, "--target", out)
    same = tree_digest(out / last.name) == tree_digest(last)
    shutil.rmtree(out)
    usecs = untar_zst(BENCH / f"linux-{KERNEL_VERSIONS[-1]}.tar.zst", out)
    shutil.rmtree(out)
    check = glacier("check", "--repo", repo, "--read-data", check=False)
    res = {"versions": rows, "incremental_unchanged_seconds": inc, "restore_seconds": rsecs, "restore_bytes": rs["bytes"], "restore_identical": same,
           "untar_seconds": usecs, "repo_bytes": repo_bytes(repo), "tar_zst_total_bytes": sum(r["tar_zst_bytes"] for r in rows),
           "logical_bytes": sum(r["bytes"] for r in rows), "check_ok": check.returncode == 0, "cpus": os.cpu_count()}
    print(f"restore linux-{KERNEL_VERSIONS[-1]}: {rsecs:.1f} s (identical: {same}); untar.zst {usecs:.1f} s; check ok: {res['check_ok']}")
    print(f"total: {res['logical_bytes'] / 2**30:.2f} GiB of data -> repository {res['repo_bytes'] / 2**20:.0f} MiB, "
          f"tar.zst archives {res['tar_zst_total_bytes'] / 2**20:.0f} MiB")
    for f in BENCH.glob("*.tar.zst"):
        f.unlink()
    return res


def projects() -> dict:
    commits = json.loads((PROJECTS / "commits.json").read_text())
    repo = BENCH / "projects-repo"
    work = BENCH / "ProiecteVSCode"
    shutil.rmtree(repo, ignore_errors=True)
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    glacier("init", "--repo", repo)
    rows = []
    tar_total = 0
    tar_secs = 0.0
    for i, c in enumerate(commits):
        d = work / c["repo"]
        shutil.rmtree(d, ignore_errors=True)
        d.mkdir()
        with tarfile.open(PROJECTS / "commits" / c["tar"]) as t:
            t.extractall(d, filter="data")
        r, secs = timed(glacier_json, "backup", "--repo", repo, work)
        arc = BENCH / "state.tar.zst"
        tar_secs += tar_zst(work, arc)
        tar_total += arc.stat().st_size
        s = r["summary"]
        rows.append({"i": i, "files": s["files"], "bytes": s["bytes"], "seconds": secs, "bytes_read": s["bytes_read"], "stored_new": s["stored_new"],
                     "files_unchanged": s["files_unchanged"]})
    arc.unlink()
    files, size = dir_size(repo)
    res = {"snapshots": len(rows), "rows": rows, "logical_bytes": sum(r["bytes"] for r in rows), "repo_bytes": size, "repo_files": files,
           "tar_zst_total_bytes": tar_total, "tar_zst_seconds": tar_secs, "glacier_seconds": sum(r["seconds"] for r in rows),
           "final_bytes": rows[-1]["bytes"], "final_files": rows[-1]["files"]}
    out = BENCH / "restore-projects"
    snap = glacier_json("snapshots", "--repo", repo)[-1]["id"]
    _, res["restore_seconds"] = timed(glacier_json, "restore", snap, "--repo", repo, "--target", out)
    res["restore_identical"] = tree_digest(out / "ProiecteVSCode") == tree_digest(work)
    shutil.rmtree(out)
    print(f"projects: {len(rows)} snapshots, {res['logical_bytes'] / 2**20:.0f} MiB backed up in total -> repository {size / 2**20:.1f} MiB "
          f"(tar.zst every time: {tar_total / 2**20:.0f} MiB); glacier {res['glacier_seconds']:.0f} s total, tar.zst {tar_secs:.0f} s; "
          f"restore {res['restore_seconds']:.1f} s identical={res['restore_identical']}")
    return res


def main():
    res = {"kernel": kernel(), "projects": projects()}
    save("bench.json", res)


if __name__ == "__main__":
    main()
