//! A simulated file system in memory, for crash tests.
//!
//! [`SimFs`] keeps two copies of every file: what reads see, and what is on
//! the "disk", which is only what was synced. Until then each write and
//! length change waits in a list. [`SimFs::crash`] cuts the power and starts
//! again: each waiting change is kept, lost, or (for a write) torn so that
//! only some of its 512-byte sectors land. A write past the end of the
//! file on the disk may also land its new length alone, the grown part
//! reading as zeros. A file created since its directory was last synced may vanish,
//! and renames since then land in order up to some point.
//! A seed makes every choice, so the same seed and the same operations
//! always give the same disk.
//!
//! [`SimFs::fail_after`] cuts the power part way through the engine's work:
//! after a given number of operations that change the disk, every call
//! fails until the next crash. [`SimFs::fail_once_after`] makes one such
//! call fail with the power still on, as a disk that reports an error does.
//! What a failed sync does to the changes it was to save is
//! [`SimFs::set_failed_sync`]'s to say: they keep waiting, or, as Linux
//! does, reads still see them but the disk never gets them (issue #59).
//! [`SimFs::set_torn_sector`] says what a sector a torn write did not land
//! holds: the old bytes, or garbage where it was overwriting bytes the disk
//! held (design 0014).

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
    /// Operations that change the disk left before one fails with the
    /// power on.
    fail_once_left: Option<u64>,
    /// Directories. They are always durable: [`Fs::create_dir_all`] syncs
    /// them, and nothing else makes one.
    dirs: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, File>,
    /// Gives each handle its own number, for locks.
    next_handle: u64,
    /// What a failed sync does to the changes it was to save.
    failed_sync: FailedSync,
    /// What a sector a torn write did not land holds.
    torn_sector: TornSector,
    /// Renames since their directory was last synced, oldest first.
    renames: Vec<Rename>,
    /// Gives each file its own number, so handles follow it when renamed.
    next_inode: u64,
}

/// What a sector that a torn write did not land holds after the crash
/// (design 0014).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TornSector {
    /// The bytes the disk held before: a disk writes a sector whole or
    /// not at all.
    #[default]
    KeepsOld,
    /// Garbage where the write was overwriting bytes the disk held, as a
    /// disk that loses power part way through a sector may leave. Only the
    /// bytes the write covers: the rest of the sector stays as it was
    /// (what SQLite calls powersafe overwrite), and past the end of the
    /// file on the disk nothing lands, as before.
    Garbled,
}

/// A rename not yet made durable by syncing its directory.
struct Rename {
    from: PathBuf,
    to: PathBuf,
    /// The file `to` named before, if any: what it names again if the
    /// rename does not land.
    replaced: Option<File>,
}

/// What a sync that fails with the power on does to the changes it was to
/// save (issue #59, design 0014).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailedSync {
    /// They stay unsynced, for a crash to decide or a later sync to save.
    #[default]
    KeepsWaiting,
    /// They are forgotten, as Linux marks the pages it failed to write
    /// clean: reads still see them, but no later sync saves them, and a
    /// crash drops them. Only a later write of the same bytes saves them.
    Forgets,
}

/// Why an operation that changes the disk failed.
enum Fault {
    PowerCut,
    DiskFailed,
}

impl From<Fault> for io::Error {
    fn from(fault: Fault) -> io::Error {
        match fault {
            Fault::PowerCut => power_cut(),
            Fault::DiskFailed => disk_failed(),
        }
    }
}

#[derive(Default)]
struct File {
    /// Which file this is, whatever its path.
    inode: u64,
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
    /// Every unsynced rename, oldest first. They land in order: once one
    /// does not, none after it does.
    pub renames: Vec<RenameFate>,
}

/// One unsynced rename and whether it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameFate {
    pub from: PathBuf,
    pub to: PathBuf,
    pub landed: bool,
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
    /// A change that is a sync: if it fails, [`FailedSync`] says what
    /// becomes of the changes it was to save.
    Sync,
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

    /// Makes one operation that changes the disk fail with the power still
    /// on, after `ops` more of them succeed, counted as
    /// [`SimFs::fail_after`] counts them. The failing operation changes
    /// nothing and is not counted. A failed sync makes nothing durable: the
    /// changes it was to save stay unsynced, for a crash to decide or a
    /// later sync to save. The operations after it work again (issue #24).
    /// It fails with EIO, as a disk does. [`SimFs::set_failed_sync`] can
    /// make a failed sync forget the changes instead. If the power is cut
    /// at that operation or before, the cut wins and [`SimFs::crash`]
    /// drops the failure: it does not carry over to the next boot.
    pub fn fail_once_after(&self, ops: u64) {
        self.disk().fail_once_left = Some(ops);
    }

    /// Sets what a sync failed by [`SimFs::fail_once_after`] does to the
    /// changes it was to save. It holds across crashes, as it is the
    /// operating system's way. A failed directory sync leaves the entries
    /// unsynced either way.
    pub fn set_failed_sync(&self, failed_sync: FailedSync) {
        self.disk().failed_sync = failed_sync;
    }

    /// Sets what a sector that a torn write did not land holds after a
    /// crash. It holds across crashes, as it is the disk's way.
    pub fn set_torn_sector(&self, torn_sector: TornSector) {
        self.disk().torn_sector = torn_sector;
    }

    /// How many operations that change the disk have succeeded since the
    /// disk was made, counted as [`SimFs::fail_after`] counts them.
    pub fn ops(&self) -> u64 {
        self.disk().ops
    }

    /// Cuts the power now, decides with `seed` what of the unsynced work
    /// reached the disk, and starts again with the power on. Handles opened
    /// before stop working and their locks are released. A
    /// [`SimFs::fail_after`] or [`SimFs::fail_once_after`] still due is
    /// dropped. Returns what happened to each unsynced change.
    pub fn crash(&self, seed: u64) -> CrashReport {
        let mut disk = self.disk();
        let mut rng = Rng(seed);
        // Files in path order, so the choices do not depend on the order of
        // anything but the operations.
        let mut report = CrashReport::default();
        undo_renames(&mut disk, &mut rng, &mut report);
        let torn_sector = disk.torn_sector;
        let files = std::mem::take(&mut disk.files);
        for (path, file) in files {
            if !file.entry_synced && rng.coin() {
                report.vanished.push(path);
                continue;
            }
            let mut data = file.synced;
            for change in file.waiting {
                apply_after_crash(&path, &mut data, change, torn_sector, &mut rng, &mut report);
            }
            disk.files.insert(
                path,
                File {
                    inode: file.inode,
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
        disk.fail_once_left = None;
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

/// Decides how many of the unsynced renames landed, oldest first, as a
/// journaling file system commits directory changes in order, and puts
/// back the files of those that did not. Draws nothing if there are none,
/// so a seed gives the disk it gave before renames were added.
fn undo_renames(disk: &mut Disk, rng: &mut Rng, report: &mut CrashReport) {
    let renames = std::mem::take(&mut disk.renames);
    if renames.is_empty() {
        return;
    }
    let landed = rng.below(renames.len() + 1);
    for (i, rename) in renames.iter().enumerate() {
        report.renames.push(RenameFate {
            from: rename.from.clone(),
            to: rename.to.clone(),
            landed: i < landed,
        });
    }
    for (i, rename) in renames.into_iter().enumerate().rev() {
        let file = disk
            .files
            .remove(&rename.to)
            .expect("a renamed file stays until undone");
        if i < landed {
            // Its new name is on the disk, and with it the file.
            disk.files.insert(
                rename.to,
                File {
                    entry_synced: true,
                    ..file
                },
            );
            continue;
        }
        disk.files.insert(rename.from, file);
        if let Some(replaced) = rename.replaced {
            disk.files.insert(rename.to, replaced);
        }
    }
}

fn lock(disk: &Mutex<Disk>) -> MutexGuard<'_, Disk> {
    // A panic inside a call leaves the disk as it was, or with one change
    // fully made: every change is a single step. So a poisoned lock still
    // guards a whole disk, and dropping a handle never panics.
    disk.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a crash does to one unsynced write. More ways may be added as
/// the simulated disk learns new faults.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Landing {
    Lost,
    Kept,
    /// Each 512-byte sector of the file the write touches lands or not:
    /// `sectors[i]` says whether the `i`-th sector it touches landed.
    Torn {
        sectors: Vec<bool>,
    },
    /// The file's new length landed but the bytes did not, as on a file
    /// system that can write the length before the contents. Only a write
    /// past the end of the file on the disk, as the changes before it left
    /// it, lands this way: its bytes from `zeros_from` (that end, or its
    /// offset if later) to its own end read as zeros, and the bytes before
    /// stay as the disk has them. So zeros never cover bytes the disk
    /// still holds: a file system grows a file only after every length
    /// change it journaled before.
    LengthOnly {
        zeros_from: u64,
    },
}

impl Landing {
    /// Kept, lost or torn, each a third of the time. A write past the end
    /// of the file on the disk, whose part from `zeros_from` on is new,
    /// lands only its length in half the cases it would be lost. That takes no more draws
    /// than before, so a seed gives the disk it gave before this landing
    /// was added unless a write past the end was lost. The tiny bias of
    /// `%` does not matter here.
    fn roll(rng: &mut Rng, zeros_from: Option<u64>) -> Landing {
        let draw = rng.next();
        match draw % 3 {
            0 => match zeros_from {
                // `% 3` used the low end of the draw; the top bit is a
                // second coin from the same draw, so no draw is added.
                Some(zeros_from) if draw >> 63 == 1 => Landing::LengthOnly { zeros_from },
                _ => Landing::Lost,
            },
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
    torn_sector: TornSector,
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
            let end = offset + bytes.len() as u64;
            let disk_len = data.len() as u64;
            let zeros_from = (end > disk_len).then(|| offset.max(disk_len));
            let mut landing = Landing::roll(rng, zeros_from);
            match landing {
                Landing::Lost => {}
                Landing::Kept => write_at(data, offset, &bytes),
                Landing::LengthOnly { .. } => {
                    // Everything past the disk's end is new, so growing
                    // the file with zeros is all there is to it.
                    let grown = range(offset, bytes.len()).expect("checked when written");
                    data.resize(grown.end, 0);
                }
                Landing::Torn { ref mut sectors } => {
                    // Sectors are counted from the start of the file. A
                    // torn write past the end leaves the file as long as
                    // the end of the last piece that landed, never past
                    // the end of the write.
                    let mut start = offset;
                    while start < end {
                        let stop = ((start / SECTOR + 1) * SECTOR).min(end);
                        let landed = rng.coin();
                        if landed {
                            let piece = (start - offset) as usize..(stop - offset) as usize;
                            write_at(data, start, &bytes[piece]);
                        } else if torn_sector == TornSector::Garbled {
                            garble(data, start..stop);
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

/// Garbles the bytes of `at` the disk holds: each is flipped, so it is
/// neither what was there nor the same bytes written again. Takes no
/// draws, so a seed lands the same sectors either way.
fn garble(data: &mut [u8], at: Range<u64>) {
    // The range was checked by [`range`] when the write was made.
    let start = usize::try_from(at.start).expect("checked when written");
    let end = usize::try_from(at.end).expect("checked when written");
    for byte in data.iter_mut().take(end).skip(start) {
        *byte = !*byte;
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
    /// Fails if the power is cut or [`SimFs::fail_once_after`] is due, and
    /// counts one operation that changes the disk.
    fn change(&mut self) -> Result<(), Fault> {
        if self.down {
            return Err(Fault::PowerCut);
        }
        // A power cut due at this operation comes before a failure due at
        // it, which the crash then drops with the rest.
        if self.ops_left == Some(0) {
            self.down = true;
            return Err(Fault::PowerCut);
        }
        if self.fail_once_left == Some(0) {
            self.fail_once_left = None;
            return Err(Fault::DiskFailed);
        }
        if let Some(left) = &mut self.ops_left {
            *left -= 1;
        }
        if let Some(left) = &mut self.fail_once_left {
            *left -= 1;
        }
        self.ops += 1;
        Ok(())
    }

    fn is_dir(&self, path: &Path) -> bool {
        is_root(path) || self.dirs.contains(path)
    }

    /// What a sync of the file numbered `inode` that failed with `fault`
    /// does to the changes it was to save: [`FailedSync`] says. A power
    /// cut leaves them for the crash to decide.
    fn after_failed_sync(&mut self, inode: u64, fault: &Fault) {
        if matches!(fault, Fault::DiskFailed) && self.failed_sync == FailedSync::Forgets {
            if let Some(file) = self.file_mut(inode) {
                file.waiting.clear();
            }
        }
    }

    /// The file numbered `inode`, under whatever name it has now.
    fn file_mut(&mut self, inode: u64) -> Option<&mut File> {
        self.files.values_mut().find(|file| file.inode == inode)
    }
}

fn power_cut() -> io::Error {
    io::Error::other("the power is cut")
}

/// What [`SimFs::fail_once_after`] fails with: EIO, what a disk reports
/// for a write or sync it could not do.
fn disk_failed() -> io::Error {
    io::Error::from_raw_os_error(libc::EIO)
}

/// An open file on a [`SimFs`].
pub struct SimFile {
    disk: Arc<Mutex<Disk>>,
    /// The file, followed through renames.
    inode: u64,
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
        if !disk.files.contains_key(path) {
            disk.next_inode += 1;
            let inode = disk.next_inode;
            disk.files.insert(
                path.to_path_buf(),
                File {
                    inode,
                    ..File::default()
                },
            );
        }
        let inode = disk.files[path].inode;
        disk.next_handle += 1;
        Ok(SimFile {
            disk: Arc::clone(&self.disk),
            inode,
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
        // Renames land in order, so those in `dir` are only durable once
        // every rename before them is: syncing makes all up to the last
        // one in `dir` durable.
        if let Some(last) = disk
            .renames
            .iter()
            .rposition(|r| parent(&r.to) == Some(dir))
        {
            disk.renames.drain(..=last);
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut disk = self.disk();
        if parent(from) != parent(to) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the simulated disk renames only within a directory",
            ));
        }
        if !disk.files.contains_key(from) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no file {}", from.display()),
            ));
        }
        disk.change()?;
        if from == to {
            return Ok(());
        }
        let file = disk.files.remove(from).expect("checked above");
        let replaced = disk.files.insert(to.to_path_buf(), file);
        disk.renames.push(Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            replaced,
        });
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
        if access == Access::Read {
            if disk.down {
                return Err(power_cut());
            }
        } else if let Err(fault) = disk.change() {
            if access == Access::Sync {
                disk.after_failed_sync(self.inode, &fault);
            }
            return Err(fault.into());
        }
        let file = disk.file_mut(self.inode).ok_or_else(|| {
            io::Error::other(
                "the file was replaced by a rename; the simulated disk does not keep it",
            )
        })?;
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
        // The waiting changes land on what the disk holds, not what reads
        // see: the two differ once a failed sync forgot changes.
        self.with(Access::Sync, |file, _| {
            for change in std::mem::take(&mut file.waiting) {
                match change {
                    Change::Write { offset, bytes } => write_at(&mut file.synced, offset, &bytes),
                    Change::SetLen(len) => file.synced.resize(len, 0),
                }
            }
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
        let handle = self.handle;
        if let Some(file) = disk.file_mut(self.inode) {
            if file.locked_by == Some(handle) {
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

    /// A number below `n`. The tiny bias of `%` does not matter here.
    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("a count fits in a u64");
        usize::try_from(self.next() % n).expect("below a usize")
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
