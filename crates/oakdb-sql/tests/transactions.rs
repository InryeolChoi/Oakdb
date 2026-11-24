//! BEGIN, COMMIT and ROLLBACK (issue #35, design 0023), seen through
//! sessions: the SQL strings they run and the rows they leave.

use std::sync::Barrier;
use std::thread;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, Output, Session, SqlError, TransactionError, Value};

fn new_db() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/oak").unwrap()
}

/// A database holding `t (id INTEGER PRIMARY KEY, n INTEGER)`.
fn with_t() -> Db<SimFs> {
    let db = new_db();
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
    db
}

fn run(session: &mut Session<'_, SimFs>, sql: &str) -> Output {
    session
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn fail(session: &mut Session<'_, SimFs>, sql: &str) -> SqlError {
    match session.execute(sql) {
        Ok(out) => panic!("{sql}: expected an error, got {out:?}"),
        Err(e) => e,
    }
}

fn rows(session: &mut Session<'_, SimFs>, sql: &str) -> Vec<Vec<Value>> {
    match run(session, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

/// Every `(id, n)` of `t` as committed: read in a transaction of its own.
fn committed(db: &Db<SimFs>) -> Vec<(i64, Option<i64>)> {
    match execute(db, "SELECT id, n FROM t").unwrap() {
        Output::Rows { rows, .. } => rows.iter().map(|row| pair(row)).collect(),
        other => panic!("{other:?}"),
    }
}

/// `(id, n)` of `t` as `session` sees it.
fn seen(session: &mut Session<'_, SimFs>) -> Vec<(i64, Option<i64>)> {
    rows(session, "SELECT id, n FROM t")
        .iter()
        .map(|row| pair(row))
        .collect()
}

fn pair(row: &[Value]) -> (i64, Option<i64>) {
    let id = match row[0] {
        Value::Integer(id) => id,
        ref other => panic!("{other:?}"),
    };
    let n = match row[1] {
        Value::Integer(n) => Some(n),
        Value::Null => None,
        ref other => panic!("{other:?}"),
    };
    (id, n)
}

fn is_conflict(e: &SqlError) -> bool {
    matches!(e, SqlError::Engine(oakdb::Error::Conflict { .. }))
}

// ------------------------------------------------------- the issue's criteria

#[test]
fn statements_after_begin_take_effect_together_at_commit() {
    let db = with_t();
    let mut session = Session::new(&db);
    assert!(!session.in_transaction());
    assert_eq!(run(&mut session, "BEGIN"), Output::Done);
    assert!(session.in_transaction());
    run(&mut session, "INSERT INTO t VALUES (1, 10), (2, 20)");
    run(&mut session, "UPDATE t SET n = n + 1 WHERE id = 1");
    run(&mut session, "DELETE FROM t WHERE id = 2");
    // Not yet: nothing is committed before COMMIT.
    assert_eq!(committed(&db), vec![]);
    assert_eq!(run(&mut session, "COMMIT"), Output::Done);
    assert!(!session.in_transaction());
    assert_eq!(committed(&db), vec![(1, Some(11))]);
}

#[test]
fn rollback_leaves_no_trace() {
    let db = with_t();
    execute(&db, "INSERT INTO t VALUES (1, 1)").unwrap();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (2, 2)");
    run(&mut session, "UPDATE t SET n = 100");
    run(&mut session, "DELETE FROM t WHERE id = 1");
    assert_eq!(run(&mut session, "ROLLBACK"), Output::Done);
    assert!(!session.in_transaction());
    assert_eq!(committed(&db), vec![(1, Some(1))]);
    assert_eq!(seen(&mut session), vec![(1, Some(1))]);
}

#[test]
fn statements_outside_a_transaction_commit_on_their_own() {
    let db = with_t();
    let mut session = Session::new(&db);
    assert_eq!(
        run(&mut session, "INSERT INTO t VALUES (1, 1)"),
        Output::Affected(1)
    );
    assert_eq!(committed(&db), vec![(1, Some(1))]);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (2, 2)");
    run(&mut session, "COMMIT");
    // Back to autocommit after COMMIT.
    run(&mut session, "INSERT INTO t VALUES (3, 3)");
    assert_eq!(
        committed(&db),
        vec![(1, Some(1)), (2, Some(2)), (3, Some(3))]
    );
}

#[test]
fn a_conflict_at_commit_reaches_sql_and_ends_the_transaction() {
    let db = with_t();
    execute(&db, "INSERT INTO t VALUES (1, 0)").unwrap();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    run(&mut a, "BEGIN");
    run(&mut a, "UPDATE t SET n = 1 WHERE id = 1");
    run(&mut a, "INSERT INTO t VALUES (2, 2)");
    // B writes the same row and commits first.
    run(&mut b, "UPDATE t SET n = 5 WHERE id = 1");
    let e = fail(&mut a, "COMMIT");
    assert!(is_conflict(&e), "{e:?}");
    // The message says the whole transaction is gone (design 0023, Q4).
    assert!(e.to_string().contains("BEGIN"), "{e}");
    // COMMIT ended the transaction even though it failed (T6): A is back
    // in autocommit, and none of its writes is there.
    assert!(!a.in_transaction());
    assert_eq!(committed(&db), vec![(1, Some(5))]);
    let e = fail(&mut a, "ROLLBACK");
    assert!(
        matches!(e, SqlError::Transaction(TransactionError::NotBegun)),
        "{e:?}"
    );
    run(&mut a, "INSERT INTO t VALUES (3, 3)");
    assert_eq!(committed(&db), vec![(1, Some(5)), (3, Some(3))]);
}

#[test]
fn an_autocommit_statement_that_conflicts_says_so() {
    let db = with_t();
    execute(&db, "INSERT INTO t VALUES (1, 0)").unwrap();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    // B's transaction began before A's autocommit write and writes the
    // same row; A commits first, so B loses at COMMIT.
    run(&mut b, "BEGIN");
    run(&mut b, "UPDATE t SET n = 2 WHERE id = 1");
    run(&mut a, "UPDATE t SET n = 1 WHERE id = 1");
    assert!(is_conflict(&fail(&mut b, "COMMIT")));
    assert_eq!(committed(&db), vec![(1, Some(1))]);
}

// ------------------------------------------------------------- the grammar

#[test]
fn the_commands_take_an_optional_transaction_and_semicolon_in_any_case() {
    let db = with_t();
    let mut session = Session::new(&db);
    for (begin, end) in [
        ("BEGIN", "COMMIT"),
        ("begin transaction;", "commit transaction;"),
        ("Begin;", "Rollback Transaction"),
        ("BEGIN TRANSACTION", "rollback;"),
    ] {
        run(&mut session, begin);
        assert!(session.in_transaction(), "{begin}");
        run(&mut session, end);
        assert!(!session.in_transaction(), "{end}");
    }
}

#[test]
fn other_transaction_forms_are_unsupported() {
    let db = with_t();
    let mut session = Session::new(&db);
    for sql in [
        "START TRANSACTION",
        "END",
        "COMMIT WORK",
        "BEGIN IMMEDIATE",
        "BEGIN DEFERRED",
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
        "SAVEPOINT a",
        "RELEASE a",
        "ROLLBACK TO a",
        "BEGIN TRANSACTION READ ONLY",
    ] {
        let e = fail(&mut session, sql);
        assert!(matches!(e, SqlError::Unsupported(_)), "{sql}: {e:?}");
        assert!(!session.in_transaction(), "{sql}");
    }
}

#[test]
fn begin_commit_and_rollback_are_keywords() {
    let db = new_db();
    let mut session = Session::new(&db);
    for word in ["begin", "commit", "rollback"] {
        let e = fail(
            &mut session,
            &format!("CREATE TABLE {word} (id INTEGER PRIMARY KEY)"),
        );
        assert!(matches!(e, SqlError::Syntax { .. }), "{word}: {e:?}");
    }
    // TRANSACTION is not one (Q1).
    run(
        &mut session,
        "CREATE TABLE transaction (id INTEGER PRIMARY KEY)",
    );
}

#[test]
fn two_commands_in_one_string_are_refused() {
    let db = with_t();
    let mut session = Session::new(&db);
    let e = fail(&mut session, "BEGIN; INSERT INTO t VALUES (1, 1)");
    assert!(matches!(e, SqlError::Unsupported(_)), "{e:?}");
    assert!(!session.in_transaction());
    assert_eq!(committed(&db), vec![]);
}

// ------------------------------------------------ commands out of their state

#[test]
fn begin_inside_a_transaction_fails_and_keeps_it_open() {
    let db = with_t();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (1, 1)");
    let e = fail(&mut session, "BEGIN");
    assert!(
        matches!(e, SqlError::Transaction(TransactionError::AlreadyBegun)),
        "{e:?}"
    );
    assert!(session.in_transaction());
    run(&mut session, "INSERT INTO t VALUES (2, 2)");
    run(&mut session, "COMMIT");
    assert_eq!(committed(&db), vec![(1, Some(1)), (2, Some(2))]);
}

#[test]
fn commit_or_rollback_outside_a_transaction_fails() {
    let db = with_t();
    let mut session = Session::new(&db);
    for sql in ["COMMIT", "ROLLBACK"] {
        let e = fail(&mut session, sql);
        assert!(
            matches!(e, SqlError::Transaction(TransactionError::NotBegun)),
            "{sql}: {e:?}"
        );
        assert!(!session.in_transaction());
    }
    // Autocommit still works.
    run(&mut session, "INSERT INTO t VALUES (1, 1)");
    assert_eq!(committed(&db), vec![(1, Some(1))]);
}

#[test]
fn the_one_shot_execute_refuses_transaction_commands() {
    let db = with_t();
    for sql in ["BEGIN", "COMMIT", "ROLLBACK", "begin transaction;"] {
        let e = execute(&db, sql).unwrap_err();
        assert!(
            matches!(e, SqlError::Transaction(TransactionError::NeedsSession)),
            "{sql}: {e:?}"
        );
    }
    // Statements still run.
    execute(&db, "INSERT INTO t VALUES (1, 1)").unwrap();
    assert_eq!(committed(&db), vec![(1, Some(1))]);
}

// ------------------------------------------- statement atomicity inside BEGIN

#[test]
fn an_insert_whose_later_row_is_a_duplicate_leaves_nothing_inside_begin() {
    let db = with_t();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (1, 1)");
    let e = fail(&mut session, "INSERT INTO t VALUES (2, 2), (1, 9)");
    assert!(matches!(e, SqlError::DuplicateKey { .. }), "{e:?}");
    // Only the failed statement is undone; the transaction goes on (T5).
    assert!(session.in_transaction());
    assert_eq!(seen(&mut session), vec![(1, Some(1))]);
    run(&mut session, "COMMIT");
    assert_eq!(committed(&db), vec![(1, Some(1))]);
}

#[test]
fn an_insert_repeating_a_key_of_its_own_leaves_nothing_inside_begin() {
    let db = with_t();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    let e = fail(&mut session, "INSERT INTO t VALUES (2, 2), (3, 3), (2, 4)");
    assert!(matches!(e, SqlError::DuplicateKey { .. }), "{e:?}");
    let e = fail(&mut session, "INSERT INTO t VALUES (4, 4), (5, 'x')");
    assert!(matches!(e, SqlError::TypeMismatch { .. }), "{e:?}");
    let e = fail(&mut session, "INSERT INTO t VALUES (6, 6), (NULL, 7)");
    assert!(matches!(e, SqlError::TypeMismatch { .. }), "{e:?}");
    let e = fail(&mut session, "INSERT INTO t VALUES (7, 7), (8)");
    assert!(matches!(e, SqlError::ValueCount { .. }), "{e:?}");
    assert_eq!(seen(&mut session), vec![]);
    run(&mut session, "COMMIT");
    assert_eq!(committed(&db), vec![]);
}

#[test]
fn an_insert_with_a_key_too_long_leaves_nothing_inside_begin() {
    let db = new_db();
    execute(&db, "CREATE TABLE s (k TEXT PRIMARY KEY)").unwrap();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    let long = "x".repeat(1000);
    let e = fail(
        &mut session,
        &format!("INSERT INTO s VALUES ('a'), ('{long}')"),
    );
    assert!(
        matches!(e, SqlError::Engine(oakdb::Error::KeyTooLarge { .. })),
        "{e:?}"
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM s"),
        Vec::<Vec<Value>>::new()
    );
    run(&mut session, "COMMIT");
    assert_eq!(
        execute(&db, "SELECT * FROM s").unwrap(),
        Output::Rows {
            columns: vec!["k".into()],
            rows: vec![]
        }
    );
}

#[test]
fn an_update_that_overflows_midway_leaves_nothing_inside_begin() {
    let db = with_t();
    execute(
        &db,
        "INSERT INTO t VALUES (1, 1), (2, 2), (3, 9223372036854775807)",
    )
    .unwrap();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "UPDATE t SET n = 0 WHERE id = 1");
    let e = fail(&mut session, "UPDATE t SET n = n * 2");
    assert!(matches!(e, SqlError::Arithmetic(_)), "{e:?}");
    assert!(session.in_transaction());
    assert_eq!(
        seen(&mut session),
        vec![(1, Some(0)), (2, Some(2)), (3, Some(i64::MAX))]
    );
    run(&mut session, "COMMIT");
    assert_eq!(
        committed(&db),
        vec![(1, Some(0)), (2, Some(2)), (3, Some(i64::MAX))]
    );
}

#[test]
fn other_statement_errors_keep_the_transaction_open() {
    let db = with_t();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (1, 1)");
    for sql in [
        "SELEC * FROM t",
        "SELECT * FROM nope",
        "SELECT nope FROM t",
        "INSERT INTO t VALUES (1, 2)",
        "UPDATE t SET id = NULL",
        "TRUNCATE t",
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    ] {
        session.execute(sql).unwrap_err();
        assert!(session.in_transaction(), "{sql}");
        assert_eq!(seen(&mut session), vec![(1, Some(1))], "{sql}");
    }
    run(&mut session, "COMMIT");
    assert_eq!(committed(&db), vec![(1, Some(1))]);
}

// ----------------------------------------------------------------- reads

#[test]
fn reads_inside_a_transaction_see_its_own_writes() {
    let db = with_t();
    execute(&db, "INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (3, 3)");
    run(&mut session, "DELETE FROM t WHERE id = 1");
    run(&mut session, "UPDATE t SET n = 20 WHERE id = 2");
    assert_eq!(seen(&mut session), vec![(2, Some(20)), (3, Some(3))]);
    assert_eq!(
        rows(&mut session, "SELECT n FROM t WHERE id = 3"),
        vec![vec![Value::Integer(3)]]
    );
    // An UPDATE in the transaction sees the row the transaction inserted.
    run(&mut session, "UPDATE t SET n = n + 1 WHERE id >= 3");
    assert_eq!(seen(&mut session), vec![(2, Some(20)), (3, Some(4))]);
    run(&mut session, "ROLLBACK");
}

#[test]
fn a_transaction_reads_the_database_as_it_was_at_begin() {
    let db = with_t();
    execute(&db, "INSERT INTO t VALUES (1, 1)").unwrap();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    run(&mut a, "BEGIN");
    run(&mut a, "INSERT INTO t VALUES (2, 2)");
    // A's uncommitted insert is not seen by B.
    assert_eq!(seen(&mut b), vec![(1, Some(1))]);
    // B's commit after A's BEGIN is not seen by A.
    run(&mut b, "INSERT INTO t VALUES (3, 3)");
    run(&mut b, "UPDATE t SET n = 10 WHERE id = 1");
    assert_eq!(seen(&mut a), vec![(1, Some(1)), (2, Some(2))]);
    run(&mut a, "COMMIT");
    // After COMMIT, A reads in autocommit and sees both.
    assert_eq!(
        seen(&mut a),
        vec![(1, Some(10)), (2, Some(2)), (3, Some(3))]
    );
}

// ---------------------------------------------------------- CREATE TABLE

#[test]
fn create_table_inside_a_transaction_is_seen_by_its_later_statements() {
    let db = new_db();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(
        &mut session,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut session, "INSERT INTO u VALUES (1, 1)");
    assert_eq!(
        rows(&mut session, "SELECT * FROM u"),
        vec![vec![Value::Integer(1), Value::Integer(1)]]
    );
    // Not seen outside before COMMIT (Q9).
    assert!(matches!(
        execute(&db, "SELECT * FROM u"),
        Err(SqlError::NoSuchTable(_))
    ));
    run(&mut session, "COMMIT");
    assert_eq!(
        execute(&db, "SELECT * FROM u").unwrap(),
        Output::Rows {
            columns: vec!["id".into(), "n".into()],
            rows: vec![vec![Value::Integer(1), Value::Integer(1)]]
        }
    );
}

#[test]
fn create_table_rolled_back_leaves_no_table() {
    let db = new_db();
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "CREATE TABLE u (id INTEGER PRIMARY KEY)");
    run(&mut session, "INSERT INTO u VALUES (1)");
    run(&mut session, "ROLLBACK");
    assert!(matches!(
        session.execute("SELECT * FROM u"),
        Err(SqlError::NoSuchTable(_))
    ));
    // The name is free again.
    run(&mut session, "CREATE TABLE u (id INTEGER PRIMARY KEY)");
}

#[test]
fn two_transactions_creating_tables_at_once_conflict() {
    let db = new_db();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    // Different names still share the table id counter (design 0023,
    // section 8): the later COMMIT loses, and loses its rows too.
    run(&mut a, "BEGIN");
    run(&mut b, "BEGIN");
    run(&mut a, "CREATE TABLE x (id INTEGER PRIMARY KEY)");
    run(&mut b, "CREATE TABLE y (id INTEGER PRIMARY KEY)");
    run(&mut b, "INSERT INTO y VALUES (1)");
    run(&mut a, "COMMIT");
    assert!(is_conflict(&fail(&mut b, "COMMIT")));
    assert!(matches!(
        execute(&db, "SELECT * FROM y"),
        Err(SqlError::NoSuchTable(_))
    ));
    // The same name conflicts too.
    run(&mut a, "BEGIN");
    run(&mut b, "BEGIN");
    run(&mut a, "CREATE TABLE z (id INTEGER PRIMARY KEY)");
    run(&mut b, "CREATE TABLE z (k TEXT PRIMARY KEY)");
    run(&mut a, "COMMIT");
    assert!(is_conflict(&fail(&mut b, "COMMIT")));
    // Running B's transaction again, it sees the table A made.
    run(&mut b, "BEGIN");
    let e = fail(&mut b, "CREATE TABLE z (k TEXT PRIMARY KEY)");
    assert!(matches!(e, SqlError::TableExists(_)), "{e:?}");
    run(&mut b, "CREATE TABLE y (id INTEGER PRIMARY KEY)");
    run(&mut b, "COMMIT");
}

// ------------------------------------------------------------ dropping

#[test]
fn dropping_a_session_with_an_open_transaction_rolls_it_back() {
    let db = with_t();
    {
        let mut session = Session::new(&db);
        run(&mut session, "BEGIN");
        run(&mut session, "INSERT INTO t VALUES (1, 1)");
    }
    assert_eq!(committed(&db), vec![]);
    // Its transaction no longer counts for conflicts: a later one writing
    // the same row commits.
    let mut session = Session::new(&db);
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (1, 2)");
    run(&mut session, "COMMIT");
    assert_eq!(committed(&db), vec![(1, Some(2))]);
}

#[test]
fn an_open_transaction_is_gone_after_reopening() {
    let fs = SimFs::new();
    {
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
        let mut session = Session::new(&db);
        run(&mut session, "BEGIN");
        run(&mut session, "INSERT INTO t VALUES (1, 1)");
        // The power goes while the transaction is open.
        fs.crash(7);
    }
    let db = Db::open_with(fs, "/oak").unwrap();
    assert_eq!(committed(&db), vec![]);
}

// ------------------------------------------------------- disk failure

#[test]
fn a_disk_failure_during_commit_ends_the_transaction_and_stops_the_engine() {
    for seed in 0..8 {
        let fs = SimFs::new();
        {
            let db = Db::open_with(fs.clone(), "/oak").unwrap();
            execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
            let mut session = Session::new(&db);
            run(&mut session, "BEGIN");
            run(&mut session, "INSERT INTO t VALUES (1, 1), (2, 2)");
            run(&mut session, "UPDATE t SET n = 0 WHERE id = 2");
            fs.fail_once_after(0);
            let e = fail(&mut session, "COMMIT");
            assert!(
                matches!(e, SqlError::Engine(oakdb::Error::OutcomeUnknown(_))),
                "{e:?}"
            );
            assert!(!session.in_transaction());
            // Every statement after it finds the engine stopped, inside a
            // transaction or not.
            let e = fail(&mut session, "INSERT INTO t VALUES (3, 3)");
            assert!(
                matches!(e, SqlError::Engine(oakdb::Error::Stopped(_))),
                "{e:?}"
            );
            run(&mut session, "BEGIN");
            let e = fail(&mut session, "SELECT * FROM t");
            assert!(
                matches!(e, SqlError::Engine(oakdb::Error::Stopped(_))),
                "{e:?}"
            );
            assert!(session.in_transaction());
            let e = fail(&mut session, "COMMIT");
            assert!(
                matches!(e, SqlError::Engine(oakdb::Error::Stopped(_))),
                "{e:?}"
            );
            assert!(!session.in_transaction());
            fs.crash(seed);
        }
        // Reopened, the transaction is there whole or not at all.
        let db = Db::open_with(fs, "/oak").unwrap();
        let found = committed(&db);
        assert!(
            found.is_empty() || found == vec![(1, Some(1)), (2, Some(0))],
            "seed {seed}: {found:?}"
        );
    }
}

// --------------------------------------------------------- concurrency

#[test]
fn sessions_on_many_threads_never_lose_an_update() {
    const THREADS: usize = 6;
    const EACH: i64 = 20;
    let db = with_t();
    execute(&db, "INSERT INTO t VALUES (1, 0), (2, 0)").unwrap();
    let start = Barrier::new(THREADS);
    thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                let mut session = Session::new(&db);
                start.wait();
                for _ in 0..EACH {
                    // Read, then write what was read plus one, in one
                    // transaction; on a conflict run it again from BEGIN.
                    loop {
                        run(&mut session, "BEGIN");
                        let n = match &rows(&mut session, "SELECT n FROM t WHERE id = 1")[0][0] {
                            Value::Integer(n) => *n,
                            other => panic!("{other:?}"),
                        };
                        run(
                            &mut session,
                            &format!("UPDATE t SET n = {} WHERE id = 1", n + 1),
                        );
                        run(&mut session, "UPDATE t SET n = n + 1 WHERE id = 2");
                        match session.execute("COMMIT") {
                            Ok(_) => break,
                            Err(e) if is_conflict(&e) => {
                                assert!(!session.in_transaction());
                            }
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
            });
        }
    });
    let total = THREADS as i64 * EACH;
    assert_eq!(committed(&db), vec![(1, Some(total)), (2, Some(total))]);
}

#[test]
fn concurrent_inserts_of_the_same_key_inside_transactions_conflict_at_commit() {
    let db = with_t();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    run(&mut a, "BEGIN");
    run(&mut b, "BEGIN");
    run(&mut a, "INSERT INTO t VALUES (1, 1)");
    // B does not see A's row, so its own INSERT succeeds inside BEGIN...
    run(&mut b, "INSERT INTO t VALUES (1, 2)");
    run(&mut a, "COMMIT");
    // ...and loses at COMMIT.
    assert!(is_conflict(&fail(&mut b, "COMMIT")));
    assert_eq!(committed(&db), vec![(1, Some(1))]);
}
