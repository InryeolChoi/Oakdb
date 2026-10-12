//! Fault experiment helper (docs/experiments/0004-crash-in-a-multi-page-commit.md).
//!
//! `multi_page_crash <dir>` runs two cases, each in its own subdirectory, where a
//! commit that writes more than one page stops part way, as if the process
//! had died. Then it reopens the database and reports what is left.
//!
//! - `split`: the commit that splits a full leaf stops just before the meta
//!   page, after the split pages are written.
//! - `two-leaves`: a commit that changes one key in each of two leaves stops
//!   after the first leaf is written.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use oakdb::fs::{Fs, FsFile, StdFs};
use oakdb::{Db, DATA_FILE};

/// Entries of a 4-byte key and a 100-byte value that fill one leaf.
const ONE_LEAF: u32 = 36;

/// The real file system, except that once `writes_left` page writes have
/// gone through, every later write fails, as if the process had died.
#[derive(Clone)]
struct CrashFs {
    writes_left: Arc<AtomicU32>,
}

struct CrashFile {
    file: <StdFs as Fs>::File,
    writes_left: Arc<AtomicU32>,
}

impl CrashFs {
    fn after(writes: u32) -> Self {
        CrashFs {
            writes_left: Arc::new(AtomicU32::new(writes)),
        }
    }
}

impl Fs for CrashFs {
    type File = CrashFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<CrashFile> {
        Ok(CrashFile {
            file: StdFs.open(path)?,
            writes_left: self.writes_left.clone(),
        })
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        StdFs.sync_dir(dir)
    }
}

impl FsFile for CrashFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let left = self.writes_left.load(Ordering::SeqCst);
        if left == 0 {
            let page = offset / 4096;
            return Err(io::Error::other(format!(
                "crashed before writing page {page}"
            )));
        }
        self.writes_left.store(left - 1, Ordering::SeqCst);
        self.file.write_all_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
    fn try_lock(&self) -> io::Result<()> {
        self.file.try_lock()
    }
    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("usage: multi_page_crash <dir>");
    split(&Path::new(&root).join("split"));
    two_leaves(&Path::new(&root).join("two-leaves"));
}

fn split(dir: &Path) {
    println!("== split");
    commit_keys(dir, 0..ONE_LEAF);
    report(dir, "before");

    // The split writes the old leaf, the new right leaf and the new root,
    // then the meta page. Let the three through.
    let db = Db::open_with(CrashFs::after(3), dir).unwrap();
    let mut txn = db.begin();
    txn.put(&ONE_LEAF.to_be_bytes(), &value(ONE_LEAF, 1))
        .unwrap();
    println!("commit: {}", txn.commit().unwrap_err());
    db.close().unwrap();
    report(dir, "after");
}

fn two_leaves(dir: &Path) {
    println!("== two-leaves");
    commit_keys(dir, 0..ONE_LEAF + 1);
    report(dir, "before");

    // Key 0 is in the left leaf, the last key in the right one. Let the
    // first page write through.
    let db = Db::open_with(CrashFs::after(1), dir).unwrap();
    let mut txn = db.begin();
    txn.put(&0u32.to_be_bytes(), &value(0, 2)).unwrap();
    txn.put(&ONE_LEAF.to_be_bytes(), &value(ONE_LEAF, 2))
        .unwrap();
    println!("commit: {}", txn.commit().unwrap_err());
    db.close().unwrap();
    report(dir, "after");
}

/// The value of key `i` in version `version`.
fn value(i: u32, version: u8) -> Vec<u8> {
    let mut value = vec![version; 100];
    value[..4].copy_from_slice(&i.to_le_bytes());
    value
}

/// Commits each of `keys` at version 1, one transaction each.
fn commit_keys(dir: &Path, keys: std::ops::Range<u32>) {
    let db = Db::open(dir).unwrap();
    for i in keys {
        let mut txn = db.begin();
        txn.put(&i.to_be_bytes(), &value(i, 1)).unwrap();
        txn.commit().unwrap();
    }
    db.close().unwrap();
}

/// Prints what a fresh open finds: how many keys there are at each version.
fn report(dir: &Path, step: &str) {
    let pages = std::fs::metadata(dir.join(DATA_FILE)).unwrap().len() / 4096;
    let db = match Db::open(dir) {
        Ok(db) => db,
        Err(e) => return println!("{step:6} file pages: {pages}  open fails: {e}"),
    };
    let txn = db.begin();
    let mut versions = std::collections::BTreeMap::new();
    for i in 0..=ONE_LEAF {
        let version = txn.get(&i.to_be_bytes()).unwrap().map(|v| v[4]);
        *versions.entry(version).or_insert(0) += 1;
    }
    println!("{step:6} file pages: {pages}  keys by version (None = missing): {versions:?}");
}
