"""Consistency after interruptions: a backup killed at a random moment leaves a valid repository,
and the next backup resumes from what was already uploaded.

    python -m glacier_eval.interrupt [--rounds 15]

Each round starts from an empty repository, runs `glacier backup` of a kernel version and kills
it (TerminateProcess on Windows, SIGKILL elsewhere) after a random fraction of the time a
complete backup takes (measured beforehand with the files already in the OS cache, so the kill
lands while data is being written). Then:

1. `glacier check --read-data` must succeed: every snapshot, tree, index entry and stored chunk
   verifies; packs written after the killed run's last index are only "unreferenced";
2. the backup is run again to completion: chunks the killed run indexed are not uploaded again
   (an index is written every 8 packs);
3. in the last round, the restore must equal the source byte for byte, and `prune` must remove
   the unreferenced packs.

Writes results/interrupt.json.
"""
from __future__ import annotations

import argparse
import random
import shutil
import subprocess
import time

from .common import DATA, GLACIER, KERNEL, dir_size, env, glacier, glacier_json, save, tree_digest

WORK = DATA / "interrupt"


def fresh_repo(path):
    shutil.rmtree(path, ignore_errors=True)
    glacier("init", "--repo", path, "--kdf-mem-mib", "8", "--kdf-time", "1")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=15)
    ap.add_argument("--version", default="6.0")
    a = ap.parse_args()
    src = KERNEL / f"linux-{a.version}"
    shutil.rmtree(WORK, ignore_errors=True)
    WORK.mkdir(parents=True)
    repo = WORK / "repo"
    # Warm the cache, then time a complete backup into an empty repository.
    fresh_repo(repo)
    glacier("backup", "--repo", repo, src)
    fresh_repo(repo)
    t0 = time.perf_counter()
    full = glacier_json("backup", "--repo", repo, src)
    full_s = time.perf_counter() - t0
    full_stored = full["summary"]["stored_new"]
    rng = random.Random(7)
    rounds = []
    for r in range(a.rounds):
        fresh_repo(repo)
        delay = rng.uniform(0.1, 0.9) * full_s
        p = subprocess.Popen([str(GLACIER), "backup", "--repo", str(repo), str(src)], env=env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(delay)
        finished = p.poll() is not None
        p.kill()
        p.wait()
        glacier("unlock", "--repo", repo)  # the killed process could not remove its lock
        c = glacier_json("check", "--repo", repo, "--read-data", check=False)
        kept = dir_size(repo / "data")[1] if (repo / "data").exists() else 0
        resumed = glacier_json("backup", "--repo", repo, src)
        c2 = glacier_json("check", "--repo", repo, "--read-data", check=False)
        row = {"killed_after_s": delay, "finished_before_kill": finished, "check_ok": not c["errors"], "errors": c["errors"][:5],
               "unreferenced_packs": len(c["unreferenced_packs"]), "pack_bytes_at_kill": kept,
               "resumed_stored_new": resumed["summary"]["stored_new"], "check_after_resume_ok": not c2["errors"]}
        rounds.append(row)
        print(f"round {r + 1:2d}: killed after {delay:5.1f} of {full_s:.1f} s ({kept / 2**20:5.0f} MiB of packs on disk); "
              f"check {'ok' if row['check_ok'] else 'FAILED ' + str(c['errors'][:2])}, {row['unreferenced_packs']} unreferenced packs; "
              f"resumed backup stored {row['resumed_stored_new'] / 2**20:.0f} of {full_stored / 2**20:.0f} MiB")
    out = WORK / "restore"
    glacier_json("restore", "latest", "--repo", repo, "--target", out)
    same = tree_digest(out / src.name) == tree_digest(src)
    before = dir_size(repo)[1]
    pr = glacier_json("prune", "--repo", repo)
    after = dir_size(repo)[1]
    final = glacier_json("check", "--repo", repo, "--read-data", check=False)
    summary = {"rounds": len(rounds), "check_failures": sum(not x["check_ok"] for x in rounds) + sum(not x["check_after_resume_ok"] for x in rounds),
               "killed_mid_backup": sum(not x["finished_before_kill"] for x in rounds), "full_backup_seconds": full_s, "full_stored_bytes": full_stored,
               "resumed_upload_fraction_mean": sum(x["resumed_stored_new"] for x in rounds) / len(rounds) / full_stored,
               "final_restore_identical": same, "final_check_ok": not final["errors"], "unreferenced_after_prune": len(final["unreferenced_packs"]),
               "repo_bytes_before_prune": before, "repo_bytes_after_prune": after, "prune": pr}
    print(summary)
    save("interrupt.json", {"summary": summary, "rounds": rounds})
    shutil.rmtree(WORK)


if __name__ == "__main__":
    main()
