//! The power-cut experiment (issue #21), shared by the `crash_sim` example
//! and the regression tests in `tests/crash.rs`.
//!
//! Not implemented yet: the tests describe what it must do.

#![allow(dead_code)]

use std::collections::BTreeMap;

use oakdb::fs::sim::CrashReport;

/// Keys are `0..KEYS`, stored as 4 big-endian bytes.
pub const KEYS: u32 = 200;

/// What the database holds: key number → value.
pub type Model = BTreeMap<u32, Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Put(u32, Vec<u8>),
    Delete(u32),
}

/// Transactions to commit one after another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    pub commits: Vec<Vec<Op>>,
}

impl Workload {
    /// A random workload: up to 30 commits of up to 8 puts and deletes,
    /// some values long enough for overflow pages.
    pub fn generate(_seed: u64) -> Workload {
        todo!()
    }

    /// How many operations that change the disk opening a new database and
    /// committing every transaction takes.
    pub fn total_ops(&self) -> u64 {
        todo!()
    }
}

/// Everything one run needs, made from one seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    pub seed: u64,
    pub workload: Workload,
    /// The power goes after this many operations that change the disk.
    pub cut_after: u64,
    /// Decides what of the unsynced work survives the crash.
    pub crash_seed: u64,
}

impl Case {
    pub fn from_seed(_seed: u64) -> Case {
        todo!()
    }
}

/// How a reopened database compares with what was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every acknowledged commit is there, and the commit the power cut
    /// short is either all there (`true`) or not at all (`false`).
    Intact { cut_commit_landed: bool },
    /// Reopening failed.
    OpenFailed(String),
    /// Reopening worked, but reading failed or scan and get disagree.
    ReadFailed(String),
    /// Keys an acknowledged commit wrote are missing or old.
    LostAcknowledged(Vec<u32>),
    /// Keys hold a value no commit ever gave them.
    WrongValue(Vec<u32>),
    /// The commit the power cut short is there for some keys and not
    /// others.
    HalfCommit(Vec<u32>),
}

impl Verdict {
    /// Whether the database kept its promise.
    pub fn is_intact(&self) -> bool {
        matches!(self, Verdict::Intact { .. })
    }
}

/// Compares `after`, what the reopened database holds, with `acked`, the
/// state after the last acknowledged commit, and `cut`, the state if the
/// commit the power cut short had landed (`None` if no commit was cut).
pub fn judge(_after: &Model, _acked: &Model, _cut: Option<&Model>) -> Verdict {
    todo!()
}

/// What happened in one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Commits acknowledged before the power went.
    pub acked: usize,
    /// The commit the power cut short, or `None` if it went while opening
    /// or not at all.
    pub cut_commit: Option<usize>,
    pub report: CrashReport,
    pub verdict: Verdict,
}

/// Opens a new database on a simulated disk, cuts the power after
/// `cut_after` operations that change the disk, commits `workload` until
/// then, crashes with `crash_seed`, reopens and judges.
pub fn run(_workload: &Workload, _cut_after: u64, _crash_seed: u64) -> Run {
    todo!()
}
