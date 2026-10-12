//! Splitting a full leaf into two leaves under a root internal page.

use std::path::Path;

use oakdb::{Db, Error, DATA_FILE, MAX_ENTRY_LEN, MAX_KEY_LEN};

/// How many 4 KB pages the data file has: two meta pages, then the tree.
fn pages_in_file(dir: &Path) -> u64 {
    std::fs::metadata(dir.join(DATA_FILE)).unwrap().len() / 4096
}

/// A 4-byte key that sorts in the order of `i`.
fn key(i: u32) -> [u8; 4] {
    i.to_be_bytes()
}

/// A 100-byte value that says which key it belongs to.
fn value(i: u32) -> Vec<u8> {
    let mut value = vec![i as u8; 100];
    value[..4].copy_from_slice(&i.to_le_bytes());
    value
}

// An entry of a 4-byte key and a 100-byte value takes 4 (slot) + 4 (cell
// header) + 4 + 100 = 112 bytes, so 36 of them fill the 4056 bytes of a
// page after its header, and the 37th does not fit.

#[test]
fn a_full_leaf_splits_into_two_leaves_under_a_new_root() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    for i in 0..36 {
        txn.put(&key(i), &value(i)).unwrap();
    }
    txn.commit().unwrap();
    assert_eq!(pages_in_file(root.path()), 3);

    let mut txn = db.begin();
    txn.put(&key(36), &value(36)).unwrap();
    txn.commit().unwrap();

    // The old leaf, a new right leaf and the new root.
    assert_eq!(pages_in_file(root.path()), 5);
    let txn = db.begin();
    for i in 0..37 {
        assert_eq!(txn.get(&key(i)).unwrap(), Some(value(i)), "key {i}");
    }
    assert_eq!(txn.get(&key(37)).unwrap(), None);
}

#[test]
fn a_split_tree_is_the_same_after_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    for i in 0..37 {
        txn.put(&key(i), &value(i)).unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();

    assert_eq!(pages_in_file(root.path()), 5);
    let txn = db.begin();
    for i in 0..37 {
        assert_eq!(txn.get(&key(i)).unwrap(), Some(value(i)), "key {i}");
    }
}

/// Puts keys `order` yields, 50 per commit, then checks every one of them,
/// once before and once after reopening.
fn put_in_order_and_check(order: impl Iterator<Item = u32>) {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let keys: Vec<u32> = order.collect();
    for chunk in keys.chunks(50) {
        let mut txn = db.begin();
        for &i in chunk {
            txn.put(&key(i), &value(i)).unwrap();
        }
        txn.commit().unwrap();
    }
    let check = |db: &Db| {
        let txn = db.begin();
        for &i in &keys {
            assert_eq!(txn.get(&key(i)).unwrap(), Some(value(i)), "key {i}");
        }
        assert_eq!(txn.get(&key(u32::MAX)).unwrap(), None);
    };
    check(&db);
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    check(&db);
    assert!(pages_in_file(root.path()) > 20);
}

#[test]
fn every_key_is_found_after_many_splits_in_ascending_order() {
    put_in_order_and_check(0..1000);
}

#[test]
fn every_key_is_found_after_many_splits_in_descending_order() {
    put_in_order_and_check((0..1000).rev());
}

#[test]
fn every_key_is_found_after_many_splits_in_scattered_order() {
    put_in_order_and_check((0..1000).map(|i| i * 7919 % 1000));
}

#[test]
fn a_value_is_refused_if_four_entries_of_its_size_would_not_fit_in_a_page() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();

    txn.put(b"k", &[1; MAX_ENTRY_LEN - 1]).unwrap();
    let err = txn.put(b"k", &[1; MAX_ENTRY_LEN]).unwrap_err();
    assert!(
        matches!(
            err,
            Error::ValueTooLarge {
                len: 1006,
                max: 1005
            }
        ),
        "{err:?}"
    );
    let err = txn.put(&[7; MAX_KEY_LEN], &[1; 495]).unwrap_err();
    assert!(
        matches!(err, Error::ValueTooLarge { len: 495, max: 494 }),
        "{err:?}"
    );
    txn.commit().unwrap();

    assert_eq!(
        db.begin().get(b"k").unwrap(),
        Some(vec![1; MAX_ENTRY_LEN - 1])
    );
}

/// The biggest entry there is: a 512-byte key and a 494-byte value. Four of
/// them fill a leaf, and the root page has room for 7 separators of 512
/// bytes, so about 20 of them fill the two-level tree.
fn big_key(i: u32) -> Vec<u8> {
    let mut key = vec![0; MAX_KEY_LEN];
    key[..4].copy_from_slice(&i.to_be_bytes());
    key
}

#[test]
fn a_commit_that_overflows_the_root_fails_and_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let big_value = [9; MAX_ENTRY_LEN - MAX_KEY_LEN];
    let mut n = 0;
    loop {
        let mut txn = db.begin();
        txn.put(&big_key(n), &big_value).unwrap();
        match txn.commit() {
            Ok(()) => n += 1,
            Err(Error::PageFull) => break,
            Err(e) => panic!("{e}"),
        }
        assert!(n < 100, "the root never filled up");
    }
    let pages = pages_in_file(root.path());

    // The delete frees space in the first leaf, but the new key goes to the
    // last one, which still has to split.
    let mut txn = db.begin();
    txn.delete(&big_key(0)).unwrap();
    txn.put(&big_key(n), &big_value).unwrap();
    let err = txn.commit().unwrap_err();

    assert!(matches!(err, Error::PageFull), "{err:?}");
    let check = |db: &Db| {
        let txn = db.begin();
        assert_eq!(txn.get(&big_key(0)).unwrap(), Some(big_value.to_vec()));
        assert_eq!(txn.get(&big_key(n)).unwrap(), None);
    };
    check(&db);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    check(&db);
    assert_eq!(pages_in_file(root.path()), pages);
}
