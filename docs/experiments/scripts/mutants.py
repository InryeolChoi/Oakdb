#!/usr/bin/env python3
"""Plants one bug at a time and reports which tests catch it.

Run from the repository root:

    python3 docs/experiments/scripts/mutants.py sim      # 20 bugs in SimFs
    python3 docs/experiments/scripts/mutants.py engine   # experiment 0007
    python3 docs/experiments/scripts/mutants.py wal      # experiment 0009
    python3 docs/experiments/scripts/mutants.py checkpoint  # experiment 0010
    python3 docs/experiments/scripts/mutants.py failure  # experiment 0011

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
    ("never torn", "match draw % 3 {", "match draw % 2 {"),
    ("torn ignores sector bounds", "let stop = ((start / SECTOR + 1) * SECTOR).min(end);", "let stop = (start + 100).min(end);"),
    ("sync_dir does nothing", "file.entry_synced = true;\n            }", "let _ = file;\n            }"),
    ("files never vanish", "if !file.entry_synced && rng.coin() {", "if false && !file.entry_synced && rng.coin() {"),
    ("length changes always land", "let landed = rng.coin();\n            if landed {", "let landed = rng.coin() || true;\n            if landed {"),
    ("seed ignored", "let mut rng = Rng(seed);", "let mut rng = Rng(seed * 0);"),
    ("reads count towards fail_after", "self.with(Access::Read, |file, _| file.now.len() as u64)", "self.with(Access::Change, |file, _| file.now.len() as u64)"),
    ("lock not released on drop", "file.locked_by = None;", "let _ = file;"),
    ("old handles keep working", "disk.boot += 1;", "disk.boot += 0;"),
    ("power stays cut after a crash", "disk.down = false;", "disk.down = disk.down;"),
    ("ops miscounted", "self.ops += 1;\n        Ok(())", "self.ops += 2;\n        Ok(())"),
    ("report lies about torn sectors", "sectors.push(landed);", "sectors.push(!landed);"),
    # Writes that land their length alone (#56).
    ("never length only", "Some(zeros_from) if draw >> 63 == 1 =>", "Some(zeros_from) if false =>"),
    ("length only writes the bytes", "data.resize(grown.end, 0);", "let _ = grown; write_at(data, offset, &bytes);"),
    ("length only does not grow the file", "data.resize(grown.end, 0);", "let _ = grown;"),
    ("zeros from the offset, not the disk's end", ".then(|| offset.max(disk_len))", ".then_some(offset)"),
    ("writes inside the file land length only", "(end > disk_len).then(|| offset.max(disk_len))", "Some(offset)"),
    # The rule before the review of PR #58: the end measured when the write
    # was made, so zeros could cover bytes the disk still held.
    ("end measured when the write was made", [
        ("    Write { offset: u64, bytes: Vec<u8> },", "    Write { offset: u64, bytes: Vec<u8>, old_len: usize },"),
        ("Change::Write { offset, bytes } => {", "Change::Write { offset, bytes, old_len } => {"),
        ("let disk_len = data.len() as u64;", "let disk_len = old_len as u64;"),
        ("data.resize(grown.end, 0);", "let zeros = disk_len.max(offset) as usize..grown.end; data.resize(data.len().max(grown.end), 0); data[zeros].fill(0);"),
        ("            write_at(&mut file.now, offset, buf);\n            file.waiting.push(Change::Write {\n                offset,\n                bytes: buf.to_vec(),\n            });",
         "            let old_len = file.now.len();\n            write_at(&mut file.now, offset, buf);\n            file.waiting.push(Change::Write {\n                offset,\n                bytes: buf.to_vec(),\n                old_len,\n            });"),
    ]),
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
    ("C1 checkpoint: no sync before the meta slot", CKPT, "    file.sync_data()?;\n    let slot = Slot {", "    let slot = Slot {"),
    ("C2 checkpoint: no sync after the meta slot", CKPT, "    file.sync_data()?;\n    wal.start(next)", "    wal.start(next)"),
    # C3 is expected to survive since PR #58: on a disk that journals the
    # emptying before the growth, losing this sync loses nothing (design
    # 0013, section 9). The sync stays as a cheap defence.
    ("C3 WAL start: no sync between emptying and the header (expected to survive)", WAL, "            self.file.set_len(0)?;\n            self.file.sync_data()?;", "            self.file.set_len(0)?;"),
    ("C4 first checkpoint: one slot only", CKPT, "0..2\n", "0..1\n"),
    ("C5 checkpoint: writes over the newest slot", CKPT, "next % 2..next % 2 + 1", "(next + 1) % 2..(next + 1) % 2 + 1"),
    ("C6 open: always slot A", DB, "(Ok(a), Ok(b)) if b.checkpoint > a.checkpoint => Ok(b),", "(Ok(a), Ok(b)) if false && b.checkpoint > a.checkpoint => Ok(b),"),
    ("C7 open: a WAL of an older checkpoint replayed", DB, "if after == slot.checkpoint => {", "if after <= slot.checkpoint => {"),
    ("C8 open: a WAL newer than every slot not refused", DB, "if after > slot.checkpoint => {", "if false && after > slot.checkpoint => {"),
    ("C9 open: a first checkpoint cut short not recovered", DB, "(Err(Error::Corrupt(_)), Some(0)) if wal_has_commit =>", "(Err(Error::Corrupt(_)), Some(0)) if false =>"),
    ("C10 replay: header not checked", WAL, "if header != encode_header(checkpoint).0 {", "if false {"),
    ("C11 replay: chain does not start from the header", WAL, "let (mut at, mut crc) = (HEADER, done.tail.crc);", "let (mut at, mut crc) = (HEADER, START);"),
    ("C12 checkpoint: a failure does not stop the engine", DB, ".inspect_err(|e| self.stop(e))", ""),
    ("C13 commit: never checkpoints", DB, "if wal.len() > CHECKPOINT_AFTER {", "if false {"),
    ("C14 open: data file length not checked", DB, "if u128::from(size) < pages_len(slot.meta.page_count) {", "if false {"),
    ("C15 checkpoint: runs with nothing to move", CKPT, "    if !wal.has_commits() {\n        return Ok(());\n    }\n", ""),
    # After the review of PR #55.
    ("C16 replay: a wrong header one header long refused (P1)", WAL, "if bytes.len() as u64 == HEADER_LEN {", "if false {"),
    ("C17 open: any slot error passed over before the first checkpoint (P2)", DB, "(Err(Error::Corrupt(_)), Some(0)) if wal_has_commit =>", "(Err(_), Some(0)) if wal_has_commit =>"),
    ("C18 open: data file length wraps around (S3)", DB, "u128::from(pages) * PAGE_SIZE as u128", "u128::from(pages.wrapping_mul(PAGE_SIZE as u64))"),
    ("C19 checkpoint: number wraps around (S3)", CKPT, ".checked_add(1)", ".wrapping_add(1).checked_add(0)"),
]
CHECKPOINT_TESTS = WAL_TESTS + [a for t in ["checkpoint", "checksum", "overflow", "disk_failure"] for a in ("--test", t)]

# Engine bugs of stopping after a disk failure (#24), and SimFs bugs of
# failing one call with the power on.
FAILURE_BUGS = [
    ("F1 commit: a failed WAL write does not stop the engine", DB, "                self.db.stop(&e);\n                return Err(e);", "                return Err(e);"),
    ("F2 commit: a failed WAL write reports Io, not OutcomeUnknown", DB, "let e = Error::OutcomeUnknown(Arc::new(e));", "let e = Error::Io(Arc::new(e));"),
    ("F3 commit: a failed checkpoint after it reports Err", DB, "                self.db.stop(&e);\n            }\n        }\n        Ok(())", "                self.db.stop(&e);\n                return Err(e);\n            }\n        }\n        Ok(())"),
    ("F4 commit: a failed checkpoint after it does not stop the engine", DB, "                self.db.stop(&e);\n            }\n        }\n        Ok(())", "                let _ = e;\n            }\n        }\n        Ok(())"),
    ("F5 commit: the stop not checked holding the lock", DB, "        self.db.check_running()?;\n        let mut next = Tree::clone(&tree);", "        let mut next = Tree::clone(&tree);"),
    ("F6 commit of nothing: the stop not checked", DB, "return self.db.check_running();", "return Ok(());"),
    ("F7 get: the stop not checked", DB, "-> Result<Option<Vec<u8>>> {\n        self.db.check_running()?;", "-> Result<Option<Vec<u8>>> {"),
    ("F8 put: the stop not checked", DB, "value: &[u8]) -> Result<()> {\n        self.db.check_running()?;", "value: &[u8]) -> Result<()> {"),
    ("F9 delete: the stop not checked", DB, "pub fn delete(&mut self, key: &[u8]) -> Result<()> {\n        self.db.check_running()?;", "pub fn delete(&mut self, key: &[u8]) -> Result<()> {"),
    ("F10 scan: the stop not checked", DB, "        if let Err(e) = self.db.check_running() {\n            return Scan::stopped(tree, e);\n        }\n", ""),
    ("F11 checkpoint: the stop not checked", DB, "        self.check_running()?;\n        checkpoint::run", "        checkpoint::run"),
    ("F12 stop: the cause is not kept", DB, "Some(cause) => Err(Error::Stopped(Arc::clone(cause))),", "Some(_) => Err(Error::Stopped(Arc::new(Error::AlreadyOpen))),"),
    ("S1 SimFs: a failure never ends", SIM, "            self.fail_once_left = None;\n            return Err(disk_failed());", "            self.fail_once_left = Some(0);\n            return Err(disk_failed());"),
    ("S2 SimFs: the countdown never moves", SIM, "&mut self.fail_once_left {\n            *left -= 1;", "&mut self.fail_once_left {\n            let _ = left;"),
    ("S3 SimFs: the failure comes before a power cut due at the same call", SIM,
     "        if self.ops_left == Some(0) {\n            self.down = true;\n            return Err(power_cut());\n        }\n        if self.fail_once_left == Some(0) {\n            self.fail_once_left = None;\n            return Err(disk_failed());\n        }\n",
     "        if self.fail_once_left == Some(0) {\n            self.fail_once_left = None;\n            return Err(disk_failed());\n        }\n        if self.ops_left == Some(0) {\n            self.down = true;\n            return Err(power_cut());\n        }\n"),
    ("S4 SimFs: the failed call is counted", SIM, "            self.fail_once_left = None;\n            return Err(disk_failed());", "            self.fail_once_left = None;\n            self.ops += 1;\n            return Err(disk_failed());"),
    # After the review of PR #57.
    ("S5 SimFs: a failure the power cut got ahead of carries over to the next boot (P4)", SIM, "        disk.fail_once_left = None;\n", ""),
    ("S6 SimFs: a failure gives no OS error code", SIM, "io::Error::from_raw_os_error(libc::EIO)", "io::Error::other(\"the disk failed\")"),
    ("F13 stop: the cause is a flattened copy (S6, P5)", DB, "let _ = self.stopped.set(Arc::new(cause.clone()));", "let _ = self.stopped.set(Arc::new(Error::from(std::io::Error::other(cause.to_string()))));"),
]
FAILURE_TESTS = CHECKPOINT_TESTS

REAL_DISK = [a for t in ["single_page", "delete_and_overwrite", "split", "multi_level", "scan", "overflow", "checksum"] for a in ("--test", t)]


def run(args):
    r = subprocess.run(["cargo", "test", "-q"] + args, capture_output=True, text=True)
    out = r.stdout + r.stderr
    failed = sorted({l.strip() for l in out.splitlines() if l.startswith("    ") and re.fullmatch(r"[a-z_:]+", l.strip())})
    return ("caught" if r.returncode else "missed") + (f" {failed}" if failed else "")


def engine(bugs, tests, label):
    """Plants each bug, which may change one of several files in places."""
    originals = {path: open(path).read() for path in (DB, WAL, CKPT, SIM)}
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
    if which == "failure":
        return engine(FAILURE_BUGS, FAILURE_TESTS, "disk failure, checkpoint, WAL, crash and sim tests")
    path, bugs = (SIM, SIM_BUGS) if which == "sim" else (DB, ENGINE_BUGS)
    original = open(path).read()
    try:
        for name, *change in bugs:
            pairs = change[0] if len(change) == 1 else [tuple(change)]
            text = original
            for old, new in pairs:
                assert text.count(old) == 1, f"{name}: pattern not found exactly once"
                text = text.replace(old, new)
            open(path, "w").write(text)
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
