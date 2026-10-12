//! Blind tests for #15 (first split: a two-level tree).
//!
//! Written only from the issue, `docs/design/0003-first-split.md`, the ADRs,
//! `docs/research/0005-btree-page-layout.md` and the public API, without
//! reading the implementation. The on-disk checks use only the documented
//! layout:
//!
//! - the data file is an array of 4096-byte pages; pages 0 and 1 are meta
//!   slots, a new database has its root leaf at page 2 (design 0001 §2);
//! - meta page: `root_page` u64 at 20..28, `page_count` u64 at 28..36;
//! - tree page header: `kind` u8 at 12 (1 leaf, 2 internal), `cell_count`
//!   u16 at 14, `right_sibling` u64 at 24..32, `rightmost_child` u64 at
//!   32..40, slots `[offset u16, len u16]` from 40;
//! - a cell is `key_len u16 | value_len u16 | key | value`; in an internal
//!   page the value is the child page number (u64), and the cell `(K, child)`
//!   means "keys below K go to child" (design 0003 §2).
//!
//! All integers are little-endian.

use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oakdb::{Db, Error, DATA_FILE, MAX_ENTRY_LEN, MAX_KEY_LEN};

const PAGE: u64 = 4096;
const KIND_LEAF: u8 = 1;
const KIND_INTERNAL: u8 = 2;

// ---------------------------------------------------------------- helpers

fn data_path(dir: &Path) -> std::path::PathBuf {
    dir.join(DATA_FILE)
}

fn file_pages(dir: &Path) -> u64 {
    let len = std::fs::metadata(data_path(dir)).unwrap().len();
    assert_eq!(len % PAGE, 0, "data file is not a whole number of pages");
    len / PAGE
}

fn read_file(dir: &Path) -> Vec<u8> {
    std::fs::read(data_path(dir)).unwrap()
}

fn write_file(dir: &Path, bytes: &[u8]) {
    std::fs::write(data_path(dir), bytes).unwrap();
}

fn page(bytes: &[u8], no: u64) -> &[u8] {
    let start = (no * PAGE) as usize;
    &bytes[start..start + PAGE as usize]
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn put_u64(bytes: &mut [u8], at: usize, v: u64) {
    bytes[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn meta_root(bytes: &[u8]) -> u64 {
    u64_at(page(bytes, 0), 20)
}

fn meta_page_count(bytes: &[u8]) -> u64 {
    u64_at(page(bytes, 0), 28)
}

fn kind(bytes: &[u8], no: u64) -> u8 {
    page(bytes, no)[12]
}

fn cell_count(bytes: &[u8], no: u64) -> u16 {
    u16_at(page(bytes, no), 14)
}

fn right_sibling(bytes: &[u8], no: u64) -> u64 {
    u64_at(page(bytes, no), 24)
}

fn rightmost_child(bytes: &[u8], no: u64) -> u64 {
    u64_at(page(bytes, no), 32)
}

/// Absolute file offset of cell `i` of page `no`, from its slot.
fn cell_offset(bytes: &[u8], no: u64, i: usize) -> usize {
    let p = page(bytes, no);
    let slot = 40 + 4 * i;
    (no * PAGE) as usize + u16_at(p, slot) as usize
}

/// Key and child page number of cell `i` of internal page `no`.
fn internal_cell(bytes: &[u8], no: u64, i: usize) -> (Vec<u8>, u64, usize) {
    let at = cell_offset(bytes, no, i);
    let key_len = u16_at(bytes, at) as usize;
    let value_len = u16_at(bytes, at + 2) as usize;
    assert_eq!(value_len, 8, "an internal cell's value is a u64 child");
    let key = bytes[at + 4..at + 4 + key_len].to_vec();
    let child_at = at + 4 + key_len;
    (key, u64_at(bytes, child_at), child_at)
}

fn key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

fn value(i: usize, len: usize) -> Vec<u8> {
    let mut v = format!("v{i}-").into_bytes();
    v.resize(len, b'a' + (i % 26) as u8);
    v
}

/// A deterministic permutation of `0..n`.
fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut s = seed;
    for i in (1..n).rev() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (s >> 33) as usize % (i + 1);
        v.swap(i, j);
    }
    v
}

/// Opens in a thread so a hang becomes a test failure, and a panic is
/// reported as such instead of passing as an error.
fn open_outcome(dir: &Path) -> Result<(), Error> {
    let dir = dir.to_path_buf();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let r = Db::open(&dir).and_then(|db| {
            // If it opens, reading everything must not panic or hang either.
            {
                let txn = db.begin();
                for i in 0..2000 {
                    txn.get(&key(i))?;
                }
            }
            db.close()
        });
        let _ = tx.send(r);
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(r) => {
            handle.join().unwrap();
            r
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("opening a damaged file hung"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("opening a damaged file panicked: {:?}", handle.join().err())
        }
    }
}

fn assert_corrupt(dir: &Path, what: &str) {
    match open_outcome(dir) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("{what}: expected Error::Corrupt, got {other:?}"),
    }
}

/// Puts keys `0..n` with `value_len`-byte values, `per_commit` per commit.
fn fill(db: &Db, n: usize, value_len: usize, per_commit: usize) {
    for chunk in (0..n).collect::<Vec<_>>().chunks(per_commit) {
        let mut txn = db.begin();
        for &i in chunk {
            txn.put(&key(i), &value(i, value_len)).unwrap();
        }
        txn.commit().unwrap();
    }
}

fn assert_all(db: &Db, n: usize, value_len: usize) {
    let txn = db.begin();
    for i in 0..n {
        assert_eq!(
            txn.get(&key(i)).unwrap(),
            Some(value(i, value_len)),
            "key {i}"
        );
    }
}

/// A database that has gone through exactly one split: root internal page
/// over two leaves. Returns the directory.
fn one_split_db() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut i = 0;
    while file_pages(dir.path()) == 3 {
        let mut txn = db.begin();
        txn.put(&key(i), &value(i, 200)).unwrap();
        txn.commit().unwrap();
        i += 1;
        assert!(i < 100, "200-byte entries never split a leaf");
    }
    db.close().unwrap();
    dir
}

// ------------------------------------------------- splitting and structure

#[test]
fn a_new_database_has_meta_slots_and_one_leaf() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path()).unwrap().close().unwrap();
    assert_eq!(file_pages(dir.path()), 3);
    let bytes = read_file(dir.path());
    assert_eq!(meta_root(&bytes), 2);
    assert_eq!(kind(&bytes, 2), KIND_LEAF);
}

#[test]
fn an_overflowing_leaf_splits_under_a_new_root_internal_page() {
    let dir = one_split_db();
    // The split added the right leaf and the new root: 3 -> 5 pages.
    assert_eq!(file_pages(dir.path()), 5);
    let bytes = read_file(dir.path());
    assert_eq!(meta_page_count(&bytes), 5);

    let root = meta_root(&bytes);
    assert_ne!(root, 2, "the root must move to the new internal page");
    assert!(root == 3 || root == 4);
    assert_eq!(kind(&bytes, root), KIND_INTERNAL);
    assert_eq!(cell_count(&bytes, root), 1);
    assert_eq!(
        right_sibling(&bytes, root),
        0,
        "internal right_sibling is 0"
    );

    // Left half keeps page 2; the cell is (first key of R, 2); rightmost is R.
    let (sep, left, _) = internal_cell(&bytes, root, 0);
    assert_eq!(left, 2);
    let right = rightmost_child(&bytes, root);
    assert_ne!(right, root);
    assert!(right == 3 || right == 4);
    assert_eq!(kind(&bytes, 2), KIND_LEAF);
    assert_eq!(kind(&bytes, right), KIND_LEAF);

    // Sibling pointers: L -> R -> none. Leaves have no rightmost child.
    assert_eq!(right_sibling(&bytes, 2), right);
    assert_eq!(right_sibling(&bytes, right), 0);
    assert_eq!(rightmost_child(&bytes, 2), 0);
    assert_eq!(rightmost_child(&bytes, right), 0);

    // The separator is the right leaf's first key, verbatim.
    let first_right = {
        let at = cell_offset(&bytes, right, 0);
        let len = u16_at(&bytes, at) as usize;
        bytes[at + 4..at + 4 + len].to_vec()
    };
    assert_eq!(sep, first_right);
    assert!(cell_count(&bytes, 2) >= 1 && cell_count(&bytes, right) >= 1);
}

#[test]
fn every_later_split_adds_exactly_one_page_and_links_all_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut pages = file_pages(dir.path());
    let mut splits = 0;
    for i in 0..400 {
        let mut txn = db.begin();
        txn.put(&key(i), &value(i, 150)).unwrap();
        txn.commit().unwrap();
        let now = file_pages(dir.path());
        if now != pages {
            let expected = if pages == 3 { 5 } else { pages + 1 };
            assert_eq!(now, expected, "after inserting key {i}");
            splits += 1;
        }
        pages = now;
    }
    assert!(splits > 5, "only {splits} splits");
    db.close().unwrap();

    // Walk the root's children in order; the leaf chain must match.
    let bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    assert_eq!(kind(&bytes, root), KIND_INTERNAL);
    let mut children: Vec<u64> = (0..cell_count(&bytes, root) as usize)
        .map(|i| internal_cell(&bytes, root, i).1)
        .collect();
    children.push(rightmost_child(&bytes, root));
    assert_eq!(children.len() as u64, pages - 2 - 1);
    for w in children.windows(2) {
        assert_eq!(right_sibling(&bytes, w[0]), w[1]);
    }
    assert_eq!(right_sibling(&bytes, *children.last().unwrap()), 0);
    for &c in &children {
        assert_eq!(kind(&bytes, c), KIND_LEAF);
    }
}

// ------------------------------------------------------- reads and reopen

fn insert_in_order_and_check(order: &[usize], per_commit: usize) {
    let dir = tempfile::tempdir().unwrap();
    let n = order.len();
    {
        let db = Db::open(dir.path()).unwrap();
        for chunk in order.chunks(per_commit) {
            let mut txn = db.begin();
            for &i in chunk {
                txn.put(&key(i), &value(i, 100)).unwrap();
            }
            txn.commit().unwrap();
        }
        assert!(file_pages(dir.path()) > 5, "expected several splits");
        assert_all(&db, n, 100);
        let txn = db.begin();
        assert_eq!(txn.get(b"k").unwrap(), None);
        assert_eq!(txn.get(&key(n)).unwrap(), None);
        assert_eq!(txn.get(b"zzz").unwrap(), None);
        assert_eq!(txn.get(b"").unwrap(), None);
        drop(txn);
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_all(&db, n, 100);
    db.close().unwrap();
    let bytes = read_file(dir.path());
    assert_eq!(meta_page_count(&bytes), file_pages(dir.path()));
}

#[test]
fn ascending_inserts_one_per_commit_are_all_found_and_survive_reopen() {
    let order: Vec<usize> = (0..1000).collect();
    insert_in_order_and_check(&order, 1);
}

#[test]
fn descending_inserts_are_all_found_and_survive_reopen() {
    let order: Vec<usize> = (0..1000).rev().collect();
    insert_in_order_and_check(&order, 7);
}

#[test]
fn random_inserts_are_all_found_and_survive_reopen() {
    insert_in_order_and_check(&shuffled(1000, 42), 13);
}

#[test]
fn one_commit_can_split_many_times() {
    insert_in_order_and_check(&shuffled(1200, 7), 1200);
}

#[test]
fn separator_keys_route_to_the_right_side() {
    // Keys equal to a separator must be found (they live in the right leaf),
    // and keys between leaves must be absent.
    let dir = one_split_db();
    let bytes = read_file(dir.path());
    let (sep, _, _) = internal_cell(&bytes, meta_root(&bytes), 0);
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    assert!(txn.get(&sep).unwrap().is_some());
    let mut just_below = sep.clone();
    *just_below.last_mut().unwrap() -= 1;
    just_below.push(0xff);
    // `just_below` sorts between the separator's predecessor and it.
    assert_eq!(txn.get(&just_below).unwrap(), None);
}

#[test]
fn uncommitted_and_rolled_back_writes_after_a_split_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    fill(&db, 300, 100, 50);
    let before = read_file(dir.path());

    let mut txn = db.begin();
    for i in 300..600 {
        txn.put(&key(i), &value(i, 100)).unwrap();
    }
    txn.delete(&key(0)).unwrap();
    assert_eq!(txn.get(&key(0)).unwrap(), None);
    assert_eq!(txn.get(&key(450)).unwrap(), Some(value(450, 100)));
    txn.rollback();

    let mut txn = db.begin();
    txn.put(&key(1000), b"x").unwrap();
    drop(txn);

    assert_eq!(read_file(dir.path()), before);
    assert_all(&db, 300, 100);
    let txn = db.begin();
    assert_eq!(txn.get(&key(450)).unwrap(), None);
    assert_eq!(txn.get(&key(1000)).unwrap(), None);
}

// ------------------------------------------------- delete and overwrite

#[test]
fn deletes_after_splits_remove_only_the_deleted_keys() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        fill(&db, 1000, 100, 100);
        let mut txn = db.begin();
        for i in (0..1000).step_by(2) {
            txn.delete(&key(i)).unwrap();
        }
        txn.delete(b"never-existed").unwrap();
        txn.commit().unwrap();
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    for i in 0..1000 {
        let expected = (i % 2 == 1).then(|| value(i, 100));
        assert_eq!(txn.get(&key(i)).unwrap(), expected, "key {i}");
    }
}

#[test]
fn deleting_everything_after_splits_then_refilling_works() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        fill(&db, 600, 100, 60);
        let mut txn = db.begin();
        for i in 0..600 {
            txn.delete(&key(i)).unwrap();
        }
        txn.commit().unwrap();
        {
            let txn = db.begin();
            for i in 0..600 {
                assert_eq!(txn.get(&key(i)).unwrap(), None);
            }
        }
        db.close().unwrap();
    }
    // Empty leaves stay in the file (design 0003 §6) and must still open.
    let db = Db::open(dir.path()).unwrap();
    {
        let txn = db.begin();
        assert_eq!(txn.get(&key(0)).unwrap(), None);
        assert_eq!(txn.get(&key(599)).unwrap(), None);
    }
    fill(&db, 600, 120, 60);
    assert_all(&db, 600, 120);
    db.close().unwrap();
    let db = Db::open(dir.path()).unwrap();
    assert_all(&db, 600, 120);
}

#[test]
fn overwrites_after_splits_keep_only_the_latest_value() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        fill(&db, 800, 50, 100);
        // Grow every value; leaves must split again to hold them.
        let pages = file_pages(dir.path());
        for chunk in shuffled(800, 3).chunks(97) {
            let mut txn = db.begin();
            for &i in chunk {
                txn.put(&key(i), &value(i + 10_000, 400)).unwrap();
            }
            txn.commit().unwrap();
        }
        assert!(file_pages(dir.path()) > pages);
        // And shrink half of them back.
        let mut txn = db.begin();
        for i in (0..800).step_by(2) {
            txn.put(&key(i), b"s").unwrap();
        }
        txn.commit().unwrap();
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    for i in 0..800 {
        let expected = if i % 2 == 0 {
            b"s".to_vec()
        } else {
            value(i + 10_000, 400)
        };
        assert_eq!(txn.get(&key(i)).unwrap(), Some(expected), "key {i}");
    }
}

#[test]
fn a_commit_mixing_deletes_and_puts_that_split_is_applied_whole() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    fill(&db, 200, 300, 20);
    let mut txn = db.begin();
    for i in 0..100 {
        txn.delete(&key(i)).unwrap();
    }
    for i in 200..400 {
        txn.put(&key(i), &value(i, 300)).unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    for i in 0..400 {
        let expected = (i >= 100).then(|| value(i, 300));
        assert_eq!(txn.get(&key(i)).unwrap(), expected, "key {i}");
    }
}

// ------------------------------------------------------ size limits

#[test]
fn key_plus_value_is_limited_to_1006_bytes() {
    assert_eq!(MAX_ENTRY_LEN, 1006);
    assert_eq!(MAX_KEY_LEN, 512);
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut txn = db.begin();
    let k = b"abcdef";
    txn.put(k, &vec![1; 1000]).unwrap();
    match txn.put(b"ghijkl", &vec![1; 1001]) {
        Err(Error::ValueTooLarge { len, max }) => {
            assert_eq!(len, 1001);
            assert_eq!(max, 1000);
        }
        other => panic!("expected ValueTooLarge, got {other:?}"),
    }
    // The rejected put leaves nothing behind in the transaction.
    assert_eq!(txn.get(b"ghijkl").unwrap(), None);

    let big_key = vec![b'K'; 512];
    txn.put(&big_key, &vec![2; 494]).unwrap();
    match txn.put(&big_key, &vec![2; 495]) {
        Err(Error::ValueTooLarge { len, max }) => {
            assert_eq!(len, 495);
            assert_eq!(max, 494);
        }
        other => panic!("expected ValueTooLarge, got {other:?}"),
    }
    // The earlier, valid value under the same key is still the one written.
    assert_eq!(txn.get(&big_key).unwrap(), Some(vec![2; 494]));

    // An empty key with a 1006-byte value is the other edge.
    txn.put(b"", &vec![3; 1006]).unwrap();
    assert!(matches!(
        txn.put(b"", &vec![3; 1007]),
        Err(Error::ValueTooLarge {
            len: 1007,
            max: 1006
        })
    ));

    // Too long a key is still KeyTooLarge, not ValueTooLarge.
    assert!(matches!(
        txn.put(&vec![b'K'; 513], b""),
        Err(Error::KeyTooLarge { len: 513, max: 512 })
    ));
    txn.commit().unwrap();

    let txn = db.begin();
    assert_eq!(txn.get(k).unwrap(), Some(vec![1; 1000]));
    assert_eq!(txn.get(&big_key).unwrap(), Some(vec![2; 494]));
    assert_eq!(txn.get(b"").unwrap(), Some(vec![3; 1006]));
}

#[test]
fn many_maximum_size_entries_split_cleanly() {
    // Entries at exactly the limit, in an order that puts each new one in the
    // middle of a full leaf: splitting in two must always succeed.
    let dir = tempfile::tempdir().unwrap();
    let n = 60;
    let order = shuffled(n, 99);
    {
        let db = Db::open(dir.path()).unwrap();
        for &i in &order {
            let mut txn = db.begin();
            txn.put(&key(i), &value(i, MAX_ENTRY_LEN - 7)).unwrap();
            txn.commit().unwrap();
        }
        assert_all(&db, n, MAX_ENTRY_LEN - 7);
        db.close().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_all(&db, n, MAX_ENTRY_LEN - 7);
}

// ------------------------------------------------ root full: PageFull

fn big_key(i: usize) -> Vec<u8> {
    let mut k = format!("B{i:04}").into_bytes();
    k.resize(MAX_KEY_LEN, b'.');
    k
}

fn big_value(i: usize) -> Vec<u8> {
    value(i, MAX_ENTRY_LEN - MAX_KEY_LEN)
}

#[test]
fn a_commit_that_needs_a_third_level_fails_with_page_full_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();

    // One per commit until the root runs out of room.
    let mut stored = 0;
    loop {
        let mut txn = db.begin();
        txn.put(&big_key(stored), &big_value(stored)).unwrap();
        match txn.commit() {
            Ok(()) => stored += 1,
            Err(Error::PageFull) => break,
            Err(e) => panic!("unexpected error {e:?}"),
        }
        assert!(stored < 200, "the root never filled with 512-byte keys");
    }
    // Design 0003 §3: about 20 entries with 512-byte keys.
    assert!(
        (8..=40).contains(&stored),
        "PageFull after {stored} entries"
    );

    let before = read_file(dir.path());

    // A commit that deletes existing keys and adds many big ones: the deletes
    // must not be applied either.
    let mut txn = db.begin();
    txn.delete(&big_key(0)).unwrap();
    txn.delete(&big_key(stored / 2)).unwrap();
    for i in 1000..1040 {
        txn.put(&big_key(i), &big_value(i)).unwrap();
    }
    assert!(matches!(txn.commit(), Err(Error::PageFull)));

    let check = |db: &Db| {
        let txn = db.begin();
        for i in 0..stored {
            assert_eq!(txn.get(&big_key(i)).unwrap(), Some(big_value(i)), "{i}");
        }
        assert_eq!(txn.get(&big_key(stored)).unwrap(), None);
        for i in 1000..1040 {
            assert_eq!(txn.get(&big_key(i)).unwrap(), None);
        }
    };
    check(&db);
    assert_eq!(
        read_file(dir.path()),
        before,
        "a failed commit must not touch the data file"
    );

    // The database keeps working: small commits that fit still succeed.
    let mut txn = db.begin();
    txn.put(&big_key(0), b"small").unwrap();
    txn.commit().unwrap();
    let txn = db.begin();
    assert_eq!(txn.get(&big_key(0)).unwrap(), Some(b"small".to_vec()));
    drop(txn);
    let mut txn = db.begin();
    txn.put(&big_key(0), &big_value(0)).unwrap();
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(dir.path()).unwrap();
    check(&db);
}

#[test]
fn page_full_in_a_single_large_commit_on_a_fresh_database_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"a", b"keep").unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin();
    txn.delete(b"a").unwrap();
    for i in 0..60 {
        txn.put(&big_key(i), &big_value(i)).unwrap();
    }
    assert!(matches!(txn.commit(), Err(Error::PageFull)));
    assert_eq!(file_pages(dir.path()), 3);
    let txn = db.begin();
    assert_eq!(txn.get(b"a").unwrap(), Some(b"keep".to_vec()));
    assert_eq!(txn.get(&big_key(0)).unwrap(), None);
    drop(txn);
    db.close().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let txn = db.begin();
    assert_eq!(txn.get(b"a").unwrap(), Some(b"keep".to_vec()));
    assert_eq!(txn.get(&big_key(0)).unwrap(), None);
}

#[test]
#[ignore = "a third level comes with internal page splits (#16)"]
fn a_full_root_splits_into_a_third_level() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for i in 0..200 {
        let mut txn = db.begin();
        txn.put(&big_key(i), &big_value(i)).unwrap();
        txn.commit().unwrap();
    }
    let txn = db.begin();
    for i in 0..200 {
        assert_eq!(txn.get(&big_key(i)).unwrap(), Some(big_value(i)));
    }
}

// ------------------------------------------------- damaged data files

#[test]
fn an_undamaged_split_file_opens() {
    let dir = one_split_db();
    open_outcome(dir.path()).unwrap();
}

#[test]
fn a_root_child_pointer_past_the_end_of_the_file_is_corrupt() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    let (_, _, child_at) = internal_cell(&bytes, root, 0);
    put_u64(&mut bytes, child_at, 9_999);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "cell child past the end");
}

#[test]
fn a_rightmost_child_past_the_end_of_the_file_is_corrupt() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    put_u64(&mut bytes, (root * PAGE) as usize + 32, u64::MAX);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "rightmost child u64::MAX");
}

#[test]
fn a_child_pointing_at_its_own_parent_is_corrupt_not_a_hang() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    put_u64(&mut bytes, (root * PAGE) as usize + 32, root);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "rightmost child = root");
}

#[test]
fn a_child_pointing_at_a_meta_page_is_corrupt() {
    for target in [0u64, 1] {
        let dir = one_split_db();
        let mut bytes = read_file(dir.path());
        let root = meta_root(&bytes);
        let (_, _, child_at) = internal_cell(&bytes, root, 0);
        put_u64(&mut bytes, child_at, target);
        write_file(dir.path(), &bytes);
        assert_corrupt(dir.path(), &format!("child = meta page {target}"));
    }
}

#[test]
fn two_children_pointing_at_the_same_leaf_is_corrupt() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    let right = rightmost_child(&bytes, root);
    let (_, _, child_at) = internal_cell(&bytes, root, 0);
    put_u64(&mut bytes, child_at, right);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "left child = right child");
}

#[test]
fn a_separator_key_that_disagrees_with_the_leaves_is_corrupt() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    // Raise the separator above every stored key: the right leaf's keys now
    // fall outside the range the root gives them.
    let at = cell_offset(&bytes, root, 0);
    bytes[at + 4] = 0xff;
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "separator above the right leaf");
}

#[test]
fn a_broken_sibling_chain_is_corrupt() {
    // L -> nothing (drops R from the chain).
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    put_u64(&mut bytes, (2 * PAGE) as usize + 24, 0);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "left leaf sibling = 0");
}

#[test]
fn a_sibling_pointing_backwards_is_corrupt() {
    // R -> L (would loop a range scan).
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    let right = rightmost_child(&bytes, root);
    put_u64(&mut bytes, (right * PAGE) as usize + 24, 2);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "right leaf sibling = left leaf");
}

#[test]
fn a_sibling_pointing_at_itself_or_past_the_file_is_corrupt() {
    for bad in [2u64, 77_777] {
        let dir = one_split_db();
        let mut bytes = read_file(dir.path());
        put_u64(&mut bytes, (2 * PAGE) as usize + 24, bad);
        write_file(dir.path(), &bytes);
        assert_corrupt(dir.path(), &format!("left leaf sibling = {bad}"));
    }
}

#[test]
fn a_last_leaf_with_a_sibling_is_corrupt() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    let root = meta_root(&bytes);
    let right = rightmost_child(&bytes, root);
    put_u64(&mut bytes, (right * PAGE) as usize + 24, root);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "last leaf sibling = root");
}

#[test]
fn a_root_page_number_past_the_end_of_the_file_is_corrupt() {
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    put_u64(&mut bytes, 20, 500);
    put_u64(&mut bytes, PAGE as usize + 20, 500);
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "meta root past the end");
}

#[test]
fn an_internal_page_kind_on_a_leaf_is_corrupt() {
    // Make the left leaf claim to be an internal page.
    let dir = one_split_db();
    let mut bytes = read_file(dir.path());
    bytes[(2 * PAGE) as usize + 12] = KIND_INTERNAL;
    write_file(dir.path(), &bytes);
    assert_corrupt(dir.path(), "leaf marked internal");
}

#[test]
fn a_truncated_file_after_a_split_is_corrupt() {
    // Drop the last page: the meta page count no longer matches the file.
    let dir = one_split_db();
    let bytes = read_file(dir.path());
    write_file(dir.path(), &bytes[..bytes.len() - PAGE as usize]);
    assert_corrupt(dir.path(), "last page cut off");
}
