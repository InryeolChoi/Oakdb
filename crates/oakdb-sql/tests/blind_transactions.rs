//! Blind tests for BEGIN, COMMIT and ROLLBACK (issue #35, design 0023).
//!
//! Written from the issue, spec #12, designs 0017/0019/0022/0023 and the public
//! API docs only, without reading the implementation or its tests.

use std::collections::BTreeMap;

use oakdb::{fs::sim::SimFs, Db};
use oakdb_sql::{execute, Output, Session, SqlError, TransactionError, Value};

fn open() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/db").unwrap()
}

fn int(n: i64) -> Value {
    Value::Integer(n)
}

fn rows(out: Output) -> Vec<Vec<Value>> {
    match out {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Every (id, n) row of `t`, read by a statement of its own (committed data only).
fn committed(db: &Db<SimFs>) -> Vec<(i64, i64)> {
    pairs(rows(execute(db, "SELECT * FROM t").unwrap()))
}

fn seen_by(s: &mut Session<'_, SimFs>) -> Vec<(i64, i64)> {
    pairs(rows(s.execute("SELECT * FROM t").unwrap()))
}

fn pairs(rows: Vec<Vec<Value>>) -> Vec<(i64, i64)> {
    rows.into_iter()
        .map(|r| match r.as_slice() {
            [Value::Integer(a), Value::Integer(b)] => (*a, *b),
            other => panic!("unexpected row {other:?}"),
        })
        .collect()
}

fn setup(db: &Db<SimFs>, initial: &[(i64, i64)]) {
    execute(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
    for (id, n) in initial {
        execute(db, &format!("INSERT INTO t VALUES ({id}, {n})")).unwrap();
    }
}

fn is_conflict(e: &SqlError) -> bool {
    matches!(e, SqlError::Engine(oakdb::Error::Conflict { .. }))
}

fn txn_err(r: Result<Output, SqlError>) -> TransactionError {
    match r {
        Err(SqlError::Transaction(t)) => t,
        other => panic!("expected a transaction error, got {other:?}"),
    }
}

// ---------------------------------------------------------------- BEGIN ... COMMIT

#[test]
fn statements_between_begin_and_commit_appear_to_others_all_at_once_at_commit() {
    let db = open();
    setup(&db, &[(1, 10), (2, 20)]);
    let mut a = Session::new(&db);
    assert_eq!(a.execute("BEGIN").unwrap(), Output::Done);
    assert!(a.in_transaction());
    assert_eq!(a.execute("INSERT INTO t VALUES (3, 30)").unwrap(), Output::Affected(1));
    assert_eq!(committed(&db), vec![(1, 10), (2, 20)]);
    assert_eq!(a.execute("UPDATE t SET n = n + 1 WHERE id = 1").unwrap(), Output::Affected(1));
    assert_eq!(committed(&db), vec![(1, 10), (2, 20)]);
    assert_eq!(a.execute("DELETE FROM t WHERE id = 2").unwrap(), Output::Affected(1));
    assert_eq!(committed(&db), vec![(1, 10), (2, 20)]);
    let mut b = Session::new(&db);
    assert_eq!(seen_by(&mut b), vec![(1, 10), (2, 20)]);
    assert_eq!(a.execute("COMMIT").unwrap(), Output::Done);
    assert!(!a.in_transaction());
    assert_eq!(committed(&db), vec![(1, 11), (3, 30)]);
    assert_eq!(seen_by(&mut b), vec![(1, 11), (3, 30)]);
}

#[test]
fn an_empty_transaction_and_a_read_only_transaction_commit_with_done() {
    let db = open();
    setup(&db, &[(1, 1)]);
    let mut s = Session::new(&db);
    assert_eq!(s.execute("BEGIN").unwrap(), Output::Done);
    assert_eq!(s.execute("COMMIT").unwrap(), Output::Done);
    s.execute("BEGIN").unwrap();
    assert_eq!(seen_by(&mut s), vec![(1, 1)]);
    assert_eq!(s.execute("COMMIT").unwrap(), Output::Done);
    assert_eq!(committed(&db), vec![(1, 1)]);
}

// ---------------------------------------------------------------- ROLLBACK

#[test]
fn rollback_leaves_no_trace_of_inserts_updates_and_deletes() {
    let db = open();
    setup(&db, &[(1, 10), (2, 20)]);
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (3, 30), (4, 40)").unwrap();
    s.execute("UPDATE t SET n = 0").unwrap();
    s.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(seen_by(&mut s), vec![(2, 0), (3, 0), (4, 0)]);
    assert_eq!(s.execute("ROLLBACK").unwrap(), Output::Done);
    assert!(!s.in_transaction());
    assert_eq!(seen_by(&mut s), vec![(1, 10), (2, 20)]);
    assert_eq!(committed(&db), vec![(1, 10), (2, 20)]);
}

#[test]
fn rollback_removes_a_table_created_inside_the_transaction() {
    let db = open();
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    assert_eq!(
        s.execute("CREATE TABLE fresh (id INTEGER PRIMARY KEY, v TEXT)").unwrap(),
        Output::Done
    );
    s.execute("INSERT INTO fresh VALUES (1, 'a')").unwrap();
    assert_eq!(
        rows(s.execute("SELECT * FROM fresh").unwrap()),
        vec![vec![int(1), Value::Text("a".into())]]
    );
    s.execute("ROLLBACK").unwrap();
    assert!(matches!(s.execute("SELECT * FROM fresh"), Err(SqlError::NoSuchTable(_))));
    assert!(matches!(execute(&db, "SELECT * FROM fresh"), Err(SqlError::NoSuchTable(_))));
    // The name is free again.
    execute(&db, "CREATE TABLE fresh (id INTEGER PRIMARY KEY)").unwrap();
}

#[test]
fn a_table_created_inside_a_transaction_is_invisible_to_others_until_commit() {
    let db = open();
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("CREATE TABLE t2 (id INTEGER PRIMARY KEY)").unwrap();
    s.execute("INSERT INTO t2 VALUES (7)").unwrap();
    assert!(matches!(execute(&db, "SELECT * FROM t2"), Err(SqlError::NoSuchTable(_))));
    s.execute("COMMIT").unwrap();
    assert_eq!(rows(execute(&db, "SELECT * FROM t2").unwrap()), vec![vec![int(7)]]);
}

// ---------------------------------------------------------------- autocommit

#[test]
fn statements_outside_begin_commit_on_their_own() {
    let db = open();
    setup(&db, &[]);
    let mut s = Session::new(&db);
    assert!(!s.in_transaction());
    s.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    assert_eq!(committed(&db), vec![(1, 1)]);
    s.execute("UPDATE t SET n = 5 WHERE id = 1").unwrap();
    assert_eq!(committed(&db), vec![(1, 5)]);
    // A failing autocommit statement leaves nothing, and the session stays in autocommit.
    assert!(matches!(
        s.execute("INSERT INTO t VALUES (2, 2), (1, 1)"),
        Err(SqlError::DuplicateKey { .. })
    ));
    assert!(!s.in_transaction());
    assert_eq!(committed(&db), vec![(1, 5)]);
    s.execute("DELETE FROM t").unwrap();
    assert_eq!(committed(&db), vec![]);
    // After a transaction ends the session is back in autocommit.
    s.execute("BEGIN").unwrap();
    s.execute("COMMIT").unwrap();
    s.execute("INSERT INTO t VALUES (9, 9)").unwrap();
    assert_eq!(committed(&db), vec![(9, 9)]);
}

// ---------------------------------------------------------------- state errors

#[test]
fn begin_inside_a_transaction_is_an_error_and_keeps_the_open_transaction() {
    let db = open();
    setup(&db, &[]);
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    assert_eq!(txn_err(s.execute("BEGIN")), TransactionError::AlreadyBegun);
    assert!(s.in_transaction());
    assert_eq!(committed(&db), vec![], "the error must not commit the open transaction");
    assert_eq!(seen_by(&mut s), vec![(1, 1)]);
    s.execute("COMMIT").unwrap();
    assert_eq!(committed(&db), vec![(1, 1)]);
}

#[test]
fn commit_and_rollback_outside_a_transaction_are_errors() {
    let db = open();
    setup(&db, &[(1, 1)]);
    let mut s = Session::new(&db);
    assert_eq!(txn_err(s.execute("COMMIT")), TransactionError::NotBegun);
    assert_eq!(txn_err(s.execute("ROLLBACK")), TransactionError::NotBegun);
    assert!(!s.in_transaction());
    s.execute("BEGIN").unwrap();
    s.execute("ROLLBACK").unwrap();
    assert_eq!(txn_err(s.execute("ROLLBACK")), TransactionError::NotBegun);
    s.execute("BEGIN").unwrap();
    s.execute("COMMIT").unwrap();
    assert_eq!(txn_err(s.execute("COMMIT")), TransactionError::NotBegun);
    assert_eq!(committed(&db), vec![(1, 1)]);
}

#[test]
fn the_one_shot_execute_rejects_transaction_commands() {
    let db = open();
    setup(&db, &[]);
    for sql in ["BEGIN", "COMMIT", "ROLLBACK", "begin transaction;", "Commit;", "rollback transaction"] {
        assert_eq!(txn_err(execute(&db, sql)), TransactionError::NeedsSession, "{sql}");
    }
    // And nothing was left open: the next one-shot statement commits at once.
    execute(&db, "INSERT INTO t VALUES (1, 1)").unwrap();
    assert_eq!(committed(&db), vec![(1, 1)]);
}

// ---------------------------------------------------------------- grammar

#[test]
fn transaction_keywords_are_case_insensitive_and_take_an_optional_transaction_word() {
    let db = open();
    setup(&db, &[]);
    let mut s = Session::new(&db);
    for (begin, end, kept) in [
        ("begin", "commit", true),
        ("Begin Transaction", "COMMIT TRANSACTION;", true),
        ("BEGIN;", "rollback", false),
        ("bEgIn TrAnSaCtIoN;", "Rollback Transaction;", false),
        ("BEGIN TRANSACTION", "commit;", true),
    ] {
        assert_eq!(s.execute(begin).unwrap(), Output::Done, "{begin}");
        assert!(s.in_transaction(), "{begin}");
        s.execute("INSERT INTO t VALUES (100, 0)").unwrap();
        assert_eq!(s.execute(end).unwrap(), Output::Done, "{end}");
        assert!(!s.in_transaction(), "{end}");
        assert_eq!(committed(&db).len(), usize::from(kept), "{begin} / {end}");
        execute(&db, "DELETE FROM t").unwrap();
    }
}

#[test]
fn transaction_variants_oakdb_does_not_support_are_rejected_without_touching_the_transaction() {
    let db = open();
    setup(&db, &[]);
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    for sql in [
        "START TRANSACTION",
        "END",
        "COMMIT WORK",
        "BEGIN IMMEDIATE",
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
        "SAVEPOINT a",
        "RELEASE a",
        "ROLLBACK TO a",
    ] {
        let r = s.execute(sql);
        assert!(
            matches!(r, Err(SqlError::Unsupported(_)) | Err(SqlError::Syntax { .. })),
            "{sql}: {r:?}"
        );
        assert!(s.in_transaction(), "{sql} must leave the transaction open");
    }
    assert_eq!(committed(&db), vec![]);
    s.execute("COMMIT").unwrap();
    assert_eq!(committed(&db), vec![(1, 1)]);
}

#[test]
fn a_transaction_command_and_a_statement_in_one_string_is_a_syntax_error() {
    let db = open();
    setup(&db, &[]);
    let mut s = Session::new(&db);
    let r = s.execute("BEGIN; INSERT INTO t VALUES (1, 1)");
    assert!(matches!(r, Err(SqlError::Syntax { .. })), "{r:?}");
    assert!(!s.in_transaction());
    assert_eq!(committed(&db), vec![]);
}

#[test]
fn transaction_is_not_reserved_and_can_name_a_table_and_a_column() {
    let db = open();
    let mut s = Session::new(&db);
    s.execute("CREATE TABLE transaction (id INTEGER PRIMARY KEY, transaction TEXT)").unwrap();
    s.execute("BEGIN TRANSACTION").unwrap();
    s.execute("INSERT INTO transaction VALUES (1, 'x')").unwrap();
    s.execute("COMMIT TRANSACTION").unwrap();
    assert_eq!(
        rows(execute(&db, "SELECT transaction FROM transaction WHERE id = 1").unwrap()),
        vec![vec![Value::Text("x".into())]]
    );
}

// ---------------------------------------------------------------- statement atomicity

/// BEGIN; INSERT (5); then a failing statement; the transaction keeps (5) and
/// nothing of the failed statement, stays open, and COMMIT writes exactly that.
fn failing_statement_inside_begin_is_undone_alone(sql: &str, check: fn(&SqlError) -> bool) {
    let db = open();
    setup(&db, &[(1, 1), (2, 2), (3, 4_611_686_018_427_387_904)]);
    let before = committed(&db);
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (5, 5)").unwrap();
    let in_txn = seen_by(&mut s);
    let r = s.execute(sql);
    match &r {
        Err(e) if check(e) => {}
        other => panic!("{sql}: unexpected {other:?}"),
    }
    assert!(s.in_transaction(), "{sql}: a statement error must not end the transaction");
    assert_eq!(seen_by(&mut s), in_txn, "{sql}: the failed statement left something");
    assert_eq!(committed(&db), before);
    s.execute("COMMIT").unwrap();
    let mut expected = before;
    expected.push((5, 5));
    assert_eq!(committed(&db), expected, "{sql}");
}

#[test]
fn a_multi_row_insert_hitting_an_existing_row_inside_begin_leaves_nothing_of_itself() {
    failing_statement_inside_begin_is_undone_alone("INSERT INTO t VALUES (8, 8), (1, 0)", |e| {
        matches!(e, SqlError::DuplicateKey { .. })
    });
}

#[test]
fn a_multi_row_insert_repeating_a_key_within_itself_inside_begin_leaves_nothing_of_itself() {
    failing_statement_inside_begin_is_undone_alone(
        "INSERT INTO t VALUES (8, 8), (9, 9), (8, 0)",
        |e| matches!(e, SqlError::DuplicateKey { .. }),
    );
}

#[test]
fn a_multi_row_insert_hitting_an_earlier_insert_of_the_same_transaction_leaves_nothing_of_itself() {
    failing_statement_inside_begin_is_undone_alone("INSERT INTO t VALUES (8, 8), (5, 0)", |e| {
        matches!(e, SqlError::DuplicateKey { .. })
    });
}

#[test]
fn a_multi_row_insert_with_a_bad_type_or_null_key_midway_inside_begin_leaves_nothing_of_itself() {
    failing_statement_inside_begin_is_undone_alone("INSERT INTO t VALUES (8, 8), (9, 'x')", |e| {
        matches!(e, SqlError::TypeMismatch { .. })
    });
    failing_statement_inside_begin_is_undone_alone("INSERT INTO t VALUES (8, 8), (NULL, 9)", |e| {
        matches!(e, SqlError::TypeMismatch { .. })
    });
    failing_statement_inside_begin_is_undone_alone("INSERT INTO t VALUES (8, 8), (9)", |e| {
        matches!(e, SqlError::ValueCount { .. })
    });
}

#[test]
fn an_update_overflowing_midway_inside_begin_leaves_nothing_of_itself() {
    failing_statement_inside_begin_is_undone_alone("UPDATE t SET n = n * 2", |e| {
        matches!(e, SqlError::Arithmetic(_))
    });
}

#[test]
fn an_update_moving_a_key_onto_another_row_inside_begin_leaves_nothing_of_itself() {
    failing_statement_inside_begin_is_undone_alone("UPDATE t SET id = id + 1 WHERE id < 3", |e| {
        matches!(e, SqlError::DuplicateKey { .. })
    });
}

#[test]
fn a_delete_failing_midway_inside_begin_leaves_nothing_of_itself() {
    // Rows 1 and 2 match (10 / 2 and 10 / 1 are positive), row 3 divides by zero.
    failing_statement_inside_begin_is_undone_alone("DELETE FROM t WHERE 10 / (3 - id) > 0", |e| {
        matches!(e, SqlError::Arithmetic(_))
    });
}

#[test]
fn ordinary_statement_errors_do_not_end_the_transaction() {
    let db = open();
    setup(&db, &[]);
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    for bad in [
        "INSERT INTO",
        "SELECT * FROM nope",
        "SELECT zz FROM t",
        "INSERT INTO t VALUES ('a', 1)",
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        "UPDATE t SET n = 'a' + 1",
    ] {
        assert!(s.execute(bad).is_err(), "{bad}");
        assert!(s.in_transaction(), "{bad}");
    }
    s.execute("INSERT INTO t VALUES (2, 2)").unwrap();
    assert_eq!(committed(&db), vec![]);
    s.execute("COMMIT").unwrap();
    assert_eq!(committed(&db), vec![(1, 1), (2, 2)]);
}

// ---------------------------------------------------------------- conflicts

#[test]
fn a_commit_that_conflicts_ends_the_transaction_and_writes_nothing() {
    let db = open();
    setup(&db, &[(1, 10)]);
    let mut a = Session::new(&db);
    a.execute("BEGIN").unwrap();
    a.execute("UPDATE t SET n = 11 WHERE id = 1").unwrap();
    a.execute("INSERT INTO t VALUES (9, 9)").unwrap();
    // Another session changes the same row and commits first.
    execute(&db, "UPDATE t SET n = 20 WHERE id = 1").unwrap();
    let e = a.execute("COMMIT").unwrap_err();
    assert!(is_conflict(&e), "expected Conflict, got {e:?}");
    assert!(!a.in_transaction(), "a failed COMMIT still ends the transaction");
    assert_eq!(committed(&db), vec![(1, 20)]);
    assert_eq!(txn_err(a.execute("ROLLBACK")), TransactionError::NotBegun);
    // Back in autocommit: the next statement commits on its own.
    a.execute("INSERT INTO t VALUES (9, 9)").unwrap();
    assert_eq!(committed(&db), vec![(1, 20), (9, 9)]);
}

#[test]
fn the_same_key_inserted_by_two_open_transactions_conflicts_at_the_later_commit() {
    let db = open();
    setup(&db, &[]);
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();
    a.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    b.execute("INSERT INTO t VALUES (1, 2)").unwrap();
    a.execute("COMMIT").unwrap();
    let e = b.execute("COMMIT").unwrap_err();
    assert!(is_conflict(&e), "{e:?}");
    assert!(!b.in_transaction());
    assert_eq!(committed(&db), vec![(1, 1)]);
}

#[test]
fn two_transactions_creating_tables_at_once_conflict_at_the_later_commit() {
    for (first, second) in [("x", "y"), ("same", "same")] {
        let db = open();
        let mut a = Session::new(&db);
        let mut b = Session::new(&db);
        a.execute("BEGIN").unwrap();
        b.execute("BEGIN").unwrap();
        a.execute(&format!("CREATE TABLE {first} (id INTEGER PRIMARY KEY)")).unwrap();
        b.execute(&format!("CREATE TABLE {second} (id INTEGER PRIMARY KEY)")).unwrap();
        b.execute(&format!("INSERT INTO {second} VALUES (1)")).unwrap();
        a.execute("COMMIT").unwrap();
        let e = b.execute("COMMIT").unwrap_err();
        assert!(is_conflict(&e), "{first}/{second}: {e:?}");
        assert!(!b.in_transaction());
        assert!(execute(&db, &format!("SELECT * FROM {first}")).is_ok());
        if first != second {
            assert!(matches!(
                execute(&db, &format!("SELECT * FROM {second}")),
                Err(SqlError::NoSuchTable(_))
            ));
        } else {
            assert_eq!(rows(execute(&db, "SELECT * FROM same").unwrap()), Vec::<Vec<Value>>::new());
        }
    }
}

// ---------------------------------------------------------------- reads inside a transaction

#[test]
fn reads_inside_a_transaction_see_own_writes_over_the_begin_snapshot_and_not_later_commits() {
    let db = open();
    setup(&db, &[(1, 10), (2, 20)]);
    let mut a = Session::new(&db);
    a.execute("BEGIN").unwrap();
    // Someone else commits after BEGIN.
    execute(&db, "INSERT INTO t VALUES (3, 30)").unwrap();
    execute(&db, "UPDATE t SET n = 21 WHERE id = 2").unwrap();
    assert_eq!(seen_by(&mut a), vec![(1, 10), (2, 20)]);
    a.execute("INSERT INTO t VALUES (4, 40)").unwrap();
    a.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(seen_by(&mut a), vec![(2, 20), (4, 40)]);
    assert_eq!(
        rows(a.execute("SELECT n FROM t WHERE id = 4").unwrap()),
        vec![vec![int(40)]]
    );
    assert_eq!(rows(a.execute("SELECT n FROM t WHERE id = 3").unwrap()), Vec::<Vec<Value>>::new());
    // Repeatable read.
    execute(&db, "INSERT INTO t VALUES (5, 50)").unwrap();
    assert_eq!(seen_by(&mut a), vec![(2, 20), (4, 40)]);
    a.execute("ROLLBACK").unwrap();
    assert_eq!(seen_by(&mut a), vec![(1, 10), (2, 21), (3, 30), (5, 50)]);
}

#[test]
fn a_table_another_session_creates_after_begin_is_not_seen_inside_the_transaction() {
    let db = open();
    let mut a = Session::new(&db);
    a.execute("BEGIN").unwrap();
    execute(&db, "CREATE TABLE later (id INTEGER PRIMARY KEY)").unwrap();
    assert!(matches!(a.execute("SELECT * FROM later"), Err(SqlError::NoSuchTable(_))));
    assert!(a.in_transaction());
    a.execute("COMMIT").unwrap();
    assert!(a.execute("SELECT * FROM later").is_ok());
}

// ---------------------------------------------------------------- dropping a session

#[test]
fn dropping_a_session_with_an_open_transaction_rolls_it_back() {
    let db = open();
    setup(&db, &[(1, 1)]);
    {
        let mut s = Session::new(&db);
        s.execute("BEGIN").unwrap();
        s.execute("INSERT INTO t VALUES (2, 2)").unwrap();
        s.execute("UPDATE t SET n = 100 WHERE id = 1").unwrap();
        s.execute("CREATE TABLE gone (id INTEGER PRIMARY KEY)").unwrap();
    }
    assert_eq!(committed(&db), vec![(1, 1)]);
    assert!(matches!(execute(&db, "SELECT * FROM gone"), Err(SqlError::NoSuchTable(_))));
    // The dropped transaction no longer counts in conflict checks: writing the
    // same keys now, in a transaction begun after the drop, commits.
    let mut s = Session::new(&db);
    s.execute("BEGIN").unwrap();
    s.execute("UPDATE t SET n = 7 WHERE id = 1").unwrap();
    s.execute("INSERT INTO t VALUES (2, 3)").unwrap();
    s.execute("CREATE TABLE gone (id INTEGER PRIMARY KEY)").unwrap();
    s.execute("COMMIT").unwrap();
    assert_eq!(committed(&db), vec![(1, 7), (2, 3)]);
}

// ---------------------------------------------------------------- threads

#[test]
fn sessions_on_threads_incrementing_a_counter_with_retries_lose_no_update() {
    const THREADS: usize = 4;
    const PER_THREAD: i64 = 25;
    let db = open();
    setup(&db, &[(1, 0)]);
    let retries = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    let mut s = Session::new(&db);
                    let mut retries = 0u64;
                    for _ in 0..PER_THREAD {
                        loop {
                            s.execute("BEGIN").unwrap();
                            let n = match rows(s.execute("SELECT n FROM t WHERE id = 1").unwrap())
                                .as_slice()
                            {
                                [row] => match row.as_slice() {
                                    [Value::Integer(n)] => *n,
                                    other => panic!("{other:?}"),
                                },
                                other => panic!("{other:?}"),
                            };
                            // Write the value read, not `n + 1` in SQL, so a lost
                            // update would really show.
                            s.execute(&format!("UPDATE t SET n = {} WHERE id = 1", n + 1))
                                .unwrap();
                            match s.execute("COMMIT") {
                                Ok(out) => {
                                    assert_eq!(out, Output::Done);
                                    break;
                                }
                                Err(e) if is_conflict(&e) => {
                                    assert!(!s.in_transaction());
                                    retries += 1;
                                }
                                Err(e) => panic!("{e:?}"),
                            }
                        }
                    }
                    retries
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum::<u64>()
    });
    let _ = retries;
    assert_eq!(committed(&db), vec![(1, THREADS as i64 * PER_THREAD)]);
}

// ---------------------------------------------------------------- model comparison

const BIG: i64 = 4_611_686_018_427_387_904; // 2^62: doubling it overflows i64

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dup,
    Arith,
    AlreadyBegun,
    NotBegun,
}

fn kind_of(e: &SqlError) -> Option<Kind> {
    match e {
        SqlError::DuplicateKey { .. } => Some(Kind::Dup),
        SqlError::Arithmetic(_) => Some(Kind::Arith),
        SqlError::Transaction(TransactionError::AlreadyBegun) => Some(Kind::AlreadyBegun),
        SqlError::Transaction(TransactionError::NotBegun) => Some(Kind::NotBegun),
        _ => None,
    }
}

type Table = BTreeMap<i64, i64>;

struct Model {
    committed: Table,
    work: Option<Table>,
}

impl Model {
    fn view(&self) -> &Table {
        self.work.as_ref().unwrap_or(&self.committed)
    }

    /// Runs a write against the current view: on success it is applied
    /// (committed at once outside a transaction), on error nothing changes.
    fn write(&mut self, f: impl FnOnce(&Table) -> Result<(Table, u64), Kind>) -> Result<Output, Kind> {
        let (next, n) = f(self.view())?;
        match &mut self.work {
            Some(w) => *w = next,
            None => self.committed = next,
        }
        Ok(Output::Affected(n))
    }
}

fn gen_stmt(r: &mut Rng) -> String {
    let id = |r: &mut Rng| r.below(10) as i64;
    match r.below(100) {
        0..=9 => "BEGIN".into(),
        10..=17 => "COMMIT".into(),
        18..=23 => "ROLLBACK".into(),
        24..=43 => {
            let count = 1 + r.below(4);
            let values: Vec<String> = (0..count)
                .map(|_| {
                    let n = if r.below(8) == 0 { BIG } else { r.below(100) as i64 };
                    format!("({}, {})", id(r), n)
                })
                .collect();
            format!("INSERT INTO t VALUES {}", values.join(", "))
        }
        44..=53 => format!("UPDATE t SET n = n * 2 WHERE id >= {}", id(r)),
        54..=61 => format!("UPDATE t SET n = n + 1 WHERE id = {}", id(r)),
        62..=71 => format!("UPDATE t SET id = id + {} WHERE id <= {}", 1 + r.below(3), id(r)),
        72..=79 => format!("DELETE FROM t WHERE id = {}", id(r)),
        80..=85 => format!("DELETE FROM t WHERE id < {}", id(r)),
        86..=91 => format!("DELETE FROM t WHERE 10 / (id - {}) > 0", id(r)),
        _ => "SELECT * FROM t".into(),
    }
}

fn nums(s: &str) -> Vec<i64> {
    s.split(|c: char| !(c.is_ascii_digit()))
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap())
        .collect()
}

fn model_step(m: &mut Model, sql: &str) -> Result<Output, Kind> {
    if sql == "BEGIN" {
        if m.work.is_some() {
            return Err(Kind::AlreadyBegun);
        }
        m.work = Some(m.committed.clone());
        return Ok(Output::Done);
    }
    if sql == "COMMIT" {
        let w = m.work.take().ok_or(Kind::NotBegun)?;
        m.committed = w;
        return Ok(Output::Done);
    }
    if sql == "ROLLBACK" {
        m.work.take().ok_or(Kind::NotBegun)?;
        return Ok(Output::Done);
    }
    if sql.starts_with("SELECT") {
        let rows = m.view().iter().map(|(k, v)| vec![int(*k), int(*v)]).collect();
        return Ok(Output::Rows { columns: vec!["id".into(), "n".into()], rows });
    }
    let x = nums(sql);
    if sql.starts_with("INSERT") {
        return m.write(|t| {
            let mut next = t.clone();
            let mut n = 0;
            for pair in x.chunks(2) {
                if next.insert(pair[0], pair[1]).is_some() {
                    return Err(Kind::Dup);
                }
                n += 1;
            }
            Ok((next, n))
        });
    }
    if sql.starts_with("UPDATE t SET n = n * 2") {
        let a = x[1];
        return m.write(|t| {
            let mut next = t.clone();
            let mut n = 0;
            for (_, v) in next.range_mut(a..) {
                *v = v.checked_mul(2).ok_or(Kind::Arith)?;
                n += 1;
            }
            Ok((next, n))
        });
    }
    if sql.starts_with("UPDATE t SET n = n + 1") {
        let a = x[1];
        return m.write(|t| {
            let mut next = t.clone();
            let n = match next.get_mut(&a) {
                Some(v) => {
                    *v = v.checked_add(1).ok_or(Kind::Arith)?;
                    1
                }
                None => 0,
            };
            Ok((next, n))
        });
    }
    if sql.starts_with("UPDATE t SET id") {
        let (k, a) = (x[0], x[1]);
        return m.write(|t| {
            let moved: Vec<(i64, i64)> = t.range(..=a).map(|(id, v)| (id + k, *v)).collect();
            let mut next: Table = t.range(a + 1..).map(|(id, v)| (*id, *v)).collect();
            for (id, v) in &moved {
                if next.insert(*id, *v).is_some() {
                    return Err(Kind::Dup);
                }
            }
            Ok((next, moved.len() as u64))
        });
    }
    if sql.starts_with("DELETE FROM t WHERE id = ") {
        let a = x[0];
        return m.write(|t| {
            let mut next = t.clone();
            let n = u64::from(next.remove(&a).is_some());
            Ok((next, n))
        });
    }
    if sql.starts_with("DELETE FROM t WHERE id < ") {
        let a = x[0];
        return m.write(|t| {
            let next: Table = t.range(a..).map(|(k, v)| (*k, *v)).collect();
            Ok((next.clone(), (t.len() - next.len()) as u64))
        });
    }
    if sql.starts_with("DELETE FROM t WHERE 10 /") {
        let a = x[1];
        return m.write(|t| {
            if t.contains_key(&a) {
                return Err(Kind::Arith);
            }
            // Integer division truncates toward zero.
            let next: Table =
                t.iter().filter(|(k, _)| 10 / (**k - a) <= 0).map(|(k, v)| (*k, *v)).collect();
            Ok((next.clone(), (t.len() - next.len()) as u64))
        });
    }
    unreachable!("{sql}");
}

#[test]
fn random_transaction_sequences_match_an_independent_model_after_every_statement() {
    for seed in 1..=60u64 {
        let db = open();
        setup(&db, &[]);
        let mut s = Session::new(&db);
        let mut m = Model { committed: Table::new(), work: None };
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut history = Vec::new();
        for _ in 0..250 {
            let sql = gen_stmt(&mut r);
            history.push(sql.clone());
            let ctx = || format!("seed {seed}, history:\n{}", history.join("\n"));
            let expected = model_step(&mut m, &sql);
            let got = s.execute(&sql);
            match (&expected, &got) {
                (Ok(e), Ok(g)) => assert_eq!(e, g, "{}", ctx()),
                (Err(e), Err(g)) => assert_eq!(Some(*e), kind_of(g), "got {g:?}; {}", ctx()),
                _ => panic!("expected {expected:?}, got {got:?}; {}", ctx()),
            }
            assert_eq!(s.in_transaction(), m.work.is_some(), "{}", ctx());
            // What the session sees: own writes over the snapshot, or committed data.
            let view: Vec<(i64, i64)> = m.view().iter().map(|(k, v)| (*k, *v)).collect();
            assert_eq!(seen_by(&mut s), view, "session view; {}", ctx());
            // What everyone else sees: committed data only.
            let done: Vec<(i64, i64)> = m.committed.iter().map(|(k, v)| (*k, *v)).collect();
            assert_eq!(committed(&db), done, "committed view; {}", ctx());
        }
    }
}
