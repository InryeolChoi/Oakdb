//! The engine API: open a database, run transactions, close it.
//!
//! The database is one data file of B+Tree pages, at most two levels deep
//! for now:
//!
//! ```text
//! page 0   meta slot A: root page and page count
//! page 1   meta slot B (same content as A for now)
//! page 2.. tree pages; new pages go at the end
//! ```
//!
//! A commit writes the pages it changed straight into the data file and
//! syncs them. If it added pages, it then writes the meta page and syncs
//! again. The WAL (ADR 0007) will replace that later.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result, MAX_ENTRY_LEN, MAX_KEY_LEN};
use crate::fs::{Fs, FsFile, StdFs};
use crate::meta::{Meta, META_LEN};
use crate::page::{Page, PAGE_SIZE};
use crate::tree::Tree;

/// Name of the data file inside the database directory.
pub const DATA_FILE: &str = "oakdb.data";

/// An open database.
pub struct Db<F: Fs = StdFs> {
    file: F::File,
    /// The committed tree. Commits hold the lock while they write, so they
    /// happen one at a time.
    tree: Mutex<Tree>,
}

impl<F: Fs> std::fmt::Debug for Db<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
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
        let mut tree = Tree::new();
        for page_no in tree.take_dirty() {
            file.write_all_at(tree.page(page_no).as_bytes(), page_offset(page_no))?;
        }
        write_meta(&file, tree.meta())?;
        file.sync_data()?;
        fs.sync_dir(dir)?;
        Ok(Db {
            file,
            tree: Mutex::new(tree),
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
        let tree = Tree::load(meta, |page_no| {
            let mut buf = vec![0; PAGE_SIZE].into_boxed_slice();
            file.read_exact_at(&mut buf, page_offset(page_no))?;
            Page::from_bytes(buf)
        })?;
        Ok(Db {
            file,
            tree: Mutex::new(tree),
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

    fn tree(&self) -> MutexGuard<'_, Tree> {
        // A panic while holding the lock cannot leave the tree half changed:
        // commits change a copy and swap it in at the end.
        self.tree.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A transaction. Its writes stay private until [`Txn::commit`]; dropping it
/// without committing throws them away.
pub struct Txn<'db, F: Fs = StdFs> {
    db: &'db Db<F>,
    /// The last write to each key: `Some(value)` for a put, `None` for a
    /// delete.
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<F: Fs> Txn<'_, F> {
    /// The value under `key`, including this transaction's own writes.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        if let Some(write) = self.writes.get(key) {
            return Ok(write.clone());
        }
        Ok(self.db.tree().get(key).map(<[u8]>::to_vec))
    }

    /// Stores `value` under `key` when the transaction commits.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        check_key(key)?;
        if key.len() + value.len() > MAX_ENTRY_LEN {
            return Err(Error::ValueTooLarge {
                len: value.len(),
                max: MAX_ENTRY_LEN - key.len(),
            });
        }
        self.writes.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    /// Removes `key` and its value when the transaction commits. Deleting a
    /// key that does not exist is not an error.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        check_key(key)?;
        self.writes.insert(key.to_vec(), None);
        Ok(())
    }

    /// Makes the writes durable and visible.
    ///
    /// Leaves that fill up split in two. A leaf only splits if what the
    /// commit leaves in it does not fit, whatever order the keys sort in.
    /// If the root has no room for another leaf ([`Error::PageFull`]), none
    /// of the writes take effect, deletes included. If writing or syncing
    /// the file fails ([`Error::Io`]), the writes are not visible now but
    /// may already be on disk and reappear after reopening: the outcome is
    /// unknown. Stopping the engine after such a failure comes with the WAL.
    pub fn commit(self) -> Result<()> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let mut tree = self.db.tree();
        let mut next = tree.clone();
        // Take out every key this transaction writes before putting any
        // back, so space freed by a delete or a shrinking value is there for
        // a put that sorts before it.
        for key in self.writes.keys() {
            next.delete(key);
        }
        for (key, value) in &self.writes {
            if let Some(value) = value {
                next.put(key, value)?;
            }
        }
        let file = &self.db.file;
        for page_no in next.take_dirty() {
            file.write_all_at(next.page(page_no).as_bytes(), page_offset(page_no))?;
        }
        file.sync_data()?;
        // The meta page goes last, so it only points at pages already on
        // disk. A crash before it still loses the keys a split moved off
        // an overwritten leaf, until the WAL comes.
        if next.meta() != tree.meta() {
            write_meta(file, next.meta())?;
            file.sync_data()?;
        }
        *tree = next;
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

/// Writes `meta` to both meta slots.
fn write_meta(file: &impl FsFile, meta: Meta) -> Result<()> {
    file.write_all_at(&meta.encode(), 0)?;
    file.write_all_at(&meta.encode(), PAGE_SIZE as u64)?;
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
