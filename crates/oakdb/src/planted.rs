//! Bugs a test can plant in the engine, to see the history checker find
//! them (issue #29, design 0024 V6, V7). They exist only with the
//! `planted-bugs` feature, and even then a database has none unless it is
//! opened with `Db::open_with_bug`.

/// A bug `Db::open_with_bug` plants, and what the history checker should
/// name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlantedBug {
    /// Every commit passes the conflict check: lost updates.
    SkipConflictCheck,
    /// Reads and scans read the latest tree, not the snapshot:
    /// non-repeatable reads and read skew.
    ReadLatest,
    /// Only scans read the latest tree.
    ScanLatest,
    /// A batch shows before its sync: if the sync fails, others read what
    /// is not there once the database is opened again (aborted reads).
    PublishBeforeSync,
    /// A commit is not checked against those ahead in its batch: lost
    /// updates, when two writes of one key share a batch.
    SkipInBatchConflict,
    /// A batch's keys are recorded, and its tree swapped in, under two
    /// locks: a transaction begun in between has the new commit numbers
    /// but the old tree, so overwrites without a conflict (lost updates).
    SplitPublish,
    /// Commits are answered before their batch shows: a thread's next
    /// transaction may not see its own commit (stale reads).
    AnswerBeforePublish,
}
