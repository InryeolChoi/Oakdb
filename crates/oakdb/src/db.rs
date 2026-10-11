//! The engine API: open a database, run transactions, close it.
//!
//! For now the whole database is one leaf page in one data file:
//!
//! ```text
//! page 0  meta slot A
//! page 1  meta slot B (same content as A for now)
//! page 2  root leaf
//! ```
//!
//! A commit writes the changed leaf straight into the data file and syncs
//! it. The WAL (ADR 0007) will replace that later.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result, MAX_KEY_LEN};
use crate::fs::{Fs, FsFile, StdFs};
use crate::meta::{Meta, META_LEN};
use crate::page::{LeafPage, PAGE_SIZE};

/// Name of the data file inside the database directory.
const DATA_FILE: &str = "oakdb.data";

/// An open database.
pub struct Db<F: Fs = StdFs> {
    file: F::File,
    meta: Meta,
    /// The committed root leaf. Commits hold the lock while they write, so
    /// they happen one at a time.
    leaf: Mutex<LeafPage>,
}

impl<F: Fs> std::fmt::Debug for Db<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

impl Db<StdFs> {
    /// Opens the database in directory `path` on the real file system,
    /// creating the directory and an empty database if they do not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Db::open_with(StdFs, path)
    }
}

impl<F: Fs> Db<F> {
    /// Opens the database in directory `path`, doing all file access through
    /// `fs`.
    pub fn open_with(fs: F, path: impl AsRef<Path>) -> Result<Self> {
        let dir = path.as_ref();
        fs.create_dir_all(dir)?;
        let file = fs.open(&dir.join(DATA_FILE))?;
        file.try_lock().map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock => Error::AlreadyOpen,
            _ => Error::Io(e),
        })?;
        if file.size()? == 0 {
            Db::create(&fs, dir, file)
        } else {
            Db::load(file)
        }
    }

    fn create(fs: &F, dir: &Path, file: F::File) -> Result<Self> {
        let meta = Meta {
            root_page: 2,
            page_count: 3,
        };
        let leaf = LeafPage::empty();
        file.write_all_at(&meta.encode(), 0)?;
        file.write_all_at(&meta.encode(), PAGE_SIZE as u64)?;
        file.write_all_at(leaf.as_bytes(), page_offset(meta.root_page))?;
        file.sync_data()?;
        fs.sync_dir(dir)?;
        Ok(Db {
            file,
            meta,
            leaf: Mutex::new(leaf),
        })
    }

    fn load(file: F::File) -> Result<Self> {
        let size = file.size()?;
        if size < META_LEN as u64 {
            return Err(Error::Corrupt("data file is too short".into()));
        }
        let mut header = [0; META_LEN];
        file.read_exact_at(&mut header, 0)?;
        let meta = Meta::decode(&header)?;
        if size < meta.page_count.saturating_mul(PAGE_SIZE as u64) {
            return Err(Error::Corrupt(
                "data file is shorter than its meta page says".into(),
            ));
        }
        let mut buf = vec![0; PAGE_SIZE].into_boxed_slice();
        file.read_exact_at(&mut buf, page_offset(meta.root_page))?;
        let leaf = LeafPage::from_bytes(buf)?;
        Ok(Db {
            file,
            meta,
            leaf: Mutex::new(leaf),
        })
    }

    /// Starts a transaction.
    pub fn begin(&self) -> Txn<'_, F> {
        Txn {
            db: self,
            writes: BTreeMap::new(),
        }
    }

    /// Closes the database. Every committed transaction is already durable,
    /// so this only releases the file and its lock.
    pub fn close(self) -> Result<()> {
        Ok(())
    }

    fn leaf(&self) -> MutexGuard<'_, LeafPage> {
        // A panic while holding the lock cannot leave the leaf half changed:
        // commits change a copy and swap it in at the end.
        self.leaf.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A transaction. Its writes stay private until [`Txn::commit`]; dropping it
/// without committing throws them away.
pub struct Txn<'db, F: Fs = StdFs> {
    db: &'db Db<F>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl<F: Fs> Txn<'_, F> {
    /// The value under `key`, including this transaction's own writes.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        if let Some(value) = self.writes.get(key) {
            return Ok(Some(value.clone()));
        }
        Ok(self.db.leaf().get(key).map(<[u8]>::to_vec))
    }

    /// Stores `value` under `key` when the transaction commits.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        check_key(key)?;
        self.writes.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    /// Makes the writes durable and visible.
    ///
    /// If the writes do not fit ([`Error::PageFull`]), none of them take
    /// effect. If writing or syncing the file fails ([`Error::Io`]), they are
    /// not visible now but may already be on disk and reappear after
    /// reopening: the outcome is unknown. Stopping the engine after such a
    /// failure comes with the WAL.
    pub fn commit(self) -> Result<()> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let mut leaf = self.db.leaf();
        let mut next = leaf.clone();
        for (key, value) in &self.writes {
            next.put(key, value)?;
        }
        let file = &self.db.file;
        file.write_all_at(next.as_bytes(), page_offset(self.db.meta.root_page))?;
        file.sync_data()?;
        *leaf = next;
        Ok(())
    }

    /// Throws away the writes.
    pub fn rollback(self) {}
}

fn check_key(key: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY_LEN {
        return Err(Error::KeyTooLarge {
            len: key.len(),
            max: MAX_KEY_LEN,
        });
    }
    Ok(())
}

fn page_offset(page_no: u64) -> u64 {
    page_no * PAGE_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::encode_for_page_size;

    #[test]
    fn a_data_file_with_another_page_size_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let meta = Meta {
            root_page: 2,
            page_count: 3,
        };
        let mut bytes = encode_for_page_size(&meta, 8192);
        bytes.resize(3 * 8192, 0);
        std::fs::write(dir.path().join(DATA_FILE), bytes).unwrap();

        let err = Db::open(dir.path()).unwrap_err();

        assert!(matches!(
            err,
            Error::PageSizeMismatch {
                expected: 4096,
                found: 8192
            }
        ));
    }

    #[test]
    fn a_new_database_records_the_page_size_in_both_meta_slots() {
        let dir = tempfile::tempdir().unwrap();
        Db::open(dir.path()).unwrap().close().unwrap();

        let bytes = std::fs::read(dir.path().join(DATA_FILE)).unwrap();

        assert_eq!(bytes.len(), 3 * PAGE_SIZE);
        assert_eq!(&bytes[16..20], &4096u32.to_le_bytes());
        assert_eq!(
            &bytes[PAGE_SIZE + 16..PAGE_SIZE + 20],
            &4096u32.to_le_bytes()
        );
    }
}
