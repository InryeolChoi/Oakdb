//! Blind tests for issue #34 (type checks and error kinds), written from
//! the issue and its comment, docs/design/0027-type-checks-and-errors.md,
//! docs/protocol.md section 6 and the public signatures of `SqlError` and
//! `Session` only, without reading the implementation or the PR's tests
//! (PR #85's verification). The wire codes come from `error_code`, the
//! function the server uses, checked against the bytes in protocol.md.

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_server_codes::code_of;
use oakdb_sql::{every_kind, execute, Output, Session, SqlError, TransactionError, Value};

/// The documented wire code for a `SqlError` (protocol.md section 6).
mod oakdb_server_codes {
    // oakdb-sql cannot depend on oakdb-server, so this file restates the
    // design's table (0027 E8) as the expected code of each kind. The
    // server crate's blind test checks the real `error_code` against it.
    use oakdb_sql::SqlError;

    pub fn code_of(e: &SqlError) -> u8 {
        match e {
            SqlError::Syntax { .. } => 0x01,
            SqlError::Unsupported(_) => 0x02,
            SqlError::NoSuchTable(_) => 0x03,
            SqlError::NoSuchColumn { .. } => 0x04,
            SqlError::TableExists(_) => 0x05,
            SqlError::DuplicateColumn(_) => 0x06,
            SqlError::ValueCount { .. } => 0x07,
            SqlError::DuplicateKey { .. } => 0x08,
            SqlError::TypeMismatch { .. } | SqlError::Type(_) => 0x09,
            SqlError::Corrupt(_) => 0x0A,
            SqlError::Arithmetic(_) => 0x0C,
            SqlError::NullKey { .. } => 0x0D,
            SqlError::TransactionConflict(_) => 0x20,
            _ => 0xFF,
        }
    }
}

const DIR: &str = "/data/db";

fn fresh() -> Db<SimFs> {
    Db::open_with(SimFs::new(), DIR).expect("open")
}

fn ok(db: &Db<SimFs>, sql: &str) -> Output {
    match execute(db, sql) {
        Ok(out) => out,
        Err(e) => panic!("{sql:?} failed: {e:?}"),
    }
}

fn err(db: &Db<SimFs>, sql: &str) -> SqlError {
    match execute(db, sql) {
        Ok(out) => panic!("{sql:?} should fail, got {out:?}"),
        Err(e) => e,
    }
}

fn rows(db: &Db<SimFs>, sql: &str) -> Vec<Vec<Value>> {
    match ok(db, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{sql:?} gave {other:?}, not rows"),
    }
}

const SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, b BLOB)";

fn empty() -> Db<SimFs> {
    let db = fresh();
    ok(&db, SCHEMA);
    db
}

fn full() -> Db<SimFs> {
    let db = empty();
    ok(&db, "INSERT INTO t VALUES (1, 'a', X'00')");
    ok(&db, "INSERT INTO t VALUES (2, 'b', NULL)");
    ok(&db, "INSERT INTO t VALUES (5, '5', X'01')");
    db
}

fn is_type(e: &SqlError) -> bool {
    matches!(e, SqlError::TypeMismatch { .. } | SqlError::Type(_))
}

/// Same kind (variant) of error, ignoring the message.
fn same_kind(a: &SqlError, b: &SqlError) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

// ------------------------------------------------- 1. type errors (0x09)

/// Statements whose types are wrong regardless of the rows (E1, E2,
/// protocol 0x09: "행을 읽기 전에 검사하므로 빈 표에서도 같다").
const TYPE_ERRORS: &[&str] = &[
    // INSERT
    "INSERT INTO t VALUES ('x', 'a', NULL)",
    "INSERT INTO t VALUES ('5', 'a', NULL)",
    "INSERT INTO t VALUES (9, 1, NULL)",
    "INSERT INTO t VALUES (9, 'a', 'not a blob')",
    "INSERT INTO t VALUES (9, X'00', NULL)",
    "INSERT INTO t (id, name) VALUES (9, 7)",
    // WHERE
    "SELECT * FROM t WHERE name = 1",
    "SELECT * FROM t WHERE id = '5'",
    "SELECT * FROM t WHERE id + 1 = 'a'",
    "SELECT * FROM t WHERE name + 1 = 2",
    "SELECT * FROM t WHERE id",
    "SELECT * FROM t WHERE name",
    "SELECT * FROM t WHERE b = 'a'",
    "SELECT * FROM t WHERE NULL + 'a' = 1",
    "SELECT * FROM t WHERE id = 1 AND name",
    "SELECT * FROM t WHERE NOT id",
    "SELECT * FROM t WHERE id < 'z'",
    "SELECT * FROM t WHERE id * name = 1",
    "SELECT * FROM t WHERE 'a' - 'b' = 0",
    "DELETE FROM t WHERE id = 'a'",
    "UPDATE t SET name = 'z' WHERE name = 1",
    // UPDATE SET
    "UPDATE t SET id = 'x'",
    "UPDATE t SET name = 1",
    "UPDATE t SET name = id",
    "UPDATE t SET id = name",
    "UPDATE t SET id = id + 'a'",
    "UPDATE t SET b = 'a'",
];

#[test]
fn type_errors_are_type_kind_and_code_0x09_on_an_empty_table() {
    for sql in TYPE_ERRORS {
        let db = empty();
        let e = err(&db, sql);
        assert!(
            is_type(&e),
            "{sql:?} on empty table: expected type error, got {e:?}"
        );
        assert_eq!(code_of(&e), 0x09, "{sql:?}: {e:?}");
    }
}

#[test]
fn type_errors_are_the_same_on_an_empty_and_a_full_table() {
    for sql in TYPE_ERRORS {
        let e1 = err(&empty(), sql);
        let e2 = err(&full(), sql);
        assert!(is_type(&e2), "{sql:?} on full table: got {e2:?}");
        assert!(same_kind(&e1, &e2), "{sql:?}: empty {e1:?} vs full {e2:?}");
        assert_eq!(e1.to_string(), e2.to_string(), "{sql:?}");
    }
}

#[test]
fn a_type_error_changes_nothing() {
    let db = full();
    for sql in TYPE_ERRORS {
        let _ = err(&db, sql);
    }
    assert_eq!(rows(&db, "SELECT * FROM t").len(), 3);
    assert_eq!(
        rows(&db, "SELECT name FROM t WHERE id = 1"),
        vec![vec![Value::Text("a".into())]]
    );
}

#[test]
fn insert_type_mismatch_names_the_column_and_types() {
    let db = empty();
    match err(&db, "INSERT INTO t VALUES ('x', 'a', NULL)") {
        SqlError::TypeMismatch {
            column,
            expected,
            found,
        } => {
            assert_eq!(column, "id");
            assert_eq!(expected, oakdb_sql::ColumnType::Integer);
            assert_eq!(found, Some(oakdb_sql::ColumnType::Text));
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn strict_typing_no_implicit_cast() {
    // E2 A: '5' is TEXT, never the integer 5, even though a row with id 5
    // and a row with name '5' both exist.
    let db = full();
    assert!(is_type(&err(&db, "SELECT * FROM t WHERE id = '5'")));
    assert!(is_type(&err(&db, "SELECT * FROM t WHERE name = 5")));
    assert!(is_type(&err(&db, "INSERT INTO t VALUES ('7', 'x', NULL)")));
    // The well-typed forms work.
    assert_eq!(rows(&db, "SELECT name FROM t WHERE id = 5").len(), 1);
    assert_eq!(rows(&db, "SELECT id FROM t WHERE name = '5'").len(), 1);
}

#[test]
fn null_fits_any_type() {
    // E3: NULL is not a type error; NULL = NULL selects nothing.
    let db = full();
    assert_eq!(rows(&db, "SELECT * FROM t WHERE NULL = NULL").len(), 0);
    assert_eq!(rows(&db, "SELECT * FROM t WHERE name = NULL").len(), 0);
    assert_eq!(rows(&db, "SELECT * FROM t WHERE id + NULL = 1").len(), 0);
    ok(&db, "INSERT INTO t VALUES (9, NULL, NULL)");
    ok(&db, "UPDATE t SET name = NULL WHERE id = 9");
}

#[test]
fn cast_is_unsupported() {
    // E2: "명시적 변환 CAST(x AS INTEGER)는 ... 만나면 지원 안 함".
    let db = full();
    let e = err(&db, "SELECT * FROM t WHERE CAST(name AS INTEGER) = 5");
    assert!(matches!(e, SqlError::Unsupported(_)), "got {e:?}");
}

// --------------------------------------------- 2. NULL primary key (0x0D)

#[test]
fn null_primary_key_on_insert_is_null_key() {
    for sql in [
        "INSERT INTO t VALUES (NULL, 'a', NULL)",
        "INSERT INTO t (id, name) VALUES (NULL, 'a')",
    ] {
        let db = empty();
        match err(&db, sql) {
            e @ SqlError::NullKey { .. } => {
                assert_eq!(code_of(&e), 0x0D);
                if let SqlError::NullKey { column } = e {
                    assert_eq!(column, "id");
                }
            }
            other => panic!("{sql:?}: expected NullKey, got {other:?}"),
        }
    }
}

#[test]
fn omitted_primary_key_is_null_key() {
    // Leaving the key column out of the column list makes it NULL.
    let db = empty();
    let e = err(&db, "INSERT INTO t (name) VALUES ('a')");
    assert!(matches!(e, SqlError::NullKey { .. }), "got {e:?}");
}

#[test]
fn update_set_key_to_literal_null_is_null_key_even_on_empty_table() {
    // E1 table: "NULL 기본 키(리터럴 NULL일 때)" is a planning error.
    for db in [empty(), full()] {
        let e = err(&db, "UPDATE t SET id = NULL");
        assert!(matches!(e, SqlError::NullKey { .. }), "got {e:?}");
    }
}

#[test]
fn update_set_key_to_computed_null_is_null_key() {
    // E1 table: "SET id = 계산식이 NULL이 될 때" is an execution error.
    let db = full();
    let e = err(&db, "UPDATE t SET id = id + NULL WHERE id = 1");
    assert!(matches!(e, SqlError::NullKey { .. }), "got {e:?}");
    assert_eq!(rows(&db, "SELECT * FROM t").len(), 3);
}

#[test]
fn null_key_is_not_a_type_mismatch_any_more() {
    // E3 / small question 1: TypeMismatch { found: None } is not produced.
    let db = full();
    for sql in [
        "INSERT INTO t VALUES (NULL, 'a', NULL)",
        "UPDATE t SET id = NULL",
        "UPDATE t SET id = id + NULL",
    ] {
        let e = err(&db, sql);
        assert!(
            !matches!(e, SqlError::TypeMismatch { found: None, .. }),
            "{sql:?}: {e:?}"
        );
    }
}

// ----------------------------------------- 3. value range errors (0x0C)

#[test]
fn overflow_is_arithmetic() {
    let db = full();
    for sql in [
        "UPDATE t SET id = id + 9223372036854775807",
        "SELECT * FROM t WHERE id + 9223372036854775807 = 1",
        "SELECT * FROM t WHERE id * 9223372036854775807 = 1",
        "SELECT * FROM t WHERE 0 - id - 9223372036854775807 = 1",
    ] {
        let e = err(&db, sql);
        assert!(matches!(e, SqlError::Arithmetic(_)), "{sql:?}: got {e:?}");
        assert_eq!(code_of(&e), 0x0C);
        assert!(e.to_string().contains("overflow"), "{sql:?}: {e}");
    }
    assert_eq!(rows(&db, "SELECT * FROM t WHERE id = 1").len(), 1);
}

#[test]
fn division_by_zero_is_arithmetic() {
    let db = full();
    for sql in [
        "SELECT * FROM t WHERE 1 / 0 = 1",
        "SELECT * FROM t WHERE id / 0 = 1",
        "SELECT * FROM t WHERE id % 0 = 1",
        "UPDATE t SET id = id / 0",
        "DELETE FROM t WHERE id / (id - id) = 1",
    ] {
        let e = err(&db, sql);
        assert!(matches!(e, SqlError::Arithmetic(_)), "{sql:?}: got {e:?}");
        assert_eq!(code_of(&e), 0x0C);
        assert!(e.to_string().contains("division by zero"), "{sql:?}: {e}");
    }
    assert_eq!(rows(&db, "SELECT * FROM t").len(), 3);
}

#[test]
fn null_divided_by_zero_is_null_not_an_error() {
    let db = full();
    assert_eq!(rows(&db, "SELECT * FROM t WHERE NULL / 0 = 1").len(), 0);
    assert_eq!(
        rows(&db, "SELECT * FROM t WHERE NOT (NULL / 0 = 1)").len(),
        0
    );
    // id 2 has name NULL; NULL / 0 on a column value is also NULL.
    ok(&db, "CREATE TABLE n (k INTEGER PRIMARY KEY, v INTEGER)");
    ok(&db, "INSERT INTO n VALUES (1, NULL)");
    assert_eq!(rows(&db, "SELECT * FROM n WHERE v / 0 = 1").len(), 0);
}

#[test]
fn literal_beyond_64_bits_is_arithmetic_with_position() {
    for db in [empty(), full()] {
        let e = err(&db, "SELECT * FROM t WHERE id = 99999999999999999999");
        assert!(matches!(e, SqlError::Arithmetic(_)), "got {e:?}");
        assert_eq!(code_of(&e), 0x0C);
        // protocol 0x0C: "64비트에 안 들어가는 정수 리터럴(메시지에 줄·칸)".
        // The literal starts at byte 27, column 28 of line 1.
        let msg = e.to_string();
        assert!(msg.contains("line 1, column 28"), "{msg}");
    }
    let db = empty();
    let e = err(
        &db,
        "INSERT INTO t VALUES (\n  99999999999999999999, 'a', NULL)",
    );
    assert!(matches!(e, SqlError::Arithmetic(_)), "got {e:?}");
    assert!(e.to_string().contains("line 2, column 3"), "{e}");
}

#[test]
fn i64_max_literal_is_fine() {
    let db = empty();
    ok(
        &db,
        "INSERT INTO t VALUES (9223372036854775807, 'max', NULL)",
    );
    assert_eq!(
        rows(&db, "SELECT * FROM t WHERE id = 9223372036854775807").len(),
        1
    );
}

// --------------------------------------------------- 4. unsupported (0x02)

#[test]
fn known_standard_forms_are_unsupported_and_named() {
    let db = full();
    let cases: &[(&str, &str)] = &[
        (
            "CREATE TABLE IF NOT EXISTS u (id INTEGER PRIMARY KEY)",
            "IF NOT EXISTS",
        ),
        ("SELECT COUNT(*) FROM t", "COUNT"),
        ("SELECT t.id FROM t", "t.id"),
        ("-- 주석\nSELECT * FROM t", "--"),
        ("SELECT * FROM t -- trailing", "--"),
        ("/* c */ SELECT * FROM t", "/*"),
    ];
    for (sql, shown) in cases {
        let e = err(&db, sql);
        match &e {
            SqlError::Unsupported(msg) => {
                assert!(
                    msg.contains(shown),
                    "{sql:?}: message {msg:?} should name {shown:?}"
                )
            }
            other => panic!("{sql:?}: expected Unsupported, got {other:?}"),
        }
        assert_eq!(code_of(&e), 0x02);
    }
    // IF NOT EXISTS did not create a table named IF or u.
    assert!(matches!(
        err(&db, "SELECT * FROM u"),
        SqlError::NoSuchTable(_)
    ));
}

#[test]
fn unknown_column_type_is_unsupported() {
    let db = fresh();
    let e = err(&db, "CREATE TABLE v (id FROBNICATE PRIMARY KEY)");
    assert!(matches!(e, SqlError::Unsupported(_)), "got {e:?}");
}

#[test]
fn a_real_typo_is_still_a_syntax_error() {
    // E13: "그 옆의 진짜 오타(SELECT * FORM t)는 여전히 Syntax".
    let db = full();
    let mut bad = Vec::new();
    for sql in [
        "SELECT * FORM t",
        "SELECT * FROM t WHER id = 1",
        "SELECT * FROM t WHERE id = 1 ORDR BY id",
        "INSERT INTO t VALUES (1,",
        "INSERT INTO t VALUS (1, 'a', NULL)",
        "UPDATE t SETT name = 'a'",
        "DELETE t WHERE id = 1",
    ] {
        let e = err(&db, sql);
        if !matches!(e, SqlError::Syntax { .. }) {
            bad.push(format!("{sql:?} -> {e:?}"));
        }
    }
    assert!(bad.is_empty(), "typos that are not Syntax: {bad:#?}");
}

#[test]
fn misspelled_statement_keyword_is_unsupported_by_the_documented_exception() {
    // Small question 3: `SELEC * FROM t` stays "unsupported: statement
    // `SELEC`", recorded as an exception to E10.
    let db = full();
    match err(&db, "SELEC * FROM t") {
        SqlError::Unsupported(msg) => assert!(msg.contains("SELEC"), "{msg}"),
        other => panic!("got {other:?}"),
    }
}

// ------------------------------------------------- 5. syntax positions

fn syntax(db: &Db<SimFs>, sql: &str) -> (usize, usize, usize, String) {
    match err(db, sql) {
        e @ SqlError::Syntax { .. } => {
            let shown = e.to_string();
            let SqlError::Syntax {
                position,
                line,
                column,
                ..
            } = e
            else {
                unreachable!()
            };
            (position, line, column, shown)
        }
        other => panic!("{sql:?}: expected Syntax, got {other:?}"),
    }
}

#[test]
fn multi_line_syntax_error_reports_line_and_column_and_keeps_byte() {
    let db = full();
    // From design 0027's "지금 상태" table: byte 27, now line 2, column 12.
    let (pos, line, col, shown) = syntax(&db, "SELECT * FROM t\nWHERE id = = 1");
    assert_eq!(pos, 27);
    assert_eq!((line, col), (2, 12));
    assert!(shown.contains("at line 2, column 12"), "{shown}");
    assert!(!shown.contains("at byte"), "{shown}");
}

#[test]
fn korean_text_before_the_error_counts_as_one_column_per_character() {
    let db = full();
    // "INSERT INTO t VALUES (1, '김치', ," — the third comma is at character
    // 31 (0-based) but byte 35, because each Korean letter is 3 bytes.
    let (pos, line, col, shown) = syntax(&db, "INSERT INTO t VALUES (1, '김치', ,)");
    assert_eq!(pos, 35);
    assert_eq!((line, col), (1, 32));
    assert!(shown.contains("at line 1, column 32"), "{shown}");
}

#[test]
fn tab_and_korean_on_a_later_line() {
    let db = full();
    // Line 1 is "INSERT INTO t VALUES" (20 bytes) + '\n'. Line 2 is
    // "\t(1, '김', ,)": the second comma is the 11th character, byte 33.
    let (pos, line, col, _) = syntax(&db, "INSERT INTO t VALUES\n\t(1, '김', ,)");
    assert_eq!(pos, 33);
    assert_eq!((line, col), (2, 11));
}

#[test]
fn korean_line_above_counts_from_the_new_line() {
    let db = full();
    // A Korean literal on line 1 must not shift the column on line 2.
    let (pos, line, col, _) = syntax(&db, "SELECT * FROM t WHERE name = '김치찌개'\nAND = 1");
    assert_eq!(line, 2);
    assert_eq!(col, 5);
    assert_eq!(pos, "SELECT * FROM t WHERE name = '김치찌개'\nAND ".len());
}

#[test]
fn first_character_is_line_1_column_1() {
    let db = fresh();
    let (pos, line, col, _) = syntax(&db, ")");
    assert_eq!((pos, line, col), (0, 1, 1));
}

// ------------------------------------------------- 6. constraints and races

#[test]
fn duplicate_key_is_0x08() {
    let db = full();
    let e = err(&db, "INSERT INTO t VALUES (1, 'again', NULL)");
    assert!(matches!(e, SqlError::DuplicateKey { .. }), "got {e:?}");
    assert_eq!(code_of(&e), 0x08);
    let e = err(&db, "UPDATE t SET id = 2 WHERE id = 1");
    assert!(matches!(e, SqlError::DuplicateKey { .. }), "got {e:?}");
}

#[test]
fn concurrent_insert_of_the_same_key_is_a_conflict_then_a_duplicate() {
    // E9 A: the loser gets a conflict (0x20); running it again gives the
    // precise answer, DuplicateKey (0x08).
    let db = empty();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();
    a.execute("INSERT INTO t VALUES (7, 'a', NULL)").unwrap();
    let first = b.execute("INSERT INTO t VALUES (7, 'b', NULL)");
    a.execute("COMMIT").unwrap();
    let loser = first.and_then(|_| b.execute("COMMIT"));
    let e = loser.expect_err("the second writer must lose");
    assert!(e.is_conflict(), "expected a conflict, got {e:?}");
    assert!(!matches!(e, SqlError::DuplicateKey { .. }), "{e:?}");
    if b.in_transaction() {
        let _ = b.execute("ROLLBACK");
    }
    let again = b
        .execute("INSERT INTO t VALUES (7, 'b', NULL)")
        .unwrap_err();
    assert!(
        matches!(again, SqlError::DuplicateKey { .. }),
        "got {again:?}"
    );
}

#[test]
fn concurrent_insert_then_delete_lets_the_retry_succeed() {
    // E9: "먼저 커밋한 쪽이 그 키를 지웠다면 다시 실행한 INSERT는 성공".
    let db = empty();
    let mut a = Session::new(&db);
    let mut b = Session::new(&db);
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();
    a.execute("INSERT INTO t VALUES (7, 'a', NULL)").unwrap();
    a.execute("DELETE FROM t WHERE id = 7").unwrap();
    let first = b.execute("INSERT INTO t VALUES (7, 'b', NULL)");
    a.execute("COMMIT").unwrap();
    let loser = first.and_then(|_| b.execute("COMMIT"));
    // Whether a write-then-delete counts as a conflict is the engine's
    // business; either way, retrying must succeed.
    if let Err(e) = loser {
        assert!(e.is_conflict(), "got {e:?}");
        if b.in_transaction() {
            let _ = b.execute("ROLLBACK");
        }
        b.execute("INSERT INTO t VALUES (7, 'b', NULL)").unwrap();
    }
    assert_eq!(
        rows(&db, "SELECT name FROM t WHERE id = 7"),
        vec![vec![Value::Text("b".into())]]
    );
}

// ------------------------------------------------- 7. kinds and codes

#[test]
fn transaction_state_error_is_its_own_kind() {
    let db = fresh();
    let e = err(&db, "BEGIN");
    assert!(
        matches!(e, SqlError::Transaction(TransactionError::NeedsSession)),
        "got {e:?}"
    );
}

#[test]
fn every_kind_covers_each_kind_in_the_table() {
    // Each kind in the table appears in every_kind() at least once.
    let kinds = every_kind();
    let has = |f: &dyn Fn(&SqlError) -> bool| kinds.iter().any(|k| f(k));
    assert!(has(&|k| matches!(k, SqlError::Syntax { .. })));
    assert!(has(&|k| matches!(k, SqlError::Unsupported(_))));
    assert!(has(&|k| matches!(k, SqlError::TypeMismatch { .. })));
    assert!(has(&|k| matches!(k, SqlError::Type(_))));
    assert!(has(&|k| matches!(k, SqlError::Arithmetic(_))));
    assert!(has(&|k| matches!(k, SqlError::NullKey { .. })));
    assert!(has(&|k| matches!(k, SqlError::DuplicateKey { .. })));
    assert!(has(&|k| matches!(k, SqlError::Transaction(_))));
    assert!(has(&|k| matches!(k, SqlError::TransactionConflict(_))));
    assert!(has(&|k| matches!(k, SqlError::Engine(_))));
}

#[test]
fn messages_follow_the_documented_shape() {
    // protocol 6: "영어로 `묶음: 내용` 꼴, 소문자로 시작, 마침표로 끝나지 않음".
    let db = full();
    let samples = [
        err(&db, "SELECT * FROM t WHERE id + 1 = 'a'"),
        err(&db, "SELECT * FROM t WHERE 1 / 0 = 1"),
        err(&db, "INSERT INTO t VALUES (NULL, 'a', NULL)"),
        err(&db, "SELECT COUNT(*) FROM t"),
        err(&db, "SELECT * FORM t"),
        err(&db, "INSERT INTO t VALUES (1, 'a', NULL)"),
        err(&db, "INSERT INTO t VALUES ('x', 'a', NULL)"),
        err(&db, "SELECT * FROM nope"),
        err(&db, "SELECT nope FROM t"),
        err(&db, "INSERT INTO t VALUES (1)"),
        err(&db, "BEGIN"),
        err(&db, "SELECT * FROM t WHERE id = 99999999999999999999"),
    ];
    let mut bad = Vec::new();
    for e in samples {
        let m = e.to_string();
        let first = m.chars().next().unwrap();
        if !first.is_ascii_lowercase() || m.ends_with('.') || !m.contains(": ") {
            bad.push(m);
        }
    }
    assert!(
        bad.is_empty(),
        "not `kind: detail`, lower case, no period: {bad:#?}"
    );
}
