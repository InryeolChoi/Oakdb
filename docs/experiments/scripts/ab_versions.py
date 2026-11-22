#!/usr/bin/env python3
"""A/B test of commit time between two engine versions, with an A/A control.

Run from the repository root:

    python3 docs/experiments/scripts/ab_versions.py <rev A> <rev B> [rounds] [commits] [modes]

`modes` is a comma-separated list of `commit_time` modes, by default
`overwrite,grow`; `reopen` and `closed` time opening after the commits
instead, without and with closing first.

Builds `ab_versions/commit_time.rs` as an example against each revision (in
a temporary git worktree), then runs the two binaries in ABBA order, each
round on a fresh database, and tests per-round mean commit times with a
two-sided Mann-Whitney U test (normal approximation), the same unit and test
as experiment 0002. The A/A control runs revision A against itself.
"""
import math
import os
import random
import shutil
import statistics
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SOURCE = os.path.join(HERE, "ab_versions", "commit_time.rs")


def build(rev, root):
    tree = os.path.join(root, rev)
    subprocess.run(["git", "worktree", "add", "-q", "--detach", tree, rev], check=True)
    shutil.copy(SOURCE, os.path.join(tree, "crates/oakdb/examples/commit_time.rs"))
    subprocess.run(["cargo", "build", "-q", "--release", "--example", "commit_time"],
                   cwd=tree, check=True, env={**os.environ, "CARGO_TARGET_DIR": os.path.join(root, "target-" + rev)})
    binary = os.path.join(root, "target-" + rev, "release/examples/commit_time")
    return tree, binary


def once(binary, mode, commits):
    with tempfile.TemporaryDirectory() as d:
        out = subprocess.run([binary, d, mode, str(commits)], capture_output=True, text=True, check=True)
    return float(out.stdout.split()[0])


def mann_whitney(a, b):
    ranked = sorted([(x, 0) for x in a] + [(x, 1) for x in b])
    ranks, i = [0.0] * len(ranked), 0
    while i < len(ranked):
        j = i
        while j + 1 < len(ranked) and ranked[j + 1][0] == ranked[i][0]:
            j += 1
        for k in range(i, j + 1):
            ranks[k] = (i + j) / 2 + 1
        i = j + 1
    r1 = sum(r for r, (_, g) in zip(ranks, ranked) if g == 0)
    n1, n2 = len(a), len(b)
    u = r1 - n1 * (n1 + 1) / 2
    z = (u - n1 * n2 / 2) / math.sqrt(n1 * n2 * (n1 + n2 + 1) / 12)
    return u, z, math.erfc(abs(z) / math.sqrt(2))


def ratio_ci(a, b, rng):
    boots = sorted(statistics.median(rng.choices(b, k=len(b))) / statistics.median(rng.choices(a, k=len(a)))
                   for _ in range(2000))
    return boots[50], boots[1949]


def compare(name, a_bin, b_bin, mode, rounds, commits):
    once(a_bin, mode, commits), once(b_bin, mode, commits)  # warm-up
    a, b = [], []
    for r in range(rounds):
        order = [(a_bin, a), (b_bin, b)] if r % 2 == 0 else [(b_bin, b), (a_bin, a)]
        for binary, out in order:
            out.append(once(binary, mode, commits))
    u, z, p = mann_whitney(a, b)
    lo, hi = ratio_ci(a, b, random.Random(1))
    ratio = statistics.median(b) / statistics.median(a)
    print(f"{name:<22} {mode:<9} A {statistics.median(a):8.1f} us  B {statistics.median(b):8.1f} us  "
          f"B/A {ratio:.3f} [{lo:.3f}, {hi:.3f}]  U={u:.0f} z={z:.2f} p={p:.2e}")


def main():
    rev_a, rev_b = sys.argv[1], sys.argv[2]
    rounds = int(sys.argv[3]) if len(sys.argv) > 3 else 20
    commits = int(sys.argv[4]) if len(sys.argv) > 4 else 200
    modes = sys.argv[5].split(",") if len(sys.argv) > 5 else ["overwrite", "grow"]
    root = tempfile.mkdtemp(prefix="ab_versions-")
    trees = []
    try:
        tree_a, a_bin = build(rev_a, root)
        trees.append(tree_a)
        tree_b, b_bin = build(rev_b, root)
        trees.append(tree_b)
        print(f"A = {rev_a}, B = {rev_b}, {rounds} rounds of {commits} commits, per-round mean (reopen: time to open)")
        for mode in modes:
            compare("A/B", a_bin, b_bin, mode, rounds, commits)
            compare("A/A (control)", a_bin, a_bin, mode, rounds, commits)
    finally:
        for tree in trees:
            subprocess.run(["git", "worktree", "remove", "--force", tree])
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
