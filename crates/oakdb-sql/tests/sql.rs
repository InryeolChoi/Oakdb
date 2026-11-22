//! The first SQL pass (issue #31, design 0017): CREATE TABLE, INSERT and
//! SELECT by primary key. As the issue asks, these tests only look at what
//! running SQL strings returns, never at the engine's keys.

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, ColumnType, Output, SqlError, Value};

const DIR: &str = "/oak";

fn open(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), DIR).unwrap()
}

fn new_db() -> Db<SimFs> {
    open(&SimFs::new())
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

fn int(i: i64) -> Value {
    Value::Integer(i)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn with_users() -> Db<SimFs> {
    let db = new_db();
    run(
        &db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, photo BLOB)",
    );
    db
}

fn unsupported(db: &Db<SimFs>, sql: &str) -> String {
    match fail(db, sql) {
        SqlError::Unsupported(what) => what,
        other => panic!("{sql}: expected unsupported, got {other:?}"),
    }
}

// ------------------------------------------------------------ happy paths

#[test]
fn create_table_produces_nothing_and_insert_counts_its_rows() {
    let db = new_db();
    assert_eq!(
        run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"),
        Output::Done
    );
    assert_eq!(
        run(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')"),
        Output::Affected(3)
    );
}

#[test]
fn an_inserted_row_is_read_back_by_its_primary_key() {
    let db = with_users();
    run(&db, "INSERT INTO users VALUES (7, 'kim', X'00ff10')");

    assert_eq!(
        run(&db, "SELECT * FROM users WHERE id = 7"),
        Output::Rows {
            columns: vec!["id".into(), "name".into(), "photo".into()],
            rows: vec![vec![
                int(7),
                text("kim"),
                Value::Blob(vec![0x00, 0xff, 0x10])
            ]],
        }
    );
}

#[test]
fn select_returns_only_the_named_columns_in_the_order_named() {
    let db = with_users();
    run(&db, "INSERT INTO users VALUES (1, 'kim', NULL)");

    assert_eq!(
        run(&db, "SELECT name, id FROM users WHERE id = 1"),
        Output::Rows {
            columns: vec!["name".into(), "id".into()],
            rows: vec![vec![text("kim"), int(1)]],
        }
    );
}

#[test]
fn a_primary_key_with_no_row_selects_nothing() {
    let db = with_users();
    run(&db, "INSERT INTO users VALUES (1, 'kim', NULL)");

    assert_eq!(
        run(&db, "SELECT * FROM users WHERE id = 2"),
        Output::Rows {
            columns: vec!["id".into(), "name".into(), "photo".into()],
            rows: vec![],
        }
    );
}

#[test]
fn where_primary_key_equals_null_selects_nothing() {
    let db = with_users();
    run(&db, "INSERT INTO users VALUES (1, 'kim', NULL)");

    assert_eq!(
        rows(&db, "SELECT * FROM users WHERE id = NULL"),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn every_row_of_a_multi_row_insert_is_there() {
    let db = with_users();
    run(
        &db,
        "INSERT INTO users VALUES (1, 'a', NULL), (2, 'b', NULL), (3, 'c', NULL)",
    );

    for (id, name) in [(1, "a"), (2, "b"), (3, "c")] {
        assert_eq!(
            rows(&db, &format!("SELECT name FROM users WHERE id = {id}")),
            vec![vec![text(name)]]
        );
    }
}

#[test]
fn insert_with_a_column_list_fills_the_rest_with_null() {
    let db = with_users();
    run(&db, "INSERT INTO users (name, id) VALUES ('lee', 5)");

    assert_eq!(
        rows(&db, "SELECT * FROM users WHERE id = 5"),
        vec![vec![int(5), text("lee"), Value::Null]]
    );
}

#[test]
fn keywords_ignore_case_and_a_trailing_semicolon_is_allowed() {
    let db = new_db();
    run(&db, "create table t (id integer primary key, v Text);");
    run(&db, "Insert Into t Values (1, 'x');");

    assert_eq!(
        rows(&db, "sElEcT v fRoM t wHeRe id = 1 ;"),
        vec![vec![text("x")]]
    );
}

#[test]
fn table_and_column_names_keep_their_case() {
    let db = new_db();
    run(&db, "CREATE TABLE Users (Id INTEGER PRIMARY KEY)");
    run(&db, "CREATE TABLE users (id INTEGER PRIMARY KEY)");
    run(&db, "INSERT INTO Users VALUES (1)");

    assert_eq!(
        rows(&db, "SELECT Id FROM Users WHERE Id = 1"),
        vec![vec![int(1)]]
    );
    assert_eq!(
        rows(&db, "SELECT id FROM users WHERE id = 1"),
        Vec::<Vec<Value>>::new()
    );
    assert!(matches!(
        fail(&db, "SELECT id FROM Users WHERE Id = 1"),
        SqlError::NoSuchColumn { .. }
    ));
}

#[test]
fn quotes_nuls_and_non_ascii_text_round_trip() {
    let db = new_db();
    run(&db, "CREATE TABLE t (k TEXT PRIMARY KEY, v TEXT)");
    run(&db, "INSERT INTO t VALUES ('it''s', '김치 '''' 찌개')");

    assert_eq!(
        rows(&db, "SELECT v FROM t WHERE k = 'it''s'"),
        vec![vec![text("김치 '' 찌개")]]
    );
}

#[test]
fn integer_primary_keys_at_both_ends_of_i64_round_trip() {
    let db = new_db();
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
    run(
        &db,
        "INSERT INTO t VALUES (-9223372036854775808, 1), (9223372036854775807, 2), (0, 3), (-1, 4)",
    );

    for (id, v) in [(i64::MIN, 1), (i64::MAX, 2), (0, 3), (-1, 4)] {
        assert_eq!(
            rows(&db, &format!("SELECT v FROM t WHERE id = {id}")),
            vec![vec![int(v)]]
        );
    }
}

#[test]
fn text_and_blob_primary_keys_find_their_rows() {
    let db = new_db();
    run(&db, "CREATE TABLE by_text (k TEXT PRIMARY KEY, v INTEGER)");
    run(&db, "CREATE TABLE by_blob (k BLOB PRIMARY KEY, v INTEGER)");
    run(
        &db,
        "INSERT INTO by_text VALUES ('a', 1), ('ab', 2), ('', 3)",
    );
    run(
        &db,
        "INSERT INTO by_blob VALUES (X'00', 1), (X'0000', 2), (X'', 3)",
    );

    assert_eq!(
        rows(&db, "SELECT v FROM by_text WHERE k = 'ab'"),
        vec![vec![int(2)]]
    );
    assert_eq!(
        rows(&db, "SELECT v FROM by_text WHERE k = ''"),
        vec![vec![int(3)]]
    );
    assert_eq!(
        rows(&db, "SELECT v FROM by_blob WHERE k = X'0000'"),
        vec![vec![int(2)]]
    );
    assert_eq!(
        rows(&db, "SELECT v FROM by_blob WHERE k = X''"),
        vec![vec![int(3)]]
    );
}

#[test]
fn the_primary_key_need_not_be_the_first_column() {
    let db = new_db();
    run(&db, "CREATE TABLE t (v TEXT, id INTEGER PRIMARY KEY)");
    run(&db, "INSERT INTO t VALUES ('x', 9)");

    assert_eq!(
        rows(&db, "SELECT * FROM t WHERE id = 9"),
        vec![vec![text("x"), int(9)]]
    );
}

#[test]
fn tables_keep_their_rows_apart() {
    let db = new_db();
    run(&db, "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)");
    run(&db, "CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT)");
    run(&db, "INSERT INTO a VALUES (1, 'from a')");
    run(&db, "INSERT INTO b VALUES (1, 'from b')");

    assert_eq!(
        rows(&db, "SELECT v FROM a WHERE id = 1"),
        vec![vec![text("from a")]]
    );
    assert_eq!(
        rows(&db, "SELECT v FROM b WHERE id = 1"),
        vec![vec![text("from b")]]
    );
}

#[test]
fn tables_and_rows_are_still_there_after_closing_and_opening_again() {
    let fs = SimFs::new();
    {
        let db = open(&fs);
        run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        run(&db, "INSERT INTO t VALUES (1, 'kept')");
        db.close().unwrap();
    }
    let db = open(&fs);
    assert_eq!(
        rows(&db, "SELECT v FROM t WHERE id = 1"),
        vec![vec![text("kept")]]
    );
    // The table id counter survived too: a new table gets rows of its own.
    run(&db, "CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)");
    assert_eq!(
        rows(&db, "SELECT v FROM u WHERE id = 1"),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn it_works_on_the_real_file_system() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path().join("db")).unwrap();
    execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v BLOB)").unwrap();
    execute(&db, "INSERT INTO t VALUES (1, X'cafe')").unwrap();

    assert_eq!(
        execute(&db, "SELECT v FROM t WHERE id = 1").unwrap(),
        Output::Rows {
            columns: vec!["v".into()],
            rows: vec![vec![Value::Blob(vec![0xca, 0xfe])]],
        }
    );
}

#[test]
fn acknowledged_tables_and_rows_survive_a_power_cut() {
    for seed in 0..64 {
        let fs = SimFs::new();
        let db = open(&fs);
        run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        for i in 0..20 {
            run(&db, &format!("INSERT INTO t VALUES ({i}, 'row {i}')"));
        }
        fs.crash(seed);
        drop(db);

        let db = open(&fs);
        for i in 0..20 {
            assert_eq!(
                rows(&db, &format!("SELECT v FROM t WHERE id = {i}")),
                vec![vec![text(&format!("row {i}"))]],
                "seed {seed}, row {i}"
            );
        }
    }
}

// ----------------------------------------------------------------- errors

#[test]
fn creating_a_table_twice_is_an_error() {
    let db = with_users();
    assert!(matches!(
        fail(&db, "CREATE TABLE users (id INTEGER PRIMARY KEY)"),
        SqlError::TableExists(name) if name == "users"
    ));
}

#[test]
fn a_table_that_does_not_exist_is_an_error() {
    let db = new_db();
    assert!(matches!(
        fail(&db, "INSERT INTO nope VALUES (1)"),
        SqlError::NoSuchTable(name) if name == "nope"
    ));
    assert!(matches!(
        fail(&db, "SELECT * FROM nope WHERE id = 1"),
        SqlError::NoSuchTable(name) if name == "nope"
    ));
}

#[test]
fn a_column_that_does_not_exist_is_an_error() {
    let db = with_users();
    for sql in [
        "SELECT age FROM users WHERE id = 1",
        "SELECT * FROM users WHERE age = 1",
        "INSERT INTO users (id, age) VALUES (1, 2)",
    ] {
        assert!(
            matches!(
                fail(&db, sql),
                SqlError::NoSuchColumn { table, column } if table == "users" && column == "age"
            ),
            "{sql}"
        );
    }
}

#[test]
fn a_column_named_twice_is_an_error() {
    let db = new_db();
    assert!(matches!(
        fail(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, id TEXT)"),
        SqlError::DuplicateColumn(name) if name == "id"
    ));
    let db = with_users();
    assert!(matches!(
        fail(&db, "INSERT INTO users (id, id) VALUES (1, 2)"),
        SqlError::DuplicateColumn(name) if name == "id"
    ));
}

#[test]
fn a_row_with_the_wrong_number_of_values_is_an_error() {
    let db = with_users();
    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES (1, 'a')"),
        SqlError::ValueCount {
            expected: 3,
            found: 2
        }
    ));
    assert!(matches!(
        fail(&db, "INSERT INTO users (id) VALUES (1, 'a')"),
        SqlError::ValueCount {
            expected: 1,
            found: 2
        }
    ));
}

#[test]
fn a_primary_key_that_is_already_there_is_an_error() {
    let db = with_users();
    run(&db, "INSERT INTO users VALUES (1, 'kim', NULL)");

    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES (1, 'lee', NULL)"),
        SqlError::DuplicateKey { table } if table == "users"
    ));
    assert_eq!(
        rows(&db, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("kim")]]
    );
}

#[test]
fn a_failing_insert_adds_none_of_its_rows() {
    let db = with_users();
    // The third row repeats the first row's key, in the same statement.
    assert!(matches!(
        fail(
            &db,
            "INSERT INTO users VALUES (1, 'a', NULL), (2, 'b', NULL), (1, 'c', NULL)"
        ),
        SqlError::DuplicateKey { .. }
    ));
    // A bad value in the last row throws away the rows before it too.
    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES (3, 'a', NULL), (4, 5, NULL)"),
        SqlError::TypeMismatch { .. }
    ));

    for id in 1..=4 {
        assert_eq!(
            rows(&db, &format!("SELECT * FROM users WHERE id = {id}")),
            Vec::<Vec<Value>>::new(),
            "id {id}"
        );
    }
}

#[test]
fn a_value_of_the_wrong_type_is_an_error() {
    let db = with_users();
    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES (1, 2, NULL)"),
        SqlError::TypeMismatch { column, expected: ColumnType::Text, found: Some(ColumnType::Integer) }
            if column == "name"
    ));
    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES ('one', 'a', NULL)"),
        SqlError::TypeMismatch { column, expected: ColumnType::Integer, found: Some(ColumnType::Text) }
            if column == "id"
    ));
    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES (1, 'a', 'not a blob')"),
        SqlError::TypeMismatch {
            expected: ColumnType::Blob,
            ..
        }
    ));
    assert!(matches!(
        fail(&db, "SELECT * FROM users WHERE id = 'one'"),
        SqlError::TypeMismatch { column, .. } if column == "id"
    ));
}

#[test]
fn a_null_primary_key_is_an_error() {
    let db = with_users();
    assert!(matches!(
        fail(&db, "INSERT INTO users VALUES (NULL, 'a', NULL)"),
        SqlError::TypeMismatch { column, found: None, .. } if column == "id"
    ));
    // Leaving the primary key out of the column list makes it NULL.
    assert!(matches!(
        fail(&db, "INSERT INTO users (name) VALUES ('a')"),
        SqlError::TypeMismatch { column, found: None, .. } if column == "id"
    ));
}

#[test]
fn a_primary_key_too_long_for_the_engine_is_an_engine_error() {
    let db = new_db();
    run(&db, "CREATE TABLE t (k TEXT PRIMARY KEY)");
    let long = "x".repeat(1000);
    assert!(matches!(
        fail(&db, &format!("INSERT INTO t VALUES ('{long}')")),
        SqlError::Engine(oakdb::Error::KeyTooLarge { .. })
    ));
}

#[test]
fn statements_oakdb_does_not_support_name_what_they_found() {
    let db = with_users();
    for (sql, named) in [
        ("UPDATE users SET name = 'x'", "UPDATE"),
        ("DELETE FROM users WHERE id = 1", "DELETE"),
        ("BEGIN", "BEGIN"),
        ("DROP TABLE users", "DROP"),
        ("CREATE INDEX i ON users (name)", "INDEX"),
    ] {
        let what = unsupported(&db, sql);
        assert!(what.contains(named), "{sql}: {what}");
    }
}

#[test]
fn clauses_oakdb_does_not_support_name_what_they_found() {
    let db = with_users();
    for (sql, named) in [
        ("SELECT * FROM users WHERE id > 1", ">"),
        ("SELECT * FROM users WHERE id <= 1", "<="),
        ("SELECT * FROM users", "WHERE"),
        ("SELECT * FROM users ORDER BY id", "ORDER"),
        ("SELECT * FROM users WHERE id = 1 AND name = 'a'", "AND"),
        ("SELECT * FROM users WHERE name = 'kim'", "name"),
        ("CREATE TABLE t (id REAL PRIMARY KEY)", "REAL"),
        (
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
            "NOT",
        ),
        ("CREATE TABLE t (id INTEGER, v TEXT)", "PRIMARY KEY"),
        (
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)",
            "PRIMARY KEY",
        ),
        (
            "INSERT INTO users SELECT * FROM users WHERE id = 1",
            "SELECT",
        ),
        ("INSERT INTO users VALUES (1 + 1, 'a', NULL)", "+"),
        ("INSERT INTO users VALUES (1.5, 'a', NULL)", "."),
        (
            "SELECT * FROM users WHERE id = 1; SELECT * FROM users WHERE id = 2",
            "SELECT",
        ),
    ] {
        let what = unsupported(&db, sql);
        assert!(what.contains(named), "{sql}: {what}");
    }
}

#[test]
fn broken_statements_are_syntax_errors_that_say_where() {
    let db = with_users();
    for (sql, at) in [
        // A missing closing parenthesis: the statement ends at byte 38.
        ("CREATE TABLE t (id INTEGER PRIMARY KEY", 38),
        // A value where a comma or `)` belongs.
        ("INSERT INTO users VALUES (1 'a' NULL)", 28),
        // An unclosed quote, where it opens.
        ("INSERT INTO users VALUES (1, 'a, NULL)", 29),
        // A keyword where a name belongs.
        ("CREATE TABLE select (id INTEGER PRIMARY KEY)", 13),
        // An integer that does not fit in 64 bits.
        ("SELECT * FROM users WHERE id = 9223372036854775808", 31),
        // Nothing at all.
        ("", 0),
    ] {
        match fail(&db, sql) {
            SqlError::Syntax { position, .. } => assert_eq!(position, at, "{sql}"),
            other => panic!("{sql}: expected a syntax error, got {other:?}"),
        }
    }
}

#[test]
fn errors_read_as_sentences() {
    let db = with_users();
    assert_eq!(
        fail(&db, "SELECT * FROM users WHERE id > 1").to_string(),
        "unsupported: operator `>` in WHERE; only `<primary key> = <value>` is supported"
    );
    assert_eq!(
        fail(&db, "SELECT * FROM nope WHERE id = 1").to_string(),
        "no such table: nope"
    );
}
