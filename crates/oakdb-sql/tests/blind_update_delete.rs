//! Blind tests for UPDATE and DELETE (issue #33, design 0022).
//!
//! Written from the issue, spec #12, designs 0017, 0019 and 0022 and the
//! public API only, without reading the implementation or its tests.

use std::collections::{BTreeMap, BTreeSet};

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, ColumnType, Output, SqlError, Value};

// ---------- helpers ----------

fn db() -> Db<SimFs> {
    Db::open_with(SimFs::new(), "/db").unwrap()
}

fn run(db: &Db<SimFs>, sql: &str) -> Output {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn err(db: &Db<SimFs>, sql: &str) -> SqlError {
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

fn i(n: i64) -> Value {
    Value::Integer(n)
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}

fn ids(db: &Db<SimFs>, table: &str) -> Vec<i64> {
    rows(db, &format!("SELECT id FROM {table}"))
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(n) => n,
            ref v => panic!("id is {v:?}"),
        })
        .collect()
}

/// A table `t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)` holding the given
/// ids, with `a = id * 10` and `b = 'r<id>'`.
fn table_with(ids: &[i64]) -> Db<SimFs> {
    let db = db();
    run(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
    );
    for &id in ids {
        run(
            &db,
            &format!("INSERT INTO t VALUES ({id}, {}, 'r{id}')", id * 10),
        );
    }
    db
}

fn all(db: &Db<SimFs>) -> Vec<Vec<Value>> {
    rows(db, "SELECT * FROM t")
}

// ---------- WHERE and no WHERE ----------

#[test]
fn update_with_where_changes_only_the_matching_rows() {
    let db = table_with(&[1, 2, 3, 4]);
    assert_eq!(
        run(&db, "UPDATE t SET a = 0 WHERE id >= 3"),
        Output::Affected(2)
    );
    assert_eq!(
        rows(&db, "SELECT id, a FROM t"),
        vec![
            vec![i(1), i(10)],
            vec![i(2), i(20)],
            vec![i(3), i(0)],
            vec![i(4), i(0)]
        ]
    );
}

#[test]
fn update_without_where_changes_every_row() {
    let db = table_with(&[1, 2, 3]);
    assert_eq!(run(&db, "UPDATE t SET b = 'x'"), Output::Affected(3));
    assert_eq!(rows(&db, "SELECT b FROM t"), vec![vec![t("x")]; 3]);
}

#[test]
fn delete_with_where_removes_only_the_matching_rows() {
    let db = table_with(&[1, 2, 3, 4, 5]);
    assert_eq!(
        run(&db, "DELETE FROM t WHERE id % 2 = 0"),
        Output::Affected(2)
    );
    assert_eq!(ids(&db, "t"), vec![1, 3, 5]);
}

#[test]
fn delete_without_where_empties_the_table_and_it_stays_usable() {
    let db = table_with(&[1, 2, 3]);
    assert_eq!(run(&db, "DELETE FROM t"), Output::Affected(3));
    assert!(all(&db).is_empty());
    run(&db, "INSERT INTO t VALUES (2, 0, 'again')");
    assert_eq!(ids(&db, "t"), vec![2]);
}

#[test]
fn a_trailing_semicolon_is_allowed_on_update_and_delete() {
    let db = table_with(&[1, 2]);
    assert_eq!(
        run(&db, "UPDATE t SET a = 1 WHERE id = 1;"),
        Output::Affected(1)
    );
    assert_eq!(run(&db, "DELETE FROM t WHERE id = 2;"), Output::Affected(1));
}

// ---------- affected counts ----------

#[test]
fn update_counts_matched_rows_even_when_the_value_does_not_change() {
    let db = table_with(&[1, 2, 3]);
    run(&db, "UPDATE t SET a = 1 WHERE id <= 2");
    assert_eq!(run(&db, "UPDATE t SET a = 1"), Output::Affected(3));
    assert_eq!(run(&db, "UPDATE t SET a = a"), Output::Affected(3));
}

#[test]
fn update_and_delete_matching_nothing_affect_zero_rows() {
    let db = table_with(&[1, 2]);
    assert_eq!(
        run(&db, "UPDATE t SET a = 5 WHERE id > 100"),
        Output::Affected(0)
    );
    assert_eq!(
        run(&db, "DELETE FROM t WHERE id > 100"),
        Output::Affected(0)
    );
    let empty = table_with(&[]);
    assert_eq!(run(&empty, "UPDATE t SET a = 5"), Output::Affected(0));
    assert_eq!(run(&empty, "DELETE FROM t"), Output::Affected(0));
    assert_eq!(ids(&db, "t"), vec![1, 2]);
}

#[test]
fn deleting_the_same_rows_twice_affects_zero_the_second_time() {
    let db = table_with(&[1, 2, 3]);
    assert_eq!(run(&db, "DELETE FROM t WHERE id < 3"), Output::Affected(2));
    assert_eq!(run(&db, "DELETE FROM t WHERE id < 3"), Output::Affected(0));
}

// ---------- Halloween ----------

#[test]
fn shifting_keys_up_by_ten_moves_each_row_exactly_once() {
    let ids_before: Vec<i64> = (1..30).collect();
    let db = table_with(&ids_before);
    assert_eq!(
        run(&db, "UPDATE t SET id = id + 10 WHERE id < 30"),
        Output::Affected(29)
    );
    assert_eq!(ids(&db, "t"), (11..40).collect::<Vec<_>>());
    // Each row kept its other columns: a = old id * 10.
    for r in all(&db) {
        let (Value::Integer(id), Value::Integer(a)) = (&r[0], &r[1]) else {
            panic!()
        };
        assert_eq!(*a, (id - 10) * 10);
        assert_eq!(r[2], t(&format!("r{}", id - 10)));
    }
}

#[test]
fn doubling_every_key_moves_each_row_exactly_once() {
    let db = table_with(&(1..=10).collect::<Vec<_>>());
    assert_eq!(run(&db, "UPDATE t SET id = id * 2"), Output::Affected(10));
    assert_eq!(ids(&db, "t"), (1..=10).map(|n| n * 2).collect::<Vec<_>>());
    assert_eq!(
        rows(&db, "SELECT a FROM t WHERE id = 20"),
        vec![vec![i(100)]]
    );
}

#[test]
fn the_design_example_of_adding_a_thousand_from_one_hundred_up_terminates() {
    let db = table_with(&(100..200).collect::<Vec<_>>());
    assert_eq!(
        run(&db, "UPDATE t SET id = id + 1000 WHERE id >= 100"),
        Output::Affected(100)
    );
    assert_eq!(ids(&db, "t"), (1100..1200).collect::<Vec<_>>());
}

#[test]
fn descending_shifts_move_each_row_once() {
    let db = table_with(&(10..20).collect::<Vec<_>>());
    assert_eq!(run(&db, "UPDATE t SET id = id - 5"), Output::Affected(10));
    assert_eq!(ids(&db, "t"), (5..15).collect::<Vec<_>>());
    assert_eq!(run(&db, "UPDATE t SET id = -id"), Output::Affected(10));
    assert_eq!(ids(&db, "t"), (-14..=-5).collect::<Vec<_>>());
}

// ---------- primary key changes ----------

#[test]
fn a_moved_row_is_readable_at_its_new_key_and_gone_at_its_old_one() {
    let db = table_with(&[1, 2, 3]);
    assert_eq!(
        run(&db, "UPDATE t SET id = 50 WHERE id = 2"),
        Output::Affected(1)
    );
    assert_eq!(
        rows(&db, "SELECT * FROM t WHERE id = 50"),
        vec![vec![i(50), i(20), t("r2")]]
    );
    assert!(rows(&db, "SELECT * FROM t WHERE id = 2").is_empty());
    assert_eq!(ids(&db, "t"), vec![1, 3, 50]);
    // The old key is free again.
    run(&db, "INSERT INTO t VALUES (2, 0, 'new')");
    assert_eq!(ids(&db, "t"), vec![1, 2, 3, 50]);
}

#[test]
fn rows_that_keep_their_key_and_rows_that_move_mix_in_one_statement() {
    let db = table_with(&[3, 4, 5]);
    // odd ids double, even ids stay: 3->6, 4->4, 5->10
    assert_eq!(
        run(&db, "UPDATE t SET id = id * (id % 2 + 1)"),
        Output::Affected(3)
    );
    assert_eq!(ids(&db, "t"), vec![4, 6, 10]);
    assert_eq!(
        rows(&db, "SELECT b FROM t WHERE id = 6"),
        vec![vec![t("r3")]]
    );
}

#[test]
fn text_primary_keys_can_be_updated_and_deleted() {
    let db = db();
    run(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, v INTEGER)");
    run(&db, "INSERT INTO s VALUES ('a', 1), ('b', 2), ('c', 3)");
    assert_eq!(
        run(&db, "UPDATE s SET k = 'z' WHERE k = 'a'"),
        Output::Affected(1)
    );
    assert_eq!(
        rows(&db, "SELECT * FROM s"),
        vec![vec![t("b"), i(2)], vec![t("c"), i(3)], vec![t("z"), i(1)]]
    );
    assert!(rows(&db, "SELECT * FROM s WHERE k = 'a'").is_empty());
    assert!(matches!(
        err(&db, "UPDATE s SET k = 'c' WHERE k = 'b'"),
        SqlError::DuplicateKey { .. }
    ));
    assert_eq!(
        run(&db, "DELETE FROM s WHERE k >= 'c'"),
        Output::Affected(2)
    );
    assert_eq!(rows(&db, "SELECT * FROM s"), vec![vec![t("b"), i(2)]]);
}

#[test]
fn a_new_text_key_longer_than_the_engine_allows_fails_and_leaves_the_table() {
    let db = db();
    run(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, v INTEGER)");
    run(&db, "INSERT INTO s VALUES ('a', 1)");
    let long = "x".repeat(oakdb::MAX_KEY_LEN + 10);
    let e = err(&db, &format!("UPDATE s SET k = '{long}'"));
    assert!(
        matches!(e, SqlError::Engine(oakdb::Error::KeyTooLarge { .. })),
        "{e:?}"
    );
    assert_eq!(rows(&db, "SELECT * FROM s"), vec![vec![t("a"), i(1)]]);
}

// ---------- after-statement duplicate check (3-1) ----------

#[test]
fn shifting_consecutive_keys_up_by_one_succeeds() {
    let db = table_with(&[1, 2, 3]);
    assert_eq!(run(&db, "UPDATE t SET id = id + 1"), Output::Affected(3));
    assert_eq!(ids(&db, "t"), vec![2, 3, 4]);
    assert_eq!(
        rows(&db, "SELECT b FROM t WHERE id = 2"),
        vec![vec![t("r1")]]
    );
}

#[test]
fn shifting_consecutive_keys_down_by_one_succeeds() {
    let db = table_with(&[1, 2, 3]);
    assert_eq!(run(&db, "UPDATE t SET id = id - 1"), Output::Affected(3));
    assert_eq!(ids(&db, "t"), vec![0, 1, 2]);
    assert_eq!(
        rows(&db, "SELECT b FROM t WHERE id = 2"),
        vec![vec![t("r3")]]
    );
}

#[test]
fn swapping_two_keys_with_one_expression_succeeds() {
    let db = table_with(&[1, 2]);
    assert_eq!(run(&db, "UPDATE t SET id = 3 - id"), Output::Affected(2));
    assert_eq!(
        all(&db),
        vec![vec![i(1), i(20), t("r2")], vec![i(2), i(10), t("r1")]]
    );
}

#[test]
fn moving_onto_an_unmatched_existing_row_fails_and_changes_nothing() {
    let db = table_with(&[1, 2, 3]);
    let before = all(&db);
    let e = err(&db, "UPDATE t SET id = id + 1 WHERE id < 3");
    assert!(
        matches!(&e, SqlError::DuplicateKey { table } if table == "t"),
        "{e:?}"
    );
    assert_eq!(all(&db), before);
}

#[test]
fn moving_onto_a_matched_row_that_keeps_its_key_fails() {
    let db = table_with(&[0, 5]);
    let before = all(&db);
    assert!(matches!(
        err(&db, "UPDATE t SET id = id * 0"),
        SqlError::DuplicateKey { .. }
    ));
    assert_eq!(all(&db), before);
}

#[test]
fn two_rows_moved_onto_the_same_key_fail_and_change_nothing() {
    let db = table_with(&[1, 2, 3]);
    let before = all(&db);
    assert!(matches!(
        err(&db, "UPDATE t SET id = 0"),
        SqlError::DuplicateKey { .. }
    ));
    assert_eq!(all(&db), before);
    let db = table_with(&[2, 3]);
    let before = all(&db);
    assert!(matches!(
        err(&db, "UPDATE t SET id = id / 2"),
        SqlError::DuplicateKey { .. }
    ));
    assert_eq!(all(&db), before);
}

#[test]
fn moving_a_single_row_onto_its_own_key_is_not_a_duplicate() {
    let db = table_with(&[1, 2]);
    assert_eq!(run(&db, "UPDATE t SET id = id"), Output::Affected(2));
    assert_eq!(
        run(&db, "UPDATE t SET id = id + 0, a = 7 WHERE id = 1"),
        Output::Affected(1)
    );
    assert_eq!(rows(&db, "SELECT a FROM t WHERE id = 1"), vec![vec![i(7)]]);
}

// ---------- SET sees the old row ----------

#[test]
fn set_a_equals_b_and_b_equals_a_swaps_them() {
    let db = db();
    run(
        &db,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
    );
    run(&db, "INSERT INTO p VALUES (1, 1, 2), (2, 3, NULL)");
    assert_eq!(run(&db, "UPDATE p SET a = b, b = a"), Output::Affected(2));
    assert_eq!(
        rows(&db, "SELECT * FROM p"),
        vec![vec![i(1), i(2), i(1)], vec![i(2), Value::Null, i(3)]]
    );
}

#[test]
fn every_set_expression_and_the_where_see_the_old_key() {
    let db = table_with(&[1, 2]);
    assert_eq!(
        run(&db, "UPDATE t SET id = id + 100, a = id WHERE id = 1"),
        Output::Affected(1)
    );
    assert_eq!(
        rows(&db, "SELECT a FROM t WHERE id = 101"),
        vec![vec![i(1)]]
    );
    assert_eq!(
        run(&db, "UPDATE t SET a = id, id = id + 100 WHERE id = 2"),
        Output::Affected(1)
    );
    assert_eq!(
        rows(&db, "SELECT a FROM t WHERE id = 102"),
        vec![vec![i(2)]]
    );
}

#[test]
fn setting_the_same_column_twice_is_a_duplicate_column_error() {
    let db = table_with(&[1]);
    let before = all(&db);
    let e = err(&db, "UPDATE t SET a = 1, a = 2");
    assert!(
        matches!(&e, SqlError::DuplicateColumn(c) if c == "a"),
        "{e:?}"
    );
    assert!(matches!(
        err(&db, "UPDATE t SET id = 1, b = 'x', id = 2"),
        SqlError::DuplicateColumn(_)
    ));
    assert_eq!(all(&db), before);
}

// ---------- types are checked before any row is read ----------

fn type_errors_on(db: &Db<SimFs>) {
    let e = err(db, "UPDATE t SET a = 'ten'");
    assert!(
        matches!(&e, SqlError::TypeMismatch { column, expected: ColumnType::Integer, found: Some(ColumnType::Text) } if column == "a"),
        "{e:?}"
    );
    let e = err(db, "UPDATE t SET b = id + 1");
    assert!(
        matches!(&e, SqlError::TypeMismatch { column, expected: ColumnType::Text, found: Some(ColumnType::Integer) } if column == "b"),
        "{e:?}"
    );
    let e = err(db, "UPDATE t SET a = b");
    assert!(
        matches!(&e, SqlError::TypeMismatch { column, .. } if column == "a"),
        "{e:?}"
    );
    let e = err(db, "UPDATE t SET id = NULL");
    assert!(
        // Issue #34 (design 0027 E3): NullKey, was TypeMismatch { found: None }.
        matches!(&e, SqlError::NullKey { column } if column == "id"),
        "{e:?}"
    );
    let e = err(db, "UPDATE t SET a = (a = 1)");
    assert!(matches!(e, SqlError::Type(_)), "{e:?}");
    let e = err(db, "UPDATE t SET a = a = 1");
    assert!(matches!(e, SqlError::Type(_)), "{e:?}");
    let e = err(db, "UPDATE t SET nope = 1");
    assert!(
        matches!(&e, SqlError::NoSuchColumn { column, .. } if column == "nope"),
        "{e:?}"
    );
    let e = err(db, "UPDATE t SET a = nope");
    assert!(
        matches!(&e, SqlError::NoSuchColumn { column, .. } if column == "nope"),
        "{e:?}"
    );
    let e = err(db, "UPDATE t SET a = 1 WHERE b + 1 = 2");
    assert!(matches!(e, SqlError::Type(_)), "{e:?}");
    let e = err(db, "DELETE FROM t WHERE a");
    assert!(matches!(e, SqlError::Type(_)), "{e:?}");
    let e = err(db, "DELETE FROM t WHERE nope = 1");
    assert!(matches!(e, SqlError::NoSuchColumn { .. }), "{e:?}");
    assert!(matches!(
        err(db, "UPDATE nope SET a = 1"),
        SqlError::NoSuchTable(_)
    ));
    assert!(matches!(
        err(db, "DELETE FROM nope"),
        SqlError::NoSuchTable(_)
    ));
}

#[test]
fn set_type_errors_are_found_on_an_empty_table() {
    type_errors_on(&table_with(&[]));
}

#[test]
fn set_type_errors_are_found_even_when_the_where_matches_nothing_and_change_nothing() {
    let db = table_with(&[1, 2]);
    let before = all(&db);
    type_errors_on(&db);
    let e = err(&db, "UPDATE t SET a = 'x' WHERE id > 1000");
    assert!(matches!(e, SqlError::TypeMismatch { .. }), "{e:?}");
    assert_eq!(all(&db), before);
}

// ---------- errors midway leave the table unchanged ----------

#[test]
fn overflow_in_the_third_row_leaves_every_row_as_it_was() {
    let db = db();
    run(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
    );
    run(
        &db,
        &format!(
            "INSERT INTO t VALUES (1, 1, 'x'), (2, 2, 'y'), (3, {}, 'z')",
            i64::MAX
        ),
    );
    let before = all(&db);
    assert!(matches!(
        err(&db, "UPDATE t SET a = a + 1"),
        SqlError::Arithmetic(_)
    ));
    assert_eq!(all(&db), before);
    assert!(matches!(
        err(&db, "UPDATE t SET b = 'changed', a = a * 2"),
        SqlError::Arithmetic(_)
    ));
    assert_eq!(all(&db), before);
}

#[test]
fn division_by_zero_in_a_middle_row_leaves_every_row_as_it_was() {
    let db = table_with(&[1, 2, 3]);
    let before = all(&db);
    assert!(matches!(
        err(&db, "UPDATE t SET a = 10 / (id - 2)"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(&db, "UPDATE t SET id = id % (id - 2) + 100"),
        SqlError::Arithmetic(_)
    ));
    assert_eq!(all(&db), before);
}

#[test]
fn a_primary_key_that_becomes_null_at_run_time_fails_and_changes_nothing() {
    let db = table_with(&[1, 2, 3]);
    run(&db, "UPDATE t SET a = NULL WHERE id = 3");
    let before = all(&db);
    let e = err(&db, "UPDATE t SET id = a");
    assert!(
        // Issue #34 (design 0027 E3): NullKey, was TypeMismatch { found: None }.
        matches!(&e, SqlError::NullKey { column } if column == "id"),
        "{e:?}"
    );
    assert_eq!(all(&db), before);
    // Rows whose a is not NULL can move.
    assert_eq!(
        run(&db, "UPDATE t SET id = a WHERE a IS NOT NULL"),
        Output::Affected(2)
    );
    assert_eq!(ids(&db, "t"), vec![3, 10, 20]);
}

// ---------- NULL ----------

#[test]
fn null_can_be_set_into_a_non_key_column_and_arithmetic_on_it_stays_null() {
    let db = table_with(&[1, 2]);
    assert_eq!(
        run(&db, "UPDATE t SET a = NULL, b = NULL WHERE id = 1"),
        Output::Affected(1)
    );
    assert_eq!(run(&db, "UPDATE t SET a = a + 1"), Output::Affected(2));
    assert_eq!(
        all(&db),
        vec![
            vec![i(1), Value::Null, Value::Null],
            vec![i(2), i(21), t("r2")]
        ]
    );
}

#[test]
fn delete_with_not_keeps_rows_whose_condition_is_unknown() {
    let db = db();
    run(
        &db,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
    );
    run(
        &db,
        "INSERT INTO u VALUES (1, 'kim', 20), (2, 'lee', NULL), (3, NULL, 30), (4, NULL, NULL), (5, 'park', 40)",
    );
    assert_eq!(
        run(&db, "DELETE FROM u WHERE NOT (name = 'kim')"),
        Output::Affected(2)
    );
    assert_eq!(ids(&db, "u"), vec![1, 3, 4]);
    // 3 NOT IN (1, NULL) is unknown, never true.
    assert_eq!(
        run(&db, "DELETE FROM u WHERE id NOT IN (1, NULL)"),
        Output::Affected(0)
    );
    assert_eq!(
        run(&db, "DELETE FROM u WHERE age NOT BETWEEN 0 AND 25"),
        Output::Affected(1)
    );
    assert_eq!(ids(&db, "u"), vec![1, 4]);
    assert_eq!(
        run(&db, "DELETE FROM u WHERE name IS NULL"),
        Output::Affected(1)
    );
    assert_eq!(ids(&db, "u"), vec![1]);
}

#[test]
fn update_with_not_skips_rows_whose_condition_is_unknown() {
    let db = db();
    run(&db, "CREATE TABLE u (id INTEGER PRIMARY KEY, age INTEGER)");
    run(&db, "INSERT INTO u VALUES (1, 10), (2, NULL), (3, 50)");
    assert_eq!(
        run(&db, "UPDATE u SET age = 0 WHERE NOT (age > 20)"),
        Output::Affected(1)
    );
    assert_eq!(
        rows(&db, "SELECT age FROM u"),
        vec![vec![i(0)], vec![Value::Null], vec![i(50)]]
    );
}

// ---------- unsupported forms ----------

#[test]
fn the_unsupported_forms_of_design_section_two_are_named_as_unsupported() {
    let db = table_with(&[1]);
    let before = all(&db);
    let mut wrong = Vec::new();
    for sql in [
        "UPDATE OR REPLACE t SET a = 1",
        "UPDATE t SET a = 1 FROM t",
        "UPDATE t SET a = 1 RETURNING a",
        "DELETE FROM t RETURNING id",
        "DELETE FROM t LIMIT 1",
        "DELETE FROM t ORDER BY id",
        "UPDATE t SET a = 1 ORDER BY id",
        "UPDATE t SET (a, b) = (1, 'x')",
        "UPDATE t SET a = DEFAULT",
        "DELETE t",
        "TRUNCATE t",
    ] {
        match execute(&db, sql) {
            Err(SqlError::Unsupported(_)) => {}
            other => wrong.push(format!("{sql} -> {other:?}")),
        }
    }
    assert_eq!(all(&db), before);
    assert!(
        wrong.is_empty(),
        "not reported as Unsupported:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn update_set_and_delete_are_reserved_words() {
    let db = db();
    assert!(execute(&db, "CREATE TABLE set (id INTEGER PRIMARY KEY)").is_err());
    assert!(execute(
        &db,
        "CREATE TABLE x (id INTEGER PRIMARY KEY, update INTEGER)"
    )
    .is_err());
    assert!(execute(&db, "CREATE TABLE delete (id INTEGER PRIMARY KEY)").is_err());
}

#[test]
fn malformed_update_and_delete_are_syntax_errors() {
    let db = table_with(&[1]);
    let mut wrong = Vec::new();
    for sql in [
        "UPDATE t SET",
        "UPDATE t SET a",
        "UPDATE t SET a = 1,",
        "DELETE FROM",
        "UPDATE t SET a = 1 WHERE",
    ] {
        let e = err(&db, sql);
        if !matches!(e, SqlError::Syntax { .. }) {
            wrong.push(format!("{sql} -> {e:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "not reported as Syntax:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn update_without_set_is_rejected_by_name() {
    // Design 0022 does not say whether a missing SET is a syntax error or an
    // unsupported form; the blind tester first expected Syntax. Either is fine
    // as long as nothing changes.
    let db = table_with(&[1]);
    let before = all(&db);
    let e = err(&db, "UPDATE t a = 1");
    assert!(
        matches!(e, SqlError::Syntax { .. } | SqlError::Unsupported(_)),
        "{e:?}"
    );
    assert_eq!(all(&db), before);
}

// ---------- persistence ----------

#[test]
fn updates_and_deletes_survive_reopening_and_a_crash() {
    let fs = SimFs::new();
    {
        let db = Db::open_with(fs.clone(), "/db").unwrap();
        run(
            &db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        );
        run(
            &db,
            "INSERT INTO t VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c')",
        );
        run(&db, "UPDATE t SET id = id + 10, a = a * 100 WHERE id >= 2");
        run(&db, "DELETE FROM t WHERE id = 1");
    }
    let expected = vec![vec![i(12), i(200), t("b")], vec![i(13), i(300), t("c")]];
    {
        let db = Db::open_with(fs.clone(), "/db").unwrap();
        assert_eq!(all(&db), expected);
        run(&db, "UPDATE t SET b = 'z' WHERE id = 13");
        // No close: every acknowledged commit must survive the crash below.
        drop(db);
    }
    fs.crash(7);
    let db = Db::open_with(fs.clone(), "/db").unwrap();
    assert_eq!(
        all(&db),
        vec![vec![i(12), i(200), t("b")], vec![i(13), i(300), t("z")]]
    );
    assert!(rows(&db, "SELECT * FROM t WHERE id = 2").is_empty());
}

// ---------- concurrency ----------

#[test]
fn concurrent_increments_of_one_row_with_retries_lose_no_update() {
    let db = table_with(&[1]);
    run(&db, "UPDATE t SET a = 0");
    const THREADS: i64 = 4;
    const EACH: i64 = 50;
    let conflicts = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..EACH {
                    loop {
                        match execute(&db, "UPDATE t SET a = a + 1 WHERE id = 1") {
                            Ok(out) => {
                                assert_eq!(out, Output::Affected(1));
                                break;
                            }
                            Err(SqlError::Engine(oakdb::Error::Conflict { .. })) => {
                                conflicts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(e) => panic!("unexpected error {e:?}"),
                        }
                    }
                }
            });
        }
    });
    assert_eq!(
        rows(&db, "SELECT a FROM t WHERE id = 1"),
        vec![vec![i(THREADS * EACH)]]
    );
}

#[test]
fn concurrent_deletes_and_updates_of_one_row_end_consistently() {
    // Each thread tries once (with retry on conflict) to delete or update row 1.
    // Whatever the interleaving, the row ends either deleted or updated, and
    // exactly one DELETE affects it if it is gone.
    let db = table_with(&[1]);
    let deleted = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|s| {
        for k in 0..4 {
            let db = &db;
            let deleted = &deleted;
            s.spawn(move || {
                let sql = if k % 2 == 0 {
                    "DELETE FROM t WHERE id = 1"
                } else {
                    "UPDATE t SET a = a + 1 WHERE id = 1"
                };
                loop {
                    match execute(db, sql) {
                        Ok(Output::Affected(n)) => {
                            if k % 2 == 0 {
                                deleted.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            }
                            break;
                        }
                        Err(SqlError::Engine(oakdb::Error::Conflict { .. })) => continue,
                        other => panic!("{other:?}"),
                    }
                }
            });
        }
    });
    assert_eq!(deleted.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(all(&db).is_empty());
}

// ---------- model comparison ----------

/// Deterministic xorshift generator.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
}

type Row = (Option<i64>, Option<String>);
type Model = BTreeMap<i64, Row>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Duplicate,
    NullKey,
    Arithmetic,
}

fn kind_of(e: &SqlError) -> Option<Kind> {
    match e {
        SqlError::DuplicateKey { .. } => Some(Kind::Duplicate),
        // Issue #34 (design 0027 E3) moved a NULL key from TypeMismatch
        // to NullKey.
        SqlError::NullKey { column } if column == "id" => Some(Kind::NullKey),
        SqlError::Arithmetic(_) => Some(Kind::Arithmetic),
        _ => None,
    }
}

/// Three-valued truth.
type Tri = Option<bool>;

#[derive(Debug, Clone)]
enum Cond {
    IdLt(i64),
    IdGe(i64),
    IdBetween(i64, i64),
    AIsNull,
    NotAEq(i64),
    AGt(i64),
    IdEven,
    BEq(&'static str),
    NotBEq(&'static str),
    Or(Box<Cond>, Box<Cond>),
}

impl Cond {
    fn sql(&self) -> String {
        match self {
            Cond::IdLt(k) => format!("id < {k}"),
            Cond::IdGe(k) => format!("id >= {k}"),
            Cond::IdBetween(x, y) => format!("id BETWEEN {x} AND {y}"),
            Cond::AIsNull => "a IS NULL".into(),
            Cond::NotAEq(k) => format!("NOT (a = {k})"),
            Cond::AGt(k) => format!("a > {k}"),
            Cond::IdEven => "id % 2 = 0".into(),
            Cond::BEq(s) => format!("b = '{s}'"),
            Cond::NotBEq(s) => format!("NOT (b = '{s}')"),
            Cond::Or(x, y) => format!("({}) OR ({})", x.sql(), y.sql()),
        }
    }
    fn eval(&self, id: i64, r: &Row) -> Tri {
        match self {
            Cond::IdLt(k) => Some(id < *k),
            Cond::IdGe(k) => Some(id >= *k),
            Cond::IdBetween(x, y) => Some(id >= *x && id <= *y),
            Cond::AIsNull => Some(r.0.is_none()),
            Cond::NotAEq(k) => r.0.map(|a| a != *k),
            Cond::AGt(k) => r.0.map(|a| a > *k),
            Cond::IdEven => Some(id % 2 == 0),
            Cond::BEq(s) => r.1.as_ref().map(|b| b == s),
            Cond::NotBEq(s) => r.1.as_ref().map(|b| b != s),
            Cond::Or(x, y) => match (x.eval(id, r), y.eval(id, r)) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
        }
    }
}

#[derive(Debug, Clone)]
enum IdSet {
    Add(i64),
    Neg,
    Const(i64),
    FromA,
    ThreeMinus,
    DoubleOdd,
}

#[derive(Debug, Clone)]
enum ASet {
    Add(i64),
    Mul(i64),
    Null,
    FromId,
    Const(i64),
}

fn gen_cond(rng: &mut Rng) -> Cond {
    match rng.below(10) {
        0 => Cond::IdLt(rng.range(-3, 25)),
        1 => Cond::IdGe(rng.range(-3, 25)),
        2 => {
            let x = rng.range(-3, 20);
            Cond::IdBetween(x, x + rng.range(0, 8))
        }
        3 => Cond::AIsNull,
        4 => Cond::NotAEq(rng.range(0, 5)),
        5 => Cond::AGt(rng.range(0, 5)),
        6 => Cond::IdEven,
        7 => Cond::BEq(["x", "y"][rng.below(2) as usize]),
        8 => Cond::NotBEq(["x", "y"][rng.below(2) as usize]),
        _ => Cond::Or(Box::new(gen_leaf(rng)), Box::new(gen_leaf(rng))),
    }
}

fn gen_leaf(rng: &mut Rng) -> Cond {
    loop {
        let c = gen_cond(rng);
        if !matches!(c, Cond::Or(..)) {
            return c;
        }
    }
}

fn gen_a(rng: &mut Rng) -> Option<i64> {
    match rng.below(8) {
        0 => None,
        1 => Some(i64::MAX - rng.range(0, 1)),
        2 => Some(1 << 62),
        _ => Some(rng.range(0, 5)),
    }
}

fn a_sql(a: Option<i64>) -> String {
    a.map_or("NULL".into(), |n| n.to_string())
}

fn b_sql(b: &Option<String>) -> String {
    b.as_ref().map_or("NULL".into(), |s| format!("'{s}'"))
}

/// Outcome of one statement on the model: the affected count, or the set of
/// error kinds an implementation of design 0022 may report.
enum Expect {
    Affected(u64, Model),
    Error(BTreeSet<Kind>),
}

fn model_update(
    m: &Model,
    cond: &Option<Cond>,
    id_set: &Option<IdSet>,
    a_set: &Option<ASet>,
    b_set: &Option<Option<String>>,
) -> Expect {
    let mut per_row = BTreeSet::new();
    let mut moved: Vec<(i64, i64, Row)> = Vec::new();
    let mut matched = 0u64;
    for (&id, r) in m {
        if let Some(c) = cond {
            if c.eval(id, r) != Some(true) {
                continue;
            }
        }
        matched += 1;
        let mut errs = BTreeSet::new();
        let new_id = match id_set {
            None => Some(id),
            Some(IdSet::Add(k)) => id.checked_add(*k).or_else(|| {
                errs.insert(Kind::Arithmetic);
                None
            }),
            Some(IdSet::Neg) => id.checked_neg().or_else(|| {
                errs.insert(Kind::Arithmetic);
                None
            }),
            Some(IdSet::Const(k)) => Some(*k),
            Some(IdSet::FromA) => r.0.or_else(|| {
                errs.insert(Kind::NullKey);
                None
            }),
            Some(IdSet::ThreeMinus) => 3i64.checked_sub(id).or_else(|| {
                errs.insert(Kind::Arithmetic);
                None
            }),
            Some(IdSet::DoubleOdd) => (id % 2 + 1).checked_mul(id).or_else(|| {
                errs.insert(Kind::Arithmetic);
                None
            }),
        };
        let new_a = match a_set {
            None => r.0,
            Some(ASet::Add(k)) => match r.0 {
                None => None,
                Some(a) => a.checked_add(*k).or_else(|| {
                    errs.insert(Kind::Arithmetic);
                    None
                }),
            },
            Some(ASet::Mul(k)) => match r.0 {
                None => None,
                Some(a) => a.checked_mul(*k).or_else(|| {
                    errs.insert(Kind::Arithmetic);
                    None
                }),
            },
            Some(ASet::Null) => None,
            Some(ASet::FromId) => Some(id),
            Some(ASet::Const(k)) => Some(*k),
        };
        let new_b = match b_set {
            None => r.1.clone(),
            Some(b) => b.clone(),
        };
        if errs.is_empty() {
            moved.push((id, new_id.unwrap(), (new_a, new_b)));
        } else {
            per_row.extend(errs);
        }
    }
    // Duplicate check after the statement (3-1): new table = old - old keys of
    // matched rows + new keys.
    let mut new_table = m.clone();
    for (old, _, _) in &moved {
        new_table.remove(old);
    }
    let mut dup = false;
    let mut seen = BTreeSet::new();
    for (_, new, row) in &moved {
        if !seen.insert(*new) || new_table.contains_key(new) {
            dup = true;
        }
        new_table.insert(*new, row.clone());
    }
    if !per_row.is_empty() {
        // Some row failed; a duplicate among the rows that did not fail may be
        // found first by an implementation that checks as it goes.
        if dup {
            per_row.insert(Kind::Duplicate);
        }
        return Expect::Error(per_row);
    }
    if dup {
        return Expect::Error([Kind::Duplicate].into());
    }
    Expect::Affected(matched, new_table)
}

fn table_rows(m: &Model) -> Vec<Vec<Value>> {
    m.iter()
        .map(|(id, (a, b))| {
            vec![
                i(*id),
                a.map_or(Value::Null, Value::Integer),
                b.as_ref().map_or(Value::Null, |s| t(s)),
            ]
        })
        .collect()
}

fn run_model(seed: u64, steps: usize, seen: &mut BTreeMap<String, u64>) {
    let db = db();
    run(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
    );
    let mut m: Model = BTreeMap::new();
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    // Often start from consecutive keys to hit 3-1's edges.
    if rng.below(2) == 0 {
        let start = rng.range(-2, 5);
        for id in start..start + rng.range(2, 8) {
            let a = gen_a(&mut rng);
            let b = Some(["x", "y"][rng.below(2) as usize].to_string());
            run(
                &db,
                &format!("INSERT INTO t VALUES ({id}, {}, {})", a_sql(a), b_sql(&b)),
            );
            m.insert(id, (a, b));
        }
    }
    for step in 0..steps {
        let (sql, expect) = match rng.below(10) {
            0..=2 => {
                let id = rng.range(-3, 25);
                let a = gen_a(&mut rng);
                let b = match rng.below(3) {
                    0 => None,
                    1 => Some("x".to_string()),
                    _ => Some("y".to_string()),
                };
                let sql = format!("INSERT INTO t VALUES ({id}, {}, {})", a_sql(a), b_sql(&b));
                let e = if m.contains_key(&id) {
                    Expect::Error([Kind::Duplicate].into())
                } else {
                    let mut n = m.clone();
                    n.insert(id, (a, b));
                    Expect::Affected(1, n)
                };
                (sql, e)
            }
            3..=7 => {
                let cond = if rng.below(4) == 0 {
                    None
                } else {
                    Some(gen_cond(&mut rng))
                };
                let id_set = match rng.below(9) {
                    0 => Some(IdSet::Add(1)),
                    1 => Some(IdSet::Add(-1)),
                    2 => Some(IdSet::Add(rng.range(-10, 10))),
                    3 => Some(IdSet::Const(rng.range(-1, 3))),
                    4 => Some(IdSet::FromA),
                    5 => Some(IdSet::Neg),
                    6 => Some(IdSet::ThreeMinus),
                    7 => Some(IdSet::DoubleOdd),
                    _ => None,
                };
                let a_set = match rng.below(7) {
                    0 => Some(ASet::Add(rng.range(-2, 2))),
                    1 => Some(ASet::Mul(2)),
                    2 => Some(ASet::Null),
                    3 => Some(ASet::FromId),
                    4 => Some(ASet::Const(rng.range(0, 5))),
                    _ => None,
                };
                let b_set = match rng.below(4) {
                    0 => Some(None),
                    1 => Some(Some("x".to_string())),
                    2 => Some(Some("y".to_string())),
                    _ => None,
                };
                let (id_set, a_set, b_set) =
                    if id_set.is_none() && a_set.is_none() && b_set.is_none() {
                        (None, Some(ASet::Add(0)), None)
                    } else {
                        (id_set, a_set, b_set)
                    };
                let mut parts = Vec::new();
                // Mix the order of assignments; the result must not depend on it.
                if let Some(a) = &a_set {
                    parts.push(match a {
                        ASet::Add(k) => format!("a = a + ({k})"),
                        ASet::Mul(k) => format!("a = a * {k}"),
                        ASet::Null => "a = NULL".into(),
                        ASet::FromId => "a = id".into(),
                        ASet::Const(k) => format!("a = {k}"),
                    });
                }
                if let Some(s) = &id_set {
                    let p = match s {
                        IdSet::Add(k) => format!("id = id + ({k})"),
                        IdSet::Neg => "id = -id".into(),
                        IdSet::Const(k) => format!("id = {k}"),
                        IdSet::FromA => "id = a".into(),
                        IdSet::ThreeMinus => "id = 3 - id".into(),
                        IdSet::DoubleOdd => "id = id * (id % 2 + 1)".into(),
                    };
                    if rng.below(2) == 0 {
                        parts.insert(0, p)
                    } else {
                        parts.push(p)
                    }
                }
                if let Some(b) = &b_set {
                    parts.push(format!("b = {}", b_sql(b)));
                }
                let mut sql = format!("UPDATE t SET {}", parts.join(", "));
                if let Some(c) = &cond {
                    sql += &format!(" WHERE {}", c.sql());
                }
                let e = model_update(&m, &cond, &id_set, &a_set, &b_set);
                (sql, e)
            }
            _ => {
                let cond = if rng.below(6) == 0 {
                    None
                } else {
                    Some(gen_cond(&mut rng))
                };
                let mut sql = "DELETE FROM t".to_string();
                if let Some(c) = &cond {
                    sql += &format!(" WHERE {}", c.sql());
                }
                let mut n = m.clone();
                n.retain(|id, r| cond.as_ref().is_some_and(|c| c.eval(*id, r) != Some(true)));
                let count = (m.len() - n.len()) as u64;
                (sql, Expect::Affected(count, n))
            }
        };
        let got = execute(&db, &sql);
        let ctx = format!("seed {seed} step {step}: {sql}\nmodel before: {m:?}");
        match expect {
            Expect::Affected(count, n) => {
                assert_eq!(
                    got.as_ref().ok(),
                    Some(&Output::Affected(count)),
                    "{ctx}\ngot {got:?}"
                );
                if sql.starts_with("UPDATE") && sql.contains("SET id") && count > 1 && n != m {
                    *seen.entry("multi-row key update ok".into()).or_default() += 1;
                }
                m = n;
            }
            Expect::Error(kinds) => {
                let e = got.expect_err(&format!("{ctx}\nexpected one of {kinds:?}"));
                let k = kind_of(&e);
                assert!(
                    k.is_some_and(|k| kinds.contains(&k)),
                    "{ctx}\nexpected one of {kinds:?}, got {e:?}"
                );
                *seen
                    .entry(format!("{:?} on {}", k.unwrap(), &sql[..6]))
                    .or_default() += 1;
            }
        }
        assert_eq!(
            all(&db),
            table_rows(&m),
            "{ctx}\ntable differs after the statement"
        );
    }
}

#[test]
fn random_insert_update_delete_sequences_match_the_model() {
    let mut seen = BTreeMap::new();
    for seed in 1..=300 {
        run_model(seed, 40, &mut seen);
    }
    // The generator must actually reach every outcome it was written for.
    for key in [
        "multi-row key update ok",
        "Duplicate on UPDATE",
        "Duplicate on INSERT",
        "NullKey on UPDATE",
        "Arithmetic on UPDATE",
    ] {
        assert!(
            seen.get(key).copied().unwrap_or(0) > 0,
            "never saw {key}: {seen:?}"
        );
    }
    eprintln!("{seen:?}");
}
