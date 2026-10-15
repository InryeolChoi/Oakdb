//! Blind tests for ticket 05 (range scan), written from issue #17, spec #12,
//! docs/design/0005-range-scan.md and the public API only.

use std::collections::BTreeMap;
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use oakdb::{Db, Txn, MAX_ENTRY_LEN, MAX_KEY_LEN};
use proptest::prelude::*;

type Kv = (Vec<u8>, Vec<u8>);

fn collect(scan: oakdb::Scan<'_>) -> Vec<Kv> {
    scan.map(|r| r.expect("scan item")).collect()
}

fn keys(v: &[Kv]) -> Vec<Vec<u8>> {
    v.iter().map(|(k, _)| k.clone()).collect()
}

fn s(x: &str) -> Vec<u8> {
    x.as_bytes().to_vec()
}

fn fill(db: &Db, pairs: &[(&[u8], &[u8])]) {
    let mut t = db.begin();
    for (k, v) in pairs {
        t.put(k, v).unwrap();
    }
    t.commit().unwrap();
}

/// Database with b, d, f, h, j committed (value = key uppercased).
fn small_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    fill(
        &db,
        &[
            (b"b", b"B"),
            (b"d", b"D"),
            (b"f", b"F"),
            (b"h", b"H"),
            (b"j", b"J"),
        ],
    );
    (dir, db)
}

fn scan_keys(txn: &Txn<'_>, lo: Bound<&str>, hi: Bound<&str>) -> Vec<String> {
    collect(txn.scan::<&str>((lo, hi)))
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect()
}

fn model_keys(lo: Bound<&str>, hi: Bound<&str>) -> Vec<String> {
    let all = ["b", "d", "f", "h", "j"];
    all.iter()
        .filter(|k| {
            let k = **k;
            let lo_ok = match lo {
                Included(x) => k >= x,
                Excluded(x) => k > x,
                Unbounded => true,
            };
            let hi_ok = match hi {
                Included(x) => k <= x,
                Excluded(x) => k < x,
                Unbounded => true,
            };
            lo_ok && hi_ok
        })
        .map(|k| k.to_string())
        .collect()
}

/// Every inclusive/exclusive/open combination, with bounds on keys, between
/// keys and outside all keys, including backwards and empty ranges.
#[test]
fn all_bound_combinations_match_model() {
    let (_d, db) = small_db();
    let txn = db.begin();
    let points = ["", "a", "b", "c", "d", "e", "f", "h", "i", "j", "k", "zz"];
    let mut checked = 0;
    for lo_kind in 0..3 {
        for hi_kind in 0..3 {
            for lo in points {
                for hi in points {
                    let lo_b = match lo_kind {
                        0 => Included(lo),
                        1 => Excluded(lo),
                        _ => Unbounded,
                    };
                    let hi_b = match hi_kind {
                        0 => Included(hi),
                        1 => Excluded(hi),
                        _ => Unbounded,
                    };
                    assert_eq!(
                        scan_keys(&txn, lo_b, hi_b),
                        model_keys(lo_b, hi_b),
                        "range ({lo_b:?}, {hi_b:?})"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 1000);
}

#[test]
fn range_syntax_forms() {
    let (_d, db) = small_db();
    let txn = db.begin();
    let k = |v: Vec<Kv>| keys(&v);
    assert_eq!(k(collect(txn.scan("d".."h"))), vec![s("d"), s("f")]);
    assert_eq!(
        k(collect(txn.scan("d"..="h"))),
        vec![s("d"), s("f"), s("h")]
    );
    assert_eq!(k(collect(txn.scan("e"..))), vec![s("f"), s("h"), s("j")]);
    assert_eq!(k(collect(txn.scan(.."d"))), vec![s("b")]);
    assert_eq!(k(collect(txn.scan(..="d"))), vec![s("b"), s("d")]);
    assert_eq!(k(collect(txn.scan::<&[u8]>(..))).len(), 5);
    assert_eq!(
        k(collect(txn.scan::<&str>((Excluded("d"), Included("h"))))),
        vec![s("f"), s("h")]
    );
    // Other key types.
    assert_eq!(
        k(collect(txn.scan(b"d".to_vec()..b"f".to_vec()))),
        vec![s("d")]
    );
    assert_eq!(k(collect(txn.scan([b'h']..=[b'j']))), vec![s("h"), s("j")]);
    // Values come with keys.
    assert_eq!(collect(txn.scan("f"..="f")), vec![(s("f"), s("F"))]);
}

#[test]
fn empty_and_backwards_ranges_yield_nothing() {
    let (_d, db) = small_db();
    let txn = db.begin();
    assert!(collect(txn.scan("h".."d")).is_empty());
    assert!(collect(txn.scan("h"..="d")).is_empty());
    assert!(collect(txn.scan("d".."d")).is_empty());
    assert!(collect(txn.scan::<&str>((Excluded("d"), Included("d")))).is_empty());
    assert!(collect(txn.scan::<&str>((Excluded("d"), Excluded("d")))).is_empty());
    assert!(collect(txn.scan::<&str>((Excluded("z"), Excluded("a")))).is_empty());
    assert_eq!(collect(txn.scan("d"..="d")).len(), 1);
    // Bound keys longer than MAX_KEY_LEN are only compared, never stored.
    let long = vec![b'c'; MAX_KEY_LEN + 100];
    assert_eq!(keys(&collect(txn.scan(long.clone()..))).len(), 4);
    assert!(collect(txn.scan(long.clone()..vec![b'a'])).is_empty());
}

#[test]
fn empty_database_scans_are_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    assert!(collect(txn.scan::<&[u8]>(..)).is_empty());
    assert!(collect(txn.scan("a".."z")).is_empty());
    assert!(collect(txn.scan("z".."a")).is_empty());
}

fn big_key(i: u32) -> Vec<u8> {
    let mut k = format!("{i:08}").into_bytes();
    k.resize(MAX_KEY_LEN, b'k');
    k
}

fn big_val(i: u32) -> Vec<u8> {
    let mut v = format!("v{i}").into_bytes();
    v.resize(MAX_ENTRY_LEN - MAX_KEY_LEN, b'v');
    v
}

/// 1200 entries of a full 1006 bytes each: hundreds of leaves, several
/// levels. Inserted in a scrambled order over several commits.
fn big_db(n: u32) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut order: Vec<u32> = (0..n).collect();
    // Deterministic scramble.
    order.sort_by_key(|i| (i.wrapping_mul(2_654_435_761)) % 10_007);
    for chunk in order.chunks(300) {
        let mut t = db.begin();
        for &i in chunk {
            t.put(&big_key(i), &big_val(i)).unwrap();
        }
        t.commit().unwrap();
    }
    (dir, db)
}

#[test]
fn many_leaves_full_scan_in_order() {
    let n = 1200;
    let (_d, db) = big_db(n);
    let txn = db.begin();
    let all = collect(txn.scan::<&[u8]>(..));
    assert_eq!(all.len(), n as usize);
    for (i, (k, v)) in all.iter().enumerate() {
        assert_eq!(k, &big_key(i as u32), "key at {i}");
        assert_eq!(v, &big_val(i as u32), "value at {i}");
    }
}

#[test]
fn many_leaves_subranges() {
    let n = 1200;
    let (_d, db) = big_db(n);
    let txn = db.begin();
    let cases = [
        (0u32, 1199u32),
        (3, 4),
        (100, 900),
        (517, 518),
        (1198, 1199),
        (250, 251),
    ];
    for (a, b) in cases {
        let got = keys(&collect(txn.scan(big_key(a)..big_key(b))));
        let want: Vec<_> = (a..b).map(big_key).collect();
        assert_eq!(got, want, "{a}..{b}");
        let got = keys(&collect(txn.scan(big_key(a)..=big_key(b))));
        let want: Vec<_> = (a..=b).map(big_key).collect();
        assert_eq!(got, want, "{a}..={b}");
        let got = keys(&collect(
            txn.scan::<Vec<u8>>((Excluded(big_key(a)), Excluded(big_key(b)))),
        ));
        let want: Vec<_> = (a + 1..b).map(big_key).collect();
        assert_eq!(got, want, "({a}, {b}) exclusive");
    }
    // Bounds between keys: the short prefix "00000500" sorts before key 500.
    let got = keys(&collect(
        txn.scan(b"00000500".to_vec()..b"00000503".to_vec()),
    ));
    assert_eq!(got, vec![big_key(500), big_key(501), big_key(502)]);
    // Past every key.
    assert!(collect(txn.scan(b"99".to_vec()..)).is_empty());
}

#[test]
fn own_writes_merged_into_scan() {
    let (_d, db) = small_db();
    let mut t = db.begin();
    t.put(b"a", b"new-a").unwrap(); // before all
    t.put(b"e", b"new-e").unwrap(); // between
    t.put(b"f", b"F2").unwrap(); // overwrite
    t.delete(b"h").unwrap(); // delete committed
    t.put(b"z", b"new-z").unwrap(); // after all
    t.put(b"g", b"tmp").unwrap();
    t.delete(b"g").unwrap(); // put then delete own write
    t.delete(b"q").unwrap(); // delete missing key
    let got = collect(t.scan::<&[u8]>(..));
    let want = vec![
        (s("a"), s("new-a")),
        (s("b"), s("B")),
        (s("d"), s("D")),
        (s("e"), s("new-e")),
        (s("f"), s("F2")),
        (s("j"), s("J")),
        (s("z"), s("new-z")),
    ];
    assert_eq!(got, want);
    assert_eq!(
        keys(&collect(t.scan("c"..="f"))),
        vec![s("d"), s("e"), s("f")]
    );
    assert_eq!(keys(&collect(t.scan("g".."k"))), vec![s("j")]);

    // Another transaction sees none of it.
    let other = db.begin();
    assert_eq!(
        keys(&collect(other.scan::<&[u8]>(..))),
        vec![s("b"), s("d"), s("f"), s("h"), s("j")]
    );
    assert_eq!(collect(other.scan("f"..="f")), vec![(s("f"), s("F"))]);
    drop(other);

    // After commit everyone sees it.
    t.commit().unwrap();
    let after = db.begin();
    assert_eq!(collect(after.scan::<&[u8]>(..)), want);
}

#[test]
fn rollback_writes_never_appear() {
    let (_d, db) = small_db();
    let mut t = db.begin();
    t.put(b"c", b"x").unwrap();
    t.delete(b"b").unwrap();
    t.rollback();
    let mut t = db.begin();
    t.put(b"c", b"x").unwrap();
    drop(t);
    let txn = db.begin();
    assert_eq!(
        keys(&collect(txn.scan::<&[u8]>(..))),
        vec![s("b"), s("d"), s("f"), s("h"), s("j")]
    );
}

#[test]
fn commit_during_scan_does_not_change_it() {
    let n = 600;
    let (_d, db) = big_db(n);
    let reader = db.begin();
    let mut scan = reader.scan::<&[u8]>(..);
    let mut seen = Vec::new();
    for _ in 0..100 {
        seen.push(scan.next().unwrap().unwrap().0);
    }
    // Another transaction rewrites and deletes keys ahead of and behind the
    // scan, and adds new ones.
    let mut w = db.begin();
    for i in 0..n {
        if i % 3 == 0 {
            w.delete(&big_key(i)).unwrap();
        } else if i % 3 == 1 {
            w.put(&big_key(i), b"changed").unwrap();
        }
    }
    for i in n..n + 200 {
        w.put(&big_key(i), &big_val(i)).unwrap();
    }
    w.commit().unwrap();

    let mut rest = Vec::new();
    for item in scan.by_ref() {
        let (k, v) = item.unwrap();
        let i: u32 = std::str::from_utf8(&k[..8]).unwrap().parse().unwrap();
        assert_eq!(v, big_val(i), "value of key {i} changed mid-scan");
        rest.push(k);
    }
    seen.extend(rest);
    let want: Vec<_> = (0..n).map(big_key).collect();
    assert_eq!(seen.len(), want.len());
    assert!(seen == want, "scan saw the concurrent commit");

    // A new scan sees the commit.
    let fresh = db.begin();
    let now = keys(&collect(fresh.scan::<&[u8]>(..)));
    let want_now: Vec<_> = (0..n + 200)
        .filter(|i| i >= &n || i % 3 != 0)
        .map(big_key)
        .collect();
    assert_eq!(now, want_now);
}

#[test]
fn scan_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let n = 1000;
    {
        let db = Db::open(dir.path()).unwrap();
        let mut t = db.begin();
        for i in (0..n).rev() {
            t.put(&big_key(i), &big_val(i)).unwrap();
        }
        t.commit().unwrap();
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    let all = collect(txn.scan::<&[u8]>(..));
    assert_eq!(all.len(), n as usize);
    assert_eq!(keys(&all), (0..n).map(big_key).collect::<Vec<_>>());
    assert_eq!(
        keys(&collect(txn.scan(big_key(10)..big_key(13)))),
        vec![big_key(10), big_key(11), big_key(12)]
    );
}

#[test]
fn scan_after_deleting_whole_runs() {
    let n = 1200;
    let (dir, db) = big_db(n);
    // Delete whole runs (many full leaves) in separate commits.
    let runs = [(0u32, 150u32), (300, 700), (1000, 1200)];
    for (a, b) in runs {
        let mut t = db.begin();
        for i in a..b {
            t.delete(&big_key(i)).unwrap();
        }
        t.commit().unwrap();
    }
    let alive = |i: &u32| !runs.iter().any(|(a, b)| (*a..*b).contains(i));
    let want: Vec<_> = (0..n).filter(alive).map(big_key).collect();
    let txn = db.begin();
    assert_eq!(keys(&collect(txn.scan::<&[u8]>(..))), want);
    // Ranges falling entirely in a hole, and spanning a hole.
    assert!(collect(txn.scan(big_key(320)..big_key(690))).is_empty());
    assert_eq!(
        keys(&collect(txn.scan(big_key(298)..=big_key(701)))),
        vec![big_key(298), big_key(299), big_key(700), big_key(701)]
    );
    assert!(collect(txn.scan(big_key(1000)..)).is_empty());
    drop(txn);
    db.close().unwrap();

    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    assert_eq!(keys(&collect(txn.scan::<&[u8]>(..))), want);
    drop(txn);

    // Delete everything, then scan.
    let mut t = db.begin();
    for k in &want {
        t.delete(k).unwrap();
    }
    t.commit().unwrap();
    assert!(collect(db.begin().scan::<&[u8]>(..)).is_empty());
}

#[test]
fn stop_early_with_take() {
    let (_d, db) = big_db(1000);
    let txn = db.begin();
    let first: Vec<_> = txn
        .scan(big_key(400)..)
        .take(5)
        .map(|r| r.unwrap().0)
        .collect();
    assert_eq!(first, (400..405).map(big_key).collect::<Vec<_>>());
    // Dropping a half-read scan leaves the db usable.
    let mut it = txn.scan::<&[u8]>(..);
    it.next();
    drop(it);
    drop(txn);
    let mut t = db.begin();
    t.put(b"zz", b"1").unwrap();
    t.commit().unwrap();
    assert_eq!(db.begin().scan(b"zz".as_slice()..).count(), 1);
}

#[test]
fn scan_is_fused_after_end() {
    let (_d, db) = small_db();
    let txn = db.begin();
    let mut it = txn.scan("i"..);
    assert_eq!(it.next().unwrap().unwrap().0, s("j"));
    assert!(it.next().is_none());
    assert!(it.next().is_none());
}

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
    Commit,
}

fn key_strat() -> impl Strategy<Value = Vec<u8>> {
    // Small alphabet so keys collide; length up to 6, plus some long keys.
    prop_oneof![
        4 => proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'b'), Just(b'c'), Just(b'd')], 0..6),
        1 => (0u8..4, 400usize..=MAX_KEY_LEN).prop_map(|(c, n)| vec![b'a' + c; n]),
    ]
}

fn op_strat() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (key_strat(), 0usize..400).prop_map(|(k, n)| Op::Put(k, vec![b'v'; n])),
        2 => key_strat().prop_map(Op::Del),
        1 => Just(Op::Commit),
    ]
}

fn bound_strat() -> impl Strategy<Value = Bound<Vec<u8>>> {
    prop_oneof![
        Just(Unbounded),
        key_strat().prop_map(Included),
        key_strat().prop_map(Excluded),
    ]
}

fn model_range(
    m: &BTreeMap<Vec<u8>, Vec<u8>>,
    lo: &Bound<Vec<u8>>,
    hi: &Bound<Vec<u8>>,
) -> Vec<Kv> {
    // BTreeMap::range panics on backwards/empty-excluded ranges; filter instead.
    m.iter()
        .filter(|(k, _)| {
            let lo_ok = match lo {
                Included(x) => *k >= x,
                Excluded(x) => *k > x,
                Unbounded => true,
            };
            let hi_ok = match hi {
                Included(x) => *k <= x,
                Excluded(x) => *k < x,
                Unbounded => true,
            };
            lo_ok && hi_ok
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn random_scans_match_btreemap(
        ops in proptest::collection::vec(op_strat(), 1..250),
        ranges in proptest::collection::vec((bound_strat(), bound_strat()), 1..20),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let mut committed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut pending = committed.clone();
        let mut txn = db.begin();
        for op in ops {
            match op {
                Op::Put(k, v) => { txn.put(&k, &v).unwrap(); pending.insert(k, v); }
                Op::Del(k) => { txn.delete(&k).unwrap(); pending.remove(&k); }
                Op::Commit => {
                    txn.commit().unwrap();
                    committed = pending.clone();
                    txn = db.begin();
                }
            }
        }
        // Scans inside the open transaction see its writes; another
        // transaction sees only committed data.
        let other = db.begin();
        for (lo, hi) in &ranges {
            let got = collect(txn.scan::<Vec<u8>>((lo.clone(), hi.clone())));
            prop_assert_eq!(got, model_range(&pending, lo, hi), "own txn {:?} {:?}", lo, hi);
            let got = collect(other.scan::<Vec<u8>>((lo.clone(), hi.clone())));
            prop_assert_eq!(got, model_range(&committed, lo, hi), "other txn {:?} {:?}", lo, hi);
            // And BTreeMap::range itself where it accepts the range.
            // BTreeMap::range panics when start > end, or start == end with
            // both ends excluded.
            let ok = match (lo, hi) {
                (Excluded(a), Excluded(b)) => a < b,
                (Included(a) | Excluded(a), Included(b) | Excluded(b)) => a <= b,
                _ => true,
            };
            if ok {
                let want: Vec<Kv> = pending
                    .range::<Vec<u8>, _>((lo.clone(), hi.clone()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let got = collect(txn.scan::<Vec<u8>>((lo.clone(), hi.clone())));
                prop_assert_eq!(got, want);
            }
        }
        let all = collect(txn.scan::<&[u8]>(..));
        prop_assert_eq!(all, pending.into_iter().collect::<Vec<_>>());
    }
}
