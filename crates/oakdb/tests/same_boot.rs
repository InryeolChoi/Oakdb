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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
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
    let mut t = db.begin();
    for (k, v) in batch {
        match v {
            Some(v) => t.put(k, v)?,
            None => t.delete(k)?,
        }
    }
    t.commit()
}

fn applied(m: &Model, b: &Batch) -> Model {
    let mut m = m.clone();
    for (k, v) in b {
        match v {
            Some(v) => {
                m.insert(k.clone(), v.clone());
            }
            None => {
                m.remove(k);
            }
        }
    }
    m
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
            let k = format!("k{:03}", rng.below(60)).into_bytes();
            if rng.below(6) == 0 {
                (k, None)
            } else {
                (k, Some(value(rng, tag)))
            }
        })
        .collect()
}

fn assert_stopped<T: std::fmt::Debug>(r: oakdb::Result<T>, what: &str) {
    match r {
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
        let b = batch(&mut rng, 6);
        commit(&db, &b).expect("setup commit");
        model = applied(&model, &b);
    }
    db.checkpoint().expect("setup checkpoint");
    for _ in 0..3 {
        let b = batch(&mut rng, 6);
        commit(&db, &b).expect("setup commit");
        model = applied(&model, &b);
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
        Action::Commit(b) => {
            let db = db.unwrap();
            match commit(&db, b) {
                Ok(()) => vec![applied(model, b)],
                Err(Error::OutcomeUnknown(_)) => {
                    assert_stopped(db.begin().get(b"k000"), "get after OutcomeUnknown");
                    assert_stopped(
                        commit(&db, &vec![(b"z".to_vec(), Some(b"z".to_vec()))]),
                        "commit after OutcomeUnknown",
                    );
                    vec![model.clone(), applied(model, b)]
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
fn after(fs: &SimFs, allowed: &[Model], seed: u64, ctx: &str) {
    let db = open(fs).unwrap_or_else(|e| panic!("{ctx}: same-boot reopen failed: {e:?}"));
    let mut seen = dump(&db);
    assert!(
        allowed.contains(&seen),
        "{ctx}: same-boot reopen shows a state that is neither before nor after"
    );
    let mut rng = Rng(seed ^ 0x5555);
    for _ in 0..3 {
        let b = batch(&mut rng, 5);
        commit(&db, &b).unwrap_or_else(|e| panic!("{ctx}: commit after reopen failed: {e:?}"));
        seen = applied(&seen, &b);
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
    let b = batch(&mut rng, 3);
    commit(&db, &b).expect("commit after final open");
    seen = applied(&seen, &b);
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

fn points(total: u64) -> Vec<u64> {
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
        for n in points(total) {
            for &seed in seeds {
                let (fs, db, model) = setup(mode, &action);
                fs.fail_once_after(n);
                let allowed = perform(&fs, db, &model, &action);
                after(
                    &fs,
                    &allowed,
                    seed,
                    &format!("{mode:?} fail at op {n}/{total} seed {seed}"),
                );
            }
        }
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn commit_failure_at_every_op_then_same_boot_reopen() {
    let b = batch(&mut Rng(9), 6);
    sweep(Action::Commit(b), &SEEDS);
}

#[test]
fn overflow_commit_failure_at_every_op_then_same_boot_reopen() {
    let big: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let b = vec![
        (b"k001".to_vec(), Some(big.clone())),
        (b"big2".to_vec(), Some(big[..20_000].to_vec())),
        (b"k002".to_vec(), None),
    ];
    sweep(Action::Commit(b), &SEEDS);
}

#[test]
fn delete_only_commit_failure_then_same_boot_reopen() {
    let b: Batch = (0..60)
        .map(|i| (format!("k{i:03}").into_bytes(), None))
        .collect();
    sweep(Action::Commit(b), &SEEDS);
}

#[test]
fn checkpoint_failure_at_every_op_then_same_boot_reopen() {
    sweep(Action::Checkpoint, &SEEDS);
}

#[test]
fn auto_checkpoint_failure_then_same_boot_reopen() {
    // One value bigger than CHECKPOINT_AFTER: the commit must checkpoint.
    let big: Vec<u8> = (0..CHECKPOINT_AFTER as usize + 300_000)
        .map(|i| (i * 13 % 256) as u8)
        .collect();
    let b = vec![
        (b"huge".to_vec(), Some(big)),
        (b"k003".to_vec(), Some(b"small".to_vec())),
    ];
    sweep(Action::Commit(b), &[3]);
}

#[test]
fn close_failure_then_same_boot_reopen() {
    sweep(Action::Close, &SEEDS);
}

#[test]
fn open_after_clean_close_failure_then_same_boot_reopen() {
    sweep(Action::OpenClean, &SEEDS);
}

#[test]
fn open_with_wal_to_replay_failure_then_same_boot_reopen() {
    sweep(Action::OpenDirty, &SEEDS);
}

#[test]
fn open_after_power_cut_failure_then_same_boot_reopen() {
    for crash_seed in [2u64, 11, 99] {
        sweep(Action::OpenAfterCrash(crash_seed), &[5, 6]);
    }
}

#[test]
fn creating_a_new_database_failure_then_same_boot_reopen() {
    sweep(Action::Create, &SEEDS);
}

/// The picture in design 0014: k1 acknowledged, k2's commit fails, the
/// database is reopened in the same boot, k3 is acknowledged, power cut.
#[test]
fn design_picture_k1_k2_k3_many_seeds() {
    for mode in MODES {
        let probe = |n: Option<u64>| {
            let fs = SimFs::new();
            fs.set_failed_sync(mode);
            let db = open(&fs).unwrap();
            commit(&db, &vec![(b"k1".to_vec(), Some(b"one".to_vec()))]).unwrap();
            let start = fs.ops();
            if let Some(n) = n {
                fs.fail_once_after(n);
            }
            let r = commit(&db, &vec![(b"k2".to_vec(), Some(b"two".to_vec()))]);
            (fs, db, r, start)
        };
        let (fs, _db, r, start) = probe(None);
        r.unwrap();
        let total = fs.ops() - start;
        for n in 0..total {
            for seed in 0..64u64 {
                let (fs, db, r, _) = probe(Some(n));
                assert!(
                    matches!(r, Err(Error::OutcomeUnknown(_))),
                    "{mode:?} n={n}: {r:?}"
                );
                drop(db);
                let db = open(&fs).unwrap();
                let k2_seen = db.begin().get(b"k2").unwrap();
                commit(&db, &vec![(b"k3".to_vec(), Some(b"three".to_vec()))]).unwrap();
                drop(db);
                fs.crash(seed);
                let db = open(&fs).unwrap();
                let t = db.begin();
                let ctx = format!("{mode:?} n={n} seed={seed}");
                assert_eq!(
                    t.get(b"k1").unwrap().as_deref(),
                    Some(&b"one"[..]),
                    "{ctx}: k1 lost"
                );
                assert_eq!(
                    t.get(b"k3").unwrap().as_deref(),
                    Some(&b"three"[..]),
                    "{ctx}: k3 lost"
                );
                assert_eq!(
                    t.get(b"k2").unwrap(),
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
fn power_cut_during_same_boot_reopen_after_failed_commit() {
    let b = batch(&mut Rng(77), 6);
    let action = Action::Commit(b.clone());
    for mode in MODES {
        let total = ops_of(mode, &action);
        for n in 0..total {
            // Count the reopen's operations for this failure point.
            let (fs, db, model) = setup(mode, &action);
            fs.fail_once_after(n);
            let allowed = perform(&fs, db, &model, &action);
            let start = fs.ops();
            drop(open(&fs).expect("reopen"));
            let reopen_ops = fs.ops() - start;
            for m in 0..=reopen_ops {
                for seed in [4u64, 8, 15, 16] {
                    let (fs, db, model) = setup(mode, &action);
                    fs.fail_once_after(n);
                    perform(&fs, db, &model, &action);
                    fs.fail_after(m);
                    drop(open(&fs));
                    fs.crash(seed);
                    after(
                        &fs,
                        &allowed,
                        seed + 100,
                        &format!("{mode:?} fail {n} cut {m}/{reopen_ops} seed {seed}"),
                    );
                }
            }
        }
    }
}

/// A failed checkpoint (pages, meta slot or WAL reset), then a power cut
/// at every operation of the same-boot reopen.
#[test]
fn power_cut_during_same_boot_reopen_after_failed_checkpoint() {
    let action = Action::Checkpoint;
    for mode in MODES {
        let total = ops_of(mode, &action);
        for n in points(total) {
            let (fs, db, model) = setup(mode, &action);
            fs.fail_once_after(n);
            perform(&fs, db, &model, &action);
            let start = fs.ops();
            drop(open(&fs).expect("reopen"));
            let reopen_ops = fs.ops() - start;
            for m in 0..=reopen_ops {
                for seed in [4u64, 8] {
                    let (fs, db, model) = setup(mode, &action);
                    fs.fail_once_after(n);
                    let allowed = perform(&fs, db, &model, &action);
                    fs.fail_after(m);
                    drop(open(&fs));
                    fs.crash(seed);
                    after(
                        &fs,
                        &allowed,
                        seed + 100,
                        &format!("{mode:?} ckpt fail {n} cut {m}/{reopen_ops} seed {seed}"),
                    );
                }
            }
        }
    }
}

/// Fail, reopen, fail again in the reopened database, several times over,
/// with no crash in between; then a power cut.
#[test]
fn repeated_commit_failures_and_same_boot_reopens() {
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
                    let b = batch(&mut rng, 4);
                    commit(&db, &b).unwrap();
                    cur = applied(&cur, &b);
                }
                let b = batch(&mut rng, 4);
                fs.fail_once_after(rng.below(4));
                match commit(&db, &b) {
                    Ok(()) => {
                        // The fault is still due; let the next commit hit it.
                        cur = applied(&cur, &b);
                        let b2 = batch(&mut rng, 4);
                        match commit(&db, &b2) {
                            Err(Error::OutcomeUnknown(_)) => {
                                cands = vec![cur.clone(), applied(&cur, &b2)]
                            }
                            Ok(()) => panic!("{mode:?} seed {seed}: fault never fired"),
                            Err(e) => panic!("{mode:?} seed {seed}: {e:?}"),
                        }
                    }
                    Err(Error::OutcomeUnknown(_)) => cands = vec![cur.clone(), applied(&cur, &b)],
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
fn two_failed_opens_in_a_row_then_a_good_one() {
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
                    after(
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
        let r = commit(&db, &vec![(b"b".to_vec(), Some(b"2".to_vec()))]);
        let Err(Error::OutcomeUnknown(cause)) = r else {
            panic!("{r:?}")
        };
        let mut t = db.begin();
        assert_stopped(t.get(b"a"), "get");
        assert_stopped(t.put(b"c", b"3"), "put");
        assert_stopped(t.delete(b"a"), "delete");
        let first = db.begin().scan::<&[u8]>(..).next().expect("one item");
        assert_stopped(first, "scan");
        assert_stopped(db.begin().commit(), "empty commit");
        assert_stopped(db.checkpoint(), "checkpoint");
        match db.close() {
            Err(Error::Stopped(inner)) => match &*inner {
                Error::OutcomeUnknown(c) => assert!(std::sync::Arc::ptr_eq(c, &cause)),
                other => panic!("Stopped holds {other:?}"),
            },
            other => panic!("close: {other:?}"),
        }
        after(
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
fn many_same_boot_reopens_without_crash_are_stable() {
    let b = batch(&mut Rng(5), 6);
    let action = Action::Commit(b);
    for mode in MODES {
        let total = ops_of(mode, &action);
        for n in 0..total {
            for seed in SEEDS {
                let (fs, db, model) = setup(mode, &action);
                fs.fail_once_after(n);
                let allowed = perform(&fs, db, &model, &action);
                let db = open(&fs).unwrap();
                let seen = dump(&db);
                assert!(allowed.contains(&seen));
                drop(db);
                for i in 0..4 {
                    let db = open(&fs).unwrap();
                    assert_eq!(dump(&db), seen, "{mode:?} n {n} reopen {i}");
                    if i % 2 == 0 {
                        db.close().unwrap();
                    }
                }
                fs.crash(seed);
                let db = open(&fs).unwrap();
                assert_eq!(
                    dump(&db),
                    seen,
                    "{mode:?} n {n} seed {seed}: crash after reopens"
                );
            }
        }
    }
}

/// A random mix of commits, checkpoints, closes, drops, crashes, one-off
/// disk failures and power cuts. The state on every open must be one the
/// history allows, and it becomes the only one allowed after that.
#[test]
fn random_failures_reopens_and_power_cuts() {
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
                let Some(d) = db.as_ref() else {
                    match open(&fs) {
                        Ok(d) => {
                            let seen = dump(&d);
                            assert!(
                                cands.contains(&seen),
                                "{ctx}: open shows a state the history does not allow"
                            );
                            cands = vec![seen];
                            db = Some(d);
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
                        let b = batch(&mut rng, size);
                        match commit(d, &b) {
                            Ok(()) => cands = vec![applied(&cur, &b)],
                            Err(Error::OutcomeUnknown(_)) => {
                                cands = vec![cur.clone(), applied(&cur, &b)];
                                db = None;
                            }
                            Err(Error::Stopped(_)) => db = None, // an auto-checkpoint failed
                            Err(e) => panic!("{ctx}: commit {e:?}"),
                        }
                    }
                    9 => {
                        if d.checkpoint().is_err() {
                            db = None;
                        }
                    }
                    10 => match try_dump(d) {
                        Ok(m) => assert_eq!(m, cur, "{ctx}: read differs from model"),
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
            let d = open(&fs).unwrap_or_else(|e| panic!("{mode:?} seed {seed}: final open {e:?}"));
            assert!(
                cands.contains(&dump(&d)),
                "{mode:?} seed {seed}: final state not allowed"
            );
        }
    }
}

/// Many seeds at every failure point of a commit, Forgets mode only (the
/// Linux case the issue is about).
#[test]
fn forgets_commit_failure_many_seeds() {
    let b = batch(&mut Rng(31), 8);
    let action = Action::Commit(b);
    let mode = FailedSync::Forgets;
    let total = ops_of(mode, &action);
    for n in 0..total {
        for seed in 0..48u64 {
            let (fs, db, model) = setup(mode, &action);
            fs.fail_once_after(n);
            let allowed = perform(&fs, db, &model, &action);
            after(&fs, &allowed, seed, &format!("Forgets n {n} seed {seed}"));
        }
    }
}

/// A failed commit in the database reopened after a failed commit: two
/// failures with a same-boot reopen in between, then a power cut.
#[test]
fn second_failure_after_same_boot_reopen() {
    let b1 = batch(&mut Rng(1), 5);
    let b2 = batch(&mut Rng(2), 5);
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
                let allowed2 = match commit(&db, &b2) {
                    Ok(()) => {
                        // The failure falls in the reopen's later work:
                        // make it fire in another commit.
                        let s = applied(&seen, &b2);
                        let b3 = vec![(b"x".to_vec(), Some(b"y".to_vec()))];
                        match commit(&db, &b3) {
                            // Fault still due after both commits: out of range.
                            Ok(()) => continue,
                            Err(Error::OutcomeUnknown(_)) | Err(Error::Stopped(_)) => {
                                vec![s.clone(), applied(&s, &b3)]
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                    Err(Error::OutcomeUnknown(_)) => vec![seen.clone(), applied(&seen, &b2)],
                    Err(e) => panic!("{e:?}"),
                };
                drop(db);
                after(
                    &fs,
                    &allowed2,
                    n1 * 100 + n2,
                    &format!("{mode:?} fail {n1} then {n2}"),
                );
            }
        }
    }
}
