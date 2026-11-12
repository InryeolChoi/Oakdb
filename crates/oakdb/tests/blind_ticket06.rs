//! Blind black-box tests for long values in overflow pages (#18).
//!
//! Written from issue #18, spec #12, ADR 0008/0009 and the design
//! `docs/design/0006-overflow.md`, through the public API only, without
//! reading the implementation.

use std::collections::BTreeMap;
use std::fs;
use std::ops::Bound;
use std::path::Path;

use oakdb::{Db, Error, DATA_FILE, MAX_ENTRY_LEN, MAX_KEY_LEN, MAX_VALUE_LEN};
use proptest::prelude::*;

/// Page size from the design (4 KiB pages, 40-byte header).
const PAGE: u64 = 4096;
/// Value bytes one overflow page holds (design section 2).
const PER_OVERFLOW_PAGE: usize = 4096 - 40;

/// A value whose bytes depend on their position and on `seed`, so a page
/// read in the wrong order or from the wrong value shows up.
fn value(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let x = (i as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add(seed.wrapping_mul(40_503));
            (x >> 13) as u8 ^ (i / PER_OVERFLOW_PAGE) as u8
        })
        .collect()
}

fn file_size(dir: &Path) -> u64 {
    fs::metadata(dir.join(DATA_FILE)).unwrap().len()
}

fn put_commit(db: &Db, key: &[u8], val: &[u8]) {
    let mut t = db.begin();
    t.put(key, val).unwrap();
    t.commit().unwrap();
}

fn delete_commit(db: &Db, key: &[u8]) {
    let mut t = db.begin();
    t.delete(key).unwrap();
    t.commit().unwrap();
}

fn get(db: &Db, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

fn scan_all(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.begin()
        .scan::<&[u8]>(..)
        .collect::<oakdb::Result<Vec<_>>>()
        .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------- round trip

#[test]
fn a_100kb_value_round_trips_through_get_scan_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let big = value(100 * 1024, 1);
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"photo", &big);
        put_commit(&db, b"apple", b"small");
        put_commit(&db, b"zebra", b"small too");
        assert_eq!(get(&db, b"photo").as_deref(), Some(&big[..]));
        let all = scan_all(&db);
        assert_eq!(
            all,
            vec![
                (b"apple".to_vec(), b"small".to_vec()),
                (b"photo".to_vec(), big.clone()),
                (b"zebra".to_vec(), b"small too".to_vec()),
            ]
        );
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(get(&db, b"photo").as_deref(), Some(&big[..]));
    assert_eq!(scan_all(&db).len(), 3);
    assert_eq!(scan_all(&db)[1].1, big);
}

#[test]
fn a_multi_megabyte_value_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let big = value(3 * 1024 * 1024 + 17, 2);
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"k", &big);
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(get(&db, b"k"), Some(big));
}

#[test]
fn values_on_both_sides_of_the_leaf_limit_and_page_boundaries_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let keys: Vec<Vec<u8>> = vec![b"k".to_vec(), vec![b'm'; 100], vec![b'z'; MAX_KEY_LEN]];
    let mut expected = BTreeMap::new();
    {
        let db = Db::open(dir.path()).unwrap();
        for (ki, key) in keys.iter().enumerate() {
            let limit = MAX_ENTRY_LEN - key.len();
            let p = PER_OVERFLOW_PAGE;
            let lens = [
                0,
                1,
                limit - 1,
                limit,
                limit + 1,
                limit + 2,
                4095,
                4096,
                4097,
                p - 1,
                p,
                p + 1,
                2 * p - 1,
                2 * p,
                2 * p + 1,
                3 * p,
            ];
            for (li, &len) in lens.iter().enumerate() {
                let mut k = key.clone();
                // keep keys distinct but at the same length
                let last = k.len() - 1;
                k[last] = b'A' + li as u8;
                if k.len() > 1 {
                    k[0] = b'0' + ki as u8;
                }
                let v = value(len, (ki * 100 + li) as u32);
                put_commit(&db, &k, &v);
                assert_eq!(
                    get(&db, &k).as_deref(),
                    Some(&v[..]),
                    "key len {} value len {len}",
                    k.len()
                );
                expected.insert(k, v);
            }
        }
        let got: BTreeMap<_, _> = scan_all(&db).into_iter().collect();
        assert_eq!(got, expected);
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let got: BTreeMap<_, _> = scan_all(&db).into_iter().collect();
    assert_eq!(got, expected);
    for (k, v) in &expected {
        assert_eq!(get(&db, k).as_ref(), Some(v));
    }
}

#[test]
fn a_long_value_uses_only_the_overflow_pages_it_needs() {
    // Design section 2: one overflow page holds 4056 value bytes; 100KB is
    // 26 pages. The leaf already exists, so the file grows by exactly the
    // chain.
    for (seed, len) in [
        MAX_ENTRY_LEN, // with a 1-byte key this is just over the leaf limit
        PER_OVERFLOW_PAGE,
        PER_OVERFLOW_PAGE + 1,
        2 * PER_OVERFLOW_PAGE,
        100 * 1024,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"a", b"short");
        let before = file_size(dir.path());
        put_commit(&db, b"b", &value(len, seed as u32));
        let pages = len.div_ceil(PER_OVERFLOW_PAGE) as u64;
        assert_eq!(
            file_size(dir.path()) - before,
            pages * PAGE,
            "value of {len} bytes should take {pages} overflow pages"
        );
    }
}

#[test]
fn a_too_long_key_is_still_rejected_with_a_long_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    let err = t
        .put(&vec![b'k'; MAX_KEY_LEN + 1], &value(10_000, 3))
        .unwrap_err();
    assert!(
        matches!(err, Error::KeyTooLarge { len, max } if len == MAX_KEY_LEN + 1 && max == MAX_KEY_LEN),
        "{err:?}"
    );
    // A long value under the longest key is accepted.
    t.put(&vec![b'k'; MAX_KEY_LEN], &value(10_000, 3)).unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, &vec![b'k'; MAX_KEY_LEN]), Some(value(10_000, 3)));
}

#[test]
fn max_value_len_is_the_u32_limit() {
    assert_eq!(MAX_VALUE_LEN, u32::MAX as usize);
}

// ------------------------------------------------------------- page reuse

#[test]
fn overwriting_a_long_value_again_and_again_does_not_grow_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"k", &value(100 * 1024, 0));
    let size = file_size(dir.path());
    for i in 1..30 {
        let v = value(100 * 1024, i);
        put_commit(&db, b"k", &v);
        assert_eq!(get(&db, b"k"), Some(v));
        assert_eq!(file_size(dir.path()), size, "overwrite {i} grew the file");
    }
}

#[test]
fn overwriting_with_a_shorter_long_value_then_a_longer_one_reuses_freed_pages() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"k", &value(10 * PER_OVERFLOW_PAGE, 0));
    let size = file_size(dir.path());
    put_commit(&db, b"k", &value(2 * PER_OVERFLOW_PAGE, 1));
    assert_eq!(file_size(dir.path()), size, "file never shrinks");
    put_commit(&db, b"k", b"short now");
    assert_eq!(file_size(dir.path()), size);
    // Ten freed pages are enough for another key's long value.
    put_commit(&db, b"other", &value(10 * PER_OVERFLOW_PAGE, 2));
    assert_eq!(file_size(dir.path()), size);
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"short now"[..]));
    assert_eq!(get(&db, b"other"), Some(value(10 * PER_OVERFLOW_PAGE, 2)));
}

#[test]
fn deleting_a_long_value_frees_its_pages_for_the_next_one_and_never_shrinks_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", &value(100 * 1024, 0));
    let size = file_size(dir.path());
    delete_commit(&db, b"a");
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(file_size(dir.path()), size, "the file never shrinks");
    for i in 0..10u32 {
        let key = format!("key{i}");
        put_commit(&db, key.as_bytes(), &value(100 * 1024, i + 1));
        assert_eq!(file_size(dir.path()), size, "round {i} grew the file");
        delete_commit(&db, key.as_bytes());
    }
    assert!(scan_all(&db).is_empty());
}

#[test]
fn freed_pages_are_still_free_after_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let size;
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"a", &value(50 * 1024, 0));
        put_commit(&db, b"keep", b"me");
        delete_commit(&db, b"a");
        size = file_size(dir.path());
        db.close().unwrap();
    }
    {
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(file_size(dir.path()), size);
        put_commit(&db, b"b", &value(50 * 1024, 1));
        assert_eq!(
            file_size(dir.path()),
            size,
            "freed pages were lost on reopen"
        );
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(get(&db, b"b"), Some(value(50 * 1024, 1)));
    assert_eq!(get(&db, b"keep").as_deref(), Some(&b"me"[..]));
    assert_eq!(get(&db, b"a"), None);
}

#[test]
fn tree_pages_freed_by_emptying_the_tree_are_reused_when_it_grows_again() {
    // Design decision 3: empty leaves and inner pages go on the free list too.
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let keys: Vec<Vec<u8>> = (0..2000u32)
        .map(|i| format!("key{i:06}").into_bytes())
        .collect();
    let mut t = db.begin();
    for k in &keys {
        t.put(k, &[7u8; 200]).unwrap();
    }
    t.commit().unwrap();
    let size = file_size(dir.path());
    let mut t = db.begin();
    for k in &keys {
        t.delete(k).unwrap();
    }
    t.commit().unwrap();
    assert!(scan_all(&db).is_empty());
    assert_eq!(file_size(dir.path()), size);
    let mut t = db.begin();
    for k in &keys {
        t.put(k, &[8u8; 200]).unwrap();
    }
    t.commit().unwrap();
    assert_eq!(
        file_size(dir.path()),
        size,
        "regrowing the tree grew the file"
    );
    assert_eq!(scan_all(&db).len(), keys.len());
}

#[test]
fn several_long_values_written_and_freed_in_one_transaction_balance_out() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    for i in 0..5u32 {
        t.put(format!("old{i}").as_bytes(), &value(20_000, i))
            .unwrap();
    }
    t.commit().unwrap();
    let size = file_size(dir.path());
    let mut t = db.begin();
    for i in 0..5u32 {
        t.delete(format!("old{i}").as_bytes()).unwrap();
        t.put(format!("new{i}").as_bytes(), &value(20_000, 10 + i))
            .unwrap();
    }
    t.commit().unwrap();
    // Same number of pages freed as needed. Allow one page for a possible
    // leaf split from the new keys, nothing more.
    assert!(
        file_size(dir.path()) <= size + PAGE,
        "file grew from {size} to {}",
        file_size(dir.path())
    );
    for i in 0..5u32 {
        assert_eq!(get(&db, format!("old{i}").as_bytes()), None);
        assert_eq!(
            get(&db, format!("new{i}").as_bytes()),
            Some(value(20_000, 10 + i))
        );
    }
}

// ------------------------------------------------------------ transactions

#[test]
fn a_transaction_sees_its_own_uncommitted_long_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"b", &value(30_000, 0));
    let mut t = db.begin();
    t.put(b"a", &value(40_000, 1)).unwrap();
    t.put(b"b", &value(9_000, 2)).unwrap();
    assert_eq!(t.get(b"a").unwrap(), Some(value(40_000, 1)));
    assert_eq!(t.get(b"b").unwrap(), Some(value(9_000, 2)));
    let seen: Vec<_> = t.scan::<&[u8]>(..).collect::<oakdb::Result<_>>().unwrap();
    assert_eq!(
        seen,
        vec![
            (b"a".to_vec(), value(40_000, 1)),
            (b"b".to_vec(), value(9_000, 2))
        ]
    );
    // Another transaction does not see it.
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(value(30_000, 0)));
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), Some(value(40_000, 1)));
    assert_eq!(get(&db, b"b"), Some(value(9_000, 2)));
}

#[test]
fn rolling_back_a_long_value_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let marker = b"ROLLED-BACK-SECRET-".repeat(6000);
    let size;
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"keep", &value(20_000, 0));
        size = file_size(dir.path());
        let mut t = db.begin();
        t.put(b"gone", &marker).unwrap();
        t.delete(b"keep").unwrap();
        assert_eq!(t.get(b"gone").unwrap().as_deref(), Some(&marker[..]));
        t.rollback();
        let mut t = db.begin();
        t.put(b"gone2", &marker).unwrap();
        drop(t);
        assert_eq!(get(&db, b"gone"), None);
        assert_eq!(get(&db, b"gone2"), None);
        assert_eq!(get(&db, b"keep"), Some(value(20_000, 0)));
        assert_eq!(file_size(dir.path()), size);
        db.close().unwrap();
    }
    let bytes = fs::read(dir.path().join(DATA_FILE)).unwrap();
    assert!(!contains(&bytes, b"ROLLED-BACK-SECRET-"));
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(scan_all(&db), vec![(b"keep".to_vec(), value(20_000, 0))]);
}

#[test]
fn a_scan_started_before_a_commit_keeps_reading_the_long_values_it_saw() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    for i in 0..4u32 {
        t.put(format!("k{i}").as_bytes(), &value(30_000, i))
            .unwrap();
    }
    t.commit().unwrap();

    let reader = db.begin();
    let mut scan = reader.scan::<&[u8]>(..);
    let first = scan.next().unwrap().unwrap();
    assert_eq!(first, (b"k0".to_vec(), value(30_000, 0)));

    // Free every chain and reuse the pages for different bytes.
    let mut w = db.begin();
    for i in 0..4u32 {
        w.delete(format!("k{i}").as_bytes()).unwrap();
    }
    w.commit().unwrap();
    let mut w = db.begin();
    for i in 0..4u32 {
        w.put(format!("x{i}").as_bytes(), &value(30_000, 100 + i))
            .unwrap();
    }
    w.commit().unwrap();

    let rest: Vec<_> = scan.collect::<oakdb::Result<_>>().unwrap();
    assert_eq!(
        rest,
        (1..4u32)
            .map(|i| (format!("k{i}").into_bytes(), value(30_000, i)))
            .collect::<Vec<_>>()
    );
}

// --------------------------------------------------------------- privacy

#[test]
fn a_deleted_long_value_leaves_no_bytes_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let secret = b"TOPSECRET-PASSWORD-".repeat(5000); // about 95KB
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"a", b"neighbour");
        put_commit(&db, b"secret", &secret);
        db.close().unwrap();
    }
    assert!(contains(
        &fs::read(dir.path().join(DATA_FILE)).unwrap(),
        b"TOPSECRET-PASSWORD-"
    ));
    {
        let db = Db::open(dir.path()).unwrap();
        delete_commit(&db, b"secret");
        db.close().unwrap();
    }
    let bytes = fs::read(dir.path().join(DATA_FILE)).unwrap();
    assert!(
        !contains(&bytes, b"TOPSECRET"),
        "deleted long value is still in the file"
    );
}

#[test]
fn an_overwritten_long_value_leaves_no_bytes_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let secret = b"OLD-LONG-VALUE-".repeat(4000);
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"k", &secret);
        // Shorter long value: some freed pages are reused, the rest zeroed.
        put_commit(&db, b"k", &value(5_000, 9));
        db.close().unwrap();
    }
    let bytes = fs::read(dir.path().join(DATA_FILE)).unwrap();
    assert!(!contains(&bytes, b"OLD-LONG-VALUE-"));
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"k", &secret);
        // Long value becomes short: the whole chain is freed.
        put_commit(&db, b"k", b"tiny");
        db.close().unwrap();
    }
    let bytes = fs::read(dir.path().join(DATA_FILE)).unwrap();
    assert!(!contains(&bytes, b"OLD-LONG-VALUE-"));
}

// ------------------------------------------------------------- integrity

#[test]
fn a_damaged_overflow_page_is_reported_not_silently_read() {
    // Design section 2: the unused tail of an overflow page must be zero and
    // is checked when read. Put one 2-page value into a fresh database: its
    // chain is the last pages of the file, the last one partly filled.
    let dir = tempfile::tempdir().unwrap();
    let v = value(PER_OVERFLOW_PAGE + 100, 4);
    {
        let db = Db::open(dir.path()).unwrap();
        put_commit(&db, b"k", &v);
        db.close().unwrap();
    }
    let path = dir.path().join(DATA_FILE);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - PAGE as usize;
    assert_eq!(
        bytes[last + 4000],
        0,
        "expected the zero tail of the last overflow page"
    );
    bytes[last + 4000] = 0xAB;
    fs::write(&path, &bytes).unwrap();
    match Db::open(dir.path()) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt, got {e:?}"),
        Ok(db) => {
            let r = db.begin().get(b"k");
            assert!(
                matches!(r, Err(Error::Corrupt(_))),
                "damaged overflow page was read without complaint: {:?}",
                r.map(|o| o.map(|v| v.len()))
            );
        }
    }
}

// ------------------------------------------------------------ many leaves

#[test]
fn long_and_short_values_mix_under_many_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let mut model = BTreeMap::new();
    {
        let db = Db::open(dir.path()).unwrap();
        for batch in 0..6u32 {
            let mut t = db.begin();
            for j in 0..100u32 {
                let i = batch * 100 + j;
                let key = format!("key{:05}", (i * 7919) % 600).into_bytes();
                let len = match i % 4 {
                    0 => 10,
                    1 => 900,
                    2 => 1 + (i as usize * 37) % (3 * PER_OVERFLOW_PAGE),
                    _ => 5_000 + i as usize,
                };
                let v = value(len, i);
                t.put(&key, &v).unwrap();
                model.insert(key, v);
            }
            t.commit().unwrap();
        }
        assert_eq!(scan_all(&db), model.clone().into_iter().collect::<Vec<_>>());
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(scan_all(&db), model.clone().into_iter().collect::<Vec<_>>());
    // Range scans in the middle of the tree.
    let from = b"key00100".to_vec();
    let to = b"key00250".to_vec();
    let got: Vec<_> = db
        .begin()
        .scan::<&[u8]>((Bound::Included(&from[..]), Bound::Excluded(&to[..])))
        .collect::<oakdb::Result<_>>()
        .unwrap();
    let want: Vec<_> = model
        .range(from.clone()..to.clone())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(got, want);
    // Delete every other key, overwrite the rest with long values.
    let size = file_size(dir.path());
    let mut t = db.begin();
    for (n, k) in model
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .enumerate()
    {
        if n.is_multiple_of(2) {
            t.delete(&k).unwrap();
            model.remove(&k);
        }
    }
    t.commit().unwrap();
    assert!(file_size(dir.path()) >= size);
    assert_eq!(scan_all(&db), model.clone().into_iter().collect::<Vec<_>>());
    for (k, v) in &model {
        assert_eq!(get(&db, k).as_ref(), Some(v));
    }
}

// ----------------------------------------------------------------- model

#[derive(Debug, Clone)]
enum Op {
    Put(u8, usize, u32),
    Delete(u8),
}

fn op() -> impl Strategy<Value = Op> {
    let len = prop_oneof![
        3 => 0usize..64,
        2 => (MAX_ENTRY_LEN - 20)..(MAX_ENTRY_LEN + 20),
        3 => 1000usize..(4 * PER_OVERFLOW_PAGE + 10),
        1 => 20_000usize..60_000,
    ];
    prop_oneof![
        3 => (0u8..24, len, any::<u32>()).prop_map(|(k, l, s)| Op::Put(k, l, s)),
        1 => (0u8..24).prop_map(Op::Delete),
    ]
}

fn key_of(k: u8) -> Vec<u8> {
    // Some keys are long so their leaf limit differs.
    if k.is_multiple_of(5) {
        let mut key = vec![b'L'; 300];
        key.push(k);
        key
    } else {
        format!("k{k:03}").into_bytes()
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn matches_a_btreemap_with_long_values(
        txns in prop::collection::vec(
            (prop::collection::vec(op(), 1..12), any::<bool>(), any::<bool>()),
            1..12,
        )
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(dir.path()).unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut last_size = file_size(dir.path());
        for (ops, commit, reopen) in txns {
            let mut pending = model.clone();
            let mut t = db.begin();
            for op in ops {
                match op {
                    Op::Put(k, len, seed) => {
                        let v = value(len, seed);
                        t.put(&key_of(k), &v).unwrap();
                        pending.insert(key_of(k), v);
                    }
                    Op::Delete(k) => {
                        t.delete(&key_of(k)).unwrap();
                        pending.remove(&key_of(k));
                    }
                }
            }
            let inside: Vec<_> = t.scan::<&[u8]>(..).collect::<oakdb::Result<_>>().unwrap();
            prop_assert_eq!(inside, pending.clone().into_iter().collect::<Vec<_>>());
            if commit {
                t.commit().unwrap();
                model = pending;
            } else {
                t.rollback();
            }
            if reopen {
                db.close().unwrap();
                db = Db::open(dir.path()).unwrap();
            }
            let size = file_size(dir.path());
            prop_assert!(size >= last_size, "file shrank from {} to {}", last_size, size);
            last_size = size;
            prop_assert_eq!(scan_all(&db), model.clone().into_iter().collect::<Vec<_>>());
            for k in 0..24u8 {
                prop_assert_eq!(get(&db, &key_of(k)), model.get(&key_of(k)).cloned());
            }
        }
        // Space is bounded: deleting everything and writing the same data
        // again does not grow the file.
        let mut t = db.begin();
        for k in model.keys() { t.delete(k).unwrap(); }
        t.commit().unwrap();
        let size = file_size(dir.path());
        let mut t = db.begin();
        for (k, v) in &model { t.put(k, v).unwrap(); }
        t.commit().unwrap();
        prop_assert_eq!(file_size(dir.path()), size);
        prop_assert_eq!(scan_all(&db), model.into_iter().collect::<Vec<_>>());
    }
}
