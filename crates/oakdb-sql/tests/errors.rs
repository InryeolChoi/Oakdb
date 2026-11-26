//! Which kind of error each statement fails with (issue #34, design 0027).
//!
//! One table drives the main test: a statement and the kind it must fail
//! with. The table holds every row of design 0027's "지금 상태" table and
//! one statement per error group, so a statement that drifts from one kind
//! to another fails here by name.

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, Session, SqlError, TransactionError};

fn new_db() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/oak").unwrap()
}

/// `t (id INTEGER PRIMARY KEY, name TEXT, data BLOB)` holding the row with
/// id 1.
fn with_table() -> Db<SimFs> {
    let db = new_db();
    execute(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, data BLOB)",
    )
    .unwrap();
    execute(&db, "INSERT INTO t VALUES (1, 'a', NULL)").unwrap();
    db
}

/// The kinds of [`SqlError`], by name, for the table below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Syntax,
    Unsupported,
    NoSuchTable,
    NoSuchColumn,
    TableExists,
    DuplicateColumn,
    ValueCount,
    DuplicateKey,
    TypeMismatch,
    Type,
    Arithmetic,
    NullKey,
    Transaction,
    KeyTooLarge,
}

fn kind(e: &SqlError) -> Kind {
    match e {
        SqlError::Syntax { .. } => Kind::Syntax,
        SqlError::Unsupported(_) => Kind::Unsupported,
        SqlError::NoSuchTable(_) => Kind::NoSuchTable,
        SqlError::NoSuchColumn { .. } => Kind::NoSuchColumn,
        SqlError::TableExists(_) => Kind::TableExists,
        SqlError::DuplicateColumn(_) => Kind::DuplicateColumn,
        SqlError::ValueCount { .. } => Kind::ValueCount,
        SqlError::DuplicateKey { .. } => Kind::DuplicateKey,
        SqlError::TypeMismatch { .. } => Kind::TypeMismatch,
        SqlError::Type(_) => Kind::Type,
        SqlError::Arithmetic(_) => Kind::Arithmetic,
        SqlError::NullKey { .. } => Kind::NullKey,
        SqlError::Transaction(_) => Kind::Transaction,
        SqlError::Engine(oakdb::Error::KeyTooLarge { .. }) => Kind::KeyTooLarge,
        other => panic!("a kind this table does not expect: {other:?}"),
    }
}

/// Each statement, run on [`with_table`], and the kind it fails with.
const TABLE: &[(&str, Kind)] = &[
    // Design 0027, "지금 상태", in its order.
    ("INSERT INTO t VALUES ('x', 'a', NULL)", Kind::TypeMismatch),
    ("SELECT * FROM t WHERE name = 1", Kind::TypeMismatch),
    ("SELECT * FROM t WHERE id + 1 = 'a'", Kind::Type),
    ("SELECT * FROM t WHERE name + 1 = 2", Kind::Type),
    ("SELECT * FROM t WHERE id", Kind::Type),
    (
        "UPDATE t SET id = id + 9223372036854775807",
        Kind::Arithmetic,
    ),
    ("SELECT * FROM t WHERE 1 / 0 = 1", Kind::Arithmetic),
    (
        "SELECT * FROM t WHERE id = 99999999999999999999",
        Kind::Arithmetic,
    ),
    ("INSERT INTO t VALUES (NULL, 'a', NULL)", Kind::NullKey),
    ("BEGIN", Kind::Transaction),
    ("INSERT INTO t VALUES (1, 'b', NULL)", Kind::DuplicateKey),
    (
        "CREATE TABLE IF NOT EXISTS u (id INTEGER PRIMARY KEY)",
        Kind::Unsupported,
    ),
    ("SELECT COUNT(*) FROM t", Kind::Unsupported),
    ("SELECT t.id FROM t", Kind::Unsupported),
    ("-- a comment\nSELECT * FROM t", Kind::Unsupported),
    (
        "CREATE TABLE v (id FROBNICATE PRIMARY KEY)",
        Kind::Unsupported,
    ),
    ("SELECT * FROM t\nWHERE id = = 1", Kind::Syntax),
    ("SELEC * FROM t", Kind::Unsupported),
    // One more per group.
    ("SELECT * FORM t", Kind::Syntax),
    ("SELECT * FROM t WHERE (id = 1", Kind::Syntax),
    ("INSERT INTO t VALUES ('a)", Kind::Syntax),
    ("DROP TABLE t", Kind::Unsupported),
    ("SELECT * FROM t ORDER BY id", Kind::Unsupported),
    ("SELECT * FROM t WHERE name LIKE 'a%'", Kind::Unsupported),
    ("SELECT * FROM nope", Kind::NoSuchTable),
    ("SELECT nope FROM t", Kind::NoSuchColumn),
    ("SELECT * FROM t WHERE nope = 1", Kind::NoSuchColumn),
    ("CREATE TABLE t (id INTEGER PRIMARY KEY)", Kind::TableExists),
    (
        "CREATE TABLE u (a INTEGER PRIMARY KEY, a TEXT)",
        Kind::DuplicateColumn,
    ),
    (
        "INSERT INTO t (id, id) VALUES (2, 2)",
        Kind::DuplicateColumn,
    ),
    ("INSERT INTO t VALUES (2, 'b')", Kind::ValueCount),
    (
        "INSERT INTO t VALUES (2, 'b', NULL), (2, 'c', NULL)",
        Kind::DuplicateKey,
    ),
    ("UPDATE t SET name = 1", Kind::TypeMismatch),
    (
        "INSERT INTO t VALUES (2, 'b', 'not a blob')",
        Kind::TypeMismatch,
    ),
    ("SELECT * FROM t WHERE data = 'a'", Kind::TypeMismatch),
    ("SELECT * FROM t WHERE 'a' = 1", Kind::Type),
    ("SELECT * FROM t WHERE -name = 1", Kind::Type),
    ("SELECT * FROM t WHERE NOT id", Kind::Type),
    ("SELECT * FROM t WHERE id = (1 = 1)", Kind::Type),
    ("SELECT * FROM t WHERE id % 0 = 1", Kind::Arithmetic),
    (
        "SELECT * FROM t WHERE id * 9223372036854775807 * 2 = 1",
        Kind::Arithmetic,
    ),
    (
        "INSERT INTO t VALUES (99999999999999999999, 'a', NULL)",
        Kind::Arithmetic,
    ),
    (
        "INSERT INTO t VALUES (-9223372036854775809, 'a', NULL)",
        Kind::Arithmetic,
    ),
    ("UPDATE t SET id = NULL", Kind::NullKey),
    ("UPDATE t SET id = id + NULL", Kind::NullKey),
    ("INSERT INTO t (name) VALUES ('b')", Kind::NullKey),
    ("COMMIT", Kind::Transaction),
    ("ROLLBACK", Kind::Transaction),
];

#[test]
fn each_statement_fails_with_its_kind() {
    let mut wrong = Vec::new();
    for &(sql, expected) in TABLE {
        let db = with_table();
        match execute(&db, sql) {
            Ok(out) => wrong.push(format!("{sql:?}: expected {expected:?}, got {out:?}")),
            Err(e) if kind(&e) != expected => {
                wrong.push(format!("{sql:?}: expected {expected:?}, got {e:?}"))
            }
            Err(_) => {}
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// A key longer than the engine takes is the engine's limit, not a rule of
/// the data (design 0027, small question 5).
#[test]
fn a_key_over_the_engine_limit_is_the_engines_error() {
    let db = new_db();
    execute(&db, "CREATE TABLE k (k TEXT PRIMARY KEY)").unwrap();
    let e = execute(
        &db,
        &format!("INSERT INTO k VALUES ('{}')", "a".repeat(600)),
    )
    .unwrap_err();
    assert_eq!(kind(&e), Kind::KeyTooLarge, "{e:?}");
}

/// Design 0027 E10: the four shapes of standard SQL the parser knows are
/// unsupported, and the message names the shape.
#[test]
fn the_four_known_shapes_are_unsupported_and_named() {
    let db = with_table();
    for (sql, named) in [
        (
            "CREATE TABLE IF NOT EXISTS u (id INTEGER PRIMARY KEY)",
            "IF NOT EXISTS",
        ),
        ("SELECT COUNT(*) FROM t", "COUNT(...)"),
        ("SELECT count(id) FROM t", "COUNT(...)"),
        ("SELECT * FROM t WHERE length(name) = 1", "LENGTH(...)"),
        ("SELECT t.id FROM t", "t.id"),
        ("SELECT * FROM t WHERE t.id = 1", "t.id"),
        ("UPDATE t SET t.name = 'b'", "t.name"),
        ("SELECT * FROM main.t", "main.t"),
        ("-- hello\nSELECT * FROM t", "--"),
        ("SELECT * FROM t -- trailing", "--"),
        ("/* hello */ SELECT * FROM t", "/*"),
    ] {
        match execute(&db, sql) {
            Err(SqlError::Unsupported(message)) => {
                assert!(message.contains(named), "{sql:?}: {message}")
            }
            other => panic!("{sql:?}: expected unsupported, got {other:?}"),
        }
    }
}

/// A table or column named `if`, and a value minus a negative number, are
/// none of the four shapes.
#[test]
fn the_shapes_do_not_catch_their_neighbours() {
    let db = new_db();
    execute(
        &db,
        "CREATE TABLE if (id INTEGER PRIMARY KEY, count INTEGER)",
    )
    .unwrap();
    execute(&db, "INSERT INTO if VALUES (1, 5)").unwrap();
    assert!(execute(&db, "SELECT count FROM if WHERE count - -1 = 6").is_ok());
    assert!(execute(&db, "SELECT * FROM if WHERE id IN (1)").is_ok());
}

/// Real typos next to the shapes are still syntax errors (E10).
#[test]
fn typos_next_to_the_shapes_are_still_syntax_errors() {
    let db = with_table();
    for sql in [
        "SELECT * FORM t",
        "SELECT id, FROM t",
        "SELECT * FROM t WHERE id = = 1",
        "CREATE TABLE u id INTEGER",
        "SELECT t. FROM t",
    ] {
        let e = execute(&db, sql).unwrap_err();
        assert_eq!(kind(&e), Kind::Syntax, "{sql:?}: {e:?}");
    }
}

/// Design 0027 E11: a syntax error says the line and the column, counted
/// in characters from 1, and keeps the byte offset in its field.
#[test]
fn a_syntax_error_names_its_line_and_column_and_keeps_its_byte() {
    let db = with_table();
    for (sql, byte, line, column) in [
        ("SELECT * FROM t WHERE id = = 1", 27, 1, 28),
        ("SELECT * FROM t\nWHERE id = = 1", 27, 2, 12),
        ("SELECT *\n\n  FROM t WHERE (", 26, 3, 17),
        // 'é' and '김' are 2 and 3 bytes but one character each.
        ("SELECT * FROM t WHERE name = 'é' AND )", 38, 1, 38),
        ("SELECT * FROM t\nWHERE name = '김치' AND )", 42, 2, 23),
        // A tab is one character.
        ("SELECT\t*\tFROM\tt\tWHERE\t)", 22, 1, 23),
        // The end of the statement is just past its last character.
        ("SELECT * FROM", 13, 1, 14),
    ] {
        match execute(&db, sql) {
            Err(e @ SqlError::Syntax { .. }) => {
                let SqlError::Syntax {
                    position,
                    line: l,
                    column: c,
                    ..
                } = &e
                else {
                    unreachable!()
                };
                assert_eq!((*position, *l, *c), (byte, line, column), "{sql:?}");
                let message = e.to_string();
                assert!(
                    message.starts_with(&format!("syntax error at line {line}, column {column}: ")),
                    "{sql:?}: {message}"
                );
            }
            other => panic!("{sql:?}: expected a syntax error, got {other:?}"),
        }
    }
}

/// An integer literal too big for 64 bits is a value out of range, and its
/// message says where it is (E4, E11).
#[test]
fn a_literal_too_big_says_where_it_is() {
    let db = with_table();
    let e = execute(&db, "SELECT * FROM t\nWHERE id = 99999999999999999999").unwrap_err();
    let message = e.to_string();
    assert!(matches!(e, SqlError::Arithmetic(_)), "{e:?}");
    assert!(message.contains("line 2, column 12"), "{message}");
    assert!(message.contains("64 bits"), "{message}");
}

/// The NULL primary key error names the column (E3).
#[test]
fn a_null_key_names_its_column() {
    let db = with_table();
    match execute(&db, "INSERT INTO t VALUES (NULL, 'a', NULL)") {
        Err(e @ SqlError::NullKey { .. }) => {
            let SqlError::NullKey { column } = &e else {
                unreachable!()
            };
            assert_eq!(column, "id");
            assert!(e.to_string().contains("cannot be NULL"), "{e}");
        }
        other => panic!("{other:?}"),
    }
}

/// Messages follow design 0027's rules: `group: what`, starting in lower
/// case, without a full stop at the end.
#[test]
fn messages_start_in_lower_case_and_have_no_full_stop() {
    for &(sql, _) in TABLE {
        let db = with_table();
        let message = execute(&db, sql).unwrap_err().to_string();
        let first = message.chars().next().unwrap();
        assert!(!first.is_uppercase(), "{sql:?}: {message}");
        assert!(!message.ends_with('.'), "{sql:?}: {message}");
    }
}

/// Design 0027 W2 (E9): two transactions insert the same key at once. The
/// one that commits second loses a conflict, not a duplicate key, and
/// running it again gets the duplicate key.
#[test]
fn the_same_key_inserted_at_once_is_a_conflict_then_a_duplicate() {
    let db = new_db();
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    let mut first = Session::new(&db);
    let mut second = Session::new(&db);
    first.execute("BEGIN").unwrap();
    second.execute("BEGIN").unwrap();
    first.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    second.execute("INSERT INTO t VALUES (1, 'b')").unwrap();
    first.execute("COMMIT").unwrap();
    let e = second.execute("COMMIT").unwrap_err();
    assert!(e.is_conflict(), "{e:?}");
    assert!(matches!(e, SqlError::TransactionConflict(_)), "{e:?}");
    second.execute("BEGIN").unwrap();
    let e = second.execute("INSERT INTO t VALUES (1, 'b')").unwrap_err();
    assert!(matches!(e, SqlError::DuplicateKey { .. }), "{e:?}");
}

/// After a conflict, the INSERT run again succeeds if the winner deleted
/// the key: the conflict could not have said "duplicate" (E9 B's flaw).
#[test]
fn after_a_conflict_the_insert_run_again_can_succeed() {
    let db = new_db();
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    execute(&db, "INSERT INTO t VALUES (1, 'a')").unwrap();
    let mut first = Session::new(&db);
    let mut second = Session::new(&db);
    first.execute("BEGIN").unwrap();
    second.execute("BEGIN").unwrap();
    first.execute("DELETE FROM t WHERE id = 1").unwrap();
    second
        .execute("UPDATE t SET name = 'b' WHERE id = 1")
        .unwrap();
    first.execute("COMMIT").unwrap();
    assert!(second.execute("COMMIT").unwrap_err().is_conflict());
    second.execute("INSERT INTO t VALUES (1, 'b')").unwrap();
}

#[test]
fn transaction_errors_say_which() {
    let db = new_db();
    let mut session = Session::new(&db);
    assert!(matches!(
        session.execute("COMMIT"),
        Err(SqlError::Transaction(TransactionError::NotBegun))
    ));
    session.execute("BEGIN").unwrap();
    assert!(matches!(
        session.execute("BEGIN"),
        Err(SqlError::Transaction(TransactionError::AlreadyBegun))
    ));
}
