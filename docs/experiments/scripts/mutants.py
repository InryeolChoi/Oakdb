#!/usr/bin/env python3
"""Plants one bug at a time and reports which tests catch it.

Run from the repository root:

    python3 docs/experiments/scripts/mutants.py sim      # 12 bugs in SimFs
    python3 docs/experiments/scripts/mutants.py engine   # experiment 0007

Each bug is a text replacement in one source file. The file is restored
after every run, even if a run fails.
"""
import re
import subprocess
import sys

SIM = "crates/oakdb/src/fs/sim.rs"
DB = "crates/oakdb/src/db.rs"

SIM_BUGS = [
    ("unsynced writes always kept", "Landing::Lost => {}", "Landing::Lost => write_at(data, offset, &bytes),"),
    ("unsynced writes always lost", "Landing::Kept => write_at(data, offset, &bytes),", "Landing::Kept => {}"),
    ("never torn", "_ => Landing::Torn,", "_ => Landing::Kept,"),
    ("torn ignores sector bounds", "let stop = ((start / SECTOR + 1) * SECTOR).min(end);", "let stop = (start + 100).min(end);"),
    ("sync_dir does nothing", "file.entry_synced = true;\n            }", "let _ = file;\n            }"),
    ("files never vanish", "if !file.entry_synced && rng.coin() {", "if false && !file.entry_synced && rng.coin() {"),
    ("length changes always land", "            if rng.coin() {\n                data.resize", "            if true {\n                data.resize"),
    ("seed ignored", "let mut rng = Rng(seed);", "let mut rng = Rng(seed * 0);"),
    ("reads count towards fail_after", "self.with(Access::Read, |file, _| file.now.len() as u64)", "self.with(Access::Change, |file, _| file.now.len() as u64)"),
    ("lock not released on drop", "file.locked_by = None;", "let _ = file;"),
    ("old handles keep working", "disk.boot += 1;", "disk.boot += 0;"),
    ("power stays cut after a crash", "disk.down = false;", "disk.down = disk.down;"),
]

ENGINE_BUGS = [
    ("E1 new DB: no directory sync", "        file.sync_data()?;\n        fs.sync_dir(dir)?;", "        file.sync_data()?;\n        let _ = (fs, dir);"),
    ("E2 new DB: no data sync", "        file.sync_data()?;\n        fs.sync_dir(dir)?;", "        fs.sync_dir(dir)?;"),
    ("E3 commit: no sync before meta", "            write_page(file, page_no, next.page(page_no).as_bytes())?;\n        }\n        file.sync_data()?;", "            write_page(file, page_no, next.page(page_no).as_bytes())?;\n        }"),
    ("E4 commit: no sync after meta", "            write_meta(file, next.meta())?;\n            file.sync_data()?;", "            write_meta(file, next.meta())?;"),
]

REAL_DISK = [a for t in ["single_page", "delete_and_overwrite", "split", "multi_level", "scan", "overflow", "checksum"] for a in ("--test", t)]


def run(args):
    r = subprocess.run(["cargo", "test", "-q"] + args, capture_output=True, text=True)
    out = r.stdout + r.stderr
    failed = sorted({l.strip() for l in out.splitlines() if l.startswith("    ") and re.fullmatch(r"[a-z_:]+", l.strip())})
    return ("caught" if r.returncode else "missed") + (f" {failed}" if failed else "")


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "sim"
    path, bugs = (SIM, SIM_BUGS) if which == "sim" else (DB, ENGINE_BUGS)
    original = open(path).read()
    try:
        for name, old, new in bugs:
            assert original.count(old) == 1, f"{name}: pattern not found exactly once"
            open(path, "w").write(original.replace(old, new))
            print(name)
            if which == "sim":
                print("  sim tests:", run(["--lib", "--test", "sim_fs", "--test", "fs_contract"]))
            else:
                print("  real-disk tests:   ", run(REAL_DISK))
                print("  model (real disk): ", run(["--test", "model", "--", "--skip", "simulated"]))
                print("  sim crash tests:   ", run(["--test", "sim_fs"]))
    finally:
        open(path, "w").write(original)


if __name__ == "__main__":
    main()
