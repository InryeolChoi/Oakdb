//! Baseline benchmarks for the single-page engine (docs/benchmarks/0001).
//!
//! Reads run against a full page. Commits overwrite keys that are already
//! there, so the page stays full and every few commits has to compact.
//! Commits are measured twice: on the real file system, and with syncs
//! turned off to see what the engine itself costs.

use std::fs::{File, OpenOptions};
use std::hint::black_box;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use oakdb::fs::{Fs, FsFile, StdFs};
use oakdb::{Db, Error};

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

/// Fills the page with 4-byte keys and values, one commit each, and
/// returns how many fit.
fn fill<F: Fs>(db: &Db<F>) -> u32 {
    for i in 0u32.. {
        let mut txn = db.begin();
        txn.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
        match txn.commit() {
            Ok(()) => {}
            Err(Error::PageFull) => return i,
            Err(e) => panic!("{e}"),
        }
    }
    unreachable!()
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
    group.finish();
}

fn commit_overwrites<F: Fs>(c: &mut Criterion, name: &str, db: &Db<F>) {
    let n = fill(db);
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
    commit_overwrites(c, "commit/no_sync", &db);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(StdFs, dir.path()).unwrap();
    commit_overwrites(c, "commit/durable", &db);
}

criterion_group!(benches, reads, commits);
criterion_main!(benches);
