//! The engine API: open a database, run transactions, close it.
//!
//! The database is one data file of B+Tree pages:
//!
//! ```text
//! page 0   meta slot A: root page, page count, free list and checkpoint
//! page 1   meta slot B: the same, as of the checkpoint before or after A's
//! page 2.. tree pages, overflow pages for long values, and free pages;
//!          new pages reuse free ones before going at the end
//! ```
//!
//! Every page goes to the file with its checksum filled in, and every page
//! read back is checked against it (`checksum.rs`).
//!
//! A commit appends an image of every page it changed and a commit record
//! with the new meta fields to the WAL (`wal.rs`, ADR 0007) and syncs it
//! once before it answers. Creating a database is its first commit. Only
//! checkpoints (`checkpoint.rs`, design 0011) write the data file: they
//! move the pages the WAL holds there, write the newer meta slot, and
//! start the WAL again. Opening takes the newest meta slot that checks
//! out, replays the WAL over the data file if the WAL follows that slot's
//! checkpoint, and cuts off whatever follows the last whole commit so the
//! next one is appended right there.
//!
//! If a write or sync fails once the database is open, the engine stops
//! (ADR 0007, design 0012): it does not retry, the commit that hit the
//! failure gets [`Error::OutcomeUnknown`], and every request after it gets
//! [`Error::Stopped`]. Opening the database again recovers it.

use std::collections::{btree_map, BTreeMap, BTreeSet};
use std::iter::{FusedIterator, Peekable};
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use crate::checkpoint;
use crate::checksum;
use crate::error::{Error, Result, MAX_KEY_LEN, MAX_VALUE_LEN};
use crate::fs::{Fs, FsFile, StdFs};
use crate::meta::{Meta, Slot, META_LEN};
use crate::page::{Page, Value, PAGE_SIZE};
use crate::tree::{Cursor, Tree};
use crate::wal::{self, Replayed, Tail, Wal, CUT_FILE, HEADER_LEN, WAL_FILE};

/// Name of the data file inside the database directory.
pub const DATA_FILE: &str = "oakdb.data";

/// A commit that leaves the WAL longer than this many bytes checkpoints,
/// which bounds how much opening replays. A fixed value for now; it may
/// become a setting of the database.
pub const CHECKPOINT_AFTER: u64 = 4 * 1024 * 1024;

/// An open database.
pub struct Db<F: Fs = StdFs> {
    /// The data file, held open for its lock. Checkpoints write it.
    file: F::File,
    /// Where commits go. Only taken while holding `tree` for now; a lock of
    /// its own so the commit thread (ADR 0006, #27) can later write the
    /// WAL while readers take the tree.
    wal: Mutex<Wal<F::File>>,
    /// The committed tree. Commits hold the lock while they write, so they
    /// happen one at a time, and swap in a new tree when done. A scan keeps
    /// the tree it started on, so commits never change it under the scan.
    tree: Mutex<Arc<Tree>>,
    /// Why the engine stopped, once a write or sync failed. Set at most
    /// once, while holding `tree`, and never cleared: this process does not
    /// trust the files again (design 0012).
    stopped: OnceLock<Arc<Error>>,
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
            _ => Error::from(e),
        })?;
        let bytes = read_all(&fs.open(&dir.join(WAL_FILE))?)?;
        let replayed = wal::replay(&bytes)?;
        // Everything is checked before anything is changed, so a database
        // that is refused is left as it was.
        let size = file.size()?;
        let slot = match size {
            0 => Ok(None),
            _ => newest_slot(&file, size).map(Some),
        };
        let (base, use_wal) = choose_base(slot, replayed.checkpoint, replayed.meta.is_some())?;
        let checkpoint = base.map_or(0, |slot| slot.checkpoint);
        if let Some(slot) = base {
            if u128::from(size) < pages_len(slot.meta.page_count) {
                return Err(Error::Corrupt(
                    "data file is shorter than its meta page says".into(),
                ));
            }
        }
        let Replayed {
            checkpoint: wal_checkpoint,
            pages,
            meta,
            tail,
        } = replayed;
        let (pages, meta) = match use_wal {
            UseWal::Replay => (pages, meta.or(base.map(|slot| slot.meta))),
            UseWal::Restart => (BTreeMap::new(), base.map(|slot| slot.meta)),
        };
        let in_wal: BTreeSet<u64> = pages.keys().copied().collect();
        let data_len = if base.is_some() { size } else { 0 };
        let tree = match meta {
            Some(meta) => Some(load(&file, data_len, meta, pages)?),
            // Nothing was ever committed. A cut while creating leaves at
            // most a header and the first commit's bytes; more than that
            // is a WAL whose commits were damaged, not a new database.
            None if bytes.len() as u64 > HEADER_LEN + wal::commit_len(1) => {
                return Err(Error::Corrupt(
                    "WAL holds no whole commit but is longer than a new database".into(),
                ))
            }
            None => None,
        };
        // Everything above was read, perhaps from the page cache. In the
        // same boot as a sync that failed, that can hold bytes the disk
        // never got, and no later sync saves them unless they are written
        // again. So what opening builds on is made durable before anything
        // is appended to it, without writing over the only copy of
        // anything a power cut could leave it needing (issue #59, design
        // 0014).
        //
        // The meta slot first, and only if the WAL follows an older
        // checkpoint: a checkpoint's slot may then be in the page cache
        // alone, as its sync failed, and the new WAL header below must not
        // reach the disk before it. It is written where it was read; if a
        // power cut garbles it there, the other slot and the WAL still on
        // the disk, which follows it, open as before. A WAL that follows
        // this very checkpoint was started only once its slot was synced.
        if let Some(slot) = base {
            if wal_checkpoint.is_some_and(|after| after < slot.checkpoint) {
                checkpoint::write_slot(&file, slot)?;
            }
        }
        let header = wal::encode_header(checkpoint).0;
        let (kept, tail, in_wal) = match use_wal {
            UseWal::Replay => {
                let end = usize::try_from(tail.end).expect("the tail is inside what was read");
                // What follows the last whole commit was never
                // acknowledged, or follows a damaged record. Keep it in the
                // cut file; the new WAL ends at the last whole commit, so
                // the next one goes right after it.
                let (whole, cut) = bytes.split_at(end);
                if !cut.is_empty() {
                    let kept = fs.open(&dir.join(CUT_FILE))?;
                    kept.write_all_at(cut, kept.size()?)?;
                    kept.sync_data()?;
                    fs.sync_dir(dir)?;
                }
                (whole, tail, in_wal)
            }
            UseWal::Restart => (&header[..], Tail::after_header(checkpoint), BTreeSet::new()),
        };
        // This also syncs the directory, so the data file, if this open
        // made it, is there to stay.
        let wal_file = wal::write_new(&fs, dir, kept)?;
        let mut wal = Wal::new(wal_file, tail, checkpoint, in_wal);
        let tree = match tree {
            Some(tree) => tree,
            // A new database, whose creation is the first commit.
            None => {
                let mut tree = Tree::new();
                append(&mut wal, &mut tree)?;
                tree
            }
        };
        Ok(Db {
            file,
            wal: Mutex::new(wal),
            tree: Mutex::new(Arc::new(tree)),
            stopped: OnceLock::new(),
        })
    }

    /// Starts a transaction.
    pub fn begin(&self) -> Txn<'_, F> {
        Txn {
            db: self,
            writes: BTreeMap::new(),
        }
    }

    /// How many pages the database uses, free ones included, counting the
    /// two meta pages: what its data file holds once checkpointed.
    pub fn page_count(&self) -> u64 {
        self.tree().meta().page_count
    }

    /// Moves the pages the WAL holds to the data file and empties the WAL,
    /// so opening has less to replay. Commits do this on their own once the
    /// WAL is longer than [`CHECKPOINT_AFTER`]. Does nothing if the WAL
    /// holds no commit.
    ///
    /// If it fails, it returns what failed, usually [`Error::Io`], and the
    /// engine stops: every request after it fails with [`Error::Stopped`]
    /// until the database is dropped and opened again. Every acknowledged
    /// commit is still there then. It is not [`Error::OutcomeUnknown`]: a
    /// checkpoint changes nothing the database holds, so no commit's
    /// outcome is in doubt, wherever it stopped.
    pub fn checkpoint(&self) -> Result<()> {
        let tree = self.tree();
        // Checked holding the lock, so a commit that failed while this one
        // waited for it is seen, and nothing is written after a failure.
        self.check_running()?;
        checkpoint::run(&self.file, &mut self.wal(), &tree).inspect_err(|e| self.stop(e))
    }

    /// Checkpoints and closes the database. Every committed transaction is
    /// already durable, so dropping it without closing loses nothing; the
    /// next open replays the WAL instead. Fails as [`Db::checkpoint`] does,
    /// and every commit acknowledged before is kept all the same.
    pub fn close(self) -> Result<()> {
        self.checkpoint()
    }

    /// Fails with [`Error::Stopped`] if the engine stopped.
    fn check_running(&self) -> Result<()> {
        match self.stopped.get() {
            Some(cause) => Err(Error::Stopped(Arc::clone(cause))),
            None => Ok(()),
        }
    }

    /// Stops the engine because of `cause`, the error the failing call
    /// returns, which every request after it reports. Called holding
    /// `tree`, so the first failure is the one kept.
    fn stop(&self, cause: &Error) {
        let _ = self.stopped.set(Arc::new(cause.clone()));
    }

    fn wal(&self) -> MutexGuard<'_, Wal<F::File>> {
        self.wal.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tree(&self) -> MutexGuard<'_, Arc<Tree>> {
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
        self.db.check_running()?;
        check_key(key)?;
        if let Some(write) = self.writes.get(key) {
            return Ok(write.clone());
        }
        let (tree, r) = {
            let locked = self.db.tree();
            match locked.find(key) {
                None => return Ok(None),
                Some(Value::Inline(value)) => return Ok(Some(value.to_vec())),
                Some(Value::Overflow(r)) => (Arc::clone(&locked), r),
            }
            // The lock is released here, at the end of the block that holds
            // it, so commits do not wait while a long value is gathered
            // from its overflow pages below.
        };
        Ok(Some(tree.value(Value::Overflow(r)).into_owned()))
    }

    /// The entries whose keys are in `range`, in key order, including this
    /// transaction's own writes. Each end of the range can include or
    /// exclude its key, or be left open: `txn.scan("a".."m")`,
    /// `txn.scan::<&[u8]>((Bound::Excluded(from), Bound::Included(to)))`,
    /// or `txn.scan::<&[u8]>(..)` for every entry. The key type has to be
    /// named where the range alone does not say it. A range whose start is
    /// past its end is empty.
    ///
    /// The scan reads the tree committed when it starts; commits made while
    /// it runs do not change what it yields. If the engine stopped, its
    /// only item is [`Error::Stopped`].
    pub fn scan<K: AsRef<[u8]>>(&self, range: impl RangeBounds<K>) -> Scan<'_> {
        let tree = Arc::clone(&self.db.tree());
        if let Err(e) = self.db.check_running() {
            return Scan::stopped(tree, e);
        }
        let (start, end) = empty_if_backwards(
            range.start_bound().map(AsRef::as_ref),
            range.end_bound().map(AsRef::as_ref),
        );
        let mut scan = Scan {
            cursor: tree.seek(start),
            tree,
            end: end.map(<[u8]>::to_vec),
            committed: None,
            writes: self.writes.range::<[u8], _>((start, end)).peekable(),
            stopped: None,
        };
        scan.read_committed();
        scan
    }

    /// Stores `value` under `key` when the transaction commits. A value
    /// longer than [`crate::MAX_ENTRY_LEN`] minus the key goes to overflow
    /// pages; values can be up to [`MAX_VALUE_LEN`] bytes.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.db.check_running()?;
        check_key(key)?;
        check_value_len(value.len())?;
        self.writes.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    /// Removes `key` and its value when the transaction commits. Deleting a
    /// key that does not exist is not an error.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.db.check_running()?;
        check_key(key)?;
        self.writes.insert(key.to_vec(), None);
        Ok(())
    }

    /// Makes the writes durable and visible.
    ///
    /// Pages that fill up split in two, and the tree grows a level when the
    /// root splits. A leaf only splits if what the commit leaves in it does
    /// not fit, whatever order the keys sort in. Leaves the commit empties
    /// leave the tree.
    ///
    /// If writing or syncing the WAL fails, it returns
    /// [`Error::OutcomeUnknown`]: the writes may or may not be on disk, and
    /// opening the database again shows them completely or not at all. The
    /// engine stops then: every request after it fails with
    /// [`Error::Stopped`]. A commit that leaves the WAL longer than
    /// [`CHECKPOINT_AFTER`] checkpoints; if that fails, the commit is
    /// durable, so it returns `Ok`, and the engine stops.
    pub fn commit(self) -> Result<()> {
        if self.writes.is_empty() {
            return self.db.check_running();
        }
        let mut tree = self.db.tree();
        // Checked holding the lock, so a commit that failed while this one
        // waited for it is seen, and nothing is written after a failure.
        self.db.check_running()?;
        let mut next = Tree::clone(&tree);
        // Take out every key this transaction writes before putting any
        // back, so space freed by a delete or a shrinking value is there for
        // a put that sorts before it.
        for key in self.writes.keys() {
            next.delete(key);
        }
        for (key, value) in &self.writes {
            if let Some(value) = value {
                next.put(key, value);
            }
        }
        // Taken even for a commit that writes nothing, to see whether the
        // WAL is due a checkpoint.
        let mut wal = self.db.wal();
        if next.has_dirty() || next.meta() != tree.meta() {
            if let Err(e) = append(&mut wal, &mut next) {
                // Appending only fails writing or syncing, so whatever it
                // is, the outcome is unknown. The tree is not swapped and
                // the WAL tail stays where it was, but neither is used
                // again: the engine stops.
                let e = Error::OutcomeUnknown(Arc::new(e));
                self.db.stop(&e);
                return Err(e);
            }
        }
        *tree = Arc::new(next);
        if wal.len() > CHECKPOINT_AFTER {
            if let Err(e) = checkpoint::run(&self.db.file, &mut wal, &tree) {
                // The commit is in the WAL and synced: it stays, whatever
                // the checkpoint did. The next request reports the stop.
                self.db.stop(&e);
            }
        }
        Ok(())
    }

    /// Throws away the writes.
    pub fn rollback(self) {}
}

/// The range from `start` to `end`, or, if it holds no key because its
/// start is past its end, an empty range that starts and ends at `start`.
/// `BTreeMap::range` panics on such ranges; it does not on this one.
fn empty_if_backwards<'k>(
    start: Bound<&'k [u8]>,
    end: Bound<&'k [u8]>,
) -> (Bound<&'k [u8]>, Bound<&'k [u8]>) {
    if let (
        Bound::Included(from) | Bound::Excluded(from),
        Bound::Included(to) | Bound::Excluded(to),
    ) = (start, end)
    {
        let both_included = matches!((start, end), (Bound::Included(_), Bound::Included(_)));
        if from > to || (from == to && !both_included) {
            return (Bound::Included(from), Bound::Excluded(from));
        }
    }
    (start, end)
}

/// The entries in a range, from [`Txn::scan`].
///
/// Merges two sorted streams: the committed tree, leaf by leaf, and the
/// transaction's own writes. Where both have a key, the write wins, and a
/// delete hides the key.
#[must_use = "a scan reads nothing until it is iterated"]
pub struct Scan<'txn> {
    tree: Arc<Tree>,
    cursor: Cursor,
    end: Bound<Vec<u8>>,
    /// The next committed entry in the range, if any, read ahead to compare
    /// with the next write.
    committed: Option<(Vec<u8>, Vec<u8>)>,
    writes: Peekable<btree_map::Range<'txn, Vec<u8>, Option<Vec<u8>>>>,
    /// The engine stopped before the scan started: the only item reports
    /// it.
    stopped: Option<Error>,
}

impl std::fmt::Debug for Scan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan").finish_non_exhaustive()
    }
}

/// No key, for the scan of a stopped engine.
static NO_WRITES: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();

impl Scan<'_> {
    /// The scan of a stopped engine: its only item is `e`.
    fn stopped<'txn>(tree: Arc<Tree>, e: Error) -> Scan<'txn> {
        Scan {
            cursor: tree.seek(Bound::Unbounded),
            tree,
            end: Bound::Unbounded,
            committed: None,
            writes: NO_WRITES.range::<[u8], _>(..).peekable(),
            stopped: Some(e),
        }
    }

    /// Reads the next committed entry into `committed`, or `None` past the
    /// end of the range.
    fn read_committed(&mut self) {
        let end = self.end.as_ref().map(Vec::as_slice);
        let tree = &self.tree;
        self.committed = tree
            .next_entry(&mut self.cursor)
            .filter(|&(key, _)| (Bound::Unbounded, end).contains(key))
            .map(|(key, value)| (key.to_vec(), tree.value(value).into_owned()));
    }
}

// Once both streams run out, `next` keeps returning `None`.
impl FusedIterator for Scan<'_> {}

impl Iterator for Scan<'_> {
    /// `Err` only as the first item, if the engine stopped: every page is
    /// in memory. Reading pages from disk on demand will make other errors
    /// possible.
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.stopped.take() {
            return Some(Err(e));
        }
        loop {
            let write_first = match (&self.committed, self.writes.peek()) {
                (None, None) => return None,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some((committed, _)), Some((written, _))) => written <= &committed,
            };
            if !write_first {
                let entry = self.committed.take();
                self.read_committed();
                return entry.map(Ok);
            }
            let (key, write) = self.writes.next().expect("peeked");
            if self
                .committed
                .as_ref()
                .is_some_and(|(committed, _)| committed == key)
            {
                self.read_committed();
            }
            if let Some(value) = write {
                return Some(Ok((key.clone(), value.clone())));
            }
        }
    }
}

fn check_value_len(len: usize) -> Result<()> {
    if len > MAX_VALUE_LEN {
        return Err(Error::ValueTooLarge {
            len,
            max: MAX_VALUE_LEN,
        });
    }
    Ok(())
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

/// Commits to `wal` the pages of `tree` changed since the last commit,
/// with their checksums, and its meta fields, and syncs.
fn append(wal: &mut Wal<impl FsFile>, tree: &mut Tree) -> std::io::Result<()> {
    let dirty = tree.take_dirty();
    let pages: Vec<(u64, &[u8; PAGE_SIZE])> = dirty
        .iter()
        .map(|&page_no| (page_no, tree.page(page_no).as_bytes()))
        .collect();
    wal.append(&pages, tree.meta())
}

/// What opening does with the WAL it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UseWal {
    /// Replay it over the data file: it follows the checkpoint opening
    /// builds on.
    Replay,
    /// Start it again after that checkpoint: it has no header, or follows
    /// an older checkpoint, whose commits the data file already holds.
    Restart,
}

/// Which meta slot opening builds on, if any, and what it does with the
/// WAL (design 0011, section 3), given the newest slot that checks out
/// (`Ok(None)` for an empty data file), the checkpoint the WAL header
/// names, and whether the WAL holds a whole commit.
fn choose_base(
    slot: Result<Option<Slot>>,
    wal_checkpoint: Option<u64>,
    wal_has_commit: bool,
) -> Result<(Option<Slot>, UseWal)> {
    match (slot, wal_checkpoint) {
        (Ok(Some(slot)), Some(after)) if after > slot.checkpoint => {
            // The slot that knows of checkpoint `after` is damaged, and
            // the WAL of the one before is gone.
            Err(Error::Corrupt(format!(
                "WAL follows checkpoint {after}, but the newest meta slot holds checkpoint {}",
                slot.checkpoint
            )))
        }
        (Ok(Some(slot)), Some(after)) if after == slot.checkpoint => {
            Ok((Some(slot), UseWal::Replay))
        }
        (Ok(Some(slot)), _) => Ok((Some(slot), UseWal::Restart)),
        (Ok(None), Some(0)) => Ok((None, UseWal::Replay)),
        (Ok(None), None) => Ok((None, UseWal::Restart)),
        (Ok(None), Some(after)) => Err(Error::Corrupt(format!(
            "WAL follows checkpoint {after}, but the data file is empty"
        ))),
        // The first checkpoint was cut short before a meta slot was
        // written whole: the WAL holds every commit since creating. Only
        // damage is passed over; a read that failed, or a data file of
        // another page size, is not what such a cut leaves.
        (Err(Error::Corrupt(_)), Some(0)) if wal_has_commit => Ok((None, UseWal::Replay)),
        (Err(e), _) => Err(e),
    }
}

/// The meta slot with the higher checkpoint of the two that check out, or
/// the error of slot A if neither does.
fn newest_slot(file: &impl FsFile, size: u64) -> Result<Slot> {
    match (read_slot(file, size, 0), read_slot(file, size, 1)) {
        (Ok(a), Ok(b)) if b.checkpoint > a.checkpoint => Ok(b),
        (Ok(a), _) => Ok(a),
        (Err(_), Ok(b)) => Ok(b),
        (Err(e), Err(_)) => Err(e),
    }
}

/// Meta slot `page_no` (0 or 1) of a data file `size` bytes long.
fn read_slot(file: &impl FsFile, size: u64, page_no: u64) -> Result<Slot> {
    let at = page_offset(page_no);
    if size < at + META_LEN as u64 {
        return Err(Error::Corrupt("data file is too short".into()));
    }
    let mut header = [0; META_LEN];
    file.read_exact_at(&mut header, at)?;
    // Magic, page size and version first: they say how long a page is
    // and whether it has a checksum at all.
    Meta::check_header(&header)?;
    if u128::from(size) < pages_len(page_no + 1) {
        return Err(Error::Corrupt("data file is too short".into()));
    }
    Slot::read(&read_page(file, page_no)?)
}

/// The tree `meta` describes, each page from the WAL if it holds one and
/// from the first `data_len` bytes of the data file if not.
fn load(
    file: &impl FsFile,
    data_len: u64,
    meta: Meta,
    mut pages: BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
) -> Result<Tree> {
    Tree::load(meta, |page_no| {
        let bytes: Box<[u8]> = match pages.remove(&page_no) {
            Some(image) => {
                checksum::verify(page_no, &*image)?;
                image
            }
            None if pages_len(page_no + 1) <= u128::from(data_len) => read_page(file, page_no)?,
            None => {
                return Err(Error::Corrupt(format!(
                    "page {page_no} is in neither the WAL nor the data file"
                )))
            }
        };
        Page::from_bytes(bytes)
    })
}

/// The whole of `file`.
fn read_all(file: &impl FsFile) -> Result<Vec<u8>> {
    let len = usize::try_from(file.size()?)
        .map_err(|_| Error::Corrupt("WAL is too long to read into memory".into()))?;
    let mut bytes = vec![0; len];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}

/// Reads page `page_no`, refusing it if its checksum does not match.
fn read_page(file: &impl FsFile, page_no: u64) -> Result<Box<[u8]>> {
    let mut buf = vec![0; PAGE_SIZE].into_boxed_slice();
    file.read_exact_at(&mut buf, page_offset(page_no))?;
    checksum::verify(page_no, &buf)?;
    Ok(buf)
}

/// Where page `page_no` starts in the data file. Only for pages known to
/// be in the database (a meta slot, or a page of a tree that was checked):
/// a page number read from disk goes through [`pages_len`] first.
pub(crate) fn page_offset(page_no: u64) -> u64 {
    page_no
        .checked_mul(PAGE_SIZE as u64)
        .expect("a page of the database is inside a file")
}

/// How many bytes `pages` pages take. In `u128`, where any page count read
/// from disk fits, so a damaged one is refused by comparing lengths
/// instead of wrapping around or panicking (review S3).
fn pages_len(pages: u64) -> u128 {
    u128::from(pages) * PAGE_SIZE as u128
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::encode_for_page_size;

    #[test]
    fn a_data_file_with_another_page_size_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let slot = Slot {
            meta: Meta {
                root_page: 2,
                page_count: 3,
                free_head: None,
                free_count: 0,
            },
            checkpoint: 1,
        };
        let mut bytes = encode_for_page_size(&slot, 8192);
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

    fn slot(checkpoint: u64) -> Slot {
        Slot {
            meta: Meta {
                root_page: 2,
                page_count: 3,
                free_head: None,
                free_count: 0,
            },
            checkpoint,
        }
    }

    fn damaged() -> Result<Option<Slot>> {
        Err(Error::Corrupt("page 0 has a wrong checksum".into()))
    }

    #[test]
    fn opening_builds_on_the_slot_the_wal_follows() {
        let base = |slot, wal, commit| choose_base(slot, wal, commit).unwrap();
        // The WAL follows the newest slot: replay it.
        assert_eq!(
            base(Ok(Some(slot(2))), Some(2), true),
            (Some(slot(2)), UseWal::Replay)
        );
        // An older WAL, or none: the slot holds it all.
        assert_eq!(
            base(Ok(Some(slot(2))), Some(1), true),
            (Some(slot(2)), UseWal::Restart)
        );
        assert_eq!(
            base(Ok(Some(slot(2))), None, false),
            (Some(slot(2)), UseWal::Restart)
        );
        // No data file yet.
        assert_eq!(base(Ok(None), Some(0), true), (None, UseWal::Replay));
        assert_eq!(base(Ok(None), None, false), (None, UseWal::Restart));
        // A first checkpoint cut before a slot was written whole.
        assert_eq!(base(damaged(), Some(0), true), (None, UseWal::Replay));
    }

    #[test]
    fn opening_refuses_a_wal_no_slot_it_can_use_explains() {
        let refused =
            |slot, wal, commit| matches!(choose_base(slot, wal, commit), Err(Error::Corrupt(_)));
        // The newest slot is damaged once its WAL is gone.
        assert!(refused(Ok(Some(slot(1))), Some(2), true));
        // A WAL after a checkpoint, with no data file.
        assert!(refused(Ok(None), Some(1), false));
        // Damaged slots and no WAL that holds everything.
        assert!(refused(damaged(), Some(0), false));
        assert!(refused(damaged(), Some(1), true));
        assert!(refused(damaged(), None, false));
    }

    #[test]
    fn only_damaged_slots_are_passed_over_before_the_first_checkpoint() {
        let other_size = Err(Error::PageSizeMismatch {
            expected: 4096,
            found: 8192,
        });
        assert!(matches!(
            choose_base(other_size, Some(0), true),
            Err(Error::PageSizeMismatch { .. })
        ));
        let failed_read = Err(Error::from(std::io::Error::other("read failed")));
        assert!(matches!(
            choose_base(failed_read, Some(0), true),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn a_value_longer_than_the_limit_is_refused() {
        assert!(check_value_len(MAX_VALUE_LEN).is_ok());
        assert!(matches!(
            check_value_len(MAX_VALUE_LEN + 1),
            Err(Error::ValueTooLarge { len, max: MAX_VALUE_LEN }) if len == MAX_VALUE_LEN + 1
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
