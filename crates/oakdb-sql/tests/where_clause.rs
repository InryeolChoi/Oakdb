//! WHERE and range scans (issue #32, design 0019). Like `sql.rs`, these
//! tests only look at what running SQL strings returns. That the primary
//! key conditions run as range scans is checked where the plan is made,
//! in `planner.rs`; here the results must be right whichever way they ran.

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

fn rows(db: &Db<SimFs>, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{sql}: expected rows, got {other:?}"),
    }
}

/// The ids (first column) of the rows `sql` selects, in the order given.
fn ids(db: &Db<SimFs>, sql: &str) -> Vec<i64> {
    rows(db, sql)
        .into_iter()
        .map(|row| match row[0] {
            Value::Integer(i) => i,
            ref other => panic!("{sql}: id {other:?}"),
        })
        .collect()
}

/// `people (id, age, name)` holding ids 1..=10. Age is `id * 10`, except
/// that ids 3 and 7 have no age; name is `p<id>`, except that id 5 has no
/// name.
fn people() -> Db<SimFs> {
    let db = new_db();
    run(
        &db,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, age INTEGER, name TEXT)",
    );
    for id in 1..=10 {
        let age = if id == 3 || id == 7 {
            "NULL".to_owned()
        } else {
            (id * 10).to_string()
        };
        let name = if id == 5 {
            "NULL".to_owned()
        } else {
            format!("'p{id}'")
        };
        run(
            &db,
            &format!("INSERT INTO people VALUES ({id}, {age}, {name})"),
        );
    }
    db
}

// ------------------------------------------------- primary key ranges

#[test]
fn primary_key_comparisons_select_ranges() {
    let db = people();
    for (sql, want) in [
        ("SELECT * FROM people WHERE id > 7", vec![8, 9, 10]),
        ("SELECT * FROM people WHERE id >= 7", vec![7, 8, 9, 10]),
        ("SELECT * FROM people WHERE id < 3", vec![1, 2]),
        ("SELECT * FROM people WHERE id <= 3", vec![1, 2, 3]),
        ("SELECT * FROM people WHERE 3 >= id", vec![1, 2, 3]),
        ("SELECT * FROM people WHERE id = 4", vec![4]),
        ("SELECT * FROM people WHERE id > 3 AND id < 6", vec![4, 5]),
        ("SELECT * FROM people WHERE id >= 3 AND id <= 3", vec![3]),
        ("SELECT * FROM people WHERE id > 5 AND id < 3", vec![]),
        (
            "SELECT * FROM people WHERE id BETWEEN 4 AND 6",
            vec![4, 5, 6],
        ),
        ("SELECT * FROM people WHERE id BETWEEN 6 AND 4", vec![]),
        (
            "SELECT * FROM people WHERE id IN (9, 2, 5, 42)",
            vec![2, 5, 9],
        ),
        ("SELECT * FROM people WHERE id > 3 + 4", vec![8, 9, 10]),
        (
            "SELECT * FROM people WHERE id != 4 AND id < 6",
            vec![1, 2, 3, 5],
        ),
        ("SELECT * FROM people WHERE id = 1 OR id = 10", vec![1, 10]),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

#[test]
fn rows_come_back_in_primary_key_order() {
    let db = people();
    assert_eq!(
        ids(&db, "SELECT * FROM people WHERE id > 0"),
        (1..=10).collect::<Vec<_>>()
    );
}

#[test]
fn select_without_where_returns_the_whole_table_and_only_that_table() {
    let db = people();
    run(&db, "CREATE TABLE other (id INTEGER PRIMARY KEY)");
    run(&db, "INSERT INTO other VALUES (100), (-100)");
    assert_eq!(
        ids(&db, "SELECT id FROM people"),
        (1..=10).collect::<Vec<_>>()
    );
    assert_eq!(ids(&db, "SELECT * FROM other"), vec![-100, 100]);
}

#[test]
fn i64_edges_of_the_primary_key_do_not_overflow() {
    let db = new_db();
    run(&db, "CREATE TABLE n (id INTEGER PRIMARY KEY)");
    run(
        &db,
        "INSERT INTO n VALUES (-9223372036854775808), (-1), (0), (1), (9223372036854775807)",
    );
    let max = i64::MAX;
    let min = i64::MIN;
    for (sql, want) in [
        ("SELECT * FROM n WHERE id > 9223372036854775807", vec![]),
        ("SELECT * FROM n WHERE id >= 9223372036854775807", vec![max]),
        ("SELECT * FROM n WHERE id < -9223372036854775808", vec![]),
        (
            "SELECT * FROM n WHERE id <= -9223372036854775808",
            vec![min],
        ),
        ("SELECT * FROM n WHERE id < 0", vec![min, -1]),
        ("SELECT * FROM n WHERE id > -1", vec![0, 1, max]),
        (
            "SELECT * FROM n WHERE id BETWEEN -9223372036854775808 AND 9223372036854775807",
            vec![min, -1, 0, 1, max],
        ),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

#[test]
fn text_primary_keys_range_bytewise() {
    let db = new_db();
    run(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, n INTEGER)");
    run(
        &db,
        "INSERT INTO s VALUES ('a', 1), ('ab', 2), ('b', 3), ('B', 4), ('', 5)",
    );
    let ns = |sql: &str| -> Vec<i64> {
        rows(&db, sql)
            .into_iter()
            .map(|row| match row[0] {
                Value::Integer(i) => i,
                ref other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(ns("SELECT n FROM s WHERE k > 'a'"), vec![2, 3]);
    assert_eq!(ns("SELECT n FROM s WHERE k >= 'a'"), vec![1, 2, 3]);
    assert_eq!(ns("SELECT n FROM s WHERE k < 'a'"), vec![5, 4]);
    assert_eq!(
        ns("SELECT n FROM s WHERE k BETWEEN 'a' AND 'az'"),
        vec![1, 2]
    );
    assert_eq!(ns("SELECT n FROM s WHERE k IN ('b', '')"), vec![5, 3]);
}

// ------------------------------------------------- filters on other columns

#[test]
fn other_columns_filter_rows() {
    let db = people();
    for (sql, want) in [
        ("SELECT * FROM people WHERE age = 40", vec![4]),
        ("SELECT * FROM people WHERE age > 80", vec![9, 10]),
        ("SELECT * FROM people WHERE name = 'p2'", vec![2]),
        (
            "SELECT * FROM people WHERE name <> 'p2' AND id < 4",
            vec![1, 3],
        ),
        (
            "SELECT * FROM people WHERE age < 30 OR name = 'p9'",
            vec![1, 2, 9],
        ),
        (
            "SELECT * FROM people WHERE id > 5 AND age >= 90",
            vec![9, 10],
        ),
        (
            "SELECT * FROM people WHERE age = id * 10",
            vec![1, 2, 4, 5, 6, 8, 9, 10],
        ),
        (
            "SELECT * FROM people WHERE id % 2 = 0 AND age IS NOT NULL",
            vec![2, 4, 6, 8, 10],
        ),
        (
            "SELECT * FROM people WHERE age BETWEEN 20 AND 50",
            vec![2, 4, 5],
        ),
        ("SELECT * FROM people WHERE age IN (10, 60, 70)", vec![1, 6]),
        ("SELECT * FROM people WHERE NOT (age > 20)", vec![1, 2]),
        (
            "SELECT * FROM people WHERE (id = 1 OR id = 2) AND age = 20",
            vec![2],
        ),
        (
            "SELECT * FROM people WHERE id = 1 OR id = 2 AND age = 10",
            vec![1],
        ),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

#[test]
fn arithmetic_follows_precedence_and_truncates_toward_zero() {
    let db = people();
    for (sql, want) in [
        ("SELECT * FROM people WHERE id = 1 + 2 * 3", vec![7]),
        ("SELECT * FROM people WHERE id = (1 + 2) * 3", vec![9]),
        ("SELECT * FROM people WHERE id = 10 - 3 - 2", vec![5]),
        ("SELECT * FROM people WHERE id = -7 / 2 + 5", vec![2]),
        ("SELECT * FROM people WHERE id = -7 % 2 + 2", vec![1]),
        ("SELECT * FROM people WHERE -id = -3", vec![3]),
        ("SELECT * FROM people WHERE id = - -4", vec![4]),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

// ------------------------------------------------- NULL

#[test]
fn null_follows_three_valued_logic() {
    let db = people();
    for (sql, want) in [
        // Comparing with NULL is never true, so `= NULL` finds nothing.
        ("SELECT * FROM people WHERE age = NULL", vec![]),
        ("SELECT * FROM people WHERE age <> NULL", vec![]),
        ("SELECT * FROM people WHERE id = NULL", vec![]),
        ("SELECT * FROM people WHERE age IS NULL", vec![3, 7]),
        ("SELECT * FROM people WHERE name IS NULL", vec![5]),
        (
            "SELECT * FROM people WHERE age IS NOT NULL AND id < 5",
            vec![1, 2, 4],
        ),
        ("SELECT * FROM people WHERE id IS NULL", vec![]),
        // NOT of unknown is unknown: the rows without an age stay out.
        (
            "SELECT * FROM people WHERE NOT (age = 30)",
            vec![1, 2, 4, 5, 6, 8, 9, 10],
        ),
        (
            "SELECT * FROM people WHERE NOT (name = 'p1') AND id < 7",
            vec![2, 3, 4, 6],
        ),
        // false AND unknown is false, true OR unknown is true.
        (
            "SELECT * FROM people WHERE NOT (id > 5 AND age > 0)",
            vec![1, 2, 3, 4, 5],
        ),
        (
            "SELECT * FROM people WHERE id = 3 OR age > 0",
            vec![1, 2, 3, 4, 5, 6, 8, 9, 10],
        ),
        // Arithmetic with NULL is NULL.
        (
            "SELECT * FROM people WHERE age + 1 > 0",
            vec![1, 2, 4, 5, 6, 8, 9, 10],
        ),
        (
            "SELECT * FROM people WHERE (age + NULL) IS NULL AND id < 3",
            vec![1, 2],
        ),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

#[test]
fn not_in_and_not_between_with_null_are_never_true() {
    let db = people();
    for (sql, want) in [
        (
            "SELECT * FROM people WHERE id NOT IN (1, 2, 3)",
            vec![4, 5, 6, 7, 8, 9, 10],
        ),
        // `x NOT IN (1, NULL)` is unknown for any x but 1, and false for 1.
        ("SELECT * FROM people WHERE id NOT IN (1, NULL)", vec![]),
        ("SELECT * FROM people WHERE id IN (1, NULL)", vec![1]),
        (
            "SELECT * FROM people WHERE age NOT IN (10, 20) AND id < 6",
            vec![4, 5],
        ),
        (
            "SELECT * FROM people WHERE id NOT BETWEEN 2 AND 9",
            vec![1, 10],
        ),
        // `id >= 4 AND id <= NULL`: false below 4, unknown from 4.
        (
            "SELECT * FROM people WHERE id NOT BETWEEN 4 AND NULL",
            vec![1, 2, 3],
        ),
        ("SELECT * FROM people WHERE id BETWEEN 4 AND NULL", vec![]),
        (
            "SELECT * FROM people WHERE age NOT BETWEEN 20 AND 90",
            vec![1, 10],
        ),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

// ------------------------------------------------- errors

#[test]
fn values_of_another_type_are_type_errors_even_on_an_empty_table() {
    let db = new_db();
    run(
        &db,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, age INTEGER, name TEXT)",
    );
    for (sql, column, expected, found) in [
        (
            "SELECT * FROM people WHERE age = 'ten'",
            "age",
            ColumnType::Integer,
            ColumnType::Text,
        ),
        (
            "SELECT * FROM people WHERE 'ten' < age",
            "age",
            ColumnType::Integer,
            ColumnType::Text,
        ),
        (
            "SELECT * FROM people WHERE name = 1",
            "name",
            ColumnType::Text,
            ColumnType::Integer,
        ),
        (
            "SELECT * FROM people WHERE name = age",
            "name",
            ColumnType::Text,
            ColumnType::Integer,
        ),
        (
            "SELECT * FROM people WHERE id IN (1, 'a')",
            "id",
            ColumnType::Integer,
            ColumnType::Text,
        ),
        (
            "SELECT * FROM people WHERE id BETWEEN 1 AND X'00'",
            "id",
            ColumnType::Integer,
            ColumnType::Blob,
        ),
    ] {
        match fail(&db, sql) {
            SqlError::TypeMismatch {
                column: c,
                expected: e,
                found: f,
            } => {
                assert_eq!((c.as_str(), e, f), (column, expected, Some(found)), "{sql}");
            }
            other => panic!("{sql}: expected a type mismatch, got {other:?}"),
        }
    }
    for sql in [
        "SELECT * FROM people WHERE name + 1 = 2",
        "SELECT * FROM people WHERE -name = 'a'",
        "SELECT * FROM people WHERE id + 1 = 'a'",
        "SELECT * FROM people WHERE id",
        "SELECT * FROM people WHERE id + 1",
        "SELECT * FROM people WHERE NULL",
        "SELECT * FROM people WHERE 'yes'",
        "SELECT * FROM people WHERE (id = 1) = (age = 2)",
        "SELECT * FROM people WHERE NOT id",
        "SELECT * FROM people WHERE id = 1 AND age",
        "SELECT * FROM people WHERE (id = 1) + 1 = 2",
        "SELECT * FROM people WHERE (id = 1) IS NULL",
        "SELECT * FROM people WHERE id IN (1, id = 2)",
    ] {
        assert!(
            matches!(fail(&db, sql), SqlError::Type(_)),
            "{sql}: expected a type error"
        );
    }
}

#[test]
fn unknown_columns_in_where_are_errors_even_on_an_empty_table() {
    let db = new_db();
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    for sql in [
        "SELECT * FROM t WHERE nope = 1",
        "SELECT * FROM t WHERE id = 1 OR nope IS NULL",
        "SELECT * FROM t WHERE id IN (nope)",
    ] {
        assert!(
            matches!(fail(&db, sql), SqlError::NoSuchColumn { ref column, .. } if column == "nope"),
            "{sql}"
        );
    }
}

#[test]
fn overflow_and_division_by_zero_are_errors_on_the_rows_read() {
    let db = people();
    for sql in [
        "SELECT * FROM people WHERE age / 0 = 1",
        "SELECT * FROM people WHERE age % 0 = 1",
        "SELECT * FROM people WHERE id + 9223372036854775807 > 0",
        "SELECT * FROM people WHERE id * -9223372036854775808 > 0",
        "SELECT * FROM people WHERE -9223372036854775808 - id < 0",
        "SELECT * FROM people WHERE -(-9223372036854775808 + id - 1) > 0",
        "SELECT * FROM people WHERE (-9223372036854775808 + id - 1) / -1 > 0",
        "SELECT * FROM people WHERE (-9223372036854775808 + id - 1) % -1 = 0",
        // A constant that overflows is not folded into the range: the
        // filter reaches it on the first row.
        "SELECT * FROM people WHERE id > 9223372036854775807 + 1",
    ] {
        assert!(
            matches!(fail(&db, sql), SqlError::Arithmetic(_)),
            "{sql}: expected an arithmetic error"
        );
    }
    // NULL / 0 is NULL: the division never happens.
    assert!(ids(&db, "SELECT * FROM people WHERE age / 0 = 1 AND id = 3").is_empty());
    // On rows the statement never reads, nothing is computed.
    assert!(ids(
        &db,
        "SELECT * FROM people WHERE id > 100 AND 1 / (id - id) = 1"
    )
    .is_empty());
    let empty = new_db();
    run(&empty, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    assert!(ids(&empty, "SELECT * FROM t WHERE id > 9223372036854775807 + 1").is_empty());
}

#[test]
fn a_negative_literal_is_not_an_overflow() {
    let db = new_db();
    run(&db, "CREATE TABLE n (id INTEGER PRIMARY KEY)");
    run(&db, "INSERT INTO n VALUES (-9223372036854775808)");
    assert_eq!(
        ids(&db, "SELECT * FROM n WHERE id = -9223372036854775808"),
        vec![i64::MIN]
    );
    assert_eq!(
        ids(&db, "SELECT * FROM n WHERE id = - 9223372036854775808"),
        vec![i64::MIN]
    );
}

#[test]
fn sql_oakdb_does_not_support_in_where_names_what_it_found() {
    let db = people();
    for (sql, named) in [
        ("SELECT * FROM people WHERE name LIKE 'p%'", "LIKE"),
        ("SELECT * FROM people WHERE id < age < 3", "<"),
        ("SELECT * FROM people WHERE (id = 1 = 1)", "="),
        ("SELECT * FROM people WHERE age IS 3", "IS"),
        ("SELECT * FROM people WHERE age IS NOT 3", "IS"),
        ("SELECT * FROM people WHERE id = 1 ORDER BY id", "ORDER"),
        ("SELECT * FROM people ORDER BY id", "ORDER"),
    ] {
        match fail(&db, sql) {
            SqlError::Unsupported(what) => assert!(what.contains(named), "{sql}: {what}"),
            other => panic!("{sql}: expected unsupported, got {other:?}"),
        }
    }
}

#[test]
fn broken_conditions_are_syntax_errors_that_say_where() {
    let db = people();
    for (sql, at) in [
        ("SELECT * FROM people WHERE (id = 1", 34),
        ("SELECT * FROM people WHERE id = ", 32),
        ("SELECT * FROM people WHERE id IN ()", 34),
        ("SELECT * FROM people WHERE id IN (1, 2", 38),
        ("SELECT * FROM people WHERE id BETWEEN 1 OR 2", 40),
        ("SELECT * FROM people WHERE AND id = 1", 27),
        ("SELECT * FROM people WHERE", 26),
    ] {
        match fail(&db, sql) {
            SqlError::Syntax { position, .. } => assert_eq!(position, at, "{sql}"),
            other => panic!("{sql}: expected a syntax error, got {other:?}"),
        }
    }
}

#[test]
fn keywords_of_where_are_case_insensitive() {
    let db = people();
    assert_eq!(
        ids(
            &db,
            "select * from people where id between 2 and 4 and not (age is null) or id in (10)"
        ),
        vec![2, 4, 10]
    );
}

#[test]
fn a_primary_key_longer_than_any_key_finds_nothing() {
    let db = new_db();
    run(&db, "CREATE TABLE t (k TEXT PRIMARY KEY)");
    let long = "x".repeat(10_000);
    assert!(rows(&db, &format!("SELECT * FROM t WHERE k = '{long}'")).is_empty());
}

// ------------------------------------------------- PR #69 review gaps

#[test]
fn unary_minus_on_text_is_a_type_error_even_with_rows() {
    // Review P5 (mutant V11): without the type check, `-name` would fail
    // only on a row, as corrupt data.
    let db = people();
    for sql in [
        "SELECT * FROM people WHERE -name = 1",
        "SELECT * FROM people WHERE -(name) IS NULL",
    ] {
        assert!(matches!(fail(&db, sql), SqlError::Type(_)), "{sql}");
    }
}

#[test]
fn between_stops_at_a_false_lower_bound() {
    // Review P5 (mutant V16): `0 BETWEEN 1 AND 1 / 0` is false without
    // computing the upper bound (design 0019, section 2-1).
    let db = people();
    assert!(ids(&db, "SELECT * FROM people WHERE 0 BETWEEN 1 AND 1 / 0").is_empty());
    assert_eq!(
        ids(
            &db,
            "SELECT * FROM people WHERE id < 3 AND 0 NOT BETWEEN 1 AND 1 / 0"
        ),
        vec![1, 2]
    );
    assert!(matches!(
        fail(&db, "SELECT * FROM people WHERE 5 BETWEEN 1 AND 1 / 0"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn null_divided_by_zero_is_null() {
    // Design 0019, section 2-2: NULL in arithmetic is NULL before the
    // divisor is looked at, so NULL / 0 and NULL % 0 are NULL, not errors.
    let db = people();
    let all: Vec<i64> = (1..=10).collect();
    assert_eq!(
        ids(&db, "SELECT * FROM people WHERE (NULL / 0) IS NULL"),
        all
    );
    assert_eq!(
        ids(&db, "SELECT * FROM people WHERE (NULL % 0) IS NULL"),
        all
    );
    assert_eq!(
        ids(
            &db,
            // The IN range reads ids 3 to 7; AND stops at a false left
            // side, so only the ages of 3 and 7, both NULL, are divided.
            "SELECT * FROM people WHERE id IN (3, 7) AND (age / 0) IS NULL"
        ),
        vec![3, 7]
    );
    // A row with an age divides by zero.
    assert!(matches!(
        fail(&db, "SELECT * FROM people WHERE (age / 0) IS NULL"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn text_keys_with_a_zero_byte_and_keys_that_start_others_range_exactly() {
    // Review P2: the end marker and the escaped 0x00 (design 0017, 2-1).
    let db = new_db();
    run(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, n INTEGER)");
    run(
        &db,
        "INSERT INTO s VALUES ('a', 1), ('a\0', 2), ('a\0b', 3), ('ab', 4), ('abc', 5), ('b', 6)",
    );
    let ns = |condition: &str| -> Vec<i64> {
        rows(&db, &format!("SELECT n FROM s WHERE {condition}"))
            .into_iter()
            .map(|row| match row[0] {
                Value::Integer(i) => i,
                ref other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(ns("k > 'a'"), vec![2, 3, 4, 5, 6]);
    assert_eq!(ns("k >= 'a\0'"), vec![2, 3, 4, 5, 6]);
    assert_eq!(ns("k > 'a\0'"), vec![3, 4, 5, 6]);
    assert_eq!(ns("k < 'ab'"), vec![1, 2, 3]);
    assert_eq!(ns("k <= 'ab'"), vec![1, 2, 3, 4]);
    assert_eq!(ns("k > 'ab' AND k < 'b'"), vec![5]);
    assert_eq!(ns("k = 'a\0'"), vec![2]);
    assert_eq!(ns("k BETWEEN 'a' AND 'a\0b'"), vec![1, 2, 3]);
}

#[test]
fn an_open_end_stops_before_the_next_table() {
    // Review P2: `id > v` runs to the end of its table, not into the next.
    let db = new_db();
    run(&db, "CREATE TABLE first (id INTEGER PRIMARY KEY)");
    run(&db, "CREATE TABLE second (id INTEGER PRIMARY KEY)");
    run(&db, "INSERT INTO first VALUES (1), (2), (3)");
    run(
        &db,
        "INSERT INTO second VALUES (-5), (0), (4), (9223372036854775807)",
    );
    assert_eq!(ids(&db, "SELECT * FROM first WHERE id > 1"), vec![2, 3]);
    assert_eq!(
        ids(&db, "SELECT * FROM first WHERE id >= -100"),
        vec![1, 2, 3]
    );
    assert_eq!(ids(&db, "SELECT * FROM second WHERE id < 1"), vec![-5, 0]);
    assert_eq!(
        ids(&db, "SELECT * FROM second WHERE id > 0"),
        vec![4, i64::MAX]
    );
}
