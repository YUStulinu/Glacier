"""The S3 destination: back up and restore one kernel version through s3lite (the project's own
S3-compatible server, on this machine) and compare with a local repository.

    python -m glacier_eval.s3

Credentials are generated for the run. Writes results/s3.json.
"""
from __future__ import annotations

import secrets
import shutil
import socket
import subprocess
import time

from .common import DATA, EXE, KERNEL, ROOT, env, glacier, glacier_json, save, timed, tree_digest

WORK = DATA / "s3"


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    WORK.mkdir(parents=True)
    s3lite = WORK / f"s3lite{EXE}"
    subprocess.run(["go", "build", "-o", str(s3lite), "./cmd/s3lite"], cwd=ROOT / "service", check=True)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    access, secret = "eval-" + secrets.token_hex(4), secrets.token_urlsafe(24)
    srv = subprocess.Popen([str(s3lite), "-addr", f"127.0.0.1:{port}", "-dir", str(WORK / "objects")],
                           env=env(S3LITE_ACCESS_KEY=access, S3LITE_SECRET_KEY=secret), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    e = env(AWS_ACCESS_KEY_ID=access, AWS_SECRET_ACCESS_KEY=secret)
    src = KERNEL / "linux-6.0"
    glacier("analyze", src, "--whole-file", env=None)  # both runs then read from the OS cache
    out = {}
    try:
        for name, repo in [("local", str(WORK / "local-repo")), ("s3", f"s3:http://127.0.0.1:{port}/glacier/repo")]:
            glacier("init", "--repo", repo, env=e)
            b, bs = timed(glacier_json, "backup", "--repo", repo, src, env=e)
            target = WORK / f"restore-{name}"
            r, rs = timed(glacier_json, "restore", "latest", "--repo", repo, "--target", target, env=e)
            same = tree_digest(target / src.name) == tree_digest(src)
            c, cs = timed(glacier_json, "check", "--repo", repo, "--read-data", env=e, check=False)
            shutil.rmtree(target)
            out[name] = {"backup_seconds": bs, "restore_seconds": rs, "check_seconds": cs, "bytes": b["summary"]["bytes"],
                         "stored": b["summary"]["stored_new"], "restore_identical": same, "check_ok": not c["errors"]}
            print(name, out[name])
    finally:
        srv.kill()
    save("s3.json", out)
    shutil.rmtree(WORK)


if __name__ == "__main__":
    main()
