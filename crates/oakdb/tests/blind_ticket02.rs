//! Blind black-box tests for ticket 02 (delete and overwrite, issue #14).
//! Written only from the public API, the issue text and the ADRs.

use std::collections::BTreeMap;
use std::path::Path;

use oakdb::{Db, Error, MAX_KEY_LEN};

const PAGE: usize = 4096;
const HEADER: usize = 40;
const USABLE: usize = PAGE - HEADER;

fn cost(k: usize, v: usize) -> usize {
    8 + k + v
}

fn put_commit(db: &Db, k: &[u8], v: &[u8]) {
    let mut t = db.begin();
    t.put(k, v).unwrap();
    t.commit().unwrap();
}

fn del_commit(db: &Db, k: &[u8]) {
    let mut t = db.begin();
    t.delete(k).unwrap();
    t.commit().unwrap();
}

fn get(db: &Db, k: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(k).unwrap()
}

fn reopen(db: Db, path: &Path) -> Db {
    db.close().unwrap();
    Db::open(path).unwrap()
}

fn key(i: usize) -> Vec<u8> {
    format!("k{i:05}").into_bytes()
}

// ---------- delete then get ----------

#[test]
fn delete_then_get_in_same_txn_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    assert_eq!(t.get(b"a").unwrap(), None);
    t.commit().unwrap();
}

#[test]
fn delete_then_get_after_commit_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    put_commit(&db, b"b", b"2");
    del_commit(&db, b"a");
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
}

#[test]
fn delete_then_get_after_reopen_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    put_commit(&db, b"b", b"2");
    del_commit(&db, b"a");
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
}

#[test]
fn deleting_every_key_then_reopening_leaves_an_empty_usable_db() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for i in 0..50 {
        put_commit(&db, &key(i), b"vvvvvvvv");
    }
    let mut t = db.begin();
    for i in 0..50 {
        t.delete(&key(i)).unwrap();
    }
    t.commit().unwrap();
    let db = reopen(db, dir.path());
    for i in 0..50 {
        assert_eq!(get(&db, &key(i)), None);
    }
    put_commit(&db, b"x", b"y");
    assert_eq!(get(&reopen(db, dir.path()), b"x"), Some(b"y".to_vec()));
}

// ---------- overwrite ----------

#[test]
fn overwrite_returns_new_value_in_txn_after_commit_and_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"k", b"old");
    let mut t = db.begin();
    t.put(b"k", b"new").unwrap();
    assert_eq!(t.get(b"k").unwrap(), Some(b"new".to_vec()));
    t.commit().unwrap();
    assert_eq!(get(&db, b"k"), Some(b"new".to_vec()));
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"k"), Some(b"new".to_vec()));
}

#[test]
fn overwrite_with_larger_then_smaller_values_keeps_only_the_latest() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"before", b"B");
    put_commit(&db, b"k", b"x");
    put_commit(&db, b"after", b"A");
    for len in [1usize, 100, 1000, 3, 0, 2000, 7] {
        let v = vec![len as u8; len];
        put_commit(&db, b"k", &v);
        assert_eq!(get(&db, b"k"), Some(v.clone()));
        assert_eq!(get(&db, b"before"), Some(b"B".to_vec()));
        assert_eq!(get(&db, b"after"), Some(b"A".to_vec()));
    }
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"k"), Some(vec![7u8; 7]));
}

#[test]
fn putting_same_key_twice_in_one_txn_keeps_last_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    t.put(b"k", b"1").unwrap();
    t.put(b"k", b"2").unwrap();
    t.put(b"k", b"3").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"k"), Some(b"3".to_vec()));
    assert_eq!(get(&reopen(db, dir.path()), b"k"), Some(b"3".to_vec()));
}

#[test]
fn overwriting_same_key_does_not_create_duplicates_that_eat_space() {
    // A key overwritten 1000 times with a ~1KB value must not run out of room.
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for i in 0..1000u32 {
        let v = vec![(i % 251) as u8; 1000];
        put_commit(&db, b"k", &v);
    }
    assert_eq!(get(&db, b"k"), Some(vec![(999 % 251) as u8; 1000]));
}

// ---------- delete of missing key ----------

#[test]
fn deleting_a_missing_key_succeeds_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    del_commit(&db, b"nope");
    del_commit(&db, b"nope");
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"nope"), None);
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
}

#[test]
fn deleting_a_key_twice_in_one_txn_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    t.delete(b"a").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), None);
}

#[test]
fn deleting_on_an_empty_db_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    del_commit(&db, b"x");
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"x"), None);
}

// ---------- interleavings ----------

#[test]
fn put_then_delete_in_one_txn_leaves_key_absent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    t.put(b"a", b"1").unwrap();
    t.delete(b"a").unwrap();
    assert_eq!(t.get(b"a").unwrap(), None);
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&reopen(db, dir.path()), b"a"), None);
}

#[test]
fn put_then_delete_of_an_existing_key_in_one_txn_deletes_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"old");
    let mut t = db.begin();
    t.put(b"a", b"new").unwrap();
    t.delete(b"a").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&reopen(db, dir.path()), b"a"), None);
}

#[test]
fn delete_then_put_in_one_txn_leaves_the_new_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"old");
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    t.put(b"a", b"new").unwrap();
    assert_eq!(t.get(b"a").unwrap(), Some(b"new".to_vec()));
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), Some(b"new".to_vec()));
    assert_eq!(get(&reopen(db, dir.path()), b"a"), Some(b"new".to_vec()));
}

#[test]
fn put_delete_put_delete_sequence_follows_last_operation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    t.put(b"a", b"1").unwrap();
    t.delete(b"a").unwrap();
    t.put(b"a", b"2").unwrap();
    t.delete(b"a").unwrap();
    t.put(b"b", b"1").unwrap();
    t.delete(b"b").unwrap();
    t.put(b"b", b"3").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(b"3".to_vec()));
}

#[test]
fn txn_get_of_untouched_keys_still_sees_committed_data_after_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    put_commit(&db, b"b", b"2");
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    assert_eq!(t.get(b"b").unwrap(), Some(b"2".to_vec()));
    t.commit().unwrap();
}

// ---------- rollback / drop ----------

#[test]
fn rolled_back_delete_leaves_key_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    t.put(b"b", b"2").unwrap();
    t.rollback();
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), None);
    assert_eq!(get(&reopen(db, dir.path()), b"a"), Some(b"1".to_vec()));
}

#[test]
fn dropped_txn_with_delete_and_overwrite_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    put_commit(&db, b"b", b"1");
    {
        let mut t = db.begin();
        t.delete(b"a").unwrap();
        t.put(b"b", b"2").unwrap();
    }
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), Some(b"1".to_vec()));
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), Some(b"1".to_vec()));
}

// ---------- space reuse ----------

#[test]
fn fill_delete_all_refill_repeats_many_times_without_page_full() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    // 30 entries * (8 + 6 + 120) = 4020 <= 4056
    let n = 30;
    for round in 0..100usize {
        for i in 0..n {
            put_commit(&db, &key(i), &vec![(round + i) as u8; 120]);
        }
        for i in 0..n {
            del_commit(&db, &key(i));
        }
    }
    let db = reopen(db, dir.path());
    for i in 0..n {
        assert_eq!(get(&db, &key(i)), None);
    }
}

#[test]
fn deleting_alternate_keys_frees_room_for_a_large_entry_despite_fragmentation() {
    // Fill the page with many small entries, delete every other one (leaving
    // scattered holes), then insert one entry that only fits if the holes are
    // coalesced ("페이지 안 정리").
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let n = 60; // 60 * (8+6+50) = 3840
    for i in 0..n {
        put_commit(&db, &key(i), &[b'v'; 50]);
    }
    for i in (0..n).step_by(2) {
        del_commit(&db, &key(i));
    }
    // live: 30 * 64 = 1920; free = 4056 - 1920 = 2136; insert 2000-byte entry.
    let big = vec![b'B'; 2000 - 8 - 3];
    put_commit(&db, b"big", &big);
    assert_eq!(get(&db, b"big"), Some(big.clone()));
    for i in 0..n {
        let expect = if i % 2 == 0 {
            None
        } else {
            Some(vec![b'v'; 50])
        };
        assert_eq!(get(&db, &key(i)), expect);
    }
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"big"), Some(big));
}

#[test]
fn long_random_churn_with_small_live_set_never_hits_page_full() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut rng = Rng(0x1234_5678);
    let mut model = BTreeMap::new();
    // live set at most 10 keys * (8 + 6 + 200) = 2140 bytes < 4056
    for step in 0..5000 {
        let k = key(rng.below(10));
        if rng.below(3) == 0 {
            del_commit(&db, &k);
            model.remove(&k);
        } else {
            let v = vec![step as u8; rng.below(201)];
            let mut t = db.begin();
            t.put(&k, &v).unwrap();
            t.commit()
                .unwrap_or_else(|e| panic!("step {step}: commit failed: {e:?}"));
            model.insert(k, v);
        }
    }
    for (k, v) in &model {
        assert_eq!(get(&db, k).as_ref(), Some(v));
    }
}

#[test]
fn growing_overwrites_near_full_page_succeed_when_final_state_fits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    // other: 8+1+1000 = 1009; k: 8+1+v. capacity for v: 4056-1009-9 = 3038
    put_commit(&db, b"o", &[b'o'; 1000]);
    for v in [100usize, 1000, 2000, 3000, 3038] {
        let val = vec![b'x'; v];
        put_commit(&db, b"k", &val);
        assert_eq!(get(&db, b"k"), Some(val));
    }
}

#[test]
fn page_can_be_filled_exactly_to_capacity_after_heavy_churn() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for round in 0..20usize {
        let mut t = db.begin();
        for i in 0..40 {
            t.put(&key(i), &vec![round as u8; 30 + (i * 7 + round) % 40])
                .unwrap();
        }
        t.commit().unwrap();
        let mut t = db.begin();
        for i in 0..40 {
            t.delete(&key(i)).unwrap();
        }
        t.commit().unwrap();
    }
    // One entry using every usable byte: 8 + 1 + v = 4056.
    let v = vec![b'z'; USABLE - cost(1, 0)];
    put_commit(&db, b"z", &v);
    assert_eq!(get(&reopen(db, dir.path()), b"z"), Some(v));
}

// ---------- commit order independence ----------

#[test]
fn commit_that_fits_succeeds_when_put_is_written_before_freeing_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let half = USABLE / 2 + 100; // two of these don't fit
    let v = vec![b'a'; half - cost(1, 0)];
    put_commit(&db, b"z", &v); // key sorts last
    let mut t = db.begin();
    t.put(b"a", &v).unwrap(); // sorts first, would overflow if applied before delete
    t.delete(b"z").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), Some(v.clone()));
    assert_eq!(get(&db, b"z"), None);
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), Some(v));
    assert_eq!(get(&db, b"z"), None);
}

#[test]
fn commit_that_fits_succeeds_when_new_key_sorts_after_the_deleted_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let v = vec![b'a'; USABLE / 2 + 100 - cost(1, 0)];
    put_commit(&db, b"a", &v);
    let mut t = db.begin();
    t.put(b"z", &v).unwrap();
    t.delete(b"a").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"z"), Some(v));
    assert_eq!(get(&db, b"a"), None);
}

#[test]
fn commit_that_shrinks_one_value_and_grows_another_succeeds_in_any_key_order() {
    for (grow, shrink) in [(b"a", b"z"), (b"z", b"a")] {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let big = USABLE - 2 * cost(1, 0) - 10;
        let mut t = db.begin();
        t.put(shrink, &vec![b's'; big]).unwrap();
        t.put(grow, &[b'g'; 10]).unwrap();
        t.commit().unwrap();
        // swap sizes: final state is same total size
        let mut t = db.begin();
        t.put(grow, &vec![b'G'; big]).unwrap();
        t.put(shrink, &[b'S'; 10]).unwrap();
        t.commit()
            .unwrap_or_else(|e| panic!("grow={grow:?}: {e:?}"));
        assert_eq!(get(&db, grow), Some(vec![b'G'; big]));
        assert_eq!(get(&db, shrink), Some(vec![b'S'; 10]));
    }
}

#[test]
fn many_key_rewrite_at_full_page_succeeds_in_one_txn() {
    // Page is completely full; one txn deletes all and inserts a disjoint
    // set of the same total size, keys interleaved with the old ones.
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    // 39 entries * (8 + 6 + 90) = 4056 exactly
    let mut t = db.begin();
    for i in 0..39 {
        t.put(&key(i * 2 + 1), &[b'o'; 90]).unwrap();
    }
    t.commit().unwrap();
    let mut t = db.begin();
    for i in 0..39 {
        t.put(&key(i * 2), &[b'n'; 90]).unwrap();
    }
    for i in 0..39 {
        t.delete(&key(i * 2 + 1)).unwrap();
    }
    t.commit().unwrap();
    let db = reopen(db, dir.path());
    for i in 0..39 {
        assert_eq!(get(&db, &key(i * 2)), Some(vec![b'n'; 90]));
        assert_eq!(get(&db, &key(i * 2 + 1)), None);
    }
}

// ---------- PageFull changes nothing ----------

#[test]
fn page_full_commit_with_deletes_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    put_commit(&db, b"b", &[b'b'; 1000]);
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    t.put(b"b", b"small").unwrap();
    t.put(b"c", &[b'c'; 4040]).unwrap(); // final: (8+1+5)+(8+1+4040)=4063 > 4056
    let r = t.commit();
    assert!(matches!(r, Err(Error::PageFull)), "got {r:?}");
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), Some(vec![b'b'; 1000]));
    assert_eq!(get(&db, b"c"), None);
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), Some(vec![b'b'; 1000]));
    assert_eq!(get(&db, b"c"), None);
}

#[test]
fn page_full_commit_where_delete_sorts_after_the_overflowing_put_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"m", &[b'm'; 100]);
    put_commit(&db, b"z", &[b'z'; 100]);
    let mut t = db.begin();
    t.put(b"a", &[b'a'; 1000]).unwrap(); // fits on its own
    t.put(b"b", &[b'b'; 3500]).unwrap(); // pushes over
    t.delete(b"z").unwrap();
    t.put(b"m", b"changed").unwrap();
    assert!(matches!(t.commit(), Err(Error::PageFull)));
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), None);
    assert_eq!(get(&db, b"m"), Some(vec![b'm'; 100]));
    assert_eq!(get(&db, b"z"), Some(vec![b'z'; 100]));
}

#[test]
fn db_stays_usable_after_a_page_full_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    let mut t = db.begin();
    t.put(b"huge", &vec![0u8; PAGE]).unwrap();
    assert!(matches!(t.commit(), Err(Error::PageFull)));
    del_commit(&db, b"a");
    put_commit(&db, b"b", b"2");
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
}

#[test]
fn one_byte_over_capacity_is_page_full_and_exact_capacity_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", &[1u8; 500]);
    // remaining = 4056 - 509
    let rem = USABLE - cost(1, 500);
    let mut t = db.begin();
    t.put(b"b", &vec![2u8; rem - cost(1, 0) + 1]).unwrap();
    assert!(matches!(t.commit(), Err(Error::PageFull)));
    put_commit(&db, b"b", &vec![2u8; rem - cost(1, 0)]);
    // now full: deleting a frees exactly enough for a same-size entry
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    t.put(b"c", &[3u8; 500]).unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"c"), Some(vec![3u8; 500]));
}

// ---------- key length limits ----------

#[test]
fn delete_with_key_longer_than_max_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut t = db.begin();
    let r = t.delete(&vec![b'k'; MAX_KEY_LEN + 1]);
    assert!(
        matches!(r, Err(Error::KeyTooLarge { len, max }) if len == MAX_KEY_LEN + 1 && max == MAX_KEY_LEN),
        "got {r:?}"
    );
    t.commit().unwrap();
}

#[test]
fn delete_with_key_of_exactly_max_len_works() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let k = vec![b'k'; MAX_KEY_LEN];
    put_commit(&db, &k, b"v");
    del_commit(&db, &k);
    assert_eq!(get(&db, &k), None);
    assert_eq!(get(&reopen(db, dir.path()), &k), None);
}

#[test]
fn rejected_oversized_delete_does_not_poison_the_txn() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    let mut t = db.begin();
    t.delete(b"a").unwrap();
    assert!(t.delete(&vec![0u8; MAX_KEY_LEN + 100]).is_err());
    t.put(b"b", b"2").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"a"), None);
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
}

// ---------- empty keys / values ----------

#[test]
fn empty_value_is_distinct_from_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"k", b"");
    assert_eq!(get(&db, b"k"), Some(vec![]));
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"k"), Some(vec![]));
    del_commit(&db, b"k");
    assert_eq!(get(&db, b"k"), None);
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"k"), None);
}

#[test]
fn empty_key_can_be_put_overwritten_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"", b"1");
    put_commit(&db, b"a", b"A");
    put_commit(&db, b"", b"2");
    assert_eq!(get(&db, b""), Some(b"2".to_vec()));
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b""), Some(b"2".to_vec()));
    del_commit(&db, b"");
    assert_eq!(get(&db, b""), None);
    assert_eq!(get(&db, b"a"), Some(b"A".to_vec()));
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b""), None);
}

#[test]
fn keys_that_are_prefixes_of_each_other_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for k in [&b"a"[..], b"ab", b"abc", b"a\0", b"ab\0"] {
        put_commit(&db, k, k);
    }
    del_commit(&db, b"ab");
    put_commit(&db, b"a", b"new");
    let db = reopen(db, dir.path());
    assert_eq!(get(&db, b"a"), Some(b"new".to_vec()));
    assert_eq!(get(&db, b"ab"), None);
    assert_eq!(get(&db, b"abc"), Some(b"abc".to_vec()));
    assert_eq!(get(&db, b"a\0"), Some(b"a\0".to_vec()));
    assert_eq!(get(&db, b"ab\0"), Some(b"ab\0".to_vec()));
}

// ---------- model-based ----------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn model_size(m: &BTreeMap<Vec<u8>, Vec<u8>>) -> usize {
    m.iter().map(|(k, v)| cost(k.len(), v.len())).sum()
}

#[test]
fn randomized_ops_match_a_btreemap_model_across_reopens() {
    for seed in [1u64, 42, 0xdead_beef, 7777] {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(dir.path()).unwrap();
        let mut rng = Rng(seed);
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for step in 0..1500 {
            let mut pending = model.clone();
            let mut t = db.begin();
            let nops = 1 + rng.below(8);
            for _ in 0..nops {
                let klen = match rng.below(10) {
                    0 => 0,
                    1 => 1 + rng.below(MAX_KEY_LEN),
                    _ => 1 + rng.below(4),
                };
                let k: Vec<u8> = (0..klen).map(|_| b'a' + rng.below(4) as u8).collect();
                match rng.below(10) {
                    0..=3 => {
                        t.delete(&k).unwrap();
                        pending.remove(&k);
                    }
                    4 => {
                        assert_eq!(
                            t.get(&k).unwrap(),
                            pending.get(&k).cloned(),
                            "seed {seed} step {step}"
                        );
                    }
                    _ => {
                        let vlen = if rng.below(10) == 0 {
                            rng.below(1500)
                        } else {
                            rng.below(40)
                        };
                        let v: Vec<u8> = (0..vlen).map(|_| rng.next() as u8).collect();
                        t.put(&k, &v).unwrap();
                        pending.insert(k, v);
                    }
                }
            }
            let rollback = rng.below(10) == 0;
            if rollback {
                t.rollback();
            } else {
                let fits = model_size(&pending) <= USABLE;
                match t.commit() {
                    Ok(()) => {
                        assert!(
                            fits,
                            "seed {seed} step {step}: commit ok but model says {} > {USABLE}",
                            model_size(&pending)
                        );
                        model = pending;
                    }
                    Err(Error::PageFull) => {
                        assert!(
                            !fits,
                            "seed {seed} step {step}: PageFull but final size {} fits",
                            model_size(&pending)
                        );
                    }
                    Err(e) => panic!("seed {seed} step {step}: {e:?}"),
                }
            }
            if step % 100 == 99 {
                db = reopen(db, dir.path());
            }
            if step % 25 == 0 {
                let r = db.begin();
                for (k, v) in &model {
                    assert_eq!(
                        r.get(k).unwrap().as_ref(),
                        Some(v),
                        "seed {seed} step {step}"
                    );
                }
                for probe in [&b""[..], b"a", b"b", b"ab", b"dd", b"abcd"] {
                    assert_eq!(
                        r.get(probe).unwrap(),
                        model.get(probe).cloned(),
                        "seed {seed} step {step}"
                    );
                }
            }
        }
        let db = reopen(db, dir.path());
        let r = db.begin();
        for (k, v) in &model {
            assert_eq!(r.get(k).unwrap().as_ref(), Some(v));
        }
    }
}

// ---------- later tickets ----------

#[test]
#[ignore = "page splitting is ticket #15: a DB larger than one page"]
fn deleting_from_a_multi_page_db_works() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for i in 0..2000 {
        put_commit(&db, &key(i), &[b'v'; 32]);
    }
    for i in (0..2000).step_by(3) {
        del_commit(&db, &key(i));
    }
    let db = reopen(db, dir.path());
    for i in 0..2000 {
        assert_eq!(get(&db, &key(i)).is_some(), i % 3 != 0);
    }
}

#[test]
#[ignore = "snapshot isolation is not implemented yet"]
fn a_reader_started_before_a_delete_commit_still_sees_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_commit(&db, b"a", b"1");
    let r = db.begin();
    del_commit(&db, b"a");
    assert_eq!(r.get(b"a").unwrap(), Some(b"1".to_vec()));
}
