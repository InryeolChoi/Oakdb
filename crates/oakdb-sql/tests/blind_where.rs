//! Blind tests for WHERE and range scans (issue #32).
//!
//! Written from issue #32, spec #12 and designs 0017 and 0019 only, through
//! the public `execute` API, without reading the implementation or the
//! PR's own tests. Row order is not promised (0019 Q4), so results are
//! sorted before comparing.

use oakdb::Db;
use oakdb_sql::{execute, ColumnType, Output, SqlError, Value};
use tempfile::TempDir;

const MIN: i64 = i64::MIN;
const MAX: i64 = i64::MAX;

fn open() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db")).unwrap();
    (dir, db)
}

fn ok(db: &Db, sql: &str) -> Output {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match ok(db, sql) {
        Output::Rows { mut rows, .. } => {
            rows.sort_by_key(|r| format!("{r:?}"));
            rows
        }
        other => panic!("{sql}: not rows: {other:?}"),
    }
}

fn err(db: &Db, sql: &str) -> SqlError {
    match execute(db, sql) {
        Ok(out) => panic!("{sql}: expected an error, got {out:?}"),
        Err(e) => e,
    }
}

/// Sorted ids of table `t` matching `cond`.
fn ids(db: &Db, cond: &str) -> Vec<i64> {
    ints(db, &format!("SELECT id FROM t WHERE {cond}"))
}

fn ints(db: &Db, sql: &str) -> Vec<i64> {
    let mut out: Vec<i64> = rows(db, sql)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => *i,
            v => panic!("{sql}: not an integer: {v:?}"),
        })
        .collect();
    out.sort();
    out
}

fn texts(db: &Db, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = rows(db, sql)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            v => panic!("{sql}: not text: {v:?}"),
        })
        .collect();
    out.sort();
    out
}

/// Table `t(id INTEGER PK, a INTEGER, b TEXT)`:
///
/// | id | a    | b      |
/// |----|------|--------|
/// | 1  | 1    | 'kim'  |
/// | 2  | 2    | 'lee'  |
/// | 3  | -7   | NULL   |
/// | 4  | NULL | 'kim'  |
/// | 5  | 5    | 'park' |
/// | 6  | 0    | ''     |
/// | 7  | NULL | NULL   |
/// | 8  | 10   | 'Kim'  |
fn people() -> (TempDir, Db) {
    let (dir, db) = open();
    ok(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
    );
    ok(
        &db,
        "INSERT INTO t VALUES (1, 1, 'kim'), (2, 2, 'lee'), (3, -7, NULL), (4, NULL, 'kim'), \
         (5, 5, 'park'), (6, 0, ''), (7, NULL, NULL), (8, 10, 'Kim')",
    );
    (dir, db)
}

/// Table `t(id INTEGER PK, a INTEGER)` with the integer edge keys.
fn edges() -> (TempDir, Db) {
    let (dir, db) = open();
    ok(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)");
    ok(
        &db,
        &format!(
            "INSERT INTO t VALUES ({MIN}, 0), (-5, 0), (-1, 0), (0, 0), (1, 0), (5, 0), ({MAX}, 0)"
        ),
    );
    (dir, db)
}

const EDGE_KEYS: [i64; 7] = [MIN, -5, -1, 0, 1, 5, MAX];

/// Table `s(k TEXT PK, n INTEGER)` with tricky text keys.
fn words() -> (TempDir, Db) {
    let (dir, db) = open();
    ok(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, n INTEGER)");
    ok(
        &db,
        "INSERT INTO s VALUES ('', 0), ('a', 1), ('ab', 2), ('abc', 3), ('b', 4), \
         ('it''s', 5), ('''', 6), ('''''', 7), ('B', 8), ('a b', 9)",
    );
    (dir, db)
}

fn keys(db: &Db, cond: &str) -> Vec<String> {
    texts(db, &format!("SELECT k FROM s WHERE {cond}"))
}

fn s(v: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- operators

#[test]
fn equals_on_a_non_key_column_picks_every_matching_row() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "b = 'kim'"), vec![1, 4]);
    assert_eq!(ids(&db, "a = 2"), vec![2]);
}

#[test]
fn both_spellings_of_not_equal_agree_and_skip_nulls() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a != 1"), vec![2, 3, 5, 6, 8]);
    assert_eq!(ids(&db, "a <> 1"), vec![2, 3, 5, 6, 8]);
    assert_eq!(ids(&db, "b <> 'kim'"), vec![2, 5, 6, 8]);
}

#[test]
fn the_four_order_comparisons_on_a_non_key_column() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a < 1"), vec![3, 6]);
    assert_eq!(ids(&db, "a <= 1"), vec![1, 3, 6]);
    assert_eq!(ids(&db, "a > 2"), vec![5, 8]);
    assert_eq!(ids(&db, "a >= 2"), vec![2, 5, 8]);
}

#[test]
fn text_compares_by_bytes_so_case_matters_and_empty_is_smallest() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "b < 'a'"), vec![6, 8]); // '' and 'Kim'
    assert_eq!(ids(&db, "b = 'KIM'"), Vec::<i64>::new());
    assert_eq!(ids(&db, "b >= 'kim'"), vec![1, 2, 4, 5]);
}

#[test]
fn and_or_and_not_combine_conditions() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a > 0 AND b = 'kim'"), vec![1]);
    assert_eq!(ids(&db, "a = 0 OR b = 'lee'"), vec![2, 6]);
    assert_eq!(ids(&db, "NOT a > 0"), vec![3, 6]);
}

#[test]
fn is_null_and_is_not_null_are_always_true_or_false() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a IS NULL"), vec![4, 7]);
    assert_eq!(ids(&db, "a IS NOT NULL"), vec![1, 2, 3, 5, 6, 8]);
    assert_eq!(ids(&db, "NOT b IS NULL"), vec![1, 2, 4, 5, 6, 8]);
}

#[test]
fn between_is_inclusive_on_both_ends_and_not_between_is_its_negation() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a BETWEEN 1 AND 5"), vec![1, 2, 5]);
    assert_eq!(ids(&db, "a NOT BETWEEN 1 AND 5"), vec![3, 6, 8]);
    assert_eq!(ids(&db, "b BETWEEN 'kim' AND 'lee'"), vec![1, 2, 4]);
}

#[test]
fn in_and_not_in_with_a_list_of_values() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a IN (0, 5, 99)"), vec![5, 6]);
    assert_eq!(ids(&db, "a NOT IN (0, 5, 99)"), vec![1, 2, 3, 8]);
    assert_eq!(ids(&db, "b IN ('lee', 'park')"), vec![2, 5]);
}

#[test]
fn in_accepts_expressions_in_its_list() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a IN (1 + 1, 2 * 5)"), vec![2, 8]);
    assert_eq!(ids(&db, "a IN (id, 99)"), vec![1, 2, 5]);
}

#[test]
fn integer_arithmetic_with_all_five_operators() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a + 1 = 3"), vec![2]);
    assert_eq!(ids(&db, "a - 1 = 4"), vec![5]);
    assert_eq!(ids(&db, "a * 3 = 30"), vec![8]);
    assert_eq!(ids(&db, "a / 2 = 2"), vec![5]);
    assert_eq!(ids(&db, "id % 2 = 0"), vec![2, 4, 6, 8]);
    assert_eq!(ids(&db, "a = id"), vec![1, 2, 5]);
}

#[test]
fn division_truncates_toward_zero_and_remainder_takes_the_dividends_sign() {
    let (_d, db) = people();
    // row 3 has a = -7
    assert_eq!(ids(&db, "a / 2 = -3"), vec![3]);
    assert_eq!(ids(&db, "a % 2 = -1"), vec![3]);
    assert_eq!(ids(&db, "id = 1 AND 7 % -2 = 1"), vec![1]);
    assert_eq!(ids(&db, "id = 1 AND -7 / -2 = 3"), vec![1]);
}

#[test]
fn unary_minus_negates_columns_and_expressions() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "-a = 7"), vec![3]);
    assert_eq!(ids(&db, "-(a + 1) = -2"), vec![1]);
    assert_eq!(ids(&db, "- - a = 5"), vec![5]);
    assert_eq!(ids(&db, "-a * 2 = -20"), vec![8]);
}

#[test]
fn and_binds_tighter_than_or() {
    let (_d, db) = people();
    // a = 1 OR (b = 'kim' AND id = 4)
    assert_eq!(ids(&db, "a = 1 OR b = 'kim' AND id = 4"), vec![1, 4]);
    // (a = 1 OR b = 'kim') AND id = 4
    assert_eq!(ids(&db, "(a = 1 OR b = 'kim') AND id = 4"), vec![4]);
}

#[test]
fn not_binds_looser_than_comparison_and_tighter_than_and() {
    let (_d, db) = people();
    // (NOT (a = 1)) AND b = 'kim' -> row 4 has a NULL, so NOT is unknown
    assert_eq!(ids(&db, "NOT a = 1 AND b = 'kim'"), Vec::<i64>::new());
    assert_eq!(ids(&db, "NOT a = 2 AND b = 'kim'"), vec![1]);
    // NOT (a = 2 OR b = 'kim')
    assert_eq!(ids(&db, "NOT (a = 2 OR b = 'kim')"), vec![5, 6, 8]);
}

#[test]
fn multiplication_binds_tighter_than_addition_and_parentheses_override() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a + 2 * 3 = 7"), vec![1]);
    assert_eq!(ids(&db, "(a + 2) * 3 = 9"), vec![1]);
    assert_eq!(ids(&db, "a - 2 - 1 = 2"), vec![5]); // left to right: (5 - 2) - 1
    assert_eq!(ids(&db, "a / 5 * 2 = 4"), vec![8]); // (10 / 5) * 2
}

#[test]
fn the_and_inside_between_belongs_to_between() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a BETWEEN 1 AND 5 AND b = 'kim'"), vec![1]);
    assert_eq!(ids(&db, "a BETWEEN 1 + 1 AND 2 * 5 AND id < 8"), vec![2, 5]);
}

#[test]
fn keywords_are_case_insensitive_in_where() {
    let (_d, db) = people();
    assert_eq!(
        ids(&db, "a between 1 and 5 and not b is null"),
        vec![1, 2, 5]
    );
    assert_eq!(
        ids(&db, "a not in (1) or a is null"),
        vec![2, 3, 4, 5, 6, 7, 8]
    );
}

#[test]
fn chained_comparison_and_like_are_unsupported() {
    let (_d, db) = people();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE a < 1 < 2"),
        SqlError::Unsupported(_)
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE b LIKE 'k%'"),
        SqlError::Unsupported(_)
    ));
}

#[test]
fn an_unclosed_parenthesis_is_a_syntax_error() {
    let (_d, db) = people();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE (a = 1"),
        SqlError::Syntax { .. }
    ));
}

#[test]
fn selected_columns_come_back_for_filtered_rows() {
    let (_d, db) = people();
    assert_eq!(
        rows(&db, "SELECT b, a FROM t WHERE a >= 5"),
        vec![
            vec![Value::Text("Kim".into()), Value::Integer(10)],
            vec![Value::Text("park".into()), Value::Integer(5)],
        ]
    );
}

// ----------------------------------------------------------- three values

#[test]
fn comparing_with_null_matches_no_row() {
    let (_d, db) = people();
    for c in [
        "a = NULL",
        "a <> NULL",
        "NULL = NULL",
        "b < NULL",
        "id = NULL",
        "NOT (a = NULL)",
    ] {
        assert_eq!(ids(&db, c), Vec::<i64>::new(), "{c}");
    }
}

#[test]
fn not_of_a_comparison_with_a_null_column_still_drops_the_row() {
    let (_d, db) = people();
    // rows 3 and 7 have b NULL: NOT (unknown) is unknown
    assert_eq!(ids(&db, "NOT (b = 'kim')"), vec![2, 5, 6, 8]);
}

#[test]
fn unknown_or_true_is_true_and_unknown_and_false_is_false() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a = NULL OR id = 1"), vec![1]);
    // NOT (unknown AND false) = NOT false = true for every row
    assert_eq!(
        ids(&db, "NOT (a = NULL AND 1 = 0)"),
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );
    // NOT (unknown OR false) = unknown
    assert_eq!(ids(&db, "NOT (a = NULL OR 1 = 0)"), Vec::<i64>::new());
}

#[test]
fn not_in_with_a_null_in_the_list_is_never_true() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a NOT IN (1, NULL)"), Vec::<i64>::new());
    assert_eq!(ids(&db, "a IN (1, NULL)"), vec![1]);
    // NOT (x NOT IN (1, NULL)) is true exactly where x = 1
    assert_eq!(ids(&db, "NOT (a NOT IN (1, NULL))"), vec![1]);
}

#[test]
fn in_with_a_null_operand_is_unknown() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "NOT (a IN (1, 2))"), vec![3, 5, 6, 8]);
}

#[test]
fn not_between_with_a_null_bound_is_true_only_when_the_known_bound_already_fails() {
    let (_d, db) = people();
    // a >= 1 AND a <= NULL: false where a < 1, unknown otherwise
    assert_eq!(ids(&db, "a NOT BETWEEN 1 AND NULL"), vec![3, 6]);
    // a >= NULL AND a <= 2: false where a > 2
    assert_eq!(ids(&db, "a NOT BETWEEN NULL AND 2"), vec![5, 8]);
    assert_eq!(ids(&db, "a BETWEEN 1 AND NULL"), Vec::<i64>::new());
}

#[test]
fn arithmetic_with_null_is_null() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "a + NULL IS NULL"), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(ids(&db, "-NULL IS NULL AND id = 1"), vec![1]);
    assert_eq!(ids(&db, "a * NULL = 0"), Vec::<i64>::new());
    assert_eq!(ids(&db, "a + 1 IS NULL"), vec![4, 7]);
}

#[test]
fn the_primary_key_is_never_null() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "id IS NULL"), Vec::<i64>::new());
    assert_eq!(ids(&db, "id IS NOT NULL"), vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

// --------------------------------------------------- integer key ranges

#[test]
fn integer_key_comparisons_at_and_around_zero_and_negatives() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "id = 0"), vec![0]);
    assert_eq!(ids(&db, "id = -5"), vec![-5]);
    assert_eq!(ids(&db, "id > -1 AND id < 5"), vec![0, 1]);
    assert_eq!(ids(&db, "id >= -5 AND id <= 0"), vec![-5, -1, 0]);
    assert_eq!(ids(&db, "id < 0"), vec![MIN, -5, -1]);
    assert_eq!(ids(&db, "id >= 0"), vec![0, 1, 5, MAX]);
}

#[test]
fn integer_key_bounds_at_i64_min_and_max() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "id > 9223372036854775807"), Vec::<i64>::new());
    assert_eq!(ids(&db, "id >= 9223372036854775807"), vec![MAX]);
    assert_eq!(ids(&db, "id = 9223372036854775807"), vec![MAX]);
    assert_eq!(ids(&db, "id < -9223372036854775808"), Vec::<i64>::new());
    assert_eq!(ids(&db, "id <= -9223372036854775808"), vec![MIN]);
    assert_eq!(ids(&db, "id = -9223372036854775808"), vec![MIN]);
    assert_eq!(
        ids(&db, "id > -9223372036854775808"),
        EDGE_KEYS[1..].to_vec()
    );
    assert_eq!(
        ids(&db, "id < 9223372036854775807"),
        EDGE_KEYS[..6].to_vec()
    );
}

#[test]
fn backwards_and_empty_integer_ranges_return_nothing() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "id > 5 AND id < 3"), Vec::<i64>::new());
    assert_eq!(ids(&db, "id > 1 AND id < 5"), Vec::<i64>::new()); // no key strictly between
    assert_eq!(ids(&db, "id > 0 AND id < 1"), Vec::<i64>::new());
    assert_eq!(ids(&db, "id >= 1 AND id <= 1"), vec![1]);
    assert_eq!(ids(&db, "id > 1 AND id <= 1"), Vec::<i64>::new());
    assert_eq!(ids(&db, "id BETWEEN 5 AND 1"), Vec::<i64>::new());
    assert_eq!(
        ids(
            &db,
            "id BETWEEN 9223372036854775807 AND -9223372036854775808"
        ),
        Vec::<i64>::new()
    );
}

#[test]
fn between_and_not_between_on_the_integer_key() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "id BETWEEN -5 AND 1"), vec![-5, -1, 0, 1]);
    assert_eq!(
        ids(
            &db,
            "id BETWEEN -9223372036854775808 AND 9223372036854775807"
        ),
        EDGE_KEYS.to_vec()
    );
    assert_eq!(ids(&db, "id NOT BETWEEN -1 AND 1"), vec![MIN, -5, 5, MAX]);
}

#[test]
fn in_on_the_integer_key_returns_only_listed_keys() {
    let (_d, db) = edges();
    assert_eq!(
        ids(&db, "id IN (9223372036854775807, -9223372036854775808)"),
        vec![MIN, MAX]
    );
    assert_eq!(ids(&db, "id IN (5, -5, 2)"), vec![-5, 5]);
    assert_eq!(ids(&db, "id IN (1, NULL)"), vec![1]);
    assert_eq!(ids(&db, "id NOT IN (0, 1)"), vec![MIN, -5, -1, 5, MAX]);
}

#[test]
fn constant_on_the_left_and_folded_constants_bound_the_key() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "1 < id"), vec![5, MAX]);
    assert_eq!(ids(&db, "0 >= id"), vec![MIN, -5, -1, 0]);
    assert_eq!(ids(&db, "id > 2 + 2"), vec![5, MAX]);
    assert_eq!(ids(&db, "id = -(2 * 3) + 1"), vec![-5]);
    assert_eq!(ids(&db, "id > 1 + NULL"), Vec::<i64>::new());
}

#[test]
fn or_and_not_equal_across_key_values() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "id = 1 OR id = 5"), vec![1, 5]);
    assert_eq!(ids(&db, "id < -1 OR id > 1"), vec![MIN, -5, 5, MAX]);
    assert_eq!(ids(&db, "id != 0"), vec![MIN, -5, -1, 1, 5, MAX]);
    assert_eq!(ids(&db, "NOT (id < 0)"), vec![0, 1, 5, MAX]);
}

#[test]
// Issue #34 (design 0027 E4): a literal out of range is Arithmetic, was
// Syntax.
fn an_integer_literal_beyond_i64_is_a_value_out_of_range() {
    let (_d, db) = edges();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE id > 9223372036854775808"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn the_key_expression_on_both_sides_keeps_working() {
    let (_d, db) = edges();
    assert_eq!(ids(&db, "id / 2 > 1"), vec![5, MAX]);
    assert_eq!(ids(&db, "id = a"), vec![0]);
}

// ------------------------------------------------------- text key ranges

#[test]
fn text_key_equality_including_empty_and_quoted_strings() {
    let (_d, db) = words();
    assert_eq!(keys(&db, "k = ''"), s(&[""]));
    assert_eq!(keys(&db, "k = 'it''s'"), s(&["it's"]));
    assert_eq!(keys(&db, "k = ''''"), s(&["'"]));
    assert_eq!(keys(&db, "k = ''''''"), s(&["''"]));
    assert_eq!(keys(&db, "k = 'A'"), Vec::<String>::new());
}

#[test]
fn text_key_ranges_use_byte_order_with_prefixes_before_longer_strings() {
    let (_d, db) = words();
    assert_eq!(keys(&db, "k > 'a' AND k < 'b'"), s(&["a b", "ab", "abc"]));
    assert_eq!(
        keys(&db, "k >= 'a' AND k < 'b'"),
        s(&["a", "a b", "ab", "abc"])
    );
    assert_eq!(keys(&db, "k BETWEEN 'a' AND 'ab'"), s(&["a", "a b", "ab"]));
    assert_eq!(keys(&db, "k > 'ab'"), s(&["abc", "b", "it's"]));
    assert_eq!(keys(&db, "k < 'a'"), s(&["", "'", "''", "B"]));
}

#[test]
fn the_empty_string_is_the_smallest_text_key() {
    let (_d, db) = words();
    assert_eq!(keys(&db, "k > ''").len(), 9);
    assert_eq!(keys(&db, "k >= ''").len(), 10);
    assert_eq!(keys(&db, "k < ''"), Vec::<String>::new());
    assert_eq!(keys(&db, "k <= ''"), s(&[""]));
}

#[test]
fn backwards_text_ranges_return_nothing() {
    let (_d, db) = words();
    assert_eq!(keys(&db, "k BETWEEN 'b' AND 'a'"), Vec::<String>::new());
    assert_eq!(keys(&db, "k > 'b' AND k < 'a'"), Vec::<String>::new());
}

#[test]
fn in_and_or_on_the_text_key() {
    let (_d, db) = words();
    assert_eq!(keys(&db, "k IN ('''', 'abc', 'zzz')"), s(&["'", "abc"]));
    assert_eq!(keys(&db, "k = 'b' OR k = ''"), s(&["", "b"]));
    assert_eq!(
        keys(&db, "k NOT IN ('', 'a', 'ab', 'abc', 'b')"),
        s(&["'", "''", "B", "a b", "it's"])
    );
}

#[test]
fn a_text_key_with_a_nul_byte_sorts_between_its_prefix_and_longer_strings() {
    let (_d, db) = open();
    ok(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, n INTEGER)");
    ok(&db, "INSERT INTO s VALUES ('a', 1), ('a\0', 2), ('ab', 3)");
    assert_eq!(keys(&db, "k > 'a'"), s(&["a\0", "ab"]));
    assert_eq!(keys(&db, "k > 'a' AND k < 'ab'"), s(&["a\0"]));
    assert_eq!(keys(&db, "k = 'a\0'"), s(&["a\0"]));
}

#[test]
fn looking_up_a_text_key_longer_than_the_engine_allows_finds_nothing() {
    let (_d, db) = words();
    let long = "x".repeat(oakdb::MAX_KEY_LEN + 10);
    assert_eq!(keys(&db, &format!("k = '{long}'")), Vec::<String>::new());
    assert_eq!(keys(&db, &format!("k > '{long}'")), Vec::<String>::new());
}

// ---------------------------------------------- non-key and mixed conditions

#[test]
fn a_key_range_combined_with_a_non_key_condition() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "id >= 2 AND a > 0"), vec![2, 5, 8]);
    assert_eq!(ids(&db, "a > 0 AND id < 5"), vec![1, 2]);
    assert_eq!(
        ids(&db, "id BETWEEN 2 AND 6 AND b IS NOT NULL"),
        vec![2, 4, 5, 6]
    );
    assert_eq!(ids(&db, "id IN (1, 3, 8) AND a > 0"), vec![1, 8]);
    assert_eq!(
        ids(&db, "(id < 3 OR id > 6) AND b IS NOT NULL"),
        vec![1, 2, 8]
    );
}

#[test]
fn an_or_between_a_key_condition_and_a_non_key_condition() {
    let (_d, db) = people();
    assert_eq!(ids(&db, "id = 7 OR a = 5"), vec![5, 7]);
    assert_eq!(ids(&db, "id > 6 OR b = 'kim'"), vec![1, 4, 7, 8]);
}

#[test]
fn several_key_conditions_intersect() {
    let (_d, db) = people();
    assert_eq!(
        ids(&db, "id > 2 AND id >= 3 AND id < 7 AND id <= 5"),
        vec![3, 4, 5]
    );
    assert_eq!(
        ids(&db, "id BETWEEN 1 AND 6 AND id IN (2, 6, 8)"),
        vec![2, 6]
    );
    assert_eq!(ids(&db, "id = 3 AND id = 4"), Vec::<i64>::new());
}

// ------------------------------------------------------ WHERE-less SELECT

#[test]
fn select_without_where_returns_the_whole_table() {
    let (_d, db) = people();
    assert_eq!(ints(&db, "SELECT id FROM t"), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(rows(&db, "SELECT * FROM t").len(), 8);
}

#[test]
fn select_without_where_on_an_empty_table_returns_no_rows() {
    let (_d, db) = open();
    ok(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)");
    assert_eq!(ints(&db, "SELECT id FROM t"), Vec::<i64>::new());
}

#[test]
fn select_without_where_reads_only_its_own_table() {
    let (_d, db) = open();
    ok(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    ok(&db, "CREATE TABLE u (id INTEGER PRIMARY KEY)");
    ok(&db, "INSERT INTO t VALUES (1), (2)");
    ok(&db, "INSERT INTO u VALUES (3)");
    assert_eq!(ints(&db, "SELECT id FROM t"), vec![1, 2]);
    assert_eq!(ints(&db, "SELECT id FROM u"), vec![3]);
    assert_eq!(ints(&db, "SELECT id FROM u WHERE id > 0"), vec![3]);
}

// ------------------------------------------------------ arithmetic errors

#[test]
fn overflow_while_filtering_is_an_arithmetic_error() {
    let (_d, db) = people();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE a + 9223372036854775807 > 0"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE a * 9223372036854775807 > 0"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(
            &db,
            "SELECT id FROM t WHERE a - 9223372036854775807 - 2 < 0"
        ),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn division_or_remainder_by_zero_is_an_arithmetic_error() {
    let (_d, db) = people();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE a / 0 = 1"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE a % 0 = 1"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE id / (a - a) = 1"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn i64_min_divided_or_reduced_by_minus_one_overflows() {
    let (_d, db) = people();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE -9223372036854775808 / -1 = 0"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE -9223372036854775808 % -1 = 0"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn negating_i64_min_overflows_but_the_literal_does_not() {
    let (_d, db) = edges();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE -id > 0"),
        SqlError::Arithmetic(_)
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE -(-9223372036854775808) > 0"),
        SqlError::Arithmetic(_)
    ));
    assert_eq!(ids(&db, "id = -9223372036854775808"), vec![MIN]);
}

#[test]
fn an_overflowing_constant_bound_errors_only_when_there_are_rows() {
    let (_d, db) = open();
    ok(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)");
    assert_eq!(ids(&db, "id > 9223372036854775807 + 1"), Vec::<i64>::new());
    ok(&db, "INSERT INTO t VALUES (1, 1)");
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE id > 9223372036854775807 + 1"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn rows_outside_the_key_range_are_not_evaluated_so_cannot_raise_errors() {
    let (_d, db) = open();
    ok(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)");
    ok(
        &db,
        "INSERT INTO t VALUES (1, 0), (2, 0), (3, 0), (4, 0), (5, 0), (6, 0), (7, 0)",
    );
    // the design's own example (0019 2-2): id = 3 is outside id > 5
    assert_eq!(ids(&db, "10 / (id - 3) > 0 AND id > 5"), vec![6, 7]);
    // without the range, the row id = 3 is read and divides by zero
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE 10 / (id - 3) > 0"),
        SqlError::Arithmetic(_)
    ));
}

#[test]
fn a_failed_select_leaves_the_table_usable() {
    let (_d, db) = people();
    let _ = err(&db, "SELECT id FROM t WHERE a / 0 = 1");
    assert_eq!(ids(&db, "a = 1"), vec![1]);
}

// ----------------------------------------------------------- type errors

fn assert_type_error(db: &Db, cond: &str) {
    match err(db, &format!("SELECT id FROM t WHERE {cond}")) {
        SqlError::Type(_) | SqlError::TypeMismatch { .. } => {}
        e => panic!("{cond}: expected a type error, got {e:?}"),
    }
}

#[test]
fn comparing_an_integer_column_with_text_is_a_type_mismatch_naming_the_column() {
    let (_d, db) = people();
    match err(&db, "SELECT id FROM t WHERE a = 'ten'") {
        SqlError::TypeMismatch {
            column,
            expected,
            found,
        } => {
            assert_eq!(column, "a");
            assert_eq!(expected, ColumnType::Integer);
            assert_eq!(found, Some(ColumnType::Text));
        }
        e => panic!("got {e:?}"),
    }
    match err(&db, "SELECT id FROM t WHERE b > 3") {
        SqlError::TypeMismatch {
            column,
            expected,
            found,
        } => {
            assert_eq!(column, "b");
            assert_eq!(expected, ColumnType::Text);
            assert_eq!(found, Some(ColumnType::Integer));
        }
        e => panic!("got {e:?}"),
    }
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE id IN (1, 'a')"),
        SqlError::TypeMismatch { .. }
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE id = 'a'"),
        SqlError::TypeMismatch { .. }
    ));
}

#[test]
fn other_type_errors_use_the_type_kind() {
    let (_d, db) = people();
    for cond in [
        "b + 1 = 2",
        "id",
        "id + 1",
        "NULL",
        "(a = 1) = (b = 'x')",
        "a + 1 = 'x'",
        "NOT a",
        "a = 1 AND b",
        "-b = 'x'",
        "'x' BETWEEN 1 AND 2",
        "(a = 1) IS NULL",
    ] {
        assert!(
            matches!(
                err(&db, &format!("SELECT id FROM t WHERE {cond}")),
                SqlError::Type(_)
            ),
            "{cond}"
        );
    }
}

#[test]
fn type_errors_are_reported_on_an_empty_table_too() {
    let (_d, db) = open();
    ok(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
    );
    for cond in ["a = 'ten'", "b + 1 = 2", "id", "a BETWEEN 1 AND 'z'"] {
        assert_type_error(&db, cond);
    }
}

#[test]
fn a_type_error_wins_over_an_arithmetic_error_because_no_row_is_read() {
    let (_d, db) = people();
    assert_type_error(&db, "a / 0 = 1 AND b = 5");
    assert_type_error(&db, "a / 0 = 1 OR b + 1 = 2");
}

#[test]
fn an_unknown_column_in_where_is_reported_before_reading_rows() {
    let (_d, db) = people();
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE zzz = 1"),
        SqlError::NoSuchColumn { .. }
    ));
    assert!(matches!(
        err(&db, "SELECT id FROM t WHERE a / 0 = 1 OR zzz = 1"),
        SqlError::NoSuchColumn { .. }
    ));
}

// ------------------------------------------------------ model comparison

/// SplitMix64: deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Clone>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize].clone()
    }
}

const SMALL_INTS: [i64; 13] = [-3, -2, -1, 0, 1, 2, 3, 4, 5, 7, 10, MIN, MAX];
const TEXTS: [&str; 8] = ["", "a", "ab", "b", "it's", "'", "B", "kim"];

#[derive(Clone, Debug)]
enum I {
    Col(usize), // index into the row's integer columns
    Lit(Option<i64>),
    Neg(Box<I>),
    Bin(char, Box<I>, Box<I>),
}

#[derive(Clone, Debug)]
enum T {
    Col(usize),
    Lit(Option<String>),
}

#[derive(Clone, Debug)]
enum S {
    I(I),
    T(T),
}

#[derive(Clone, Debug)]
enum P {
    Cmp(&'static str, S, S),
    IsNull(S, bool),
    Between(S, S, S, bool),
    In(S, Vec<S>, bool),
    Not(Box<P>),
    And(Box<P>, Box<P>),
    Or(Box<P>, Box<P>),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tv {
    T,
    F,
    U,
}

/// A model row: integer columns and text columns, by schema position.
#[derive(Clone, Debug)]
struct Row {
    ints: Vec<Option<i64>>,
    texts: Vec<Option<String>>,
}

/// Model failure: an arithmetic error the design promises, or a case the
/// design leaves open (NULL divided by zero), treated the same: Oakdb may
/// then fail with Arithmetic or succeed.
#[derive(Debug)]
struct Fail;

struct Schema {
    int_cols: Vec<&'static str>,
    text_cols: Vec<&'static str>,
}

fn gen_i(r: &mut Rng, sc: &Schema, depth: u32) -> I {
    let leaf = depth == 0 || r.below(3) == 0;
    if leaf {
        match r.below(6) {
            0..=2 => I::Col(r.below(sc.int_cols.len() as u64) as usize),
            3 => I::Lit(None),
            _ => I::Lit(Some(r.pick(&SMALL_INTS))),
        }
    } else if r.below(5) == 0 {
        I::Neg(Box::new(gen_i(r, sc, depth - 1)))
    } else {
        let op = r.pick(&['+', '-', '*', '/', '%']);
        I::Bin(
            op,
            Box::new(gen_i(r, sc, depth - 1)),
            Box::new(gen_i(r, sc, depth - 1)),
        )
    }
}

fn gen_t(r: &mut Rng, sc: &Schema) -> T {
    match r.below(5) {
        0..=2 => T::Col(r.below(sc.text_cols.len() as u64) as usize),
        3 => T::Lit(None),
        _ => T::Lit(Some(r.pick(&TEXTS).to_string())),
    }
}

fn gen_s(r: &mut Rng, sc: &Schema, is_int: bool) -> S {
    if is_int {
        let depth = r.below(3) as u32;
        S::I(gen_i(r, sc, depth))
    } else {
        S::T(gen_t(r, sc))
    }
}

fn gen_p(r: &mut Rng, sc: &Schema, depth: u32) -> P {
    let leaf = depth == 0 || r.below(3) == 0;
    if leaf {
        let is_int = r.below(3) != 0;
        match r.below(8) {
            0..=3 => {
                let op = r.pick(&["=", "!=", "<>", "<", "<=", ">", ">="]);
                P::Cmp(op, gen_s(r, sc, is_int), gen_s(r, sc, is_int))
            }
            4 => P::IsNull(gen_s(r, sc, is_int), r.below(2) == 0),
            5 | 6 => P::Between(
                gen_s(r, sc, is_int),
                gen_s(r, sc, is_int),
                gen_s(r, sc, is_int),
                r.below(2) == 0,
            ),
            _ => {
                let n = 1 + r.below(4) as usize;
                let list = (0..n).map(|_| gen_s(r, sc, is_int)).collect();
                P::In(gen_s(r, sc, is_int), list, r.below(2) == 0)
            }
        }
    } else {
        match r.below(5) {
            0 => P::Not(Box::new(gen_p(r, sc, depth - 1))),
            1 | 2 => P::And(
                Box::new(gen_p(r, sc, depth - 1)),
                Box::new(gen_p(r, sc, depth - 1)),
            ),
            _ => P::Or(
                Box::new(gen_p(r, sc, depth - 1)),
                Box::new(gen_p(r, sc, depth - 1)),
            ),
        }
    }
}

fn text_lit(s: &Option<String>) -> String {
    match s {
        None => "NULL".into(),
        Some(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

fn sql_i(e: &I, sc: &Schema) -> String {
    match e {
        I::Col(c) => sc.int_cols[*c].to_string(),
        I::Lit(None) => "NULL".into(),
        I::Lit(Some(v)) if *v < 0 => format!("({v})"),
        I::Lit(Some(v)) => v.to_string(),
        I::Neg(x) => format!("(-({}))", sql_i(x, sc)),
        I::Bin(op, a, b) => format!("({} {op} {})", sql_i(a, sc), sql_i(b, sc)),
    }
}

fn sql_s(e: &S, sc: &Schema) -> String {
    match e {
        S::I(i) => sql_i(i, sc),
        S::T(T::Col(c)) => sc.text_cols[*c].to_string(),
        S::T(T::Lit(l)) => text_lit(l),
    }
}

fn sql_p(p: &P, sc: &Schema) -> String {
    match p {
        P::Cmp(op, a, b) => format!("({} {op} {})", sql_s(a, sc), sql_s(b, sc)),
        P::IsNull(a, neg) => {
            format!(
                "({} IS {}NULL)",
                sql_s(a, sc),
                if *neg { "NOT " } else { "" }
            )
        }
        P::Between(a, lo, hi, neg) => format!(
            "({} {}BETWEEN {} AND {})",
            sql_s(a, sc),
            if *neg { "NOT " } else { "" },
            sql_s(lo, sc),
            sql_s(hi, sc)
        ),
        P::In(a, list, neg) => format!(
            "({} {}IN ({}))",
            sql_s(a, sc),
            if *neg { "NOT " } else { "" },
            list.iter()
                .map(|x| sql_s(x, sc))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        P::Not(x) => format!("(NOT {})", sql_p(x, sc)),
        P::And(a, b) => format!("({} AND {})", sql_p(a, sc), sql_p(b, sc)),
        P::Or(a, b) => format!("({} OR {})", sql_p(a, sc), sql_p(b, sc)),
    }
}

fn eval_i(e: &I, row: &Row) -> Result<Option<i64>, Fail> {
    Ok(match e {
        I::Col(c) => row.ints[*c],
        I::Lit(v) => *v,
        I::Neg(x) => match eval_i(x, row)? {
            None => None,
            Some(v) => Some(v.checked_neg().ok_or(Fail)?),
        },
        I::Bin(op, a, b) => {
            // every subexpression is computed: no shortcuts
            let (a, b) = (eval_i(a, row)?, eval_i(b, row)?);
            match (a, b) {
                // NULL / 0 is not settled by the design: lenient
                (None, Some(0)) if matches!(op, '/' | '%') => return Err(Fail),
                (Some(a), Some(b)) => Some(
                    match op {
                        '+' => a.checked_add(b),
                        '-' => a.checked_sub(b),
                        '*' => a.checked_mul(b),
                        '/' => a.checked_div(b),
                        '%' => a.checked_rem(b),
                        _ => unreachable!(),
                    }
                    .ok_or(Fail)?,
                ),
                _ => None,
            }
        }
    })
}

#[derive(PartialEq, PartialOrd, Clone, Debug)]
enum V {
    I(i64),
    T(String),
}

fn eval_s(e: &S, row: &Row) -> Result<Option<V>, Fail> {
    Ok(match e {
        S::I(i) => eval_i(i, row)?.map(V::I),
        S::T(T::Col(c)) => row.texts[*c].clone().map(V::T),
        S::T(T::Lit(l)) => l.clone().map(V::T),
    })
}

fn cmp(op: &str, a: &Option<V>, b: &Option<V>) -> Tv {
    match (a, b) {
        (Some(a), Some(b)) => {
            let r = match op {
                "=" => a == b,
                "!=" | "<>" => a != b,
                "<" => a < b,
                "<=" => a <= b,
                ">" => a > b,
                ">=" => a >= b,
                _ => unreachable!(),
            };
            if r {
                Tv::T
            } else {
                Tv::F
            }
        }
        _ => Tv::U,
    }
}

fn and(a: Tv, b: Tv) -> Tv {
    match (a, b) {
        (Tv::F, _) | (_, Tv::F) => Tv::F,
        (Tv::T, Tv::T) => Tv::T,
        _ => Tv::U,
    }
}

fn or(a: Tv, b: Tv) -> Tv {
    match (a, b) {
        (Tv::T, _) | (_, Tv::T) => Tv::T,
        (Tv::F, Tv::F) => Tv::F,
        _ => Tv::U,
    }
}

fn not(a: Tv) -> Tv {
    match a {
        Tv::T => Tv::F,
        Tv::F => Tv::T,
        Tv::U => Tv::U,
    }
}

fn eval_p(p: &P, row: &Row) -> Result<Tv, Fail> {
    Ok(match p {
        P::Cmp(op, a, b) => cmp(op, &eval_s(a, row)?, &eval_s(b, row)?),
        P::IsNull(a, neg) => {
            let n = eval_s(a, row)?.is_none();
            if n != *neg {
                Tv::T
            } else {
                Tv::F
            }
        }
        P::Between(a, lo, hi, neg) => {
            let (a, lo, hi) = (eval_s(a, row)?, eval_s(lo, row)?, eval_s(hi, row)?);
            let r = and(cmp(">=", &a, &lo), cmp("<=", &a, &hi));
            if *neg {
                not(r)
            } else {
                r
            }
        }
        P::In(a, list, neg) => {
            let a = eval_s(a, row)?;
            let vals = list
                .iter()
                .map(|x| eval_s(x, row))
                .collect::<Result<Vec<_>, _>>()?;
            let r = vals.iter().fold(Tv::F, |acc, v| or(acc, cmp("=", &a, v)));
            if *neg {
                not(r)
            } else {
                r
            }
        }
        P::Not(x) => not(eval_p(x, row)?),
        P::And(a, b) => {
            let (a, b) = (eval_p(a, row)?, eval_p(b, row)?);
            and(a, b)
        }
        P::Or(a, b) => {
            let (a, b) = (eval_p(a, row)?, eval_p(b, row)?);
            or(a, b)
        }
    })
}

/// Generates conditions that lean on the primary key so ranges are planned.
fn gen_where(r: &mut Rng, sc: &Schema, key_is_int: bool) -> P {
    let key = if key_is_int {
        S::I(I::Col(0))
    } else {
        S::T(T::Col(0))
    };
    let constant = |r: &mut Rng| -> S {
        if key_is_int {
            if r.below(4) == 0 {
                S::I(gen_i(
                    r,
                    &Schema {
                        int_cols: vec!["id"],
                        text_cols: vec![],
                    },
                    1,
                ))
                .strip_cols()
            } else {
                S::I(I::Lit(Some(r.pick(&SMALL_INTS))))
            }
        } else {
            S::T(T::Lit(Some(r.pick(&TEXTS).to_string())))
        }
    };
    let key_cond = |r: &mut Rng| -> P {
        match r.below(4) {
            0 | 1 => P::Cmp(
                r.pick(&["=", "<", "<=", ">", ">=", "!="]),
                key.clone(),
                constant(r),
            ),
            2 => P::Between(key.clone(), constant(r), constant(r), r.below(4) == 0),
            _ => {
                let n = 1 + r.below(3) as usize;
                P::In(
                    key.clone(),
                    (0..n).map(|_| constant(r)).collect(),
                    r.below(4) == 0,
                )
            }
        }
    };
    match r.below(4) {
        0 => gen_p(r, sc, 3),
        1 => P::And(Box::new(key_cond(r)), Box::new(gen_p(r, sc, 2))),
        2 => P::And(Box::new(key_cond(r)), Box::new(key_cond(r))),
        _ => P::And(
            Box::new(P::And(Box::new(key_cond(r)), Box::new(gen_p(r, sc, 1)))),
            Box::new(key_cond(r)),
        ),
    }
}

impl S {
    /// Turns columns into literals so the expression is a constant.
    fn strip_cols(self) -> S {
        fn go(e: I) -> I {
            match e {
                I::Col(_) => I::Lit(Some(3)),
                I::Lit(v) => I::Lit(v),
                I::Neg(x) => I::Neg(Box::new(go(*x))),
                I::Bin(op, a, b) => I::Bin(op, Box::new(go(*a)), Box::new(go(*b))),
            }
        }
        match self {
            S::I(i) => S::I(go(i)),
            t => t,
        }
    }
}

fn model_check(seed: u64, key_is_int: bool, cases: usize) {
    let mut r = Rng(seed);
    let (_d, db) = open();
    let sc = if key_is_int {
        ok(
            &db,
            "CREATE TABLE m (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, t TEXT)",
        );
        Schema {
            int_cols: vec!["id", "x", "y"],
            text_cols: vec!["t"],
        }
    } else {
        ok(
            &db,
            "CREATE TABLE m (k TEXT PRIMARY KEY, x INTEGER, t TEXT)",
        );
        Schema {
            int_cols: vec!["x"],
            text_cols: vec!["k", "t"],
        }
    };
    let mut model: Vec<Row> = Vec::new();
    let mut seen_i = std::collections::BTreeSet::new();
    let mut seen_t = std::collections::BTreeSet::new();
    // non-key integers stay small so arithmetic mostly succeeds; the
    // extremes still come in through keys and literals
    let small_int = |r: &mut Rng| -> Option<i64> {
        if r.below(4) == 0 {
            None
        } else {
            Some(r.pick(&SMALL_INTS[..11]))
        }
    };
    let small_text = |r: &mut Rng| -> Option<String> {
        if r.below(4) == 0 {
            None
        } else {
            Some(r.pick(&TEXTS).to_string())
        }
    };
    let mut tuples = Vec::new();
    while model.len() < 36 {
        let row = if key_is_int {
            let id = if r.below(3) == 0 {
                r.pick(&SMALL_INTS)
            } else {
                r.below(40) as i64 - 20
            };
            if !seen_i.insert(id) {
                continue;
            }
            Row {
                ints: vec![Some(id), small_int(&mut r), small_int(&mut r)],
                texts: vec![small_text(&mut r)],
            }
        } else {
            let k = format!("{}{}", r.pick(&TEXTS), r.pick(&["", "a", "z", "'", "0"]));
            if !seen_t.insert(k.clone()) {
                continue;
            }
            Row {
                ints: vec![small_int(&mut r)],
                texts: vec![Some(k), small_text(&mut r)],
            }
        };
        let lit_i = |v: Option<i64>| v.map_or("NULL".to_string(), |v| v.to_string());
        tuples.push(if key_is_int {
            format!(
                "({}, {}, {}, {})",
                lit_i(row.ints[0]),
                lit_i(row.ints[1]),
                lit_i(row.ints[2]),
                text_lit(&row.texts[0])
            )
        } else {
            format!(
                "({}, {}, {})",
                text_lit(&row.texts[0]),
                lit_i(row.ints[0]),
                text_lit(&row.texts[1])
            )
        });
        model.push(row);
    }
    ok(&db, &format!("INSERT INTO m VALUES {}", tuples.join(", ")));
    let key_of = |row: &Row| -> String {
        if key_is_int {
            row.ints[0].unwrap().to_string()
        } else {
            row.texts[0].clone().unwrap()
        }
    };
    let key_name = if key_is_int { "id" } else { "k" };

    let (mut exact, mut lenient) = (0, 0);
    for case in 0..cases {
        let p = gen_where(&mut r, &sc, key_is_int);
        let sql = format!("SELECT {key_name} FROM m WHERE {}", sql_p(&p, &sc));
        let expected: Result<Vec<String>, Fail> = (|| {
            let mut out = Vec::new();
            for row in &model {
                if eval_p(&p, row)? == Tv::T {
                    out.push(key_of(row));
                }
            }
            out.sort();
            Ok(out)
        })();
        let got = execute(&db, &sql).map(|o| match o {
            Output::Rows { rows, .. } => {
                let mut v: Vec<String> = rows
                    .into_iter()
                    .map(|r| match &r[0] {
                        Value::Integer(i) => i.to_string(),
                        Value::Text(s) => s.clone(),
                        v => panic!("unexpected {v:?}"),
                    })
                    .collect();
                v.sort();
                v
            }
            o => panic!("not rows: {o:?}"),
        });
        match (expected, got) {
            (Ok(want), Ok(got)) => {
                exact += 1;
                assert_eq!(got, want, "seed {seed} case {case}: {sql}");
            }
            (Ok(_), Err(e)) => {
                panic!("seed {seed} case {case}: {sql}\nmodel had no error, Oakdb: {e:?}")
            }
            (Err(Fail), Ok(_)) | (Err(Fail), Err(SqlError::Arithmetic(_))) => lenient += 1,
            (Err(Fail), Err(e)) => {
                panic!("seed {seed} case {case}: {sql}\nmodel: arithmetic error, Oakdb: {e:?}")
            }
        }
    }
    // the generator must mostly produce error-free conditions to mean anything
    assert!(
        exact * 2 > cases,
        "only {exact} of {cases} cases checked exactly ({lenient} lenient)"
    );
}

#[test]
fn random_conditions_on_an_integer_key_table_match_a_full_scan_model() {
    for seed in [1, 2, 3, 0xDEAD_BEEF] {
        model_check(seed, true, 300);
    }
}

#[test]
fn random_conditions_on_a_text_key_table_match_a_full_scan_model() {
    for seed in [11, 12, 13] {
        model_check(seed, false, 300);
    }
}
