//! Write conflicts (issue #26, design 0016, ADR 0006): of two transactions
//! that write the same key, the one that commits later fails with
//! [`Error::Conflict`] (first committer wins). Only writes count: reads
//! and scans never conflict, so write skew is allowed (snapshot
//! isolation).

use std::collections::BTreeMap;

use oakdb::fs::sim::SimFs;
use oakdb::{Db, Error};
use proptest::prelude::*;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn open() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/oak").unwrap()
}

fn put(db: &Db<SimFs>, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn get(db: &Db<SimFs>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

fn is_conflict_on(result: Result<(), Error>, key: &[u8]) -> bool {
    matches!(result, Err(Error::Conflict { key: k }) if k == key)
}

#[test]
fn of_two_transactions_writing_the_same_key_the_later_commit_fails() {
    let db = open();
    put(&db, b"k", b"0");
    let mut first = db.begin();
    let mut second = db.begin();
    first.put(b"k", b"first").unwrap();
    second.put(b"k", b"second").unwrap();

    first.commit().unwrap();
    let result = second.commit();

    assert!(is_conflict_on(result, b"k"));
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"first"[..]));
}

#[test]
fn transactions_writing_different_keys_both_commit() {
    let db = open();
    let mut first = db.begin();
    let mut second = db.begin();
    first.put(b"a", b"1").unwrap();
    second.put(b"b", b"2").unwrap();

    first.commit().unwrap();
    second.commit().unwrap();

    assert_eq!(get(&db, b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&db, b"b").as_deref(), Some(&b"2"[..]));
}

#[test]
fn a_transaction_that_lost_can_retry_and_commit() {
    let db = open();
    let mut first = db.begin();
    let mut second = db.begin();
    first.put(b"counter", b"1").unwrap();
    second.put(b"counter", b"1").unwrap();
    first.commit().unwrap();
    assert!(is_conflict_on(second.commit(), b"counter"));

    // The retry begins after the winner, reads its value and builds on it.
    let mut retry = db.begin();
    assert_eq!(retry.get(b"counter").unwrap().as_deref(), Some(&b"1"[..]));
    retry.put(b"counter", b"2").unwrap();
    retry.commit().unwrap();
    assert_eq!(get(&db, b"counter").as_deref(), Some(&b"2"[..]));
}

#[test]
fn a_delete_conflicts_with_a_put_of_the_same_key_either_way() {
    for put_first in [true, false] {
        let db = open();
        put(&db, b"k", b"0");
        let mut putter = db.begin();
        let mut deleter = db.begin();
        putter.put(b"k", b"1").unwrap();
        deleter.delete(b"k").unwrap();
        let (winner, loser) = if put_first {
            (putter, deleter)
        } else {
            (deleter, putter)
        };
        winner.commit().unwrap();
        assert!(
            is_conflict_on(loser.commit(), b"k"),
            "put first: {put_first}"
        );
    }
}

#[test]
fn a_commit_before_a_transaction_began_does_not_conflict_with_it() {
    let db = open();
    let mut earlier = db.begin();
    earlier.put(b"k", b"earlier").unwrap();
    earlier.commit().unwrap();

    let mut later = db.begin();
    later.put(b"k", b"later").unwrap();
    later.commit().unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"later"[..]));
}

#[test]
fn a_conflicting_commit_writes_nothing_and_the_engine_goes_on() {
    let db = open();
    let mut first = db.begin();
    let mut second = db.begin();
    first.put(b"shared", b"first").unwrap();
    second.put(b"own", b"second").unwrap();
    second.put(b"shared", b"second").unwrap();
    first.commit().unwrap();

    assert!(is_conflict_on(second.commit(), b"shared"));

    // None of the losing commit's writes are there, not even to the key
    // no one else wrote, and the engine takes more commits.
    assert_eq!(get(&db, b"own"), None);
    put(&db, b"after", b"ok");
    assert_eq!(get(&db, b"after").as_deref(), Some(&b"ok"[..]));
}

#[test]
fn reading_a_key_another_transaction_changed_is_no_conflict() {
    let db = open();
    put(&db, b"k", b"0");
    let mut reader = db.begin();
    assert_eq!(reader.get(b"k").unwrap().as_deref(), Some(&b"0"[..]));
    let _ = reader.scan::<&[u8]>(..).count();
    put(&db, b"k", b"1");

    reader.put(b"other", b"x").unwrap();
    reader.commit().unwrap();
}

#[test]
fn write_skew_is_allowed() {
    // The rule "at least one of x and y is 1" holds before. Each
    // transaction checks it on its snapshot and sets a different key to
    // 0. Neither writes what the other writes, so both commit, and the
    // rule is broken: snapshot isolation allows this (ADR 0006).
    let db = open();
    put(&db, b"x", b"1");
    put(&db, b"y", b"1");
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    let ones = |txn: &oakdb::Txn<'_, SimFs>| {
        [b"x", b"y"]
            .iter()
            .filter(|k| txn.get(&k[..]).unwrap().as_deref() == Some(&b"1"[..]))
            .count()
    };
    assert_eq!(ones(&t1), 2);
    assert_eq!(ones(&t2), 2);
    t1.put(b"x", b"0").unwrap();
    t2.put(b"y", b"0").unwrap();

    t1.commit().unwrap();
    t2.commit().unwrap();

    assert_eq!(get(&db, b"x").as_deref(), Some(&b"0"[..]));
    assert_eq!(get(&db, b"y").as_deref(), Some(&b"0"[..]));
}

#[test]
fn a_write_another_transaction_undid_still_conflicts() {
    // A→B→A: the value is back where the snapshot saw it, but two commits
    // wrote the key after it began. Comparing values would miss this.
    let db = open();
    put(&db, b"k", b"A");
    let mut late = db.begin();
    put(&db, b"k", b"B");
    put(&db, b"k", b"A");
    late.put(b"k", b"C").unwrap();
    assert!(is_conflict_on(late.commit(), b"k"));
}

#[test]
fn a_conflict_holds_through_checkpoints() {
    let db = open();
    let mut late = db.begin();
    put(&db, b"k", b"winner");
    db.checkpoint().unwrap();
    late.put(b"k", b"late").unwrap();
    assert!(is_conflict_on(late.commit(), b"k"));
}

/// What a test transaction does: its writes, and the commit number of the
/// database model when it began.
struct Open<'db> {
    txn: oakdb::Txn<'db, SimFs>,
    begun_after: usize,
    keys: Vec<Vec<u8>>,
    writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

#[derive(Debug, Clone)]
enum Step {
    Begin,
    /// Put (`Some`) or delete under key `k` in open transaction `i % n`.
    Write(usize, u8, Option<u8>),
    /// Commit open transaction `i % n`.
    Commit(usize),
    /// Roll back open transaction `i % n`.
    Rollback(usize),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        2 => Just(Step::Begin),
        5 => (any::<usize>(), 0u8..12, prop::option::of(any::<u8>())).prop_map(|(i, k, v)| Step::Write(i, k, v)),
        3 => any::<usize>().prop_map(Step::Commit),
        1 => any::<usize>().prop_map(Step::Rollback),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Open transactions interleave in one thread. Each commit fails with
    /// a conflict exactly when a commit since its transaction began wrote
    /// one of its keys; otherwise its writes land. The database always
    /// holds what the model of successful commits says.
    #[test]
    fn a_commit_fails_exactly_when_a_later_commit_wrote_one_of_its_keys(steps in prop::collection::vec(step(), 1..80)) {
        let db = open();
        let mut model = Model::new();
        // For each successful commit, in order, the keys it wrote.
        let mut history: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut open: Vec<Open<'_>> = Vec::new();
        for step in steps {
            match step {
                Step::Begin => open.push(Open { txn: db.begin(), begun_after: history.len(), keys: Vec::new(), writes: Vec::new() }),
                Step::Write(_, _, _) | Step::Commit(_) | Step::Rollback(_) if open.is_empty() => {}
                Step::Write(i, k, v) => {
                    let n = open.len();
                    let t = &mut open[i % n];
                    let key = vec![b'k', k];
                    let value = v.map(|v| vec![v; 1 + usize::from(v) * 7]);
                    match &value {
                        Some(value) => t.txn.put(&key, value).unwrap(),
                        None => t.txn.delete(&key).unwrap(),
                    }
                    t.keys.push(key.clone());
                    t.writes.push((key, value));
                }
                Step::Rollback(i) => {
                    let n = open.len();
                    open.remove(i % n).txn.rollback();
                }
                Step::Commit(i) => {
                    let n = open.len();
                    let t = open.remove(i % n);
                    let lost_to = history[t.begun_after..]
                        .iter()
                        .flatten()
                        .find(|written| t.keys.contains(written))
                        .cloned();
                    let result = t.txn.commit();
                    match lost_to {
                        Some(_) => prop_assert!(matches!(result, Err(Error::Conflict { ref key }) if t.keys.contains(key)), "{:?}", result),
                        None => {
                            prop_assert!(result.is_ok(), "{:?}", result);
                            if !t.writes.is_empty() {
                                for (key, value) in &t.writes {
                                    match value {
                                        Some(value) => model.insert(key.clone(), value.clone()),
                                        None => model.remove(key),
                                    };
                                }
                                history.push(t.keys.clone());
                            }
                        }
                    }
                }
            }
        }
        drop(open);
        let all: Model = db.begin().scan::<&[u8]>(..).map(Result::unwrap).collect();
        prop_assert_eq!(all, model);
    }
}
