"""Tests of the evaluation helpers; the end-to-end test needs the release build of glacier."""
import io
import tarfile

import pytest

from glacier_eval import common, fetch


def member(name, sym=False):
    t = tarfile.TarInfo(name)
    if sym:
        t.type = tarfile.SYMTYPE
        t.linkname = "x"
    return t


def test_windows_safe_filter():
    stats = {"links": 0, "reserved_or_invalid": 0, "case_collisions": 0}
    names = [m.name for m in fetch.windows_safe([
        member("linux/Makefile"), member("linux/drivers/aux.c"), member("linux/include/CON.h"), member("linux/a/xt_DSCP.h"),
        member("linux/a/xt_dscp.h"), member("linux/link", sym=True), member("linux/b/what?.txt"), member("linux/c/auxiliary.c"),
    ], stats)]
    assert names == ["linux/Makefile", "linux/a/xt_DSCP.h", "linux/c/auxiliary.c"]
    assert stats == {"links": 1, "reserved_or_invalid": 3, "case_collisions": 1}


def test_digest_and_size(tmp_path):
    (tmp_path / "a").mkdir()
    (tmp_path / "a" / "x.txt").write_bytes(b"hello")
    (tmp_path / "y").write_bytes(b"")
    assert common.dir_size(tmp_path) == (2, 5)
    d = common.tree_digest(tmp_path)
    assert set(d) == {"a/x.txt", "y"}
    assert d["a/x.txt"].startswith("2cf24dba")


@pytest.mark.skipif(not common.GLACIER.exists(), reason="needs the release build")
def test_backup_restore_check_corrupt(tmp_path, monkeypatch):
    monkeypatch.setattr(common, "DATA", tmp_path)
    src = tmp_path / "src"
    (src / "d").mkdir(parents=True)
    for i in range(20):
        (src / "d" / f"f{i}.txt").write_bytes((f"line {i}\n" * (i * 500)).encode())
    repo = tmp_path / "repo"
    common.glacier("init", "--repo", repo, "--kdf-mem-mib", "8", "--kdf-time", "1")
    s = common.glacier_json("backup", "--repo", repo, src)
    assert s["summary"]["files"] == 20
    common.glacier_json("restore", s["id"], "--repo", repo, "--target", tmp_path / "out")
    assert common.tree_digest(tmp_path / "out" / "src") == common.tree_digest(src)
    assert common.glacier("check", "--repo", repo, "--read-data", check=False).returncode == 0
    pack = next((repo / "data").rglob("*"), None)
    while pack is not None and pack.is_dir():
        pack = next(pack.iterdir())
    data = bytearray(pack.read_bytes())
    data[len(data) // 3] ^= 0x10
    pack.write_bytes(bytes(data))
    assert common.glacier("check", "--repo", repo, "--read-data", check=False).returncode == 1
