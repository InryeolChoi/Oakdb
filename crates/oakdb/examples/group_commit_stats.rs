//! How big group commit's batches get (issue #28, design 0021 G7): commits
//! from several threads at once on a real disk, then what `Db::stats`
//! counted.
//!
//! ```text
//! cargo run --release --example group_commit_stats -- <dir> [threads] [commits per thread]
//! ```
//!
//! For each thread count in `1,2,4,8,16` (or the one given), on a fresh
//! database in `<dir>`, prints the commits a second, the batches, the WAL
//! syncs, the mean batch and the largest batch.

use std::time::Instant;

use oakdb::Db;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(args.get(1).expect("usage: <dir> [threads] [commits]"));
    let counts: Vec<u32> = match args.get(2) {
        Some(t) => vec![t.parse().expect("threads is a number")],
        None => vec![1, 2, 4, 8, 16],
    };
    let per_thread: u32 = args.get(3).map_or(300, |n| n.parse().expect("a number"));
    println!("threads  commits/s  commits  batches  wal_syncs  mean batch  largest");
    for threads in counts {
        let db_dir = dir.join(format!("threads-{threads}"));
        let _ = std::fs::remove_dir_all(&db_dir);
        let db = Db::open(&db_dir).expect("open");
        let before = db.stats();
        let start = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let db = &db;
                s.spawn(move || {
                    for i in 0..per_thread {
                        let mut txn = db.begin();
                        let key = [t.to_be_bytes(), i.to_be_bytes()].concat();
                        txn.put(&key, b"v").expect("put");
                        txn.commit().expect("commit");
                    }
                });
            }
        });
        let secs = start.elapsed().as_secs_f64();
        let stats = db.stats();
        let commits = stats.commits - before.commits;
        let batches = stats.batches - before.batches;
        println!(
            "{threads:>7}  {:>9.0}  {commits:>7}  {batches:>7}  {:>9}  {:>10.2}  {:>7}",
            f64::from(threads * per_thread) / secs,
            stats.wal_syncs - before.wal_syncs,
            commits as f64 / batches as f64,
            stats.largest_batch,
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&db_dir);
    }
}
