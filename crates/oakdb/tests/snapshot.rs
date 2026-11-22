//! Snapshot reads (issue #25, design 0015, ADR 0006): a transaction reads
//! the database as it was committed when the transaction began, whatever
//! is committed after, and sees its own writes on top.

use std::collections::BTreeMap;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use proptest::prelude::*;

/// What the database holds: key → value.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn open() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/oak").unwrap()
}

fn put(db: &Db<SimFs>, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn delete(db: &Db<SimFs>, key: &[u8]) {
    let mut txn = db.begin();
    txn.delete(key).unwrap();
    txn.commit().unwrap();
}

/// Every entry a transaction's scan yields.
fn scan_all(txn: &oakdb::Txn<'_, SimFs>) -> Model {
    txn.scan::<&[u8]>(..).map(Result::unwrap).collect()
}

#[test]
fn a_transaction_does_not_see_a_commit_made_after_it_began() {
    let db = open();
    put(&db, b"k", b"old");
    let txn = db.begin();

    put(&db, b"k", b"new");
    put(&db, b"added", b"later");
    delete(&db, b"k");

    assert_eq!(txn.get(b"k").unwrap().as_deref(), Some(&b"old"[..]));
    assert_eq!(txn.get(b"added").unwrap(), None);
    assert_eq!(
        scan_all(&txn),
        Model::from([(b"k".to_vec(), b"old".to_vec())])
    );
    // A transaction begun now sees the commits.
    let now = db.begin();
    assert_eq!(now.get(b"k").unwrap(), None);
    assert_eq!(now.get(b"added").unwrap().as_deref(), Some(&b"later"[..]));
}

#[test]
fn a_transaction_sees_its_own_writes_over_its_snapshot() {
    let db = open();
    put(&db, b"a", b"1");
    put(&db, b"b", b"2");
    let mut txn = db.begin();
    put(&db, b"c", b"3");

    txn.put(b"a", b"mine").unwrap();
    txn.delete(b"b").unwrap();

    assert_eq!(txn.get(b"a").unwrap().as_deref(), Some(&b"mine"[..]));
    assert_eq!(txn.get(b"b").unwrap(), None);
    assert_eq!(txn.get(b"c").unwrap(), None);
    assert_eq!(
        scan_all(&txn),
        Model::from([(b"a".to_vec(), b"mine".to_vec())])
    );
}

#[test]
fn a_long_value_replaced_after_a_transaction_began_reads_whole_from_its_snapshot() {
    // A value on overflow pages: the replacement frees those pages and a
    // later commit may reuse them. The snapshot keeps its own.
    let db = open();
    let old = vec![1; 30_000];
    put(&db, b"long", &old);
    let txn = db.begin();

    put(&db, b"long", &[2; 30_000]);
    put(&db, b"other", &[3; 30_000]);

    assert_eq!(txn.get(b"long").unwrap(), Some(old.clone()));
    assert_eq!(scan_all(&txn).get(&b"long"[..]), Some(&old));
}

#[test]
fn a_snapshot_holds_through_checkpoints() {
    // A checkpoint writes pages over the data file in place; the snapshot
    // is in memory and does not change.
    let db = open();
    let mut model = Model::new();
    for i in 0..300u32 {
        let (key, value) = (i.to_be_bytes().to_vec(), vec![i as u8; 200]);
        put(&db, &key, &value);
        model.insert(key, value);
    }
    let txn = db.begin();
    for i in 0..300u32 {
        put(&db, &i.to_be_bytes(), &[0xEE; 500]);
        if i % 50 == 0 {
            db.checkpoint().unwrap();
        }
    }
    db.checkpoint().unwrap();

    assert_eq!(scan_all(&txn), model);
}

#[test]
fn a_scan_started_later_in_a_transaction_still_reads_its_snapshot() {
    let db = open();
    put(&db, b"a", b"1");
    let txn = db.begin();
    let before = scan_all(&txn);
    put(&db, b"b", b"2");
    assert_eq!(scan_all(&txn), before);
}

#[test]
fn a_rolled_back_or_dropped_transaction_leaves_no_trace() {
    let db = open();
    put(&db, b"k", b"kept");
    let mut rolled_back = db.begin();
    rolled_back.put(b"k", b"gone").unwrap();
    rolled_back.put(b"new", b"gone").unwrap();
    rolled_back.rollback();
    {
        let mut dropped = db.begin();
        dropped.delete(b"k").unwrap();
    }
    let txn = db.begin();
    assert_eq!(
        scan_all(&txn),
        Model::from([(b"k".to_vec(), b"kept".to_vec())])
    );
}

/// One step of a history: a commit of puts and deletes, or a new
/// transaction that makes its own puts and deletes and stays open.
#[derive(Debug, Clone)]
enum Step {
    Commit(Vec<(u8, Option<u8>)>),
    Begin(Vec<(u8, Option<u8>)>),
}

/// Puts (`Some`) and deletes (`None`) under the keys the numbers stand for.
fn writes() -> impl Strategy<Value = Vec<(u8, Option<u8>)>> {
    prop::collection::vec((0u8..40, prop::option::of(any::<u8>())), 0..6)
}

/// Makes `writes` in `txn` and in `model`.
fn write(txn: &mut oakdb::Txn<'_, SimFs>, model: &mut Model, writes: Vec<(u8, Option<u8>)>) {
    for (k, v) in writes {
        match v {
            Some(v) => {
                txn.put(&key_of(k), &value_of(v)).unwrap();
                model.insert(key_of(k), value_of(v));
            }
            None => {
                txn.delete(&key_of(k)).unwrap();
                model.remove(&key_of(k));
            }
        }
    }
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => writes().prop_map(Step::Commit),
        1 => writes().prop_map(Step::Begin),
    ]
}

/// The key and value bytes a step's numbers stand for. Values vary in
/// length so some go to overflow pages.
fn key_of(k: u8) -> Vec<u8> {
    vec![b'k', k]
}

fn value_of(v: u8) -> Vec<u8> {
    vec![v; 1 + usize::from(v) * 97]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every transaction, read at the end of a random history, sees
    /// exactly what was committed when it began, with its own writes on
    /// top.
    #[test]
    fn every_snapshot_reads_the_database_as_of_its_beginning(steps in prop::collection::vec(step(), 1..60)) {
        let db = open();
        let mut model = Model::new();
        let mut snapshots = Vec::new();
        for step in steps {
            match step {
                Step::Begin(own) => {
                    let mut txn = db.begin();
                    let mut seen = model.clone();
                    write(&mut txn, &mut seen, own);
                    snapshots.push((txn, seen));
                }
                Step::Commit(writes) => {
                    let mut txn = db.begin();
                    write(&mut txn, &mut model, writes);
                    txn.commit().unwrap();
                }
            }
        }
        for (i, (txn, seen)) in snapshots.iter().enumerate() {
            prop_assert_eq!(&scan_all(txn), seen, "snapshot {}: scan", i);
            for k in 0..40 {
                let got = txn.get(&key_of(k)).unwrap();
                prop_assert_eq!(got.as_ref(), seen.get(&key_of(k)), "snapshot {}: get {}", i, k);
            }
        }
    }
}
