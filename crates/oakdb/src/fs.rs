//! File access for the engine.
//!
//! Every read, write and sync the engine does goes through the [`Fs`] trait.
//! Production code uses [`StdFs`]; crash tests will plug in a simulated file
//! system that can drop writes that were never synced.
//!
//! Durability differs by platform, so it is handled here and nowhere else:
//! on macOS `fsync` only hands data to the drive, so we use `F_FULLFSYNC` to
//! flush the drive's cache; on Linux `fdatasync` is enough for file contents.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// A file system the engine can store data in.
pub trait Fs {
    type File: FsFile;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_data_reads_back_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");

        let file = StdFs.open(&path).unwrap();
        file.write_all_at(b"oak", 4096).unwrap();
        file.sync_data().unwrap();
        StdFs.sync_dir(dir.path()).unwrap();
        drop(file);

        let file = StdFs.open(&path).unwrap();
        let mut buf = [0u8; 3];
        file.read_exact_at(&mut buf, 4096).unwrap();
        assert_eq!(&buf, b"oak");
        assert_eq!(file.size().unwrap(), 4099);
    }

    #[test]
    fn opening_keeps_existing_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        StdFs.open(&path).unwrap().write_all_at(b"keep", 0).unwrap();

        let file = StdFs.open(&path).unwrap();
        let mut buf = [0u8; 4];
        file.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"keep");
    }

    #[test]
    fn set_len_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let file = StdFs.open(&dir.path().join("data")).unwrap();
        file.write_all_at(&[1; 100], 0).unwrap();
        file.set_len(10).unwrap();
        assert_eq!(file.size().unwrap(), 10);
    }
}
