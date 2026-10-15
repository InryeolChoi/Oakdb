//! Benchmarks for one leaf (docs/benchmarks/0001), a two-level tree
//! (docs/benchmarks/0003) and range scans (docs/benchmarks/0005).
//!
//! Reads run against a full leaf. Commits overwrite keys that are already
//! there, so the leaf stays full and every few commits has to compact.
//! Commits are measured twice: on the real file system, and with syncs
//! turned off to see what the engine itself costs. The `two_levels` cases
//! do the same on a tree of about 160 leaves under one root. Scans read 100
//! entries from a moving start, and the whole of that tree.

use std::fs::{File, OpenOptions};
use std::hint::black_box;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use oakdb::fs::{Fs, FsFile, StdFs};
use oakdb::Db;

/// The real file system, except that syncs do nothing. Not durable: only
/// for measuring the engine without the disk.
#[derive(Clone, Copy)]
struct NoSyncFs;

struct NoSyncFile(File);

impl Fs for NoSyncFs {
    type File = NoSyncFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<NoSyncFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(NoSyncFile(file))
    }

    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

impl FsFile for NoSyncFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_all_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn try_lock(&self) -> io::Result<()> {
        self.0.try_lock().map_err(io::Error::from)
    }
    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
}

/// How many entries of a 4-byte key and value fill one leaf.
const ONE_LEAF: u32 = 253;

/// About 160 leaves of such entries, all under the root.
const TWO_LEVELS: u32 = 20_000;

/// Fills a leaf with 4-byte keys and values, one commit each, and returns
/// how many there are.
fn fill<F: Fs>(db: &Db<F>) -> u32 {
    for i in 0..ONE_LEAF {
        let mut txn = db.begin();
        txn.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
        txn.commit().unwrap();
    }
    ONE_LEAF
}

/// Puts [`TWO_LEVELS`] 4-byte keys and values in one commit and returns how
/// many there are.
fn fill_two_levels<F: Fs>(db: &Db<F>) -> u32 {
    let mut txn = db.begin();
    for i in 0..TWO_LEVELS {
        txn.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
    }
    txn.commit().unwrap();
    TWO_LEVELS
}

fn reads(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(NoSyncFs, dir.path()).unwrap();
    let n = fill(&db);
    let mut group = c.benchmark_group("get");
    let mut i = 0u32;
    group.bench_function("hit", |b| {
        b.iter(|| {
            i = (i + 7) % n;
            black_box(db.begin().get(&i.to_be_bytes()).unwrap())
        })
    });
    group.bench_function("miss", |b| {
        b.iter(|| black_box(db.begin().get(b"absent!").unwrap()))
    });

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(NoSyncFs, dir.path()).unwrap();
    let n = fill_two_levels(&db);
    group.bench_function("hit_two_levels", |b| {
        b.iter(|| {
            i = (i + 7919) % n;
            black_box(db.begin().get(&i.to_be_bytes()).unwrap())
        })
    });
    group.finish();
}

fn commit_overwrites<F: Fs>(c: &mut Criterion, name: &str, db: &Db<F>, n: u32) {
    let mut i = 0u32;
    c.bench_function(name, |b| {
        b.iter(|| {
            i = (i + 7) % n;
            let mut txn = db.begin();
            txn.put(&i.to_be_bytes(), &i.wrapping_mul(31).to_be_bytes())
                .unwrap();
            txn.commit().unwrap();
        })
    });
}

fn commits(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(NoSyncFs, dir.path()).unwrap();
    let n = fill(&db);
    commit_overwrites(c, "commit/no_sync", &db, n);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(StdFs, dir.path()).unwrap();
    let n = fill(&db);
    commit_overwrites(c, "commit/durable", &db, n);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(NoSyncFs, dir.path()).unwrap();
    let n = fill_two_levels(&db);
    commit_overwrites(c, "commit/no_sync_two_levels", &db, n);
}

fn scans(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(NoSyncFs, dir.path()).unwrap();
    let n = fill_two_levels(&db);
    let mut group = c.benchmark_group("scan");
    let mut i = 0u32;
    group.bench_function("100_two_levels", |b| {
        b.iter(|| {
            i = (i + 7919) % (n - 100);
            let txn = db.begin();
            let count = txn
                .scan(i.to_be_bytes()..)
                .take(100)
                .fold(0, |count, item| {
                    black_box(item.unwrap());
                    count + 1
                });
            assert_eq!(count, 100);
        })
    });
    group.bench_function("all_two_levels", |b| {
        b.iter(|| {
            let txn = db.begin();
            let count = txn.scan::<&[u8]>(..).fold(0, |count, item| {
                black_box(item.unwrap());
                count + 1
            });
            assert_eq!(count, n as usize);
        })
    });
    group.finish();
}

criterion_group!(benches, reads, commits, scans);
criterion_main!(benches);
