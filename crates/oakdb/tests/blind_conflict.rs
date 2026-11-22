//! Blind tests for write conflicts (issue #26, design 0016, ADR 0006),
//! written from the spec by a tester who did not read the implementation
//! (PR #63's verification).

use std::sync::atomic::{AtomicU64, Ordering};

use oakdb::fs::sim::SimFs;
use oakdb::fs::Fs;
use oakdb::{Db, Error};

fn open() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/oak").unwrap()
}

fn put<F: Fs>(db: &Db<F>, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn get<F: Fs>(db: &Db<F>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

fn all<F: Fs>(db: &Db<F>) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.begin().scan::<&[u8]>(..).map(Result::unwrap).collect()
}

fn assert_conflict(result: Result<(), Error>, key: &[u8]) {
    match result {
        Err(Error::Conflict { key: k }) => assert_eq!(k, key),
        other => panic!("expected Conflict on {key:?}, got {other:?}"),
    }
}

#[test]
fn the_later_of_two_puts_to_one_key_gets_a_conflict() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"k", b"a").unwrap();
    b.put(b"k", b"b").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"k");
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"a"[..]));
}

#[test]
fn the_first_to_commit_wins_whichever_began_first() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"k", b"a").unwrap();
    b.put(b"k", b"b").unwrap();
    b.commit().unwrap();
    assert_conflict(a.commit(), b"k");
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"b"[..]));
}

#[test]
fn a_put_then_a_delete_of_one_key_conflict() {
    let db = open();
    put(&db, b"k", b"0");
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"k", b"a").unwrap();
    b.delete(b"k").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"k");
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"a"[..]));
}

#[test]
fn a_delete_then_a_put_of_one_key_conflict() {
    let db = open();
    put(&db, b"k", b"0");
    let mut a = db.begin();
    let mut b = db.begin();
    a.delete(b"k").unwrap();
    b.put(b"k", b"b").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"k");
    assert_eq!(get(&db, b"k"), None);
}

#[test]
fn two_deletes_of_one_key_conflict() {
    let db = open();
    put(&db, b"k", b"0");
    let mut a = db.begin();
    let mut b = db.begin();
    a.delete(b"k").unwrap();
    b.delete(b"k").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"k");
}

#[test]
fn deleting_a_missing_key_still_counts_as_a_write() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    a.delete(b"ghost").unwrap();
    b.put(b"ghost", b"b").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"ghost");
    assert_eq!(get(&db, b"ghost"), None);
}

#[test]
fn writing_the_value_already_there_still_counts_as_a_write() {
    let db = open();
    put(&db, b"k", b"same");
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"k", b"same").unwrap();
    b.put(b"k", b"other").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"k");
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"same"[..]));
}

#[test]
fn a_change_and_change_back_still_conflicts() {
    let db = open();
    put(&db, b"k", b"A");
    let mut late = db.begin();
    put(&db, b"k", b"B");
    put(&db, b"k", b"A");
    late.put(b"k", b"L").unwrap();
    assert_conflict(late.commit(), b"k");
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"A"[..]));
}

#[test]
fn transactions_writing_different_keys_both_succeed() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"a", b"1").unwrap();
    b.put(b"b", b"2").unwrap();
    a.commit().unwrap();
    b.commit().unwrap();
    assert_eq!(
        all(&db),
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec())
        ]
    );
}

#[test]
fn a_retry_after_a_conflict_succeeds_and_sees_the_winner() {
    let db = open();
    put(&db, b"n", b"1");
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"n", b"2").unwrap();
    let seen = b.get(b"n").unwrap().unwrap();
    assert_eq!(seen, b"1");
    b.put(b"n", b"2").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"n");

    let mut retry = db.begin();
    assert_eq!(retry.get(b"n").unwrap().as_deref(), Some(&b"2"[..]));
    retry.put(b"n", b"3").unwrap();
    retry.commit().unwrap();
    assert_eq!(get(&db, b"n").as_deref(), Some(&b"3"[..]));
}

/// Snapshot isolation allows write skew (design 0016 B4): two doctors each
/// see the other on call and each take themselves off. Both commit.
#[test]
fn write_skew_is_allowed() {
    let db = open();
    put(&db, b"alice", b"on");
    put(&db, b"bob", b"on");
    let mut a = db.begin();
    let mut b = db.begin();
    assert_eq!(a.get(b"bob").unwrap().as_deref(), Some(&b"on"[..]));
    assert_eq!(b.get(b"alice").unwrap().as_deref(), Some(&b"on"[..]));
    a.put(b"alice", b"off").unwrap();
    b.put(b"bob", b"off").unwrap();
    a.commit().unwrap();
    b.commit().unwrap();
    assert_eq!(get(&db, b"alice").as_deref(), Some(&b"off"[..]));
    assert_eq!(get(&db, b"bob").as_deref(), Some(&b"off"[..]));
}

#[test]
fn reads_of_a_key_another_commit_wrote_do_not_conflict() {
    let db = open();
    put(&db, b"k", b"0");
    let mut reader = db.begin();
    assert_eq!(reader.get(b"k").unwrap().as_deref(), Some(&b"0"[..]));
    put(&db, b"k", b"1");
    reader.put(b"other", b"x").unwrap();
    reader.commit().unwrap();
    assert_eq!(get(&db, b"other").as_deref(), Some(&b"x"[..]));
}

#[test]
fn scans_over_keys_others_changed_do_not_conflict() {
    let db = open();
    put(&db, b"a", b"0");
    put(&db, b"b", b"0");
    let mut scanner = db.begin();
    assert_eq!(scanner.scan::<&[u8]>(..).count(), 2);
    put(&db, b"a", b"1");
    put(&db, b"c", b"new");
    // The scan still shows its snapshot.
    assert_eq!(scanner.scan::<&[u8]>(..).count(), 2);
    scanner.put(b"z", b"z").unwrap();
    scanner.commit().unwrap();
}

#[test]
fn a_read_only_transaction_commits_even_after_others_wrote_what_it_read() {
    let db = open();
    put(&db, b"k", b"0");
    let reader = db.begin();
    reader.get(b"k").unwrap();
    put(&db, b"k", b"1");
    reader.commit().unwrap();
}

#[test]
fn a_commit_before_begin_does_not_conflict() {
    let db = open();
    put(&db, b"k", b"0");
    let mut t = db.begin();
    t.put(b"k", b"1").unwrap();
    t.commit().unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"1"[..]));
}

#[test]
fn a_commit_while_another_is_open_but_before_this_began_does_not_conflict() {
    let db = open();
    let _old = db.begin(); // keeps the record of writes alive
    put(&db, b"k", b"0");
    let mut t = db.begin();
    t.put(b"k", b"1").unwrap();
    t.commit().unwrap();
}

#[test]
fn three_interleaved_transactions_only_the_first_on_each_key_wins() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    let mut c = db.begin();
    a.put(b"x", b"a").unwrap();
    b.put(b"y", b"b").unwrap();
    b.put(b"x", b"b").unwrap();
    c.put(b"y", b"c").unwrap();
    c.put(b"z", b"c").unwrap();
    a.commit().unwrap();
    assert_conflict(b.commit(), b"x");
    // b wrote nothing, so c's y does not clash with it.
    c.commit().unwrap();
    assert_eq!(get(&db, b"x").as_deref(), Some(&b"a"[..]));
    assert_eq!(get(&db, b"y").as_deref(), Some(&b"c"[..]));
    assert_eq!(get(&db, b"z").as_deref(), Some(&b"c"[..]));
}

#[test]
fn a_later_transaction_conflicts_with_a_middle_one() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    b.put(b"k", b"b").unwrap();
    b.commit().unwrap();
    let mut c = db.begin(); // began after b
    c.put(b"k", b"c").unwrap();
    a.put(b"k", b"a").unwrap();
    c.commit().unwrap();
    assert_conflict(a.commit(), b"k");
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"c"[..]));
}

#[test]
fn a_conflicting_commit_writes_none_of_its_keys_and_the_engine_keeps_going() {
    let db = open();
    put(&db, b"shared", b"0");
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"shared", b"a").unwrap();
    b.put(b"only-b-1", b"b").unwrap();
    b.put(b"shared", b"b").unwrap();
    b.put(b"only-b-2", b"b").unwrap();
    b.delete(b"never").unwrap();
    a.commit().unwrap();
    assert!(matches!(b.commit(), Err(Error::Conflict { .. })));
    assert_eq!(get(&db, b"only-b-1"), None);
    assert_eq!(get(&db, b"only-b-2"), None);
    assert_eq!(get(&db, b"shared").as_deref(), Some(&b"a"[..]));

    put(&db, b"after", b"ok");
    assert_eq!(get(&db, b"after").as_deref(), Some(&b"ok"[..]));
    db.checkpoint().unwrap();
    assert_eq!(all(&db).len(), 2);
}

#[test]
fn conflicts_are_detected_across_a_checkpoint() {
    let db = open();
    let mut late = db.begin();
    put(&db, b"k", b"winner");
    db.checkpoint().unwrap();
    late.put(b"k", b"loser").unwrap();
    assert_conflict(late.commit(), b"k");
    db.checkpoint().unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"winner"[..]));
}

#[test]
fn conflicts_are_detected_after_many_unrelated_commits() {
    let db = open();
    let mut late = db.begin();
    put(&db, b"k", b"winner");
    for i in 0..300u32 {
        put(&db, format!("other-{i:04}").as_bytes(), b"v");
        if i % 100 == 99 {
            db.checkpoint().unwrap();
        }
    }
    late.put(b"k", b"loser").unwrap();
    assert_conflict(late.commit(), b"k");
}

#[test]
fn a_long_running_transaction_survives_hundreds_of_unrelated_commits() {
    let db = open();
    put(&db, b"mine", b"0");
    let mut long = db.begin();
    for i in 0..500u32 {
        put(&db, format!("o{i:04}").as_bytes(), &i.to_be_bytes());
        if i % 128 == 0 {
            db.checkpoint().unwrap();
        }
    }
    assert_eq!(long.get(b"mine").unwrap().as_deref(), Some(&b"0"[..]));
    assert_eq!(long.get(b"o0001").unwrap(), None);
    long.put(b"mine", b"1").unwrap();
    long.commit().unwrap();
    assert_eq!(get(&db, b"mine").as_deref(), Some(&b"1"[..]));
    assert_eq!(all(&db).len(), 501);
}

#[test]
fn a_long_running_transaction_conflicts_on_a_key_written_early_among_hundreds() {
    let db = open();
    let mut long = db.begin();
    for i in 0..500u32 {
        let key = if i == 3 {
            "mine".to_string()
        } else {
            format!("o{i:04}")
        };
        put(&db, key.as_bytes(), b"v");
    }
    long.put(b"a", b"1").unwrap();
    long.put(b"mine", b"1").unwrap();
    assert_conflict(long.commit(), b"mine");
    assert_eq!(get(&db, b"a"), None);
}

#[test]
fn rolling_back_or_dropping_a_transaction_does_not_affect_others() {
    let db = open();
    let mut rolled = db.begin();
    rolled.put(b"k", b"r").unwrap();
    {
        let mut dropped = db.begin();
        dropped.put(b"k", b"d").unwrap();
    }
    let mut keeper = db.begin();
    keeper.put(b"k", b"keep").unwrap();
    rolled.rollback();
    keeper.commit().unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"keep"[..]));

    // A transaction older than one that was dropped still sees conflicts.
    let mut old = db.begin();
    drop(db.begin());
    put(&db, b"k", b"new");
    old.put(b"k", b"old").unwrap();
    assert_conflict(old.commit(), b"k");
}

#[test]
fn the_conflict_error_message_names_the_key() {
    let db = open();
    let mut a = db.begin();
    let mut b = db.begin();
    a.put(b"balance", b"1").unwrap();
    b.put(b"balance", b"2").unwrap();
    a.commit().unwrap();
    let err = b.commit().unwrap_err();
    assert!(err.to_string().contains("balance"), "{err}");
}

#[test]
fn concurrent_read_modify_write_increments_lose_no_updates() {
    let db = open();
    put(&db, b"counter", &0u64.to_be_bytes());
    const THREADS: usize = 8;
    const PER_THREAD: u64 = 50;
    let successes = AtomicU64::new(0);
    let conflicts = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..PER_THREAD {
                    loop {
                        let mut txn = db.begin();
                        let v = txn.get(b"counter").unwrap().unwrap();
                        let n = u64::from_be_bytes(v.try_into().unwrap());
                        std::thread::yield_now();
                        txn.put(b"counter", &(n + 1).to_be_bytes()).unwrap();
                        match txn.commit() {
                            Ok(()) => {
                                successes.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            Err(Error::Conflict { key }) => {
                                assert_eq!(key, b"counter");
                                conflicts.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("unexpected error {e:?}"),
                        }
                    }
                }
            });
        }
    });
    let total = successes.load(Ordering::Relaxed);
    assert_eq!(total, THREADS as u64 * PER_THREAD);
    let v = get(&db, b"counter").unwrap();
    assert_eq!(u64::from_be_bytes(v.try_into().unwrap()), total);
    eprintln!("conflicts retried: {}", conflicts.load(Ordering::Relaxed));
}

#[test]
fn many_threads_writing_disjoint_keys_never_conflict() {
    let db = open();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 50;
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let db = &db;
            s.spawn(move || {
                for i in 0..PER_THREAD {
                    let mut txn = db.begin();
                    txn.put(format!("t{t}-{i:03}").as_bytes(), b"v").unwrap();
                    txn.put(format!("t{t}-last").as_bytes(), &i.to_be_bytes())
                        .unwrap();
                    txn.commit().expect("disjoint keys must not conflict");
                }
            });
        }
    });
    assert_eq!(all(&db).len(), THREADS * (PER_THREAD + 1));
}
