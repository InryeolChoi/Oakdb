//! Values too long for a leaf go to overflow pages, and pages that fall out
//! of use go on a free list that later writes take from before the file
//! grows (#18).

use std::path::Path;

use oakdb::{Db, DATA_FILE, MAX_ENTRY_LEN, MAX_KEY_LEN};

/// Value bytes one overflow page holds: a 4 KB page minus its header.
const PER_PAGE: usize = 4096 - 40;

/// How many 4 KB pages the data file has.
fn pages_in_file(dir: &Path) -> u64 {
    std::fs::metadata(dir.join(DATA_FILE)).unwrap().len() / 4096
}

/// A value of `len` bytes that differs at every position and with every
/// `seed`, so a page read out of order or from the wrong chain shows.
fn value(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3] ^ seed)
        .collect()
}

fn put(db: &Db, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn delete(db: &Db, key: &[u8]) {
    let mut txn = db.begin();
    txn.delete(key).unwrap();
    txn.commit().unwrap();
}

fn get(db: &Db, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

#[test]
fn a_value_of_100_kb_round_trips_and_survives_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let big = value(100 * 1024, 1);

    put(&db, b"photo", &big);

    assert_eq!(get(&db, b"photo"), Some(big.clone()));
    db.close().unwrap();
    assert_eq!(get(&Db::open(root.path()).unwrap(), b"photo"), Some(big));
}

#[test]
fn values_on_each_side_of_every_boundary_round_trip() {
    // The leaf limit for a 1-byte key, then one, two and many overflow
    // pages, each just full and one byte over.
    let lens = [
        MAX_ENTRY_LEN - 1,
        MAX_ENTRY_LEN,
        PER_PAGE,
        PER_PAGE + 1,
        2 * PER_PAGE,
        2 * PER_PAGE + 1,
        25 * PER_PAGE + 7,
    ];
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    for (i, &len) in lens.iter().enumerate() {
        put(&db, &[i as u8], &value(len, i as u8));
    }
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    for (i, &len) in lens.iter().enumerate() {
        assert_eq!(
            get(&db, &[i as u8]),
            Some(value(len, i as u8)),
            "{len} bytes"
        );
    }
}

#[test]
fn a_long_value_takes_only_the_pages_its_length_needs() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let before = pages_in_file(root.path());

    put(&db, b"k", &value(2 * PER_PAGE + 1, 0));

    assert_eq!(pages_in_file(root.path()), before + 3);
}

#[test]
fn replacing_a_long_value_reuses_its_pages() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put(&db, b"k", &value(100 * 1024, 0));
    let pages = pages_in_file(root.path());

    for seed in 1..6 {
        put(&db, b"k", &value(100 * 1024, seed));
        assert_eq!(get(&db, b"k"), Some(value(100 * 1024, seed)));
    }

    assert_eq!(pages_in_file(root.path()), pages);
}

#[test]
fn deleting_a_long_value_frees_its_pages_for_the_next_one() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put(&db, b"a", &value(50 * 1024, 1));
    let pages = pages_in_file(root.path());

    delete(&db, b"a");
    put(&db, b"b", &value(50 * 1024, 2));

    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(value(50 * 1024, 2)));
    assert_eq!(pages_in_file(root.path()), pages);
}

#[test]
fn freed_pages_are_still_free_after_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put(&db, b"a", &value(50 * 1024, 1));
    let pages = pages_in_file(root.path());
    delete(&db, b"a");
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    put(&db, b"b", &value(50 * 1024, 2));

    assert_eq!(pages_in_file(root.path()), pages);
    db.close().unwrap();
    assert_eq!(
        get(&Db::open(root.path()).unwrap(), b"b"),
        Some(value(50 * 1024, 2))
    );
}

#[test]
fn a_freed_page_keeps_nothing_of_the_value_it_held() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let secret: Vec<u8> = b"oak-secret-".repeat(1000);
    put(&db, b"k", &secret);

    delete(&db, b"k");

    let file = std::fs::read(root.path().join(DATA_FILE)).unwrap();
    assert!(!file.windows(11).any(|w| w == b"oak-secret-"));
}

#[test]
fn a_long_value_can_shrink_back_into_its_leaf_and_give_its_pages_back() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put(&db, b"k", &value(30 * 1024, 1));
    let pages = pages_in_file(root.path());

    put(&db, b"k", b"short");
    put(&db, b"other", &value(30 * 1024, 2));

    assert_eq!(get(&db, b"k"), Some(b"short".to_vec()));
    assert_eq!(get(&db, b"other"), Some(value(30 * 1024, 2)));
    assert_eq!(pages_in_file(root.path()), pages);
}

#[test]
fn the_longest_key_can_hold_a_long_value() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let key = [9; MAX_KEY_LEN];

    put(&db, &key, &value(10_000, 3));

    db.close().unwrap();
    assert_eq!(
        get(&Db::open(root.path()).unwrap(), &key),
        Some(value(10_000, 3))
    );
}

#[test]
fn many_long_values_split_their_leaves_like_short_ones() {
    // A leaf cell for a long value is 4 + 4 + 4 + 12 bytes, so about 170
    // fit in a leaf; 600 need several leaves and a root.
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    for chunk in (0u32..600).collect::<Vec<_>>().chunks(50) {
        let mut txn = db.begin();
        for &i in chunk {
            txn.put(&i.to_be_bytes(), &value(1500 + i as usize, i as u8))
                .unwrap();
        }
        txn.commit().unwrap();
    }
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    let scanned: Vec<(Vec<u8>, Vec<u8>)> = txn.scan::<&[u8]>(..).map(Result::unwrap).collect();
    assert_eq!(scanned.len(), 600);
    for (i, (key, val)) in (0u32..).zip(scanned) {
        assert_eq!(key, i.to_be_bytes());
        assert_eq!(val, value(1500 + i as usize, i as u8), "key {i}");
    }
}

#[test]
fn a_transaction_sees_its_own_long_value_and_a_rollback_leaves_no_pages() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put(&db, b"b", &value(9000, 1));
    let pages = pages_in_file(root.path());

    let mut txn = db.begin();
    txn.put(b"a", &value(20_000, 2)).unwrap();
    assert_eq!(txn.get(b"a").unwrap(), Some(value(20_000, 2)));
    let scanned: Vec<(Vec<u8>, Vec<u8>)> = txn.scan::<&[u8]>(..).map(Result::unwrap).collect();
    assert_eq!(
        scanned,
        [
            (b"a".to_vec(), value(20_000, 2)),
            (b"b".to_vec(), value(9000, 1))
        ]
    );
    txn.rollback();

    assert_eq!(get(&db, b"a"), None);
    assert_eq!(pages_in_file(root.path()), pages);
}

#[test]
fn a_scan_started_before_a_commit_still_reads_the_long_value_it_saw() {
    // The commit frees the old value's pages and reuses them at once; the
    // scan keeps the tree, and so the pages, it started on. The scan reads
    // one entry ahead, so the long value is the third: still unread when
    // the commit lands.
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    put(&db, b"a", b"1");
    put(&db, b"b", b"2");
    put(&db, b"c", &value(40_000, 1));
    let reader = db.begin();
    let mut scan = reader.scan::<&[u8]>(..);
    assert_eq!(scan.next().unwrap().unwrap().0, b"a");

    let mut txn = db.begin();
    txn.delete(b"c").unwrap();
    txn.put(b"d", &value(40_000, 2)).unwrap();
    txn.commit().unwrap();

    assert_eq!(scan.next().unwrap().unwrap().0, b"b");
    assert_eq!(
        scan.next().unwrap().unwrap(),
        (b"c".to_vec(), value(40_000, 1))
    );
    assert!(scan.next().is_none());
}
