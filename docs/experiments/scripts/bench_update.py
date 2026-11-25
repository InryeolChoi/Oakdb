#!/usr/bin/env python3
"""Runs the UPDATE/DELETE benchmark of design 0022, section 8 (issue #33).

For each row count and mode it runs `update_bench` (crates/oakdb-sql/
examples) under the system `time`, which reports the process's peak
memory (`/usr/bin/time -l` on macOS, `-v` on Linux), and prints a
Markdown table: the statement's time and the peak memory beyond the
`load` run of the same size, which only loads the table.

    python3 docs/experiments/scripts/bench_update.py [--rows 10000,100000] [--repeat 5]

Each cell is the median of `--repeat` runs. Build happens first, in
release mode.
"""

import argparse
import platform
import re
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
MODES = ["load", "update", "update-key", "delete"]


def build():
    subprocess.run(
        ["cargo", "build", "--release", "-q", "-p", "oakdb-sql", "--example", "update_bench"],
        cwd=ROOT,
        check=True,
    )
    return ROOT / "target" / "release" / "examples" / "update_bench"


def run(binary, rows, mode):
    """(statement seconds, peak memory in bytes) of one run."""
    mac = platform.system() == "Darwin"
    flag = "-l" if mac else "-v"
    done = subprocess.run(
        ["/usr/bin/time", flag, str(binary), str(rows), mode],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    seconds = float(re.search(r"statement_s=([0-9.]+)", done.stdout).group(1))
    if mac:
        # "  123456  maximum resident set size" in bytes.
        peak = int(re.search(r"(\d+)\s+maximum resident set size", done.stderr).group(1))
    else:
        # "Maximum resident set size (kbytes): 123456".
        peak = 1024 * int(re.search(r"Maximum resident set size \(kbytes\): (\d+)", done.stderr).group(1))
    return seconds, peak


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rows", default="10000,100000")
    parser.add_argument("--repeat", type=int, default=5)
    args = parser.parse_args()
    binary = build()
    print(f"# {platform.system()} {platform.machine()}, median of {args.repeat} runs\n")
    print("| rows | mode | statement time (ms) | peak memory (MiB) | beyond load (MiB) |")
    print("|---|---|---|---|---|")
    for rows in (int(r) for r in args.rows.split(",")):
        base = None
        for mode in MODES:
            runs = [run(binary, rows, mode) for _ in range(args.repeat)]
            seconds = statistics.median(r[0] for r in runs)
            peak = statistics.median(r[1] for r in runs) / 2**20
            if mode == "load":
                base = peak
            print(
                f"| {rows} | {mode} | {seconds * 1000:.1f} | {peak:.1f} | {peak - base:+.1f} |",
                flush=True,
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
