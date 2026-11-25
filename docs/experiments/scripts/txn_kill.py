#!/usr/bin/env python3
"""Fault experiment 0018, part A (issue #35, design 0023 section 10).

Kills `txn_kill` (crates/oakdb-sql/examples) with SIGKILL in the middle of
SQL transactions and checks what reopening finds:

- open: kill while a BEGIN ... transaction of 10 000 rows is open, never
  committed. Reopened, nothing of it may be there.
- loop: kill at a random moment while transactions of several statements
  run and commit one after another. Reopened, each transaction is there
  whole or not at all, and every one whose COMMIT returned is there.

    python3 docs/experiments/scripts/txn_kill.py [--rounds 20] [--seed 1]

Prints one line per round and a summary. Build happens first, in release
mode.
"""

import argparse
import random
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]


def build():
    subprocess.run(
        ["cargo", "build", "--release", "-q", "-p", "oakdb-sql", "--example", "txn_kill"],
        cwd=ROOT,
        check=True,
    )
    return ROOT / "target" / "release" / "examples" / "txn_kill"


def check(binary, *args):
    out = subprocess.run([str(binary), *args], capture_output=True, text=True)
    if out.returncode != 0:
        print(out.stdout, out.stderr, file=sys.stderr)
        sys.exit(f"check failed: {args}")
    return out.stdout.strip()


def round_open(binary):
    with tempfile.TemporaryDirectory() as tmp:
        db = str(Path(tmp) / "db")
        proc = subprocess.Popen([str(binary), "open", db], stdout=subprocess.PIPE, text=True)
        start = time.monotonic()
        line = proc.stdout.readline().strip()
        assert line == "open", line
        took = time.monotonic() - start
        proc.send_signal(signal.SIGKILL)
        proc.wait()
        return check(binary, "check-open", db), took


def round_loop(binary, delay):
    with tempfile.TemporaryDirectory() as tmp:
        db = str(Path(tmp) / "db")
        proc = subprocess.Popen([str(binary), "loop", db], stdout=subprocess.PIPE, text=True)
        time.sleep(delay)
        proc.send_signal(signal.SIGKILL)
        proc.wait()
        acked = [line.strip() for line in proc.stdout if line.strip()]
        last = acked[-1] if acked else "-1"
        return check(binary, "check-loop", db, last), int(last)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rounds", type=int, default=20)
    parser.add_argument("--seed", type=int, default=1)
    args = parser.parse_args()
    rng = random.Random(args.seed)
    binary = build()

    for i in range(args.rounds):
        result, took = round_open(binary)
        print(f"open  {i:2}: transaction open after {took:.2f}s, killed -> {result}")
    for i in range(args.rounds):
        delay = rng.uniform(0.05, 1.5)
        result, last = round_loop(binary, delay)
        print(f"loop  {i:2}: killed after {delay:.2f}s, last acknowledged {last} -> {result}")
    print(f"all {2 * args.rounds} rounds passed")


if __name__ == "__main__":
    main()
