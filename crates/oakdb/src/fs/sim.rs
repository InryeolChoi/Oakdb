//! A simulated file system in memory, for crash tests.
//!
//! Not implemented yet: the tests in `tests/fs_contract.rs` and
//! `tests/sim_fs.rs` describe what it must do.

use std::io;
use std::path::Path;

use super::{Fs, FsFile};

/// A file system that lives in memory and can lose power.
#[derive(Clone, Default)]
pub struct SimFs {}

impl SimFs {
    /// An empty disk with the power on.
    pub fn new() -> Self {
        SimFs {}
    }

    /// Cuts the power after `ops` more operations that change the disk.
    pub fn fail_after(&self, _ops: u64) {
        todo!()
    }

    /// Cuts the power now and starts again.
    pub fn crash(&self, _seed: u64) {
        todo!()
    }

    /// Whether a file or directory is at `path`.
    pub fn exists(&self, _path: &Path) -> bool {
        todo!()
    }
}

/// An open file on a [`SimFs`].
pub struct SimFile {}

impl Fs for SimFs {
    type File = SimFile;

    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        todo!()
    }

    fn open(&self, _path: &Path) -> io::Result<SimFile> {
        todo!()
    }

    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        todo!()
    }
}

impl FsFile for SimFile {
    fn read_exact_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<()> {
        todo!()
    }

    fn write_all_at(&self, _buf: &[u8], _offset: u64) -> io::Result<()> {
        todo!()
    }

    fn size(&self) -> io::Result<u64> {
        todo!()
    }

    fn set_len(&self, _len: u64) -> io::Result<()> {
        todo!()
    }

    fn try_lock(&self) -> io::Result<()> {
        todo!()
    }

    fn sync_data(&self) -> io::Result<()> {
        todo!()
    }
}

impl Drop for SimFile {
    /// Releases the file's lock, as closing a real file does.
    fn drop(&mut self) {}
}
