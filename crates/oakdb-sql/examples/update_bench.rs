//! Benchmark helper for design 0022, section 8 (issue #33): how long a
//! whole-table UPDATE or DELETE takes, and, run under a peak-memory
//! counter (`docs/experiments/scripts/bench_update.py`), how much memory
//! it needs on top of loading the table.
//!
//! `update_bench <rows> <mode>` makes a fresh database in a temporary
//! directory, loads `rows` rows of `(id INTEGER PRIMARY KEY, n INTEGER,
//! s TEXT)` with a 32-byte text, then runs one statement by `mode`:
//!
//! - `load`: nothing; the baseline the others are compared with
//! - `update`: `UPDATE t SET n = n + 1` (every row, keys stay)
//! - `update-key`: `UPDATE t SET id = id + <rows>` (every row moves)
//! - `delete`: `DELETE FROM t`
//!
//! It prints one line: `rows=<rows> mode=<mode> load_s=<s> statement_s=<s>
//! affected=<n>`. Run it in release mode.

use std::time::Instant;

use oakdb::Db;
use oakdb_sql::{execute, Output};

/// Rows per INSERT while loading.
const BATCH: usize = 1000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(rows), Some(mode)) = (
        args.get(1).and_then(|r| r.parse::<usize>().ok()),
        args.get(2),
    ) else {
        eprintln!("usage: update_bench <rows> load|update|update-key|delete");
        std::process::exit(2);
    };
    let statement = match mode.as_str() {
        "load" => None,
        "update" => Some("UPDATE t SET n = n + 1".to_owned()),
        "update-key" => Some(format!("UPDATE t SET id = id + {rows}")),
        "delete" => Some("DELETE FROM t".to_owned()),
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    };

    let dir = tempfile::tempdir().expect("a temporary directory");
    let db = Db::open(dir.path()).expect("a fresh database");
    let load = Instant::now();
    execute(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)",
    )
    .expect("CREATE TABLE");
    let mut id = 0;
    while id < rows {
        let end = (id + BATCH).min(rows);
        let values: Vec<String> = (id..end)
            .map(|i| format!("({i}, {i}, '{:032}')", i))
            .collect();
        execute(&db, &format!("INSERT INTO t VALUES {}", values.join(", "))).expect("INSERT");
        id = end;
    }
    let load_s = load.elapsed().as_secs_f64();

    let (statement_s, affected) = match statement {
        None => (0.0, 0),
        Some(sql) => {
            let start = Instant::now();
            let out = execute(&db, &sql).expect("the statement");
            let elapsed = start.elapsed().as_secs_f64();
            match out {
                Output::Affected(n) => (elapsed, n),
                other => panic!("{other:?}"),
            }
        }
    };
    println!("rows={rows} mode={mode} load_s={load_s:.3} statement_s={statement_s:.3} affected={affected}");
}
