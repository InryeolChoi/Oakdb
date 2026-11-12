//! Fault experiment helper (docs/experiments/0005-crash-with-overflow-pages.md).
//!
//! `overflow_crash <dir>` stops commits that write overflow pages or free
//! pages part way, as if the process had died, then reopens the database
//! and reports what is left. Each case runs in its own subdirectory.
//!
//! - `new-pages`: a long value goes on new pages at the end of the file.
//! - `reused-pages`: a long value goes on pages a deleted value freed.
//! - `delete`: deleting a long value frees its pages.
//!
//! A commit writes its changed pages in page number order, then the meta
//! page. The leaf (page 2) sorts before every overflow page, so it is
//! always the first page written.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use oakdb::fs::{Fs, FsFile, StdFs};
use oakdb::{Db, DATA_FILE};

/// A value that takes five overflow pages.
const LONG: usize = 5 * 4056;

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
        .expect("usage: overflow_crash <dir>");
    let root = Path::new(&root);
    // The commit writes the leaf, then five overflow pages, then the meta
    // page: seven writes.
    for writes in [1, 3, 6] {
        new_pages(&root.join(format!("new-pages-{writes}")), writes);
    }
    for writes in [1, 3, 6] {
        reused_pages(&root.join(format!("reused-pages-{writes}")), writes);
    }
    for writes in [1, 3, 6] {
        delete(&root.join(format!("delete-{writes}")), writes);
    }
}

fn new_pages(dir: &Path, writes: u32) {
    println!("== new-pages, stop after {writes} page writes");
    commit(dir, |txn| txn.put(b"small", b"kept").unwrap());
    report(dir, "before");
    crash_commit(dir, writes, |txn| txn.put(b"long", &long(1)).unwrap());
    report(dir, "after");
}

fn reused_pages(dir: &Path, writes: u32) {
    println!("== reused-pages, stop after {writes} page writes");
    commit(dir, |txn| txn.put(b"small", b"kept").unwrap());
    commit(dir, |txn| txn.put(b"old", &long(1)).unwrap());
    commit(dir, |txn| txn.delete(b"old").unwrap());
    report(dir, "before");
    crash_commit(dir, writes, |txn| txn.put(b"long", &long(2)).unwrap());
    report(dir, "after");
}

fn delete(dir: &Path, writes: u32) {
    println!("== delete, stop after {writes} page writes");
    commit(dir, |txn| txn.put(b"small", b"kept").unwrap());
    commit(dir, |txn| txn.put(b"long", &long(1)).unwrap());
    report(dir, "before");
    crash_commit(dir, writes, |txn| txn.delete(b"long").unwrap());
    report(dir, "after");
}

fn long(version: u8) -> Vec<u8> {
    vec![version; LONG]
}

fn commit(dir: &Path, write: impl FnOnce(&mut oakdb::Txn<'_>)) {
    let db = Db::open(dir).unwrap();
    let mut txn = db.begin();
    write(&mut txn);
    txn.commit().unwrap();
    db.close().unwrap();
}

fn crash_commit(dir: &Path, writes: u32, write: impl FnOnce(&mut oakdb::Txn<'_, CrashFs>)) {
    let db = Db::open_with(CrashFs::after(writes), dir).unwrap();
    let mut txn = db.begin();
    write(&mut txn);
    match txn.commit() {
        Ok(()) => println!("commit: ok"),
        Err(e) => println!("commit: {e}"),
    }
    db.close().unwrap();
}

/// Prints what a fresh open finds.
fn report(dir: &Path, step: &str) {
    let pages = std::fs::metadata(dir.join(DATA_FILE)).unwrap().len() / 4096;
    let db = match Db::open(dir) {
        Ok(db) => db,
        Err(e) => return println!("{step:6} file pages: {pages}  open fails: {e}"),
    };
    let txn = db.begin();
    let show = |key: &[u8]| match txn.get(key).unwrap() {
        None => "missing".to_string(),
        Some(v) if v.len() == LONG => format!("long v{}", v[0]),
        Some(v) => String::from_utf8_lossy(&v).into_owned(),
    };
    println!(
        "{step:6} file pages: {pages}  small: {}  long: {}",
        show(b"small"),
        show(b"long")
    );
}
