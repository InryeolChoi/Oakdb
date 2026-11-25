//! Blind tests for issue #28 (group commit).
//!
//! Written from issue #28, issue #2, spec #12, design 0021 (with 0018 and
//! 0012), ADR 0006/0007 and the public API docs only, without reading the
//! implementation or its tests.
//!
//! Batches are made deterministic with the sync gate (`SimFs::hold_syncs`):
//! commit A is held in its sync, the others are sent one by one, and each is
//! known to be waiting at the commit thread once `Stats::received` counts it.
//! Time is only used to turn a hang into a failure (`WATCHDOG`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use oakdb::fs::sim::{FailedSync, HeldSyncs, SimFs};
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error, Result, CHECKPOINT_AFTER, COMMIT_THREAD, MAX_BATCH, WAL_FILE};

const DIR: &str = "/db";
const WATCHDOG: Duration = Duration::from_secs(20);

fn open(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), DIR).expect("open")
}

fn put1(db: &Db<SimFs>, key: &[u8], value: &[u8]) -> Result<()> {
    let mut txn = db.begin();
    txn.put(key, value)?;
    txn.commit()
}

fn get(db: &Db<SimFs>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).expect("get")
}

/// Waits until the commit thread has received `n` requests in all.
fn wait_received(db: &Db<SimFs>, n: u64) {
    let deadline = Instant::now() + WATCHDOG;
    while db.stats().received < n {
        assert!(
            Instant::now() < deadline,
            "the commit thread never received request {n} (stats {:?})",
            db.stats()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn join_within<T>(handle: ScopedJoinHandle<'_, T>) -> T {
    let deadline = Instant::now() + WATCHDOG;
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "a thread did not finish in time");
        thread::sleep(Duration::from_millis(1));
    }
    handle.join().expect("thread panicked")
}

fn batch_key(i: usize) -> Vec<u8> {
    format!("batch/{i:04}").into_bytes()
}

/// Sends commit A (`a` = `1`) and waits until it is held in its sync, then
/// sends one commit per entry of `writes`, in order, each only once the one
/// before is waiting at the commit thread. They all form the next batch.
fn held_batch<'scope, 'env>(
    s: &'scope Scope<'scope, 'env>,
    db: &'env Db<SimFs>,
    gate: &HeldSyncs,
    writes: &[(Vec<u8>, Vec<u8>)],
) -> (
    ScopedJoinHandle<'scope, Result<()>>,
    Vec<ScopedJoinHandle<'scope, Result<()>>>,
) {
    let base = db.stats().received;
    let a = s.spawn(move || put1(db, b"a", b"1"));
    assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
    wait_received(db, base + 1);
    let mut rest = Vec::new();
    for (i, (k, v)) in writes.iter().cloned().enumerate() {
        rest.push(s.spawn(move || put1(db, &k, &v)));
        wait_received(db, base + 2 + i as u64);
    }
    (a, rest)
}

fn distinct_writes(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n)
        .map(|i| (batch_key(i), format!("v{i}").into_bytes()))
        .collect()
}

/// How many of the `n` batch keys the database holds.
fn batch_present(db: &Db<SimFs>, n: usize) -> usize {
    let txn = db.begin();
    (0..n)
        .filter(|&i| txn.get(&batch_key(i)).expect("get").is_some())
        .count()
}

fn is_outcome_unknown(r: &Result<()>) -> bool {
    matches!(r, Err(Error::OutcomeUnknown(_)))
}

fn is_stopped(r: &Result<()>) -> bool {
    matches!(r, Err(Error::Stopped(_)))
}

const N: usize = 8;

// ---------------------------------------------------------------- sharing

#[test]
fn commits_waiting_behind_a_sync_share_the_next_sync() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let gate = fs.hold_syncs();
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &distinct_writes(N));
        gate.let_waiting_go();
        // The whole batch reaches the gate as one sync.
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        gate.release();
        assert!(join_within(a).is_ok());
        for h in rest {
            assert!(join_within(h).is_ok());
        }
    });
    let st = db.stats();
    assert_eq!(st.commits - before.commits, 1 + N as u64);
    assert_eq!(st.wal_syncs - before.wal_syncs, 2, "{st:?}");
    assert_eq!(st.batches - before.batches, 2, "{st:?}");
    assert_eq!(st.largest_batch, N as u64, "{st:?}");
    assert!(st.wal_syncs < st.commits);
    assert_eq!(batch_present(&db, N), N);
}

#[test]
fn a_batch_holds_at_most_max_batch_commits_and_the_rest_follow_in_the_next() {
    let n = MAX_BATCH + 72;
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let gate = fs.hold_syncs();
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &distinct_writes(n));
        gate.release();
        assert!(join_within(a).is_ok());
        for h in rest {
            assert!(join_within(h).is_ok());
        }
    });
    let st = db.stats();
    assert_eq!(st.largest_batch, MAX_BATCH as u64, "{st:?}");
    assert_eq!(
        st.batches - before.batches,
        3,
        "A, then 128, then 72: {st:?}"
    );
    assert_eq!(st.wal_syncs - before.wal_syncs, 3, "{st:?}");
    assert_eq!(st.commits - before.commits, 1 + n as u64);
    assert_eq!(batch_present(&db, n), n);
}

#[test]
fn a_lone_commit_is_not_kept_waiting_for_company() {
    let fs = SimFs::new();
    let db = open(&fs);
    let gate = fs.hold_syncs();
    thread::scope(|s| {
        let h = s.spawn(|| put1(&db, b"alone", b"1"));
        // No other commit ever comes; its sync still arrives.
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        gate.release();
        assert!(join_within(h).is_ok());
    });
    let st = db.stats();
    assert_eq!(
        (st.commits, st.batches, st.wal_syncs, st.largest_batch),
        (1, 1, 1, 1),
        "{st:?}"
    );
    assert_eq!(st.received, 1);
}

#[test]
fn one_thread_committing_alone_makes_one_batch_and_one_sync_per_commit() {
    let fs = SimFs::new();
    let db = open(&fs);
    let start = Instant::now();
    for i in 0..50 {
        put1(&db, format!("k{i}").as_bytes(), b"v").unwrap();
    }
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "lone commits were delayed"
    );
    let st = db.stats();
    assert_eq!(
        (st.commits, st.batches, st.wal_syncs, st.largest_batch),
        (50, 50, 50, 1),
        "{st:?}"
    );
}

// ---------------------------------------------------------------- conflicts in a batch

#[test]
fn two_commits_writing_the_same_key_in_one_batch_the_first_to_arrive_wins() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let gate = fs.hold_syncs();
    let writes = vec![
        (b"k".to_vec(), b"first".to_vec()),
        (b"k".to_vec(), b"second".to_vec()),
    ];
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &writes);
        gate.release();
        assert!(join_within(a).is_ok());
        let results: Vec<_> = rest.into_iter().map(join_within).collect();
        assert!(results[0].is_ok(), "first: {:?}", results[0]);
        match &results[1] {
            Err(Error::Conflict { key }) => assert_eq!(key, b"k"),
            other => panic!("second should conflict, got {other:?}"),
        }
    });
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"first"[..]));
    let st = db.stats();
    assert_eq!(st.conflicts - before.conflicts, 1);
    assert_eq!(st.commits - before.commits, 2);
    drop(db);
    let db = open(&fs);
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"first"[..]));
}

#[test]
fn commits_writing_different_keys_in_one_batch_all_succeed_with_overlapping_ones_conflicting() {
    let fs = SimFs::new();
    let db = open(&fs);
    let gate = fs.hold_syncs();
    // x, y, x, z, y: the 3rd and 5th lose to the 1st and 2nd.
    let writes: Vec<_> = ["x", "y", "x", "z", "y"]
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_bytes().to_vec(), format!("{i}").into_bytes()))
        .collect();
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &writes);
        gate.release();
        assert!(join_within(a).is_ok());
        let ok: Vec<bool> = rest.into_iter().map(|h| join_within(h).is_ok()).collect();
        assert_eq!(ok, vec![true, true, false, true, false]);
    });
    assert_eq!(get(&db, b"x").as_deref(), Some(&b"0"[..]));
    assert_eq!(get(&db, b"y").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&db, b"z").as_deref(), Some(&b"3"[..]));
}

/// `Error::Conflict` says: "begin a new transaction, which reads the
/// winner's write, and try again". A loser inside a batch must not be told
/// to retry before the winner it lost to is visible.
#[test]
fn a_commit_that_lost_inside_a_batch_and_retries_at_once_reads_the_winners_write() {
    let fs = SimFs::new();
    let db = open(&fs);
    let gate = fs.hold_syncs();
    let writes = vec![
        (b"k".to_vec(), b"win".to_vec()),
        (b"k".to_vec(), b"lose".to_vec()),
    ];
    thread::scope(|s| {
        let db = &db;
        let base = db.stats().received;
        let a = s.spawn(move || put1(db, b"a", b"1"));
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        wait_received(db, base + 1);
        let mut hs = Vec::new();
        for (i, (k, v)) in writes.into_iter().enumerate() {
            hs.push(s.spawn(move || {
                let r = put1(db, &k, &v);
                // Retry at once, as the error says to: what does it read?
                let seen = db.begin().get(b"k").unwrap();
                (r, seen)
            }));
            wait_received(db, base + 2 + i as u64);
        }
        gate.let_waiting_go();
        // The batch is now held in its sync.
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        let loser = hs.pop().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !loser.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        gate.release();
        let (r, seen) = join_within(loser);
        assert!(matches!(r, Err(Error::Conflict { .. })), "{r:?}");
        assert_eq!(
            seen.as_deref(),
            Some(&b"win"[..]),
            "the loser was answered Conflict before the winner's write was visible"
        );
        assert!(join_within(hs.pop().unwrap()).0.is_ok());
        assert!(join_within(a).is_ok());
    });
}

// ---------------------------------------------------------------- visibility

#[test]
fn nothing_of_a_batch_is_visible_before_its_sync_and_all_of_it_after() {
    let fs = SimFs::new();
    let db = open(&fs);
    let gate = fs.hold_syncs();
    let done = AtomicBool::new(false);
    let partial = AtomicU64::new(0);
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &distinct_writes(N));
        gate.let_waiting_go();
        assert!(
            join_within(a).is_ok(),
            "A is answered once its sync is done"
        );
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        // The batch is in its sync: nothing of it shows yet.
        assert_eq!(get(&db, b"a").as_deref(), Some(&b"1"[..]));
        assert_eq!(batch_present(&db, N), 0);
        let old = db.begin();
        // A watcher must only ever see none or all of the batch.
        let watcher = s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                let seen = batch_present(&db, N);
                if seen != 0 && seen != N {
                    partial.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        gate.release();
        for h in rest {
            assert!(join_within(h).is_ok());
            // Each committer's own write is visible once it is answered.
        }
        assert_eq!(batch_present(&db, N), N);
        // A transaction begun before still sees its snapshot.
        assert_eq!(old.get(&batch_key(0)).unwrap(), None);
        drop(old);
        done.store(true, Ordering::SeqCst);
        join_within(watcher);
    });
    assert_eq!(
        partial.load(Ordering::SeqCst),
        0,
        "a reader saw part of a batch"
    );
}

#[test]
fn a_committer_reads_its_own_write_as_soon_as_it_is_answered() {
    let fs = SimFs::new();
    let db = open(&fs);
    let gate = fs.hold_syncs();
    let base = db.stats().received;
    thread::scope(|s| {
        let a = s.spawn(|| put1(&db, b"a", b"1"));
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        wait_received(&db, base + 1);
        let mut hs = Vec::new();
        for i in 0..N {
            let db = &db;
            hs.push(s.spawn(move || {
                put1(db, &batch_key(i), b"mine").unwrap();
                get(db, &batch_key(i))
            }));
            wait_received(db, base + 2 + i as u64);
        }
        gate.release();
        assert!(join_within(a).is_ok());
        for h in hs {
            assert_eq!(join_within(h).as_deref(), Some(&b"mine"[..]));
        }
    });
}

// ---------------------------------------------------------------- failures

/// How many disk-changing operations the batch of `N` makes, from the end
/// of A's sync to the end of the batch's sync.
fn batch_ops() -> u64 {
    let fs = SimFs::new();
    let db = open(&fs);
    put1(&db, b"p", b"0").unwrap();
    let gate = fs.hold_syncs();
    let mut d = 0;
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &distinct_writes(N));
        let at_a = fs.ops();
        gate.let_waiting_go();
        assert_eq!(gate.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        // A's sync, then the batch's writes; the batch's sync is waiting.
        d = fs.ops() - at_a;
        gate.release();
        assert!(join_within(a).is_ok());
        for h in rest {
            assert!(join_within(h).is_ok());
        }
    });
    assert!(d >= 2, "a batch writes and syncs: {d}");
    d
}

enum Fault {
    /// `fail_once_after`: one operation fails, the power stays on.
    Once(FailedSync),
    /// `fail_after`: the power is cut.
    Cut,
}

/// Holds A, queues the batch, arms `fault` at operation `j` of the batch
/// (0-based, counted after A's sync) and lets everything go. Returns
/// whether the batch was acknowledged, after checking every answer.
fn run_faulty_batch(fs: &SimFs, fault: &Fault, j: u64, d: u64) -> bool {
    let db = open(fs);
    put1(&db, b"p", b"0").unwrap();
    let gate = fs.hold_syncs();
    let mut acked = false;
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &distinct_writes(N));
        match fault {
            Fault::Once(how) => {
                fs.set_failed_sync(*how);
                fs.fail_once_after(1 + j);
            }
            Fault::Cut => fs.fail_after(1 + j),
        }
        gate.release();
        assert!(join_within(a).is_ok(), "A synced before the fault");
        let results: Vec<_> = rest.into_iter().map(join_within).collect();
        if j < d {
            for r in &results {
                assert!(
                    is_outcome_unknown(r),
                    "j={j}: every commit of the failed batch gets OutcomeUnknown, got {r:?}"
                );
            }
            let after = put1(&db, b"later", b"x");
            assert!(
                is_stopped(&after),
                "j={j}: a later commit is Stopped, got {after:?}"
            );
            assert!(matches!(db.checkpoint(), Err(Error::Stopped(_))));
        } else {
            for r in &results {
                assert!(r.is_ok(), "j={j}: {r:?}");
            }
            acked = true;
        }
    });
    drop(db);
    acked
}

fn check_reopened(fs: &SimFs, acked: bool, what: &str) {
    let db = open(fs);
    assert_eq!(get(&db, b"p").as_deref(), Some(&b"0"[..]), "{what}");
    assert_eq!(
        get(&db, b"a").as_deref(),
        Some(&b"1"[..]),
        "{what}: A was acknowledged"
    );
    assert_eq!(
        get(&db, b"later"),
        None,
        "{what}: a Stopped commit wrote nothing"
    );
    let seen = batch_present(&db, N);
    if acked {
        assert_eq!(seen, N, "{what}: acknowledged batch lost");
    } else {
        assert!(
            seen == 0 || seen == N,
            "{what}: batch half there ({seen} of {N})"
        );
    }
}

#[test]
fn a_batch_whose_write_or_sync_fails_answers_every_commit_outcome_unknown_and_reopens_all_or_nothing(
) {
    let d = batch_ops();
    for how in [FailedSync::KeepsWaiting, FailedSync::Forgets] {
        for j in 0..d {
            // Reopened as it is, without a crash.
            let fs = SimFs::new();
            run_faulty_batch(&fs, &Fault::Once(how), j, d);
            check_reopened(&fs, false, &format!("{how:?} j={j} no crash"));
            // And after a crash.
            for seed in 0..4 {
                let fs = SimFs::new();
                run_faulty_batch(&fs, &Fault::Once(how), j, d);
                fs.crash(seed);
                check_reopened(&fs, false, &format!("{how:?} j={j} seed={seed}"));
            }
        }
    }
}

#[test]
fn a_failed_sync_of_a_batch_gives_its_losers_conflict_and_its_winners_outcome_unknown() {
    let fs = SimFs::new();
    let db = open(&fs);
    put1(&db, b"p", b"0").unwrap();
    let gate = fs.hold_syncs();
    // Fail the batch's first operation after A's sync.
    let writes = vec![
        (b"k".to_vec(), b"win".to_vec()),
        (b"k".to_vec(), b"lose".to_vec()),
    ];
    thread::scope(|s| {
        let (a, rest) = held_batch(s, &db, &gate, &writes);
        fs.fail_once_after(1);
        gate.release();
        assert!(join_within(a).is_ok());
        let r: Vec<_> = rest.into_iter().map(join_within).collect();
        assert!(is_outcome_unknown(&r[0]), "winner: {:?}", r[0]);
        assert!(
            matches!(r[1], Err(Error::Conflict { .. })),
            "loser: {:?}",
            r[1]
        );
    });
}

#[test]
fn acknowledged_commits_survive_a_power_cut_at_every_point_of_a_batch() {
    let d = batch_ops();
    // j = d cuts the power right after the batch is acknowledged.
    for j in 0..=d {
        for seed in 0..6 {
            let fs = SimFs::new();
            let acked = run_faulty_batch(&fs, &Fault::Cut, j, d);
            assert_eq!(acked, j >= d);
            fs.crash(seed * 31 + j);
            check_reopened(&fs, acked, &format!("cut j={j} seed={seed}"));
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[derive(Debug)]
struct Outcome {
    acked: Vec<u64>,
    unknown: Option<u64>,
    stopped: Vec<u64>,
}

#[test]
fn acknowledged_commits_from_many_threads_survive_random_power_cuts() {
    const THREADS: usize = 6;
    const COMMITS: u64 = 30;
    let mut rng = Rng(0x28);
    for round in 0..40u64 {
        let fs = SimFs::new();
        let db = open(&fs);
        let cut = rng.next() % 250;
        fs.fail_after(cut);
        let outcomes: Vec<Outcome> = thread::scope(|s| {
            let hs: Vec<_> = (0..THREADS)
                .map(|t| {
                    let db = &db;
                    s.spawn(move || {
                        let mut o = Outcome {
                            acked: vec![],
                            unknown: None,
                            stopped: vec![],
                        };
                        for n in 0..COMMITS {
                            let mut txn = db.begin();
                            // `put` may already say the engine stopped.
                            let r = txn
                                .put(format!("t{t}/{n:03}").as_bytes(), b"x")
                                .and_then(|()| {
                                    txn.put(
                                        format!("t{t}/last").as_bytes(),
                                        n.to_string().as_bytes(),
                                    )
                                })
                                .and_then(|()| txn.commit());
                            match r {
                                Ok(()) => o.acked.push(n),
                                Err(Error::OutcomeUnknown(_)) => {
                                    assert!(
                                        o.unknown.is_none(),
                                        "two OutcomeUnknown in one thread"
                                    );
                                    o.unknown = Some(n);
                                }
                                Err(Error::Stopped(_)) => o.stopped.push(n),
                                Err(e) => panic!("unexpected {e:?}"),
                            }
                            if o.unknown.is_some() || !o.stopped.is_empty() {
                                // Every later request must be Stopped too.
                                let r = put1(db, format!("t{t}/after").as_bytes(), b"x");
                                assert!(is_stopped(&r), "after a failure: {r:?}");
                                break;
                            }
                        }
                        o
                    })
                })
                .collect();
            hs.into_iter().map(join_within).collect()
        });
        drop(db);
        fs.crash(round);
        let db = Db::open_with(fs.clone(), DIR)
            .unwrap_or_else(|e| panic!("round {round} cut {cut}: reopen failed: {e:?}"));
        let txn = db.begin();
        for (t, o) in outcomes.iter().enumerate() {
            let has = |n: u64| {
                txn.get(format!("t{t}/{n:03}").as_bytes())
                    .unwrap()
                    .is_some()
            };
            let what = format!("round {round} cut {cut} thread {t} {o:?}");
            for &n in &o.acked {
                assert!(has(n), "{what}: acknowledged commit {n} lost");
            }
            for &n in &o.stopped {
                assert!(!has(n), "{what}: Stopped commit {n} is there");
            }
            assert_eq!(
                txn.get(format!("t{t}/after").as_bytes()).unwrap(),
                None,
                "{what}"
            );
            let last = txn
                .get(format!("t{t}/last").as_bytes())
                .unwrap()
                .map(|v| String::from_utf8(v).unwrap().parse::<u64>().unwrap());
            let pending_there = o.unknown.map(has).unwrap_or(false);
            let expected = if pending_there {
                o.unknown
            } else {
                o.acked.last().copied()
            };
            assert_eq!(last, expected, "{what}: commits not whole");
        }
    }
}

// ---------------------------------------------------------------- checkpoint

#[test]
fn a_checkpoint_request_ends_the_batch_before_it_and_runs_between_batches() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let gate = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let (a, first) = held_batch(s, db, &gate, &distinct_writes(3));
        let base = db.stats().received;
        let cp = s.spawn(|| db.checkpoint());
        wait_received(db, base + 1);
        let mut second = Vec::new();
        for i in 3..6 {
            let k = batch_key(i);
            second.push(s.spawn(move || put1(db, &k, b"v")));
            wait_received(db, base + 2 + (i - 3) as u64);
        }
        gate.release();
        assert!(join_within(a).is_ok());
        for h in first.into_iter().chain(second) {
            assert!(join_within(h).is_ok());
        }
        assert!(join_within(cp).is_ok());
    });
    let st = db.stats();
    assert_eq!(
        st.batches - before.batches,
        3,
        "A, then 3, checkpoint, then 3: {st:?}"
    );
    assert_eq!(
        st.wal_syncs - before.wal_syncs,
        3,
        "checkpoint syncs are not counted: {st:?}"
    );
    assert_eq!(st.largest_batch, 3, "{st:?}");
    assert_eq!(st.received - before.received, 8);
    drop(db);
    let db = open(&fs);
    assert_eq!(batch_present(&db, 6), 6);
}

fn wal_len(fs: &SimFs) -> u64 {
    fs.open(&std::path::Path::new(DIR).join(WAL_FILE))
        .unwrap()
        .size()
        .unwrap()
}

#[test]
fn a_commit_that_fills_the_wal_is_answered_before_the_checkpoint_it_starts() {
    let fs = SimFs::new();
    let db = open(&fs);
    let chunk = vec![7u8; 64 * 1024];
    let mut i = 0;
    // Fill the WAL until one more large commit crosses CHECKPOINT_AFTER.
    while wal_len(&fs) + 400 * 1024 < CHECKPOINT_AFTER {
        put1(&db, format!("fill{i}").as_bytes(), &chunk).unwrap();
        i += 1;
    }
    assert!(wal_len(&fs) < CHECKPOINT_AFTER);
    let big = vec![9u8; 600 * 1024];
    let gate = fs.hold_syncs();
    thread::scope(|s| {
        let h = s.spawn(|| put1(&db, b"big", &big));
        assert_eq!(
            gate.wait_for_sync().as_deref(),
            Some(COMMIT_THREAD),
            "the commit's WAL sync"
        );
        gate.let_waiting_go();
        // The checkpoint's first sync: the commit is answered by now.
        assert_eq!(
            gate.wait_for_sync().as_deref(),
            Some(COMMIT_THREAD),
            "the checkpoint's sync"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !h.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let answered_first = h.is_finished();
        gate.release();
        assert!(join_within(h).is_ok());
        assert!(
            answered_first,
            "the commit waited for the checkpoint it started"
        );
    });
    // The checkpoint did run: a request after it sees a short WAL.
    put1(&db, b"after", b"1").unwrap();
    assert!(wal_len(&fs) < CHECKPOINT_AFTER, "WAL was not checkpointed");
    assert_eq!(get(&db, b"big").map(|v| v.len()), Some(600 * 1024));
}

// ---------------------------------------------------------------- invariants

#[test]
fn counters_incremented_by_many_threads_with_retries_lose_no_update() {
    const THREADS: u64 = 8;
    const EACH: u64 = 40;
    let fs = SimFs::new();
    let db = open(&fs);
    put1(&db, b"c", b"0").unwrap();
    let before = db.stats();
    let seen_conflicts = AtomicU64::new(0);
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..EACH {
                    loop {
                        let mut txn = db.begin();
                        let n: u64 = String::from_utf8(txn.get(b"c").unwrap().unwrap())
                            .unwrap()
                            .parse()
                            .unwrap();
                        txn.put(b"c", (n + 1).to_string().as_bytes()).unwrap();
                        match txn.commit() {
                            Ok(()) => break,
                            Err(Error::Conflict { .. }) => {
                                seen_conflicts.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                }
            });
        }
    });
    let total = THREADS * EACH;
    assert_eq!(get(&db, b"c"), Some(total.to_string().into_bytes()));
    let st = db.stats();
    assert_eq!(st.commits - before.commits, total);
    assert_eq!(
        st.conflicts - before.conflicts,
        seen_conflicts.load(Ordering::SeqCst)
    );
    assert!(
        st.wal_syncs <= st.batches && st.largest_batch <= MAX_BATCH as u64,
        "{st:?}"
    );
    drop(db);
    let db = open(&fs);
    assert_eq!(get(&db, b"c"), Some(total.to_string().into_bytes()));
}

#[test]
fn bank_transfers_across_threads_keep_the_total_in_every_snapshot() {
    const ACCOUNTS: u64 = 5;
    const THREADS: u64 = 6;
    const EACH: u64 = 40;
    let fs = SimFs::new();
    let db = open(&fs);
    {
        let mut txn = db.begin();
        for a in 0..ACCOUNTS {
            txn.put(format!("acct{a}").as_bytes(), b"1000").unwrap();
        }
        txn.commit().unwrap();
    }
    let read = |txn: &oakdb::Txn<'_, SimFs>, a: u64| -> i64 {
        String::from_utf8(txn.get(format!("acct{a}").as_bytes()).unwrap().unwrap())
            .unwrap()
            .parse()
            .unwrap()
    };
    let done = AtomicBool::new(false);
    let bad = AtomicU64::new(0);
    thread::scope(|s| {
        let reader = s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                let txn = db.begin();
                let sum: i64 = (0..ACCOUNTS).map(|a| read(&txn, a)).sum();
                if sum != 1000 * ACCOUNTS as i64 {
                    bad.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let db = &db;
                s.spawn(move || {
                    let mut rng = Rng(t + 1);
                    for _ in 0..EACH {
                        let from = rng.next() % ACCOUNTS;
                        let to = (from + 1 + rng.next() % (ACCOUNTS - 1)) % ACCOUNTS;
                        let amount = (rng.next() % 50) as i64;
                        loop {
                            let mut txn = db.begin();
                            let f = read(&txn, from);
                            let g = read(&txn, to);
                            txn.put(
                                format!("acct{from}").as_bytes(),
                                (f - amount).to_string().as_bytes(),
                            )
                            .unwrap();
                            txn.put(
                                format!("acct{to}").as_bytes(),
                                (g + amount).to_string().as_bytes(),
                            )
                            .unwrap();
                            match txn.commit() {
                                Ok(()) => break,
                                Err(Error::Conflict { .. }) => continue,
                                Err(e) => panic!("{e:?}"),
                            }
                        }
                    }
                })
            })
            .collect();
        for w in workers {
            join_within(w);
        }
        done.store(true, Ordering::SeqCst);
        join_within(reader);
    });
    assert_eq!(
        bad.load(Ordering::SeqCst),
        0,
        "a snapshot saw money appear or vanish"
    );
    drop(db);
    let db = open(&fs);
    let txn = db.begin();
    let sum: i64 = (0..ACCOUNTS).map(|a| read(&txn, a)).sum();
    assert_eq!(sum, 1000 * ACCOUNTS as i64);
}

#[test]
fn a_transaction_still_conflicts_with_a_commit_made_after_it_began() {
    let fs = SimFs::new();
    let db = open(&fs);
    put1(&db, b"k", b"0").unwrap();
    let mut slow = db.begin();
    put1(&db, b"k", b"fast").unwrap();
    assert_eq!(
        slow.get(b"k").unwrap().as_deref(),
        Some(&b"0"[..]),
        "snapshot isolation"
    );
    slow.put(b"k", b"slow").unwrap();
    let before = db.stats().conflicts;
    assert!(matches!(slow.commit(), Err(Error::Conflict { .. })));
    assert_eq!(db.stats().conflicts, before + 1);
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"fast"[..]));
}

#[test]
fn a_commit_that_changes_nothing_new_still_counts_as_a_write_for_conflicts() {
    // Design 0021 Q3 / 0016: writing the same value is still a write.
    let fs = SimFs::new();
    let db = open(&fs);
    put1(&db, b"k", b"same").unwrap();
    let mut t = db.begin();
    put1(&db, b"k", b"same").unwrap();
    t.put(b"k", b"other").unwrap();
    assert!(matches!(t.commit(), Err(Error::Conflict { .. })));
}

#[test]
fn stats_start_at_zero_for_a_freshly_opened_database() {
    let fs = SimFs::new();
    let db = open(&fs);
    put1(&db, b"k", b"v").unwrap();
    drop(db);
    let db = open(&fs);
    let st = db.stats();
    assert_eq!(
        (
            st.received,
            st.commits,
            st.conflicts,
            st.batches,
            st.wal_syncs,
            st.largest_batch
        ),
        (0, 0, 0, 0, 0, 0),
        "{st:?}"
    );
}
