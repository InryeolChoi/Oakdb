#!/usr/bin/env python3
"""Plants one bug at a time and reports which tests catch it.

Run from the repository root:

    python3 docs/experiments/scripts/mutants.py sim      # 14 bugs in SimFs
    python3 docs/experiments/scripts/mutants.py engine   # experiment 0007
    python3 docs/experiments/scripts/mutants.py wal      # experiment 0009
    python3 docs/experiments/scripts/mutants.py checkpoint  # experiment 0010

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
CKPT = "crates/oakdb/src/checkpoint.rs"

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
    ("W1 commit: no sync", WAL, "        self.file.write_all_at(&bytes, self.tail.end)?;\n        self.file.sync_data()?;", "        self.file.write_all_at(&bytes, self.tail.end)?;"),
    ("W2 open: tail not cut off", WAL, "        self.file.set_len(self.tail.end)?;\n", ""),
    ("W3 open: no directory sync", DB, "        fs.sync_dir(dir)?;\n        Ok(Db {", "        Ok(Db {"),
    ("W4 replay: pages applied without a commit", WAL, "pending.push((u64_at(PAGE_NO_AT), Box::new(image)));", "done.pages.insert(u64_at(PAGE_NO_AT), Box::new(image));"),
    ("W5 crc not chained", WAL, [("let crc = crc32c::crc32c_append(prev, &out[start..]);", "let crc = crc32c::crc32c_append(START, &out[start..]); let _ = prev;"),
                                 ("crc = crc32c::crc32c_append(crc, body);", "crc = crc32c::crc32c_append(START, body);")]),
    ("W6 commit: end not moved on", WAL, "end: self.tail.end + bytes.len() as u64,", "end: self.tail.end,"),
    ("W7 replay: bad crc ignored", WAL, "if stored != crc.to_le_bytes() {\n            return Ok(done);", "if false && stored != crc.to_le_bytes() {\n            return Ok(done);"),
    ("W9 open: cut bytes not kept", DB, "kept.write_all_at(cut, kept.size()?)?;", "let _ = (&kept, cut);"),
    ("W10 open: a long WAL with no commit made into a new DB", DB, "None if bytes.len() as u64 > HEADER_LEN + wal::commit_len(1) => {", "None if false => {"),
    ("W11 commit: a commit of nothing written anyway", DB, "if next.has_dirty() || next.meta() != tree.meta() {", "if true {"),
    ("W8 replay: last commit's crc not kept", WAL, "            end: at as u64,\n            crc,", "            end: at as u64,\n            crc: START,"),
]
WAL_TESTS = ["--lib", "--test", "wal", "--test", "crash", "--test", "sim_fs"]

# Engine bugs of checkpoints and the two meta slots (#23).
CHECKPOINT_BUGS = [
    ("C1 checkpoint: no sync before the meta slot", CKPT, "    file.sync_data()?;\n    let next", "    let next"),
    ("C2 checkpoint: no sync after the meta slot", CKPT, "    file.sync_data()?;\n    wal.start(next)", "    wal.start(next)"),
    ("C3 WAL start: no sync between emptying and the header", WAL, "            self.file.set_len(0)?;\n            self.file.sync_data()?;", "            self.file.set_len(0)?;"),
    ("C4 first checkpoint: one slot only", CKPT, "0..2\n", "0..1\n"),
    ("C5 checkpoint: writes over the newest slot", CKPT, "next % 2..next % 2 + 1", "(next + 1) % 2..(next + 1) % 2 + 1"),
    ("C6 open: always slot A", DB, "(Ok(a), Ok(b)) if b.checkpoint > a.checkpoint => Ok(b),", "(Ok(a), Ok(b)) if false && b.checkpoint > a.checkpoint => Ok(b),"),
    ("C7 open: a WAL of an older checkpoint replayed", DB, "(Some(slot), after == Some(slot.checkpoint))", "(Some(slot), after.is_some())"),
    ("C8 open: a WAL newer than every slot not refused", DB, "if after > slot.checkpoint => {", "if false && after > slot.checkpoint => {"),
    ("C9 open: a first checkpoint cut short not recovered", DB, "(Err(_), Some(0)) if replayed.meta.is_some() =>", "(Err(_), Some(0)) if false =>"),
    ("C10 replay: header not checked", WAL, "if header != encode_header(checkpoint).0 {", "if false {"),
    ("C11 replay: chain does not start from the header", WAL, "let (mut at, mut crc) = (HEADER_LEN as usize, done.tail.crc);", "let (mut at, mut crc) = (HEADER_LEN as usize, START);"),
    ("C12 checkpoint: a failure does not stop the engine", CKPT, "        wal.stop();\n", ""),
    ("C13 commit: never checkpoints", DB, "if wal.len() > CHECKPOINT_AFTER {", "if false {"),
    ("C14 open: data file length not checked", DB, "if size < page_offset(slot.meta.page_count) {", "if false {"),
    ("C15 checkpoint: runs with nothing to move", CKPT, "    if !wal.has_commits() {\n        return Ok(());\n    }\n", ""),
]
CHECKPOINT_TESTS = WAL_TESTS + [a for t in ["checkpoint", "checksum", "overflow"] for a in ("--test", t)]

REAL_DISK = [a for t in ["single_page", "delete_and_overwrite", "split", "multi_level", "scan", "overflow", "checksum"] for a in ("--test", t)]


def run(args):
    r = subprocess.run(["cargo", "test", "-q"] + args, capture_output=True, text=True)
    out = r.stdout + r.stderr
    failed = sorted({l.strip() for l in out.splitlines() if l.startswith("    ") and re.fullmatch(r"[a-z_:]+", l.strip())})
    return ("caught" if r.returncode else "missed") + (f" {failed}" if failed else "")


def engine(bugs, tests, label):
    """Plants each bug, which may change one of several files in places."""
    originals = {path: open(path).read() for path in (DB, WAL, CKPT)}
    try:
        for name, path, *change in bugs:
            pairs = change[0] if len(change) == 1 else [tuple(change)]
            text = originals[path]
            for old, new in pairs:
                assert text.count(old) == 1, f"{name}: pattern not found exactly once"
                text = text.replace(old, new)
            open(path, "w").write(text)
            print(name)
            print(f"  {label}:", run(tests))
            open(path, "w").write(originals[path])
    finally:
        for path, text in originals.items():
            open(path, "w").write(text)


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "sim"
    if which == "wal":
        return engine(WAL_BUGS, WAL_TESTS, "WAL, crash and sim tests")
    if which == "checkpoint":
        return engine(CHECKPOINT_BUGS, CHECKPOINT_TESTS, "checkpoint, WAL, crash and sim tests")
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
