//! Blind tests for issue #59 (same-boot reopen after a failed sync),
//! written from the issue, design 0012/0014 and the public API only, by a
//! tester who did not read the implementation (PR #60's verification).
//!
//! The promise: every acknowledged commit survives, a failed commit is
//! all-or-nothing, whether the database is reopened in the same boot
//! (no crash) or after a power cut, under both `FailedSync` modes.

use std::collections::BTreeMap;

use oakdb::fs::sim::{FailedSync, SimFs};
use oakdb::{Db, Error, CHECKPOINT_AFTER};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
type Batch = Vec<(Vec<u8>, Option<Vec<u8>>)>;

const DIR: &str = "/data/db";
const MODES: [FailedSync; 2] = [FailedSync::KeepsWaiting, FailedSync::Forgets];
const SEEDS: [u64; 3] = [1, 7, 0xdead_beef];

// ---------------------------------------------------------------- helpers

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, fail_at: u64) -> u64 {
        self.next() % fail_at
    }
}

fn open(fs: &SimFs) -> oakdb::Result<Db<SimFs>> {
    Db::open_with(fs.clone(), DIR)
}

fn try_dump(db: &Db<SimFs>) -> oakdb::Result<Model> {
    db.begin().scan::<&[u8]>(..).collect()
}

fn dump(db: &Db<SimFs>) -> Model {
    try_dump(db).expect("scan")
}

fn commit(db: &Db<SimFs>, batch: &Batch) -> oakdb::Result<()> {
    let mut txn = db.begin();
    for (key, value) in batch {
        match value {
            Some(value) => txn.put(key, value)?,
            None => txn.delete(key)?,
        }
    }
    txn.commit()
}

fn with_writes(model: &Model, batch: &Batch) -> Model {
    let mut model = model.clone();
    for (key, value) in batch {
        match value {
            Some(value) => {
                model.insert(key.clone(), value.clone());
            }
            None => {
                model.remove(key);
            }
        }
    }
    model
}

fn value(rng: &mut Rng, tag: u64) -> Vec<u8> {
    let len = [8usize, 100, 900, 3000, 9000][rng.below(5) as usize];
    (0..len)
        .map(|i| (tag.wrapping_mul(31) as usize).wrapping_add(i * 7) as u8)
        .collect()
}

fn batch(rng: &mut Rng, size: usize) -> Batch {
    let tag = rng.next();
    (0..size)
        .map(|_| {
            let key = format!("k{:03}", rng.below(60)).into_bytes();
            if rng.below(6) == 0 {
                (key, None)
            } else {
                (key, Some(value(rng, tag)))
            }
        })
        .collect()
}

fn assert_stopped<T: std::fmt::Debug>(result: oakdb::Result<T>, what: &str) {
    match result {
        Err(Error::Stopped(_)) => {}
        other => panic!("{what}: expected Stopped, got {other:?}"),
    }
}

/// The step that may fail.
#[derive(Clone)]
enum Action {
    Commit(Batch),
    Checkpoint,
    Close,
    /// Open after a clean close (WAL holds only its header).
    OpenClean,
    /// Open after a drop (WAL holds commits to replay).
    OpenDirty,
    /// Open after a power cut with this seed.
    OpenAfterCrash(u64),
    /// Make a new database.
    Create,
}

/// Builds a database with some acknowledged history, ready for `action`.
fn setup(mode: FailedSync, action: &Action) -> (SimFs, Option<Db<SimFs>>, Model) {
    let fs = SimFs::new();
    fs.set_failed_sync(mode);
    if let Action::Create = action {
        return (fs, None, Model::new());
    }
    let mut rng = Rng(42);
    let db = open(&fs).expect("open new");
    let mut model = Model::new();
    for _ in 0..4 {
        let writes = batch(&mut rng, 6);
        commit(&db, &writes).expect("setup commit");
        model = with_writes(&model, &writes);
    }
    db.checkpoint().expect("setup checkpoint");
    for _ in 0..3 {
        let writes = batch(&mut rng, 6);
        commit(&db, &writes).expect("setup commit");
        model = with_writes(&model, &writes);
    }
    match action {
        Action::OpenClean => {
            db.close().expect("setup close");
            (fs, None, model)
        }
        Action::OpenDirty => {
            drop(db);
            (fs, None, model)
        }
        Action::OpenAfterCrash(seed) => {
            drop(db);
            fs.crash(*seed);
            // Acknowledged commits survive a crash; check it first so a
            // later failure is not blamed on the same-boot path.
            let db = open(&fs).expect("open after setup crash");
            assert_eq!(dump(&db), model, "setup crash lost acknowledged data");
            drop(db);
            (fs, None, model)
        }
        _ => (fs, Some(db), model),
    }
}

/// Runs `action`, returning the states the database may hold afterwards.
fn perform(fs: &SimFs, db: Option<Db<SimFs>>, model: &Model, action: &Action) -> Vec<Model> {
    match action {
        Action::Commit(writes) => {
            let db = db.unwrap();
            match commit(&db, writes) {
                Ok(()) => vec![with_writes(model, writes)],
                Err(Error::OutcomeUnknown(_)) => {
                    assert_stopped(db.begin().get(b"k000"), "get after OutcomeUnknown");
                    assert_stopped(
                        commit(&db, &vec![(b"z".to_vec(), Some(b"z".to_vec()))]),
                        "commit after OutcomeUnknown",
                    );
                    vec![model.clone(), with_writes(model, writes)]
                }
                Err(e) => panic!("commit failed with {e:?}, expected OutcomeUnknown"),
            }
        }
        Action::Checkpoint => {
            let db = db.unwrap();
            if db.checkpoint().is_err() {
                assert_stopped(db.begin().get(b"k000"), "get after failed checkpoint");
                assert_stopped(db.checkpoint(), "checkpoint after failed checkpoint");
            }
            vec![model.clone()]
        }
        Action::Close => {
            let _ = db.unwrap().close();
            vec![model.clone()]
        }
        Action::OpenClean | Action::OpenDirty | Action::OpenAfterCrash(_) | Action::Create => {
            if let Ok(db) = open(fs) {
                // If opening succeeded, it must show the acknowledged state.
                assert_eq!(&dump(&db), model, "open that succeeded shows wrong state");
            }
            vec![model.clone()]
        }
    }
}

/// Reopens in the same boot, commits on top, cuts the power and checks
/// that everything acknowledged after the reopen is still there.
fn opens_holding_one_of(fs: &SimFs, allowed: &[Model], seed: u64, ctx: &str) {
    let db = open(fs).unwrap_or_else(|e| panic!("{ctx}: same-boot reopen failed: {e:?}"));
    let mut seen = dump(&db);
    assert!(
        allowed.contains(&seen),
        "{ctx}: same-boot reopen shows a state that is neither before nor after"
    );
    let mut rng = Rng(seed ^ 0x5555);
    for _ in 0..3 {
        let writes = batch(&mut rng, 5);
        commit(&db, &writes).unwrap_or_else(|e| panic!("{ctx}: commit after reopen failed: {e:?}"));
        seen = with_writes(&seen, &writes);
    }
    drop(db);
    fs.crash(seed);
    let db = open(fs).unwrap_or_else(|e| panic!("{ctx}: open after crash failed: {e:?}"));
    assert_eq!(
        dump(&db),
        seen,
        "{ctx}: power cut after same-boot reopen lost acknowledged commits"
    );
    // And it keeps working.
    let writes = batch(&mut rng, 3);
    commit(&db, &writes).expect("commit after final open");
    seen = with_writes(&seen, &writes);
    db.close().expect("close");
    let db = open(fs).expect("open after close");
    assert_eq!(dump(&db), seen, "{ctx}: clean close lost data");
}

/// How many disk-changing operations `action` takes with no fault.
fn ops_of(mode: FailedSync, action: &Action) -> u64 {
    let (fs, db, model) = setup(mode, action);
    let start = fs.ops();
    perform(&fs, db, &model, action);
    fs.ops() - start
}

fn failure_points(total: u64) -> Vec<u64> {
    if total <= 160 {
        (0..total).collect()
    } else {
        let mut p: Vec<u64> = (0..60).collect();
        p.extend((60..total - 60).step_by(((total - 120) / 40).max(1) as usize));
        p.extend(total - 60..total);
        p
    }
}

/// Fails each operation of `action` once, then runs [`after`].
fn sweep(action: Action, seeds: &[u64]) {
    for mode in MODES {
        let total = ops_of(mode, &action);
        assert!(total > 0, "action changes nothing on disk");
        for fail_at in failure_points(total) {
            for &seed in seeds {
                let (fs, db, model) = setup(mode, &action);
                fs.fail_once_after(fail_at);
                let allowed = perform(&fs, db, &model, &action);
                opens_holding_one_of(
                    &fs,
                    &allowed,
                    seed,
                    &format!("{mode:?} fail at op {fail_at}/{total} seed {seed}"),
                );
            }
        }
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn a_commit_failing_at_any_operation_keeps_its_promise_when_reopened_in_the_same_boot() {
    let writes = batch(&mut Rng(9), 6);
    sweep(Action::Commit(writes), &SEEDS);
}

#[test]
fn a_commit_of_a_long_value_failing_at_any_operation_keeps_its_promise_when_reopened() {
    let big: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let writes = vec![
        (b"k001".to_vec(), Some(big.clone())),
        (b"big2".to_vec(), Some(big[..20_000].to_vec())),
        (b"k002".to_vec(), None),
    ];
    sweep(Action::Commit(writes), &SEEDS);
}

#[test]
fn a_commit_of_deletes_only_that_fails_keeps_its_promise_when_reopened() {
    let writes: Batch = (0..60)
        .map(|i| (format!("k{i:03}").into_bytes(), None))
        .collect();
    sweep(Action::Commit(writes), &SEEDS);
}

#[test]
fn a_checkpoint_failing_at_any_operation_keeps_every_commit_when_reopened() {
    sweep(Action::Checkpoint, &SEEDS);
}

#[test]
fn a_checkpoint_a_commit_starts_that_fails_keeps_every_commit_when_reopened() {
    // One value bigger than CHECKPOINT_AFTER: the commit must checkpoint.
    let big: Vec<u8> = (0..CHECKPOINT_AFTER as usize + 300_000)
        .map(|i| (i * 13 % 256) as u8)
        .collect();
    let writes = vec![
        (b"huge".to_vec(), Some(big)),
        (b"k003".to_vec(), Some(b"small".to_vec())),
    ];
    sweep(Action::Commit(writes), &[3]);
}

#[test]
fn a_close_that_fails_keeps_every_commit_when_reopened() {
    sweep(Action::Close, &SEEDS);
}

#[test]
fn an_opening_after_a_clean_close_that_fails_leaves_a_database_that_opens() {
    sweep(Action::OpenClean, &SEEDS);
}

#[test]
fn an_opening_with_a_wal_to_replay_that_fails_leaves_a_database_that_opens() {
    sweep(Action::OpenDirty, &SEEDS);
}

#[test]
fn an_opening_after_a_power_cut_that_fails_leaves_a_database_that_opens() {
    for crash_seed in [2u64, 11, 99] {
        sweep(Action::OpenAfterCrash(crash_seed), &[5, 6]);
    }
}

#[test]
fn creating_a_database_that_fails_leaves_one_that_opens() {
    sweep(Action::Create, &SEEDS);
}

/// The picture in design 0014: k1 acknowledged, k2's commit fails, the
/// database is reopened in the same boot, k3 is acknowledged, power cut.
#[test]
fn a_commit_acknowledged_after_a_forgotten_one_survives_a_power_cut() {
    for mode in MODES {
        let probe = |fail_at: Option<u64>| {
            let fs = SimFs::new();
            fs.set_failed_sync(mode);
            let db = open(&fs).unwrap();
            commit(&db, &vec![(b"k1".to_vec(), Some(b"one".to_vec()))]).unwrap();
            let start = fs.ops();
            if let Some(fail_at) = fail_at {
                fs.fail_once_after(fail_at);
            }
            let result = commit(&db, &vec![(b"k2".to_vec(), Some(b"two".to_vec()))]);
            (fs, db, result, start)
        };
        let (fs, _db, result, start) = probe(None);
        result.unwrap();
        let total = fs.ops() - start;
        for fail_at in 0..total {
            for seed in 0..64u64 {
                let (fs, db, result, _) = probe(Some(fail_at));
                assert!(
                    matches!(result, Err(Error::OutcomeUnknown(_))),
                    "{mode:?} n={fail_at}: {result:?}"
                );
                drop(db);
                let db = open(&fs).unwrap();
                let k2_seen = db.begin().get(b"k2").unwrap();
                commit(&db, &vec![(b"k3".to_vec(), Some(b"three".to_vec()))]).unwrap();
                drop(db);
                fs.crash(seed);
                let db = open(&fs).unwrap();
                let txn = db.begin();
                let ctx = format!("{mode:?} n={fail_at} seed={seed}");
                assert_eq!(
                    txn.get(b"k1").unwrap().as_deref(),
                    Some(&b"one"[..]),
                    "{ctx}: k1 lost"
                );
                assert_eq!(
                    txn.get(b"k3").unwrap().as_deref(),
                    Some(&b"three"[..]),
                    "{ctx}: k3 lost"
                );
                assert_eq!(
                    txn.get(b"k2").unwrap(),
                    k2_seen,
                    "{ctx}: k2 changed after k3 was acknowledged on top of it"
                );
            }
        }
    }
}

/// A forgotten commit, then a power cut at every operation of the
/// same-boot reopen.
#[test]
fn a_power_cut_while_reopening_after_a_failed_commit_keeps_its_promise() {
    let writes = batch(&mut Rng(77), 6);
    let action = Action::Commit(writes.clone());
    for mode in MODES {
        let total = ops_of(mode, &action);
        for fail_at in 0..total {
            // Count the reopen's operations for this failure point.
            let (fs, db, model) = setup(mode, &action);
            fs.fail_once_after(fail_at);
            let allowed = perform(&fs, db, &model, &action);
            let start = fs.ops();
            drop(open(&fs).expect("reopen"));
            let reopen_ops = fs.ops() - start;
            for cut in 0..=reopen_ops {
                for seed in [4u64, 8, 15, 16] {
                    let (fs, db, model) = setup(mode, &action);
                    fs.fail_once_after(fail_at);
                    perform(&fs, db, &model, &action);
                    fs.fail_after(cut);
                    drop(open(&fs));
                    fs.crash(seed);
                    opens_holding_one_of(
                        &fs,
                        &allowed,
                        seed + 100,
                        &format!("{mode:?} fail {fail_at} cut {cut}/{reopen_ops} seed {seed}"),
                    );
                }
            }
        }
    }
}

/// A failed checkpoint (pages, meta slot or WAL reset), then a power cut
/// at every operation of the same-boot reopen.
#[test]
fn a_power_cut_while_reopening_after_a_failed_checkpoint_keeps_every_commit() {
    let action = Action::Checkpoint;
    for mode in MODES {
        let total = ops_of(mode, &action);
        for fail_at in failure_points(total) {
            let (fs, db, model) = setup(mode, &action);
            fs.fail_once_after(fail_at);
            perform(&fs, db, &model, &action);
            let start = fs.ops();
            drop(open(&fs).expect("reopen"));
            let reopen_ops = fs.ops() - start;
            for cut in 0..=reopen_ops {
                for seed in [4u64, 8] {
                    let (fs, db, model) = setup(mode, &action);
                    fs.fail_once_after(fail_at);
                    let allowed = perform(&fs, db, &model, &action);
                    fs.fail_after(cut);
                    drop(open(&fs));
                    fs.crash(seed);
                    opens_holding_one_of(
                        &fs,
                        &allowed,
                        seed + 100,
                        &format!("{mode:?} ckpt fail {fail_at} cut {cut}/{reopen_ops} seed {seed}"),
                    );
                }
            }
        }
    }
}

/// Fail, reopen, fail again in the reopened database, several times over,
/// with no crash in between; then a power cut.
#[test]
fn failed_commits_and_reopenings_in_turn_keep_every_promise() {
    for mode in MODES {
        for seed in 0..40u64 {
            let mut rng = Rng(seed);
            let fs = SimFs::new();
            fs.set_failed_sync(mode);
            let mut cands = vec![Model::new()];
            for round in 0..6 {
                let db = open(&fs)
                    .unwrap_or_else(|e| panic!("{mode:?} seed {seed} round {round}: {e:?}"));
                let seen = dump(&db);
                assert!(
                    cands.contains(&seen),
                    "{mode:?} seed {seed} round {round}: bad state on reopen"
                );
                let mut cur = seen;
                for _ in 0..rng.below(3) {
                    let writes = batch(&mut rng, 4);
                    commit(&db, &writes).unwrap();
                    cur = with_writes(&cur, &writes);
                }
                let writes = batch(&mut rng, 4);
                fs.fail_once_after(rng.below(4));
                match commit(&db, &writes) {
                    Ok(()) => {
                        // The fault is still due; let the next commit hit it.
                        cur = with_writes(&cur, &writes);
                        let more_writes = batch(&mut rng, 4);
                        match commit(&db, &more_writes) {
                            Err(Error::OutcomeUnknown(_)) => {
                                cands = vec![cur.clone(), with_writes(&cur, &more_writes)]
                            }
                            Ok(()) => panic!("{mode:?} seed {seed}: fault never fired"),
                            Err(e) => panic!("{mode:?} seed {seed}: {e:?}"),
                        }
                    }
                    Err(Error::OutcomeUnknown(_)) => {
                        cands = vec![cur.clone(), with_writes(&cur, &writes)]
                    }
                    Err(e) => panic!("{mode:?} seed {seed}: {e:?}"),
                }
            }
            fs.crash(seed);
            let db = open(&fs).unwrap();
            assert!(
                cands.contains(&dump(&db)),
                "{mode:?} seed {seed}: bad state after final crash"
            );
        }
    }
}

/// Opening fails, opening again in the same boot fails too, then works.
#[test]
fn two_failed_openings_in_a_row_leave_a_database_that_opens() {
    for mode in MODES {
        for action in [Action::OpenDirty, Action::Create] {
            let total = ops_of(mode, &action);
            for n1 in 0..total.min(40) {
                for n2 in [0u64, 1, 2, 3, 5, 8] {
                    let (fs, _db, model) = setup(mode, &action);
                    fs.fail_once_after(n1);
                    let _ = open(&fs).map(drop);
                    fs.fail_once_after(n2);
                    let start = fs.ops();
                    if open(&fs).map(drop).is_ok() && fs.ops() - start <= n2 {
                        // The second fault may still be due: not this case.
                        continue;
                    }
                    opens_holding_one_of(
                        &fs,
                        std::slice::from_ref(&model),
                        n1 * 31 + n2,
                        &format!("{mode:?} open fails {n1} then {n2}"),
                    );
                }
            }
        }
    }
}

/// After a failure, every request that returns data is `Stopped`, and
/// `Stopped` holds the very error the disk gave.
#[test]
fn every_request_is_stopped_after_a_failed_commit() {
    for mode in MODES {
        let fs = SimFs::new();
        fs.set_failed_sync(mode);
        let db = open(&fs).unwrap();
        commit(&db, &vec![(b"a".to_vec(), Some(b"1".to_vec()))]).unwrap();
        fs.fail_once_after(0);
        let result = commit(&db, &vec![(b"b".to_vec(), Some(b"2".to_vec()))]);
        let Err(Error::OutcomeUnknown(cause)) = result else {
            panic!("{result:?}")
        };
        let mut txn = db.begin();
        assert_stopped(txn.get(b"a"), "get");
        assert_stopped(txn.put(b"c", b"3"), "put");
        assert_stopped(txn.delete(b"a"), "delete");
        let first = db.begin().scan::<&[u8]>(..).next().expect("one item");
        assert_stopped(first, "scan");
        assert_stopped(db.begin().commit(), "empty commit");
        assert_stopped(db.checkpoint(), "checkpoint");
        drop(txn);
        match db.close() {
            Err(Error::Stopped(inner)) => match &*inner {
                Error::OutcomeUnknown(c) => assert!(std::sync::Arc::ptr_eq(c, &cause)),
                other => panic!("Stopped holds {other:?}"),
            },
            other => panic!("close: {other:?}"),
        }
        opens_holding_one_of(
            &fs,
            &[
                [(b"a".to_vec(), b"1".to_vec())].into(),
                [
                    (b"a".to_vec(), b"1".to_vec()),
                    (b"b".to_vec(), b"2".to_vec()),
                ]
                .into(),
            ],
            3,
            "stopped",
        );
    }
}

/// After a failed commit, several same-boot reopens and clean closes with
/// no crash keep the same state, and a crash at the end keeps it too.
#[test]
fn reopening_many_times_in_the_same_boot_changes_nothing() {
    let writes = batch(&mut Rng(5), 6);
    let action = Action::Commit(writes);
    for mode in MODES {
        let total = ops_of(mode, &action);
        for fail_at in 0..total {
            for seed in SEEDS {
                let (fs, db, model) = setup(mode, &action);
                fs.fail_once_after(fail_at);
                let allowed = perform(&fs, db, &model, &action);
                let db = open(&fs).unwrap();
                let seen = dump(&db);
                assert!(allowed.contains(&seen));
                drop(db);
                for i in 0..4 {
                    let db = open(&fs).unwrap();
                    assert_eq!(dump(&db), seen, "{mode:?} n {fail_at} reopen {i}");
                    if i % 2 == 0 {
                        db.close().unwrap();
                    }
                }
                fs.crash(seed);
                let db = open(&fs).unwrap();
                assert_eq!(
                    dump(&db),
                    seen,
                    "{mode:?} n {fail_at} seed {seed}: crash after reopens"
                );
            }
        }
    }
}

/// A random mix of commits, checkpoints, closes, drops, crashes, one-off
/// disk failures and power cuts. The state on every open must be one the
/// history allows, and it becomes the only one allowed after that.
#[test]
fn random_failures_reopenings_and_power_cuts_keep_every_promise() {
    for mode in MODES {
        for seed in 0..150u64 {
            let mut rng = Rng(seed.wrapping_mul(0x1234_5678) ^ mode as u64);
            let fs = SimFs::new();
            fs.set_failed_sync(mode);
            let mut cands = vec![Model::new()];
            let mut db: Option<Db<SimFs>> = None;
            let mut power_cut_armed = false;
            let mut failed_opens = 0;
            for step in 0..80 {
                let ctx = format!("{mode:?} seed {seed} step {step}");
                let Some(reopened) = db.as_ref() else {
                    match open(&fs) {
                        Ok(reopened) => {
                            let seen = dump(&reopened);
                            assert!(
                                cands.contains(&seen),
                                "{ctx}: open shows a state the history does not allow"
                            );
                            cands = vec![seen];
                            db = Some(reopened);
                            failed_opens = 0;
                        }
                        Err(e) => {
                            failed_opens += 1;
                            assert!(
                                power_cut_armed || failed_opens < 2,
                                "{ctx}: second same-boot open fails with no fault due: {e:?}"
                            );
                            if power_cut_armed {
                                fs.crash(rng.next());
                                power_cut_armed = false;
                            }
                        }
                    }
                    continue;
                };
                let cur = cands[0].clone();
                match rng.below(20) {
                    0..=8 => {
                        let size = 1 + rng.below(5) as usize;
                        let writes = batch(&mut rng, size);
                        match commit(reopened, &writes) {
                            Ok(()) => cands = vec![with_writes(&cur, &writes)],
                            Err(Error::OutcomeUnknown(_)) => {
                                cands = vec![cur.clone(), with_writes(&cur, &writes)];
                                db = None;
                            }
                            Err(Error::Stopped(_)) => db = None, // an auto-checkpoint failed
                            Err(e) => panic!("{ctx}: commit {e:?}"),
                        }
                    }
                    9 => {
                        if reopened.checkpoint().is_err() {
                            db = None;
                        }
                    }
                    10 => match try_dump(reopened) {
                        Ok(cut) => assert_eq!(cut, cur, "{ctx}: read differs from model"),
                        Err(Error::Stopped(_)) => db = None,
                        Err(e) => panic!("{ctx}: scan {e:?}"),
                    },
                    11 => {
                        let _ = db.take().unwrap().close();
                    }
                    12 => db = None,
                    13 => {
                        db = None;
                        fs.crash(rng.next());
                        power_cut_armed = false;
                    }
                    14..=17 => fs.fail_once_after(rng.below(30)),
                    _ => {
                        fs.fail_after(rng.below(60));
                        power_cut_armed = true;
                    }
                }
            }
            drop(db);
            fs.crash(seed);
            let reopened =
                open(&fs).unwrap_or_else(|e| panic!("{mode:?} seed {seed}: final open {e:?}"));
            assert!(
                cands.contains(&dump(&reopened)),
                "{mode:?} seed {seed}: final state not allowed"
            );
        }
    }
}

/// Many seeds at every failure point of a commit, Forgets mode only (the
/// Linux case the issue is about).
#[test]
fn a_forgotten_commit_keeps_its_promise_for_many_seeds() {
    let writes = batch(&mut Rng(31), 8);
    let action = Action::Commit(writes);
    let mode = FailedSync::Forgets;
    let total = ops_of(mode, &action);
    for fail_at in 0..total {
        for seed in 0..48u64 {
            let (fs, db, model) = setup(mode, &action);
            fs.fail_once_after(fail_at);
            let allowed = perform(&fs, db, &model, &action);
            opens_holding_one_of(
                &fs,
                &allowed,
                seed,
                &format!("Forgets n {fail_at} seed {seed}"),
            );
        }
    }
}

/// A failed commit in the database reopened after a failed commit: two
/// failures with a same-boot reopen in between, then a power cut.
#[test]
fn a_second_failure_after_reopening_keeps_every_promise() {
    let b1 = batch(&mut Rng(1), 5);
    let more_writes = batch(&mut Rng(2), 5);
    let action = Action::Commit(b1.clone());
    for mode in MODES {
        let total = ops_of(mode, &action);
        for n1 in 0..total {
            for n2 in 0..total + 6 {
                let (fs, db, model) = setup(mode, &action);
                fs.fail_once_after(n1);
                let allowed1 = perform(&fs, db, &model, &action);
                let db = open(&fs).unwrap();
                let seen = dump(&db);
                assert!(allowed1.contains(&seen));
                fs.fail_once_after(n2);
                let allowed2 = match commit(&db, &more_writes) {
                    Ok(()) => {
                        // The failure falls in the reopen's later work:
                        // make it fire in another commit.
                        let next = with_writes(&seen, &more_writes);
                        let b3 = vec![(b"x".to_vec(), Some(b"y".to_vec()))];
                        match commit(&db, &b3) {
                            // Fault still due after both commits: out of range.
                            Ok(()) => continue,
                            Err(Error::OutcomeUnknown(_)) | Err(Error::Stopped(_)) => {
                                vec![next.clone(), with_writes(&next, &b3)]
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                    Err(Error::OutcomeUnknown(_)) => {
                        vec![seen.clone(), with_writes(&seen, &more_writes)]
                    }
                    Err(e) => panic!("{e:?}"),
                };
                drop(db);
                opens_holding_one_of(
                    &fs,
                    &allowed2,
                    n1 * 100 + n2,
                    &format!("{mode:?} fail {n1} then {n2}"),
                );
            }
        }
    }
}
