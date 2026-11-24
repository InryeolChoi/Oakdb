//! Disk failures with the power on (issue #24, design 0012, ADR 0007): when
//! a write or sync fails, the engine does not retry. The commit that hit
//! the failure is told its outcome is unknown, every request after it is
//! told the engine stopped, and opening the database again recovers every
//! acknowledged commit from the WAL, with the failed one there completely
//! or not at all.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use oakdb::fs::sim::{FailedSync, SimFs, TornSector};
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error, WAL_FILE};

/// What the database holds: key number → value.
type Model = BTreeMap<u32, Vec<u8>>;

fn dir() -> PathBuf {
    PathBuf::from("/oak")
}

fn open(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), dir()).unwrap()
}

fn put(db: &Db<SimFs>, key: u32, value: &[u8], model: &mut Model) {
    let mut txn = db.begin();
    txn.put(&key.to_be_bytes(), value).unwrap();
    txn.commit().unwrap();
    model.insert(key, value.to_vec());
}

/// Keys 0..n with values that differ by key, one commit each, and a long
/// value that goes to overflow pages.
fn fill(db: &Db<SimFs>, n: u32) -> Model {
    let mut model = Model::new();
    for i in 0..n {
        put(
            db,
            i,
            &vec![i as u8; 50 + (i as usize * 37) % 300],
            &mut model,
        );
    }
    put(db, 1000, &vec![7; 20_000], &mut model);
    model
}

/// Everything the database holds.
fn read_all(db: &Db<SimFs>) -> Model {
    db.begin()
        .scan::<&[u8]>(..)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (u32::from_be_bytes(key.try_into().unwrap()), value)
        })
        .collect()
}

fn is_stopped<T>(result: Result<T, Error>) -> bool {
    matches!(result, Err(Error::Stopped(_)))
}

/// Checks that every kind of request fails with [`Error::Stopped`].
fn assert_stopped(db: &Db<SimFs>, what: &str) {
    let mut txn = db.begin();
    assert!(is_stopped(txn.get(b"k")), "{what}: get");
    assert!(is_stopped(txn.put(b"k", b"v")), "{what}: put");
    assert!(is_stopped(txn.delete(b"k")), "{what}: delete");
    let mut scan = txn.scan::<&[u8]>(..);
    assert!(
        matches!(scan.next(), Some(Err(Error::Stopped(_)))),
        "{what}: scan"
    );
    assert!(scan.next().is_none(), "{what}: scan goes on");
    drop(scan);
    assert!(is_stopped(txn.commit()), "{what}: a commit of nothing");
    assert!(is_stopped(db.checkpoint()), "{what}: checkpoint");
}

/// A transaction that puts `value` under each of `keys`, and the model as
/// it would be once it commits.
fn writes<'db>(
    db: &'db Db<SimFs>,
    keys: &[u32],
    value: &[u8],
    model: &Model,
) -> (oakdb::Txn<'db, SimFs>, Model) {
    let mut txn = db.begin();
    let mut next = model.clone();
    for &key in keys {
        txn.put(&key.to_be_bytes(), value).unwrap();
        next.insert(key, value.to_vec());
    }
    (txn, next)
}

/// Opens the database again, after a crash with `seed` if there is one,
/// and checks that it holds `acked` or, if the failed commit landed,
/// `landed`. Then checks the database goes on: a commit, a crash, and
/// opening again keep it. Returns whether the failed commit landed.
fn recovers(fs: &SimFs, crash: Option<u64>, acked: &Model, landed: &Model, what: &str) -> bool {
    if let Some(seed) = crash {
        fs.crash(seed);
    }
    let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{what}: {e}"));
    let got = read_all(&db);
    assert!(
        &got == acked || &got == landed,
        "{what}: lost an acknowledged commit or half the failed one"
    );
    let landed_now = &got == landed && acked != landed;
    let mut model = got;
    put(&db, 4242, b"later", &mut model);
    drop(db);
    fs.crash(crash.unwrap_or(0) + 1000);
    let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{what}, then: {e}"));
    assert_eq!(read_all(&db), model, "{what}, then");
    landed_now
}

/// Both ways a failed sync can treat the changes it was to save: they
/// keep waiting, or reads see them but the disk never gets them, as on
/// Linux (issue #59).
const FAILED_SYNCS: [FailedSync; 2] = [FailedSync::KeepsWaiting, FailedSync::Forgets];

/// An empty disk whose failed syncs do as `failed_sync` says.
fn disk_whose_failed_syncs(failed_sync: FailedSync) -> SimFs {
    let fs = SimFs::new();
    fs.set_failed_sync(failed_sync);
    fs
}

/// No crash, and crash seeds 0..4.
fn reopenings() -> impl Iterator<Item = Option<u64>> {
    std::iter::once(None).chain((0..4).map(Some))
}

/// Every way a failed sync can treat its changes, with every reopening.
fn sweeps() -> impl Iterator<Item = (FailedSync, Option<u64>)> {
    FAILED_SYNCS
        .into_iter()
        .flat_map(|failed_sync| reopenings().map(move |crash| (failed_sync, crash)))
}

#[test]
fn a_failed_wal_sync_makes_the_commit_unknown() {
    let fs = SimFs::new();
    let db = open(&fs);
    let model = fill(&db, 20);
    let (txn, _) = writes(&db, &[1, 2, 3], b"new", &model);

    // A commit is one write and one sync of the WAL: fail the sync.
    fs.fail_once_after(1);

    assert!(matches!(txn.commit(), Err(Error::OutcomeUnknown(_))));
}

#[test]
fn a_failed_wal_write_makes_the_commit_unknown_too() {
    let fs = SimFs::new();
    let db = open(&fs);
    let model = fill(&db, 20);
    let (txn, _) = writes(&db, &[1, 2, 3], b"new", &model);

    fs.fail_once_after(0);

    assert!(matches!(txn.commit(), Err(Error::OutcomeUnknown(_))));
}

#[test]
fn every_request_after_a_failure_says_the_engine_stopped() {
    let fs = SimFs::new();
    let db = open(&fs);
    let model = fill(&db, 20);
    let (begun_before, _) = writes(&db, &[5], b"waiting", &model);
    let (txn, _) = writes(&db, &[1], b"new", &model);
    fs.fail_once_after(1);
    let failed = txn.commit().unwrap_err();

    // The disk works again, but the engine does not trust it.
    assert_stopped(&db, "after a failed commit");
    assert!(
        is_stopped(begun_before.commit()),
        "a transaction begun before"
    );
    let txn = db.begin();
    let mut scan = txn.scan::<&[u8]>(..);
    match scan.next() {
        // The cause is the error the failed commit got, shared, not a copy.
        Some(Err(Error::Stopped(cause))) => match (&*cause, &failed) {
            (Error::OutcomeUnknown(kept), Error::OutcomeUnknown(got)) => {
                assert!(Arc::ptr_eq(kept, got), "the cause is a copy");
            }
            other => panic!("the cause and the failure: {other:?}"),
        },
        other => panic!("expected Stopped, got {other:?}"),
    }
    drop(scan);
    drop(txn);
    assert!(is_stopped(db.close()), "close");
}

#[test]
fn the_errors_say_what_happened_and_why() {
    let fs = SimFs::new();
    let db = open(&fs);
    let model = fill(&db, 5);
    let (txn, _) = writes(&db, &[1], b"new", &model);
    fs.fail_once_after(1);

    let failed = txn.commit().unwrap_err();
    let stopped = db.begin().get(b"k").unwrap_err();

    assert!(std::error::Error::source(&failed).is_some());
    assert!(std::error::Error::source(&stopped).is_some());
    assert!(failed.to_string().contains("unknown"), "{failed}");
    assert!(stopped.to_string().contains("stopped"), "{stopped}");
}

#[test]
fn a_stopped_database_holds_its_lock_until_it_is_dropped() {
    let fs = SimFs::new();
    let db = open(&fs);
    let model = fill(&db, 5);
    let (txn, _) = writes(&db, &[1], b"new", &model);
    fs.fail_once_after(1);
    txn.commit().unwrap_err();

    assert!(matches!(
        Db::open_with(fs.clone(), dir()),
        Err(Error::AlreadyOpen)
    ));
    drop(db);
    open(&fs);
}

#[test]
fn reopening_after_a_failed_sync_recovers_every_acknowledged_commit() {
    for failed_sync in FAILED_SYNCS {
        let fs = disk_whose_failed_syncs(failed_sync);
        let db = open(&fs);
        let model = fill(&db, 50);
        let (txn, landed) = writes(&db, &[1, 2, 3, 2000], b"new", &model);
        fs.fail_once_after(1);
        txn.commit().unwrap_err();
        drop(db);

        recovers(
            &fs,
            None,
            &model,
            &landed,
            &format!("{failed_sync:?}, no crash"),
        );
    }
}

#[test]
fn a_failure_at_any_disk_operation_of_a_commit_keeps_every_acknowledged_commit() {
    let ops = {
        let fs = SimFs::new();
        let db = open(&fs);
        let model = fill(&db, 60);
        let (txn, _) = writes(&db, &[1, 30, 59, 3000], &[9; 5000], &model);
        let start = fs.ops();
        txn.commit().unwrap();
        fs.ops() - start
    };
    assert_eq!(ops, 2, "a commit writes and syncs the WAL once");
    let mut landings = [0, 0];
    for fail in 0..ops {
        for (failed_sync, crash) in sweeps() {
            let what = format!("failing operation {fail}, crash {crash:?}, {failed_sync:?}");
            let fs = disk_whose_failed_syncs(failed_sync);
            let db = open(&fs);
            let model = fill(&db, 60);
            let (txn, landed) = writes(&db, &[1, 30, 59, 3000], &[9; 5000], &model);
            fs.fail_once_after(fail);

            let result = txn.commit();

            assert!(
                matches!(result, Err(Error::OutcomeUnknown(_))),
                "{what}: {result:?}"
            );
            assert_stopped(&db, &what);
            drop(db);
            landings[usize::from(recovers(&fs, crash, &model, &landed, &what))] += 1;
        }
    }
    // Outcome unknown means both happen.
    assert!(landings[0] > 0 && landings[1] > 0, "{landings:?}");
}

/// Builds a database checkpointed `before` times, with commits after the
/// last one, and returns what it holds.
fn history(db: &Db<SimFs>, before: u32) -> Model {
    let mut model = fill(db, 120);
    for round in 0..before {
        db.checkpoint().unwrap();
        for i in (round * 10)..(round * 10 + 10) {
            put(db, i, &vec![round as u8; 3000], &mut model);
        }
    }
    put(db, 500, &vec![9; 20_000], &mut model);
    model
}

#[test]
fn a_failure_at_any_disk_operation_of_a_checkpoint_keeps_every_commit() {
    // The first checkpoint writes both meta slots; later ones write one.
    for before in [0, 2] {
        let ops = {
            let fs = SimFs::new();
            let db = open(&fs);
            history(&db, before);
            let start = fs.ops();
            db.checkpoint().unwrap();
            fs.ops() - start
        };
        assert!(ops > 4, "a checkpoint takes {ops} operations");
        for fail in 0..ops {
            for (failed_sync, crash) in sweeps() {
                let what = format!(
                    "{before} checkpoints before, failing operation {fail}, crash {crash:?}, {failed_sync:?}"
                );
                let fs = disk_whose_failed_syncs(failed_sync);
                let db = open(&fs);
                let model = history(&db, before);
                fs.fail_once_after(fail);

                let result = db.checkpoint();

                // No commit is waiting on a checkpoint: the failure itself
                // is what it reports.
                assert!(matches!(result, Err(Error::Io(_))), "{what}: {result:?}");
                assert_stopped(&db, &what);
                drop(db);
                recovers(&fs, crash, &model, &model, &what);
            }
        }
    }
}

#[test]
fn a_commit_whose_checkpoint_fails_is_kept_and_stops_the_engine() {
    // Two commits of long values leave the WAL just under CHECKPOINT_AFTER;
    // the third goes over it and checkpoints.
    let long = |byte: u8| vec![byte; 1_600_000];
    let setup = |fs: &SimFs| {
        let db = open(fs);
        let mut model = Model::new();
        put(&db, 1, &long(1), &mut model);
        put(&db, 2, &long(2), &mut model);
        (db, model)
    };
    let ops = {
        let fs = SimFs::new();
        let (db, model) = setup(&fs);
        let (txn, _) = writes(&db, &[3], &long(3), &model);
        let start = fs.ops();
        txn.commit().unwrap();
        // The commit is answered before its checkpoint runs (design 0021
        // G6); a checkpoint asked for now waits for that one and finds
        // nothing to do.
        db.checkpoint().unwrap();
        fs.ops() - start
    };
    assert!(ops > 10, "the commit did not checkpoint: {ops} operations");
    // The WAL write and sync, the first operations of the checkpoint, and
    // its last ones: emptying the WAL and writing its header.
    let fails: Vec<u64> = [0, 1, 2, 3, ops / 2]
        .into_iter()
        .chain(ops - 5..ops)
        .collect();
    for fail in fails {
        for (failed_sync, crash) in sweeps() {
            let what =
                format!("failing operation {fail} of {ops}, crash {crash:?}, {failed_sync:?}");
            let fs = disk_whose_failed_syncs(failed_sync);
            let (db, model) = setup(&fs);
            let (txn, landed) = writes(&db, &[3], &long(3), &model);
            fs.fail_once_after(fail);

            let result = txn.commit();

            if fail < 2 {
                assert!(
                    matches!(result, Err(Error::OutcomeUnknown(_))),
                    "{what}: {result:?}"
                );
            } else {
                // The commit is in the WAL and synced before the checkpoint
                // starts: it is kept, whatever the checkpoint does.
                assert!(result.is_ok(), "{what}: {result:?}");
            }
            // The commit is answered before its checkpoint runs (design
            // 0021 G6): a checkpoint asked for now runs after that one.
            // The engine stopped either way: at the commit or at its
            // checkpoint.
            assert!(is_stopped(db.checkpoint()), "{what}: checkpoint");
            assert_stopped(&db, &what);
            drop(db);
            let acked = if fail < 2 { &model } else { &landed };
            recovers(&fs, crash, acked, &landed, &what);
        }
    }
}

/// Makes a database on an empty disk and returns what it holds.
type Setup = fn(&SimFs) -> Model;

/// A database whose WAL ends in bytes that are not a whole commit, which
/// opening keeps in the cut file and cuts off.
fn with_garbage_after_the_tail(fs: &SimFs) -> Model {
    let db = open(fs);
    let model = fill(&db, 30);
    drop(db);
    let wal = fs.open(&dir().join(WAL_FILE)).unwrap();
    wal.write_all_at(&[0xAB; 5000], wal.size().unwrap())
        .unwrap();
    wal.sync_data().unwrap();
    model
}

/// A database whose WAL is from before its last checkpoint, which opening
/// empties and starts again.
fn with_a_stale_wal(fs: &SimFs) -> Model {
    let db = open(fs);
    let mut model = fill(&db, 30);
    db.checkpoint().unwrap();
    put(&db, 7, b"old", &mut model);
    let wal = fs.open(&dir().join(WAL_FILE)).unwrap();
    let mut stale = vec![0; wal.size().unwrap() as usize];
    wal.read_exact_at(&mut stale, 0).unwrap();
    db.checkpoint().unwrap();
    put(&db, 7, b"new", &mut model);
    db.close().unwrap();
    wal.set_len(0).unwrap();
    wal.write_all_at(&stale, 0).unwrap();
    wal.sync_data().unwrap();
    model
}

#[test]
fn a_failure_at_any_disk_operation_of_opening_leaves_a_database_that_opens() {
    let setups: [(&str, Setup); 3] = [
        ("creating", |_| Model::new()),
        ("cutting off the tail", with_garbage_after_the_tail),
        ("starting a stale WAL again", with_a_stale_wal),
    ];
    for (name, setup) in setups {
        let ops = {
            let fs = SimFs::new();
            setup(&fs);
            let start = fs.ops();
            open(&fs);
            fs.ops() - start
        };
        assert!(ops >= 4, "{name}: opening takes {ops} operations");
        for fail in 0..ops {
            for (failed_sync, crash) in sweeps() {
                let what =
                    format!("{name}, failing operation {fail}, crash {crash:?}, {failed_sync:?}");
                let fs = disk_whose_failed_syncs(failed_sync);
                let model = setup(&fs);
                fs.fail_once_after(fail);

                let result = Db::open_with(fs.clone(), dir());

                // Nothing was acknowledged yet: opening reports the failure
                // and opening again reads everything afresh.
                assert!(matches!(result, Err(Error::Io(_))), "{what}: {result:?}");
                recovers(&fs, crash, &model, &model, &what);
            }
        }
    }
}

/// EIO, the error a disk gives for a failed write or sync, and the one the
/// simulated disk fails with: 5 on Linux and macOS alike.
const EIO: i32 = 5;

#[test]
fn every_request_after_a_failed_commit_reports_the_original_error() {
    // Review S6 and P5 of PR #57: the cause keeps its kind and OS error
    // code (ENOSPC, EIO), so a caller can tell a full disk from a broken
    // one, and the source chain reaches it.
    let fs = SimFs::new();
    let db = open(&fs);
    let model = fill(&db, 5);
    let (txn, _) = writes(&db, &[1], b"new", &model);
    fs.fail_once_after(1);

    let failed = txn.commit().unwrap_err();
    let stopped = db.begin().get(b"k").unwrap_err();

    let Error::OutcomeUnknown(io) = &failed else {
        panic!("expected OutcomeUnknown, got {failed:?}");
    };
    assert_eq!(io.raw_os_error(), Some(EIO));
    let Error::Stopped(cause) = &stopped else {
        panic!("expected Stopped, got {stopped:?}");
    };
    match &**cause {
        Error::OutcomeUnknown(kept) => {
            assert_eq!(kept.raw_os_error(), Some(EIO));
            assert_eq!(kept.kind(), io.kind());
        }
        other => panic!("the cause is {other:?}"),
    }
    let root = std::error::Error::source(&stopped)
        .and_then(std::error::Error::source)
        .expect("Stopped → OutcomeUnknown → io::Error");
    let root = root.downcast_ref::<std::io::Error>().expect("an io::Error");
    assert_eq!(root.raw_os_error(), Some(EIO));
}

#[test]
fn every_request_after_a_failed_checkpoint_reports_the_original_error() {
    let fs = SimFs::new();
    let db = open(&fs);
    fill(&db, 5);
    fs.fail_once_after(0);

    let failed = db.checkpoint().unwrap_err();
    let stopped = db.begin().get(b"k").unwrap_err();

    let Error::Io(io) = &failed else {
        panic!("expected Io, got {failed:?}");
    };
    assert_eq!(io.raw_os_error(), Some(EIO));
    let Error::Stopped(cause) = &stopped else {
        panic!("expected Stopped, got {stopped:?}");
    };
    match &**cause {
        Error::Io(kept) => assert_eq!(kept.raw_os_error(), Some(EIO)),
        other => panic!("the cause is {other:?}"),
    }
}

/// A database whose last commit failed at its WAL sync on a disk that
/// forgets, dropped without a crash, so the page cache still holds the
/// commit the disk never got. The commit changes pages no commit since the
/// last checkpoint changed, so the WAL on the disk holds no image of them.
/// Returns what it held before that commit and what it holds with it.
fn with_a_forgotten_commit(fs: &SimFs) -> (Model, Model) {
    fs.set_failed_sync(FailedSync::Forgets);
    let db = open(fs);
    let mut model = history(&db, 2);
    db.checkpoint().unwrap();
    put(&db, 119, &[7; 30], &mut model);
    let (txn, landed) = writes(&db, &[1, 2, 3, 3000], &[9; 5000], &model);
    fs.fail_once_after(1);
    txn.commit().unwrap_err();
    (model, landed)
}

/// A database whose checkpoint failed at operation `fail` on a disk that
/// forgets, dropped without a crash. Returns what it holds, twice: no
/// commit's outcome is unknown.
fn with_a_forgotten_checkpoint(fs: &SimFs, fail: u64) -> (Model, Model) {
    fs.set_failed_sync(FailedSync::Forgets);
    let db = open(fs);
    let model = history(&db, 2);
    fs.fail_once_after(fail);
    db.checkpoint().unwrap_err();
    (model.clone(), model)
}

/// Both ways a sector a torn write missed can come out of a crash: as it
/// was, or garbled (design 0014).
const TORN_SECTORS: [TornSector; 2] = [TornSector::KeepsOld, TornSector::Garbled];

/// Cuts the power at every operation of opening the database `setup`
/// leaves, then opens it after the crash, and checks that every
/// acknowledged commit is there: those before `setup`'s failure, and one
/// made after opening if opening finished. On a disk that keeps the old
/// bytes of a torn sector and on one that garbles them: opening must not
/// write over the only copy of what it relies on.
fn power_cuts_while_reopening(name: &str, setup: &dyn Fn(&SimFs) -> (Model, Model)) {
    let ops = {
        let fs = SimFs::new();
        setup(&fs);
        let start = fs.ops();
        open(&fs);
        fs.ops() - start
    };
    assert!(ops >= 4, "{name}: opening takes {ops} operations");
    for torn_sector in TORN_SECTORS {
        for cut in 0..=ops {
            for seed in 0..8 {
                let what = format!(
                    "{name}, {torn_sector:?}, power cut at operation {cut} of opening, seed {seed}"
                );
                let fs = SimFs::new();
                fs.set_torn_sector(torn_sector);
                let (model, landed) = setup(&fs);
                fs.fail_after(cut);

                // What the database may hold after the crash.
                let mut allowed = vec![model, landed.clone()];
                if let Ok(db) = Db::open_with(fs.clone(), dir()) {
                    // Opening finished: what it read is on the disk, and a
                    // commit now is acknowledged or its outcome is unknown.
                    assert_eq!(read_all(&db), landed, "{what}");
                    let mut later = landed.clone();
                    later.insert(4242, b"later".to_vec());
                    let mut txn = db.begin();
                    txn.put(&4242u32.to_be_bytes(), b"later").unwrap();
                    allowed = match txn.commit() {
                        Ok(()) => vec![later],
                        Err(_) => vec![landed, later],
                    };
                }
                fs.crash(seed);
                let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{what}: {e}"));
                assert!(
                    allowed.contains(&read_all(&db)),
                    "{what}: lost an acknowledged commit or half of one"
                );
            }
        }
    }
}

/// A database with commits in its WAL since its last checkpoint, cut off
/// by a crash, so everything is on the disk. Returns what it holds, twice.
fn after_a_crash(fs: &SimFs) -> (Model, Model) {
    let db = open(fs);
    let model = history(&db, 2);
    drop(db);
    fs.crash(0);
    (model.clone(), model)
}

#[test]
fn a_power_cut_while_reopening_after_a_crash_keeps_every_commit() {
    // Design 0014: opening writes nothing over the only copy of a commit
    // or a meta slot, so a disk that garbles what it was overwriting when
    // the power went loses nothing.
    power_cuts_while_reopening("after a crash", &after_a_crash);
}

/// A database that was checkpointed and closed. Returns what it holds,
/// twice.
fn after_closing(fs: &SimFs) -> (Model, Model) {
    let db = open(fs);
    let model = history(&db, 2);
    db.close().unwrap();
    fs.crash(0);
    (model.clone(), model)
}

#[test]
fn a_power_cut_while_reopening_after_closing_keeps_every_commit() {
    power_cuts_while_reopening("after closing", &after_closing);
}

#[test]
fn a_power_cut_while_reopening_after_a_forgotten_commit_keeps_every_acknowledged_commit() {
    // Issue #59: opening writes again what it read before it appends
    // anything. A cut part way through leaves the disk as the failed
    // commit left it, or with what was read made durable.
    power_cuts_while_reopening("a forgotten commit", &with_a_forgotten_commit);
}

#[test]
fn a_power_cut_while_reopening_after_a_forgotten_checkpoint_keeps_every_commit() {
    let ops = {
        let fs = SimFs::new();
        let db = open(&fs);
        history(&db, 2);
        let start = fs.ops();
        db.checkpoint().unwrap();
        fs.ops() - start
    };
    for fail in 0..ops {
        power_cuts_while_reopening(
            &format!("checkpoint failing at operation {fail}"),
            &|fs: &SimFs| with_a_forgotten_checkpoint(fs, fail),
        );
    }
}

#[test]
fn what_opening_again_finds_stays_through_a_crash() {
    // PR #60, mutant K7: an opening cut short after writing the new WAL
    // file, holding a forgotten commit, then an opening that does not see
    // that commit. Whatever the second opening finds, failed commit or
    // not, a crash after it must not change. A new WAL file left longer
    // than what that opening wrote would bring the commit back.
    let ops = {
        let fs = SimFs::new();
        with_a_forgotten_commit(&fs);
        let start = fs.ops();
        open(&fs);
        fs.ops() - start
    };
    for torn_sector in TORN_SECTORS {
        for cut in 0..=ops {
            for seed in 0..8 {
                let what =
                    format!("{torn_sector:?}, cut at operation {cut} of opening, seed {seed}");
                let fs = SimFs::new();
                fs.set_torn_sector(torn_sector);
                with_a_forgotten_commit(&fs);
                fs.fail_after(cut);
                drop(Db::open_with(fs.clone(), dir()));
                fs.crash(seed);

                let found = read_all(
                    &Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{what}: {e}")),
                );
                fs.crash(seed + 100);
                let db = Db::open_with(fs.clone(), dir())
                    .unwrap_or_else(|e| panic!("{what}, then: {e}"));
                assert_eq!(
                    read_all(&db),
                    found,
                    "{what}: what opening found changed in a crash"
                );
            }
        }
    }
}
