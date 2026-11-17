#!/usr/bin/env python3
"""Plants one bug at a time and reports which tests catch it.

Run from the repository root:

    python3 docs/experiments/scripts/mutants.py sim      # 14 bugs in SimFs
    python3 docs/experiments/scripts/mutants.py engine   # experiment 0007
    python3 docs/experiments/scripts/mutants.py wal      # experiment 0009

A bug is one replacement, or a list of them for a bug that spans places.

Each bug is a text replacement in one source file. The file is restored
after every run, even if a run fails.
"""
import re
import subprocess
import sys

SIM = "crates/oakdb/src/fs/sim.rs"
DB = "crates/oakdb/src/db.rs"
WAL = "crates/oakdb/src/wal.rs"

SIM_BUGS = [
    ("unsynced writes always kept", "Landing::Lost => {}", "Landing::Lost => write_at(data, offset, &bytes),"),
    ("unsynced writes always lost", "Landing::Kept => write_at(data, offset, &bytes),", "Landing::Kept => {}"),
    ("never torn", "match rng.below(3) {", "match rng.below(2) {"),
    ("torn ignores sector bounds", "let stop = ((start / SECTOR + 1) * SECTOR).min(end);", "let stop = (start + 100).min(end);"),
    ("sync_dir does nothing", "file.entry_synced = true;\n            }", "let _ = file;\n            }"),
    ("files never vanish", "if !file.entry_synced && rng.coin() {", "if false && !file.entry_synced && rng.coin() {"),
    ("length changes always land", "let landed = rng.coin();\n            if landed {", "let landed = rng.coin() || true;\n            if landed {"),
    ("seed ignored", "let mut rng = Rng(seed);", "let mut rng = Rng(seed * 0);"),
    ("reads count towards fail_after", "self.with(Access::Read, |file, _| file.now.len() as u64)", "self.with(Access::Change, |file, _| file.now.len() as u64)"),
    ("lock not released on drop", "file.locked_by = None;", "let _ = file;"),
    ("old handles keep working", "disk.boot += 1;", "disk.boot += 0;"),
    ("power stays cut after a crash", "disk.down = false;", "disk.down = disk.down;"),
    ("ops miscounted", "None => {\n                self.ops += 1;", "None => {\n                self.ops += 2;"),
    ("report lies about torn sectors", "sectors.push(landed);", "sectors.push(!landed);"),
]

ENGINE_BUGS = [
    ("E1 new DB: no directory sync", "        file.sync_data()?;\n        fs.sync_dir(dir)?;", "        file.sync_data()?;\n        let _ = (fs, dir);"),
    ("E2 new DB: no data sync", "        file.sync_data()?;\n        fs.sync_dir(dir)?;", "        fs.sync_dir(dir)?;"),
    ("E3 commit: no sync before meta", "            write_page(file, page_no, next.page(page_no).as_bytes())?;\n        }\n        file.sync_data()?;", "            write_page(file, page_no, next.page(page_no).as_bytes())?;\n        }"),
    ("E4 commit: no sync after meta", "            write_meta(file, next.meta())?;\n            file.sync_data()?;", "            write_meta(file, next.meta())?;"),
]

# Engine bugs of the WAL (#22), each in db.rs or wal.rs.
WAL_BUGS = [
    ("W1 commit: no sync", WAL, "self.file.sync_data()?;\n        self.tail = Tail {", "self.tail = Tail {"),
    ("W2 open: tail not cut off", DB, "            wal_file.set_len(tail.end)?;", ""),
    ("W3 open: new WAL, no directory sync", DB, "if tail == Tail::EMPTY {\n            fs.sync_dir(dir)?;", "if false {\n            fs.sync_dir(dir)?;"),
    ("W4 replay: pages applied without a commit", WAL, "pending.push((u64_at(PAGE_NO_AT), Box::new(image)));", "done.pages.insert(u64_at(PAGE_NO_AT), Box::new(image));"),
    ("W5 crc not chained", WAL, [("let crc = crc32c::crc32c_append(prev, &out[start..]);", "let crc = crc32c::crc32c_append(START, &out[start..]); let _ = prev;"),
                                 ("crc = crc32c::crc32c_append(crc, body);", "crc = crc32c::crc32c_append(START, body);")]),
    ("W6 commit: end not moved on", WAL, "end: self.tail.end + bytes.len() as u64,", "end: self.tail.end,"),
    ("W7 replay: bad crc ignored", WAL, "if stored != crc.to_le_bytes() {\n            return done;", "if false && stored != crc.to_le_bytes() {\n            return done;"),
    ("W9 open: cut bytes not kept", DB, "kept.write_all_at(cut, kept.size()?)?;", "let _ = (&kept, cut);"),
    ("W10 open: a long WAL with no commit made into a new DB", DB, "None if bytes.len() as u64 > wal::commit_len(1) => {", "None if false => {"),
    ("W11 commit: a commit of nothing written anyway", DB, "if next.has_dirty() || next.meta() != tree.meta() {", "if true {"),
    ("W8 replay: last commit's crc not kept", WAL, "            end: at as u64,\n            crc,", "            end: at as u64,\n            crc: START,"),
]
WAL_TESTS = ["--lib", "--test", "wal", "--test", "crash", "--test", "sim_fs"]

REAL_DISK = [a for t in ["single_page", "delete_and_overwrite", "split", "multi_level", "scan", "overflow", "checksum"] for a in ("--test", t)]


def run(args):
    r = subprocess.run(["cargo", "test", "-q"] + args, capture_output=True, text=True)
    out = r.stdout + r.stderr
    failed = sorted({l.strip() for l in out.splitlines() if l.startswith("    ") and re.fullmatch(r"[a-z_:]+", l.strip())})
    return ("caught" if r.returncode else "missed") + (f" {failed}" if failed else "")


def wal():
    """Plants each WAL bug, which may change db.rs or wal.rs in places."""
    originals = {path: open(path).read() for path in (DB, WAL)}
    try:
        for name, path, *change in WAL_BUGS:
            pairs = change[0] if len(change) == 1 else [tuple(change)]
            text = originals[path]
            for old, new in pairs:
                assert text.count(old) == 1, f"{name}: pattern not found exactly once"
                text = text.replace(old, new)
            open(path, "w").write(text)
            print(name)
            print("  WAL, crash and sim tests:", run(WAL_TESTS))
            open(path, "w").write(originals[path])
    finally:
        for path, text in originals.items():
            open(path, "w").write(text)


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "sim"
    if which == "wal":
        return wal()
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
