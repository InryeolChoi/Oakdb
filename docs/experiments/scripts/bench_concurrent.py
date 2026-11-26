#!/usr/bin/env python3
"""Concurrent write benchmark: Oakdb against SQLite, group commit on and off
(issue #30, design 0026). Standard library only.

Run from the repository root:

    python3 docs/experiments/scripts/bench_concurrent.py curve  <csv> [rounds] [plan]
    python3 docs/experiments/scripts/bench_concurrent.py claims <csv> [rounds]
    python3 docs/experiments/scripts/bench_concurrent.py report <csv>... [--svg <prefix>]

`curve` measures every point of a plan (`main`, the default, or `quick`)
`rounds` times (10), the systems of a point taking turns in an order that
flips every round, and appends one CSV row per measured round. `claims`
runs the hypotheses written down before measuring (design 0026 Q12), each
with its A/A control, in ABBA order (30 rounds), appends the rounds to the
CSV and prints the verdicts. `report` prints Markdown tables from CSV files
and, with --svg, draws the charts.

Each round is one run of the `concurrent_write` binary of the
`oakdb-bench` crate (built here with --release) on a fresh database under
$TMPDIR: 0.5 s of warm-up, then commits counted for 3 s. The statistics
are those of `ab_versions.py`: a two-sided Mann-Whitney U test of the
per-round commits a second, and a 95% bootstrap interval of the ratio of
medians.
"""
import csv
import math
import os
import random
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from ab_versions import mann_whitney, ratio_ci  # noqa: E402

BINARY = os.path.join("target", "release", "concurrent_write")
REST = 0.5  # seconds between rounds, for the disk to settle
WARMUP, WINDOW = "0.5", "3"

THREADS = [1, 2, 4, 8, 16, 32, 64]
# (workload, systems, thread counts): design 0026 Q2, Q3, Q6, Q7, Q8.
PLANS = {
    "main": [
        ("w1", ["oak", "oak-off", "sqlite-s1", "sqlite-s2"], THREADS),
        ("w1", ["oak", "oak-nosync"], THREADS + [128, 256]),
        ("w1", ["sqlite-s3", "oak-sql"], [1, 16]),
        ("w2", ["oak", "oak-off", "sqlite-s1", "sqlite-s2"], [1, 16]),
        ("w3", ["oak", "sqlite-s1", "sqlite-s2"], [1, 16]),
    ],
    # A few points, for CI runners (`measure.yml`).
    "quick": [
        ("w1", ["oak", "oak-off", "sqlite-s1", "sqlite-s2"], [1, 4, 16]),
    ],
}

# The claims, written before measuring (design 0026 Q12): name, A (the
# baseline), B, threads, and the rule for B/A.
CLAIMS = [
    ("H1 oak/sqlite-s1", "sqlite-s1", "oak", 16, "greater"),
    ("H1 oak/sqlite-s2", "sqlite-s2", "oak", 16, "greater"),
    ("A/A oak 16", "oak", "oak", 16, "same"),
    ("A/A sqlite-s1 16", "sqlite-s1", "sqlite-s1", 16, "same"),
    ("H2 oak/oak-off 16", "oak-off", "oak", 16, "greater"),
    ("H3 oak/oak-off 1", "oak-off", "oak", 1, "equivalent"),
    ("A/A oak 1", "oak", "oak", 1, "same"),
]

FIELDS = ["kind", "label", "round", "system", "workload", "threads", "commits", "secs",
          "per_second", "p50_us", "p99_us", "retries", "timeouts", "total_commits",
          "thread_min", "thread_max", "batches", "wal_syncs", "conflicts", "largest_batch",
          "batch_sizes", "wal_write_ms", "hot_sum", "journal_mode", "synchronous", "fullfsync",
          "checkpoint_fullfsync", "page_size", "wal_autocheckpoint", "sqlite_version", "load1"]


def build():
    subprocess.run(["cargo", "build", "-q", "--release", "-p", "oakdb-bench", "--bin", "concurrent_write"],
                   check=True)


def once(system, workload, threads, seed):
    with tempfile.TemporaryDirectory(prefix="bench-") as d:
        out = subprocess.run([BINARY, d, system, workload, str(threads), WARMUP, WINDOW, "10000", str(seed)],
                             capture_output=True, text=True, check=True)
    row = dict(field.split("=", 1) for field in out.stdout.split())
    row["load1"] = f"{os.getloadavg()[0]:.2f}"
    time.sleep(REST)
    return row


class Writer:
    def __init__(self, path):
        new = not os.path.exists(path) or os.path.getsize(path) == 0
        self.file = open(path, "a", newline="")
        self.csv = csv.DictWriter(self.file, FIELDS, extrasaction="ignore")
        if new:
            self.csv.writeheader()

    def write(self, kind, label, r, row):
        self.csv.writerow({"kind": kind, "label": label, "round": r, **row})
        self.file.flush()


def curve(path, rounds, plan):
    build()
    out = Writer(path)
    for workload, systems, counts in PLANS[plan]:
        for threads in counts:
            if threads > 64:
                systems_here = [s for s in systems if s == "oak"]
            else:
                systems_here = systems
            for r in range(rounds):
                order = systems_here if r % 2 == 0 else list(reversed(systems_here))
                for system in order:
                    row = once(system, workload, threads, seed=r + 1)
                    out.write("curve", f"{workload}", r, row)
                    print(f"{workload} {threads:>3} {system:<10} r{r:<2} {float(row['per_second']):9.1f}/s "
                          f"load {row['load1']}", flush=True)


def verdict(rule, lo, hi, p):
    if rule == "greater":
        return "holds" if lo > 1 and p < 0.05 else "does not hold"
    if rule == "equivalent":
        return "holds" if 0.97 <= lo and hi <= 1.03 else "does not hold"
    return "control ok" if lo <= 1 <= hi and p > 0.1 else "control FAILED: noise, do not judge"


def claims(path, rounds):
    build()
    out = Writer(path)
    results = []
    for name, a, b, threads, rule in CLAIMS:
        once(a, "w1", threads, 0), once(b, "w1", threads, 0)  # warm-up
        xs, ys = [], []
        for r in range(rounds):
            order = [(a, xs, "A"), (b, ys, "B")] if r % 2 == 0 else [(b, ys, "B"), (a, xs, "A")]
            for system, into, side in order:
                row = once(system, "w1", threads, seed=r + 1)
                out.write("claim", f"{name}|{side}", r, row)
                into.append(float(row["per_second"]))
        u, z, p = mann_whitney(xs, ys)
        lo, hi = ratio_ci(xs, ys, random.Random(1))
        ratio = statistics.median(ys) / statistics.median(xs)
        line = (f"{name:<20} A {statistics.median(xs):8.1f}/s  B {statistics.median(ys):8.1f}/s  "
                f"B/A {ratio:.3f} [{lo:.3f}, {hi:.3f}]  U={u:.0f} z={z:.2f} p={p:.2e}  "
                f"{verdict(rule, lo, hi, p)}")
        print(line, flush=True)
        results.append(line)
    return results


# ---- report ----

def read(paths):
    rows = []
    for path in paths:
        with open(path, newline="") as f:
            rows.extend(csv.DictReader(f))
    return rows


def median_ci(xs, rng):
    boots = sorted(statistics.median(rng.choices(xs, k=len(xs))) for _ in range(2000))
    return statistics.median(xs), boots[50], boots[1949]


def points(rows, workload, field="per_second"):
    """{system: {threads: [values]}} of the curve rows of `workload`."""
    out = {}
    for row in rows:
        if row["kind"] != "curve" or row["workload"] != workload:
            continue
        out.setdefault(row["system"], {}).setdefault(int(row["threads"]), []).append(float(row[field]))
    return out


ORDER = ["oak", "oak-off", "oak-nosync", "oak-sql", "sqlite-s1", "sqlite-s2", "sqlite-s3"]


def table(rows, workload, field, fmt):
    pts = points(rows, workload, field)
    systems = [s for s in ORDER if s in pts]
    counts = sorted({t for s in systems for t in pts[s]})
    rng = random.Random(1)
    print(f"| threads | " + " | ".join(systems) + " |")
    print("|---:|" + "---:|" * len(systems))
    for t in counts:
        cells = []
        for s in systems:
            xs = pts[s].get(t)
            if not xs:
                cells.append("")
                continue
            m, lo, hi = median_ci(xs, rng)
            cells.append(f"{fmt(m)} [{fmt(lo)}, {fmt(hi)}]")
        print(f"| {t} | " + " | ".join(cells) + " |")
    print()


def batch_table(rows):
    print("| threads | batches | mean batch | 1 | 2-3 | 4-7 | 8-15 | 16-31 | 32-63 | 64-127 | 128+ | largest | "
          "sync ms (mean) |")
    print("|---:|" + "---:|" * 12)
    by = {}
    for row in rows:
        if row["kind"] == "curve" and row["workload"] == "w1" and row["system"] == "oak":
            by.setdefault(int(row["threads"]), []).append(row)
    for t in sorted(by):
        sizes = [0] * 8
        batches = 0
        ms = 0.0
        largest = 0
        for row in by[t]:
            for i, n in enumerate(row["batch_sizes"].split("/")):
                sizes[i] += int(n)
            batches += int(row["batches"])
            ms += float(row["wal_write_ms"])
            largest = max(largest, int(row["largest_batch"]))
        total = sum(sizes) or 1
        fr = [f"{100 * n / total:.0f}%" for n in sizes]
        mean = sum(int(r["commits"]) for r in by[t]) / max(batches, 1)
        print(f"| {t} | {batches} | {mean:.1f} | " + " | ".join(fr) + f" | {largest} | {ms / max(batches, 1):.2f} |")
    print()


def hot_table(rows):
    print("| system | threads | commits/s | retries per commit | hot sum = all commits |")
    print("|---|---:|---:|---:|---|")
    by = {}
    for row in rows:
        if row["kind"] == "curve" and row["workload"] == "w3":
            by.setdefault((row["system"], int(row["threads"])), []).append(row)
    for (s, t), rs in sorted(by.items(), key=lambda kv: (ORDER.index(kv[0][0]), kv[0][1])):
        tput = statistics.median(float(r["per_second"]) for r in rs)
        retries = sum(int(r["retries"]) for r in rs) / max(sum(int(r["total_commits"]) for r in rs), 1)
        ok = all(r["hot_sum"] == r["total_commits"] for r in rs)
        print(f"| {s} | {t} | {tput:.1f} | {retries:.3f} | {'yes' if ok else 'NO'} |")
    print()


def claims_table(rows):
    by = {}
    for row in rows:
        if row["kind"] == "claim":
            name, side = row["label"].split("|")
            by.setdefault(name, {"A": [], "B": []})[side].append(float(row["per_second"]))
    rules = {name: rule for name, _, _, _, rule in CLAIMS}
    print("| claim | A median | B median | B/A | 95% CI | p | verdict |")
    print("|---|---:|---:|---:|---|---:|---|")
    for name, _, _, _, _ in CLAIMS:
        if name not in by:
            continue
        xs, ys = by[name]["A"], by[name]["B"]
        u, z, p = mann_whitney(xs, ys)
        lo, hi = ratio_ci(xs, ys, random.Random(1))
        ratio = statistics.median(ys) / statistics.median(xs)
        print(f"| {name} | {statistics.median(xs):.1f} | {statistics.median(ys):.1f} | {ratio:.3f} | "
              f"[{lo:.3f}, {hi:.3f}] | {p:.2e} | {verdict(rules[name], lo, hi, p)} |")
    print()


# ---- SVG ----

COLORS = {"oak": "#1b6ac9", "oak-off": "#8fb8ea", "oak-nosync": "#1b6ac9", "oak-sql": "#0b8a6a",
          "sqlite-s1": "#d2652d", "sqlite-s2": "#a63d12", "sqlite-s3": "#d2652d"}
DASHED = {"oak-nosync", "sqlite-s3"}
LABELS = {"oak": "Oakdb (group commit)", "oak-off": "Oakdb (batch of 1)",
          "oak-nosync": "Oakdb, no sync (not durable)", "oak-sql": "Oakdb SQL INSERT",
          "sqlite-s1": "SQLite S1 (conn per thread)", "sqlite-s2": "SQLite S2 (one conn, mutex)",
          "sqlite-s3": "SQLite S3 defaults (not as durable)"}


def svg_lines(pts, title, ylabel, path):
    w, h, left, right, top, bottom = 760, 460, 70, 230, 40, 50
    xs = sorted({t for s in pts for t in pts[s]})
    ys = [v for s in pts for t in pts[s] for v in pts[s][t] if v > 0]
    x0, x1 = math.log2(min(xs)), math.log2(max(xs))
    y0, y1 = math.floor(math.log10(min(ys))), math.ceil(math.log10(max(ys)))
    px = lambda t: left + (math.log2(t) - x0) / max(x1 - x0, 1) * (w - left - right)
    py = lambda v: top + (1 - (math.log10(max(v, 10 ** y0)) - y0) / (y1 - y0)) * (h - top - bottom)
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" font-family="sans-serif" font-size="12">',
           f'<rect width="{w}" height="{h}" fill="white"/>',
           f'<text x="{left}" y="22" font-size="15" font-weight="bold">{title}</text>']
    for e in range(y0, y1 + 1):
        y = py(10 ** e)
        out.append(f'<line x1="{left}" x2="{w - right}" y1="{y:.1f}" y2="{y:.1f}" stroke="#ddd"/>')
        out.append(f'<text x="{left - 8}" y="{y + 4:.1f}" text-anchor="end">{10 ** e:,}</text>')
    for t in xs:
        x = px(t)
        out.append(f'<line x1="{x:.1f}" x2="{x:.1f}" y1="{top}" y2="{h - bottom}" stroke="#eee"/>')
        out.append(f'<text x="{x:.1f}" y="{h - bottom + 18}" text-anchor="middle">{t}</text>')
    out.append(f'<text x="{(left + w - right) / 2}" y="{h - 10}" text-anchor="middle">threads</text>')
    out.append(f'<text x="16" y="{(top + h - bottom) / 2}" transform="rotate(-90 16 {(top + h - bottom) / 2})" '
               f'text-anchor="middle">{ylabel}</text>')
    rng = random.Random(1)
    legend_y = top + 10
    for s in [s for s in ORDER if s in pts]:
        color, dash = COLORS[s], ' stroke-dasharray="6 4"' if s in DASHED else ""
        coords = []
        for t in sorted(pts[s]):
            m, lo, hi = median_ci(pts[s][t], rng)
            x = px(t)
            coords.append(f"{x:.1f},{py(m):.1f}")
            out.append(f'<line x1="{x:.1f}" x2="{x:.1f}" y1="{py(lo):.1f}" y2="{py(hi):.1f}" stroke="{color}"/>')
            out.append(f'<circle cx="{x:.1f}" cy="{py(m):.1f}" r="3" fill="{color}"/>')
        out.append(f'<polyline points="{" ".join(coords)}" fill="none" stroke="{color}" stroke-width="2"{dash}/>')
        out.append(f'<line x1="{w - right + 12}" x2="{w - right + 36}" y1="{legend_y}" y2="{legend_y}" '
                   f'stroke="{color}" stroke-width="2"{dash}/>')
        out.append(f'<text x="{w - right + 42}" y="{legend_y + 4}" font-size="11">{LABELS[s]}</text>')
        legend_y += 20
    out.append("</svg>")
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")


def svg_batches(rows, path):
    by = {}
    for row in rows:
        if row["kind"] == "curve" and row["workload"] == "w1" and row["system"] == "oak":
            sizes = by.setdefault(int(row["threads"]), [0] * 8)
            for i, n in enumerate(row["batch_sizes"].split("/")):
                sizes[i] += int(n)
    names = ["1", "2-3", "4-7", "8-15", "16-31", "32-63", "64-127", "128"]
    shades = ["#e8f0fb", "#cfe0f6", "#a9c8ef", "#7eaee6", "#5492db", "#2f76cc", "#1b5aa3", "#0e3a6e"]
    w, left, top, bar = 760, 70, 40, 26
    h = top + len(by) * (bar + 8) + 70
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" font-family="sans-serif" font-size="12">',
           f'<rect width="{w}" height="{h}" fill="white"/>',
           f'<text x="{left}" y="22" font-size="15" font-weight="bold">Oakdb batch sizes (share of batches)</text>']
    width = w - left - 30
    for k, t in enumerate(sorted(by)):
        y = top + k * (bar + 8)
        total = sum(by[t]) or 1
        x = left
        out.append(f'<text x="{left - 8}" y="{y + bar / 2 + 4}" text-anchor="end">{t} thr</text>')
        for i, n in enumerate(by[t]):
            bw = n / total * width
            if bw > 0:
                out.append(f'<rect x="{x:.1f}" y="{y}" width="{bw:.1f}" height="{bar}" fill="{shades[i]}"/>')
            x += bw
    ly = top + len(by) * (bar + 8) + 20
    for i, name in enumerate(names):
        x = left + i * 80
        out.append(f'<rect x="{x}" y="{ly}" width="14" height="14" fill="{shades[i]}"/>')
        out.append(f'<text x="{x + 18}" y="{ly + 12}">{name}</text>')
    out.append(f'<text x="{left}" y="{ly + 36}" font-size="11">commits per batch</text>')
    out.append("</svg>")
    with open(path, "w") as f:
        f.write("\n".join(out) + "\n")


def report(paths, svg):
    rows = read(paths)
    loads = [float(r["load1"]) for r in rows if r.get("load1")]
    if loads:
        print(f"load average (1 min) during rounds: median {statistics.median(loads):.1f}, "
              f"min {min(loads):.1f}, max {max(loads):.1f}\n")
    fmt0 = lambda v: f"{v:,.0f}"
    fmt1 = lambda v: f"{v:,.1f}"
    for workload in ["w1", "w2"]:
        if points(rows, workload):
            print(f"### {workload}: commits a second, median [95% CI]\n")
            table(rows, workload, "per_second", fmt0)
            print(f"### {workload}: p99 latency in ms, median [95% CI]\n")
            table(rows, workload, "p99_us", lambda v: fmt1(v / 1000))
            print(f"### {workload}: p50 latency in ms, median [95% CI]\n")
            table(rows, workload, "p50_us", lambda v: fmt1(v / 1000))
    if any(r["system"] == "oak" and r["kind"] == "curve" for r in rows):
        print("### Oakdb batch sizes (w1)\n")
        batch_table(rows)
    if points(rows, "w3"):
        print("### w3 (hot counters)\n")
        hot_table(rows)
    if any(r["kind"] == "claim" for r in rows):
        print("### Claims\n")
        claims_table(rows)
    if svg:
        tput = points(rows, "w1")
        svg_lines(tput, "Commits a second (W1), median and 95% CI", "commits / s", f"{svg}-throughput.svg")
        p99 = {s: {t: [v / 1000 for v in vs] for t, vs in ts.items()} for s, ts in points(rows, "w1", "p99_us").items()}
        svg_lines(p99, "p99 latency (W1), median and 95% CI", "ms", f"{svg}-p99.svg")
        svg_batches(rows, f"{svg}-batches.svg")


def main():
    cmd = sys.argv[1]
    if cmd == "curve":
        curve(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 10,
              sys.argv[4] if len(sys.argv) > 4 else "main")
    elif cmd == "claims":
        claims(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 30)
    elif cmd == "report":
        args = sys.argv[2:]
        svg = None
        if "--svg" in args:
            at = args.index("--svg")
            svg = args[at + 1]
            args = args[:at] + args[at + 2:]
        report(args, svg)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
