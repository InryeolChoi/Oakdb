//! Blind tests for issue #27 (many threads and the commit thread).
//!
//! Written from issue #27, spec #12, design 0018, ADR 0006 and the public
//! API docs only, without reading the implementation or its tests.
//!
//! Timing: nothing here depends on a sleep to pass. Where a thread must
//! finish while a commit is held in its sync, the test waits for it with a
//! generous timeout (`WATCHDOG`) that only turns a hang into a failure.
//! Two tests also check that a held commit has *not* returned yet; a correct
//! implementation can never have returned there, so a short pause before
//! the check only makes it more likely to catch a wrong one.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use oakdb::fs::sim::{HeldSyncs, SimFs};
use oakdb::fs::StdFs;
use oakdb::{Db, Error, Txn, COMMIT_THREAD};

/// Only used to fail a test that would otherwise hang.
const WATCHDOG: Duration = Duration::from_secs(20);

const DIR: &str = "db";

/// How long a test may run before the whole run fails: a deadlock is a bug
/// these tests look for, and it should fail, not hang (PR #74 review, P6).
const DEADLINE: Duration = Duration::from_secs(120);

/// Ends the test process with a failure if the calling test still runs
/// after [`DEADLINE`]. Hold what it returns until the test ends.
fn deadline() -> mpsc::Sender<()> {
    let name = thread::current().name().unwrap_or("a test").to_string();
    let (running, finished) = mpsc::channel::<()>();
    thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(DEADLINE) {
            eprintln!("{name} still runs after {DEADLINE:?}: it hangs");
            std::process::exit(101);
        }
    });
    running
}

fn open_sim() -> (SimFs, Db<SimFs>) {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), DIR).expect("open");
    (fs, db)
}

fn reopen(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), DIR).expect("reopen")
}

fn put_one<F: oakdb::fs::Fs>(db: &Db<F>, key: &[u8], value: &[u8]) {
    let mut t = db.begin();
    t.put(key, value).unwrap();
    t.commit().unwrap();
}

fn all<F: oakdb::fs::Fs>(db: &Db<F>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let t = db.begin();
    let out = t
        .scan::<&[u8]>(..)
        .map(|r| r.expect("scan item"))
        .collect::<BTreeMap<_, _>>();
    out
}

fn get<F: oakdb::fs::Fs>(db: &Db<F>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

/// Waits for `rx` or fails the test, letting the held syncs go first so
/// the threads stuck behind them can finish and the scope can end.
fn expect_within<T>(rx: &mpsc::Receiver<T>, held: &mut Option<HeldSyncs>, what: &str) -> T {
    match rx.recv_timeout(WATCHDOG) {
        Ok(v) => v,
        Err(_) => {
            if let Some(h) = held.take() {
                h.release();
            }
            panic!("{what} did not finish while a commit was held in its sync");
        }
    }
}

fn counter(v: Option<Vec<u8>>) -> u64 {
    v.map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Sharing the handle
// ---------------------------------------------------------------------

#[test]
fn the_database_is_send_and_sync_and_a_transaction_is_send() {
    let _deadline = deadline();
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Db<StdFs>>();
    send_sync::<Db<SimFs>>();
    send::<Txn<'static, StdFs>>();
    send::<Txn<'static, SimFs>>();
    send_sync::<Error>();
}

#[test]
fn an_arc_database_can_be_shared_with_spawned_threads_that_each_commit() {
    let _deadline = deadline();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path()).unwrap());
    let handles: Vec<_> = (0..8u8)
        .map(|i| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for n in 0..20u8 {
                    put_one(&*db, &[b't', i, n], &[i, n]);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let got = all(&*db);
    assert_eq!(got.len(), 8 * 20);
    for i in 0..8u8 {
        for n in 0..20u8 {
            assert_eq!(got.get(&vec![b't', i, n]), Some(&vec![i, n]));
        }
    }
}

#[test]
fn a_borrowed_database_can_be_shared_by_scoped_threads() {
    let _deadline = deadline();
    let (_fs, db) = open_sim();
    thread::scope(|s| {
        for i in 0..6u8 {
            let db = &db;
            s.spawn(move || {
                for n in 0..25u8 {
                    put_one(db, &[i, n], b"v");
                }
            });
        }
    });
    assert_eq!(all(&db).len(), 6 * 25);
}

#[test]
fn a_transaction_begun_on_one_thread_can_be_committed_on_another() {
    let _deadline = deadline();
    let (_fs, db) = open_sim();
    let mut t = db.begin();
    t.put(b"k", b"from main").unwrap();
    thread::scope(|s| {
        s.spawn(move || t.commit().unwrap()).join().unwrap();
    });
    assert_eq!(get(&db, b"k"), Some(b"from main".to_vec()));
}

#[test]
fn a_commit_acknowledged_on_one_thread_is_seen_by_a_transaction_begun_after_on_another() {
    let _deadline = deadline();
    let (_fs, db) = open_sim();
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        let dbr = &db;
        s.spawn(move || {
            for n in 0..50u32 {
                put_one(dbr, b"latest", &n.to_le_bytes());
                tx.send(n).unwrap();
            }
            drop(tx);
        });
        s.spawn(|| {
            for n in rx {
                let seen = get(&db, b"latest").unwrap();
                let seen = u32::from_le_bytes(seen.try_into().unwrap());
                assert!(
                    seen >= n,
                    "acknowledged {n} but a later transaction saw {seen}"
                );
            }
        });
    });
}

// ---------------------------------------------------------------------
// The commit thread and the sync gate (SimFs::hold_syncs)
// ---------------------------------------------------------------------

#[test]
fn the_sync_of_a_commit_runs_on_the_commit_thread() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let c = s.spawn(|| put_one(&db, b"k", b"v"));
        let name = held.wait_for_sync();
        held.release();
        c.join().unwrap();
        assert_eq!(name.as_deref(), Some(COMMIT_THREAD));
        assert_eq!(COMMIT_THREAD, "oakdb-commit");
    });
    assert_eq!(get(&db, b"k"), Some(b"v".to_vec()));
}

#[test]
fn a_commit_held_in_its_sync_does_not_return_until_the_sync_is_let_go() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    let held = fs.hold_syncs();
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        s.spawn(|| {
            let r = {
                let mut t = db.begin();
                t.put(b"k", b"v").unwrap();
                t.commit()
            };
            tx.send(r).unwrap();
        });
        held.wait_for_sync();
        // A correct commit cannot answer before its sync; the pause only
        // gives a wrong one time to answer early.
        thread::sleep(Duration::from_millis(50));
        assert!(
            rx.try_recv().is_err(),
            "the commit returned while its sync was held"
        );
        held.release();
        rx.recv_timeout(WATCHDOG).unwrap().unwrap();
    });
}

#[test]
fn while_a_commit_is_in_its_sync_other_threads_begin_read_write_and_end_transactions() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    put_one(&db, b"a", b"1");
    put_one(&db, b"b", b"2");
    let mut held = Some(fs.hold_syncs());
    let (done_tx, done_rx) = mpsc::channel();
    let (commit_tx, commit_rx) = mpsc::channel();
    thread::scope(|s| {
        s.spawn(|| {
            let mut t = db.begin();
            t.put(b"c", b"3").unwrap();
            commit_tx.send(t.commit()).unwrap();
        });
        held.as_ref().unwrap().wait_for_sync();
        s.spawn(|| {
            // begin, get, scan, put, delete, rollback, a read-only txn
            // ending, page_count: none of them may wait for the sync.
            let mut t = db.begin();
            assert_eq!(t.get(b"a").unwrap(), Some(b"1".to_vec()));
            let keys: Vec<_> = t.scan::<&[u8]>(..).map(|r| r.unwrap().0).collect();
            assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
            t.put(b"x", b"y").unwrap();
            t.delete(b"a").unwrap();
            assert_eq!(t.get(b"x").unwrap(), Some(b"y".to_vec()));
            t.rollback();

            let r = db.begin();
            assert_eq!(r.get(b"b").unwrap(), Some(b"2".to_vec()));
            drop(r);

            let mut w = db.begin();
            w.put(b"never", b"committed").unwrap();
            drop(w);

            assert!(db.page_count() >= 2);
            done_tx.send(()).unwrap();
        });
        expect_within(&done_rx, &mut held, "work on another thread");
        assert!(
            commit_rx.try_recv().is_err(),
            "the held commit returned before its sync"
        );
        held.take().unwrap().release();
        commit_rx.recv_timeout(WATCHDOG).unwrap().unwrap();
    });
    assert_eq!(get(&db, b"c"), Some(b"3".to_vec()));
    assert_eq!(get(&db, b"never"), None);
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
}

#[test]
fn a_commit_with_no_writes_does_not_wait_for_another_commits_sync() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    let mut held = Some(fs.hold_syncs());
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        let c = s.spawn(|| put_one(&db, b"k", b"v"));
        held.as_ref().unwrap().wait_for_sync();
        s.spawn(|| {
            let t = db.begin();
            let _ = t.get(b"k").unwrap();
            tx.send(t.commit()).unwrap();
        });
        let r = expect_within(&rx, &mut held, "an empty commit");
        assert!(r.is_ok(), "an empty commit failed: {r:?}");
        held.take().unwrap().release();
        c.join().unwrap();
    });
}

#[test]
fn the_writes_of_a_commit_in_its_sync_are_not_visible_to_transactions_begun_meanwhile() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    put_one(&db, b"k", b"old");
    let mut held = Some(fs.hold_syncs());
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        let c = s.spawn(|| put_one(&db, b"k", b"new"));
        held.as_ref().unwrap().wait_for_sync();
        s.spawn(|| {
            let t = db.begin();
            tx.send((t.get(b"k").unwrap(), all(&db).get(b"k".as_slice()).cloned()))
                .unwrap();
        });
        let (in_txn, in_scan) = expect_within(&rx, &mut held, "a read");
        assert_eq!(in_txn, Some(b"old".to_vec()));
        assert_eq!(in_scan, Some(b"old".to_vec()));
        held.take().unwrap().release();
        c.join().unwrap();
    });
    assert_eq!(get(&db, b"k"), Some(b"new".to_vec()));
}

#[test]
fn a_transaction_begun_while_a_commit_syncs_conflicts_with_it_on_the_same_key() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    let mut held = Some(fs.hold_syncs());
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        let a = s.spawn(|| put_one(&db, b"k", b"a"));
        held.as_ref().unwrap().wait_for_sync();
        // Begun while A is in its sync: its snapshot is from before A.
        s.spawn(|| {
            let mut t = db.begin();
            t.put(b"k", b"b").unwrap();
            tx.send(()).unwrap();
            let r = t.commit();
            assert!(
                matches!(r, Err(Error::Conflict { ref key }) if key == b"k"),
                "expected a conflict with the commit that was syncing, got {r:?}"
            );
        });
        expect_within(&rx, &mut held, "a begin");
        held.take().unwrap().release();
        a.join().unwrap();
    });
    assert_eq!(get(&db, b"k"), Some(b"a".to_vec()));
}

#[test]
fn a_commit_sent_while_another_syncs_is_judged_after_it_and_waits_for_it() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    // B begins first, so both A and B start from the same snapshot.
    let mut b = db.begin();
    b.put(b"k", b"b").unwrap();
    b.put(b"only-b", b"b").unwrap();
    let held = fs.hold_syncs();
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        let a = s.spawn(|| put_one(&db, b"k", b"a"));
        held.wait_for_sync();
        s.spawn(move || tx.send(b.commit()).unwrap());
        thread::sleep(Duration::from_millis(50));
        assert!(
            rx.try_recv().is_err(),
            "B was answered while A was still syncing"
        );
        held.release();
        a.join().unwrap();
        let r = rx.recv_timeout(WATCHDOG).unwrap();
        assert!(
            matches!(r, Err(Error::Conflict { .. })),
            "B should lose to A, got {r:?}"
        );
    });
    assert_eq!(get(&db, b"k"), Some(b"a".to_vec()));
    assert_eq!(
        get(&db, b"only-b"),
        None,
        "a conflicting commit wrote something"
    );
}

#[test]
fn commits_on_other_keys_queued_behind_a_syncing_commit_all_succeed_once_it_is_let_go() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let first = s.spawn(|| put_one(&db, b"first", b"1"));
        held.wait_for_sync();
        let rest: Vec<_> = (0..6u8)
            .map(|i| {
                let db = &db;
                s.spawn(move || put_one(db, &[b'q', i], &[i]))
            })
            .collect();
        held.release();
        first.join().unwrap();
        for h in rest {
            h.join().unwrap();
        }
    });
    let got = all(&db);
    assert_eq!(got.len(), 7);
    drop(db);
    assert_eq!(all(&reopen(&fs)), got);
}

#[test]
fn a_checkpoint_asked_for_while_a_commit_syncs_runs_after_it() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    let held = fs.hold_syncs();
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        let c = s.spawn(|| put_one(&db, b"k", b"v"));
        held.wait_for_sync();
        s.spawn(|| tx.send(db.checkpoint()).unwrap());
        thread::sleep(Duration::from_millis(50));
        assert!(
            rx.try_recv().is_err(),
            "the checkpoint finished before the commit before it"
        );
        held.release();
        c.join().unwrap();
        rx.recv_timeout(WATCHDOG).unwrap().unwrap();
    });
    drop(db);
    // Crash with a seed: the checkpointed commit must be on disk.
    fs.crash(7);
    assert_eq!(get(&reopen(&fs), b"k"), Some(b"v".to_vec()));
}

// ---------------------------------------------------------------------
// Order and durability with many threads
// ---------------------------------------------------------------------

#[test]
fn many_threads_on_their_own_keys_end_with_exactly_what_each_thread_committed() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    const THREADS: u8 = 8;
    let models: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = thread::scope(|s| {
        let hs: Vec<_> = (0..THREADS)
            .map(|i| {
                let db = &db;
                s.spawn(move || {
                    let mut model = BTreeMap::new();
                    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15 ^ u64::from(i);
                    let mut next = || {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng
                    };
                    for _ in 0..60 {
                        let mut t = db.begin();
                        let mut pending = model.clone();
                        for _ in 0..(next() % 5 + 1) {
                            let key = vec![b'p', i, (next() % 16) as u8];
                            if next() % 4 == 0 {
                                t.delete(&key).unwrap();
                                pending.remove(&key);
                            } else {
                                let val = next().to_le_bytes().to_vec();
                                t.put(&key, &val).unwrap();
                                pending.insert(key, val);
                            }
                        }
                        // The thread's own keys as its snapshot plus writes.
                        let mine: BTreeMap<_, _> = t
                            .scan(vec![b'p', i]..vec![b'p', i + 1])
                            .map(|r| r.unwrap())
                            .collect();
                        assert_eq!(mine, pending);
                        if next() % 5 == 0 {
                            t.rollback();
                        } else {
                            t.commit().unwrap();
                            model = pending;
                        }
                    }
                    model
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let expected: BTreeMap<_, _> = models.into_iter().flatten().collect();
    assert_eq!(all(&db), expected);
    drop(db);
    assert_eq!(all(&reopen(&fs)), expected, "after reopening");
}

#[test]
fn counters_incremented_from_many_threads_with_retries_lose_no_update() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    const THREADS: u64 = 8;
    const EACH: u64 = 40;
    let conflicts = AtomicUsize::new(0);
    thread::scope(|s| {
        for i in 0..THREADS {
            let (db, conflicts) = (&db, &conflicts);
            s.spawn(move || {
                for n in 0..EACH {
                    let key = [b'c', (n % 3) as u8];
                    loop {
                        let mut t = db.begin();
                        let v = counter(t.get(&key).unwrap()) + 1;
                        t.put(&key, &v.to_le_bytes()).unwrap();
                        // A log entry in the same commit: one per success.
                        t.put(format!("log-{i}-{n}").as_bytes(), b"").unwrap();
                        match t.commit() {
                            Ok(()) => break,
                            Err(Error::Conflict { .. }) => {
                                conflicts.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("unexpected {e:?}"),
                        }
                    }
                }
            });
        }
    });
    let check = |db: &Db<SimFs>| {
        let total: u64 = (0..3u8).map(|k| counter(get(db, &[b'c', k]))).sum();
        assert_eq!(total, THREADS * EACH);
        let logs = all(db).keys().filter(|k| k.starts_with(b"log-")).count() as u64;
        assert_eq!(logs, THREADS * EACH);
    };
    check(&db);
    drop(db);
    check(&reopen(&fs));
}

#[test]
fn transfers_between_accounts_from_many_threads_keep_the_total_in_every_snapshot() {
    let _deadline = deadline();
    let (_fs, db) = open_sim();
    const ACCOUNTS: u8 = 5;
    for a in 0..ACCOUNTS {
        put_one(&db, &[b'a', a], &100u64.to_le_bytes());
    }
    let stop = std::sync::atomic::AtomicBool::new(false);
    thread::scope(|s| {
        let writers: Vec<_> = (0..4u8)
            .map(|w| {
                let db = &db;
                s.spawn(move || {
                    for n in 0..50u8 {
                        let from = [b'a', (w + n) % ACCOUNTS];
                        let to = [b'a', (w + 2 * n + 1) % ACCOUNTS];
                        if from == to {
                            continue;
                        }
                        loop {
                            let mut t = db.begin();
                            let f = counter(t.get(&from).unwrap());
                            let g = counter(t.get(&to).unwrap());
                            if f == 0 {
                                break;
                            }
                            t.put(&from, &(f - 1).to_le_bytes()).unwrap();
                            t.put(&to, &(g + 1).to_le_bytes()).unwrap();
                            match t.commit() {
                                Ok(()) => break,
                                Err(Error::Conflict { .. }) => continue,
                                Err(e) => panic!("unexpected {e:?}"),
                            }
                        }
                    }
                })
            })
            .collect();
        let stop = &stop;
        let db = &db;
        let reader = s.spawn(move || {
            let mut reads = 0;
            while !stop.load(Ordering::Relaxed) || reads == 0 {
                let t = db.begin();
                let total: u64 = t
                    .scan::<&[u8]>(..)
                    .map(|r| counter(Some(r.unwrap().1)))
                    .sum();
                assert_eq!(
                    total,
                    100 * u64::from(ACCOUNTS),
                    "a snapshot saw half a transfer"
                );
                reads += 1;
            }
        });
        for h in writers {
            h.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
    });
}

// ---------------------------------------------------------------------
// Conflicts across threads
// ---------------------------------------------------------------------

#[test]
fn of_two_threads_writing_the_same_key_from_the_same_snapshot_exactly_one_commits() {
    let _deadline = deadline();
    for round in 0..20u8 {
        let (_fs, db) = open_sim();
        let barrier = Barrier::new(2);
        let results: Vec<_> = thread::scope(|s| {
            let hs: Vec<_> = (0..2u8)
                .map(|i| {
                    let (db, barrier) = (&db, &barrier);
                    s.spawn(move || {
                        let mut t = db.begin();
                        t.put(b"k", &[i]).unwrap();
                        t.put(&[b'o', i], b"").unwrap();
                        // Both have begun before either commits.
                        barrier.wait();
                        (i, t.commit())
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<u8> = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(winners.len(), 1, "round {round}: {results:?}");
        let loser = 1 - winners[0];
        assert!(
            matches!(&results[loser as usize].1, Err(Error::Conflict { key }) if key == b"k"),
            "round {round}: {results:?}"
        );
        assert_eq!(get(&db, b"k"), Some(vec![winners[0]]));
        assert_eq!(get(&db, &[b'o', loser]), None);
        assert_eq!(get(&db, &[b'o', winners[0]]), Some(vec![]));
    }
}

#[test]
fn a_reader_on_one_thread_keeps_its_snapshot_while_other_threads_commit() {
    let _deadline = deadline();
    let (_fs, db) = open_sim();
    put_one(&db, b"k", b"0");
    let reader = db.begin();
    thread::scope(|s| {
        for i in 1..=4u8 {
            let db = &db;
            s.spawn(move || put_one(db, &[b'n', i], b"new"));
        }
        let db = &db;
        s.spawn(move || put_one(db, b"k", b"1"));
    });
    assert_eq!(reader.get(b"k").unwrap(), Some(b"0".to_vec()));
    assert_eq!(reader.scan::<&[u8]>(..).count(), 1);
    drop(reader);
    assert_eq!(get(&db, b"k"), Some(b"1".to_vec()));
    assert_eq!(all(&db).len(), 5);
}

// ---------------------------------------------------------------------
// When the engine stops
// ---------------------------------------------------------------------

/// Commits from many threads on their own keys until each sees an error,
/// with the power cut part way. Returns per thread the outcomes in order.
fn commit_until_stopped(db: &Db<SimFs>) -> Vec<Vec<(u32, Result<(), Error>)>> {
    thread::scope(|s| {
        let hs: Vec<_> = (0..6u8)
            .map(|i| {
                s.spawn(move || {
                    let mut out = Vec::new();
                    for n in 0..400u32 {
                        let mut t = db.begin();
                        // Once the engine stops, put fails too (design 0012).
                        let r = t
                            .put(format!("t{i}-{n:04}").as_bytes(), b"v")
                            .and_then(|()| t.commit());
                        let failed = r.is_err();
                        out.push((n, r));
                        if failed {
                            // A few more after the first failure.
                            for m in 1..=3 {
                                let mut t = db.begin();
                                let r = t
                                    .put(format!("t{i}-{:04}", n + m).as_bytes(), b"v")
                                    .and_then(|()| t.commit());
                                out.push((n + m, r));
                            }
                            break;
                        }
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

#[test]
fn after_a_power_cut_one_batch_is_unknown_the_rest_stop_and_every_acknowledged_commit_survives() {
    let _deadline = deadline();
    for seed in 0..4u64 {
        let (fs, db) = open_sim();
        fs.fail_after(40 + seed * 7);
        let outcomes = commit_until_stopped(&db);

        let mut unknown = Vec::new();
        let mut acked = Vec::new();
        for (i, thread_out) in outcomes.iter().enumerate() {
            let mut seen_error = false;
            for (n, r) in thread_out {
                let key = format!("t{i}-{n:04}").into_bytes();
                match r {
                    Ok(()) => {
                        assert!(!seen_error, "seed {seed}: Ok after an error on thread {i}");
                        acked.push(key);
                    }
                    Err(Error::OutcomeUnknown(_)) => {
                        assert!(!seen_error, "seed {seed}: OutcomeUnknown after an error");
                        seen_error = true;
                        unknown.push(key);
                    }
                    Err(Error::Stopped(_)) => seen_error = true,
                    Err(e) => panic!("seed {seed}: unexpected {e:?}"),
                }
            }
            assert!(seen_error, "seed {seed}: thread {i} never saw the failure");
        }
        // Group commit (#28): the failure meets one batch, which holds at
        // most one commit per thread.
        assert!(
            (1..=6).contains(&unknown.len()),
            "seed {seed}: {} commits met the failure, not one batch",
            unknown.len()
        );
        assert_eq!(
            unknown.len() as u64,
            db.stats().in_doubt,
            "seed {seed}: not every commit that passed in the failed batch is in doubt"
        );

        // Every request on every thread now stops.
        thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let mut t = db.begin();
                    assert!(matches!(t.put(b"x", b"y"), Err(Error::Stopped(_))));
                    assert!(matches!(t.get(b"x"), Err(Error::Stopped(_))));
                    assert!(matches!(db.begin().commit(), Err(Error::Stopped(_))));
                    assert!(matches!(db.checkpoint(), Err(Error::Stopped(_))));
                });
            }
        });

        drop(db);
        fs.crash(seed);
        let db = reopen(&fs);
        let got = all(&db);
        for key in &acked {
            assert!(
                got.contains_key(key),
                "seed {seed}: acknowledged {key:?} was lost"
            );
        }
        let allowed: std::collections::BTreeSet<_> = acked.iter().chain(&unknown).collect();
        for key in got.keys() {
            assert!(
                allowed.contains(key),
                "seed {seed}: {key:?} was never acknowledged"
            );
        }
    }
}

#[test]
fn after_a_failed_disk_operation_with_the_power_on_all_threads_see_the_engine_stopped_and_reopening_keeps_acked_commits(
) {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    put_one(&db, b"before", b"v");
    fs.fail_once_after(30);
    let outcomes = commit_until_stopped(&db);
    let unknown = outcomes
        .iter()
        .flatten()
        .filter(|(_, r)| matches!(r, Err(Error::OutcomeUnknown(_))))
        .count();
    // One batch (#28): at most one commit per thread.
    assert!((1..=6).contains(&unknown), "{unknown} commits in doubt");
    assert_eq!(
        unknown as u64,
        db.stats().in_doubt,
        "not every commit that passed in the failed batch is in doubt"
    );
    for (i, out) in outcomes.iter().enumerate() {
        assert!(
            out.iter()
                .any(|(_, r)| matches!(r, Err(Error::Stopped(_)) | Err(Error::OutcomeUnknown(_)))),
            "thread {i} never saw the engine stop"
        );
        for (_, r) in out {
            assert!(
                matches!(
                    r,
                    Ok(()) | Err(Error::Stopped(_)) | Err(Error::OutcomeUnknown(_))
                ),
                "unexpected {r:?}"
            );
        }
    }
    let acked: Vec<Vec<u8>> = outcomes
        .iter()
        .enumerate()
        .flat_map(|(i, out)| {
            out.iter()
                .filter(|(_, r)| r.is_ok())
                .map(move |(n, _)| format!("t{i}-{n:04}").into_bytes())
        })
        .collect();
    drop(db);
    let db = reopen(&fs);
    for key in acked.iter().chain([&b"before".to_vec()]) {
        assert!(get(&db, key).is_some(), "acknowledged {key:?} was lost");
    }
}

// ---------------------------------------------------------------------
// Dropping and closing
// ---------------------------------------------------------------------

#[test]
fn dropping_the_database_joins_the_commit_thread_so_it_can_be_opened_again_at_once() {
    let _deadline = deadline();
    let dir = tempfile::tempdir().unwrap();
    for round in 0..10u8 {
        let db = Db::open(dir.path()).unwrap();
        thread::scope(|s| {
            for i in 0..4u8 {
                let db = &db;
                s.spawn(move || put_one(db, &[round, i], b"v"));
            }
        });
        drop(db);
        // The lock is free when drop returns: no AlreadyOpen.
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(all(&db).len(), 40);
}

#[test]
fn a_shared_database_is_dropped_by_the_last_arc_on_another_thread_and_closes_cleanly() {
    let _deadline = deadline();
    let fs = SimFs::new();
    let db = Arc::new(Db::open_with(fs.clone(), DIR).unwrap());
    let hs: Vec<_> = (0..4u8)
        .map(|i| {
            let db = Arc::clone(&db);
            thread::spawn(move || put_one(&*db, &[i], b"v"))
        })
        .collect();
    let last = thread::spawn(move || drop(db));
    for h in hs {
        h.join().unwrap();
    }
    last.join().unwrap();
    // Whichever thread dropped it last, the files are free again.
    let db = reopen(&fs);
    assert_eq!(all(&db).len(), 4);
}

#[test]
fn close_after_commits_from_many_threads_succeeds_and_keeps_them_through_a_crash() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    thread::scope(|s| {
        for i in 0..5u8 {
            let db = &db;
            s.spawn(move || {
                for n in 0..10u8 {
                    put_one(db, &[i, n], &[n]);
                }
            });
        }
    });
    db.close().unwrap();
    fs.crash(3);
    assert_eq!(all(&reopen(&fs)).len(), 50);
}

#[test]
fn a_stopped_database_still_answers_every_thread_and_drops_without_hanging() {
    let _deadline = deadline();
    let (fs, db) = open_sim();
    fs.fail_after(0);
    // The first commit meets the cut.
    let mut t = db.begin();
    t.put(b"k", b"v").unwrap();
    assert!(matches!(t.commit(), Err(Error::OutcomeUnknown(_))));
    let (tx, rx) = mpsc::channel();
    thread::scope(|s| {
        for i in 0..8u8 {
            let (db, tx) = (&db, tx.clone());
            s.spawn(move || {
                let mut t = db.begin();
                let r = t.put(&[i], b"v").and_then(|_| t.commit());
                tx.send(r).unwrap();
            });
        }
        drop(tx);
        for _ in 0..8 {
            let r = rx
                .recv_timeout(WATCHDOG)
                .expect("a thread was never answered");
            assert!(matches!(r, Err(Error::Stopped(_))), "got {r:?}");
        }
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        drop(db);
        tx.send(()).unwrap();
    });
    rx.recv_timeout(WATCHDOG)
        .expect("dropping a stopped database hung");
}

#[test]
fn opening_and_dropping_many_databases_leaves_no_commit_thread_waiting() {
    let _deadline = deadline();
    // Each Db starts a commit thread; drop must join it, or this would
    // still pass but leak. We check drop returns, and the lock is free.
    let fs = SimFs::new();
    for _ in 0..50 {
        let db = Db::open_with(fs.clone(), DIR).unwrap();
        drop(db);
    }
    let _db = reopen(&fs);
}
