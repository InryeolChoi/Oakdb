//! Reading a range of keys in order through the engine API.

use std::ops::Bound::{self, Excluded, Included, Unbounded};

use oakdb::{Db, Txn, MAX_ENTRY_LEN, MAX_KEY_LEN};

/// The keys `scan` yields, as strings, checking that every item is `Ok`.
fn keys(scan: oakdb::Scan<'_>) -> Vec<String> {
    scan.map(|item| String::from_utf8(item.unwrap().0).unwrap())
        .collect()
}

/// A database holding keys "b", "d", "f" and "h", each with its own name as
/// the value.
fn four_keys() -> (tempfile::TempDir, Db) {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    for key in ["h", "b", "f", "d"] {
        txn.put(key.as_bytes(), key.as_bytes()).unwrap();
    }
    txn.commit().unwrap();
    (root, db)
}

#[test]
fn a_scan_yields_keys_and_values_in_key_order() {
    let (_root, db) = four_keys();
    let txn = db.begin();

    let items: Vec<(Vec<u8>, Vec<u8>)> = txn.scan::<&[u8]>(..).map(Result::unwrap).collect();

    assert_eq!(
        items,
        [b"b", b"d", b"f", b"h"].map(|k| (k.to_vec(), k.to_vec()))
    );
}

#[test]
fn each_end_of_a_range_can_include_or_exclude_its_key() {
    let (_root, db) = four_keys();
    let txn = db.begin();

    assert_eq!(keys(txn.scan("d".."h")), ["d", "f"]);
    assert_eq!(keys(txn.scan("d"..="h")), ["d", "f", "h"]);
    assert_eq!(keys(txn.scan("d"..)), ["d", "f", "h"]);
    assert_eq!(keys(txn.scan(.."f")), ["b", "d"]);
    assert_eq!(keys(txn.scan(..="f")), ["b", "d", "f"]);
    assert_eq!(
        keys(txn.scan::<&str>((Excluded("d"), Included("h")))),
        ["f", "h"]
    );
    assert_eq!(
        keys(txn.scan::<&str>((Excluded("d"), Excluded("h")))),
        ["f"]
    );
    assert_eq!(
        keys(txn.scan::<&str>((Excluded("d"), Unbounded))),
        ["f", "h"]
    );
}

#[test]
fn ends_between_keys_and_outside_all_keys_work() {
    let (_root, db) = four_keys();
    let txn = db.begin();

    assert_eq!(keys(txn.scan("c".."g")), ["d", "f"]);
    assert_eq!(keys(txn.scan("a".."z")), ["b", "d", "f", "h"]);
    assert_eq!(keys(txn.scan("i".."z")), Vec::<String>::new());
    assert_eq!(keys(txn.scan("a".."b")), Vec::<String>::new());
}

#[test]
fn an_empty_or_backwards_range_yields_nothing() {
    let (_root, db) = four_keys();
    let txn = db.begin();

    assert!(keys(txn.scan("f".."d")).is_empty());
    assert!(keys(txn.scan("d".."d")).is_empty());
    assert!(keys(txn.scan::<&str>((Excluded("d"), Excluded("d")))).is_empty());
    assert!(keys(txn.scan::<&str>((Excluded("d"), Included("d")))).is_empty());
    assert_eq!(keys(txn.scan("d"..="d")), ["d"]);
}

#[test]
fn an_empty_database_scans_to_nothing() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    assert!(keys(db.begin().scan::<&[u8]>(..)).is_empty());
}

#[test]
fn a_scan_sees_the_transactions_own_writes() {
    let (_root, db) = four_keys();
    let mut txn = db.begin();
    txn.put(b"a", b"new").unwrap();
    txn.put(b"d", b"changed").unwrap();
    txn.delete(b"f").unwrap();
    txn.put(b"g", b"new").unwrap();
    txn.delete(b"x").unwrap();

    let items: Vec<(Vec<u8>, Vec<u8>)> = txn.scan::<&[u8]>(..).map(Result::unwrap).collect();

    let expected: [(&[u8], &[u8]); 5] = [
        (b"a", b"new"),
        (b"b", b"b"),
        (b"d", b"changed"),
        (b"g", b"new"),
        (b"h", b"h"),
    ];
    assert_eq!(items, expected.map(|(k, v)| (k.to_vec(), v.to_vec())));
    // Other transactions do not see them.
    assert_eq!(keys(db.begin().scan::<&[u8]>(..)), ["b", "d", "f", "h"]);
}

#[test]
fn a_scan_keeps_reading_the_tree_it_started_on_while_others_commit() {
    let (_root, db) = four_keys();
    let reader = db.begin();
    let mut scan = reader.scan::<&[u8]>(..);
    assert_eq!(scan.next().unwrap().unwrap().0, b"b");

    let mut writer = db.begin();
    writer.put(b"c", b"new").unwrap();
    writer.delete(b"f").unwrap();
    writer.commit().unwrap();

    let rest: Vec<Vec<u8>> = scan.map(|item| item.unwrap().0).collect();
    assert_eq!(rest, [b"d".to_vec(), b"f".to_vec(), b"h".to_vec()]);
}

/// A key that sorts in the order of `i`, `len` bytes long.
fn long_key(i: u32, len: usize) -> Vec<u8> {
    let mut key = vec![b'k'; len];
    key[..4].copy_from_slice(&i.to_be_bytes());
    key
}

/// Puts keys `0..n` of length `len` with values as long as they can be, 50
/// per commit, so they spread over many leaves and several levels.
fn many_leaves(db: &Db, n: u32, len: usize) {
    let keys: Vec<u32> = (0..n).map(|i| i * 7919 % n).collect();
    for chunk in keys.chunks(50) {
        let mut txn = db.begin();
        for &i in chunk {
            txn.put(&long_key(i, len), &vec![i as u8; MAX_ENTRY_LEN - len])
                .unwrap();
        }
        txn.commit().unwrap();
    }
}

fn indexes(txn: &Txn<'_>, from: Bound<u32>, to: Bound<u32>, len: usize) -> Vec<u32> {
    let bound = |b: Bound<u32>| b.map(|i| long_key(i, len));
    txn.scan((bound(from), bound(to)))
        .map(|item| {
            let (key, value) = item.unwrap();
            assert_eq!(key.len(), len);
            let i = u32::from_be_bytes(key[..4].try_into().unwrap());
            assert_eq!(value, vec![i as u8; MAX_ENTRY_LEN - len]);
            i
        })
        .collect()
}

#[test]
fn a_range_over_many_leaves_follows_them_from_left_to_right() {
    // Four entries fill a leaf, so 1000 keys spread over about 300 leaves
    // in a five-level tree.
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    many_leaves(&db, 1000, MAX_KEY_LEN);
    let txn = db.begin();

    assert_eq!(
        indexes(&txn, Unbounded, Unbounded, MAX_KEY_LEN),
        (0..1000).collect::<Vec<_>>()
    );
    assert_eq!(
        indexes(&txn, Included(123), Excluded(877), MAX_KEY_LEN),
        (123..877).collect::<Vec<_>>()
    );
    assert_eq!(
        indexes(&txn, Excluded(500), Included(503), MAX_KEY_LEN),
        [501, 502, 503]
    );
}

#[test]
fn a_range_skips_leaves_that_deletes_emptied() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    many_leaves(&db, 400, MAX_KEY_LEN);
    let mut txn = db.begin();
    for i in 100..300 {
        txn.delete(&long_key(i, MAX_KEY_LEN)).unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();

    let expected: Vec<u32> = (0..100).chain(300..400).collect();
    assert_eq!(indexes(&txn, Unbounded, Unbounded, MAX_KEY_LEN), expected);
    assert!(indexes(&txn, Included(100), Excluded(300), MAX_KEY_LEN).is_empty());
}

#[test]
fn a_scan_can_stop_early() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    many_leaves(&db, 300, 8);
    let txn = db.begin();

    let first: Vec<u32> = txn
        .scan(long_key(42, 8)..)
        .take(3)
        .map(|item| u32::from_be_bytes(item.unwrap().0[..4].try_into().unwrap()))
        .collect();

    assert_eq!(first, [42, 43, 44]);
}
