//! Blind tests for disk write failure handling (issue #24, design 0012).
//!
//! Written from the issue, design 0012, design 0008, the ADRs and the public
//! API only, without reading the implementation. Each test cites the
//! sentence it checks.

use std::collections::BTreeMap;
use std::error::Error as _;
use std::io;
use std::path::{Path, PathBuf};

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error, CHECKPOINT_AFTER};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

const SEEDS: u64 = 6;

fn dir() -> PathBuf {
    PathBuf::from("/oak")
}

/// A database on a fresh simulated disk with a few acknowledged commits.
fn setup() -> (SimFs, Db<SimFs>, Model) {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).expect("open");
    let mut model = Model::new();
    for i in 0..5u32 {
        let mut txn = db.begin();
        for j in 0..4u32 {
            let k = format!("k{i}-{j}").into_bytes();
            let v = format!("v{i}-{j}").into_bytes();
            txn.put(&k, &v).unwrap();
            model.insert(k, v);
        }
        txn.commit().expect("acknowledged commit");
    }
    (fs, db, model)
}

/// Writes of the commit that will fail: new keys, an overwrite and a delete.
fn failing_writes(db: &Db<SimFs>) -> oakdb::Txn<'_, SimFs> {
    let mut txn = db.begin();
    for j in 0..20u32 {
        txn.put(format!("f{j:02}").as_bytes(), &[j as u8; 300]).unwrap();
    }
    txn.put(b"k0-0", b"overwritten").unwrap();
    txn.delete(b"k1-1").unwrap();
    txn
}

/// The model with the failing commit applied.
fn with_failing(model: &Model) -> Model {
    let mut m = model.clone();
    for j in 0..20u32 {
        m.insert(format!("f{j:02}").into_bytes(), vec![j as u8; 300]);
    }
    m.insert(b"k0-0".to_vec(), b"overwritten".to_vec());
    m.remove(b"k1-1".as_slice());
    m
}

fn contents(db: &Db<SimFs>) -> Model {
    let txn = db.begin();
    txn.scan::<&[u8]>(..)
        .map(|r| r.expect("scan item"))
        .collect()
}

fn reopen(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), dir()).expect("reopen")
}

/// Whether the error chain of `e` holds the disk failure the simulated
/// disk reports.
fn holds_disk_failure(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            if io.to_string() == "the disk failed" {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

/// Asserts `r` is `Stopped` holding the original disk failure; returns
/// the cause's text.
#[track_caller]
fn assert_stopped<T: std::fmt::Debug>(r: oakdb::Result<T>) -> String {
    match r {
        Err(Error::Stopped(cause)) => {
            assert!(
                holds_disk_failure(&*cause),
                "Stopped does not hold the original failure: {cause:?}"
            );
            cause.to_string()
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[track_caller]
fn assert_outcome_unknown<T: std::fmt::Debug>(r: oakdb::Result<T>) {
    match r {
        Err(Error::OutcomeUnknown(e)) => assert_eq!(e.to_string(), "the disk failed"),
        other => panic!("expected OutcomeUnknown, got {other:?}"),
    }
}

#[track_caller]
fn assert_all_or_nothing(got: &Model, before: &Model, after: &Model) {
    assert!(
        got == before || got == after,
        "failed commit is partly visible: got {} keys, before {}, after {}",
        got.len(),
        before.len(),
        after.len()
    );
}

/// How many disk-changing operations `f` does on a fresh copy of `setup`.
fn ops_of(f: impl FnOnce(&SimFs, Db<SimFs>)) -> u64 {
    let (fs, db, _) = setup();
    let before = fs.ops();
    f(&fs, db);
    fs.ops() - before
}

fn commit_ops() -> u64 {
    ops_of(|_, db| failing_writes(&db).commit().unwrap())
}

// ---------------------------------------------------------------------
// SimFs::fail_once_after
// ---------------------------------------------------------------------

// Design 0012 §6: "실패한 호출은 아무것도 바꾸지 않고 실패", "실패한 호출은
// `ops()`에 세지 않는다", "전원은 켜진 채라 다음 호출부터 다시 된다".
#[test]
fn fail_once_after_fails_one_write_that_changes_nothing() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/d")).unwrap();
    let f = fs.open(Path::new("/d/x")).unwrap();
    f.write_all_at(b"aaaa", 0).unwrap();
    let ops = fs.ops();
    fs.fail_once_after(1);
    f.write_all_at(b"bb", 0).unwrap(); // one succeeds
    assert!(f.write_all_at(b"cc", 2).is_err()); // the second fails
    assert_eq!(fs.ops(), ops + 1, "failed call is not counted");
    let mut buf = [0u8; 4];
    f.read_exact_at(&mut buf, 0).unwrap(); // power still on: reads work
    assert_eq!(&buf, b"bbaa", "failed write changed nothing");
    f.write_all_at(b"dd", 2).unwrap(); // later calls work
    f.sync_data().unwrap();
    assert_eq!(fs.ops(), ops + 3);
    f.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"bbdd");
}

// Design 0012 §6: "`sync_data` 아무것도 굳히지 않고 실패. 기다리던 변경은
// 계속 기다린다: 정전이 나면 주사위로 정해지고, 나중에 sync가 성공하면 굳는다".
#[test]
fn fail_once_after_failed_sync_leaves_changes_waiting() {
    // Some seed loses the write after a failed sync.
    let mut lost_once = false;
    for seed in 0..40 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let f = fs.open(Path::new("/d/x")).unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        f.write_all_at(b"old!", 0).unwrap();
        f.sync_data().unwrap();
        f.write_all_at(b"new!", 0).unwrap();
        fs.fail_once_after(0);
        assert!(f.sync_data().is_err());
        drop(f);
        fs.crash(seed);
        let f = fs.open(Path::new("/d/x")).unwrap();
        let mut buf = [0u8; 4];
        f.read_exact_at(&mut buf, 0).unwrap();
        if &buf == b"old!" {
            lost_once = true;
        }
    }
    assert!(lost_once, "a failed sync made the write durable for every seed");

    // A later sync that succeeds saves it.
    for seed in 0..20 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let f = fs.open(Path::new("/d/x")).unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        f.write_all_at(b"new!", 0).unwrap();
        fs.fail_once_after(0);
        assert!(f.sync_data().is_err());
        f.sync_data().unwrap();
        drop(f);
        fs.crash(seed);
        let f = fs.open(Path::new("/d/x")).unwrap();
        let mut buf = [0u8; 4];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"new!", "seed {seed}");
    }
}

// Design 0012 §6: "`open`, `sync_dir`, `create_dir_all` 아무것도 바꾸지 않고
// 실패" and only once ("**한 번만**").
#[test]
fn fail_once_after_fails_directory_calls_once() {
    let fs = SimFs::new();
    fs.fail_once_after(0);
    assert!(fs.create_dir_all(Path::new("/d")).is_err());
    assert!(!fs.exists(Path::new("/d")), "failed create_dir_all made nothing");
    fs.create_dir_all(Path::new("/d")).unwrap();
    fs.fail_once_after(0);
    assert!(fs.open(Path::new("/d/x")).is_err());
    assert!(!fs.exists(Path::new("/d/x")), "failed open made no file");
    let _f = fs.open(Path::new("/d/x")).unwrap();
    fs.fail_once_after(0);
    assert!(fs.sync_dir(Path::new("/d")).is_err());
    fs.sync_dir(Path::new("/d")).unwrap();
}

// ---------------------------------------------------------------------
// The failing commit
// ---------------------------------------------------------------------

// Issue #24: "실패한 배치의 커밋은 '결과를 알 수 없음' 오류를 받는다";
// design 0012 §1: "커밋 | WAL 쓰기·sync | 정지, 그 커밋은 `OutcomeUnknown`".
// Sync is the last disk-changing operation of a commit that does not
// checkpoint ("A commit appends ... and syncs it once before it answers").
#[test]
fn a_failed_wal_sync_gives_outcome_unknown() {
    let (fs, db, _) = setup();
    let n = commit_ops();
    assert!(n >= 1);
    fs.fail_once_after(n - 1);
    assert_outcome_unknown(failing_writes(&db).commit());
}

// Design 0012 §1: "쓰기 실패 | 같다 ... sync 실패와 구별하지 않는다".
#[test]
fn a_failed_wal_write_gives_outcome_unknown() {
    let (fs, db, _) = setup();
    fs.fail_once_after(0);
    assert_outcome_unknown(failing_writes(&db).commit());
}

// Error docs: "`source()`: 실패한 호출의 `source()`는 `io::Error`".
#[test]
fn outcome_unknown_source_is_the_io_error() {
    let (fs, db, _) = setup();
    fs.fail_once_after(0);
    let e = failing_writes(&db).commit().unwrap_err();
    let src = e.source().expect("source");
    let io = src.downcast_ref::<io::Error>().expect("io::Error source");
    assert_eq!(io.to_string(), "the disk failed");
}

// Design 0012 §3: every request after the stop gets `Stopped`; "`Stopped`는
// 처음 원인을 담음", "`Stopped`의 `source()`는 원인 `Error`".
#[test]
fn every_later_request_is_stopped_and_holds_the_cause() {
    let (fs, db, _) = setup();
    let mut early = db.begin(); // begun before the stop
    early.put(b"early", b"x").unwrap();
    fs.fail_once_after(0);
    assert_outcome_unknown(failing_writes(&db).commit());

    let mut causes = Vec::new();
    let mut txn = db.begin();
    causes.push(assert_stopped(txn.get(b"k0-0")));
    causes.push(assert_stopped(txn.put(b"a", b"b")));
    causes.push(assert_stopped(txn.delete(b"k0-1")));
    let mut scan = txn.scan::<&[u8]>(..);
    causes.push(assert_stopped(scan.next().expect("one item")));
    assert!(scan.next().is_none(), "scan ends after Stopped");
    drop(scan);
    causes.push(assert_stopped(txn.commit()));
    causes.push(assert_stopped(db.checkpoint()));

    causes.push(assert_stopped(early.get(b"early")));
    causes.push(assert_stopped(early.put(b"z", b"z")));
    causes.push(assert_stopped(early.commit()));

    let e = db.checkpoint().unwrap_err();
    let src = e.source().expect("Stopped has a source");
    assert!(src.downcast_ref::<Error>().is_some(), "source is the cause Error");

    causes.push(assert_stopped(db.close()));
    assert!(causes.windows(2).all(|w| w[0] == w[1]), "one cause: {causes:?}");
}

// Design 0012 §3: "`Txn::commit` | `Err(Stopped)`. 쓴 것이 없는 커밋도".
#[test]
fn an_empty_commit_after_the_stop_is_stopped() {
    let (fs, db, _) = setup();
    fs.fail_once_after(0);
    assert_outcome_unknown(failing_writes(&db).commit());
    assert_stopped(db.begin().commit());
}

// Design 0012 §3: "`Db::begin`, `page_count`, `Txn::rollback` | 그대로";
// §4: "메모리 트리 | 바꿔 끼우지 않는다" (page_count describes the committed
// tree, so it does not grow by the failed commit).
#[test]
fn begin_page_count_and_rollback_still_work_after_the_stop() {
    let (fs, db, _) = setup();
    let pages = db.page_count();
    let mut big = db.begin();
    for j in 0..200u32 {
        big.put(format!("big{j:04}").as_bytes(), &[7u8; 500]).unwrap();
    }
    fs.fail_once_after(0);
    assert_outcome_unknown(big.commit());
    assert_eq!(db.page_count(), pages, "failed commit not swapped in");
    let txn = db.begin();
    txn.rollback();
    let mut txn = db.begin();
    let _ = txn.put(b"x", b"y");
    txn.rollback();
    assert_eq!(db.page_count(), pages);
}

// ADR 0007 / design 0012: "fsync가 한 번 실패한 파일은 이 프로세스에서 다시
// 믿지 않는다"; error docs: Stopped "this request did nothing".
#[test]
fn a_stopped_engine_never_touches_the_disk_again() {
    let (fs, db, _) = setup();
    fs.fail_once_after(commit_ops() - 1);
    assert_outcome_unknown(failing_writes(&db).commit());
    let ops = fs.ops();
    for _ in 0..3 {
        let mut txn = db.begin();
        let _ = txn.put(b"retry", b"retry");
        assert_stopped(txn.commit());
        assert_stopped(db.checkpoint());
    }
    assert_stopped(db.close());
    assert_eq!(fs.ops(), ops, "a stopped engine wrote or synced");
}

// Design 0012 §3: "멈추기 **전에** 시작한 `Scan` | 끝까지 돈다. 시작할 때
// 잡은 트리(응답한 커밋들)만 읽는다".
#[test]
fn a_scan_started_before_the_stop_runs_to_the_end() {
    let (fs, db, model) = setup();
    let reader = db.begin();
    let mut scan = reader.scan::<&[u8]>(..);
    let first = scan.next().unwrap().unwrap();
    fs.fail_once_after(0);
    assert_outcome_unknown(failing_writes(&db).commit());
    let mut got: Model = std::iter::once(first)
        .chain(scan.map(|r| r.expect("scan started before the stop")))
        .collect();
    assert_eq!(got.len(), model.len());
    assert_eq!(std::mem::take(&mut got), model, "nothing of the failed commit");
}

// Design 0012 §10: "멈춘 Db는 잠금을 쥠"; §7: "다시 열기는 `Db`를 버리고
// (잠금이 풀린다) `Db::open_with`를 다시 부르는 것".
#[test]
fn a_stopped_db_keeps_its_lock_until_dropped() {
    let (fs, db, model) = setup();
    fs.fail_once_after(0);
    assert_outcome_unknown(failing_writes(&db).commit());
    assert!(matches!(
        Db::open_with(fs.clone(), dir()),
        Err(Error::AlreadyOpen)
    ));
    drop(db);
    let db = reopen(&fs);
    assert_eq!(contents(&db), model);
}

// ---------------------------------------------------------------------
// Recovery after a failed commit
// ---------------------------------------------------------------------

// Issue #24: "재시작하면 WAL로 정상 복구된다"; design 0012 §4: "다시 연 뒤 |
// 통째로 있거나 통째로 없다". Sweep: every disk-changing operation of the
// commit (§10 "커밋·정리·열기의 모든 디스크 연산마다").
#[test]
fn every_operation_of_a_commit_fails_and_reopen_recovers_without_power_cut() {
    let n = commit_ops();
    for k in 0..n {
        let (fs, db, model) = setup();
        fs.fail_once_after(k);
        assert_outcome_unknown(failing_writes(&db).commit());
        assert_stopped(db.begin().get(b"k0-0"));
        drop(db);
        let db = reopen(&fs);
        assert_all_or_nothing(&contents(&db), &model, &with_failing(&model));
    }
}

// Same, with a power cut after the failure (design 0012 §10 "정전 시드 여러 개").
#[test]
fn every_operation_of_a_commit_fails_and_reopen_recovers_after_power_cut() {
    let n = commit_ops();
    for k in 0..n {
        for seed in 0..SEEDS {
            let (fs, db, model) = setup();
            fs.fail_once_after(k);
            assert_outcome_unknown(failing_writes(&db).commit());
            drop(db);
            fs.crash(seed);
            let db = reopen(&fs);
            assert_all_or_nothing(&contents(&db), &model, &with_failing(&model));
        }
    }
}

// Design 0012 §7: "Db를 버리고 다시 열기 ... 응답한 커밋은 모두 있음";
// the reopened engine works normally and its commits are durable.
#[test]
fn the_reopened_database_takes_new_commits_that_survive_a_power_cut() {
    for k in [0, commit_ops() - 1] {
        for seed in 0..SEEDS {
            let (fs, db, model) = setup();
            fs.fail_once_after(k);
            assert_outcome_unknown(failing_writes(&db).commit());
            drop(db);
            let db = reopen(&fs);
            let base = contents(&db);
            assert_all_or_nothing(&base, &model, &with_failing(&model));
            let mut txn = db.begin();
            txn.put(b"after", b"reopen").unwrap();
            txn.commit().unwrap();
            drop(db);
            fs.crash(seed);
            let db = reopen(&fs);
            let mut want = base.clone();
            want.insert(b"after".to_vec(), b"reopen".to_vec());
            assert_eq!(contents(&db), want, "k {k} seed {seed}");
        }
    }
}

// ---------------------------------------------------------------------
// Explicit checkpoint and close
// ---------------------------------------------------------------------

fn checkpoint_ops() -> u64 {
    ops_of(|_, db| db.checkpoint().unwrap())
}

// Design 0012 §2: "직접 부른 `checkpoint()`·`close()`가 실패하면 원인 `Io`를
// 그대로 돌려준다"; Db::checkpoint docs: "every request after it fails with
// Stopped ... Every acknowledged commit is still there then".
#[test]
fn every_operation_of_an_explicit_checkpoint_fails_with_io_then_stops() {
    let n = checkpoint_ops();
    assert!(n >= 1, "checkpoint with commits in the WAL touches the disk");
    for k in 0..n {
        let (fs, db, model) = setup();
        fs.fail_once_after(k);
        match db.checkpoint() {
            Err(Error::Io(e)) => assert_eq!(e.to_string(), "the disk failed", "k {k}"),
            other => panic!("k {k}: expected Io, got {other:?}"),
        }
        assert_stopped(db.begin().get(b"k0-0"));
        assert_stopped(db.checkpoint());
        let mut txn = db.begin();
        txn.put(b"x", b"y").unwrap_err();
        assert_stopped(db.begin().commit());
        drop(db);
        assert_eq!(contents(&reopen(&fs)), model, "k {k}");
        for seed in 0..SEEDS {
            let fs2 = {
                let (fs2, db2, _) = setup();
                fs2.fail_once_after(k);
                db2.checkpoint().unwrap_err();
                drop(db2);
                fs2
            };
            fs2.crash(seed);
            assert_eq!(contents(&reopen(&fs2)), model, "k {k} seed {seed}");
        }
    }
}

// Db::checkpoint docs: "Does nothing if the WAL holds no commit".
#[test]
fn a_checkpoint_with_an_empty_wal_touches_no_disk() {
    let (fs, db, _) = setup();
    db.checkpoint().unwrap();
    let ops = fs.ops();
    db.checkpoint().unwrap();
    assert_eq!(fs.ops(), ops);
}

fn close_ops() -> u64 {
    ops_of(|_, db| db.close().unwrap())
}

// Design 0012 §1: "직접 부른 정리·닫기 | 정지, `Io` 그대로 ... 결과를 모르는
// 커밋이 없다".
#[test]
fn every_operation_of_close_fails_with_io_and_reopen_has_everything() {
    let n = close_ops();
    assert!(n >= 1);
    for k in 0..n {
        for seed in 0..=SEEDS {
            let (fs, db, model) = setup();
            fs.fail_once_after(k);
            match db.close() {
                Err(Error::Io(e)) => assert_eq!(e.to_string(), "the disk failed"),
                other => panic!("k {k}: expected Io, got {other:?}"),
            }
            if seed < SEEDS {
                fs.crash(seed);
            }
            assert_eq!(contents(&reopen(&fs)), model, "k {k} seed {seed}");
        }
    }
}

// ---------------------------------------------------------------------
// Auto-checkpoint after a commit
// ---------------------------------------------------------------------

/// A database whose WAL is just under CHECKPOINT_AFTER, so one more small
/// commit checkpoints.
fn near_checkpoint() -> (SimFs, Db<SimFs>, Model) {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    db.checkpoint().unwrap();
    let mut model = Model::new();
    let chunk = 64 * 1024;
    let mut i = 0u32;
    // About 4 KiB of WAL per 4 KiB of value; stop well before the limit.
    while (i as u64 + 2) * chunk as u64 + 64 * 1024 < CHECKPOINT_AFTER {
        let mut txn = db.begin();
        let k = format!("big{i:03}").into_bytes();
        let v = vec![(i % 251) as u8; chunk];
        txn.put(&k, &v).unwrap();
        txn.commit().unwrap();
        model.insert(k, v);
        i += 1;
    }
    (fs, db, model)
}

/// Commits until one checkpoints (detected by the disk-op count jumping),
/// returning how many commits it took before the checkpointing one.
fn commits_until_checkpoint() -> (usize, u64, u64) {
    let (fs, db, _) = near_checkpoint();
    let mut normal = None;
    for c in 0..200usize {
        let before = fs.ops();
        let mut txn = db.begin();
        txn.put(format!("t{c:03}").as_bytes(), &vec![1u8; 32 * 1024]).unwrap();
        txn.commit().unwrap();
        let used = fs.ops() - before;
        match normal {
            None => normal = Some(used),
            Some(n) if used > n + 2 => return (c, n, used),
            _ => {}
        }
    }
    panic!("no auto-checkpoint seen");
}

// Design 0012 §1/decision 4: "커밋이 WAL에 닿은 뒤 자동 정리가 실패하면 |
// 커밋은 이미 디스크에 있으니 `Ok`를 돌려주고 엔진만 멈춘다". Txn::commit
// docs: "if that fails, the commit is durable, so it returns Ok, and the
// engine stops". Sweep every operation of the checkpointing commit: the
// WAL part gives OutcomeUnknown, the checkpoint part Ok, both stop.
#[test]
fn every_operation_of_a_checkpointing_commit_fails() {
    let (c, normal, total) = commits_until_checkpoint();
    let mut saw_ok = false;
    let mut saw_unknown = false;
    let mut last_was_ok = false;
    for k in 0..total {
        let (fs, db, mut model) = near_checkpoint();
        for i in 0..c {
            let mut txn = db.begin();
            let key = format!("t{i:03}").into_bytes();
            txn.put(&key, &vec![1u8; 32 * 1024]).unwrap();
            txn.commit().unwrap();
            model.insert(key, vec![1u8; 32 * 1024]);
        }
        let key = format!("t{c:03}").into_bytes();
        let mut txn = db.begin();
        txn.put(&key, &vec![1u8; 32 * 1024]).unwrap();
        fs.fail_once_after(k);
        let mut after = model.clone();
        after.insert(key.clone(), vec![1u8; 32 * 1024]);
        let r = txn.commit();
        let acked = match r {
            Ok(()) => {
                saw_ok = true;
                last_was_ok = true;
                true
            }
            Err(Error::OutcomeUnknown(_)) => {
                assert!(!last_was_ok, "k {k}: WAL failure after a checkpoint failure");
                assert!(k < normal, "k {k}: OutcomeUnknown past the WAL operations");
                saw_unknown = true;
                false
            }
            other => panic!("k {k}: {other:?}"),
        };
        assert_stopped(db.begin().get(&key));
        drop(db);
        let seed = k % 3;
        let fs2 = fs.clone();
        if k % 2 == 1 {
            fs2.crash(seed);
        }
        let got = contents(&reopen(&fs2));
        if acked {
            assert_eq!(got, after, "k {k}: acknowledged commit lost");
        } else {
            assert_all_or_nothing(&got, &model, &after);
        }
    }
    assert!(saw_ok && saw_unknown);
}

// ---------------------------------------------------------------------
// Failures at open
// ---------------------------------------------------------------------

// Design 0012 §1: "열기 | ... 새 DB의 첫 커밋, 디렉터리 sync | `Io` 등 오류를
// 돌려줌. 멈출 `Db`가 없다"; "다시 열면 디스크를 처음부터 다시 읽는다".
#[test]
fn every_operation_of_creating_a_database_fails_and_open_again_works() {
    let n = {
        let fs = SimFs::new();
        let _db = Db::open_with(fs.clone(), dir()).unwrap();
        fs.ops()
    };
    for k in 0..n {
        for seed in 0..=SEEDS {
            let fs = SimFs::new();
            fs.fail_once_after(k);
            match Db::open_with(fs.clone(), dir()) {
                Err(Error::Io(e)) => assert_eq!(e.to_string(), "the disk failed", "k {k}"),
                other => panic!("k {k}: expected Io, got {other:?}"),
            }
            if seed < SEEDS {
                fs.crash(seed);
            }
            let db = reopen(&fs);
            assert!(contents(&db).is_empty(), "k {k} seed {seed}");
            let mut txn = db.begin();
            txn.put(b"a", b"1").unwrap();
            txn.commit().unwrap();
            drop(db);
            fs.crash(seed);
            assert_eq!(contents(&reopen(&fs)).len(), 1, "k {k} seed {seed}");
        }
    }
}

// Design 0012 §1: "열기 | 잠금, 읽기, 꼬리 자르기, `.cut` 파일, WAL 새로 쓰기
// ... | `Io` 등 오류를 돌려줌"; acknowledged commits are all there after.
#[test]
fn every_operation_of_reopening_fails_and_open_again_has_everything() {
    let prepare = || {
        let (fs, db, model) = setup();
        drop(db);
        (fs, model)
    };
    let n = {
        let (fs, _) = prepare();
        let before = fs.ops();
        let _db = reopen(&fs);
        fs.ops() - before
    };
    assert!(n >= 1);
    for k in 0..n {
        let (fs, model) = prepare();
        fs.fail_once_after(k);
        match Db::open_with(fs.clone(), dir()) {
            Err(Error::Io(_)) => {}
            other => panic!("k {k}: expected Io, got {other:?}"),
        }
        assert_eq!(contents(&reopen(&fs)), model, "k {k}");
    }
}

// Same, for an open that has to cut a torn tail left by a power cut in
// the middle of a commit (design 0012 §1: "꼬리 자르기, `.cut` 파일").
#[test]
fn every_operation_of_an_open_that_cuts_a_torn_tail_fails() {
    let n_commit = commit_ops();
    for seed in 0..SEEDS {
        let prepare = || {
            let (fs, db, model) = setup();
            fs.fail_after(n_commit / 2);
            let _ = failing_writes(&db).commit();
            drop(db);
            fs.crash(seed);
            (fs, model)
        };
        let n = {
            let (fs, _) = prepare();
            let before = fs.ops();
            let _db = reopen(&fs);
            fs.ops() - before
        };
        for k in 0..n {
            let (fs, model) = prepare();
            fs.fail_once_after(k);
            assert!(
                Db::open_with(fs.clone(), dir()).is_err(),
                "seed {seed} k {k}: open ignored a failure"
            );
            for crash in [false, true] {
                if crash {
                    fs.crash(seed + 100);
                }
                let got = contents(&reopen(&fs));
                assert_all_or_nothing(&got, &model, &with_failing(&model));
            }
        }
    }
}

// ---------------------------------------------------------------------
// Several commits and the first-failure rule
// ---------------------------------------------------------------------

// Design 0012 §7: "응답한 커밋은 sync가 **성공한** 뒤에 응답했으니 디스크에
// 있다"; sweep the failure across a run of commits.
#[test]
fn acknowledged_commits_survive_a_failure_anywhere_in_a_run() {
    let run_ops = {
        let (fs, db, _) = setup();
        let before = fs.ops();
        for i in 0..6u32 {
            let mut t = db.begin();
            t.put(format!("r{i}").as_bytes(), b"v").unwrap();
            t.commit().unwrap();
        }
        fs.ops() - before
    };
    for k in 0..run_ops {
        for seed in 0..3 {
            let (fs, db, mut model) = setup();
            fs.fail_once_after(k);
            let mut failed = None;
            for i in 0..6u32 {
                let key = format!("r{i}").into_bytes();
                let mut t = db.begin();
                let r = t.put(&key, b"v").and_then(|()| t.commit());
                match (failed.is_some(), r) {
                    (false, Ok(())) => {
                        model.insert(key, b"v".to_vec());
                    }
                    (false, Err(e)) => {
                        assert!(matches!(e, Error::OutcomeUnknown(_)), "k {k}: {e:?}");
                        failed = Some(key);
                    }
                    (true, r) => {
                        assert_stopped(r);
                    }
                }
            }
            assert!(failed.is_some(), "k {k}: the failure never came");
            drop(db);
            fs.crash(seed);
            let got = contents(&reopen(&fs));
            let mut after = model.clone();
            after.insert(failed.unwrap(), b"v".to_vec());
            assert_all_or_nothing(&got, &model, &after);
        }
    }
}

// Display of the new kinds names what happened (error docs).
#[test]
fn the_new_errors_display_their_cause() {
    let (fs, db, _) = setup();
    fs.fail_once_after(0);
    let e = failing_writes(&db).commit().unwrap_err();
    assert!(e.to_string().contains("the disk failed"), "{e}");
    let s = db.begin().get(b"a").unwrap_err();
    assert!(s.to_string().contains("the disk failed"), "{s}");
}
