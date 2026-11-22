//! Blind tests for snapshot reads (issue #25, parent #12 stories 8-13,
//! design 0015, ADR 0006), written from the spec alone by a tester who
//! did not read the implementation (PR #61's verification).

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::path::PathBuf;

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Txn, CHECKPOINT_AFTER, DATA_FILE};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn dir() -> PathBuf {
    PathBuf::from("/oak")
}

fn sim() -> (SimFs, Db<SimFs>) {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    (fs, db)
}

fn put<F: Fs>(db: &Db<F>, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn del<F: Fs>(db: &Db<F>, key: &[u8]) {
    let mut txn = db.begin();
    txn.delete(key).unwrap();
    txn.commit().unwrap();
}

fn all<F: Fs>(txn: &Txn<'_, F>) -> Vec<(Vec<u8>, Vec<u8>)> {
    txn.scan::<&[u8]>(..).map(Result::unwrap).collect()
}

fn keys_in<F: Fs>(txn: &Txn<'_, F>, from: &str, to: &str) -> Vec<String> {
    txn.scan(from..to)
        .map(|r| String::from_utf8(r.unwrap().0).unwrap())
        .collect()
}

fn model_items(model: &Model) -> Vec<(Vec<u8>, Vec<u8>)> {
    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn key(i: u32) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

fn long(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 251) as u8))
        .collect()
}

fn base(db: &Db<SimFs>) {
    let mut t = db.begin();
    for k in ["b", "d", "f", "h"] {
        t.put(k.as_bytes(), format!("old-{k}").as_bytes()).unwrap();
    }
    t.commit().unwrap();
}

#[test]
fn get_does_not_see_an_overwrite_committed_after_begin() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    put(&db, b"d", b"new");
    assert_eq!(snap.get(b"d").unwrap(), Some(b"old-d".to_vec()));
    assert_eq!(db.begin().get(b"d").unwrap(), Some(b"new".to_vec()));
}

#[test]
fn get_does_not_see_an_insert_committed_after_begin() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    put(&db, b"e", b"inserted");
    assert_eq!(snap.get(b"e").unwrap(), None);
    assert_eq!(db.begin().get(b"e").unwrap(), Some(b"inserted".to_vec()));
}

#[test]
fn get_still_sees_a_key_deleted_after_begin() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    del(&db, b"f");
    assert_eq!(snap.get(b"f").unwrap(), Some(b"old-f".to_vec()));
    assert_eq!(db.begin().get(b"f").unwrap(), None);
}

#[test]
fn a_snapshot_of_an_empty_database_stays_empty() {
    let (_fs, db) = sim();
    let snap = db.begin();
    put(&db, b"a", b"1");
    assert_eq!(snap.get(b"a").unwrap(), None);
    assert!(all(&snap).is_empty());
}

#[test]
fn full_scan_ignores_inserts_overwrites_and_deletes_after_begin() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    {
        let mut t = db.begin();
        t.put(b"a", b"new-a").unwrap();
        t.put(b"d", b"new-d").unwrap();
        t.delete(b"f").unwrap();
        t.put(b"z", b"new-z").unwrap();
        t.commit().unwrap();
    }
    let expected: Vec<_> = ["b", "d", "f", "h"]
        .iter()
        .map(|k| (k.as_bytes().to_vec(), format!("old-{k}").into_bytes()))
        .collect();
    assert_eq!(all(&snap), expected);
    let now: Vec<_> = all(&db.begin()).into_iter().map(|(k, _)| k).collect();
    assert_eq!(now, [b"a", b"b", b"d", b"h", b"z"].map(|k| k.to_vec()));
}

#[test]
fn range_scans_with_every_bound_kind_use_the_snapshot() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    {
        let mut t = db.begin();
        t.put(b"c", b"x").unwrap();
        t.put(b"e", b"x").unwrap();
        t.delete(b"d").unwrap();
        t.delete(b"h").unwrap();
        t.commit().unwrap();
    }
    let k = |s: oakdb::Scan<'_>| -> Vec<String> {
        s.map(|r| String::from_utf8(r.unwrap().0).unwrap())
            .collect()
    };
    assert_eq!(keys_in(&snap, "c", "g"), ["d", "f"]);
    assert_eq!(k(snap.scan("d"..="h")), ["d", "f", "h"]);
    assert_eq!(k(snap.scan("d"..)), ["d", "f", "h"]);
    assert_eq!(k(snap.scan(..="d")), ["b", "d"]);
    assert_eq!(
        k(snap.scan::<&str>((Excluded("b"), Included("h")))),
        ["d", "f", "h"]
    );
    assert_eq!(k(snap.scan::<&str>((Excluded("d"), Unbounded))), ["f", "h"]);
    // A new transaction sees the new state.
    assert_eq!(keys_in(&db.begin(), "c", "g"), ["c", "e", "f"]);
}

#[test]
fn a_scan_started_late_in_a_transaction_still_reads_the_snapshot() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    assert_eq!(snap.get(b"b").unwrap(), Some(b"old-b".to_vec()));
    for i in 0..20u32 {
        put(&db, format!("c{i:02}").as_bytes(), b"later");
    }
    del(&db, b"b");
    assert_eq!(keys_in(&snap, "a", "z"), ["b", "d", "f", "h"]);
}

#[test]
fn a_scan_iterator_created_before_a_commit_is_unaffected_by_it() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    let mut it = snap.scan::<&[u8]>(..);
    let first = it.next().unwrap().unwrap();
    put(&db, b"c", b"new");
    del(&db, b"h");
    let rest: Vec<Vec<u8>> = it.map(|r| r.unwrap().0).collect();
    assert_eq!(first.0, b"b");
    assert_eq!(rest, [b"d", b"f", b"h"].map(|k| k.to_vec()));
}

#[test]
fn own_writes_are_visible_and_others_commits_are_not() {
    let (_fs, db) = sim();
    base(&db);
    let mut me = db.begin();
    me.put(b"d", b"mine-d").unwrap();
    me.put(b"e", b"mine-e").unwrap();
    me.delete(b"h").unwrap();
    {
        let mut other = db.begin();
        other.put(b"c", b"other-c").unwrap();
        other.put(b"f", b"other-f").unwrap();
        other.commit().unwrap();
    }
    assert_eq!(me.get(b"d").unwrap(), Some(b"mine-d".to_vec()));
    assert_eq!(me.get(b"e").unwrap(), Some(b"mine-e".to_vec()));
    assert_eq!(me.get(b"h").unwrap(), None);
    assert_eq!(me.get(b"c").unwrap(), None);
    assert_eq!(me.get(b"f").unwrap(), Some(b"old-f".to_vec()));
    assert_eq!(
        all(&me),
        vec![
            (b"b".to_vec(), b"old-b".to_vec()),
            (b"d".to_vec(), b"mine-d".to_vec()),
            (b"e".to_vec(), b"mine-e".to_vec()),
            (b"f".to_vec(), b"old-f".to_vec()),
        ]
    );
}

#[test]
fn own_writes_overlay_a_range_scan_at_its_bounds() {
    let (_fs, db) = sim();
    base(&db);
    let mut me = db.begin();
    me.put(b"a", b"1").unwrap(); // below range
    me.put(b"c", b"1").unwrap(); // lower bound, included
    me.delete(b"d").unwrap();
    me.put(b"g", b"1").unwrap(); // upper bound, excluded
    me.put(b"e", b"1").unwrap();
    me.delete(b"e").unwrap(); // put then delete: gone
    me.delete(b"f").unwrap();
    me.put(b"f", b"again").unwrap(); // delete then put: present
    put(&db, b"ca", b"other"); // someone else's commit, invisible
    assert_eq!(keys_in(&me, "c", "g"), ["c", "f"]);
    assert_eq!(me.get(b"f").unwrap(), Some(b"again".to_vec()));
    assert_eq!(me.get(b"e").unwrap(), None);
}

#[test]
fn writes_are_invisible_to_others_until_commit_then_only_to_later_txns() {
    let (_fs, db) = sim();
    base(&db);
    let before = db.begin();
    let mut w = db.begin();
    w.put(b"x", b"1").unwrap();
    w.delete(b"b").unwrap();
    let during = db.begin();
    assert_eq!(during.get(b"x").unwrap(), None);
    assert_eq!(during.get(b"b").unwrap(), Some(b"old-b".to_vec()));
    w.commit().unwrap();
    let after = db.begin();
    assert_eq!(before.get(b"x").unwrap(), None);
    assert_eq!(during.get(b"x").unwrap(), None);
    assert_eq!(during.get(b"b").unwrap(), Some(b"old-b".to_vec()));
    assert_eq!(after.get(b"x").unwrap(), Some(b"1".to_vec()));
    assert_eq!(after.get(b"b").unwrap(), None);
}

#[test]
fn rollback_leaves_no_trace() {
    let (_fs, db) = sim();
    base(&db);
    let mut t = db.begin();
    t.put(b"a", b"x").unwrap();
    t.put(b"d", b"x").unwrap();
    t.delete(b"f").unwrap();
    t.rollback();
    let r = db.begin();
    assert_eq!(r.get(b"a").unwrap(), None);
    assert_eq!(r.get(b"d").unwrap(), Some(b"old-d".to_vec()));
    assert_eq!(r.get(b"f").unwrap(), Some(b"old-f".to_vec()));
    assert_eq!(all(&r).len(), 4);
}

#[test]
fn dropping_without_commit_leaves_no_trace_even_across_reopen() {
    let fs = SimFs::new();
    {
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        base(&db);
        {
            let mut t = db.begin();
            t.put(b"a", &long(1, 10_000)).unwrap();
            t.delete(b"b").unwrap();
            // dropped here
        }
        assert_eq!(db.begin().get(b"a").unwrap(), None);
        assert_eq!(db.begin().get(b"b").unwrap(), Some(b"old-b".to_vec()));
        db.close().unwrap();
    }
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let t = db.begin();
    assert_eq!(t.get(b"a").unwrap(), None);
    assert_eq!(all(&t).len(), 4);
}

#[test]
fn a_rolled_back_txn_does_not_disturb_an_open_snapshot() {
    let (_fs, db) = sim();
    base(&db);
    let snap = db.begin();
    let mut t = db.begin();
    t.put(b"b", b"x").unwrap();
    t.rollback();
    put(&db, b"b", b"committed");
    assert_eq!(snap.get(b"b").unwrap(), Some(b"old-b".to_vec()));
}

#[test]
fn long_values_over_a_page_read_whole_from_the_snapshot() {
    let (_fs, db) = sim();
    let v1 = long(7, 20_000);
    let w1 = long(9, 5_000);
    put(&db, b"big", &v1);
    put(&db, b"big2", &w1);
    let snap = db.begin();
    put(&db, b"big", &long(100, 30_000));
    del(&db, b"big2");
    // Reuse the freed pages with other long values.
    for i in 0..10u8 {
        put(&db, format!("other{i}").as_bytes(), &long(200 + i, 12_000));
    }
    db.checkpoint().unwrap();
    assert_eq!(snap.get(b"big").unwrap(), Some(v1.clone()));
    assert_eq!(snap.get(b"big2").unwrap(), Some(w1.clone()));
    assert_eq!(
        all(&snap),
        vec![(b"big".to_vec(), v1), (b"big2".to_vec(), w1)]
    );
}

#[test]
fn a_snapshot_survives_page_splits_after_begin() {
    let (_fs, db) = sim();
    let mut model = Model::new();
    {
        let mut t = db.begin();
        for i in (0..2000u32).step_by(10) {
            t.put(&key(i), &long(i as u8, 60)).unwrap();
            model.insert(key(i), long(i as u8, 60));
        }
        t.commit().unwrap();
    }
    let snap = db.begin();
    for chunk in (0..2000u32).collect::<Vec<_>>().chunks(200) {
        let mut t = db.begin();
        for &i in chunk {
            t.put(&key(i), &long(i as u8 ^ 0x55, 120)).unwrap();
        }
        t.commit().unwrap();
    }
    assert_eq!(all(&snap), model_items(&model));
    assert_eq!(snap.get(&key(5)).unwrap(), None);
    assert_eq!(all(&db.begin()).len(), 2000);
}

#[test]
fn a_snapshot_survives_page_merges_after_begin() {
    let (_fs, db) = sim();
    let mut model = Model::new();
    {
        let mut t = db.begin();
        for i in 0..2000u32 {
            t.put(&key(i), &long(i as u8, 100)).unwrap();
            model.insert(key(i), long(i as u8, 100));
        }
        t.commit().unwrap();
    }
    let snap = db.begin();
    for chunk in (0..2000u32)
        .filter(|i| i % 50 != 0)
        .collect::<Vec<_>>()
        .chunks(300)
    {
        let mut t = db.begin();
        for &i in chunk {
            t.delete(&key(i)).unwrap();
        }
        t.commit().unwrap();
    }
    db.checkpoint().unwrap();
    assert_eq!(all(&snap), model_items(&model));
    let lo = key(100);
    let hi = key(200);
    let got: Vec<_> = snap
        .scan::<&[u8]>((Included(&lo[..]), Excluded(&hi[..])))
        .map(|r| r.unwrap().0)
        .collect();
    assert_eq!(got, (100..200).map(key).collect::<Vec<_>>());
    assert_eq!(all(&db.begin()).len(), 40);
}

#[test]
fn a_snapshot_holds_through_explicit_checkpoints() {
    let (_fs, db) = sim();
    base(&db);
    db.checkpoint().unwrap();
    let snap = db.begin();
    for round in 0..4u8 {
        put(&db, b"d", &[round]);
        put(&db, format!("n{round}").as_bytes(), b"x");
        del(&db, b"h");
        db.checkpoint().unwrap();
        assert_eq!(snap.get(b"d").unwrap(), Some(b"old-d".to_vec()));
    }
    assert_eq!(keys_in(&snap, "a", "z"), ["b", "d", "f", "h"]);
}

#[test]
fn a_snapshot_holds_through_automatic_checkpoints() {
    let (fs, db) = sim();
    let mut model = Model::new();
    {
        let mut t = db.begin();
        for i in 0..300u32 {
            t.put(&key(i), &long(i as u8, 200)).unwrap();
            model.insert(key(i), long(i as u8, 200));
        }
        t.commit().unwrap();
    }
    let snap = db.begin();
    let mut written: u64 = 0;
    let mut round = 0u32;
    while written < 3 * CHECKPOINT_AFTER {
        let mut t = db.begin();
        for i in 0..20u32 {
            let k = key((round * 20 + i) % 300);
            t.put(&k, &long(round as u8, 3_000)).unwrap();
            written += 3_000;
        }
        t.delete(&key(round % 300)).unwrap();
        t.commit().unwrap();
        round += 1;
    }
    let data = fs.open(&dir().join(DATA_FILE)).unwrap().size().unwrap();
    assert!(data > 0, "no automatic checkpoint ran");
    assert_eq!(all(&snap), model_items(&model));
    for i in [0u32, 1, 150, 299] {
        assert_eq!(snap.get(&key(i)).unwrap(), model.get(&key(i)).cloned());
    }
}

#[test]
fn many_snapshots_open_at_once_each_read_their_own_moment() {
    let (_fs, db) = sim();
    let mut model = Model::new();
    let mut snaps: Vec<(Txn<'_, SimFs>, Model)> = Vec::new();
    let mut seed: u64 = 0x1234_5678;
    for step in 0..60u32 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let mut t = db.begin();
        for j in 0..(1 + seed % 5) {
            let k = key(((seed >> (j * 8)) % 80) as u32);
            if (seed >> (j + 3)).is_multiple_of(4) {
                t.delete(&k).unwrap();
                model.remove(&k);
            } else {
                let len = if (seed >> (j + 11)).is_multiple_of(7) {
                    6_000
                } else {
                    30
                };
                let v = long(step as u8, len);
                t.put(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        t.commit().unwrap();
        if step % 5 == 0 {
            snaps.push((db.begin(), model.clone()));
        }
        if step % 17 == 0 {
            db.checkpoint().unwrap();
        }
    }
    for (snap, expected) in &snaps {
        assert_eq!(all(snap), model_items(expected));
        for i in 0..80u32 {
            assert_eq!(snap.get(&key(i)).unwrap(), expected.get(&key(i)).cloned());
        }
    }
}

#[test]
fn two_overlapping_txns_on_different_keys_both_commit() {
    let (_fs, db) = sim();
    base(&db);
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"a1", b"A").unwrap();
    b.put(b"b1", b"B").unwrap();
    b.commit().unwrap();
    assert_eq!(a.get(b"b1").unwrap(), None);
    a.commit().unwrap();
    let r = db.begin();
    assert_eq!(r.get(b"a1").unwrap(), Some(b"A".to_vec()));
    assert_eq!(r.get(b"b1").unwrap(), Some(b"B".to_vec()));
    assert_eq!(r.get(b"d").unwrap(), Some(b"old-d".to_vec()));
}

#[test]
fn committed_data_seen_by_new_txns_survives_reopen() {
    let fs = SimFs::new();
    {
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        base(&db);
        let snap = db.begin();
        put(&db, b"d", b"new");
        assert_eq!(snap.get(b"d").unwrap(), Some(b"old-d".to_vec()));
        drop(snap);
        db.close().unwrap();
    }
    let db = Db::open_with(fs, dir()).unwrap();
    assert_eq!(db.begin().get(b"d").unwrap(), Some(b"new".to_vec()));
}
