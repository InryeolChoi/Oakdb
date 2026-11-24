//! Random transactions on 4 and 8 threads, checked for snapshot isolation
//! anomalies by `oakdb-history` (issue #29, design 0024): the engine shows
//! none, and makes write skew, which is allowed. `OAKDB_HISTORY_SEEDS=N`
//! runs seeds 1 to N instead of the few fixed ones.

mod workload;

use std::time::Duration;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use workload::{checked, run, seeds, Plan};

/// Long enough that commits wait behind a sync and go as one batch
/// (design 0024 Q3).
const SYNC_DELAY: Duration = Duration::from_micros(200);

fn no_anomalies_on(threads: u32) {
    let mut write_skews = 0;
    let mut largest_batch = 0;
    for seed in seeds(&[1, 2, 3]) {
        let fs = SimFs::new();
        fs.set_sync_delay(SYNC_DELAY);
        let db = Db::open_with(fs, "/oak").unwrap();
        let history = run(&db, &Plan::new(threads, seed));
        let report = checked(&format!("threads-{threads}-seed-{seed}"), &history);
        assert!(
            report.anomalies.is_empty(),
            "seed {seed}: {}",
            report.anomalies[0]
        );
        write_skews += report.write_skews;
        let stats = db.stats();
        eprintln!(
            "  batches {}, largest {}, commits {}, conflicts {}",
            stats.batches, stats.largest_batch, stats.commits, stats.conflicts
        );
        largest_batch = largest_batch.max(stats.largest_batch);
    }
    // The run did reach what it checks: write skew, and batches.
    assert!(write_skews > 0, "no write skew: the workload is too weak");
    assert!(largest_batch > 1, "no batch held two commits");
}

#[test]
fn four_threads_show_no_anomaly() {
    no_anomalies_on(4);
}

#[test]
fn eight_threads_show_no_anomaly() {
    no_anomalies_on(8);
}
