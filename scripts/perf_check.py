#!/usr/bin/env python3
"""Compares the speed and memory of two versions of Droid Lite with `--bench` on one project.

    scripts/perf_check.py <old-ref> [<new-ref>] [--project DIR] [--rounds N]

The new version is HEAD when not given. The project is --project, else
`git config droidlite.perfProject`. Each version is built once (release, memstats) and kept
in target/perf/<sha>, so a second check of the same commit does not build again. The runs
alternate old/new and swap who goes first each round, because run order biases short steps.
Each version gets a warm-up run first (library cache, file cache).

Exit 1 when a step is slower or keeps more memory than the limits below; the table says which.
"""

import argparse
import os
import re
import shutil
import statistics
import subprocess
import sys

ROOT = subprocess.run(
    ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
).stdout.strip()
PERF = os.path.join(ROOT, "target", "perf")
LINE = re.compile(r"^(.{34}) +(-?[\d.]+) ms(?: +([+-][\d.]+) MB heap)?")
# a step fails when it is both this much slower in ratio and in absolute time
SLOWER_RATIO, SLOWER_MS = 1.15, 2.0
# and when it keeps both this much more memory in ratio and in MB
HEAP_RATIO, HEAP_MB = 1.05, 2.0


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()


def build(sha):
    """The bench binary of commit `sha`, built in its own worktree and kept for next time."""
    exe = os.path.join(PERF, sha, "droid-lite")
    if os.path.exists(exe):
        return exe
    tree = os.path.join(PERF, "worktree")
    if os.path.exists(tree):
        git("worktree", "remove", "--force", tree)
    git("worktree", "add", "--detach", tree, sha)
    try:
        print(f"building {sha[:8]}…", flush=True)
        subprocess.run(
            ["cargo", "build", "--quiet", "--release", "--features", "memstats"],
            cwd=tree,
            # one shared target folder: the second build reuses most of the first
            env={**os.environ, "CARGO_TARGET_DIR": os.path.join(PERF, "cargo")},
            check=True,
        )
        os.makedirs(os.path.dirname(exe), exist_ok=True)
        shutil.copy2(os.path.join(PERF, "cargo", "release", "droid-lite"), exe)
    finally:
        git("worktree", "remove", "--force", tree)
    return exe


def run(exe, project, cache):
    env = {**os.environ, "DROID_LITE_CACHE_DIR": cache}
    out = subprocess.run([exe, "--bench", project], capture_output=True, text=True, env=env, check=True).stdout
    steps = {}
    for line in out.splitlines():
        m = LINE.match(line)
        if m:
            steps[m.group(1).strip()] = (float(m.group(2)), float(m.group(3) or 0))
    return steps


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("old")
    ap.add_argument("new", nargs="?", default="HEAD")
    ap.add_argument("--project")
    ap.add_argument("--rounds", type=int, default=7)
    a = ap.parse_args()
    project = a.project or subprocess.run(
        ["git", "config", "droidlite.perfProject"], cwd=ROOT, capture_output=True, text=True
    ).stdout.strip()
    if not project or not os.path.isdir(project):
        sys.exit("No project to bench: pass --project DIR or run "
                 "`git config droidlite.perfProject /path/to/android/project`.")
    old, new = git("rev-parse", a.old + "^{commit}"), git("rev-parse", a.new + "^{commit}")
    exes = {"old": build(old), "new": build(new)}
    caches = {k: os.path.join(PERF, "cache-" + k) for k in exes}
    for k in exes:
        shutil.rmtree(caches[k], ignore_errors=True)
        run(exes[k], project, caches[k])  # warm-up
    rows = {"old": [], "new": []}
    for i in range(a.rounds):
        order = ["old", "new"] if i % 2 == 0 else ["new", "old"]
        for k in order:
            rows[k].append(run(exes[k], project, caches[k]))
        print(f"round {i + 1}/{a.rounds}", flush=True)

    def median(k, step, field):
        vals = [r[step][field] for r in rows[k] if step in r]
        return statistics.median(vals) if vals else None

    failed = []
    print(f"\n{'step':<34} {'old ms':>9} {'new ms':>9} {'old MB':>8} {'new MB':>8}")
    for step in rows["new"][0]:
        o_ms, n_ms = median("old", step, 0), median("new", step, 0)
        o_mb, n_mb = median("old", step, 1), median("new", step, 1)
        flag = ""
        if o_ms is None:
            flag = "  new step"
        else:
            if n_ms > o_ms * SLOWER_RATIO and n_ms - o_ms > SLOWER_MS:
                flag += "  SLOWER"
            if n_mb > max(o_mb, 0) * HEAP_RATIO and n_mb - o_mb > HEAP_MB:
                flag += "  MORE MEMORY"
        if "SLOWER" in flag or "MEMORY" in flag:
            failed.append(step)
        fmt = lambda v, w, p: f"{v:>{w}.{p}f}" if v is not None else " " * (w - 1) + "-"
        print(f"{step:<34} {fmt(o_ms, 9, 2)} {fmt(n_ms, 9, 2)} {fmt(o_mb, 8, 1)} {fmt(n_mb, 8, 1)}{flag}")
    print(f"\nold {old[:8]}, new {new[:8]}, {a.rounds} rounds, medians")
    if failed:
        print(f"perf check FAILED: {', '.join(failed)}")
        sys.exit(1)
    print("perf check passed")


if __name__ == "__main__":
    main()
