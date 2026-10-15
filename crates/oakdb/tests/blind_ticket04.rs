//! Blind tests for ticket 04 (multi-level tree), written from issue #16,
//! design 0004 and the public API only, without reading the implementation.

use std::collections::BTreeMap;
use std::path::Path;

use oakdb::{Db, Error, DATA_FILE, MAX_ENTRY_LEN, MAX_KEY_LEN};
use proptest::prelude::*;

const BIG_VAL: usize = MAX_ENTRY_LEN - MAX_KEY_LEN; // 494

/// A 512-byte key whose order follows `n`.
fn key(n: u32) -> Vec<u8> {
    let mut k = format!("{n:010}").into_bytes();
    k.resize(MAX_KEY_LEN, b'k');
    k
}

/// A 494-byte value that encodes `n` and a generation tag.
fn val(n: u32, gen: u8) -> Vec<u8> {
    let mut v = format!("v{gen}-{n:010}").into_bytes();
    v.resize(BIG_VAL, gen);
    v
}

fn file_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join(DATA_FILE)).unwrap().len()
}

/// A permutation of 0..n that is neither ascending nor descending.
fn scattered(n: u32) -> Vec<u32> {
    // 7919 is prime and coprime with the n values used here.
    (0..n).map(|i| (i.wrapping_mul(7919)) % n).collect()
}

fn put_all(db: &Db, keys: &[u32], gen: u8, per_commit: usize) {
    for chunk in keys.chunks(per_commit) {
        let mut t = db.begin();
        for &n in chunk {
            t.put(&key(n), &val(n, gen)).unwrap();
        }
        t.commit().expect("no commit may fail for lack of space");
    }
}

fn check(db: &Db, model: &BTreeMap<u32, Vec<u8>>, universe: u32) {
    let t = db.begin();
    for n in 0..universe {
        let got = t.get(&key(n)).unwrap();
        assert_eq!(got.as_ref(), model.get(&n), "key {n}");
    }
}

fn reopen(db: Db, dir: &Path) -> Db {
    db.close().unwrap();
    Db::open(dir).unwrap()
}

#[test]
fn grows_past_three_levels_scattered_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let n = 1000;
    let order = scattered(n);
    put_all(&db, &order, 1, 37);
    let model: BTreeMap<u32, Vec<u8>> = (0..n).map(|i| (i, val(i, 1))).collect();
    check(&db, &model, n);
    // ~1000 entries of ~1 KB need hundreds of 4 KB pages.
    assert!(file_len(dir.path()) >= 250 * 4096, "len {}", file_len(dir.path()));
    let db = reopen(db, dir.path());
    check(&db, &model, n);
    db.close().unwrap();
}

#[test]
fn grows_with_ascending_descending_and_single_commit() {
    for (name, order, per) in [
        ("asc", (0..800).collect::<Vec<u32>>(), 10usize),
        ("desc", (0..800).rev().collect(), 10),
        ("one-commit", scattered(800), 800),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        put_all(&db, &order, 2, per);
        let model: BTreeMap<u32, Vec<u8>> = (0..800).map(|i| (i, val(i, 2))).collect();
        check(&db, &model, 800);
        let db = reopen(db, dir.path());
        check(&db, &model, 800);
        db.close().unwrap();
        eprintln!("{name} ok");
    }
}

#[test]
fn delete_everything_then_regrow() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path()).unwrap();
    let n = 600;
    for round in 0..3u8 {
        put_all(&db, &scattered(n), round, 50);
        let before = file_len(dir.path());
        // Delete everything, in chunks, in a scattered order.
        for chunk in scattered(n).chunks(71) {
            let mut t = db.begin();
            for &i in chunk {
                t.delete(&key(i)).unwrap();
            }
            t.commit().unwrap();
        }
        assert!(file_len(dir.path()) >= before, "file shrank");
        check(&db, &BTreeMap::new(), n);
        db = reopen(db, dir.path());
        check(&db, &BTreeMap::new(), n);
    }
    // Regrow once more and verify.
    put_all(&db, &scattered(n), 9, 600);
    let model: BTreeMap<u32, Vec<u8>> = (0..n).map(|i| (i, val(i, 9))).collect();
    check(&db, &model, n);
    let db = reopen(db, dir.path());
    check(&db, &model, n);
    db.close().unwrap();
}

#[test]
fn delete_all_in_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_all(&db, &scattered(700), 1, 100);
    let mut t = db.begin();
    for i in 0..700 {
        t.delete(&key(i)).unwrap();
    }
    t.commit().unwrap();
    check(&db, &BTreeMap::new(), 700);
    put_all(&db, &[5, 3, 600], 4, 10);
    let model: BTreeMap<u32, Vec<u8>> = [5, 3, 600].into_iter().map(|i| (i, val(i, 4))).collect();
    let db = reopen(db, dir.path());
    check(&db, &model, 700);
    db.close().unwrap();
}

#[test]
fn deleting_runs_that_empty_leaves_in_middle_and_ends() {
    let n = 800;
    // Each run is far bigger than a leaf (~4 entries), so whole leaves and
    // whole subtrees empty out.
    let runs: [(&str, std::ops::Range<u32>); 5] = [
        ("first", 0..120),
        ("last", 680..800),
        ("middle", 300..500),
        ("small-middle", 401..413),
        ("all-but-ends", 1..799),
    ];
    for (name, run) in runs {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        put_all(&db, &scattered(n), 1, 80);
        let mut model: BTreeMap<u32, Vec<u8>> = (0..n).map(|i| (i, val(i, 1))).collect();
        let mut t = db.begin();
        for i in run.clone() {
            t.delete(&key(i)).unwrap();
            model.remove(&i);
        }
        t.commit().unwrap();
        check(&db, &model, n);
        // Put back into the hole and around it; neighbours must still work.
        let back: Vec<u32> = run.clone().step_by(3).collect();
        put_all(&db, &back, 7, 25);
        for &i in &back {
            model.insert(i, val(i, 7));
        }
        check(&db, &model, n);
        let db = reopen(db, dir.path());
        check(&db, &model, n);
        db.close().unwrap();
        eprintln!("{name} ok");
    }
}

#[test]
fn deleting_runs_one_key_per_commit_from_each_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let n = 400;
    put_all(&db, &scattered(n), 1, 50);
    let mut model: BTreeMap<u32, Vec<u8>> = (0..n).map(|i| (i, val(i, 1))).collect();
    // Alternate deleting the smallest and the largest key, one per commit.
    let (mut lo, mut hi) = (0u32, n - 1);
    while lo < hi {
        for i in [lo, hi] {
            let mut t = db.begin();
            t.delete(&key(i)).unwrap();
            t.commit().unwrap();
            model.remove(&i);
        }
        lo += 1;
        hi -= 1;
        if lo % 50 == 0 {
            check(&db, &model, n);
        }
    }
    check(&db, &model, n);
    let db = reopen(db, dir.path());
    check(&db, &model, n);
    db.close().unwrap();
}

#[test]
fn mixed_delete_and_put_in_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let n = 600;
    // Even keys only.
    let evens: Vec<u32> = scattered(n).into_iter().filter(|i| i % 2 == 0).collect();
    put_all(&db, &evens, 1, 60);
    let mut model: BTreeMap<u32, Vec<u8>> = evens.iter().map(|&i| (i, val(i, 1))).collect();
    // One commit: delete a big run of evens (emptying leaves) and insert odds
    // into that same range and elsewhere, and overwrite some evens.
    let mut t = db.begin();
    for i in (100..400).filter(|i| i % 2 == 0) {
        t.delete(&key(i)).unwrap();
        model.remove(&i);
    }
    for i in (101..599).step_by(2) {
        t.put(&key(i), &val(i, 3)).unwrap();
        model.insert(i, val(i, 3));
    }
    for i in (400..600).step_by(4) {
        t.put(&key(i), &val(i, 5)).unwrap();
        model.insert(i, val(i, 5));
    }
    // Put then delete, delete then put, inside the same transaction.
    t.put(&key(1), &val(1, 6)).unwrap();
    t.delete(&key(1)).unwrap();
    t.delete(&key(0)).unwrap();
    t.put(&key(0), &val(0, 6)).unwrap();
    model.insert(0, val(0, 6));
    t.commit().unwrap();
    check(&db, &model, n);
    let db = reopen(db, dir.path());
    check(&db, &model, n);
    db.close().unwrap();
}

#[test]
fn file_never_shrinks() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path()).unwrap();
    let mut last = file_len(dir.path());
    let mut model = BTreeMap::new();
    let mut seed: u64 = 12345;
    for step in 0..120 {
        let mut t = db.begin();
        for _ in 0..15 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let i = ((seed >> 33) % 500) as u32;
            if (seed >> 20) % 3 == 0 {
                t.delete(&key(i)).unwrap();
                model.remove(&i);
            } else {
                t.put(&key(i), &val(i, step as u8)).unwrap();
                model.insert(i, val(i, step as u8));
            }
        }
        t.commit().unwrap();
        if step % 40 == 39 {
            db = reopen(db, dir.path());
        }
        let now = file_len(dir.path());
        assert!(now >= last, "file shrank at step {step}: {last} -> {now}");
        last = now;
    }
    check(&db, &model, 500);
    db.close().unwrap();
}

#[test]
fn size_limits_still_enforced_on_a_tall_tree() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_all(&db, &scattered(500), 1, 100);
    let mut t = db.begin();
    let long_key = vec![b'x'; MAX_KEY_LEN + 1];
    match t.put(&long_key, b"v") {
        Err(Error::KeyTooLarge { len, max }) => {
            assert_eq!((len, max), (MAX_KEY_LEN + 1, MAX_KEY_LEN));
        }
        other => panic!("expected KeyTooLarge, got {other:?}"),
    }
    match t.put(&key(7), &vec![0u8; BIG_VAL + 1]) {
        Err(Error::ValueTooLarge { len, max }) => {
            assert_eq!((len, max), (BIG_VAL + 1, BIG_VAL));
        }
        other => panic!("expected ValueTooLarge, got {other:?}"),
    }
    let short = b"short";
    match t.put(short, &vec![0u8; MAX_ENTRY_LEN - short.len() + 1]) {
        Err(Error::ValueTooLarge { max, .. }) => assert_eq!(max, MAX_ENTRY_LEN - short.len()),
        other => panic!("expected ValueTooLarge, got {other:?}"),
    }
    // Boundary sizes are accepted and stored.
    t.put(short, &vec![9u8; MAX_ENTRY_LEN - short.len()]).unwrap();
    t.put(&key(7), &vec![8u8; BIG_VAL]).unwrap();
    t.commit().unwrap();
    let t = db.begin();
    assert_eq!(t.get(short).unwrap().unwrap().len(), MAX_ENTRY_LEN - short.len());
    assert_eq!(t.get(&key(7)).unwrap().unwrap(), vec![8u8; BIG_VAL]);
    // Rejected writes left the old value alone.
    assert_eq!(t.get(&key(8)).unwrap().unwrap(), val(8, 1));
    assert_eq!(t.get(&long_key[..MAX_KEY_LEN]).unwrap(), None);
}

#[test]
fn overwrite_small_to_big_and_rollback_on_tall_tree() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    // Start with small values so leaves are dense, then grow every value.
    let mut t = db.begin();
    for i in 0..600 {
        t.put(&key(i), b"s").unwrap();
    }
    t.commit().unwrap();
    put_all(&db, &scattered(600), 2, 120);
    // A rolled-back big transaction leaves nothing behind.
    let mut t = db.begin();
    for i in 600..900 {
        t.put(&key(i), &val(i, 3)).unwrap();
    }
    for i in 0..300 {
        t.delete(&key(i)).unwrap();
    }
    t.rollback();
    let model: BTreeMap<u32, Vec<u8>> = (0..600).map(|i| (i, val(i, 2))).collect();
    check(&db, &model, 900);
    let db = reopen(db, dir.path());
    check(&db, &model, 900);
    db.close().unwrap();
}

#[derive(Debug, Clone)]
enum Op {
    Put(u16, u16),
    Delete(u16),
    DeleteRun(u16, u16),
    Get(u16),
    Commit,
    Rollback,
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => (0u16..400, 0u16..=BIG_VAL as u16).prop_map(|(k, l)| Op::Put(k, l)),
        3 => (0u16..400).prop_map(Op::Delete),
        1 => (0u16..400, 1u16..200).prop_map(|(a, n)| Op::DeleteRun(a, n)),
        3 => (0u16..400).prop_map(Op::Get),
        3 => Just(Op::Commit),
        1 => Just(Op::Rollback),
        1 => Just(Op::Reopen),
    ]
}

/// Variable-length keys (sometimes short, mostly long) keep fill uneven.
fn pkey(k: u16) -> Vec<u8> {
    let len = if k % 5 == 0 { 8 } else { 200 + (k as usize * 37) % (MAX_KEY_LEN - 199) };
    let mut v = format!("{k:05}").into_bytes();
    v.resize(len, b'a' + (k % 26) as u8);
    v
}

fn pval(k: u16, len: u16) -> Vec<u8> {
    let max = MAX_ENTRY_LEN - pkey(k).len();
    vec![(k % 251) as u8 ^ (len % 7) as u8; (len as usize).min(max)]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
    #[test]
    fn matches_btreemap(ops in prop::collection::vec(op(), 1..1500)) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Some(Db::open(dir.path()).unwrap());
        let mut committed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut pending = committed.clone();
        let mut txn_ops: Vec<Op> = Vec::new();

        // Re-apply pending ops in a fresh txn at commit time so the borrow of
        // `db` stays simple.
        let apply = |db: &Db, ops: &[Op]| -> oakdb::Result<()> {
            let mut t = db.begin();
            for op in ops {
                match *op {
                    Op::Put(k, l) => t.put(&pkey(k), &pval(k, l))?,
                    Op::Delete(k) => t.delete(&pkey(k))?,
                    Op::DeleteRun(a, n) => {
                        for k in a..(a + n).min(400) { t.delete(&pkey(k))?; }
                    }
                    _ => {}
                }
            }
            t.commit()
        };

        for op in ops {
            match op {
                Op::Put(k, l) => { pending.insert(pkey(k), pval(k, l)); txn_ops.push(op); }
                Op::Delete(k) => { pending.remove(&pkey(k)); txn_ops.push(op); }
                Op::DeleteRun(a, n) => {
                    for k in a..(a + n).min(400) { pending.remove(&pkey(k)); }
                    txn_ops.push(op);
                }
                Op::Get(k) => {
                    // Read through a txn that carries the pending writes.
                    let d = db.as_ref().unwrap();
                    let mut t = d.begin();
                    for o in &txn_ops {
                        match *o {
                            Op::Put(k, l) => t.put(&pkey(k), &pval(k, l)).unwrap(),
                            Op::Delete(k) => t.delete(&pkey(k)).unwrap(),
                            Op::DeleteRun(a, n) => for k in a..(a + n).min(400) { t.delete(&pkey(k)).unwrap(); },
                            _ => {}
                        }
                    }
                    prop_assert_eq!(t.get(&pkey(k)).unwrap(), pending.get(&pkey(k)).cloned());
                    t.rollback();
                }
                Op::Commit => {
                    apply(db.as_ref().unwrap(), &txn_ops).expect("commit");
                    committed = pending.clone();
                    txn_ops.clear();
                }
                Op::Rollback => { pending = committed.clone(); txn_ops.clear(); }
                Op::Reopen => {
                    db.take().unwrap().close().unwrap();
                    db = Some(Db::open(dir.path()).unwrap());
                    pending = committed.clone();
                    txn_ops.clear();
                }
            }
        }
        apply(db.as_ref().unwrap(), &txn_ops).expect("final commit");
        committed = pending;
        for round in 0..2 {
            let d = db.as_ref().unwrap();
            let t = d.begin();
            for k in 0..400u16 {
                prop_assert_eq!(t.get(&pkey(k)).unwrap(), committed.get(&pkey(k)).cloned(), "round {} key {}", round, k);
            }
            drop(t);
            db.take().unwrap().close().unwrap();
            db = Some(Db::open(dir.path()).unwrap());
        }
    }
}
