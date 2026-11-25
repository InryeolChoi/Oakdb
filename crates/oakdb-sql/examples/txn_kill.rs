//! Fault experiment helper for docs/experiments/0018-sql-transactions.md
//! (issue #35, design 0023 section 10): kill the process with SIGKILL in
//! the middle of BEGIN ... COMMIT and check what reopening finds.
//! `docs/experiments/scripts/txn_kill.py` drives it.
//!
//! - `txn_kill open <dir>`: commits rows 1 and 2 of `t (id INTEGER
//!   PRIMARY KEY, n INTEGER)` on their own, then BEGINs, inserts 10 000
//!   rows, updates row 1 and deletes row 2, prints `open` and waits to be
//!   killed without committing.
//! - `txn_kill check-open <dir>`: checks that `t` holds rows 1 and 2 as
//!   committed and nothing of the open transaction.
//! - `txn_kill loop <dir>`: runs transactions one after another, each
//!   `BEGIN; INSERT 100 rows with n = k; UPDATE c SET n = k; COMMIT`, and
//!   prints `k` once COMMIT returns, until killed.
//! - `txn_kill check-loop <dir> <last>`: checks that every transaction is
//!   there whole or not at all: `c.n` is some `m >= last`, and `t` holds
//!   exactly the 100 rows of each `k` in `0..=m`.

use oakdb::Db;
use oakdb_sql::{execute, Output, Session, Value};

/// Rows the open transaction inserts.
const OPEN_ROWS: i64 = 10_000;
/// Rows each transaction of the loop inserts.
const PER_TXN: i64 = 100;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("open"), Some(dir)) => open(dir),
        (Some("check-open"), Some(dir)) => check_open(dir),
        (Some("loop"), Some(dir)) => run_loop(dir),
        (Some("check-loop"), Some(dir)) => {
            check_loop(dir, args.get(3).and_then(|s| s.parse().ok()).unwrap_or(-1))
        }
        _ => {
            eprintln!("usage: txn_kill open|check-open|loop|check-loop <dir> [last]");
            std::process::exit(2);
        }
    }
}

fn run(session: &mut Session<'_>, sql: &str) {
    session
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn insert_rows(session: &mut Session<'_>, ids: std::ops::Range<i64>, n: impl Fn(i64) -> i64) {
    let ids: Vec<i64> = ids.collect();
    for chunk in ids.chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|&id| format!("({id}, {})", n(id))).collect();
        run(session, &format!("INSERT INTO t VALUES {}", values.join(", ")));
    }
}

fn open(dir: &str) {
    let db = Db::open(dir).expect("open");
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").expect("CREATE");
    execute(&db, "INSERT INTO t VALUES (1, 1), (2, 2)").expect("INSERT");
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    insert_rows(&mut session, 100..100 + OPEN_ROWS, |id| id);
    run(&mut session, "UPDATE t SET n = 100 WHERE id = 1");
    run(&mut session, "DELETE FROM t WHERE id = 2");
    println!("open");
    loop {
        std::thread::park();
    }
}

fn pairs(db: &Db, sql: &str) -> Vec<(i64, i64)> {
    match execute(db, sql).expect(sql) {
        Output::Rows { rows, .. } => rows
            .iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Integer(a), Value::Integer(b)) => (*a, *b),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn check_open(dir: &str) {
    let db = Db::open(dir).expect("reopen");
    let found = pairs(&db, "SELECT id, n FROM t");
    assert_eq!(found, vec![(1, 1), (2, 2)], "a trace of the open transaction");
    println!("ok: t holds rows 1 and 2 as committed, nothing of the open transaction");
}

fn run_loop(dir: &str) {
    let db = Db::open(dir).expect("open");
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").expect("CREATE");
    execute(&db, "CREATE TABLE c (id INTEGER PRIMARY KEY, n INTEGER)").expect("CREATE");
    execute(&db, "INSERT INTO c VALUES (0, -1)").expect("INSERT");
    let mut session = Session::new(&db);
    for k in 0.. {
        run(&mut session, "BEGIN");
        insert_rows(&mut session, k * PER_TXN..(k + 1) * PER_TXN, |_| k);
        run(&mut session, &format!("UPDATE c SET n = {k} WHERE id = 0"));
        run(&mut session, "COMMIT");
        println!("{k}");
    }
}

fn check_loop(dir: &str, last: i64) {
    let db = Db::open(dir).expect("reopen");
    let m = match pairs(&db, "SELECT id, n FROM c")[..] {
        [(0, m)] => m,
        ref other => panic!("c holds {other:?}"),
    };
    assert!(m >= last, "COMMIT of {last} returned, but c.n = {m}");
    let rows = pairs(&db, "SELECT id, n FROM t");
    assert_eq!(rows.len() as i64, (m + 1) * PER_TXN, "c.n = {m}");
    for (id, n) in &rows {
        assert_eq!(*n, id / PER_TXN, "row {id}");
    }
    println!("ok: {} transactions whole (last acknowledged {last}), none partial", m + 1);
}
