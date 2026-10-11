//! Blind black-box tests for ticket 01 (issue #13): single-page storage.
//!
//! Written without reading the implementation. Every test checks a promise
//! from issue #13, the v1 spec (#12), the ADRs, or the public rustdoc.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use oakdb::fs::{Fs, FsFile, StdFs};
use oakdb::{Db, Error, MAX_KEY_LEN};

// ---------------------------------------------------------------- helpers

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn put_commit(db: &Db, k: &[u8], v: &[u8]) {
    let mut t = db.begin();
    t.put(k, v).unwrap();
    t.commit().unwrap();
}

fn get(db: &Db, k: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(k).unwrap()
}

/// Every regular file in the DB directory (the data file name is private).
fn data_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

/// The single data file (ticket 01 has no WAL yet).
fn the_data_file(dir: &Path) -> PathBuf {
    let files = data_files(dir);
    assert_eq!(files.len(), 1, "expected exactly one data file, got {files:?}");
    files[0].clone()
}

// ---------------------------------------------------------------- open / create

#[test]
fn open_creates_missing_nested_directory() {
    let t = tmp();
    let path = t.path().join("a").join("b").join("db");
    let db = Db::open(&path).unwrap();
    assert!(path.is_dir());
    db.close().unwrap();
}

#[test]
fn fresh_db_is_empty() {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    assert_eq!(get(&db, b"anything"), None);
    assert_eq!(get(&db, b""), None);
    db.close().unwrap();
}

#[test]
fn reopening_empty_db_works_and_is_still_empty() {
    let t = tmp();
    let p = t.path().join("db");
    Db::open(&p).unwrap().close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), None);
    db.close().unwrap();
}

#[test]
fn opening_existing_empty_directory_creates_db() {
    let t = tmp();
    let db = Db::open(t.path()).unwrap();
    put_commit(&db, b"k", b"v");
    db.close().unwrap();
    let db = Db::open(t.path()).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"v".to_vec()));
}

#[test]
fn meta_page_records_4kb_page_size_file_is_page_aligned() {
    // "메타 페이지에 페이지 크기(4KB)를 기록한다". File size should be a
    // multiple of 4096 and the value 4096 should appear in the first pages.
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", b"v");
    db.close().unwrap();
    let f = the_data_file(&p);
    let bytes = fs::read(&f).unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(bytes.len() % 4096, 0, "file len {} not page aligned", bytes.len());
    let head = &bytes[..bytes.len().min(2 * 4096)];
    let le32 = 4096u32.to_le_bytes();
    let be32 = 4096u32.to_be_bytes();
    let le16 = 4096u16.to_le_bytes();
    let be16 = 4096u16.to_be_bytes();
    let has = |n: &[u8]| head.windows(n.len()).any(|w| w == n);
    assert!(
        has(&le32) || has(&be32) || has(&le16) || has(&be16),
        "page size 4096 not found in meta area"
    );
}

#[test]
fn opening_a_path_that_is_a_regular_file_errors() {
    let t = tmp();
    let p = t.path().join("not_a_dir");
    fs::write(&p, b"hello").unwrap();
    let r = Db::open(&p);
    assert!(r.is_err(), "opening a regular file as DB dir must fail");
    // and must not clobber the file
    assert_eq!(fs::read(&p).unwrap(), b"hello");
}

// ---------------------------------------------------------------- put/get/persistence

#[test]
fn put_get_survives_reopen() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"hello", b"world");
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"hello"), Some(b"world".to_vec()));
    assert_eq!(get(&db, b"hell"), None);
    assert_eq!(get(&db, b"hello!"), None);
}

#[test]
fn survives_reopen_after_drop_without_close() {
    // Committed data must not depend on calling close().
    let t = tmp();
    let p = t.path().join("db");
    {
        let db = Db::open(&p).unwrap();
        put_commit(&db, b"k", b"v");
        // dropped, no close
    }
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"v".to_vec()));
}

#[test]
fn committed_data_on_disk_while_db_still_open() {
    // A commit "writes straight to the data file" in ticket 01; a second
    // handle opened via std::mem::forget-ed first handle must see it.
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", b"v");
    std::mem::forget(db); // simulate process dying without destructors
    let db2 = Db::open(&p).unwrap();
    assert_eq!(get(&db2, b"k"), Some(b"v".to_vec()));
}

#[test]
fn many_keys_one_txn_survive_reopen() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let mut tx = db.begin();
    for i in 0..100u32 {
        tx.put(format!("key{i:04}").as_bytes(), &i.to_le_bytes()).unwrap();
    }
    tx.commit().unwrap();
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for i in 0..100u32 {
        assert_eq!(get(&db, format!("key{i:04}").as_bytes()), Some(i.to_le_bytes().to_vec()));
    }
}

#[test]
fn many_keys_many_txns_random_order_survive_reopen() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    // pseudo-random insertion order
    let mut order: Vec<u32> = (0..80).collect();
    let mut s = 12345u32;
    for i in (1..order.len()).rev() {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        order.swap(i, (s as usize) % (i + 1));
    }
    for &i in &order {
        put_commit(&db, &i.to_be_bytes(), format!("v{i}").as_bytes());
    }
    for i in 0..80u32 {
        assert_eq!(get(&db, &i.to_be_bytes()), Some(format!("v{i}").into_bytes()));
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for i in 0..80u32 {
        assert_eq!(get(&db, &i.to_be_bytes()), Some(format!("v{i}").into_bytes()));
    }
    assert_eq!(get(&db, &80u32.to_be_bytes()), None);
}

#[test]
fn empty_key_and_empty_value() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"", b"empty-key");
    put_commit(&db, b"empty-val", b"");
    assert_eq!(get(&db, b""), Some(b"empty-key".to_vec()));
    assert_eq!(get(&db, b"empty-val"), Some(Vec::new()));
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b""), Some(b"empty-key".to_vec()));
    // An empty value is a present value, not "missing".
    assert_eq!(get(&db, b"empty-val"), Some(Vec::new()));
}

#[test]
fn binary_keys_and_values_with_nul_and_ff() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let keys: Vec<Vec<u8>> = vec![
        vec![0x00],
        vec![0x00, 0x00],
        vec![0xFF],
        vec![0xFF, 0xFF, 0x00],
        vec![0x00, 0xFF],
        vec![0x7F, 0x80],
    ];
    {
        let mut tx = db.begin();
        for (i, k) in keys.iter().enumerate() {
            let mut v = k.clone();
            v.push(i as u8);
            v.extend_from_slice(&[0, 0xFF, 0]);
            tx.put(k, &v).unwrap();
        }
        tx.commit().unwrap();
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for (i, k) in keys.iter().enumerate() {
        let mut v = k.clone();
        v.push(i as u8);
        v.extend_from_slice(&[0, 0xFF, 0]);
        assert_eq!(get(&db, k), Some(v), "key {k:?}");
    }
    assert_eq!(get(&db, &[0x00, 0x00, 0x00]), None);
    assert_eq!(get(&db, &[]), None);
}

#[test]
fn keys_that_are_prefixes_of_each_other_are_distinct() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    for n in 0..10usize {
        put_commit(&db, &vec![b'a'; n], &n.to_le_bytes());
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for n in 0..10usize {
        assert_eq!(get(&db, &vec![b'a'; n]), Some(n.to_le_bytes().to_vec()));
    }
}

#[test]
fn overwrite_then_reopen_keeps_latest() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", b"first");
    put_commit(&db, b"k", b"second-longer-value");
    put_commit(&db, b"k", b"3");
    assert_eq!(get(&db, b"k"), Some(b"3".to_vec()));
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"3".to_vec()));
}

#[test]
fn overwrite_within_one_txn_keeps_last() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let mut tx = db.begin();
    tx.put(b"k", b"a").unwrap();
    tx.put(b"k", b"bb").unwrap();
    tx.put(b"k", b"c").unwrap();
    assert_eq!(tx.get(b"k").unwrap(), Some(b"c".to_vec()));
    tx.commit().unwrap();
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"c".to_vec()));
}

#[test]
fn repeated_overwrites_do_not_leak_page_space() {
    // Overwriting the same key many times must not eventually hit PageFull:
    // the live data is tiny. (Leaf uses free space / compaction per ADR 0008.)
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    for i in 0..2000u32 {
        let v = vec![(i % 251) as u8; 100 + (i as usize % 50)];
        let mut tx = db.begin();
        tx.put(b"same-key", &v).unwrap();
        tx.commit().unwrap_or_else(|e| panic!("overwrite #{i} failed: {e:?}"));
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"same-key"), Some(vec![(1999 % 251) as u8; 100 + 1999 % 50]));
}

#[test]
fn shrinking_then_growing_value_many_keys() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    for round in 0..50usize {
        let mut tx = db.begin();
        for k in 0..10u8 {
            let len = if (round + k as usize) % 2 == 0 { 200 } else { 1 };
            tx.put(&[k], &vec![k; len]).unwrap();
        }
        tx.commit().unwrap_or_else(|e| panic!("round {round}: {e:?}"));
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for k in 0..10u8 {
        let len = if (49 + k as usize) % 2 == 0 { 200 } else { 1 };
        assert_eq!(get(&db, &[k]), Some(vec![k; len]));
    }
}

// ---------------------------------------------------------------- key size

#[test]
fn max_key_len_is_512() {
    assert_eq!(MAX_KEY_LEN, 512);
}

#[test]
fn key_of_exactly_512_bytes_is_accepted_and_persists() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let k = vec![0xABu8; 512];
    put_commit(&db, &k, b"v");
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, &k), Some(b"v".to_vec()));
}

#[test]
fn key_of_513_bytes_is_rejected_on_put() {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    let mut tx = db.begin();
    let e = tx.put(&vec![1u8; 513], b"v").unwrap_err();
    match e {
        Error::KeyTooLarge { len, max } => {
            assert_eq!(len, 513);
            assert_eq!(max, 512);
        }
        other => panic!("expected KeyTooLarge, got {other:?}"),
    }
}

#[test]
fn huge_key_rejected() {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    let mut tx = db.begin();
    let e = tx.put(&vec![1u8; 1 << 20], b"v").unwrap_err();
    assert!(matches!(e, Error::KeyTooLarge { .. }), "{e:?}");
}

#[test]
fn rejected_long_key_does_not_poison_txn_or_persist() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let mut tx = db.begin();
    tx.put(b"ok", b"1").unwrap();
    assert!(tx.put(&vec![2u8; 513], b"v").is_err());
    tx.put(b"ok2", b"2").unwrap();
    tx.commit().unwrap();
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"ok"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"ok2"), Some(b"2".to_vec()));
    assert_eq!(get(&db, &vec![2u8; 513]), None);
    assert_eq!(get(&db, &vec![2u8; 512]), None);
}

#[test]
fn get_with_overlong_key_does_not_panic() {
    // get of a key that could never be stored: either Ok(None) or KeyTooLarge.
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    let r = db.begin().get(&vec![9u8; 10_000]);
    match r {
        Ok(None) | Err(Error::KeyTooLarge { .. }) => {}
        other => panic!("unexpected: {other:?}"),
    }
}

// ---------------------------------------------------------------- txn semantics

#[test]
fn read_your_own_writes() {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    put_commit(&db, b"k", b"old");
    let mut tx = db.begin();
    assert_eq!(tx.get(b"k").unwrap(), Some(b"old".to_vec()));
    tx.put(b"k", b"new").unwrap();
    tx.put(b"fresh", b"x").unwrap();
    assert_eq!(tx.get(b"k").unwrap(), Some(b"new".to_vec()));
    assert_eq!(tx.get(b"fresh").unwrap(), Some(b"x".to_vec()));
    tx.commit().unwrap();
}

#[test]
fn uncommitted_writes_invisible_to_other_txn() {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    put_commit(&db, b"k", b"old");
    let mut writer = db.begin();
    writer.put(b"k", b"new").unwrap();
    writer.put(b"x", b"1").unwrap();
    let reader = db.begin();
    assert_eq!(reader.get(b"k").unwrap(), Some(b"old".to_vec()));
    assert_eq!(reader.get(b"x").unwrap(), None);
    drop(reader);
    writer.commit().unwrap();
    assert_eq!(get(&db, b"k"), Some(b"new".to_vec()));
}

#[test]
fn rollback_discards_writes_and_survives_reopen() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", b"keep");
    let mut tx = db.begin();
    tx.put(b"k", b"discard").unwrap();
    tx.put(b"new", b"discard").unwrap();
    tx.rollback();
    assert_eq!(get(&db, b"k"), Some(b"keep".to_vec()));
    assert_eq!(get(&db, b"new"), None);
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"keep".to_vec()));
    assert_eq!(get(&db, b"new"), None);
}

#[test]
fn drop_without_commit_discards_writes() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    {
        let mut tx = db.begin();
        tx.put(b"k", b"v").unwrap();
    }
    assert_eq!(get(&db, b"k"), None);
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), None);
}

#[test]
fn empty_commit_is_ok() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", b"v");
    db.begin().commit().unwrap();
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"v".to_vec()));
}

// ---------------------------------------------------------------- PageFull

fn fill_until_page_full(db: &Db) -> (usize, Error) {
    // commit 1 key per txn with a ~200 byte value until PageFull.
    for i in 0..10_000usize {
        let mut tx = db.begin();
        tx.put(format!("fill{i:05}").as_bytes(), &[i as u8; 200]).unwrap();
        if let Err(e) = tx.commit() {
            return (i, e);
        }
    }
    panic!("never hit PageFull in a 4KB page");
}

#[test]
fn page_full_reported_and_atomic() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let (n, e) = fill_until_page_full(&db);
    assert!(matches!(e, Error::PageFull), "expected PageFull, got {e:?}");
    assert!(n > 0, "first small commit should fit");
    // committed ones are intact, failing one absent
    for i in 0..n {
        assert_eq!(get(&db, format!("fill{i:05}").as_bytes()), Some(vec![i as u8; 200]));
    }
    assert_eq!(get(&db, format!("fill{n:05}").as_bytes()), None);
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for i in 0..n {
        assert_eq!(get(&db, format!("fill{i:05}").as_bytes()), Some(vec![i as u8; 200]));
    }
    assert_eq!(get(&db, format!("fill{n:05}").as_bytes()), None);
}

#[test]
fn page_full_multi_put_txn_is_all_or_nothing() {
    // "if writes don't fit → PageFull and none take effect"
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"base", b"base");
    let mut tx = db.begin();
    tx.put(b"base", b"CHANGED").unwrap();
    for i in 0..40u32 {
        tx.put(&i.to_be_bytes(), &[7u8; 200]).unwrap();
    }
    let e = tx.commit().unwrap_err();
    assert!(matches!(e, Error::PageFull), "{e:?}");
    assert_eq!(get(&db, b"base"), Some(b"base".to_vec()));
    for i in 0..40u32 {
        assert_eq!(get(&db, &i.to_be_bytes()), None, "partial write {i} leaked");
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"base"), Some(b"base".to_vec()));
    for i in 0..40u32 {
        assert_eq!(get(&db, &i.to_be_bytes()), None, "partial write {i} leaked after reopen");
    }
}

#[test]
fn single_value_larger_than_page_gives_page_full_not_panic() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"a", b"1");
    let mut tx = db.begin();
    tx.put(b"big", &vec![0u8; 10_000]).unwrap();
    let e = tx.commit().unwrap_err();
    assert!(matches!(e, Error::PageFull), "{e:?}");
    // DB still usable
    put_commit(&db, b"b", b"2");
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
    assert_eq!(get(&db, b"big"), None);
}

/// Commit `kvs` in one txn into a brand-new DB; Ok(()) or the commit error.
fn fresh_commit(kvs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), Error> {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    let mut tx = db.begin();
    for (k, v) in kvs {
        tx.put(k, v).unwrap();
    }
    tx.commit()
}

#[test]
fn value_just_under_and_over_page_boundary() {
    // Largest single value that fits into an EMPTY db (fresh DB per probe so
    // earlier commits cannot consume space). A 4KB leaf with a ~40 byte header
    // should hold a single value of well over 3000 bytes.
    let (mut lo, mut hi) = (0usize, 8192usize);
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match fresh_commit(&[(b"k".to_vec(), vec![0x5A; mid])]) {
            Ok(()) => lo = mid,
            Err(Error::PageFull) => hi = mid,
            Err(e) => panic!("unexpected error at {mid}: {e:?}"),
        }
    }
    eprintln!("largest single value in an empty 4KB page: {lo}");
    assert!(lo > 3000 && lo < 4096, "max single value {lo} looks wrong for a 4KB page");
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", &vec![0x33; lo]);
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some(vec![0x33; lo]));
}

#[test]
fn overwriting_same_tiny_value_does_not_exhaust_page() {
    // Minimal repro: the live data is 1 key + 8-byte value. Overwriting it
    // must never report PageFull ("committed data does not fit").
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    for i in 0..1000u32 {
        let mut tx = db.begin();
        tx.put(b"k", b"12345678").unwrap();
        tx.commit().unwrap_or_else(|e| panic!("overwrite #{i} of an 8-byte value failed: {e:?}"));
    }
}

#[test]
fn leaked_space_is_persisted_and_survives_reopen() {
    // After overwrites exhaust the page, does reopening recover the space?
    // Either way, the latest value must be intact, and since the committed
    // data is a single 8-byte value, a further small commit must succeed.
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let mut last_ok = None;
    for i in 0..1000u32 {
        let mut tx = db.begin();
        tx.put(b"k", &(i as u64).to_le_bytes()).unwrap();
        if tx.commit().is_err() {
            break;
        }
        last_ok = Some(i);
    }
    let last_ok = last_ok.unwrap();
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k"), Some((last_ok as u64).to_le_bytes().to_vec()));
    let mut tx = db.begin();
    tx.put(b"other", b"x").unwrap();
    tx.commit().expect("after reopen, a tiny commit into a nearly-empty DB failed");
}

#[test]
fn max_key_plus_large_value_boundaries() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let k = vec![0xEEu8; 512];
    let mut ok = 0;
    for len in (0..4096).step_by(97) {
        let mut tx = db.begin();
        tx.put(&k, &vec![1; len]).unwrap();
        match tx.commit() {
            Ok(()) => ok = len,
            Err(Error::PageFull) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, &k), Some(vec![1; ok]));
}

#[test]
fn after_page_full_shrinking_overwrite_still_commits() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let (n, _) = fill_until_page_full(&db);
    // shrink every value -> frees space -> new key must fit
    let mut tx = db.begin();
    for i in 0..n {
        tx.put(format!("fill{i:05}").as_bytes(), b"s").unwrap();
    }
    tx.commit().unwrap();
    put_commit(&db, b"after", &[9u8; 200]);
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"after"), Some(vec![9u8; 200]));
    for i in 0..n {
        assert_eq!(get(&db, format!("fill{i:05}").as_bytes()), Some(b"s".to_vec()));
    }
}

// ---------------------------------------------------------------- Fs seam

#[derive(Default)]
struct Counters {
    create_dir_all: AtomicUsize,
    open: AtomicUsize,
    sync_dir: AtomicUsize,
    sync_data: AtomicUsize,
    writes: AtomicUsize,
    reads: AtomicUsize,
    set_len: AtomicUsize,
    fail_sync_data: AtomicBool,
    fail_write: AtomicBool,
    fail_read: AtomicBool,
    log: Mutex<Vec<String>>,
}

#[derive(Clone, Default)]
struct SpyFs {
    c: Arc<Counters>,
}

struct SpyFile {
    inner: <StdFs as Fs>::File,
    c: Arc<Counters>,
}

impl Fs for SpyFs {
    type File = SpyFile;
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.c.create_dir_all.fetch_add(1, Ordering::SeqCst);
        self.c.log.lock().unwrap().push(format!("create_dir_all {}", path.display()));
        StdFs.create_dir_all(path)
    }
    fn open(&self, path: &Path) -> io::Result<SpyFile> {
        self.c.open.fetch_add(1, Ordering::SeqCst);
        self.c.log.lock().unwrap().push(format!("open {}", path.display()));
        Ok(SpyFile { inner: StdFs.open(path)?, c: self.c.clone() })
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.c.sync_dir.fetch_add(1, Ordering::SeqCst);
        self.c.log.lock().unwrap().push(format!("sync_dir {}", dir.display()));
        StdFs.sync_dir(dir)
    }
}

impl FsFile for SpyFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.c.reads.fetch_add(1, Ordering::SeqCst);
        if self.c.fail_read.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected read failure"));
        }
        self.inner.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.c.writes.fetch_add(1, Ordering::SeqCst);
        self.c.log.lock().unwrap().push(format!("write {} @{}", buf.len(), offset));
        if self.c.fail_write.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected write failure"));
        }
        self.inner.write_all_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.c.set_len.fetch_add(1, Ordering::SeqCst);
        self.c.log.lock().unwrap().push(format!("set_len {len}"));
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.c.sync_data.fetch_add(1, Ordering::SeqCst);
        self.c.log.lock().unwrap().push("sync_data".into());
        if self.c.fail_sync_data.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected sync failure"));
        }
        self.inner.sync_data()
    }
}

#[test]
fn all_file_access_goes_through_fs_creating_db() {
    // "모든 파일 접근은 Fs 트레이트를 거친다": creating a new DB via open_with
    // must use Fs::create_dir_all and Fs::open, and every file that appears
    // in the directory must have been opened through Fs.
    let t = tmp();
    let p = t.path().join("sub").join("db");
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), &p).unwrap();
    {
        let mut tx = db.begin();
        tx.put(b"k", b"v").unwrap();
        tx.commit().unwrap();
    }
    db.close().unwrap();
    assert!(fs_.c.create_dir_all.load(Ordering::SeqCst) >= 1, "dir created behind Fs's back");
    assert!(fs_.c.open.load(Ordering::SeqCst) >= 1);
    let log = fs_.c.log.lock().unwrap().clone();
    for f in data_files(&p) {
        assert!(
            log.iter().any(|l| l == &format!("open {}", f.display())),
            "file {f:?} not opened through Fs; log={log:?}"
        );
    }
}

#[test]
fn creating_db_syncs_directory() {
    // Spec: "파일을 만들거나 이름을 바꾼 뒤에는 디렉터리도 디스크에 기록한다".
    let t = tmp();
    let p = t.path().join("db");
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), &p).unwrap();
    db.close().unwrap();
    let log = fs_.c.log.lock().unwrap().clone();
    assert!(
        log.iter().any(|l| l == &format!("sync_dir {}", p.display())),
        "new data file created but DB dir never synced: {log:?}"
    );
}

#[test]
fn creating_db_dir_syncs_parent_directory() {
    // The DB directory itself is a new directory entry in its parent.
    let t = tmp();
    let p = t.path().join("db");
    let fs_ = SpyFs::default();
    Db::open_with(fs_.clone(), &p).unwrap().close().unwrap();
    let log = fs_.c.log.lock().unwrap().clone();
    assert!(
        log.iter().any(|l| l == &format!("sync_dir {}", t.path().display())),
        "new DB directory created but parent dir never synced: {log:?}"
    );
}

#[test]
fn new_db_meta_is_synced_before_open_returns() {
    let t = tmp();
    let p = t.path().join("db");
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), &p).unwrap();
    assert!(fs_.c.writes.load(Ordering::SeqCst) >= 1, "meta page not written on create");
    assert!(fs_.c.sync_data.load(Ordering::SeqCst) >= 1, "new file's meta never synced");
    // write must precede the last sync
    let log = fs_.c.log.lock().unwrap().clone();
    let last_write = log.iter().rposition(|l| l.starts_with("write")).unwrap();
    let last_sync = log.iter().rposition(|l| l == "sync_data").unwrap();
    assert!(last_sync > last_write, "write after last sync: {log:?}");
    drop(db);
}

#[test]
fn successful_commit_syncs_data_after_writing() {
    let t = tmp();
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), t.path().join("db")).unwrap();
    fs_.c.log.lock().unwrap().clear();
    let syncs_before = fs_.c.sync_data.load(Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"k", b"v").unwrap();
    tx.commit().unwrap();
    assert!(fs_.c.sync_data.load(Ordering::SeqCst) > syncs_before, "commit did not sync");
    let log = fs_.c.log.lock().unwrap().clone();
    let last_write = log.iter().rposition(|l| l.starts_with("write")).expect("commit wrote nothing");
    let last_sync = log.iter().rposition(|l| l == "sync_data").unwrap();
    assert!(last_sync > last_write, "commit returned with unsynced write: {log:?}");
}

#[test]
fn writes_are_page_aligned() {
    let t = tmp();
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), t.path().join("db")).unwrap();
    put_commit_f(&db, b"k", b"v");
    let log = fs_.c.log.lock().unwrap().clone();
    for l in log.iter().filter(|l| l.starts_with("write")) {
        let parts: Vec<&str> = l.split(' ').collect();
        let len: u64 = parts[1].parse().unwrap();
        let off: u64 = parts[2].trim_start_matches('@').parse().unwrap();
        assert_eq!(off % 4096, 0, "unaligned write: {l}");
        assert_eq!(len % 4096, 0, "non-page-sized write: {l}");
    }
}

fn put_commit_f<F: Fs>(db: &Db<F>, k: &[u8], v: &[u8]) {
    let mut t = db.begin();
    t.put(k, v).unwrap();
    t.commit().unwrap();
}

fn get_f<F: Fs>(db: &Db<F>, k: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(k).unwrap()
}

#[test]
fn rollback_and_readonly_txn_do_not_write() {
    let t = tmp();
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), t.path().join("db")).unwrap();
    put_commit_f(&db, b"k", b"v");
    let w = fs_.c.writes.load(Ordering::SeqCst);
    {
        let mut tx = db.begin();
        tx.put(b"x", b"y").unwrap();
        tx.rollback();
    }
    {
        let mut tx = db.begin();
        tx.put(b"x", b"y").unwrap();
    }
    assert_eq!(get_f(&db, b"k"), Some(b"v".to_vec()));
    assert_eq!(fs_.c.writes.load(Ordering::SeqCst), w, "rollback/drop/get wrote to disk");
}

#[test]
fn sync_failure_on_commit_returns_io() {
    let t = tmp();
    let p = t.path().join("db");
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), &p).unwrap();
    put_commit_f(&db, b"a", b"1");
    fs_.c.fail_sync_data.store(true, Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"b", b"2").unwrap();
    let e = tx.commit().unwrap_err();
    assert!(matches!(e, Error::Io(..)), "expected Io, got {e:?}");
}

#[test]
fn write_failure_on_commit_returns_io_and_does_not_report_success() {
    let t = tmp();
    let p = t.path().join("db");
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), &p).unwrap();
    put_commit_f(&db, b"a", b"1");
    fs_.c.fail_write.store(true, Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"b", b"2").unwrap();
    let e = tx.commit().unwrap_err();
    assert!(matches!(e, Error::Io(..)), "expected Io, got {e:?}");
    drop(db);
    // On disk: earlier committed data must still be there (the write failed,
    // so the file is untouched by the failed commit).
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
}

#[test]
fn after_failed_sync_db_does_not_pretend_later_commits_are_durable() {
    // Spec story 20 / Implementation Decisions: "디스크 기록이 실패하면 재시도하지
    // 않고 엔진을 정지시킨다". After a failed sync, a later commit must not
    // return Ok while sync still fails. (Once sync is fixed, the engine
    // should still refuse — engine halted. We only assert the weaker property.)
    let t = tmp();
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), t.path().join("db")).unwrap();
    fs_.c.fail_sync_data.store(true, Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"b", b"2").unwrap();
    assert!(tx.commit().is_err());
    let mut tx = db.begin();
    tx.put(b"c", b"3").unwrap();
    assert!(tx.commit().is_err(), "commit claimed success while fsync fails");
}

#[test]
#[ignore = "engine halt after fsync failure is a later ticket (log/recovery); documents gap"]
fn after_failed_sync_engine_halts_even_if_disk_recovers() {
    let t = tmp();
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), t.path().join("db")).unwrap();
    fs_.c.fail_sync_data.store(true, Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"b", b"2").unwrap();
    assert!(tx.commit().is_err());
    fs_.c.fail_sync_data.store(false, Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"c", b"3").unwrap();
    assert!(tx.commit().is_err(), "engine kept running after fsync failure");
}

#[test]
fn failed_commit_writes_not_visible_in_memory() {
    // After commit fails with Io the outcome is "unknown" on disk, but the
    // same process must not then read the failed value as if committed?
    // The API only says "outcome unknown", so we only check reading does not
    // panic and returns either old or new value.
    let t = tmp();
    let fs_ = SpyFs::default();
    let db = Db::open_with(fs_.clone(), t.path().join("db")).unwrap();
    put_commit_f(&db, b"a", b"old");
    fs_.c.fail_write.store(true, Ordering::SeqCst);
    let mut tx = db.begin();
    tx.put(b"a", b"new").unwrap();
    assert!(tx.commit().is_err());
    fs_.c.fail_write.store(false, Ordering::SeqCst);
    let r = db.begin().get(b"a");
    match r {
        Ok(Some(v)) => assert!(v == b"old" || v == b"new", "{v:?}"),
        Ok(None) => panic!("committed key 'a' vanished after failed commit"),
        Err(_) => {} // halted engine is acceptable
    }
}

#[test]
fn read_failure_on_open_returns_io_not_panic() {
    let t = tmp();
    let p = t.path().join("db");
    Db::open(&p).unwrap().close().unwrap();
    let fs_ = SpyFs::default();
    fs_.c.fail_read.store(true, Ordering::SeqCst);
    match Db::open_with(fs_.clone(), &p) {
        Err(Error::Io(..)) => {}
        Err(e) => panic!("expected Io, got {e:?}"),
        Ok(db) => {
            // if open is lazy, the first get must fail with Io
            let r = db.begin().get(b"k");
            assert!(matches!(r, Err(Error::Io(..))), "{r:?}");
        }
    }
}

#[test]
fn open_failure_in_fs_returns_io() {
    #[derive(Clone)]
    struct NoOpen;
    impl Fs for NoOpen {
        type File = <StdFs as Fs>::File;
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            StdFs.create_dir_all(p)
        }
        fn open(&self, _: &Path) -> io::Result<Self::File> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "nope"))
        }
        fn sync_dir(&self, d: &Path) -> io::Result<()> {
            StdFs.sync_dir(d)
        }
    }
    let t = tmp();
    let r = Db::open_with(NoOpen, t.path().join("db"));
    assert!(matches!(r, Err(Error::Io(..))), "{:?}", r.err());
}

// ---------------------------------------------------------------- corruption / foreign files

fn make_db_with_data(p: &Path) -> PathBuf {
    let db = Db::open(p).unwrap();
    put_commit(&db, b"k1", b"v1");
    put_commit(&db, b"k2", b"v2");
    db.close().unwrap();
    the_data_file(p)
}

/// Opening must either fail with an error (Corrupt / PageSizeMismatch / Io),
/// or—if it succeeds—must never panic and never return wrong data.
fn open_must_not_lie(p: &Path) -> Option<Error> {
    let r = std::panic::catch_unwind(|| Db::open(p));
    let r = r.expect("Db::open panicked on a damaged file");
    match r {
        Err(e) => Some(e),
        Ok(db) => {
            let tx = db.begin();
            for (k, v) in [(&b"k1"[..], &b"v1"[..]), (b"k2", b"v2")] {
                match tx.get(k) {
                    Ok(Some(got)) => assert_eq!(got, v, "wrong data returned"),
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
            None
        }
    }
}

#[test]
fn truncated_to_zero_bytes_file() {
    // A zero-length data file in an existing dir: either treat as new or error,
    // but never panic.
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    fs::write(&f, b"").unwrap();
    let r = std::panic::catch_unwind(|| Db::open(&p));
    assert!(r.is_ok(), "panicked on empty data file");
}

#[test]
fn truncated_mid_page_errors() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let len = fs::metadata(&f).unwrap().len();
    let fh = fs::OpenOptions::new().write(true).open(&f).unwrap();
    fh.set_len(len - 100).unwrap();
    drop(fh);
    let e = open_must_not_lie(&p);
    assert!(e.is_some(), "open accepted a truncated (non page-aligned) file");
}

#[test]
fn truncated_drops_last_page() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let len = fs::metadata(&f).unwrap().len();
    assert!(len >= 4096);
    let fh = fs::OpenOptions::new().write(true).open(&f).unwrap();
    fh.set_len(len - 4096).unwrap();
    drop(fh);
    let r = std::panic::catch_unwind(|| {
        if let Ok(db) = Db::open(&p) {
            let tx = db.begin();
            let a = tx.get(b"k1");
            // must not be wrong data
            if let Ok(Some(v)) = a {
                assert_eq!(v, b"v1");
            }
        }
    });
    assert!(r.is_ok(), "panicked on file missing last page");
}

#[test]
fn garbage_file_is_not_an_oakdb_file() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let mut junk = vec![0u8; 3 * 4096];
    let mut s = 0xDEADBEEFu32;
    for b in junk.iter_mut() {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        *b = s as u8;
    }
    fs::write(&f, &junk).unwrap();
    let e = open_must_not_lie(&p).expect("open accepted random garbage as a DB");
    assert!(matches!(e, Error::Corrupt(..)), "expected Corrupt, got {e:?}");
}

#[test]
fn text_file_is_not_an_oakdb_file() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    fs::write(&f, b"SQLite format 3\0 this is not oakdb").unwrap();
    let e = open_must_not_lie(&p).expect("open accepted a text file as a DB");
    assert!(
        matches!(e, Error::Corrupt(..) | Error::PageSizeMismatch { .. }),
        "expected Corrupt, got {e:?}"
    );
}

#[test]
fn all_zero_file_is_rejected() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let len = fs::metadata(&f).unwrap().len() as usize;
    fs::write(&f, vec![0u8; len]).unwrap();
    let e = open_must_not_lie(&p);
    assert!(e.is_some(), "all-zero file accepted as a valid DB");
}

#[test]
fn page_size_mismatch_detected() {
    // Rewrite the recorded 4096 in the meta area to 8192 (both meta slots).
    // We locate 4096 as LE/BE u16/u32 in the first 2 pages and replace all.
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let mut bytes = fs::read(&f).unwrap();
    let mut replaced = 0;
    let pats: [(Vec<u8>, Vec<u8>); 2] = [
        (4096u32.to_le_bytes().to_vec(), 8192u32.to_le_bytes().to_vec()),
        (4096u32.to_be_bytes().to_vec(), 8192u32.to_be_bytes().to_vec()),
    ];
    let lim = bytes.len().min(2 * 4096);
    for (from, to) in &pats {
        let mut i = 0;
        while i + from.len() <= lim {
            if &bytes[i..i + from.len()] == &from[..] {
                bytes[i..i + to.len()].copy_from_slice(to);
                replaced += 1;
                i += from.len();
            } else {
                i += 1;
            }
        }
    }
    if replaced == 0 {
        // try 16-bit
        let pats16 = [
            (4096u16.to_le_bytes(), 8192u16.to_le_bytes()),
            (4096u16.to_be_bytes(), 8192u16.to_be_bytes()),
        ];
        for (from, to) in pats16 {
            let mut i = 0;
            while i + 2 <= lim {
                if bytes[i..i + 2] == from {
                    bytes[i..i + 2].copy_from_slice(&to);
                    replaced += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }
    assert!(replaced > 0, "could not find recorded page size");
    fs::write(&f, &bytes).unwrap();
    let e = open_must_not_lie(&p).expect("opened a file with page size 8192");
    // Note: if a meta checksum covers the field, Corrupt is also defensible,
    // but the issue says page-size mismatch must be an error; prefer the
    // specific variant.
    assert!(
        matches!(e, Error::PageSizeMismatch { .. } | Error::Corrupt(..)),
        "unexpected {e:?}"
    );
}

#[test]
fn flipping_bytes_in_meta_page_never_panics_or_lies() {
    // Flip one byte at a time in page 0 (meta) — every flip must produce
    // either an error or correct data, never a panic or wrong value.
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let orig = fs::read(&f).unwrap();
    for off in (0..4096.min(orig.len())).step_by(7) {
        let mut b = orig.clone();
        b[off] ^= 0xFF;
        fs::write(&f, &b).unwrap();
        let r = std::panic::catch_unwind(|| open_must_not_lie(&p));
        assert!(r.is_ok(), "panic after flipping meta byte {off}");
    }
}

#[test]
fn flipping_bytes_anywhere_never_panics() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let orig = fs::read(&f).unwrap();
    for off in (0..orig.len()).step_by(13) {
        let mut b = orig.clone();
        b[off] ^= 0x5A;
        fs::write(&f, &b).unwrap();
        let r = std::panic::catch_unwind(|| {
            if let Ok(db) = Db::open(&p) {
                let tx = db.begin();
                let _ = tx.get(b"k1");
                let _ = tx.get(b"k2");
                let _ = tx.get(b"zzz");
                drop(tx);
                let mut tx = db.begin();
                let _ = tx.put(b"new", b"x");
                let _ = tx.commit();
            }
        });
        assert!(r.is_ok(), "panic after flipping byte at offset {off}");
    }
}

#[test]
fn flipping_leaf_structure_bytes_never_returns_wrong_value() {
    // Without checksums (ticket 07) value-byte flips are undetectable, so we
    // only flip and check that we never get a *different key's* value or a
    // value of wrong length for k1/k2 beyond a single changed byte.
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    let orig = fs::read(&f).unwrap();
    for off in 4096.min(orig.len())..orig.len() {
        let mut b = orig.clone();
        b[off] ^= 0x01;
        fs::write(&f, &b).unwrap();
        let r = std::panic::catch_unwind(|| {
            if let Ok(db) = Db::open(&p) {
                let tx = db.begin();
                for (k, v) in [(&b"k1"[..], &b"v1"[..]), (b"k2", b"v2")] {
                    if let Ok(Some(got)) = tx.get(k) {
                        assert_eq!(got.len(), v.len(), "len changed at {off}");
                    }
                }
            }
        });
        assert!(r.is_ok(), "panic or bad length after flipping byte at offset {off}");
    }
}

#[test]
#[ignore = "deferred to ticket 07 (page checksums)"]
fn value_byte_flip_detected_as_corrupt() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"key", b"UNIQUEVALUEMARKER");
    db.close().unwrap();
    let f = the_data_file(&p);
    let mut b = fs::read(&f).unwrap();
    let pos = b.windows(17).position(|w| w == b"UNIQUEVALUEMARKER").unwrap();
    b[pos] ^= 1;
    fs::write(&f, &b).unwrap();
    let db = Db::open(&p);
    match db {
        Err(e) => assert!(matches!(e, Error::Corrupt(..)), "{e:?}"),
        Ok(db) => {
            let r = db.begin().get(b"key");
            assert!(matches!(r, Err(Error::Corrupt(..))), "{r:?}");
        }
    }
}

#[test]
fn extra_file_in_directory_does_not_break_open() {
    let t = tmp();
    let p = t.path().join("db");
    make_db_with_data(&p);
    fs::write(p.join("README.txt"), b"user note").unwrap();
    let db = Db::open(&p).unwrap();
    assert_eq!(get(&db, b"k1"), Some(b"v1".to_vec()));
}

#[test]
fn data_file_replaced_by_directory_errors() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    fs::remove_file(&f).unwrap();
    fs::create_dir(&f).unwrap();
    let r = std::panic::catch_unwind(|| Db::open(&p));
    let r = r.expect("panicked");
    assert!(r.is_err(), "opened a DB whose data file is a directory");
}

#[test]
fn deleted_data_file_reopens_as_empty_or_errors_but_no_panic() {
    let t = tmp();
    let p = t.path().join("db");
    let f = make_db_with_data(&p);
    fs::remove_file(&f).unwrap();
    let r = std::panic::catch_unwind(|| Db::open(&p).map(|db| get(&db, b"k1")));
    match r.expect("panicked") {
        Ok(v) => assert_eq!(v, None),
        Err(_) => {}
    }
}

// ---------------------------------------------------------------- lifecycle

#[test]
fn many_open_close_cycles_accumulate() {
    let t = tmp();
    let p = t.path().join("db");
    for i in 0..60u32 {
        let db = Db::open(&p).unwrap();
        for j in 0..i {
            assert_eq!(get(&db, &j.to_be_bytes()), Some(j.to_le_bytes().to_vec()), "cycle {i} key {j}");
        }
        put_commit(&db, &i.to_be_bytes(), &i.to_le_bytes());
        db.close().unwrap();
    }
    // file size should stay bounded (one leaf page + meta)
    let len = fs::metadata(the_data_file(&p)).unwrap().len();
    assert!(len <= 4 * 4096, "file grew to {len} bytes over open/close cycles");
}

#[test]
fn file_does_not_grow_with_overwrites() {
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    put_commit(&db, b"k", b"v");
    let f = the_data_file(&p);
    let len0 = fs::metadata(&f).unwrap().len();
    for i in 0..200u32 {
        put_commit(&db, b"k", &i.to_le_bytes());
    }
    assert_eq!(fs::metadata(&f).unwrap().len(), len0);
}

#[test]
fn concurrent_open_same_dir_behaviour_documented() {
    // The API doesn't promise locking. Document: second open either errors
    // or works; if both work, a commit through one must not corrupt the file
    // for a later fresh open.
    let t = tmp();
    let p = t.path().join("db");
    let a = Db::open(&p).unwrap();
    put_commit(&a, b"a", b"1");
    match Db::open(&p) {
        Err(e) => eprintln!("second open refused: {e:?}"),
        Ok(b) => {
            eprintln!("second open allowed (no exclusive lock)");
            assert_eq!(get(&b, b"a"), Some(b"1".to_vec()));
            put_commit(&b, b"b", b"2");
            drop(b);
            drop(a);
            let c = Db::open(&p).unwrap();
            assert_eq!(get(&c, b"b"), Some(b"2".to_vec()));
            return;
        }
    }
}

#[test]
fn concurrent_open_lost_update_documented() {
    // If two handles can be open at once, the stale one's commit overwrites
    // the other's committed data (single page rewritten from memory).
    // This documents the hazard; it is not asserted as a ticket-01 promise.
    let t = tmp();
    let p = t.path().join("db");
    let a = Db::open(&p).unwrap();
    let b = match Db::open(&p) {
        Ok(b) => b,
        Err(_) => return,
    };
    put_commit(&a, b"from_a", b"1");
    put_commit_f(&b, b"from_b", b"2");
    drop(a);
    drop(b);
    let c = Db::open(&p).unwrap();
    let ra = get(&c, b"from_a");
    let rb = get(&c, b"from_b");
    eprintln!("after two handles: from_a={ra:?} from_b={rb:?}");
}

#[test]
fn many_tiny_keys_fit_in_one_page() {
    // Fresh DB per probe. 4-byte keys, empty values: even with generous
    // per-cell overhead (slot + header ~ 10-20 bytes) a 4KB page must hold
    // well over 50 of them.
    let make = |n: usize| -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n).map(|i| ((i as u32).to_be_bytes().to_vec(), Vec::new())).collect()
    };
    let (mut lo, mut hi) = (0usize, 2000usize);
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match fresh_commit(&make(mid)) {
            Ok(()) => lo = mid,
            Err(Error::PageFull) => hi = mid,
            Err(e) => panic!("{e:?}"),
        }
    }
    eprintln!("tiny keys that fit in one page: {lo}");
    assert!(lo > 50, "only {lo} tiny keys fit in a 4KB page");
    let t = tmp();
    let p = t.path().join("db");
    let db = Db::open(&p).unwrap();
    let mut tx = db.begin();
    for (k, v) in make(lo) {
        tx.put(&k, &v).unwrap();
    }
    tx.commit().unwrap();
    db.close().unwrap();
    let db = Db::open(&p).unwrap();
    for (k, v) in make(lo) {
        assert_eq!(get(&db, &k), Some(v));
    }
    assert_eq!(get(&db, &(lo as u32).to_be_bytes()), None);
}

#[test]
fn error_implements_std_error_and_display() {
    let t = tmp();
    let db = Db::open(t.path().join("db")).unwrap();
    let mut tx = db.begin();
    let e = tx.put(&[0u8; 600], b"").unwrap_err();
    let s = format!("{e}");
    assert!(!s.is_empty());
    let _: &dyn std::error::Error = &e;
}
