"""Plots for the README from results/*.json.

    python -m glacier_eval.report
"""
from __future__ import annotations

import json

from .common import DOCS, RESULTS

BLUE, ORANGE, GRAY, GREEN = "#2a78d6", "#eb6834", "#8a94a6", "#1baf7a"


def style(ax, grid="x"):
    ax.grid(axis=grid, alpha=0.25)
    for s in ("top", "right"):
        ax.spines[s].set_visible(False)


def main():
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    DOCS.mkdir(exist_ok=True)
    d = json.loads((RESULTS / "dedup.json").read_text())
    labels = {"fastcdc-16384": "FastCDC 16 KiB", "fastcdc-65536": "FastCDC 64 KiB (default)", "fastcdc-262144": "FastCDC 256 KiB",
              "fastcdc-1048576": "FastCDC 1 MiB (restic's size)", "fastcdc-2097152": "FastCDC 2 MiB (Borg's size)", "fixed-65536": "fixed 64 KiB",
              "whole-file": "whole files"}
    fig, axes = plt.subplots(1, 2, figsize=(12.5, 3.8), sharey=True)
    for ax, key, title in zip(axes, ["kernel", "projects"], ["Linux 6.0-6.7 (8 versions)", "project history (every commit)"]):
        rows = d[key]
        ratios = [r["total_ratio"] for r in rows]
        bars = ax.barh(range(len(rows)), ratios, color=[BLUE if r["strategy"] == "fastcdc-65536" else (GRAY if not r["strategy"].startswith("fastcdc") else "#7fb0ea") for r in rows], height=0.6)
        for b, v in zip(bars, ratios):
            ax.text(v + 0.05, b.get_y() + b.get_height() / 2, f"{v:.1f}x", va="center", fontsize=9)
        ax.set_yticks(range(len(rows)), [labels.get(r["strategy"], r["strategy"]) for r in rows], fontsize=9)
        ax.invert_yaxis()
        ax.set_xlabel("data / stored (deduplicated and compressed)")
        ax.set_title(f"{title}: {rows[0]['logical_bytes'] / 2**30:.2f} GiB", loc="left", fontsize=10)
        style(ax)
    fig.tight_layout()
    fig.savefig(DOCS / "dedup.png", dpi=130)
    plt.close(fig)

    b = json.loads((RESULTS / "bench.json").read_text())
    p = b["projects"]
    fig, ax = plt.subplots(figsize=(8, 3.6))
    cum_logical, cum = [], []
    total = 0
    for r in p["rows"]:
        total += r["bytes"]
        cum_logical.append(total / 2**30)
    ax.plot(range(1, len(cum_logical) + 1), cum_logical, color=GRAY, lw=2, label="data backed up (sum of snapshots)")
    ax.axhline(p["tar_zst_total_bytes"] / 2**30, color=ORANGE, lw=1.5, ls="--", label=f"tar + zstd every time: {p['tar_zst_total_bytes'] / 2**30:.2f} GiB")
    ax.axhline(p["repo_bytes"] / 2**30, color=BLUE, lw=2, label=f"Glacier repository: {p['repo_bytes'] / 2**20:.0f} MiB")
    ax.set(xlabel="snapshot (one after each commit, in time order)", ylabel="GiB")
    ax.set_title(f"{p['snapshots']} backups of the projects folder", loc="left", fontsize=10)
    ax.legend(frameon=False, fontsize=9)
    style(ax, "y")
    fig.tight_layout()
    fig.savefig(DOCS / "projects.png", dpi=130)
    print("wrote docs/dedup.png, docs/projects.png")


if __name__ == "__main__":
    main()
