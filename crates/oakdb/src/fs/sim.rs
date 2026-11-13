//! A simulated file system in memory, for crash tests.
//!
//! [`SimFs`] keeps two copies of every file: what reads see, and what is on
//! the "disk", which is only what was synced. Until then each write and
//! length change waits in a list. [`SimFs::crash`] cuts the power and starts
//! again: each waiting change is kept, lost, or (for a write) torn so that
//! only some of its 512-byte sectors land, and a file created since its
//! directory was last synced may vanish. A seed makes every choice, so the
//! same seed and the same operations always give the same disk.
//!
//! [`SimFs::fail_after`] cuts the power part way through the engine's work:
//! after a given number of operations that change the disk, every call
//! fails until the next crash.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Fs, FsFile};

/// The unit a torn write lands in: a disk writes whole sectors or nothing.
pub const SECTOR: u64 = 512;

/// A file system that lives in memory and can lose power.
///
/// Clones share the same disk, so a test can keep one and hand another to
/// the engine.
#[derive(Clone, Default)]
pub struct SimFs {
    disk: Arc<Mutex<Disk>>,
}

#[derive(Default)]
struct Disk {
    /// Counts crashes. Handles opened before the last one stop working.
    boot: u64,
    /// The power is cut: every call fails until [`SimFs::crash`].
    down: bool,
    /// Operations that change the disk left before the power is cut.
    ops_left: Option<u64>,
    /// Operations that changed the disk so far, across crashes.
    ops: u64,
    /// Directories. They are always durable: [`Fs::create_dir_all`] syncs
    /// them, and nothing else makes one.
    dirs: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, File>,
    /// Gives each handle its own number, for locks.
    next_handle: u64,
}

#[derive(Default)]
struct File {
    /// What reads see.
    now: Vec<u8>,
    /// What a crash starts from: the contents at the last sync.
    synced: Vec<u8>,
    /// Changes since the last sync, oldest first.
    waiting: Vec<Change>,
    /// The directory entry survives a crash: its directory was synced
    /// after the file was created.
    entry_synced: bool,
    /// The handle holding the lock.
    locked_by: Option<u64>,
}

enum Change {
    Write { offset: u64, bytes: Vec<u8> },
    SetLen(usize),
}

/// What a crash did to the work that was never synced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrashReport {
    /// Files that vanished because their directory was never synced.
    pub vanished: Vec<PathBuf>,
    /// Every unsynced write of the files that stayed, oldest first within
    /// a file, files in path order.
    pub writes: Vec<WriteFate>,
    /// Every unsynced length change, in the same order.
    pub lengths: Vec<LengthFate>,
}

/// One unsynced write and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteFate {
    pub path: PathBuf,
    pub offset: u64,
    pub len: usize,
    pub landing: Landing,
}

/// One unsynced length change and whether it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LengthFate {
    pub path: PathBuf,
    pub len: u64,
    pub landed: bool,
}

/// Whether a call on an open file counts towards [`SimFs::fail_after`].
#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    Change,
}

impl SimFs {
    /// An empty disk with the power on.
    pub fn new() -> Self {
        SimFs::default()
    }

    /// Cuts the power after `ops` more operations that change the disk:
    /// writes, length changes, syncs, directory creation, and opens (of an
    /// existing file too). Reads, sizes and locks do not count. The next
    /// such operation after those finds the power gone and fails without
    /// touching the disk, and so does every call after it, reads included,
    /// until [`SimFs::crash`]. Until that operation comes, reads still work:
    /// `fail_after(0)` cuts the power at the next change, not at once.
    pub fn fail_after(&self, ops: u64) {
        self.disk().ops_left = Some(ops);
    }

    /// How many operations that change the disk have succeeded since the
    /// disk was made, counted as [`SimFs::fail_after`] counts them.
    pub fn ops(&self) -> u64 {
        self.disk().ops
    }

    /// Cuts the power now, decides with `seed` what of the unsynced work
    /// reached the disk, and starts again with the power on. Handles opened
    /// before stop working and their locks are released. Returns what
    /// happened to each unsynced change.
    pub fn crash(&self, seed: u64) -> CrashReport {
        let mut disk = self.disk();
        let mut rng = Rng(seed);
        // Files in path order, so the choices do not depend on the order of
        // anything but the operations.
        let mut report = CrashReport::default();
        let files = std::mem::take(&mut disk.files);
        for (path, file) in files {
            if !file.entry_synced && rng.coin() {
                report.vanished.push(path);
                continue;
            }
            let mut data = file.synced;
            for change in file.waiting {
                apply_after_crash(&path, &mut data, change, &mut rng, &mut report);
            }
            disk.files.insert(
                path,
                File {
                    now: data.clone(),
                    synced: data,
                    entry_synced: true,
                    ..File::default()
                },
            );
        }
        disk.boot += 1;
        disk.down = false;
        disk.ops_left = None;
        report
    }

    /// Whether a file or directory is at `path`.
    pub fn exists(&self, path: &Path) -> bool {
        let disk = self.disk();
        disk.is_dir(path) || disk.files.contains_key(path)
    }

    fn disk(&self) -> MutexGuard<'_, Disk> {
        lock(&self.disk)
    }
}

fn lock(disk: &Mutex<Disk>) -> MutexGuard<'_, Disk> {
    // A panic inside a call leaves the disk as it was, or with one change
    // fully made: every change is a single step. So a poisoned lock still
    // guards a whole disk, and dropping a handle never panics.
    disk.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a crash does to one unsynced write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    Lost,
    Kept,
    /// Each 512-byte sector of the file the write touches lands or not:
    /// `sectors[i]` says whether the `i`-th sector it touches landed.
    Torn {
        sectors: Vec<bool>,
    },
}

impl Landing {
    /// One of the three, each a third of the time.
    fn roll(rng: &mut Rng) -> Landing {
        match rng.below(3) {
            0 => Landing::Lost,
            1 => Landing::Kept,
            _ => Landing::Torn {
                sectors: Vec::new(),
            },
        }
    }
}

/// What part of a change that was never synced lands on the disk, noted
/// in `report`.
fn apply_after_crash(
    path: &Path,
    data: &mut Vec<u8>,
    change: Change,
    rng: &mut Rng,
    report: &mut CrashReport,
) {
    match change {
        Change::SetLen(len) => {
            let landed = rng.coin();
            if landed {
                data.resize(len, 0);
            }
            report.lengths.push(LengthFate {
                path: path.to_path_buf(),
                len: len as u64,
                landed,
            });
        }
        Change::Write { offset, bytes } => {
            let mut landing = Landing::roll(rng);
            match landing {
                Landing::Lost => {}
                Landing::Kept => write_at(data, offset, &bytes),
                Landing::Torn { ref mut sectors } => {
                    // Sectors are counted from the start of the file. A
                    // torn write past the end leaves the file as long as
                    // the last sector that landed.
                    let end = offset + bytes.len() as u64;
                    let mut start = offset;
                    while start < end {
                        let stop = ((start / SECTOR + 1) * SECTOR).min(end);
                        let landed = rng.coin();
                        if landed {
                            let piece = (start - offset) as usize..(stop - offset) as usize;
                            write_at(data, start, &bytes[piece]);
                        }
                        sectors.push(landed);
                        start = stop;
                    }
                }
            }
            report.writes.push(WriteFate {
                path: path.to_path_buf(),
                offset,
                len: bytes.len(),
                landing,
            });
        }
    }
}

/// Writes `bytes` at `offset`, filling any gap with zeros. The range was
/// checked by [`range`] when the write was made.
fn write_at(data: &mut Vec<u8>, offset: u64, bytes: &[u8]) {
    let at = range(offset, bytes.len()).expect("checked when written");
    if data.len() < at.end {
        data.resize(at.end, 0);
    }
    data[at].copy_from_slice(bytes);
}

/// The bytes `offset..offset + len` as memory indexes, or the error a real
/// file gives for an offset no file can reach. A file in memory cannot be
/// sparse: a large offset takes that much memory.
fn range(offset: u64, len: usize) -> io::Result<Range<usize>> {
    usize::try_from(offset)
        .ok()
        .and_then(|start| Some(start..start.checked_add(len)?))
        .filter(|r| u64::try_from(r.end).is_ok_and(|end| end <= i64::MAX as u64))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset too large for a file"))
}

/// The parent directory of `path`, or `None` at the root.
fn parent(path: &Path) -> Option<&Path> {
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

fn is_root(path: &Path) -> bool {
    parent(path).is_none() && path.file_name().is_none()
}

impl Disk {
    /// Fails if the power is cut, and counts one operation that changes
    /// the disk.
    fn change(&mut self) -> io::Result<()> {
        if self.down {
            return Err(power_cut());
        }
        match self.ops_left {
            Some(0) => {
                self.down = true;
                Err(power_cut())
            }
            Some(ref mut left) => {
                *left -= 1;
                self.ops += 1;
                Ok(())
            }
            None => {
                self.ops += 1;
                Ok(())
            }
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        is_root(path) || self.dirs.contains(path)
    }
}

fn power_cut() -> io::Error {
    io::Error::other("the power is cut")
}

/// An open file on a [`SimFs`].
pub struct SimFile {
    disk: Arc<Mutex<Disk>>,
    path: PathBuf,
    /// The boot this handle was opened in.
    boot: u64,
    handle: u64,
}

impl Fs for SimFs {
    type File = SimFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut disk = self.disk();
        disk.change()?;
        let missing: Vec<&Path> = path.ancestors().take_while(|p| !disk.is_dir(p)).collect();
        if let Some(file) = missing.iter().find(|p| disk.files.contains_key(**p)) {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is a file", file.display()),
            ));
        }
        for dir in missing {
            disk.dirs.insert(dir.to_path_buf());
        }
        Ok(())
    }

    fn open(&self, path: &Path) -> io::Result<SimFile> {
        let mut disk = self.disk();
        disk.change()?;
        if disk.is_dir(path) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{} is a directory", path.display()),
            ));
        }
        if let Some(dir) = parent(path) {
            if disk.files.contains_key(dir) {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is a file", dir.display()),
                ));
            }
            if !disk.is_dir(dir) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no directory for {}", path.display()),
                ));
            }
        }
        disk.files.entry(path.to_path_buf()).or_default();
        disk.next_handle += 1;
        Ok(SimFile {
            disk: Arc::clone(&self.disk),
            path: path.to_path_buf(),
            boot: disk.boot,
            handle: disk.next_handle,
        })
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let mut disk = self.disk();
        disk.change()?;
        if !disk.is_dir(dir) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no directory {}", dir.display()),
            ));
        }
        for (path, file) in disk.files.iter_mut() {
            if parent(path) == Some(dir) {
                file.entry_synced = true;
            }
        }
        Ok(())
    }
}

impl SimFile {
    /// Runs `f` on this file, if the power is on and the handle is from
    /// this boot.
    fn with<T>(&self, access: Access, f: impl FnOnce(&mut File, u64) -> T) -> io::Result<T> {
        let mut disk = lock(&self.disk);
        if disk.boot != self.boot {
            return Err(io::Error::other("the file was opened before a crash"));
        }
        if access == Access::Change {
            disk.change()?;
        } else if disk.down {
            return Err(power_cut());
        }
        // The file exists: nothing removes one within a boot.
        let file = disk.files.get_mut(&self.path).expect("open file exists");
        Ok(f(file, self.handle))
    }
}

impl FsFile for SimFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.with(Access::Read, |file, _| {
            if buf.is_empty() {
                return Ok(());
            }
            let bytes = range(offset, buf.len())
                .ok()
                .and_then(|at| file.now.get(at))
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end"))?;
            buf.copy_from_slice(bytes);
            Ok(())
        })?
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        range(offset, buf.len())?;
        self.with(Access::Change, |file, _| {
            write_at(&mut file.now, offset, buf);
            file.waiting.push(Change::Write {
                offset,
                bytes: buf.to_vec(),
            });
        })
    }

    fn size(&self) -> io::Result<u64> {
        self.with(Access::Read, |file, _| file.now.len() as u64)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let len = range(len, 0)?.end;
        self.with(Access::Change, |file, _| {
            file.now.resize(len, 0);
            file.waiting.push(Change::SetLen(len));
        })
    }

    fn try_lock(&self) -> io::Result<()> {
        self.with(Access::Read, |file, handle| match file.locked_by {
            Some(holder) if holder != handle => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the file is locked",
            )),
            _ => {
                file.locked_by = Some(handle);
                Ok(())
            }
        })?
    }

    fn sync_data(&self) -> io::Result<()> {
        self.with(Access::Change, |file, _| {
            file.synced.clone_from(&file.now);
            file.waiting.clear();
        })
    }
}

impl Drop for SimFile {
    /// Releases the file's lock, as closing a real file does.
    fn drop(&mut self) {
        let mut disk = lock(&self.disk);
        if disk.boot != self.boot {
            return;
        }
        if let Some(file) = disk.files.get_mut(&self.path) {
            if file.locked_by == Some(self.handle) {
                file.locked_by = None;
            }
        }
    }
}

/// SplitMix64: a small random number generator written out here, so a seed
/// gives the same disk whatever crate versions are in use.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }

    /// A number in `0..n`. The tiny bias of `%` does not matter here.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_gives_its_published_values() {
        // The first outputs for seed 1234567, from the reference code.
        let mut rng = Rng(1234567);
        assert_eq!(rng.next(), 6457827717110365317);
        assert_eq!(rng.next(), 3203168211198807973);
    }
}
