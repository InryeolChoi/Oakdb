//! File access for the engine.
//!
//! Every read, write and sync the engine does goes through the [`Fs`] trait.
//! Production code uses [`StdFs`]; crash tests plug in [`sim::SimFs`], a
//! file system in memory that can lose writes that were never synced.
//!
//! Durability differs by platform, so it is handled here and nowhere else:
//! on macOS `fsync` only hands data to the drive, so we use `F_FULLFSYNC` to
//! flush the drive's cache; on Linux `fdatasync` is enough for file contents.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub mod sim;

/// A file system the engine can store data in.
pub trait Fs {
    type File: FsFile;

    /// Creates `path` and any missing parent directories, and makes every
    /// directory it created durable. Succeeds if `path` already exists.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Opens a file for reading and writing, creating it if it does not exist.
    fn open(&self, path: &Path) -> io::Result<Self::File>;

    /// Makes the directory entry for files created or renamed in `dir` durable.
    ///
    /// A newly created file can vanish after a crash even if its contents were
    /// synced, unless the directory that holds it is synced too.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// An open file. Reads and writes use explicit offsets so the engine never
/// depends on a shared cursor.
pub trait FsFile {
    /// Reads exactly `buf.len()` bytes starting at `offset`.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Writes all of `buf` starting at `offset`. The data is not durable until
    /// [`FsFile::sync_data`] returns successfully.
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Returns the current file length in bytes.
    fn size(&self) -> io::Result<u64>;

    /// Grows or shrinks the file to `len` bytes.
    fn set_len(&self, len: u64) -> io::Result<()>;

    /// Takes an exclusive lock on the file without waiting, held until the
    /// file is closed. Fails with [`io::ErrorKind::WouldBlock`] if another
    /// open handle, in this process or another, already holds it.
    fn try_lock(&self) -> io::Result<()>;

    /// Makes every write so far durable on the storage device.
    ///
    /// If this fails, the caller must not retry and treat the data as saved:
    /// the kernel may already have dropped the unsaved pages.
    fn sync_data(&self) -> io::Result<()>;
}

/// The real file system of the operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

impl Fs for StdFs {
    type File = StdFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        if path.is_dir() {
            return Ok(());
        }
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        self.create_dir_all(parent)?;
        match std::fs::create_dir(path) {
            Ok(()) => {}
            // Someone else created it between our check and now.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(e),
        }
        self.sync_dir(parent)
    }

    fn open(&self, path: &Path) -> io::Result<StdFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(StdFile(file))
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let dir = File::open(dir)?;
        full_sync(&dir)
    }
}

/// A file on the real file system.
#[derive(Debug)]
pub struct StdFile(File);

impl FsFile for StdFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(&self.0, buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.0, buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn try_lock(&self) -> io::Result<()> {
        // `flock` on both platforms: the lock belongs to this open file, so
        // a second open of the same path in the same process is refused too.
        self.0.try_lock().map_err(io::Error::from)
    }

    fn sync_data(&self) -> io::Result<()> {
        data_sync(&self.0)
    }
}

#[cfg(target_os = "macos")]
fn full_sync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is valid for as long as `file` is borrowed.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn data_sync(file: &File) -> io::Result<()> {
    full_sync(file)
}

#[cfg(target_os = "linux")]
fn full_sync(file: &File) -> io::Result<()> {
    file.sync_all()
}

#[cfg(target_os = "linux")]
fn data_sync(file: &File) -> io::Result<()> {
    file.sync_data()
}
