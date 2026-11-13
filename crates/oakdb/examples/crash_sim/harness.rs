//! The power-cut experiment (issue #21), shared by the `crash_sim` example
//! and the regression tests in `tests/crash.rs`.
//!
//! One seed makes a [`Case`]: a random workload, the point where the power
//! goes, and the seed that decides what of the unsynced work survives.
//! [`run`] plays it on a simulated disk and [`judge`]s the reopened
//! database against the promise: every acknowledged commit is there, and
//! the commit the power cut short is there completely or not at all. It
//! uses only the engine's public API.

// Each user (the example, the tests) uses a different part.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use oakdb::fs::sim::{CrashReport, SimFs};
use oakdb::Db;

/// Where the database lives on the simulated disk.
pub const DIR: &str = "/oak";

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
    pub fn generate(seed: u64) -> Workload {
        let mut rng = Rng(seed);
        let commits = (0..1 + rng.below(30))
            .map(|_| {
                (0..1 + rng.below(8))
                    .map(|_| {
                        let key = rng.below(u64::from(KEYS)) as u32;
                        if rng.below(5) == 0 {
                            return Op::Delete(key);
                        }
                        let len = if rng.below(6) == 0 {
                            1500 + rng.below(12_000)
                        } else {
                            rng.below(400)
                        };
                        Op::Put(key, value(len as usize, rng.next() as u8))
                    })
                    .collect()
            })
            .collect();
        Workload { commits }
    }

    /// How many operations that change the disk opening a new database and
    /// committing every transaction takes.
    pub fn total_ops(&self) -> u64 {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), DIR).unwrap();
        for ops in &self.commits {
            let mut txn = db.begin();
            for op in ops {
                op.apply_to(&mut txn);
            }
            txn.commit().unwrap();
        }
        fs.ops()
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
    pub fn from_seed(seed: u64) -> Case {
        let mut rng = Rng(seed);
        let workload = Workload::generate(rng.next());
        let cut_after = rng.below(workload.total_ops());
        Case {
            seed,
            workload,
            cut_after,
            crash_seed: rng.next(),
        }
    }

    pub fn run(&self) -> Run {
        run(&self.workload, self.cut_after, self.crash_seed)
    }
}

impl Op {
    fn apply_to(&self, txn: &mut oakdb::Txn<'_, SimFs>) {
        match self {
            Op::Put(key, value) => txn.put(&key.to_be_bytes(), value).unwrap(),
            Op::Delete(key) => txn.delete(&key.to_be_bytes()).unwrap(),
        }
    }

    fn apply_to_model(&self, model: &mut Model) {
        match self {
            Op::Put(key, value) => model.insert(*key, value.clone()),
            Op::Delete(key) => model.remove(key),
        };
    }
}

/// A value of `len` bytes that changes along its length, so a sector of
/// it in the wrong place shows.
fn value(len: usize, fill: u8) -> Vec<u8> {
    (0..len).map(|i| fill.wrapping_add((i / 7) as u8)).collect()
}

/// How a reopened database compares with what was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every acknowledged commit is there, and the commit the power cut
    /// short is either all there (`true`) or not at all (`false`).
    Intact { cut_commit_landed: bool },
    /// Reopening failed.
    OpenFailed(String),
    /// Reopening or reading panicked: worse than an error, the program
    /// using the database would stop.
    Panicked(String),
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
///
/// A key the cut commit did not touch must hold its acknowledged value;
/// otherwise an acknowledged commit was lost. A key it touched must hold
/// the old or the new value; otherwise the value is wrong. If all that
/// holds but the keys are not all old or all new, the commit landed in
/// half. The worst finding wins.
pub fn judge(after: &Model, acked: &Model, cut: Option<&Model>) -> Verdict {
    if after == acked {
        return Verdict::Intact {
            cut_commit_landed: false,
        };
    }
    let cut = match cut {
        Some(cut) if after == cut => {
            return Verdict::Intact {
                cut_commit_landed: true,
            }
        }
        Some(cut) => cut,
        None => acked,
    };
    let keys: BTreeSet<u32> = [acked, cut, after]
        .iter()
        .flat_map(|m| m.keys().copied())
        .collect();
    let (mut lost, mut wrong, mut touched) = (Vec::new(), Vec::new(), Vec::new());
    for key in keys {
        let (old, new, now) = (acked.get(&key), cut.get(&key), after.get(&key));
        if old == new {
            if now != old {
                lost.push(key);
            }
        } else if now != old && now != new {
            wrong.push(key);
        } else {
            touched.push(key);
        }
    }
    if !lost.is_empty() {
        Verdict::LostAcknowledged(lost)
    } else if !wrong.is_empty() {
        Verdict::WrongValue(wrong)
    } else {
        Verdict::HalfCommit(touched)
    }
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
    /// What the acknowledged commits left, what the cut commit would have
    /// left, and what the reopened database holds (if it could be read).
    pub acked_state: Model,
    pub cut_state: Option<Model>,
    pub after: Option<Model>,
}

/// Opens a new database on a simulated disk, cuts the power after
/// `cut_after` operations that change the disk, commits `workload` until
/// then, crashes with `crash_seed`, reopens and judges.
pub fn run(workload: &Workload, cut_after: u64, crash_seed: u64) -> Run {
    let fs = SimFs::new();
    fs.fail_after(cut_after);
    let mut acked = Model::new();
    let mut acked_commits = 0;
    let mut cut = None;
    if let Ok(db) = Db::open_with(fs.clone(), DIR) {
        for (i, ops) in workload.commits.iter().enumerate() {
            let mut next = acked.clone();
            let mut txn = db.begin();
            for op in ops {
                op.apply_to(&mut txn);
                op.apply_to_model(&mut next);
            }
            if txn.commit().is_ok() {
                acked = next;
                acked_commits += 1;
            } else {
                cut = Some((i, next));
                break;
            }
        }
    }
    let report = fs.crash(crash_seed);
    let reopen = || match Db::open_with(fs.clone(), DIR) {
        Err(e) => (Verdict::OpenFailed(e.to_string()), None),
        Ok(db) => match read_all(&db) {
            Err(message) => (Verdict::ReadFailed(message), None),
            Ok(after) => (
                judge(&after, &acked, cut.as_ref().map(|(_, m)| m)),
                Some(after),
            ),
        },
    };
    // Nothing is shared with the code that panics but the simulated disk,
    // which a panic leaves whole.
    let (verdict, after) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(reopen))
        .unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            (Verdict::Panicked(message), None)
        });
    let (cut_commit, cut_state) = cut.unzip();
    Run {
        acked: acked_commits,
        cut_commit,
        report,
        verdict,
        acked_state: acked,
        cut_state,
        after,
    }
}

/// Everything the database holds, read both by scanning and key by key.
fn read_all(db: &Db<SimFs>) -> Result<Model, String> {
    let txn = db.begin();
    let mut scanned = Model::new();
    for entry in txn.scan::<&[u8]>(..) {
        let (key, value) = entry.map_err(|e| e.to_string())?;
        let key: [u8; 4] = key
            .as_slice()
            .try_into()
            .map_err(|_| format!("a key of {} bytes", key.len()))?;
        scanned.insert(u32::from_be_bytes(key), value);
    }
    let mut got = Model::new();
    for key in 0..KEYS {
        if let Some(value) = txn.get(&key.to_be_bytes()).map_err(|e| e.to_string())? {
            got.insert(key, value);
        }
    }
    if scanned != got {
        return Err("scan and get disagree".into());
    }
    Ok(got)
}

/// SplitMix64, as in the simulated disk, so a seed means the same case
/// whatever crate versions are in use.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
