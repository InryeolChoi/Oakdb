//! Blind tests for issue #31 (SQL first pass), written from the issue,
//! spec #12 stories 29-39 and docs/design/0017 only, without reading the
//! implementation (PR #62's verification). They check only the results of
//! executing SQL strings.

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, ColumnType, Output, SqlError, Value};

const DIR: &str = "/data/db";

fn open(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), DIR).expect("open")
}

fn fresh() -> (SimFs, Db<SimFs>) {
    let fs = SimFs::new();
    let db = open(&fs);
    (fs, db)
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

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn users(db: &Db<SimFs>) {
    assert_eq!(
        ok(
            db,
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, photo BLOB)"
        ),
        Output::Done
    );
}

fn is_unsupported(e: &SqlError) -> bool {
    matches!(e, SqlError::Unsupported(_))
}

fn is_syntax(e: &SqlError) -> bool {
    matches!(e, SqlError::Syntax { .. })
}

// ------------------------------------------------------------ CREATE TABLE

#[test]
fn create_table_with_all_three_types_and_one_primary_key_returns_done() {
    let (_fs, db) = fresh();
    users(&db);
    assert_eq!(
        ok(&db, "INSERT INTO users VALUES (1, 'kim', X'01')"),
        Output::Affected(1)
    );
}

#[test]
fn create_table_without_primary_key_is_unsupported() {
    let (_fs, db) = fresh();
    let e = err(&db, "CREATE TABLE t (a INTEGER, b TEXT)");
    assert!(is_unsupported(&e), "{e:?}");
    // And the table was not created.
    assert!(matches!(
        err(&db, "INSERT INTO t VALUES (1, 'x')"),
        SqlError::NoSuchTable(_)
    ));
}

#[test]
fn create_table_with_two_primary_keys_is_unsupported() {
    let (_fs, db) = fresh();
    let e = err(
        &db,
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT PRIMARY KEY)",
    );
    assert!(is_unsupported(&e), "{e:?}");
}

#[test]
fn create_table_twice_gives_table_exists() {
    let (_fs, db) = fresh();
    users(&db);
    let e = err(&db, "CREATE TABLE users (id INTEGER PRIMARY KEY)");
    assert!(
        matches!(&e, SqlError::TableExists(n) if n == "users"),
        "{e:?}"
    );
}

#[test]
fn create_table_with_duplicate_column_gives_duplicate_column() {
    let (_fs, db) = fresh();
    let e = err(&db, "CREATE TABLE t (a INTEGER PRIMARY KEY, a TEXT)");
    assert!(
        matches!(&e, SqlError::DuplicateColumn(c) if c == "a"),
        "{e:?}"
    );
}

#[test]
fn create_table_with_real_type_is_unsupported_and_unknown_type_is_an_error() {
    let (_fs, db) = fresh();
    let e = err(&db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b REAL)");
    assert!(is_unsupported(&e), "{e:?}");
    let e = err(&db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b FROBNICATE)");
    assert!(is_unsupported(&e) || is_syntax(&e), "{e:?}");
}

#[test]
fn table_names_are_case_sensitive() {
    let (_fs, db) = fresh();
    ok(&db, "CREATE TABLE Users (id INTEGER PRIMARY KEY)");
    ok(&db, "CREATE TABLE users (id INTEGER PRIMARY KEY)");
    ok(&db, "INSERT INTO Users VALUES (1)");
    assert_eq!(
        rows(&db, "SELECT * FROM users WHERE id = 1"),
        Vec::<Vec<Value>>::new()
    );
}

// ------------------------------------------------------------ INSERT

#[test]
fn insert_without_column_list_then_select_star() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (1, 'kim', X'00ff')");
    match ok(&db, "SELECT * FROM users WHERE id = 1") {
        Output::Rows { columns, rows } => {
            assert_eq!(columns, vec!["id", "name", "photo"]);
            assert_eq!(
                rows,
                vec![vec![
                    Value::Integer(1),
                    t("kim"),
                    Value::Blob(vec![0, 0xff])
                ]]
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_with_column_list_in_other_order_and_missing_column_is_null() {
    let (_fs, db) = fresh();
    users(&db);
    assert_eq!(
        ok(&db, "INSERT INTO users (name, id) VALUES ('lee', 7)"),
        Output::Affected(1)
    );
    assert_eq!(
        rows(&db, "SELECT * FROM users WHERE id = 7"),
        vec![vec![Value::Integer(7), t("lee"), Value::Null]]
    );
}

#[test]
fn insert_multi_row_counts_rows() {
    let (_fs, db) = fresh();
    users(&db);
    assert_eq!(
        ok(
            &db,
            "INSERT INTO users VALUES (1, 'a', NULL), (2, 'b', NULL), (3, 'c', NULL)"
        ),
        Output::Affected(3)
    );
    for (id, name) in [(1, "a"), (2, "b"), (3, "c")] {
        assert_eq!(
            rows(&db, &format!("SELECT name FROM users WHERE id = {id}")),
            vec![vec![t(name)]]
        );
    }
}

#[test]
fn multi_row_insert_with_duplicate_key_inside_inserts_nothing() {
    let (_fs, db) = fresh();
    users(&db);
    let e = err(
        &db,
        "INSERT INTO users VALUES (1, 'a', NULL), (2, 'b', NULL), (1, 'c', NULL)",
    );
    assert!(
        matches!(&e, SqlError::DuplicateKey { table } if table == "users"),
        "{e:?}"
    );
    assert!(rows(&db, "SELECT * FROM users WHERE id = 1").is_empty());
    assert!(rows(&db, "SELECT * FROM users WHERE id = 2").is_empty());
}

#[test]
fn multi_row_insert_colliding_with_existing_row_inserts_nothing() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (5, 'old', NULL)");
    let e = err(
        &db,
        "INSERT INTO users VALUES (4, 'x', NULL), (5, 'new', NULL)",
    );
    assert!(matches!(e, SqlError::DuplicateKey { .. }), "{e:?}");
    assert!(rows(&db, "SELECT * FROM users WHERE id = 4").is_empty());
    assert_eq!(
        rows(&db, "SELECT name FROM users WHERE id = 5"),
        vec![vec![t("old")]]
    );
}

#[test]
fn multi_row_insert_with_type_mismatch_in_last_row_inserts_nothing() {
    let (_fs, db) = fresh();
    users(&db);
    let e = err(&db, "INSERT INTO users VALUES (1, 'a', NULL), (2, 3, NULL)");
    assert!(
        matches!(&e, SqlError::TypeMismatch { column, expected: ColumnType::Text, found: Some(ColumnType::Integer) } if column == "name"),
        "{e:?}"
    );
    assert!(rows(&db, "SELECT * FROM users WHERE id = 1").is_empty());
}

#[test]
fn null_is_allowed_in_non_key_columns() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (1, NULL, NULL)");
    assert_eq!(
        rows(&db, "SELECT * FROM users WHERE id = 1"),
        vec![vec![Value::Integer(1), Value::Null, Value::Null]]
    );
}

#[test]
fn null_primary_key_is_a_type_mismatch_explicit_or_omitted() {
    let (_fs, db) = fresh();
    users(&db);
    let e = err(&db, "INSERT INTO users VALUES (NULL, 'a', NULL)");
    assert!(
        matches!(&e, SqlError::TypeMismatch { column, found: None, .. } if column == "id"),
        "{e:?}"
    );
    let e = err(&db, "INSERT INTO users (name) VALUES ('a')");
    assert!(
        matches!(&e, SqlError::TypeMismatch { column, found: None, .. } if column == "id"),
        "{e:?}"
    );
}

#[test]
fn type_mismatch_for_each_type() {
    let (_fs, db) = fresh();
    users(&db);
    for sql in [
        "INSERT INTO users VALUES ('1', 'a', NULL)",
        "INSERT INTO users VALUES (1, X'61', NULL)",
        "INSERT INTO users VALUES (1, 'a', 'not a blob')",
        "INSERT INTO users VALUES (1, 'a', 5)",
    ] {
        let e = err(&db, sql);
        assert!(matches!(e, SqlError::TypeMismatch { .. }), "{sql}: {e:?}");
    }
    assert!(rows(&db, "SELECT * FROM users WHERE id = 1").is_empty());
}

#[test]
fn insert_with_wrong_value_count_or_unknown_column_or_table() {
    let (_fs, db) = fresh();
    users(&db);
    let e = err(&db, "INSERT INTO users VALUES (1, 'a')");
    assert!(
        matches!(
            e,
            SqlError::ValueCount {
                expected: 3,
                found: 2
            }
        ),
        "{e:?}"
    );
    let e = err(&db, "INSERT INTO users (id, age) VALUES (1, 2)");
    assert!(
        matches!(&e, SqlError::NoSuchColumn { column, .. } if column == "age"),
        "{e:?}"
    );
    let e = err(&db, "INSERT INTO users (id, id) VALUES (1, 2)");
    assert!(matches!(e, SqlError::DuplicateColumn(_)), "{e:?}");
    let e = err(&db, "INSERT INTO ghosts VALUES (1)");
    assert!(
        matches!(&e, SqlError::NoSuchTable(n) if n == "ghosts"),
        "{e:?}"
    );
}

// ------------------------------------------------------------ SELECT

#[test]
fn select_column_list_returns_those_columns_in_that_order() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (1, 'kim', X'ab')");
    match ok(&db, "SELECT photo, id FROM users WHERE id = 1") {
        Output::Rows { columns, rows } => {
            assert_eq!(columns, vec!["photo", "id"]);
            assert_eq!(rows, vec![vec![Value::Blob(vec![0xab]), Value::Integer(1)]]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn integer_keys_including_negative_and_extremes_round_trip() {
    let (_fs, db) = fresh();
    ok(&db, "CREATE TABLE n (k INTEGER PRIMARY KEY, v TEXT)");
    let keys = [
        0i64,
        1,
        -1,
        2,
        10,
        -10,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
        i64::MAX - 1,
    ];
    for k in keys {
        ok(&db, &format!("INSERT INTO n VALUES ({k}, 'v{k}')"));
    }
    for k in keys {
        assert_eq!(
            rows(&db, &format!("SELECT * FROM n WHERE k = {k}")),
            vec![vec![Value::Integer(k), t(&format!("v{k}"))]],
            "key {k}"
        );
    }
    assert!(rows(&db, "SELECT * FROM n WHERE k = 3").is_empty());
}

#[test]
fn integer_out_of_i64_range_is_a_syntax_error() {
    let (_fs, db) = fresh();
    ok(&db, "CREATE TABLE n (k INTEGER PRIMARY KEY)");
    let e = err(&db, "INSERT INTO n VALUES (9223372036854775808)");
    assert!(is_syntax(&e), "{e:?}");
    let e = err(&db, "INSERT INTO n VALUES (-9223372036854775809)");
    assert!(is_syntax(&e), "{e:?}");
}

#[test]
fn text_keys_with_quotes_unicode_and_prefixes_round_trip() {
    let (_fs, db) = fresh();
    ok(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, n INTEGER)");
    let keys = ["a", "ab", "it's", "''", "", "한글 키", "émoji 🌳", "a b"];
    for (i, k) in keys.iter().enumerate() {
        let lit = k.replace('\'', "''");
        assert_eq!(
            ok(&db, &format!("INSERT INTO s VALUES ('{lit}', {i})")),
            Output::Affected(1)
        );
    }
    for (i, k) in keys.iter().enumerate() {
        let lit = k.replace('\'', "''");
        assert_eq!(
            rows(&db, &format!("SELECT k, n FROM s WHERE k = '{lit}'")),
            vec![vec![t(k), Value::Integer(i as i64)]],
            "key {k:?}"
        );
    }
    assert!(rows(&db, "SELECT * FROM s WHERE k = 'abc'").is_empty());
}

#[test]
fn blob_keys_with_zero_bytes_round_trip() {
    let (_fs, db) = fresh();
    ok(&db, "CREATE TABLE b (k BLOB PRIMARY KEY, v TEXT)");
    let keys: [&[u8]; 6] = [
        b"\x00",
        b"\x00\x00",
        b"\x00\xff",
        b"a",
        b"a\x00",
        b"\xff\x00\x01",
    ];
    for (i, k) in keys.iter().enumerate() {
        let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
        ok(&db, &format!("INSERT INTO b VALUES (X'{hex}', 'v{i}')"));
    }
    for (i, k) in keys.iter().enumerate() {
        let hex: String = k.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(
            rows(&db, &format!("SELECT * FROM b WHERE k = X'{hex}'")),
            vec![vec![Value::Blob(k.to_vec()), t(&format!("v{i}"))]],
            "key {k:?}"
        );
    }
    assert!(rows(&db, "SELECT * FROM b WHERE k = X'0000ff'").is_empty());
}

#[test]
fn select_unknown_table_or_column_errors() {
    let (_fs, db) = fresh();
    users(&db);
    let e = err(&db, "SELECT * FROM ghosts WHERE id = 1");
    assert!(matches!(e, SqlError::NoSuchTable(_)), "{e:?}");
    let e = err(&db, "SELECT age FROM users WHERE id = 1");
    assert!(
        matches!(&e, SqlError::NoSuchColumn { table, column } if table == "users" && column == "age"),
        "{e:?}"
    );
}

#[test]
fn where_value_of_wrong_type_is_mismatch_and_eq_null_is_no_rows() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (1, 'a', NULL)");
    let e = err(&db, "SELECT * FROM users WHERE id = 'a'");
    assert!(matches!(e, SqlError::TypeMismatch { .. }), "{e:?}");
    assert!(rows(&db, "SELECT * FROM users WHERE id = NULL").is_empty());
}

#[test]
fn keywords_are_case_insensitive_and_semicolon_is_optional() {
    let (_fs, db) = fresh();
    ok(&db, "create table T (Id integer primary key, Nm text);");
    ok(&db, "InSeRt InTo T (Id, Nm) vAlUeS (1, 'x') ;");
    ok(&db, "insert into T values (2, null), (-3, 'y')");
    assert_eq!(
        rows(&db, "select Nm from T where Id = 1;"),
        vec![vec![t("x")]]
    );
    assert_eq!(
        rows(&db, "SeLeCt * FrOm T WhErE Id = -3"),
        vec![vec![Value::Integer(-3), t("y")]]
    );
    assert_eq!(
        rows(&db, "select Nm from T where Id = 2"),
        vec![vec![Value::Null]]
    );
    // Hex digits in either case, and the x prefix in either case.
    ok(&db, "CREATE TABLE b (k BLOB PRIMARY KEY)");
    ok(&db, "INSERT INTO b VALUES (x'aB')");
    assert_eq!(
        rows(&db, "SELECT * FROM b WHERE k = X'Ab'"),
        vec![vec![Value::Blob(vec![0xab])]]
    );
}

// ------------------------------------------------------------ unsupported

#[test]
fn unsupported_statements_say_unsupported() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (1, 'a', NULL)");
    for sql in [
        "UPDATE users SET name = 'b' WHERE id = 1",
        "DELETE FROM users WHERE id = 1",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "SELECT * FROM users JOIN users WHERE id = 1",
        "SELECT * FROM users WHERE name = 'a'",
        "SELECT * FROM users WHERE id > 1",
        "SELECT * FROM users WHERE id < 1",
        "SELECT * FROM users",
        "SELECT * FROM users WHERE id = 1 ORDER BY id",
        "SELECT * FROM users WHERE id = 1; SELECT * FROM users WHERE id = 1",
        "CREATE INDEX i ON users (name)",
        "CREATE TABLE z (a INTEGER PRIMARY KEY NOT NULL)",
    ] {
        let e = err(&db, sql);
        assert!(is_unsupported(&e), "{sql}: {e:?}");
    }
    // Nothing above changed the row.
    assert_eq!(
        rows(&db, "SELECT name FROM users WHERE id = 1"),
        vec![vec![t("a")]]
    );
}

// ------------------------------------------------------------ syntax errors

#[test]
fn broken_statements_give_syntax_errors_with_a_position_inside_the_text() {
    let (_fs, db) = fresh();
    users(&db);
    for sql in [
        "INSERT INTO users VALUES (1, 'kim, NULL)",
        "INSERT INTO users VALUES (1, 'a', X'abc')",
        "INSERT INTO users VALUES (1, 'a', X'zz')",
        "INSERT INTO users VALUES (1, 'a', NULL",
        "CREATE TABLE t (a INTEGER PRIMARY KEY",
        "INSERT INTO users VALUES (1 'a' NULL)",
        "SELECT * FROM users WHERE id =",
        "CREATE TABLE select (a INTEGER PRIMARY KEY)",
        "INSERT INTO users VALUES",
    ] {
        match err(&db, sql) {
            SqlError::Syntax { position, .. } => {
                assert!(position <= sql.len(), "{sql}: position {position}")
            }
            e => panic!("{sql}: expected syntax error, got {e:?}"),
        }
    }
    assert!(rows(&db, "SELECT * FROM users WHERE id = 1").is_empty());
}

#[test]
fn unterminated_string_position_points_at_or_after_the_quote() {
    let (_fs, db) = fresh();
    users(&db);
    let sql = "INSERT INTO users VALUES (1, 'kim)";
    let quote = sql.find('\'').unwrap();
    match err(&db, sql) {
        SqlError::Syntax { position, .. } => assert!(position >= quote, "{position} < {quote}"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn empty_and_whitespace_input_errors_without_panicking() {
    let (_fs, db) = fresh();
    for sql in ["", "   ", ";", "\n\t"] {
        assert!(execute(&db, sql).is_err(), "{sql:?}");
    }
}

// ------------------------------------------------------------ durability

#[test]
fn tables_and_rows_survive_close_and_reopen() {
    let fs = SimFs::new();
    {
        let db = open(&fs);
        users(&db);
        ok(&db, "CREATE TABLE s (k TEXT PRIMARY KEY, v INTEGER)");
        ok(
            &db,
            "INSERT INTO users VALUES (1, 'kim', X'00'), (2, 'lee', NULL)",
        );
        ok(&db, "INSERT INTO s VALUES ('x', 9)");
    }
    let db = open(&fs);
    assert_eq!(
        rows(&db, "SELECT * FROM users WHERE id = 1"),
        vec![vec![Value::Integer(1), t("kim"), Value::Blob(vec![0])]]
    );
    assert_eq!(
        rows(&db, "SELECT v FROM s WHERE k = 'x'"),
        vec![vec![Value::Integer(9)]]
    );
    assert!(matches!(
        err(&db, "CREATE TABLE s (k INTEGER PRIMARY KEY)"),
        SqlError::TableExists(_)
    ));
    // A table created after reopening gets its own rows, separate from older ones.
    ok(&db, "CREATE TABLE u2 (id INTEGER PRIMARY KEY)");
    ok(&db, "INSERT INTO u2 VALUES (1)");
    assert_eq!(
        rows(&db, "SELECT * FROM u2 WHERE id = 1"),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        rows(&db, "SELECT name FROM users WHERE id = 1"),
        vec![vec![t("kim")]]
    );
}

#[test]
fn acknowledged_inserts_survive_a_power_cut() {
    for seed in [1u64, 2, 3, 42, 0xdead_beef, 12345] {
        let fs = SimFs::new();
        {
            let db = open(&fs);
            users(&db);
            ok(
                &db,
                "INSERT INTO users VALUES (1, 'a', NULL), (2, 'b', X'00')",
            );
            ok(&db, "INSERT INTO users VALUES (3, 'c', NULL)");
        }
        fs.crash(seed);
        let db = open(&fs);
        for (id, name) in [(1, "a"), (2, "b"), (3, "c")] {
            assert_eq!(
                rows(&db, &format!("SELECT name FROM users WHERE id = {id}")),
                vec![vec![t(name)]],
                "seed {seed} id {id}"
            );
        }
    }
}

#[test]
fn acknowledged_insert_survives_a_power_cut_without_closing() {
    for seed in [5u64, 6, 7] {
        let fs = SimFs::new();
        let db = open(&fs);
        users(&db);
        ok(&db, "INSERT INTO users VALUES (1, 'a', NULL)");
        fs.crash(seed);
        drop(db);
        let db = open(&fs);
        assert_eq!(
            rows(&db, "SELECT name FROM users WHERE id = 1"),
            vec![vec![t("a")]],
            "seed {seed}"
        );
    }
}

#[test]
fn values_longer_than_a_page_round_trip_and_survive_reopen() {
    let fs = SimFs::new();
    let text: String = (0..20_000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let blob: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
    let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    {
        let db = open(&fs);
        users(&db);
        ok(
            &db,
            &format!("INSERT INTO users VALUES (1, '{text}', X'{hex}')"),
        );
        assert_eq!(
            rows(&db, "SELECT name, photo FROM users WHERE id = 1"),
            vec![vec![t(&text), Value::Blob(blob.clone())]]
        );
    }
    let db = open(&fs);
    assert_eq!(
        rows(&db, "SELECT name, photo FROM users WHERE id = 1"),
        vec![vec![t(&text), Value::Blob(blob)]]
    );
}

#[test]
fn long_text_primary_key_errors_cleanly_or_round_trips() {
    // Keys are capped by the engine (MAX_KEY_LEN); a too-long key must give
    // an error, never a panic, and must not leave a half row.
    let (_fs, db) = fresh();
    ok(&db, "CREATE TABLE s (k TEXT PRIMARY KEY)");
    let k = "k".repeat(2000);
    match execute(&db, &format!("INSERT INTO s VALUES ('{k}')")) {
        Ok(_) => assert_eq!(
            rows(&db, &format!("SELECT * FROM s WHERE k = '{k}'")),
            vec![vec![t(&k)]]
        ),
        Err(e) => {
            assert!(matches!(e, SqlError::Engine(_)), "{e:?}");
            // Looking the same too-long key up may give zero rows or the
            // same engine error (the design does not say which); both mean
            // no half row was left.
            match execute(&db, &format!("SELECT * FROM s WHERE k = '{k}'")) {
                Ok(Output::Rows { rows, .. }) => assert!(rows.is_empty()),
                Ok(other) => panic!("{other:?}"),
                Err(e) => assert!(matches!(e, SqlError::Engine(_)), "{e:?}"),
            }
        }
    }
}

// ------------------------------------------------------------ fuzz-ish

#[test]
fn random_token_soup_never_panics() {
    let (_fs, db) = fresh();
    users(&db);
    ok(&db, "INSERT INTO users VALUES (1, 'a', NULL)");
    const TOKENS: &[&str] = &[
        "SELECT",
        "INSERT",
        "INTO",
        "VALUES",
        "CREATE",
        "TABLE",
        "FROM",
        "WHERE",
        "PRIMARY",
        "KEY",
        "INTEGER",
        "TEXT",
        "BLOB",
        "NULL",
        "UPDATE",
        "DELETE",
        "BEGIN",
        "users",
        "t",
        "id",
        "name",
        "photo",
        "*",
        "(",
        ")",
        ",",
        ";",
        "=",
        "<",
        ">",
        ".",
        "+",
        "-",
        "1",
        "-1",
        "9223372036854775807",
        "99999999999999999999",
        "'a'",
        "'it''s'",
        "'",
        "''",
        "X'00'",
        "X'0'",
        "X'zz'",
        "x'",
        "\"q\"",
        "é",
        "🌳",
        " ",
        "\n",
        "--",
        "/*",
        "\0",
    ];
    let mut s = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for _ in 0..2000 {
        let n = 1 + (next() % 10) as usize;
        let mut sql = String::new();
        for _ in 0..n {
            sql.push_str(TOKENS[(next() % TOKENS.len() as u64) as usize]);
            if next() % 3 != 0 {
                sql.push(' ');
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&db, &sql)));
        assert!(result.is_ok(), "panicked on {sql:?}");
    }
    // The database still works afterwards.
    assert_eq!(rows(&db, "SELECT name FROM users WHERE id = 1").len(), 1);
}
