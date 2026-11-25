//! The history checker finds the bugs planted in the engine, each by the
//! anomaly it should make (issue #29, design 0024 V7). Built only with
//! `--features planted-bugs`. The bugs that show only when threads meet at
//! the wrong moment are run on several seeds, and how many found them is
//! printed for docs/experiments/0016-history-checker.md;
//! `OAKDB_HISTORY_SEEDS=N` runs seeds 1 to N.

mod workload;

use std::time::{Duration, Instant};

use oakdb::fs::sim::{FailedSync, SimFs};
use oakdb::{Db, PlantedBug};
use oakdb_history::{Elem, History, Kind, Outcome, Report, Txn, TxnId};
use workload::{append, checked, last_reads, now, outcome, read, run, seeds, Plan};

const SYNC_DELAY: Duration = Duration::from_micros(200);

fn run_with(bug: PlantedBug, threads: u32, seed: u64) -> Report {
    let fs = SimFs::new();
    fs.set_sync_delay(SYNC_DELAY);
    let db = Db::open_with_bug(fs, "/oak", bug).unwrap();
    let history = run(&db, &Plan::new(threads, seed));
    checked(&format!("{bug:?}-threads-{threads}-seed-{seed}"), &history)
}

/// Runs `bug` on 8 threads for each of `seeds` and returns on how many
/// the checker named one of `expected`. Every anomaly found must be one
/// `bug` can make: expected, or `allowed`. A G1b read is a G-single cycle
/// too, so G-single is allowed wherever G1b is.
fn detections(bug: PlantedBug, expected: &[Kind], allowed: &[Kind], seeds: &[u64]) -> usize {
    let mut allowed = allowed.to_vec();
    if expected.contains(&Kind::IntermediateRead) || allowed.contains(&Kind::IntermediateRead) {
        allowed.push(Kind::GSingle);
    }
    let mut found = 0;
    for &seed in seeds {
        let report = run_with(bug, 8, seed);
        let mut kinds: Vec<Kind> = report.anomalies.iter().map(|a| a.kind()).collect();
        kinds.sort();
        kinds.dedup();
        for k in &kinds {
            assert!(
                expected.contains(k) || allowed.contains(k),
                "{bug:?} made {k:?}, which it should not"
            );
        }
        found += usize::from(kinds.iter().any(|k| expected.contains(k)));
    }
    eprintln!(
        "{bug:?}: {expected:?} found on {found} of {} seeds",
        seeds.len()
    );
    found
}

#[test]
fn skipping_the_conflict_check_is_found_as_lost_updates() {
    let seeds = seeds(&[1, 2, 3]);
    // A lost write breaks the reads after it, which may show as more.
    let also = [Kind::GSingle, Kind::CommitCycle, Kind::StaleRead];
    let found = detections(
        PlantedBug::SkipConflictCheck,
        &[Kind::LostUpdate],
        &also,
        &seeds,
    );
    assert_eq!(found, seeds.len());
}

#[test]
fn reading_the_latest_tree_is_found_as_read_skew() {
    // Only a commit landing between two reads of one transaction makes it.
    let seeds = seeds(&(1..=10).collect::<Vec<_>>());
    let read_skew = [Kind::GSingle, Kind::NonRepeatableRead];
    let also = [Kind::LostUpdate, Kind::CommitCycle];
    let found = detections(PlantedBug::ReadLatest, &read_skew, &also, &seeds);
    assert!(found > 0, "found on no seed");
}

#[test]
fn scanning_the_latest_tree_is_found_as_read_skew() {
    // Likewise, and only one step in ten is a scan.
    let seeds = seeds(&(1..=10).collect::<Vec<_>>());
    let read_skew = [Kind::GSingle, Kind::NonRepeatableRead];
    let found = detections(
        PlantedBug::ScanLatest,
        &read_skew,
        &[Kind::CommitCycle],
        &seeds,
    );
    assert!(found > 0, "found on no seed");
}

#[test]
fn skipping_the_check_within_a_batch_is_found_as_lost_updates() {
    let seeds = seeds(&[1, 2, 3]);
    let also = [Kind::GSingle, Kind::CommitCycle, Kind::StaleRead];
    let found = detections(
        PlantedBug::SkipInBatchConflict,
        &[Kind::LostUpdate],
        &also,
        &seeds,
    );
    assert_eq!(found, seeds.len());
}

#[test]
fn recording_keys_and_swapping_the_tree_apart_is_found_as_lost_updates() {
    // Only a transaction that begins in the gap between the two makes it.
    let seeds = seeds(&(1..=10).collect::<Vec<_>>());
    let also = [Kind::GSingle, Kind::CommitCycle, Kind::StaleRead];
    let found = detections(PlantedBug::SplitPublish, &[Kind::LostUpdate], &also, &seeds);
    assert!(found > 0, "found on no seed");
}

#[test]
fn answering_before_the_batch_shows_is_found_as_stale_reads() {
    // Only a thread that begins again before the batch shows makes it.
    let seeds = seeds(&(1..=10).collect::<Vec<_>>());
    // The last commit's answer can come before it shows, and the reads at
    // the end then miss it: a write answered `Ok` that is not there.
    let also = [Kind::NonRepeatableRead, Kind::LostUpdate];
    let found = detections(
        PlantedBug::AnswerBeforePublish,
        &[Kind::StaleRead],
        &also,
        &seeds,
    );
    assert!(found > 0, "found on no seed");
}

/// One commit held in its sync while another transaction reads its key;
/// then the sync fails, and the database is opened again after a power
/// cut. Returns the history.
fn read_during_a_failing_sync(bug: Option<PlantedBug>) -> History {
    let fs = SimFs::new();
    let db = match bug {
        Some(bug) => Db::open_with_bug(fs.clone(), "/oak", bug),
        None => Db::open_with(fs.clone(), "/oak"),
    }
    .unwrap();
    let clock = Instant::now();
    let (writer, reader) = (TxnId::new(0, 0), TxnId::new(1, 0));
    let key = workload::key(0);
    let held = fs.hold_syncs();
    let mut txns = std::thread::scope(|s| {
        let committing = s.spawn(|| {
            let begin = now(clock);
            let mut txn = db.begin();
            let mut ops = Vec::new();
            append(&mut txn, &mut ops, &key, Elem::new(writer, 0)).unwrap();
            let outcome = outcome(txn.commit());
            Txn {
                id: writer,
                outcome,
                begin,
                end: now(clock),
                ops,
            }
        });
        held.wait_for_sync();
        // The sync waiting now fails, and forgets what it was to save.
        fs.set_failed_sync(FailedSync::Forgets);
        fs.fail_once_after(0);
        let begin = now(clock);
        let txn = db.begin();
        let mut ops = Vec::new();
        read(&txn, &mut ops, &key).unwrap();
        let read_outcome = outcome(txn.commit());
        let reading = Txn {
            id: reader,
            outcome: read_outcome,
            begin,
            end: now(clock),
            ops,
        };
        held.release();
        vec![committing.join().unwrap(), reading]
    });
    assert_eq!(txns[0].outcome, Outcome::Info, "the sync did not fail");
    drop(db);
    fs.crash(0);
    let db = Db::open_with(fs, "/oak").unwrap();
    txns.sort_by_key(|t| t.begin);
    History {
        txns,
        last: Some(last_reads(&db, 1)),
    }
}

#[test]
fn publishing_before_the_sync_is_found_as_an_aborted_read_once_the_sync_fails() {
    let history = read_during_a_failing_sync(Some(PlantedBug::PublishBeforeSync));
    let report = checked("PublishBeforeSync", &history);
    let kinds: Vec<Kind> = report.anomalies.iter().map(|a| a.kind()).collect();
    assert_eq!(kinds, [Kind::AbortedRead], "{history}");
}

#[test]
fn without_the_bug_a_read_during_a_failing_sync_is_clean() {
    let history = read_during_a_failing_sync(None);
    let report = checked("read-during-failing-sync", &history);
    assert!(report.anomalies.is_empty(), "{history}");
}
