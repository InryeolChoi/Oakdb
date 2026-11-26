//! Group commit (issue #28, design 0021, ADR 0007): the commit thread takes
//! every commit waiting for it, up to `MAX_BATCH`, writes them to the WAL
//! at once and syncs once, then answers them all.
//!
//! Batches are made without sleeping: `SimFs::hold_syncs` holds one commit
//! in its sync while others queue up behind it, and `Db::stats().received`
//! tells the test when they are all waiting. `HeldSyncs::let_waiting_go`
//! then lets that sync go and holds the batch's own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::sim::{HeldSyncs, SimFs, TornSector};
use oakdb::fs::{Fs, FsFile};
use oakdb::{
    Db, Error, Stats, Txn, BATCH_SIZE_BUCKETS, CHECKPOINT_AFTER, COMMIT_THREAD, MAX_BATCH, WAL_FILE,
};

/// How long a test waits for something that must happen before failing.
const WAIT: Duration = Duration::from_secs(10);

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

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

fn all<F: Fs>(db: &Db<F>) -> Model {
    db.begin().scan::<&[u8]>(..).map(Result::unwrap).collect()
}

/// What the WAL holds now, synced or not.
fn wal_bytes(fs: &SimFs) -> Vec<u8> {
    let file = fs.open(&Path::new("/oak").join(WAL_FILE)).unwrap();
    let mut bytes = vec![0; usize::try_from(file.size().unwrap()).unwrap()];
    file.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Waits until `n` requests in all have reached the commit thread's queue.
fn wait_received<F: Fs>(db: &Db<F>, n: u64) {
    let deadline = Instant::now() + WAIT;
    while db.stats().received < n {
        assert!(
            Instant::now() < deadline,
            "{n} requests never reached the commit thread: {:?}",
            db.stats()
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Commits `txn` on its own thread of `s`, and returns once its request
/// waits at the commit thread.
fn send<'scope, 'db: 'scope>(
    s: &'scope thread::Scope<'scope, '_>,
    db: &'db Db<SimFs>,
    txn: Txn<'db, SimFs>,
) -> thread::ScopedJoinHandle<'scope, Result<(), Error>> {
    let before = db.stats().received;
    let handle = s.spawn(move || txn.commit());
    wait_received(db, before + 1);
    handle
}

/// A transaction that puts each of `keys` to `value`.
fn writing<'db>(db: &'db Db<SimFs>, keys: &[Vec<u8>], value: &[u8]) -> Txn<'db, SimFs> {
    let mut txn = db.begin();
    for key in keys {
        txn.put(key, value).unwrap();
    }
    txn
}

/// Starts a commit of `key` = `value` and waits until it is held in its
/// sync: the commit the next ones queue up behind.
fn hold_first<'scope, 'db: 'scope>(
    s: &'scope thread::Scope<'scope, '_>,
    db: &'db Db<SimFs>,
    held: &HeldSyncs,
    key: &[u8],
    value: &[u8],
) -> thread::ScopedJoinHandle<'scope, Result<(), Error>> {
    let txn = writing(db, &[key.to_vec()], value);
    let handle = s.spawn(move || txn.commit());
    held.wait_for_sync();
    handle
}

fn delta(after: Stats, before: Stats) -> (u64, u64, u64, u64) {
    (
        after.commits - before.commits,
        after.batches - before.batches,
        after.wal_syncs - before.wal_syncs,
        after.conflicts - before.conflicts,
    )
}

#[test]
fn commits_waiting_behind_a_sync_share_the_next_one() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let held = fs.hold_syncs();
    let ops_while_held = thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let ops = fs.ops();
        let waiting: Vec<_> = (0..5u8)
            .map(|i| send(s, db, writing(db, &[vec![b'k', i]], b"v")))
            .collect();
        held.release();
        first.join().unwrap().unwrap();
        for commit in waiting {
            commit.join().unwrap().unwrap();
        }
        fs.ops() - ops
    });
    let after = db.stats();
    // Six commits in two batches: the first alone, then the five that
    // waited behind it, with one sync for the five.
    assert_eq!(delta(after, before), (6, 2, 2, 0), "{after:?}");
    assert_eq!(after.largest_batch, 5);
    // The first's sync, then one write and one sync for the five.
    assert_eq!(ops_while_held, 3);
    drop(db);
    let db = open(&fs);
    for i in 0..5u8 {
        assert_eq!(get(&db, &[b'k', i]), Some(b"v".to_vec()));
    }
}

#[test]
fn a_batch_holds_at_most_max_batch_commits() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let n = u32::try_from(MAX_BATCH).unwrap() + 2;
        let start = db.stats().received;
        let waiting: Vec<_> = (0..n)
            .map(|i| {
                let txn = writing(db, &[i.to_be_bytes().to_vec()], b"v");
                s.spawn(move || txn.commit())
            })
            .collect();
        wait_received(db, start + u64::from(n));
        held.release();
        first.join().unwrap().unwrap();
        for commit in waiting {
            commit.join().unwrap().unwrap();
        }
    });
    let after = db.stats();
    assert_eq!(after.largest_batch, MAX_BATCH as u64, "{after:?}");
    // The first, a full batch, and the two left over.
    assert_eq!(after.batches - before.batches, 3, "{after:?}");
    assert_eq!(after.wal_syncs - before.wal_syncs, 3, "{after:?}");
}

/// Which bucket of `Stats::batch_sizes` a batch of `len` commits counts
/// in: 1, 2-3, 4-7, ..., 64-127, 128 and more.
fn bucket(len: u64) -> usize {
    usize::try_from(len.ilog2())
        .unwrap()
        .min(BATCH_SIZE_BUCKETS - 1)
}

#[test]
fn each_batch_counts_once_in_the_bucket_of_its_size() {
    // A batch of one, then one of five, then the most a batch holds.
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let waiting: Vec<_> = (0..5u8)
            .map(|i| send(s, db, writing(db, &[vec![b'k', i]], b"v")))
            .collect();
        held.let_waiting_go();
        first.join().unwrap().unwrap();
        held.wait_for_sync();
        let n = u32::try_from(MAX_BATCH).unwrap();
        let start = db.stats().received;
        let full: Vec<_> = (0..n)
            .map(|i| {
                let txn = writing(db, &[i.to_be_bytes().to_vec()], b"v");
                s.spawn(move || txn.commit())
            })
            .collect();
        wait_received(db, start + u64::from(n));
        held.release();
        for commit in waiting.into_iter().chain(full) {
            commit.join().unwrap().unwrap();
        }
    });
    let after = db.stats();
    let mut grew = [0u64; BATCH_SIZE_BUCKETS];
    for (at, (a, b)) in after.batch_sizes.iter().zip(before.batch_sizes).enumerate() {
        grew[at] = a - b;
    }
    let mut want = [0u64; BATCH_SIZE_BUCKETS];
    for len in [1, 5, MAX_BATCH as u64] {
        want[bucket(len)] += 1;
    }
    assert_eq!(grew, want, "{after:?}");
    assert_eq!(after.batch_sizes.iter().sum::<u64>(), after.batches);
    assert!(
        after.wal_write_nanos > before.wal_write_nanos,
        "writing the WAL took no time: {after:?}"
    );
}

#[test]
fn the_buckets_split_at_powers_of_two() {
    assert_eq!(BATCH_SIZE_BUCKETS, 8);
    let edges = [
        (1, 0),
        (2, 1),
        (3, 1),
        (4, 2),
        (7, 2),
        (8, 3),
        (127, 6),
        (128, 7),
    ];
    for (len, at) in edges {
        assert_eq!(bucket(len), at, "batch of {len}");
    }
}

#[test]
fn of_two_commits_in_one_batch_writing_one_key_the_later_conflicts() {
    // Both began before either committed, so the second to arrive must
    // fail as it would in separate batches: otherwise it overwrites the
    // first's write without seeing it, a lost update.
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"k", b"0");
    let before = db.stats();
    let held = fs.hold_syncs();
    let (b, c) = thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let b = writing(db, &[b"k".to_vec()], b"b");
        let c = writing(db, &[b"k".to_vec(), b"other".to_vec()], b"c");
        let b = send(s, db, b);
        let c = send(s, db, c);
        held.release();
        first.join().unwrap().unwrap();
        (b.join().unwrap(), c.join().unwrap())
    });
    b.unwrap();
    match c {
        Err(Error::Conflict { key }) => assert_eq!(key, b"k"),
        other => panic!("expected a conflict on k, got {other:?}"),
    }
    let after = db.stats();
    assert_eq!(delta(after, before), (2, 2, 2, 1), "{after:?}");
    assert_eq!(after.largest_batch, 2);
    assert_eq!(get(&db, b"k"), Some(b"b".to_vec()));
    assert_eq!(get(&db, b"other"), None);
    drop(db);
    let db = open(&fs);
    assert_eq!(get(&db, b"k"), Some(b"b".to_vec()));
    assert_eq!(get(&db, b"other"), None);
}

#[test]
fn a_batch_shows_all_at_once_and_only_after_its_sync() {
    let fs = SimFs::new();
    let db = open(&fs);
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let b = send(s, db, writing(db, &[b"b".to_vec()], b"2"));
        let c = send(s, db, writing(db, &[b"c".to_vec()], b"3"));
        held.let_waiting_go();
        // The batch of b and c is written and held in its sync.
        assert_eq!(held.wait_for_sync().as_deref(), Some(COMMIT_THREAD));
        first.join().unwrap().unwrap();
        assert_eq!(get(db, b"a"), Some(b"1".to_vec()));
        assert_eq!(get(db, b"b"), None, "b shows before its sync");
        assert_eq!(get(db, b"c"), None, "c shows before its sync");
        held.release();
        b.join().unwrap().unwrap();
        c.join().unwrap().unwrap();
    });
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
    assert_eq!(get(&db, b"c"), Some(b"3".to_vec()));
}

#[test]
fn a_failed_batch_sync_leaves_every_commit_in_it_in_doubt() {
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"before", b"0");
    let held = fs.hold_syncs();
    let (first, batch) = thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let batch: Vec<_> = [b"b", b"c", b"d"]
            .into_iter()
            .map(|key| send(s, db, writing(db, &[key.to_vec()], b"v")))
            .collect();
        held.let_waiting_go();
        held.wait_for_sync();
        // The batch's sync fails.
        fs.fail_once_after(0);
        held.release();
        let first = first.join().unwrap();
        let batch: Vec<_> = batch.into_iter().map(|t| t.join().unwrap()).collect();
        (first, batch)
    });
    first.unwrap();
    for result in &batch {
        assert!(
            matches!(result, Err(Error::OutcomeUnknown(_))),
            "{result:?}"
        );
    }
    let mut after = db.begin();
    assert!(matches!(after.put(b"e", b"v"), Err(Error::Stopped(_))));
    assert!(matches!(after.commit(), Err(Error::Stopped(_))));
    assert!(matches!(db.checkpoint(), Err(Error::Stopped(_))));
    drop(db);
    let db = open(&fs);
    assert_eq!(get(&db, b"before"), Some(b"0".to_vec()));
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    let landed: Vec<_> = [b"b", b"c", b"d"]
        .into_iter()
        .map(|key| get(&db, key).is_some())
        .collect();
    assert!(
        landed.iter().all(|&l| l) || landed.iter().all(|&l| !l),
        "half a batch landed: {landed:?}"
    );
    assert_eq!(get(&db, b"e"), None);
}

/// Values that show where each commit is in the WAL.
const SECOND: &[u8] = b"second-commit-value";
const THIRD: &[u8] = b"third-commit-value";

#[test]
fn a_checkpoint_request_cuts_a_batch() {
    // A checkpoint runs after the commits sent before it and before the
    // ones sent after it, so it ends one batch and the next begins.
    let fs = SimFs::new();
    let db = open(&fs);
    let before = db.stats();
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let second = send(s, db, writing(db, &[b"b".to_vec()], SECOND));
        let start = db.stats().received;
        let checkpoint = s.spawn(move || db.checkpoint());
        wait_received(db, start + 1);
        let third = send(s, db, writing(db, &[b"c".to_vec()], THIRD));
        held.release();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        checkpoint.join().unwrap().unwrap();
        third.join().unwrap().unwrap();
    });
    let after = db.stats();
    assert_eq!(after.batches - before.batches, 3, "{after:?}");
    assert_eq!(after.largest_batch, 1, "{after:?}");
    // The checkpoint emptied the WAL of the second commit; the third came
    // after it. All three keys share one leaf, so the third's page image
    // holds the second's value too, but only once: had the checkpoint run
    // before the second, the WAL would hold the second's own image as
    // well, and had it run after the third, neither.
    let wal = wal_bytes(&fs);
    assert!(
        find(&wal, THIRD).is_some(),
        "the checkpoint ran after the third"
    );
    let seconds = wal.windows(SECOND.len()).filter(|w| *w == SECOND).count();
    assert_eq!(seconds, 1, "the checkpoint ran before the second");
}

#[test]
fn a_commit_is_answered_before_the_checkpoint_it_sets_off() {
    // The batch is durable once synced: its answer does not wait for the
    // checkpoint that follows it (design 0021 G6).
    let fs = SimFs::new();
    let db = open(&fs);
    let value = vec![7; 3000];
    let mut i = 0u32;
    while wal_bytes(&fs).len() as u64 <= CHECKPOINT_AFTER - 256 * 1024 {
        let mut txn = db.begin();
        for _ in 0..16 {
            txn.put(&i.to_be_bytes(), &value).unwrap();
            i += 1;
        }
        txn.commit().unwrap();
    }
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let (done, answered) = mpsc::channel();
        let mut txn = db.begin();
        for k in 0..100u32 {
            txn.put(&(i + k).to_be_bytes(), &value).unwrap();
        }
        s.spawn(move || {
            let _ = done.send(txn.commit());
        });
        // The commit's sync, then the checkpoint's first.
        held.wait_for_sync();
        held.let_waiting_go();
        held.wait_for_sync();
        let answer = answered.recv_timeout(Duration::from_secs(3));
        held.release();
        assert!(
            matches!(answer, Ok(Ok(()))),
            "the commit waited for the checkpoint: {answer:?}"
        );
    });
    // The sync held after the commit's was the checkpoint's. A checkpoint
    // asked for now runs after it, and finds the WAL empty.
    db.checkpoint().unwrap();
    assert!(wal_bytes(&fs).len() < 1024);
}

/// A commit a test tried: its keys, the value it put under each, and how
/// it went.
struct Tried {
    keys: Vec<Vec<u8>>,
    value: Vec<u8>,
    result: Result<(), Error>,
}

/// Judges what opening after a power cut found, `got`, against `base`,
/// what was there before, and the commits tried since: acknowledged
/// commits always survive; a commit in doubt is all there or not at all;
/// a refused one is not there; nothing else is.
fn judge(got: &Model, base: &Model, tried: &[Tried]) -> Result<(), String> {
    let mut known: BTreeSet<&[u8]> = base.keys().map(Vec::as_slice).collect();
    for t in tried {
        known.extend(t.keys.iter().map(Vec::as_slice));
        let there = t
            .keys
            .iter()
            .filter(|key| got.get(*key) == Some(&t.value))
            .count();
        let whole = there == t.keys.len();
        match &t.result {
            Ok(()) if !whole => return Err(format!("acknowledged {:?} lost", t.keys)),
            Err(Error::OutcomeUnknown(_)) if there != 0 && !whole => {
                return Err(format!("half of {:?} landed", t.keys))
            }
            Err(Error::OutcomeUnknown(_)) | Ok(()) => {}
            Err(_) if there != 0 => return Err(format!("refused {:?} landed", t.keys)),
            Err(_) => {}
        }
    }
    for (key, value) in base {
        let rewritten = tried
            .iter()
            .any(|t| t.keys.contains(key) && got.get(key) == Some(&t.value));
        if got.get(key) != Some(value) && !rewritten {
            return Err(format!("{key:?} from before was lost"));
        }
    }
    match got.keys().find(|key| !known.contains(key.as_slice())) {
        Some(key) => Err(format!("{key:?} was never written")),
        None => Ok(()),
    }
}

#[test]
fn the_judge_tells_each_way_of_breaking() {
    let base: Model = [(b"x".to_vec(), b"0".to_vec())].into();
    let tried = |result| Tried {
        keys: vec![b"a".to_vec(), b"b".to_vec()],
        value: b"v".to_vec(),
        result,
    };
    let unknown = || Err(Error::OutcomeUnknown(std::io::Error::other("x").into()));
    let with = |keys: &[&[u8]]| {
        let mut m = base.clone();
        m.extend(keys.iter().map(|k| (k.to_vec(), b"v".to_vec())));
        m
    };
    assert!(judge(&with(&[b"a", b"b"]), &base, &[tried(Ok(()))]).is_ok());
    assert!(judge(&with(&[b"a"]), &base, &[tried(Ok(()))]).is_err());
    assert!(judge(&with(&[]), &base, &[tried(unknown())]).is_ok());
    assert!(judge(&with(&[b"a", b"b"]), &base, &[tried(unknown())]).is_ok());
    assert!(judge(&with(&[b"b"]), &base, &[tried(unknown())]).is_err());
    assert!(judge(
        &with(&[b"a"]),
        &base,
        &[tried(Err(Error::Conflict { key: vec![] }))]
    )
    .is_err());
    assert!(judge(&with(&[b"z"]), &base, &[]).is_err());
    assert!(judge(&Model::new(), &base, &[]).is_err());
}

/// Keys `n` of a commit: three, so "all or nothing" means something.
fn keys_of(t: u8, i: u8) -> Vec<Vec<u8>> {
    (0..3u8).map(|j| vec![b'k', t, i, j]).collect()
}

#[test]
fn a_power_cut_anywhere_in_a_batch_keeps_acknowledged_commits_and_whole_batches() {
    // Ops after the first commit's sync is let go: its sync, the batch's
    // write, the batch's sync. Cut the power at each, and once past them.
    for cut in 0..4 {
        for seed in 0..12u64 {
            let fs = SimFs::new();
            if seed % 2 == 1 {
                fs.set_torn_sector(TornSector::Garbled);
            }
            let db = open(&fs);
            put(&db, b"before", b"0");
            let base = all(&db);
            let held = fs.hold_syncs();
            let tried = thread::scope(|s| {
                let db = &db;
                let first_keys = keys_of(0, 0);
                let first = s.spawn({
                    let txn = writing(db, &first_keys, b"first");
                    move || txn.commit()
                });
                held.wait_for_sync();
                let batch: Vec<_> = (1..4u8)
                    .map(|t| {
                        let keys = keys_of(t, 0);
                        let value = vec![b'v', t];
                        let handle = send(s, db, writing(db, &keys, &value));
                        (keys, value, handle)
                    })
                    .collect();
                fs.fail_after(cut);
                held.release();
                let mut tried = vec![Tried {
                    keys: first_keys,
                    value: b"first".to_vec(),
                    result: first.join().unwrap(),
                }];
                tried.extend(batch.into_iter().map(|(keys, value, h)| Tried {
                    keys,
                    value,
                    result: h.join().unwrap(),
                }));
                tried
            });
            drop(db);
            fs.crash(seed);
            let db = open(&fs);
            let got = all(&db);
            if let Err(e) = judge(&got, &base, &tried) {
                panic!("cut {cut}, seed {seed}: {e}");
            }
            // The batch lands whole or not at all: it is one WAL commit.
            let landed: Vec<bool> = tried[1..]
                .iter()
                .map(|t| got.get(&t.keys[0]) == Some(&t.value))
                .collect();
            assert!(
                landed.iter().all(|&l| l) || landed.iter().all(|&l| !l),
                "cut {cut}, seed {seed}: half a batch landed: {landed:?}"
            );
        }
    }
}

/// A small deterministic random number generator for the power-cut runs.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

#[test]
fn power_cuts_among_threads_keep_every_acknowledged_commit() {
    power_cuts_among_threads(open);
}

/// Four threads commit while the power is cut at a random op, for 24 seeds;
/// opening again must find every acknowledged commit, each commit whole or
/// not at all. The databases come from `open`.
fn power_cuts_among_threads(open: impl Fn(&SimFs) -> Db<SimFs>) {
    // The threads' order is up to the scheduler, so which commits share a
    // batch is too; the judge does not depend on it.
    let mut rng = Rng(28);
    for seed in 0..24u64 {
        let fs = SimFs::new();
        let db = open(&fs);
        put(&db, b"before", b"0");
        let base = all(&db);
        fs.fail_after(4 + rng.below(80));
        let tried: Vec<Vec<Tried>> = thread::scope(|s| {
            let threads: Vec<_> = (0..4u8)
                .map(|t| {
                    let db = &db;
                    s.spawn(move || {
                        (0..12u8)
                            .map(|i| {
                                let keys = keys_of(t, i);
                                let value = vec![b'v', t, i];
                                let mut txn = db.begin();
                                let result = keys
                                    .iter()
                                    .try_for_each(|key| txn.put(key, &value))
                                    .and_then(|()| txn.commit());
                                Tried {
                                    keys,
                                    value,
                                    result,
                                }
                            })
                            .collect()
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        for thread in &tried {
            let first_error = thread.iter().position(|t| t.result.is_err());
            if let Some(at) = first_error {
                assert!(
                    thread[at..].iter().all(|t| t.result.is_err()),
                    "seed {seed}: Ok after the engine stopped"
                );
            }
        }
        drop(db);
        fs.crash(seed);
        let db = open(&fs);
        let tried: Vec<Tried> = tried.into_iter().flatten().collect();
        if let Err(e) = judge(&all(&db), &base, &tried) {
            panic!("seed {seed}: {e}");
        }
    }
}

/// Group commit turned off, for the benchmark's "off" line (issue #30,
/// design 0026 Q5): a database whose batches hold one commit each.
#[cfg(feature = "bench-knobs")]
fn open_one_at_a_time(fs: &SimFs) -> Db<SimFs> {
    Db::open_with_max_batch(fs.clone(), "/oak", 1).unwrap()
}

#[cfg(feature = "bench-knobs")]
#[test]
fn with_batches_of_one_every_commit_has_its_own_sync() {
    let fs = SimFs::new();
    let db = open_one_at_a_time(&fs);
    let before = db.stats();
    let held = fs.hold_syncs();
    thread::scope(|s| {
        let db = &db;
        let first = hold_first(s, db, &held, b"a", b"1");
        let waiting: Vec<_> = (0..5u8)
            .map(|i| send(s, db, writing(db, &[vec![b'k', i]], b"v")))
            .collect();
        held.release();
        first.join().unwrap().unwrap();
        for commit in waiting {
            commit.join().unwrap().unwrap();
        }
    });
    let after = db.stats();
    // Five waited behind the first, yet each went on its own.
    assert_eq!(delta(after, before), (6, 6, 6, 0), "{after:?}");
    assert_eq!(after.largest_batch, 1);
    assert_eq!(after.batch_sizes[0] - before.batch_sizes[0], 6);
}

#[cfg(feature = "bench-knobs")]
#[test]
fn with_batches_of_one_power_cuts_keep_every_acknowledged_commit() {
    power_cuts_among_threads(open_one_at_a_time);
}

#[cfg(feature = "bench-knobs")]
#[test]
#[should_panic(expected = "at least one")]
fn a_batch_cannot_hold_no_commit() {
    let _ = Db::open_with_max_batch(SimFs::new(), "/oak", 0);
}
