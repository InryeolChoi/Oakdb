//! UPDATE and DELETE (issue #33, design 0022), seen only through the SQL
//! strings they run and the rows they leave.

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, ColumnType, Output, SqlError, Value};

fn new_db() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/oak").unwrap()
}

fn run(db: &Db<SimFs>, sql: &str) -> Output {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn fail(db: &Db<SimFs>, sql: &str) -> SqlError {
    match execute(db, sql) {
        Ok(out) => panic!("{sql}: expected an error, got {out:?}"),
        Err(e) => e,
    }
}

fn affected(db: &Db<SimFs>, sql: &str) -> u64 {
    match run(db, sql) {
        Output::Affected(n) => n,
        other => panic!("{sql}: expected a row count, got {other:?}"),
    }
}

fn rows(db: &Db<SimFs>, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

fn int(i: i64) -> Value {
    Value::Integer(i)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

/// `t (id INTEGER PRIMARY KEY, n INTEGER, name TEXT)` holding `(id, n,
/// name)` for each row given.
fn table(given: &[(i64, Option<i64>, &str)]) -> Db<SimFs> {
    let db = new_db();
    run(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, name TEXT)",
    );
    for (id, n, name) in given {
        let n = n.map_or("NULL".to_owned(), |n| n.to_string());
        run(&db, &format!("INSERT INTO t VALUES ({id}, {n}, '{name}')"));
    }
    db
}

fn all(db: &Db<SimFs>) -> Vec<Vec<Value>> {
    rows(db, "SELECT * FROM t")
}

fn ids(db: &Db<SimFs>) -> Vec<i64> {
    rows(db, "SELECT id FROM t")
        .into_iter()
        .map(|row| match row[..] {
            [Value::Integer(id)] => id,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

// ------------------------------------------------------------ the issue

#[test]
fn update_changes_the_rows_where_matches_and_counts_them() {
    let db = table(&[(1, Some(10), "a"), (2, Some(20), "b"), (3, Some(30), "c")]);
    assert_eq!(affected(&db, "UPDATE t SET name = 'x' WHERE n >= 20"), 2);
    assert_eq!(
        all(&db),
        vec![
            vec![int(1), int(10), text("a")],
            vec![int(2), int(20), text("x")],
            vec![int(3), int(30), text("x")],
        ]
    );
}

#[test]
fn delete_removes_the_rows_where_matches_and_counts_them() {
    let db = table(&[(1, Some(10), "a"), (2, Some(20), "b"), (3, Some(30), "c")]);
    assert_eq!(affected(&db, "DELETE FROM t WHERE name = 'b' OR id = 3"), 2);
    assert_eq!(all(&db), vec![vec![int(1), int(10), text("a")]]);
    assert_eq!(affected(&db, "DELETE FROM t WHERE id = 99"), 0);
    assert_eq!(affected(&db, "UPDATE t SET n = 0 WHERE id = 99"), 0);
}

#[test]
fn where_follows_three_valued_logic_so_null_rows_stay() {
    // `NOT (n = 1)` is unknown on a NULL n: the row is neither changed
    // nor deleted (design 0019, section 2).
    let db = table(&[(1, Some(1), "a"), (2, None, "b"), (3, Some(3), "c")]);
    assert_eq!(affected(&db, "DELETE FROM t WHERE NOT (n = 1)"), 1);
    assert_eq!(ids(&db), vec![1, 2]);
}

#[test]
fn update_and_delete_without_where_take_every_row() {
    let db = table(&[(1, Some(1), "a"), (2, None, "b"), (3, Some(3), "c")]);
    assert_eq!(affected(&db, "UPDATE t SET n = 0"), 3);
    assert_eq!(
        rows(&db, "SELECT n FROM t"),
        vec![vec![int(0)], vec![int(0)], vec![int(0)]]
    );
    assert_eq!(affected(&db, "DELETE FROM t;"), 3);
    assert!(all(&db).is_empty());
    assert_eq!(affected(&db, "DELETE FROM t"), 0);
    assert_eq!(affected(&db, "UPDATE t SET n = 1"), 0);
}

#[test]
fn the_count_is_of_rows_matched_even_when_nothing_changes() {
    let db = table(&[(1, Some(5), "a"), (2, Some(5), "b"), (3, Some(6), "c")]);
    assert_eq!(affected(&db, "UPDATE t SET n = 5"), 3);
    assert_eq!(affected(&db, "UPDATE t SET n = n"), 3);
}

// ------------------------------------------------------------ U3: Halloween

#[test]
fn moving_rows_forward_changes_each_row_once() {
    // Rows 0..29; each moves 10 ahead, onto keys that rows still to be
    // read stood on. Scanning while writing would meet row 0 again at 10,
    // then at 20, and so on.
    let db = new_db();
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, was INTEGER)");
    for id in 0..30 {
        run(&db, &format!("INSERT INTO t VALUES ({id}, {id})"));
    }
    assert_eq!(affected(&db, "UPDATE t SET id = id + 10 WHERE id < 30"), 30);
    let got = rows(&db, "SELECT id, was FROM t");
    let expected: Vec<Vec<Value>> = (0..30).map(|was| vec![int(was + 10), int(was)]).collect();
    assert_eq!(got, expected);
}

// ------------------------------------------------------------ U5, U6: primary key

#[test]
fn changing_the_primary_key_moves_the_row() {
    let db = table(&[(1, Some(10), "a"), (2, Some(20), "b")]);
    assert_eq!(affected(&db, "UPDATE t SET id = 100 WHERE id = 1"), 1);
    assert!(rows(&db, "SELECT * FROM t WHERE id = 1").is_empty());
    assert_eq!(
        rows(&db, "SELECT * FROM t WHERE id = 100"),
        vec![vec![int(100), int(10), text("a")]]
    );
    assert_eq!(ids(&db), vec![2, 100]);
    // The old key is free again.
    run(&db, "INSERT INTO t VALUES (1, 0, 'new')");
    assert_eq!(ids(&db), vec![1, 2, 100]);
}

#[test]
fn a_text_primary_key_moves_too() {
    let db = new_db();
    run(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, c TEXT)");
    run(&db, "INSERT INTO s VALUES ('a', 'z'), ('b', 'y')");
    assert_eq!(affected(&db, "UPDATE s SET k = c"), 2);
    assert_eq!(
        rows(&db, "SELECT * FROM s"),
        vec![vec![text("y"), text("y")], vec![text("z"), text("z")]]
    );
}

#[test]
fn shifting_consecutive_keys_succeeds_either_way() {
    // Checked against the table after the whole statement (U6): 1, 2, 3
    // become 2, 3, 4 although 2 and 3 are taken until their own rows move.
    let db = table(&[(1, Some(1), "a"), (2, Some(2), "b"), (3, Some(3), "c")]);
    assert_eq!(affected(&db, "UPDATE t SET id = id + 1"), 3);
    assert_eq!(ids(&db), vec![2, 3, 4]);
    assert_eq!(affected(&db, "UPDATE t SET id = id - 1"), 3);
    assert_eq!(ids(&db), vec![1, 2, 3]);
    assert_eq!(
        rows(&db, "SELECT n FROM t"),
        vec![vec![int(1)], vec![int(2)], vec![int(3)]]
    );
    // Swapping two keys through a third value works as well.
    assert_eq!(affected(&db, "UPDATE t SET id = 4 - id"), 3);
    assert_eq!(
        rows(&db, "SELECT id, n FROM t"),
        vec![
            vec![int(1), int(3)],
            vec![int(2), int(2)],
            vec![int(3), int(1)]
        ]
    );
}

#[test]
fn a_real_duplicate_key_fails_and_changes_nothing() {
    for sql in [
        // Two new keys of the same statement meet.
        "UPDATE t SET id = 0",
        // A row the statement does not move holds the new key.
        "UPDATE t SET id = id + 1 WHERE id < 3",
        // A matched row that keeps its key holds it: 0 stays, 3 -> 0.
        "UPDATE t SET id = id * 0 WHERE id = 0 OR id = 3",
    ] {
        let db = table(&[
            (0, Some(0), "z"),
            (1, Some(1), "a"),
            (2, Some(2), "b"),
            (3, Some(3), "c"),
        ]);
        let before = all(&db);
        assert!(
            matches!(fail(&db, sql), SqlError::DuplicateKey { ref table } if table == "t"),
            "{sql}"
        );
        assert_eq!(all(&db), before, "{sql}");
    }
}

// ------------------------------------------------------------ U7: old values

#[test]
fn every_set_expression_reads_the_old_row() {
    let db = new_db();
    run(
        &db,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
    );
    run(&db, "INSERT INTO p VALUES (1, 1, 2)");
    assert_eq!(affected(&db, "UPDATE p SET a = b, b = a"), 1);
    assert_eq!(rows(&db, "SELECT a, b FROM p"), vec![vec![int(2), int(1)]]);
    assert_eq!(affected(&db, "UPDATE p SET a = a + 10, b = a, id = a"), 1);
    assert_eq!(
        rows(&db, "SELECT * FROM p"),
        vec![vec![int(2), int(12), int(2)]]
    );
}

// ------------------------------------------------------------ U8: types

#[test]
fn type_errors_fail_before_any_row_is_read() {
    // On an empty table: no row could have caused them.
    let db = table(&[]);
    type Check = fn(&SqlError) -> bool;
    let cases: Vec<(&str, Check)> = vec![
        (
            "UPDATE t SET n = 'x'",
            |e| matches!(e, SqlError::TypeMismatch { column, expected: ColumnType::Integer, found: Some(ColumnType::Text) } if column == "n"),
        ),
        (
            "UPDATE t SET name = id + 1",
            |e| matches!(e, SqlError::TypeMismatch { column, expected: ColumnType::Text, found: Some(ColumnType::Integer) } if column == "name"),
        ),
        (
            "UPDATE t SET id = NULL",
            // Issue #34 (design 0027 E3): NullKey, was TypeMismatch { found: None }.
            |e| matches!(e, SqlError::NullKey { column } if column == "id"),
        ),
        (
            "UPDATE t SET nope = 1",
            |e| matches!(e, SqlError::NoSuchColumn { column, .. } if column == "nope"),
        ),
        (
            "UPDATE t SET n = nope + 1",
            |e| matches!(e, SqlError::NoSuchColumn { column, .. } if column == "nope"),
        ),
        ("UPDATE t SET n = (n = 1)", |e| {
            matches!(e, SqlError::Type(_))
        }),
        ("UPDATE t SET n = name + 1", |e| {
            matches!(e, SqlError::Type(_))
        }),
        (
            "UPDATE t SET n = 1, n = 2",
            |e| matches!(e, SqlError::DuplicateColumn(c) if c == "n"),
        ),
        ("UPDATE t SET n = 1 WHERE name = 1", |e| {
            matches!(e, SqlError::TypeMismatch { .. })
        }),
        ("DELETE FROM t WHERE n = 'x'", |e| {
            matches!(e, SqlError::TypeMismatch { .. })
        }),
        ("DELETE FROM t WHERE n", |e| matches!(e, SqlError::Type(_))),
        (
            "UPDATE nope SET n = 1",
            |e| matches!(e, SqlError::NoSuchTable(t) if t == "nope"),
        ),
        (
            "DELETE FROM nope",
            |e| matches!(e, SqlError::NoSuchTable(t) if t == "nope"),
        ),
    ];
    for (sql, expected) in cases {
        let e = fail(&db, sql);
        assert!(expected(&e), "{sql}: {e:?}");
    }
}

#[test]
fn a_null_primary_key_found_on_a_row_fails_and_changes_nothing() {
    let db = table(&[(1, Some(1), "a"), (2, None, "b")]);
    let before = all(&db);
    assert!(matches!(
        fail(&db, "UPDATE t SET id = n"),
        // Issue #34 (design 0027 E3): NullKey, was TypeMismatch { found: None }.
        SqlError::NullKey { ref column } if column == "id"
    ));
    assert_eq!(all(&db), before);
    // NULL in another column is fine.
    assert_eq!(affected(&db, "UPDATE t SET n = NULL WHERE id = 1"), 1);
}

// ------------------------------------------------------------ U10: atomicity

#[test]
fn an_error_midway_applies_nothing() {
    // Rows 1 and 2 are fine; row 3 overflows.
    let db = table(&[
        (1, Some(1), "a"),
        (2, Some(2), "b"),
        (3, Some(i64::MAX), "c"),
    ]);
    let before = all(&db);
    for sql in [
        "UPDATE t SET n = n + 1",
        "UPDATE t SET name = 'x', id = id + 10, n = n * 2",
        "DELETE FROM t WHERE n + 1 > 0",
        "DELETE FROM t WHERE 10 / (n - 2) > 0",
    ] {
        assert!(matches!(fail(&db, sql), SqlError::Arithmetic(_)), "{sql}");
        assert_eq!(all(&db), before, "{sql}");
    }
}

// ------------------------------------------------------------ U1: grammar

#[test]
fn forms_oakdb_does_not_support_say_so() {
    let db = table(&[(1, Some(1), "a")]);
    for sql in [
        "UPDATE OR REPLACE t SET n = 1",
        "UPDATE t SET (n, name) = (1, 'x')",
        "UPDATE t SET n = DEFAULT",
        "UPDATE t SET n = 1 FROM t",
        "UPDATE t SET n = 1 RETURNING n",
        "UPDATE t SET n = 1 WHERE id = 1 ORDER BY id",
        "DELETE t WHERE id = 1",
        "DELETE FROM t LIMIT 1",
        "DELETE FROM t WHERE id = 1 RETURNING id",
        "UPDATE t SET n = 1; DELETE FROM t",
    ] {
        assert!(matches!(fail(&db, sql), SqlError::Unsupported(_)), "{sql}");
    }
    assert_eq!(all(&db), vec![vec![int(1), int(1), text("a")]]);
}

#[test]
fn broken_statements_are_syntax_errors() {
    let db = table(&[(1, Some(1), "a")]);
    for sql in [
        "UPDATE",
        "UPDATE t",
        "UPDATE t SET",
        "UPDATE t SET n",
        "UPDATE t SET n 1",
        "UPDATE t SET n = ",
        "UPDATE t SET n = 1,",
        "UPDATE t SET n = 1 WHERE",
        "DELETE",
        "DELETE FROM",
        "DELETE FROM t WHERE",
        "UPDATE t SET set = 1",
    ] {
        assert!(
            matches!(fail(&db, sql), SqlError::Syntax { .. }),
            "{sql}: {:?}",
            execute(&db, sql)
        );
    }
}

#[test]
fn update_set_and_delete_are_keywords() {
    let db = new_db();
    for name in ["update", "SET", "delete"] {
        assert!(
            matches!(
                fail(
                    &db,
                    &format!("CREATE TABLE {name} (id INTEGER PRIMARY KEY)")
                ),
                SqlError::Syntax { .. }
            ),
            "{name}"
        );
    }
}

// ------------------------------------------------------------ U11: concurrency

#[test]
fn concurrent_increments_conflict_instead_of_losing_updates() {
    let db = table(&[(1, Some(0), "a")]);
    const THREADS: i64 = 4;
    const EACH: i64 = 25;
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..EACH {
                    // Retry on a conflict, as a client would (U11).
                    loop {
                        match execute(&db, "UPDATE t SET n = n + 1 WHERE id = 1") {
                            Ok(Output::Affected(1)) => break,
                            Err(SqlError::Engine(oakdb::Error::Conflict { .. })) => continue,
                            other => panic!("{other:?}"),
                        }
                    }
                }
            });
        }
    });
    assert_eq!(
        rows(&db, "SELECT n FROM t"),
        vec![vec![int(THREADS * EACH)]]
    );
}

#[test]
fn update_and_delete_survive_reopening() {
    let fs = SimFs::new();
    {
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
        run(&db, "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)");
        run(&db, "UPDATE t SET id = id + 10, n = n * 100 WHERE id >= 2");
        run(&db, "DELETE FROM t WHERE id = 1");
        db.close().unwrap();
    }
    let db = Db::open_with(fs, "/oak").unwrap();
    assert_eq!(
        rows(&db, "SELECT * FROM t"),
        vec![vec![int(12), int(200)], vec![int(13), int(300)]]
    );
}
