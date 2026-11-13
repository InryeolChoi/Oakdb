//! Random operations, applied both to the engine and to a `BTreeMap`, must
//! always give the same answers.
//!
//! Keys are long so the tree grows past two levels with a few hundred
//! entries: a 4 KB internal page holds only about ten of them. One test
//! mostly puts, so the tree grows four levels deep; the other deletes long
//! runs of keys, so leaves empty out and the root hands over to its last
//! child. Both also scan ranges, with each end included, excluded or
//! open, and between keys or on them. About one put in five writes a value
//! too long for its leaf, so overflow pages fill, are replaced, freed and
//! reused from the free list.
//!
//! Each test runs on the real file system and on the simulated one
//! (`oakdb::fs::sim`), which must give the same answers.
//!
//! Failing cases proptest finds are saved in `model.proptest-regressions`
//! beside this file and run first every time; the ones that taught us
//! something also get a named test.

use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};
use std::path::Path;

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, StdFs};
use oakdb::{Db, MAX_ENTRY_LEN, MAX_KEY_LEN};
use proptest::prelude::*;

/// How many different keys the operations use. Few enough that puts and
/// deletes hit the same keys again, enough for a four-level tree.
const KEYS: u16 = 1000;

/// Key `i`: sorts in the order of `i`, and is between 200 and 512 bytes
/// long so leaves and internal pages fill up at different rates.
fn key(i: u16) -> Vec<u8> {
    let mut key = i.to_be_bytes().to_vec();
    key.resize(200 + usize::from(i.wrapping_mul(7919) % 313), b'k');
    key
}

/// Longest part of a long value past what fits in its leaf: up to four
/// overflow pages.
const LONGEST_OVERFLOW: usize = 4 * 4056;

/// A value of `len` bytes that changes along its length, so a page of it
/// read in the wrong place shows.
fn value(len: usize, fill: u8) -> Vec<u8> {
    (0..len)
        .map(|i| fill.wrapping_add((i / 251) as u8))
        .collect()
}

#[derive(Debug, Clone)]
enum Op {
    /// Puts a value under key `key`: of `len` bytes cut down to what the
    /// key leaves room for in its leaf, or, if `long`, of that much plus
    /// `len`, which takes up to four overflow pages.
    Put {
        key: u16,
        len: u16,
        long: bool,
        fill: u8,
    },
    Delete {
        key: u16,
    },
    /// Deletes keys `from..from + count`, so whole leaves empty out.
    DeleteRun {
        from: u16,
        count: u16,
    },
    Get {
        key: u16,
    },
    /// Reads the first `take` entries of a range.
    Scan {
        from: Bound<Vec<u8>>,
        to: Bound<Vec<u8>>,
        take: u16,
    },
    Commit,
    Rollback,
    /// Throws away the open transaction and opens the database again.
    Reopen,
}

/// How often each operation comes, relative to the others.
struct Mix {
    put: u32,
    delete: u32,
    delete_run: u32,
    longest_run: u16,
    get: u32,
    scan: u32,
    commit: u32,
    /// Both rollbacks and reopens, which throw a transaction away.
    discard: u32,
}

/// Mostly puts, and four of five transactions commit.
const GROWING: Mix = Mix {
    put: 60,
    delete: 10,
    delete_run: 2,
    longest_run: 20,
    get: 15,
    scan: 4,
    commit: 8,
    discard: 1,
};

/// Long runs of deletes, often enough to empty most of the tree.
const SHRINKING: Mix = Mix {
    put: 24,
    delete: 6,
    delete_run: 1,
    longest_run: KEYS,
    get: 6,
    scan: 2,
    commit: 4,
    discard: 2,
};

/// One end of a range: open, or on key `i` or just after it (before key
/// `i + 1`), included or excluded.
fn bound() -> impl Strategy<Value = Bound<Vec<u8>>> {
    prop_oneof![
        1 => Just(Bound::Unbounded),
        4 => (0..KEYS, any::<bool>(), any::<bool>()).prop_map(|(i, after, included)| {
            let mut at = key(i);
            if after {
                at.push(0);
            }
            if included {
                Bound::Included(at)
            } else {
                Bound::Excluded(at)
            }
        }),
    ]
}

fn op(mix: Mix) -> impl Strategy<Value = Op> {
    prop_oneof![
        mix.put => (0..KEYS, any::<u16>(), prop::bool::weighted(0.2), any::<u8>())
            .prop_map(|(key, len, long, fill)| Op::Put { key, len, long, fill }),
        mix.delete => (0..KEYS).prop_map(|key| Op::Delete { key }),
        mix.delete_run => (0..KEYS, 1..=mix.longest_run)
            .prop_map(|(from, count)| Op::DeleteRun { from, count }),
        mix.get => (0..KEYS).prop_map(|key| Op::Get { key }),
        mix.scan => (bound(), bound(), 1..400u16)
            .prop_map(|(from, to, take)| Op::Scan { from, to, take }),
        mix.commit => Just(Op::Commit),
        mix.discard => Just(Op::Rollback),
        mix.discard => Just(Op::Reopen),
    ]
}

/// What the database should hold.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// Runs `ops` against a database in `dir` on `fs` and against the model,
/// checking every read. Ends by checking every key, before and after
/// reopening.
fn run<F: Fs + Clone>(fs: F, dir: &Path, ops: &[Op]) -> std::result::Result<(), TestCaseError> {
    let open = || Db::open_with(fs.clone(), dir).unwrap();
    let mut db = open();
    let mut committed = Model::new();
    let mut ops = ops.iter();
    loop {
        let mut pending = committed.clone();
        let mut reopen = false;
        let mut done = true;
        {
            let mut txn = db.begin();
            for op in ops.by_ref() {
                match *op {
                    Op::Put {
                        key: i,
                        len,
                        long,
                        fill,
                    } => {
                        let key = key(i);
                        let room = MAX_ENTRY_LEN - key.len();
                        let len = if long {
                            room + 1 + usize::from(len) % LONGEST_OVERFLOW
                        } else {
                            usize::from(len) % (room + 1)
                        };
                        let value = value(len, fill);
                        txn.put(&key, &value).unwrap();
                        pending.insert(key, value);
                    }
                    Op::Delete { key: i } => {
                        txn.delete(&key(i)).unwrap();
                        pending.remove(&key(i));
                    }
                    Op::DeleteRun { from, count } => {
                        for i in from..from.saturating_add(count).min(KEYS) {
                            txn.delete(&key(i)).unwrap();
                            pending.remove(&key(i));
                        }
                    }
                    Op::Scan {
                        ref from,
                        ref to,
                        take,
                    } => {
                        let got: Vec<(Vec<u8>, Vec<u8>)> = txn
                            .scan((from.clone(), to.clone()))
                            .take(usize::from(take))
                            .map(Result::unwrap)
                            .collect();
                        prop_assert_eq!(got, in_range(&pending, from, to, usize::from(take)));
                    }
                    Op::Get { key: i } => {
                        prop_assert_eq!(txn.get(&key(i)).unwrap(), pending.get(&key(i)).cloned());
                    }
                    Op::Commit => {
                        txn.commit().unwrap();
                        committed = pending;
                        done = false;
                        break;
                    }
                    Op::Rollback => {
                        done = false;
                        break;
                    }
                    Op::Reopen => {
                        reopen = true;
                        done = false;
                        break;
                    }
                }
            }
        }
        if reopen {
            db.close().unwrap();
            db = open();
        }
        if done {
            break;
        }
    }
    check_all(&db, &committed)?;
    db.close().unwrap();
    check_all(&open(), &committed)
}

/// The first `take` entries of `model` from `from` to `to`, in order.
fn in_range(
    model: &Model,
    from: &Bound<Vec<u8>>,
    to: &Bound<Vec<u8>>,
    take: usize,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    model
        .iter()
        .filter(|&(key, _)| (from.as_ref(), to.as_ref()).contains(key))
        .take(take)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn check_all<F: Fs>(db: &Db<F>, model: &Model) -> std::result::Result<(), TestCaseError> {
    let txn = db.begin();
    let scanned: Vec<(Vec<u8>, Vec<u8>)> = txn.scan::<&[u8]>(..).map(Result::unwrap).collect();
    prop_assert_eq!(
        scanned,
        in_range(model, &Bound::Unbounded, &Bound::Unbounded, usize::MAX)
    );
    for i in 0..KEYS {
        prop_assert_eq!(
            txn.get(&key(i)).unwrap(),
            model.get(&key(i)).cloned(),
            "key {}",
            i
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn a_growing_tree_answers_like_a_btreemap(ops in prop::collection::vec(op(GROWING), 1..3000)) {
        let dir = tempfile::tempdir().unwrap();
        run(StdFs, dir.path(), &ops)?;
    }

    #[test]
    fn a_shrinking_tree_answers_like_a_btreemap(ops in prop::collection::vec(op(SHRINKING), 1..1500)) {
        let dir = tempfile::tempdir().unwrap();
        run(StdFs, dir.path(), &ops)?;
    }

    #[test]
    fn a_growing_tree_answers_like_a_btreemap_on_the_simulated_disk(ops in prop::collection::vec(op(GROWING), 1..3000)) {
        run(SimFs::new(), Path::new("/oak"), &ops)?;
    }

    #[test]
    fn a_shrinking_tree_answers_like_a_btreemap_on_the_simulated_disk(ops in prop::collection::vec(op(SHRINKING), 1..1500)) {
        run(SimFs::new(), Path::new("/oak"), &ops)?;
    }
}

#[test]
fn keys_are_valid() {
    for i in 0..KEYS {
        assert!(key(i).len() <= MAX_KEY_LEN);
    }
}
