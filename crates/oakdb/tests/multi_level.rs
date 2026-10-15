//! Trees of any height: internal pages split as the tree grows, and leaves
//! that empty out leave the tree as it shrinks.

use std::path::Path;

use oakdb::{Db, DATA_FILE, MAX_ENTRY_LEN, MAX_KEY_LEN};

/// How many 4 KB pages the data file has: two meta pages, then the tree.
fn pages_in_file(dir: &Path) -> u64 {
    std::fs::metadata(dir.join(DATA_FILE)).unwrap().len() / 4096
}

/// The biggest entry there is: a 512-byte key and a 494-byte value. Four of
/// them fill a leaf, and an internal page has room for only 7 separators of
/// 512 bytes, so a two-level tree holds about 20 of them at most.
fn big_key(i: u32) -> Vec<u8> {
    let mut key = vec![0; MAX_KEY_LEN];
    key[..4].copy_from_slice(&i.to_be_bytes());
    key
}

fn big_value(i: u32) -> Vec<u8> {
    let mut value = vec![i as u8; MAX_ENTRY_LEN - MAX_KEY_LEN];
    value[..4].copy_from_slice(&i.to_le_bytes());
    value
}

/// Puts big entries `keys`, ten per commit.
fn put_big(db: &Db, keys: impl IntoIterator<Item = u32>) {
    let keys: Vec<u32> = keys.into_iter().collect();
    for chunk in keys.chunks(10) {
        let mut txn = db.begin();
        for &i in chunk {
            txn.put(&big_key(i), &big_value(i)).unwrap();
        }
        txn.commit().unwrap();
    }
}

fn delete_big(db: &Db, keys: impl IntoIterator<Item = u32>) {
    let mut txn = db.begin();
    for i in keys {
        txn.delete(&big_key(i)).unwrap();
    }
    txn.commit().unwrap();
}

/// Checks that exactly the big entries `present` out of `0..n` are there.
fn check_big(db: &Db, n: u32, present: impl Fn(u32) -> bool) {
    let txn = db.begin();
    for i in 0..n {
        let expected = present(i).then(|| big_value(i));
        assert_eq!(txn.get(&big_key(i)).unwrap(), expected, "key {i}");
    }
}

#[test]
fn the_tree_grows_past_two_levels_when_the_root_fills_up() {
    // 03 stopped at about 20 of these entries with `PageFull`. 1000 need
    // four levels: 250 leaves, under about 40 internal pages, under about
    // 6, under the root.
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    put_big(&db, (0..1000).map(|i| i * 7919 % 1000));

    check_big(&db, 1001, |i| i < 1000);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    check_big(&db, 1001, |i| i < 1000);
}

#[test]
fn ascending_and_descending_puts_grow_a_tree_that_reads_back_the_same() {
    for order in [
        (0..600).collect::<Vec<u32>>(),
        (0..600).rev().collect::<Vec<u32>>(),
    ] {
        let root = tempfile::tempdir().unwrap();
        let db = Db::open(root.path()).unwrap();

        put_big(&db, order);

        db.close().unwrap();
        let db = Db::open(root.path()).unwrap();
        check_big(&db, 600, |_| true);
    }
}

#[test]
fn a_tree_emptied_of_every_key_is_empty_after_reopening_and_grows_again() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put_big(&db, 0..300);

    delete_big(&db, 0..300);

    check_big(&db, 300, |_| false);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    check_big(&db, 300, |_| false);
    put_big(&db, 100..200);
    check_big(&db, 300, |i| (100..200).contains(&i));
}

#[test]
fn deleting_whole_leaves_in_the_middle_keeps_the_rest() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put_big(&db, 0..400);
    let kept = |i: u32| !(50..350).contains(&i) || i.is_multiple_of(97);

    delete_big(&db, (50..350).filter(|&i| !kept(i)));

    check_big(&db, 400, kept);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    check_big(&db, 400, kept);
    // The keys in the gap go back in, to whatever leaves now cover them.
    put_big(&db, 50..350);
    check_big(&db, 400, |_| true);
}

#[test]
fn deleting_and_putting_in_one_commit_across_many_leaves_takes_effect_together() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put_big(&db, 0..200);

    let mut txn = db.begin();
    for i in 0..200 {
        if i % 2 == 0 {
            txn.delete(&big_key(i)).unwrap();
        }
    }
    for i in 200..300 {
        txn.put(&big_key(i), &big_value(i)).unwrap();
    }
    txn.commit().unwrap();

    let present = |i: u32| i >= 200 || i % 2 == 1;
    check_big(&db, 300, present);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    check_big(&db, 300, present);
}

#[test]
fn a_tree_that_shrank_does_not_reuse_or_lose_pages_in_the_file() {
    // Pages that leave the tree stay in the file for now; reusing them
    // comes later. The file must never shrink under the tree.
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put_big(&db, 0..300);
    let grown = pages_in_file(root.path());

    delete_big(&db, 0..300);

    assert_eq!(pages_in_file(root.path()), grown);
}
