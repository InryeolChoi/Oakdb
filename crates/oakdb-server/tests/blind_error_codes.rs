//! Blind tests for issue #34's wire codes, written from
//! docs/protocol.md section 6 and docs/design/0027 E8 only, without
//! reading the implementation or the PR's tests (PR #85's verification).
//! The expected bytes are written out here, not taken from the crate's
//! `code` constants, so the documented numbers are what is tested.

use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use oakdb::Db;
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::error_code;
use oakdb_server::server::{Config, Server};
use oakdb_sql::{every_kind, Output, SqlError, TransactionError};

const TIMEOUT: Duration = Duration::from_secs(30);

fn start(dir: &std::path::Path) -> SocketAddr {
    let db = Db::open(dir).unwrap();
    let server = Server::bind(
        db,
        Config {
            port: 0,
            ..Config::default()
        },
    )
    .unwrap();
    let addr = server.local_addr();
    thread::spawn(move || server.run());
    addr
}

fn connect(addr: SocketAddr) -> Client {
    let c = Client::connect(addr).unwrap();
    c.set_timeout(Some(TIMEOUT)).unwrap();
    c
}

fn code(c: &mut Client, sql: &str) -> u8 {
    match c.query(sql) {
        Err(ClientError::Server { code, .. }) => code,
        other => panic!("{sql:?}: expected an ERROR, got {other:?}"),
    }
}

fn message(c: &mut Client, sql: &str) -> String {
    match c.query(sql) {
        Err(ClientError::Server { message, .. }) => message,
        other => panic!("{sql:?}: expected an ERROR, got {other:?}"),
    }
}

/// Every statement's documented code, sent over one connection; after
/// each error the next statement still works on the same connection.
#[test]
fn each_kind_has_its_documented_code_over_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path());
    let mut c = connect(addr);
    c.query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.query("INSERT INTO t VALUES (1, 'a')").unwrap();
    c.query("INSERT INTO t VALUES (2, 'b')").unwrap();

    let cases: &[(&str, u8)] = &[
        ("SELECT * FROM t\nWHERE id = = 1", 0x01),
        ("SELECT * FORM t", 0x01),
        (
            "CREATE TABLE IF NOT EXISTS u (id INTEGER PRIMARY KEY)",
            0x02,
        ),
        ("SELECT COUNT(*) FROM t", 0x02),
        ("SELECT t.id FROM t", 0x02),
        ("-- 주석\nSELECT * FROM t", 0x02),
        ("SELECT * FROM nope", 0x03),
        ("SELECT nope FROM t", 0x04),
        ("CREATE TABLE t (id INTEGER PRIMARY KEY)", 0x05),
        ("CREATE TABLE u (a INTEGER PRIMARY KEY, a TEXT)", 0x06),
        ("INSERT INTO t VALUES (3)", 0x07),
        ("INSERT INTO t VALUES (1, 'dup')", 0x08),
        ("INSERT INTO t VALUES ('x', 'a')", 0x09),
        ("SELECT * FROM t WHERE id = '5'", 0x09),
        ("SELECT * FROM t WHERE id + 1 = 'a'", 0x09),
        ("SELECT * FROM t WHERE name + 1 = 2", 0x09),
        ("SELECT * FROM t WHERE id", 0x09),
        ("UPDATE t SET name = 1", 0x09),
        ("UPDATE t SET id = id + 9223372036854775807", 0x0C),
        ("SELECT * FROM t WHERE 1 / 0 = 1", 0x0C),
        ("SELECT * FROM t WHERE id = 99999999999999999999", 0x0C),
        ("INSERT INTO t VALUES (NULL, 'a')", 0x0D),
        ("UPDATE t SET id = NULL", 0x0D),
    ];
    let mut wrong = Vec::new();
    for (sql, want) in cases {
        let got = code(&mut c, sql);
        if got != *want {
            wrong.push(format!("{sql:?}: want 0x{want:02X}, got 0x{got:02X}"));
        }
        // The connection survives every one of these (protocol: 유지).
        match c.query("SELECT name FROM t WHERE id = 1").unwrap() {
            Output::Rows { rows, .. } => assert_eq!(rows.len(), 1, "after {sql:?}"),
            other => panic!("after {sql:?}: {other:?}"),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn type_error_code_is_the_same_on_an_empty_table() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path());
    let mut c = connect(addr);
    c.query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    for sql in [
        "SELECT * FROM t WHERE id + 1 = 'a'",
        "SELECT * FROM t WHERE name + 1 = 2",
        "SELECT * FROM t WHERE id",
        "UPDATE t SET id = 'x'",
        "DELETE FROM t WHERE name = 1",
    ] {
        assert_eq!(code(&mut c, sql), 0x09, "{sql:?}");
    }
}

#[test]
fn syntax_error_message_on_the_wire_has_line_and_column() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path());
    let mut c = connect(addr);
    c.query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    let m = message(&mut c, "SELECT * FROM t\nWHERE id = = 1");
    assert!(m.contains("line 2, column 12"), "{m}");
    let m = message(&mut c, "INSERT INTO t VALUES (1, '김치', ,)");
    assert!(m.contains("line 1, column 32"), "{m}");
}

/// The real mapping the server uses, over every known kind: the codes
/// from protocol.md section 6, and nothing falls to 0x1F except the
/// transaction-state error the protocol says waits for #37 (0x0B).
#[test]
fn no_known_kind_maps_to_0x1f_except_the_pending_transaction_kind() {
    let mut wrong = Vec::new();
    for e in every_kind() {
        let got = error_code(&e);
        let want: &[u8] = match &e {
            SqlError::Syntax { .. } => &[0x01],
            SqlError::Unsupported(_) => &[0x02],
            SqlError::NoSuchTable(_) => &[0x03],
            SqlError::NoSuchColumn { .. } => &[0x04],
            SqlError::TableExists(_) => &[0x05],
            SqlError::DuplicateColumn(_) => &[0x06],
            SqlError::ValueCount { .. } => &[0x07],
            SqlError::DuplicateKey { .. } => &[0x08],
            SqlError::TypeMismatch { .. } | SqlError::Type(_) => &[0x09],
            SqlError::Corrupt(_) => &[0x0A],
            // "#37 전까지 이 오류는 0x1F로 온다"; 0x0B once #37 lands.
            SqlError::Transaction(_) => &[0x1F, 0x0B],
            SqlError::Arithmetic(_) => &[0x0C],
            SqlError::NullKey { .. } => &[0x0D],
            SqlError::TransactionConflict(_) => &[0x20],
            SqlError::Engine(oakdb::Error::Conflict { .. }) => &[0x20],
            SqlError::Engine(oakdb::Error::KeyTooLarge { .. }) => &[0x21],
            SqlError::Engine(oakdb::Error::ValueTooLarge { .. }) => &[0x22],
            SqlError::Engine(oakdb::Error::OutcomeUnknown(_)) => &[0x25],
            SqlError::Engine(oakdb::Error::Stopped(_)) => &[0x26],
            SqlError::Engine(_) => &[0x23, 0x24, 0x3F],
            _ => &[],
        };
        if !want.contains(&got) {
            wrong.push(format!("{e:?}: want {want:02X?}, got 0x{got:02X}"));
        }
        if got == 0x1F && !matches!(e, SqlError::Transaction(_)) {
            wrong.push(format!("{e:?} falls to 0x1F"));
        }
        // Statement errors stay in 0x01-0x1F, engine ones in 0x20-0x3F.
        let in_range = match &e {
            SqlError::TransactionConflict(_) | SqlError::Engine(_) => (0x20..=0x3F).contains(&got),
            _ => (0x01..=0x1F).contains(&got),
        };
        if !in_range {
            wrong.push(format!("{e:?}: 0x{got:02X} outside its documented range"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn begin_without_a_session_is_the_pending_transaction_code() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path());
    let mut c = connect(addr);
    let got = code(&mut c, "BEGIN");
    assert!(got == 0x1F || got == 0x0B, "got 0x{got:02X}");
    // And the mapping agrees for the kind itself.
    let e = SqlError::Transaction(TransactionError::NeedsSession);
    assert_eq!(error_code(&e), got);
    c.query("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
}
