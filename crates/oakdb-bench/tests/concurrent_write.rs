//! The benchmark measures what it says it does (issue #30, design 0026):
//! short windows on a real disk, so these run in CI in a second or two.
//! The numbers themselves are not checked; how they are counted is.

use std::time::Duration;

use oakdb::BATCH_SIZE_BUCKETS;
use oakdb_bench::{run, Config, System, Workload};

fn config(system: System, workload: Workload, threads: usize) -> Config {
    Config {
        system,
        workload,
        threads,
        warmup: Duration::from_millis(20),
        window: Duration::from_millis(150),
        preload: 200,
        seed: 7,
    }
}

const ALL: [System; 7] = [
    System::Oak,
    System::OakOff,
    System::OakNoSync,
    System::OakSql,
    System::SqliteS1,
    System::SqliteS2,
    System::SqliteS3,
];

#[test]
fn every_system_commits_from_several_threads() {
    for system in ALL {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), &config(system, Workload::Distinct, 3));
        assert!(out.commits > 0, "{system:?}: {out:?}");
        assert!(out.total_commits >= out.commits, "{system:?}: {out:?}");
        assert_eq!(out.timeouts, 0, "{system:?}: {out:?}");
        assert!(out.p50_us <= out.p99_us, "{system:?}: {out:?}");
        assert!(
            out.thread_commits_min <= out.thread_commits_max
                && out.thread_commits_max <= out.commits,
            "{system:?}: {out:?}"
        );
    }
}

#[test]
fn a_window_of_no_time_counts_no_commit_and_is_no_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(System::Oak, Workload::Distinct, 2);
    c.window = Duration::ZERO;
    let out = run(dir.path(), &c);
    assert_eq!(out.commits, 0, "{out:?}");
    assert_eq!(out.per_second(), 0.0);
}

#[test]
fn sqlite_runs_with_the_durability_oakdb_has() {
    // Read back from the database, not taken from what was asked.
    for system in [System::SqliteS1, System::SqliteS2] {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), &config(system, Workload::Distinct, 1));
        let pragma = |name: &str| {
            out.pragma(name)
                .unwrap_or_else(|| panic!("{name}: {out:?}"))
        };
        assert_eq!(pragma("journal_mode"), "wal", "{system:?}");
        // 2 is FULL: a sync at every commit.
        assert_eq!(pragma("synchronous"), "2", "{system:?}");
        assert_eq!(pragma("fullfsync"), "1", "{system:?}");
        assert_eq!(pragma("checkpoint_fullfsync"), "1", "{system:?}");
        assert_eq!(pragma("page_size"), "4096", "{system:?}");
        assert!(pragma("sqlite_version").starts_with('3'), "{system:?}");
    }
}

#[test]
fn sqlite_s3_keeps_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &config(System::SqliteS3, Workload::Distinct, 1));
    assert_eq!(out.pragma("journal_mode"), Some("delete"), "{out:?}");
    assert_eq!(out.pragma("fullfsync"), Some("0"), "{out:?}");
}

#[test]
fn group_commit_off_puts_each_commit_in_a_batch_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &config(System::OakOff, Workload::Distinct, 4));
    let engine = out.engine.expect("Oakdb counts batches");
    assert_eq!(engine.largest_batch, 1, "{out:?}");
    assert_eq!(
        engine.batch_sizes[1..],
        [0; BATCH_SIZE_BUCKETS - 1],
        "{out:?}"
    );
    assert_eq!(engine.wal_syncs, engine.batches, "{out:?}");
}

#[test]
fn group_commit_on_shares_syncs_among_threads() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &config(System::Oak, Workload::Distinct, 8));
    let engine = out.engine.expect("Oakdb counts batches");
    assert!(engine.largest_batch > 1, "{out:?}");
}

#[test]
fn a_hot_counter_loses_no_increment() {
    // Every commit adds one to one of the hot counters, so they add up to
    // the commits: none was lost to a conflict that went unnoticed.
    for system in [System::Oak, System::SqliteS1, System::SqliteS2] {
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), &config(system, Workload::Hot, 4));
        assert_eq!(out.hot_sum, Some(out.total_commits), "{system:?}: {out:?}");
    }
}

#[test]
fn the_output_line_names_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &config(System::Oak, Workload::Large, 2));
    let line = out.line();
    for field in [
        "system=oak",
        "workload=w2",
        "threads=2",
        "commits=",
        "per_second=",
        "p50_us=",
        "p99_us=",
        "retries=",
        "thread_min=",
        "thread_max=",
        "batches=",
        "wal_syncs=",
        "batch_sizes=",
        "wal_write_ms=",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
    assert!(!line.contains('\n'));
}

#[test]
fn names_parse_back() {
    for system in ALL {
        assert_eq!(system.name().parse::<System>(), Ok(system));
    }
    for workload in [Workload::Distinct, Workload::Large, Workload::Hot] {
        assert_eq!(workload.name().parse::<Workload>(), Ok(workload));
    }
    assert!("sqlite".parse::<System>().is_err());
}
