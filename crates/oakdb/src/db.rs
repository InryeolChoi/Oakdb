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
//! A transaction reads the tree committed when it began (its snapshot,
//! issue #25), with its own writes on top, and commits its writes to the
//! latest tree.
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
//!
//! Threads share a database (`&Db` in a scope, or `Arc<Db>`) and run
//! transactions at once (issue #27, design 0018). Commits and checkpoints
//! go, in the order they come, to the commit thread, which alone touches
//! the files. It takes the commits waiting for it as one batch, which
//! shares one WAL write and one sync (issue #28, design 0021). It holds no lock while it syncs: transactions
//! begin, read and end meanwhile, and a commit shows only once it is
//! synced.

use std::any::Any;
use std::collections::{btree_map, BTreeMap, BTreeSet, HashSet};
use std::iter::{FusedIterator, Peekable};
use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, PoisonError};
use std::thread::JoinHandle;

use crate::checkpoint;
use crate::checksum;
use crate::commit_queue::{self, Handler, Queue};
use crate::conflict::RecentWrites;
use crate::error::{Error, Result, MAX_KEY_LEN, MAX_VALUE_LEN};
use crate::fs::{Fs, FsFile, StdFs};
use crate::meta::{Meta, Slot, META_LEN};
use crate::page::{Page, PAGE_SIZE};
use crate::planted::PlantedBug;
use crate::sync::{mpsc, Mutex, MutexGuard};
use crate::tree::{Cursor, Tree};
use crate::wal::{self, Replayed, Tail, Wal, CUT_FILE, HEADER_LEN, WAL_FILE};

/// Name of the data file inside the database directory.
pub const DATA_FILE: &str = "oakdb.data";

/// A commit that leaves the WAL longer than this many bytes checkpoints,
/// which bounds how much opening replays. A fixed value for now; it may
/// become a setting of the database.
pub const CHECKPOINT_AFTER: u64 = 4 * 1024 * 1024;

/// The name of the thread that commits and checkpoints (design 0018). It
/// is public so that tests and tools can tell that thread apart, in a
/// debugger, `top -H`, or a test that checks which thread syncs.
pub const COMMIT_THREAD: &str = "oakdb-commit";

/// The most commits the commit thread takes into one batch, which shares
/// one WAL write and one sync (issue #28, design 0021). Commits waiting
/// beyond it go in the next batch, in the order they came.
pub const MAX_BATCH: usize = 128;

/// What the commit thread did since the database was opened, from
/// [`Db::stats`] (issue #28, design 0021). Several commits sharing one sync
/// show as `wal_syncs < commits`. A batch holds the commits that passed and
/// those that conflicted, so the mean batch is
/// `(commits + conflicts + in_doubt) / batches`, not `commits / batches`.
/// The numbers are read one by one while the commit thread works, so they
/// may be a moment apart: `commits` can briefly exceed `received`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Requests sent to the commit thread, commits and checkpoints. A
    /// request is counted once it is waiting there, so just after it is
    /// sent: seeing the count means the request is queued.
    pub received: u64,
    /// Commits answered `Ok` that had writes.
    pub commits: u64,
    /// Commits that failed with [`Error::Conflict`].
    pub conflicts: u64,
    /// Batches of commits the commit thread took, one or more each.
    pub batches: u64,
    /// Syncs of the WAL that commits made: one per batch that changed
    /// anything. Checkpoints' syncs are not counted.
    pub wal_syncs: u64,
    /// The most commits one batch held, conflicting ones included, as
    /// [`MAX_BATCH`] counts them.
    pub largest_batch: u64,
    /// Commits answered [`Error::OutcomeUnknown`]: those that passed the
    /// conflict check in the batch whose write or sync failed.
    pub in_doubt: u64,
}

/// An open database. Threads can share it: it is `Send` and `Sync`.
pub struct Db<F: Fs = StdFs> {
    /// What transactions share with the commit thread.
    shared: Arc<Shared>,
    /// Where commits and checkpoints go, to the commit thread. Closed when
    /// the database is dropped.
    requests: Arc<Queue<Request>>,
    /// The commit thread, which holds the files. Joined when the database
    /// is dropped. `None` only then.
    thread: Option<JoinHandle<()>>,
    /// The file system the files came from.
    fs: PhantomData<fn() -> F>,
}

/// What transactions share with the commit thread.
struct Shared {
    /// The committed tree, and the keys recent commits wrote. Held only for
    /// moments: to begin or end a transaction, and by the commit thread to
    /// check for conflicts and to swap in a new tree, never while it syncs.
    /// A transaction keeps the tree it began on, so commits never change
    /// it under it.
    committed: Mutex<Committed>,
    /// The bug planted, if any (issue #29).
    #[cfg(feature = "planted-bugs")]
    bug: Option<PlantedBug>,
    /// Why the engine stopped, once a write or sync failed. Set at most
    /// once, by the commit thread, and never cleared: this process does
    /// not trust the files again (design 0012).
    stopped: OnceLock<Arc<Error>>,
    /// What [`Db::stats`] reports.
    counters: Counters,
}

/// The numbers of [`Stats`], kept as the commit thread works. Each is
/// written by one side only: `received` by the threads that send requests,
/// the rest by the commit thread, before it answers.
#[derive(Default)]
struct Counters {
    received: AtomicU64,
    commits: AtomicU64,
    conflicts: AtomicU64,
    batches: AtomicU64,
    wal_syncs: AtomicU64,
    largest_batch: AtomicU64,
    in_doubt: AtomicU64,
}

impl Counters {
    fn stats(&self) -> Stats {
        Stats {
            received: self.received.load(Ordering::Relaxed),
            commits: self.commits.load(Ordering::Relaxed),
            conflicts: self.conflicts.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            wal_syncs: self.wal_syncs.load(Ordering::Relaxed),
            largest_batch: self.largest_batch.load(Ordering::Relaxed),
            in_doubt: self.in_doubt.load(Ordering::Relaxed),
        }
    }
}

/// A transaction's writes: the last write to each key, `Some(value)` for a
/// put, `None` for a delete.
type Writes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// Where the commit thread answers a request.
type Answer = mpsc::Sender<Result<()>>;

/// Work for the commit thread: a commit, or a checkpoint with where to
/// answer it.
type Request = commit_queue::Request<Waiting, Answer>;

/// The commit thread's own state: the files, and the latest tree.
struct Committer<File: FsFile> {
    shared: Arc<Shared>,
    /// The data file, held open for its lock. Checkpoints write it.
    file: File,
    /// Where commits go.
    wal: Wal<File>,
    /// The latest committed tree, the same as `shared`'s: kept here too so
    /// building the next commit takes no lock.
    tree: Arc<Tree>,
}

/// What commits change, under one lock, so a transaction's snapshot and
/// the commit number it began at always match.
struct Committed {
    tree: Arc<Tree>,
    /// The keys recent commits wrote, to find conflicts (issue #26).
    recent: RecentWrites,
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
        Self::open_planted(fs, path.as_ref(), None)
    }

    /// Opens the database as [`Db::open_with`] does, with `bug` planted in
    /// it, for a test to see the history checker find it (issue #29).
    #[cfg(feature = "planted-bugs")]
    pub fn open_with_bug(fs: F, path: impl AsRef<Path>, bug: PlantedBug) -> Result<Self> {
        Self::open_planted(fs, path.as_ref(), Some(bug))
    }

    fn open_planted(fs: F, dir: &Path, bug: Option<PlantedBug>) -> Result<Self> {
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
        let tree = Arc::new(tree);
        #[cfg(not(feature = "planted-bugs"))]
        let _ = bug;
        let shared = Arc::new(Shared {
            committed: Mutex::new(Committed {
                tree: Arc::clone(&tree),
                recent: RecentWrites::default(),
            }),
            stopped: OnceLock::new(),
            #[cfg(feature = "planted-bugs")]
            bug,
            counters: Counters::default(),
        });
        let committer = Committer {
            shared: Arc::clone(&shared),
            file,
            wal,
            tree,
        };
        let requests = Arc::new(Queue::new());
        let queue = Arc::clone(&requests);
        let thread = std::thread::Builder::new()
            .name(COMMIT_THREAD.into())
            .spawn(move || committer.run(&queue))?;
        Ok(Db {
            shared,
            requests,
            thread: Some(thread),
            fs: PhantomData,
        })
    }

    /// Starts a transaction. It reads the database as committed now,
    /// whatever is committed while it runs (issue #25). Holding it holds
    /// that version of the tree in memory: pages commits change after this
    /// are copies, so the ones it reads stay until it ends. It takes the
    /// tree's lock for a moment; commits do not hold it while they sync.
    pub fn begin(&self) -> Txn<'_, F> {
        let mut committed = self.shared.committed();
        Txn {
            db: self,
            snapshot: Arc::clone(&committed.tree),
            begun_after: committed.recent.begin(),
            writes: BTreeMap::new(),
        }
    }

    /// How many pages the database uses, free ones included, counting the
    /// two meta pages: what its data file holds once checkpointed.
    pub fn page_count(&self) -> u64 {
        self.shared.committed().tree.meta().page_count
    }

    /// What the commit thread did since the database was opened.
    pub fn stats(&self) -> Stats {
        self.shared.counters.stats()
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
    ///
    /// It runs on the commit thread, after the commits that came before it.
    pub fn checkpoint(&self) -> Result<()> {
        self.check_running()?;
        self.ask(Request::Checkpoint)
    }

    /// Checkpoints and closes the database. Every committed transaction is
    /// already durable, so dropping it without closing loses nothing; the
    /// next open replays the WAL instead. Fails as [`Db::checkpoint`] does,
    /// and every commit acknowledged before is kept all the same.
    pub fn close(self) -> Result<()> {
        self.checkpoint()
    }

    /// Fails with [`Error::Stopped`] if the engine stopped, as every
    /// request after the failure does. [`Db::begin`] cannot fail, so a
    /// caller that wants to refuse a transaction at its start (the SQL
    /// layer's BEGIN) asks this first.
    pub fn check_running(&self) -> Result<()> {
        self.shared.check_running()
    }

    /// Sends the commit thread the request `request` makes of where to
    /// answer, and waits for the answer. The commit thread answers every
    /// request until the database is dropped, even after it panicked
    /// ([`Committer::guarded`]).
    fn ask(&self, request: impl FnOnce(Answer) -> Request) -> Result<()> {
        let (answer, answered) = mpsc::channel();
        self.requests.send(request(answer));
        // Counted once it waits there, so a test that sees the count knows
        // the request is queued (design 0021 Q6).
        bump(&self.shared.counters.received, 1);
        answered
            .recv()
            .expect("the commit thread answers every request")
    }
}

impl<F: Fs> Drop for Db<F> {
    /// Stops the commit thread and waits for it, so the files are closed
    /// and their lock is free when this returns. No request can be under
    /// way: each waits for its answer holding a borrow of the database.
    fn drop(&mut self) {
        self.requests.close();
        if let Some(thread) = self.thread.take() {
            // A commit thread that panicked already made its requester
            // panic; another panic here, perhaps while unwinding, helps no
            // one.
            let _ = thread.join();
        }
    }
}

impl Shared {
    /// Whether `bug` is planted. Always false without the `planted-bugs`
    /// feature: the code it guards is still compiled, but the optimizer
    /// removes it. Only the public API (`PlantedBug`, `Db::open_with_bug`)
    /// is behind the feature.
    #[inline(always)]
    fn planted(&self, bug: PlantedBug) -> bool {
        #[cfg(feature = "planted-bugs")]
        return self.bug == Some(bug);
        #[cfg(not(feature = "planted-bugs"))]
        {
            let _ = bug;
            false
        }
    }

    /// Fails with [`Error::Stopped`] if the engine stopped.
    fn check_running(&self) -> Result<()> {
        match self.stopped.get() {
            Some(cause) => Err(Error::Stopped(Arc::clone(cause))),
            None => Ok(()),
        }
    }

    /// Stops the engine because of `cause`, the error the failing call
    /// returns, which every request after it reports. Only the commit
    /// thread calls it, one request at a time, so the first failure is the
    /// one kept.
    fn stop(&self, cause: &Error) {
        let _ = self.stopped.set(Arc::new(cause.clone()));
    }

    fn committed(&self) -> MutexGuard<'_, Committed> {
        // A panic while holding the lock cannot leave the tree half changed:
        // the commit thread changes a copy and records the keys before it
        // swaps the copy in.
        self.committed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A commit waiting in a batch: the transaction's writes, the number of
/// the last commit in its snapshot, and where to answer it. The answer is
/// taken once sent, so a batch that panics part way knows who is left.
struct Waiting {
    writes: Writes,
    begun_after: u64,
    answer: Option<Answer>,
}

impl Waiting {
    fn new(writes: Writes, begun_after: u64, answer: Answer) -> Self {
        Waiting {
            writes,
            begun_after,
            answer: Some(answer),
        }
    }

    fn answer(&mut self, result: Result<()>) {
        if let Some(answer) = self.answer.take() {
            // A requester that is gone (it panicked) needs no answer.
            let _ = answer.send(result);
        }
    }
}

/// What the conflict check decided for one commit of a batch.
enum Verdict {
    /// It writes: it goes on the batch's tree.
    Passed,
    /// A recorded commit wrote one of its keys: answered at once.
    Lost(Error),
    /// A commit ahead in the same batch wrote one of its keys: answered once
    /// the batch shows (or failed), so a transaction begun after the answer
    /// reads the winner's write, as [`Error::Conflict`] promises.
    LostInBatch(Error),
}

impl<File: FsFile> Committer<File> {
    /// Answers requests in the order they came until the database is
    /// dropped. Commits go in batches (issue #28, design 0021): see
    /// [`commit_queue::serve`].
    fn run(mut self, requests: &Queue<Request>) {
        commit_queue::serve(&mut self, requests, MAX_BATCH);
    }

    /// Runs `work`, the request being answered. If it panics, a bug, it
    /// returns `failed` of the panic's message and the engine stops, so
    /// every request after it gets [`Error::Stopped`] instead of a panic in
    /// its caller's thread. A commit that panicked gets
    /// [`Error::OutcomeUnknown`]: it may have reached the WAL.
    ///
    /// The thread's state may be half changed after a panic, which is why
    /// unwinding is safe to catch here: the engine stops, and every request
    /// checks that first, so nothing reads that state again.
    fn guarded<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> T,
        failed: fn(Arc<std::io::Error>) -> Error,
    ) -> Result<T> {
        panic::catch_unwind(AssertUnwindSafe(|| work(self))).map_err(|payload| {
            let e = failed(Arc::new(std::io::Error::other(format!(
                "the commit thread panicked: {}",
                panic_message(payload.as_ref())
            ))));
            self.shared.stop(&e);
            e
        })
    }

    /// Commits `batch`, in the order it came, and answers each: see
    /// [`Txn::commit`]. The commits that pass the conflict check go on one
    /// copy of the tree, which goes to the WAL as one commit with one sync
    /// (ADR 0007: after a power cut a batch is all there or not at all),
    /// and shows once that sync is done. A commit that lost to a recorded
    /// commit is answered at once; one that lost to a commit ahead in the
    /// batch, only once the batch shows or failed (design 0021 Q2).
    fn commit_batch(&mut self, batch: &mut [Waiting]) {
        // The check that matters: a commit that was waiting here when the
        // batch ahead of it failed writes nothing. The caller's check
        // before sending only saves a trip.
        if let Err(e) = self.shared.check_running() {
            for waiting in batch {
                waiting.answer(Err(e.clone()));
            }
            return;
        }
        let counters = &self.shared.counters;
        bump(&counters.batches, 1);
        counters
            .largest_batch
            .fetch_max(batch.len() as u64, Ordering::Relaxed);
        let (verdicts, next) = self.judge(batch);
        let mut passed = Vec::new();
        let mut later = Vec::new();
        for (at, verdict) in verdicts.into_iter().enumerate() {
            match verdict {
                Verdict::Passed => passed.push(at),
                Verdict::Lost(e) => {
                    bump(&self.shared.counters.conflicts, 1);
                    batch[at].answer(Err(e));
                }
                Verdict::LostInBatch(e) => {
                    bump(&self.shared.counters.conflicts, 1);
                    later.push((at, e));
                }
            }
        }
        if let Some(mut next) = next {
            let early = self.shared.planted(PlantedBug::PublishBeforeSync);
            if early {
                let commits = passed.iter().map(|&at| batch[at].writes.clone()).collect();
                self.publish(Tree::clone(&next), commits);
            }
            if let Err(e) = self.write(&mut next) {
                bump(&self.shared.counters.in_doubt, passed.len() as u64);
                for &at in &passed {
                    batch[at].answer(Err(e.clone()));
                }
                answer_later(batch, later);
                return;
            }
            if self.shared.planted(PlantedBug::AnswerBeforePublish) {
                for &at in &passed {
                    batch[at].answer(Ok(()));
                }
                std::thread::yield_now();
            }
            if !early {
                let commits: Vec<Writes> = passed
                    .iter()
                    .map(|&at| std::mem::take(&mut batch[at].writes))
                    .collect();
                self.publish(next, commits);
            }
            bump(&self.shared.counters.commits, passed.len() as u64);
            for &at in &passed {
                batch[at].answer(Ok(()));
            }
        }
        answer_later(batch, later);
        // After the answers: the batch is durable, and a checkpoint that
        // fails does not change them (design 0021 G6).
        self.checkpoint_if_due();
    }

    /// Checks each commit of `batch` for conflicts, in order, and makes the
    /// writes of those that pass on a copy of the latest tree, taken only
    /// once one passes.
    ///
    /// First committer wins (ADR 0006): a commit fails if a commit since its
    /// transaction began wrote one of its keys. Only writes are compared,
    /// so reads never conflict (snapshot isolation; write skew is allowed).
    /// Only this thread records keys, so none can be recorded between this
    /// check and the batch's record in [`Committer::publish`], though the
    /// lock is let go. The keys of the commits ahead in the batch are not
    /// recorded until the batch is synced, so they are compared here too.
    /// Every transaction in the batch began before the batch shows, so
    /// such a key was written after it began: a conflict, whatever its
    /// number would be.
    fn judge(&self, batch: &[Waiting]) -> (Vec<Verdict>, Option<Tree>) {
        let mut next: Option<Tree> = None;
        let mut written: HashSet<&[u8]> = HashSet::new();
        let mut verdicts = Vec::with_capacity(batch.len());
        for waiting in batch {
            let keys = || waiting.writes.keys().map(Vec::as_slice);
            let skip = self.shared.planted(PlantedBug::SkipConflictCheck);
            let in_batch = match skip || self.shared.planted(PlantedBug::SkipInBatchConflict) {
                true => None,
                false => keys().find(|key| written.contains(key)),
            };
            let recorded = || {
                if skip {
                    return None;
                }
                self.shared
                    .committed()
                    .recent
                    .conflict(waiting.begun_after, keys())
                    .map(<[u8]>::to_vec)
            };
            let verdict = if let Some(key) = in_batch {
                Verdict::LostInBatch(Error::Conflict { key: key.to_vec() })
            } else if let Some(key) = recorded() {
                Verdict::Lost(Error::Conflict { key })
            } else {
                apply(
                    next.get_or_insert_with(|| Tree::clone(&self.tree)),
                    &waiting.writes,
                );
                written.extend(keys());
                Verdict::Passed
            };
            verdicts.push(verdict);
        }
        (verdicts, next)
    }

    /// Writes the pages `next` changed and its meta fields to the WAL, and
    /// syncs, if it changed anything. No lock is held meanwhile:
    /// transactions begin and end on the tree before this batch.
    fn write(&mut self, next: &mut Tree) -> Result<()> {
        if !next.has_dirty() && next.meta() == self.tree.meta() {
            return Ok(());
        }
        append(&mut self.wal, next).map_err(|e| {
            // Appending only fails writing or syncing, so whatever it is,
            // the outcome is unknown. The tree is not swapped and the WAL
            // tail stays where it was, but neither is used again: the
            // engine stops.
            let e = Error::OutcomeUnknown(Arc::new(e));
            self.shared.stop(&e);
            e
        })?;
        bump(&self.shared.counters.wal_syncs, 1);
        Ok(())
    }

    /// Makes `next`, now durable, the tree transactions begin on, and
    /// records the keys of each of `commits`, in order, for later
    /// conflicts. All under one lock: a transaction sees the whole batch or
    /// none of it.
    fn publish(&mut self, next: Tree, commits: Vec<Writes>) {
        let next = Arc::new(next);
        if self.shared.planted(PlantedBug::SplitPublish) {
            {
                let mut committed = self.shared.committed();
                for writes in commits {
                    committed.recent.record(writes.into_keys());
                }
            }
            // Wide enough for a transaction to begin in it.
            std::thread::sleep(std::time::Duration::from_micros(50));
            self.shared.committed().tree = Arc::clone(&next);
            self.tree = next;
            return;
        }
        {
            let mut committed = self.shared.committed();
            // The keys go down before the new tree goes in: were anything
            // in between to panic, a transaction could see the tree with no
            // record of who wrote it, and overwrite it without a conflict.
            // This way it can at worst conflict with a commit that never
            // showed. Even writes that changed nothing in the tree count: a
            // write is a write, and a transaction that began before this
            // one and writes the same key must fail all the same.
            for writes in commits {
                committed.recent.record(writes.into_keys());
            }
            committed.tree = Arc::clone(&next);
        }
        self.tree = next;
    }

    /// Checkpoints if the WAL is longer than [`CHECKPOINT_AFTER`]. If that
    /// fails, the engine stops, but the batch before it is in the WAL and
    /// synced: it stays, and was answered `Ok`. The next request reports
    /// the stop.
    fn checkpoint_if_due(&mut self) {
        if self.wal.len() > CHECKPOINT_AFTER {
            if let Err(e) = checkpoint::run(&self.file, &mut self.wal, &self.tree) {
                self.shared.stop(&e);
            }
        }
    }

    /// See [`Db::checkpoint`].
    fn checkpoint(&mut self) -> Result<()> {
        // Checked here, so nothing is written after a failure.
        self.shared.check_running()?;
        checkpoint::run(&self.file, &mut self.wal, &self.tree).inspect_err(|e| self.shared.stop(e))
    }
}

impl<File: FsFile> Handler for Committer<File> {
    type Commit = Waiting;
    type Checkpoint = Answer;

    fn commit(&mut self, mut batch: Vec<Waiting>) {
        if let Err(e) = self.guarded(|c| c.commit_batch(&mut batch), Error::OutcomeUnknown) {
            // It panicked, a bug: whoever it did not answer may be in the
            // WAL.
            let left = batch.iter().filter(|w| w.answer.is_some()).count();
            bump(&self.shared.counters.in_doubt, left as u64);
            for waiting in &mut batch {
                waiting.answer(Err(e.clone()));
            }
        }
    }

    fn checkpoint(&mut self, answer: Answer) {
        let result = self.guarded(Self::checkpoint, Error::Io).and_then(|r| r);
        let _ = answer.send(result);
    }
}

/// Makes a transaction's `writes` in `tree`.
fn apply(tree: &mut Tree, writes: &Writes) {
    // Take out every key the transaction writes before putting any back,
    // so space freed by a delete or a shrinking value is there for a put
    // that sorts before it.
    for key in writes.keys() {
        tree.delete(key);
    }
    for (key, value) in writes {
        if let Some(value) = value {
            tree.put(key, value);
        }
    }
}

/// Answers the commits of `batch` that lost to a commit ahead in it.
fn answer_later(batch: &mut [Waiting], later: Vec<(usize, Error)>) {
    for (at, e) in later {
        batch[at].answer(Err(e));
    }
}

/// Adds `n` to `counter`, one of [`Counters`].
fn bump(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// The message a panic was raised with, if it was text.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "(no message)"
    }
}

/// A transaction. It reads the snapshot taken when it began, with its own
/// writes on top. Its writes stay private until [`Txn::commit`]; dropping
/// it without committing throws them away.
///
/// Transactions are isolated by snapshot (ADR 0006): of two that write the
/// same key, the later to commit fails with [`Error::Conflict`]. Reads are
/// not compared, so **write skew** is allowed: two transactions that each
/// read keys the other writes, and write different keys, both commit,
/// even if a rule both checked no longer holds afterwards. Write a key
/// both depend on to make them conflict.
pub struct Txn<'db, F: Fs = StdFs> {
    db: &'db Db<F>,
    /// The committed tree when the transaction began. The whole database
    /// is in memory and a commit swaps in a changed copy, so this tree
    /// never changes, checkpoints included: they write the data file, not
    /// the pages in memory.
    snapshot: Arc<Tree>,
    /// The number of the last commit in the snapshot: a commit numbered
    /// higher that wrote one of this transaction's keys makes its commit
    /// fail (issue #26).
    begun_after: u64,
    /// The last write to each key: `Some(value)` for a put, `None` for a
    /// delete.
    writes: Writes,
}

impl<F: Fs> Txn<'_, F> {
    /// The value under `key` in the transaction's snapshot, or its own
    /// write to it.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.db.check_running()?;
        check_key(key)?;
        if let Some(write) = self.writes.get(key) {
            return Ok(write.clone());
        }
        let latest;
        let tree = if self.db.shared.planted(PlantedBug::ReadLatest) {
            latest = Arc::clone(&self.db.shared.committed().tree);
            &latest
        } else {
            &self.snapshot
        };
        // No lock: the snapshot is the transaction's own.
        Ok(tree.find(key).map(|value| tree.value(value).into_owned()))
    }

    /// The entries whose keys are in `range`, in key order, including this
    /// transaction's own writes. Each end of the range can include or
    /// exclude its key, or be left open: `txn.scan("a".."m")`,
    /// `txn.scan::<&[u8]>((Bound::Excluded(from), Bound::Included(to)))`,
    /// or `txn.scan::<&[u8]>(..)` for every entry. The key type has to be
    /// named where the range alone does not say it. A range whose start is
    /// past its end is empty.
    ///
    /// The scan reads the transaction's snapshot, so commits made since it
    /// began, or while the scan runs, do not change what it yields. If the
    /// engine stopped, its only item is [`Error::Stopped`].
    pub fn scan<K: AsRef<[u8]>>(&self, range: impl RangeBounds<K>) -> Scan<'_> {
        let shared = &self.db.shared;
        let tree =
            if shared.planted(PlantedBug::ReadLatest) || shared.planted(PlantedBug::ScanLatest) {
                Arc::clone(&shared.committed().tree)
            } else {
                Arc::clone(&self.snapshot)
            };
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
    ///
    /// If another transaction wrote one of the keys this one writes and
    /// committed after this one began, it returns [`Error::Conflict`] and
    /// writes nothing (issue #26: the first to commit wins). The engine
    /// goes on; begin a new transaction, which sees the winner's write, and
    /// try again.
    ///
    /// The commit runs on the commit thread, after the commits that came
    /// before it (design 0018). Were that thread to panic, a bug, this
    /// commit gets [`Error::OutcomeUnknown`] and the engine stops, as if
    /// the disk had failed.
    pub fn commit(mut self) -> Result<()> {
        // Answers a commit with no writes, and saves a trip to the commit
        // thread once the engine stopped. The commit thread checks again,
        // and that check is the one that keeps writes from following a
        // failure.
        self.db.check_running()?;
        if self.writes.is_empty() {
            return Ok(());
        }
        let writes = std::mem::take(&mut self.writes);
        let begun_after = self.begun_after;
        self.db
            .ask(|answer| Request::Commit(Waiting::new(writes, begun_after, answer)))
    }

    /// Throws away the writes.
    pub fn rollback(self) {}
}

impl<F: Fs> Drop for Txn<'_, F> {
    /// Ends the transaction, committed or not, so the commits it began
    /// after no longer need keeping for conflicts with it.
    fn drop(&mut self) {
        self.db.shared.committed().recent.end(self.begun_after);
    }
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

/// Checks that a value of `len` bytes fits: at most [`MAX_VALUE_LEN`],
/// or [`Error::ValueTooLarge`]. [`Txn::put`] checks this itself; a caller
/// that wants to refuse a value before writing anything (the SQL layer,
/// which checks a whole statement first) calls it to apply the same rule.
pub fn check_value_len(len: usize) -> Result<()> {
    if len > MAX_VALUE_LEN {
        return Err(Error::ValueTooLarge {
            len,
            max: MAX_VALUE_LEN,
        });
    }
    Ok(())
}

/// Checks that `key` fits: at most [`MAX_KEY_LEN`] bytes, or
/// [`Error::KeyTooLarge`]. [`Txn::get`], [`Txn::put`] and [`Txn::delete`]
/// check this themselves; it is public so that a caller can apply the
/// same rule before writing anything.
pub fn check_key(key: &[u8]) -> Result<()> {
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

// Under `--cfg loom` the engine's locks are loom's, which panic outside a
// loom model: only the models in `commit_queue` run then.
#[cfg(all(test, not(loom)))]
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
