//! Many threads and the commit thread (issue #27, design 0018, ADR 0006):
//! threads share one database and run transactions without waiting for
//! each other; commits go one at a time through the commit thread, and
//! while one syncs, other threads begin, read, write and end transactions.
//!
//! `SimFs::hold_syncs` holds a commit in its sync for as long as a test
//! needs, so "does not wait" is tested without sleeping: the other thread
//! has to finish while the sync is held. A time limit only turns a hang
//! into a failure.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use oakdb::fs::sim::{HeldSyncs, SimFs};
use oakdb::fs::{Fs, FsFile, StdFs};
use oakdb::{Db, Error, Txn, WAL_FILE};
use proptest::prelude::*;

/// How long work that must not wait for a held sync may take.
const WAIT: Duration = Duration::from_secs(3);

fn open(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), "/oak").unwrap()
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

/// What the WAL holds now, synced or not. Opening the file counts as one
/// operation that changes the disk, as `SimFs::fail_after` counts them.
fn wal_bytes(fs: &SimFs) -> Vec<u8> {
    let file = fs
        .open(&std::path::Path::new("/oak").join(WAL_FILE))
        .unwrap();
    let mut bytes = vec![0; usize::try_from(file.size().unwrap()).unwrap()];
    file.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

/// Where `needle` first shows in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Runs `work` on another thread and returns what it returns. Fails if it
/// does not finish within [`WAIT`]: it waited for the held sync. Lets the
/// sync go first, so the test fails instead of hanging.
fn without_waiting<T: Send>(held: &mut Option<HeldSyncs>, work: impl FnOnce() -> T + Send) -> T {
    thread::scope(|s| {
        let (done, finished) = mpsc::channel();
        let worker = s.spawn(move || {
            let out = work();
            let _ = done.send(());
            out
        });
        let in_time = finished.recv_timeout(WAIT).is_ok();
        if !in_time {
            held.take();
        }
        let out = worker.join().unwrap();
        assert!(in_time, "it waited for another commit's sync");
        out
    })
}

/// Starts a commit of `key` = `value` on its own thread of scope `s` and
/// waits until it is held in its sync. Returns the commit's thread.
fn commit_held<'scope>(
    s: &'scope thread::Scope<'scope, '_>,
    db: &'scope Db<SimFs>,
    held: &HeldSyncs,
    key: &'static [u8],
    value: &'static [u8],
) -> thread::ScopedJoinHandle<'scope, Result<(), Error>> {
    let committer = s.spawn(move || {
        let mut txn = db.begin();
        txn.put(key, value).unwrap();
        txn.commit()
    });
    held.wait_for_sync();
    committer
}

#[test]
fn the_database_and_its_transactions_can_go_to_other_threads() {
    fn shared<T: Send + Sync>() {}
    fn sent<T: Send>() {}
    shared::<Db<StdFs>>();
    shared::<Db<SimFs>>();
    sent::<Txn<'static, StdFs>>();
    sent::<Txn<'static, SimFs>>();
}

#[test]
fn threads_share_one_database_through_an_arc() {
    let fs = SimFs::new();
    let db = Arc::new(open(&fs));
    let threads: Vec<_> = (0..4u8)
        .map(|t| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for i in 0..20u8 {
                    put(&*db, &[t, i], &[i]);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(all(&*db).len(), 4 * 20);
}

#[test]
fn a_transaction_begins_reads_and_writes_while_another_commit_syncs() {
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"old", b"1");
    thread::scope(|s| {
        // Held inside the scope: if an assertion fails, unwinding drops it,
        // letting the held commit go, before the scope waits for that
        // commit's thread. The test fails instead of hanging.
        let mut held = Some(fs.hold_syncs());
        let committer = commit_held(s, &db, held.as_ref().unwrap(), b"new", b"2");
        without_waiting(&mut held, || {
            let mut txn = db.begin();
            assert_eq!(txn.get(b"old").unwrap(), Some(b"1".to_vec()));
            assert_eq!(txn.scan::<&[u8]>(..).count(), 1);
            txn.put(b"mine", b"3").unwrap();
            txn.delete(b"old").unwrap();
            assert_eq!(txn.get(b"mine").unwrap(), Some(b"3".to_vec()));
            txn.rollback();
        });
        held.take();
        committer.join().unwrap().unwrap();
    });
    assert_eq!(get(&db, b"new"), Some(b"2".to_vec()));
}

#[test]
fn a_transaction_ends_while_another_commit_syncs() {
    // A transaction that only reads still ends: it leaves the record of
    // open transactions that conflicts are found with (design 0016).
    // Before the commit thread, that waited for any commit's sync.
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"old", b"1");
    let reader = db.begin();
    let writer = {
        let mut txn = db.begin();
        txn.put(b"other", b"x").unwrap();
        txn
    };
    thread::scope(|s| {
        // Held inside the scope: if an assertion fails, unwinding drops it,
        // letting the held commit go, before the scope waits for that
        // commit's thread. The test fails instead of hanging.
        let mut held = Some(fs.hold_syncs());
        let db = &db;
        let committer = commit_held(s, db, held.as_ref().unwrap(), b"new", b"2");
        without_waiting(&mut held, move || {
            assert_eq!(reader.get(b"old").unwrap(), Some(b"1".to_vec()));
            drop(reader);
            writer.rollback();
            // A read-only commit writes nothing and ends at once too.
            db.begin().commit().unwrap();
        });
        held.take();
        committer.join().unwrap().unwrap();
    });
}

#[test]
fn the_page_count_is_read_while_another_commit_syncs() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.page_count();
    thread::scope(|s| {
        // Held inside the scope: if an assertion fails, unwinding drops it,
        // letting the held commit go, before the scope waits for that
        // commit's thread. The test fails instead of hanging.
        let mut held = Some(fs.hold_syncs());
        let committer = commit_held(s, &db, held.as_ref().unwrap(), b"new", b"2");
        let during = without_waiting(&mut held, || db.page_count());
        assert_eq!(during, before);
        held.take();
        committer.join().unwrap().unwrap();
    });
}

#[test]
fn a_commit_is_seen_only_once_it_is_synced() {
    let fs = SimFs::new();
    let db = open(&fs);
    thread::scope(|s| {
        // Held inside the scope: if an assertion fails, unwinding drops it,
        // letting the held commit go, before the scope waits for that
        // commit's thread. The test fails instead of hanging.
        let mut held = Some(fs.hold_syncs());
        let committer = commit_held(s, &db, held.as_ref().unwrap(), b"new", b"2");
        let seen = without_waiting(&mut held, || get(&db, b"new"));
        assert_eq!(seen, None, "a commit still syncing was seen");
        held.take();
        committer.join().unwrap().unwrap();
    });
    assert_eq!(get(&db, b"new"), Some(b"2".to_vec()));
}

#[test]
fn a_transaction_that_began_during_a_commit_conflicts_with_it() {
    // It began before the commit showed, so it did not see the commit's
    // write: writing the same key must fail.
    let fs = SimFs::new();
    let db = open(&fs);
    let mut late = None;
    thread::scope(|s| {
        // Held inside the scope: if an assertion fails, unwinding drops it,
        // letting the held commit go, before the scope waits for that
        // commit's thread. The test fails instead of hanging.
        let mut held = Some(fs.hold_syncs());
        let committer = commit_held(s, &db, held.as_ref().unwrap(), b"k", b"first");
        late = Some(without_waiting(&mut held, || db.begin()));
        held.take();
        committer.join().unwrap().unwrap();
    });
    let mut late = late.unwrap();
    assert_eq!(late.get(b"k").unwrap(), None);
    late.put(b"k", b"second").unwrap();
    match late.commit() {
        Err(Error::Conflict { key }) => assert_eq!(key, b"k"),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn commits_run_on_the_commit_thread() {
    let fs = SimFs::new();
    let db = open(&fs);
    let held = fs.hold_syncs();
    let syncing = thread::scope(|s| {
        let committer = thread::Builder::new()
            .name("client".into())
            .spawn_scoped(s, || put(&db, b"k", b"v"))
            .unwrap();
        let syncing = held.wait_for_sync();
        held.release();
        committer.join().unwrap();
        syncing
    });
    assert_eq!(syncing.as_deref(), Some("oakdb-commit"));
}

#[test]
fn a_checkpoint_runs_on_the_commit_thread() {
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"k", b"v");
    let held = fs.hold_syncs();
    let syncing = thread::scope(|s| {
        let checkpointer = s.spawn(|| db.checkpoint());
        let syncing = held.wait_for_sync();
        held.release();
        checkpointer.join().unwrap().unwrap();
        syncing
    });
    assert_eq!(syncing.as_deref(), Some("oakdb-commit"));
}

/// Values that show where each commit is in the WAL.
const FIRST: &[u8] = b"first-commit-value";
const SECOND: &[u8] = b"second-commit-value";

#[test]
fn a_commit_waits_for_the_one_ahead_of_it() {
    let fs = SimFs::new();
    let db = open(&fs);
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let first = commit_held(s, db, &held, b"a", FIRST);
        let (done, finished) = mpsc::channel();
        let second = s.spawn(move || {
            put(db, b"b", SECOND);
            let _ = done.send(());
        });
        // The second cannot finish while the first is held in its sync:
        // commits go one at a time. Waiting a while only gives a wrong
        // implementation the chance to show itself.
        let early = finished.recv_timeout(Duration::from_millis(200)).is_ok();
        held.release();
        first.join().unwrap().unwrap();
        second.join().unwrap();
        assert!(!early, "a commit finished before the one ahead of it");
    });
    assert_eq!(
        all(&db),
        vec![
            (b"a".to_vec(), FIRST.to_vec()),
            (b"b".to_vec(), SECOND.to_vec())
        ]
    );
    // And the WAL holds them in that order: the first commit's page image
    // holds only its own value, the second's comes after it.
    let wal = wal_bytes(&fs);
    let first_at = find(&wal, FIRST).expect("the first commit is in the WAL");
    let second_at = find(&wal, SECOND).expect("the second commit is in the WAL");
    assert!(
        first_at < second_at,
        "the WAL holds the second commit first"
    );
}

#[test]
fn commits_waiting_behind_a_failed_commit_write_nothing() {
    // The commit thread checks for a stop when it takes each request, not
    // only the caller before sending it: a commit already waiting when the
    // one ahead fails must not reach the WAL (design 0012).
    let fs = SimFs::new();
    let db = open(&fs);
    let (failed, waiting, wal_before) = thread::scope(|s| {
        let held = fs.hold_syncs();
        let db = &db;
        let first = commit_held(s, db, &held, b"a", b"1");
        // The first commit's pages are written; it waits to sync.
        let wal_before = wal_bytes(&fs).len();
        let waiting: Vec<_> = [b"b", b"c"]
            .into_iter()
            .map(|key| {
                s.spawn(move || {
                    let mut txn = db.begin();
                    txn.put(key, b"2").unwrap();
                    txn.commit()
                })
            })
            .collect();
        // Long enough for both to be waiting behind the first; a correct
        // engine passes however long it is.
        thread::sleep(Duration::from_millis(100));
        fs.fail_once_after(0);
        held.release();
        let failed = first.join().unwrap();
        let waiting: Vec<_> = waiting.into_iter().map(|t| t.join().unwrap()).collect();
        (failed, waiting, wal_before)
    });
    assert!(
        matches!(failed, Err(Error::OutcomeUnknown(_))),
        "{failed:?}"
    );
    for result in &waiting {
        assert!(matches!(result, Err(Error::Stopped(_))), "{result:?}");
    }
    assert_eq!(wal_bytes(&fs).len(), wal_before, "a waiting commit wrote");
    drop(db);
    let db = open(&fs);
    assert_eq!(get(&db, b"b"), None);
    assert_eq!(get(&db, b"c"), None);
}

/// A key a thread tried to commit, and how it went.
type Outcome = (Vec<u8>, Result<(), Error>);

#[test]
fn after_a_disk_failure_among_threads_only_one_commit_is_in_doubt() {
    // One write or sync fails part way through; every commit after it is
    // told the engine stopped, and opening again finds every acknowledged
    // commit and none that was refused.
    let fs = SimFs::new();
    let db = open(&fs);
    fs.fail_once_after(25);
    let results: Vec<Vec<Outcome>> = thread::scope(|s| {
        let threads: Vec<_> = (0..4u8)
            .map(|t| {
                let db = &db;
                s.spawn(move || {
                    (0..20u8)
                        .map(|i| {
                            let key = vec![t, i];
                            let mut txn = db.begin();
                            let result = txn.put(&key, b"v").and_then(|()| txn.commit());
                            (key, result)
                        })
                        .collect()
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    drop(db);

    let mut in_doubt = 0;
    for thread in &results {
        let mut stopped = false;
        for (key, result) in thread {
            match result {
                Ok(()) => assert!(!stopped, "{key:?} committed after the engine stopped"),
                Err(Error::OutcomeUnknown(_)) => {
                    in_doubt += 1;
                    stopped = true;
                }
                Err(Error::Stopped(_)) => stopped = true,
                Err(e) => panic!("unexpected error {e:?}"),
            }
        }
    }
    assert_eq!(in_doubt, 1);

    let db = open(&fs);
    for (key, result) in results.iter().flatten() {
        match result {
            Ok(()) => assert_eq!(get(&db, key), Some(b"v".to_vec()), "{key:?} lost"),
            Err(Error::Stopped(_)) => assert_eq!(get(&db, key), None, "{key:?} written"),
            _ => {}
        }
    }
}

#[test]
fn dropping_the_database_lets_it_open_again_at_once() {
    // Dropping stops the commit thread, which holds the files, before it
    // returns: the lock on the data file is free.
    let dir = tempfile::tempdir().unwrap();
    for round in 0..5u8 {
        let db = Db::open(dir.path()).unwrap();
        put(&db, &[round], b"v");
        drop(db);
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(all(&db).len(), 5);
}

/// One step of a thread in the model test. Keys are numbers below 8 in
/// the thread's own range.
#[derive(Debug, Clone)]
enum Op {
    Put(u8, u8),
    Delete(u8),
    Commit,
    Rollback,
    /// Reads every key and scans the thread's range, against the model.
    Check,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..8u8, any::<u8>()).prop_map(|(k, v)| Op::Put(k, v)),
        2 => (0..8u8).prop_map(Op::Delete),
        2 => Just(Op::Commit),
        1 => Just(Op::Rollback),
        2 => Just(Op::Check),
    ]
}

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// Runs `ops` as thread `t`, whose keys start with `t`, and returns what
/// it committed. No other thread writes its keys, so its transactions
/// never conflict, and what they read is its own model, whatever other
/// threads do meanwhile.
fn run_thread(db: &Db<SimFs>, t: u8, ops: &[Op]) -> Model {
    let key = |k: u8| vec![t, k];
    let mut committed = Model::new();
    let mut txn = Some(db.begin());
    let mut view = committed.clone();
    for op in ops {
        let open = txn.as_mut().expect("a transaction is always open");
        match *op {
            Op::Put(k, v) => {
                open.put(&key(k), &[v]).unwrap();
                view.insert(key(k), vec![v]);
            }
            Op::Delete(k) => {
                open.delete(&key(k)).unwrap();
                view.remove(&key(k));
            }
            Op::Commit => {
                // The next transaction begins once the commit has ended,
                // so it sees it.
                txn.take().unwrap().commit().unwrap();
                txn = Some(db.begin());
                committed = view.clone();
            }
            Op::Rollback => {
                txn.take().unwrap().rollback();
                txn = Some(db.begin());
                view = committed.clone();
            }
            Op::Check => {
                for k in 0..8 {
                    assert_eq!(open.get(&key(k)).unwrap(), view.get(&key(k)).cloned());
                }
                let scanned: Model = open
                    .scan(vec![t]..vec![t + 1])
                    .map(Result::unwrap)
                    .collect();
                assert_eq!(scanned, view);
            }
        }
    }
    committed
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Threads run transactions at once, each on its own keys. The
    /// database ends as the union of what each committed, and opens again
    /// the same.
    #[test]
    fn threads_running_transactions_at_once_keep_every_commit(
        ops in prop::collection::vec(prop::collection::vec(op(), 0..40), 1..5)
    ) {
        let fs = SimFs::new();
        let db = open(&fs);
        let models: Vec<Model> = thread::scope(|s| {
            let threads: Vec<_> = ops
                .iter()
                .enumerate()
                .map(|(t, ops)| {
                    let db = &db;
                    s.spawn(move || run_thread(db, t as u8, ops))
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let expected: Vec<(Vec<u8>, Vec<u8>)> = models.into_iter().flatten().collect();
        prop_assert_eq!(all(&db), expected.clone());
        drop(db);
        prop_assert_eq!(all(&open(&fs)), expected);
    }
}
