//! Blind tests for per-connection transactions (#37), written from issue
//! #37, design 0025, docs/protocol.md (2-1, 2-2, 2-3, 6) and ADRs 0006 and
//! 0010 only, without reading the implementation or its tests.
//!
//! Most tests talk to a real server over TCP with frames built by hand from
//! `docs/protocol.md`, so the documented bytes are what is tested. A few
//! use the public `Client` to check `in_transaction()`, and two run the
//! `oakdb` binary to check the command-line options.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use oakdb_server::client::{Client, ClientError};
use oakdb_server::server::{Config, Monitor, Server};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_oakdb");

const HELLO: u8 = 0x01;
const QUERY: u8 = 0x02;
const READY: u8 = 0x81;
const ROWS: u8 = 0x82;
const AFFECTED: u8 = 0x83;
const DONE: u8 = 0x84;
const ERROR: u8 = 0x85;

const WRONG_TXN_STATE: u8 = 0x0B;
const CONFLICT: u8 = 0x20;
const TOO_MANY: u8 = 0x45;
const IDLE_IN_TXN: u8 = 0x47;
const IDLE: u8 = 0x48;

/// Generous socket deadline so a bug fails the test instead of hanging CI.
const READ_DEADLINE: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------
// An in-process server
// ---------------------------------------------------------------------

struct Srv {
    addr: SocketAddr,
    monitor: Monitor<oakdb::fs::StdFs>,
    _dir: tempfile::TempDir,
}

fn start_with(f: impl FnOnce(&mut Config)) -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let db = oakdb::Db::open(dir.path()).unwrap();
    let mut cfg = Config {
        port: 0,
        max_connections: 64,
        ..Config::default()
    };
    f(&mut cfg);
    let server = Server::bind(db, cfg).unwrap();
    let addr = server.local_addr();
    let monitor = server.monitor();
    thread::spawn(move || server.run());
    Srv {
        addr: SocketAddr::from(([127, 0, 0, 1], addr.port())),
        monitor,
        _dir: dir,
    }
}

fn start() -> Srv {
    start_with(|_| {})
}

fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    f()
}

// ---------------------------------------------------------------------
// A raw protocol client built from docs/protocol.md
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum V {
    Null,
    Int(i64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
enum Resp {
    Ready(u16),
    Rows(Vec<String>, Vec<Vec<V>>),
    Affected(u64),
    Done,
    Error(u8, String),
    /// The server closed the socket (EOF or reset).
    Closed,
}

struct Raw {
    s: TcpStream,
}

fn frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
    f.push(cmd);
    f.extend_from_slice(payload);
    f
}

fn hello_payload() -> Vec<u8> {
    let mut p = b"OAKDB".to_vec();
    p.extend_from_slice(&[0, 1]);
    p
}

impl Raw {
    fn connect_only(addr: SocketAddr) -> Raw {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(READ_DEADLINE)).unwrap();
        s.set_write_timeout(Some(READ_DEADLINE)).unwrap();
        s.set_nodelay(true).ok();
        Raw { s }
    }

    fn open(addr: SocketAddr) -> Raw {
        let mut r = Raw::connect_only(addr);
        r.send(&frame(HELLO, &hello_payload()));
        assert_eq!(r.read(), Resp::Ready(1));
        r
    }

    fn send(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).unwrap();
    }

    fn q(&mut self, sql: &str) -> Resp {
        self.send(&frame(QUERY, sql.as_bytes()));
        self.read()
    }

    fn ok(&mut self, sql: &str) {
        match self.q(sql) {
            Resp::Done | Resp::Affected(_) | Resp::Rows(..) => {}
            other => panic!("{sql:?} -> {other:?}"),
        }
    }

    fn ints(&mut self, sql: &str) -> Vec<Vec<i64>> {
        match self.q(sql) {
            Resp::Rows(_, rows) => rows
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|v| match v {
                            V::Int(i) => i,
                            other => panic!("not an integer: {other:?}"),
                        })
                        .collect()
                })
                .collect(),
            other => panic!("{sql:?} -> {other:?}"),
        }
    }

    fn ids(&mut self, table: &str) -> Vec<i64> {
        self.ints(&format!("SELECT id FROM {table}"))
            .into_iter()
            .map(|r| r[0])
            .collect()
    }

    fn read_exact_or_closed(&mut self, buf: &mut [u8]) -> Result<(), Resp> {
        let mut got = 0;
        while got < buf.len() {
            match self.s.read(&mut buf[got..]) {
                Ok(0) => return Err(Resp::Closed),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    return Err(Resp::Closed)
                }
                Err(e) => panic!("read failed (deadline {READ_DEADLINE:?}?): {e}"),
            }
        }
        Ok(())
    }

    fn read(&mut self) -> Resp {
        let mut len = [0u8; 4];
        if let Err(r) = self.read_exact_or_closed(&mut len) {
            return r;
        }
        let len = u32::from_be_bytes(len) as usize;
        assert!(len >= 1, "zero-length frame from server");
        let mut body = vec![0u8; len];
        if let Err(r) = self.read_exact_or_closed(&mut body) {
            panic!("server frame cut short: {r:?}");
        }
        let cmd = body[0];
        let d = &body[1..];
        match cmd {
            READY => Resp::Ready(u16::from_be_bytes([d[0], d[1]])),
            AFFECTED => Resp::Affected(u64::from_be_bytes(d[..8].try_into().unwrap())),
            DONE => Resp::Done,
            ERROR => Resp::Error(d[0], String::from_utf8_lossy(&d[1..]).into_owned()),
            ROWS => parse_rows(d),
            other => panic!("unknown response command 0x{other:02x}"),
        }
    }

    /// Reads the next response and expects ERROR `code` followed by close.
    fn expect_error_then_closed(&mut self, code: u8) -> String {
        match self.read() {
            Resp::Error(c, m) if c == code => {
                assert_eq!(self.read(), Resp::Closed, "not closed after 0x{code:02x}");
                m
            }
            other => panic!("expected ERROR 0x{code:02x} then close, got {other:?}"),
        }
    }
}

fn parse_rows(d: &[u8]) -> Resp {
    let mut p = 0usize;
    let u16_at = |p: &mut usize| {
        let v = u16::from_be_bytes([d[*p], d[*p + 1]]);
        *p += 2;
        v
    };
    let u32_at = |p: &mut usize| {
        let v = u32::from_be_bytes(d[*p..*p + 4].try_into().unwrap());
        *p += 4;
        v
    };
    let ncols = u16_at(&mut p) as usize;
    let mut cols = Vec::new();
    for _ in 0..ncols {
        let n = u16_at(&mut p) as usize;
        cols.push(String::from_utf8(d[p..p + n].to_vec()).unwrap());
        p += n;
    }
    let nrows = u32_at(&mut p) as usize;
    let mut rows = Vec::new();
    for _ in 0..nrows {
        let mut row = Vec::new();
        for _ in 0..ncols {
            let tag = d[p];
            p += 1;
            row.push(match tag {
                0 => V::Null,
                1 => {
                    let v = i64::from_be_bytes(d[p..p + 8].try_into().unwrap());
                    p += 8;
                    V::Int(v)
                }
                2 => {
                    let n = u32_at(&mut p) as usize;
                    let s = String::from_utf8(d[p..p + n].to_vec()).unwrap();
                    p += n;
                    V::Text(s)
                }
                3 => {
                    let n = u32_at(&mut p) as usize;
                    let b = d[p..p + n].to_vec();
                    p += n;
                    V::Blob(b)
                }
                t => panic!("unknown value tag {t}"),
            });
        }
        rows.push(row);
    }
    assert_eq!(p, d.len(), "trailing bytes in ROWS");
    Resp::Rows(cols, rows)
}

fn is_err(r: &Resp, code: u8) -> bool {
    matches!(r, Resp::Error(c, _) if *c == code)
}

fn setup_table(srv: &Srv) {
    let mut c = Raw::open(srv.addr);
    c.ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
}

/// Sends RST instead of FIN when the socket is dropped.
fn drop_with_reset(s: TcpStream) {
    use std::os::unix::io::AsRawFd;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: setsockopt on a socket we own with a correctly sized struct.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
    drop(s);
}

/// The engine's open transactions drop back to `want`.
fn open_txns_settle(srv: &Srv, want: u64) {
    assert!(
        wait_until(Duration::from_secs(10), || srv
            .monitor
            .stats()
            .open_transactions
            == want),
        "open_transactions stayed at {} (want {want})",
        srv.monitor.stats().open_transactions
    );
}

// ---------------------------------------------------------------------
// Isolation, COMMIT, ROLLBACK
// ---------------------------------------------------------------------

#[test]
fn an_uncommitted_insert_is_invisible_to_others_until_commit() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    assert_eq!(a.q("INSERT INTO t VALUES (1, 10)"), Resp::Affected(1));
    // A sees its own write, B does not.
    assert_eq!(a.ids("t"), vec![1]);
    assert_eq!(b.ids("t"), Vec::<i64>::new());
    assert_eq!(a.q("COMMIT"), Resp::Done);
    assert_eq!(b.ids("t"), vec![1]);
}

#[test]
fn a_transaction_sees_the_snapshot_taken_at_begin() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN TRANSACTION"), Resp::Done);
    assert_eq!(b.q("INSERT INTO t VALUES (7, 70)"), Resp::Affected(1));
    assert_eq!(
        a.ids("t"),
        Vec::<i64>::new(),
        "A saw a commit made after its BEGIN"
    );
    assert_eq!(a.q("COMMIT;"), Resp::Done);
    assert_eq!(a.ids("t"), vec![7]);
}

#[test]
fn rollback_discards_every_write_of_the_transaction() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    a.ok("INSERT INTO t VALUES (1, 1)");
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (2, 2)");
    a.ok("UPDATE t SET v = 100 WHERE id = 1");
    a.ok("DELETE FROM t WHERE id = 1");
    assert_eq!(a.q("ROLLBACK"), Resp::Done);
    assert_eq!(a.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
    assert_eq!(b.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
    // Back in autocommit: a ROLLBACK now has nothing to end.
    assert!(is_err(&a.q("ROLLBACK"), WRONG_TXN_STATE));
}

#[test]
fn outside_a_transaction_each_statement_commits_by_itself() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    assert_eq!(a.q("INSERT INTO t VALUES (1, 1)"), Resp::Affected(1));
    assert_eq!(b.ids("t"), vec![1]);
    // A failing autocommit statement leaves nothing behind.
    assert!(is_err(&a.q("INSERT INTO t VALUES (1, 2)"), 0x08));
    assert_eq!(b.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
    // COMMIT / ROLLBACK cannot undo an autocommitted statement.
    let r = a.q("ROLLBACK");
    match &r {
        Resp::Error(WRONG_TXN_STATE, m) => {
            assert!(m.contains("no transaction is open"), "message: {m}")
        }
        other => panic!("ROLLBACK outside a transaction -> {other:?}"),
    }
    assert_eq!(b.ids("t"), vec![1]);
}

// ---------------------------------------------------------------------
// Transaction-state errors (0x0B) and statement errors inside
// ---------------------------------------------------------------------

#[test]
fn begin_twice_and_commit_without_begin_send_0x0b_and_keep_the_state() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);

    match a.q("COMMIT") {
        Resp::Error(WRONG_TXN_STATE, m) => {
            assert!(m.contains("no transaction is open"), "message: {m}")
        }
        other => panic!("COMMIT without BEGIN -> {other:?}"),
    }
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1, 1)");
    let r = a.q("BEGIN");
    assert!(is_err(&r, WRONG_TXN_STATE), "second BEGIN -> {r:?}");
    // The first transaction is still open: its write is still invisible to
    // B and lands at COMMIT.
    assert_eq!(b.ids("t"), Vec::<i64>::new());
    a.ok("INSERT INTO t VALUES (2, 2)");
    assert_eq!(a.q("COMMIT"), Resp::Done);
    assert_eq!(b.ids("t"), vec![1, 2]);
}

#[test]
fn the_0x0b_messages_use_the_words_of_protocol_md() {
    // protocol.md 6: 트랜잭션 중 `BEGIN`("already in a transaction"),
    // 트랜잭션 밖 `COMMIT`/`ROLLBACK`("no transaction is open").
    let srv = start();
    let mut a = Raw::open(srv.addr);
    for sql in ["COMMIT", "ROLLBACK"] {
        match a.q(sql) {
            Resp::Error(WRONG_TXN_STATE, m) => {
                assert!(m.contains("no transaction is open"), "{sql} message: {m}")
            }
            other => panic!("{sql} without BEGIN -> {other:?}"),
        }
    }
    assert_eq!(a.q("BEGIN"), Resp::Done);
    match a.q("BEGIN") {
        Resp::Error(WRONG_TXN_STATE, m) => {
            assert!(m.contains("already in a transaction"), "message: {m}")
        }
        other => panic!("second BEGIN -> {other:?}"),
    }
}

#[test]
fn a_failed_statement_inside_a_transaction_leaves_the_transaction_open() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1, 1)");
    assert!(is_err(&a.q("INSERT INTO t VALUES (1, 9)"), 0x08));
    assert!(is_err(&a.q("SELECT * FROM nope"), 0x03));
    let r = a.q("SELEC");
    assert!(matches!(r, Resp::Error(1..=0x0A, _)), "SELEC -> {r:?}");
    // Still uncommitted, still visible to A only.
    assert_eq!(b.ids("t"), Vec::<i64>::new());
    assert_eq!(a.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
    assert_eq!(a.q("COMMIT"), Resp::Done);
    assert_eq!(b.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
}

#[test]
fn commit_with_trailing_words_is_not_a_commit_and_the_transaction_stays_open() {
    // protocol.md 2-1: "`COMMIT now` ... `0x02`, 트랜잭션은 열린 채".
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1, 1)");
    let r = a.q("COMMIT now");
    assert!(is_err(&r, 0x02), "COMMIT now -> {r:?}");
    assert_eq!(b.ids("t"), Vec::<i64>::new());
    assert_eq!(a.q("ROLLBACK"), Resp::Done);
    assert_eq!(b.ids("t"), Vec::<i64>::new());
}

// ---------------------------------------------------------------------
// Conflicts (0x20)
// ---------------------------------------------------------------------

#[test]
fn the_later_commit_of_the_same_row_gets_0x20_and_writes_nothing() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    let mut c = Raw::open(srv.addr);
    c.ok("INSERT INTO t VALUES (1, 0)");

    assert_eq!(a.q("BEGIN"), Resp::Done);
    assert_eq!(b.q("BEGIN"), Resp::Done);
    assert_eq!(a.q("UPDATE t SET v = 1 WHERE id = 1"), Resp::Affected(1));
    assert_eq!(b.q("UPDATE t SET v = 2 WHERE id = 1"), Resp::Affected(1));
    b.ok("INSERT INTO t VALUES (99, 99)");
    assert_eq!(a.q("COMMIT"), Resp::Done);
    match b.q("COMMIT") {
        Resp::Error(CONFLICT, m) => assert!(!m.is_empty()),
        other => panic!("loser's COMMIT -> {other:?}"),
    }
    // The loser wrote nothing at all: not the row, not its other insert.
    assert_eq!(c.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
    // The loser's transaction is over; the connection is kept.
    assert!(is_err(&b.q("COMMIT"), WRONG_TXN_STATE));
    assert_eq!(b.ints("SELECT id, v FROM t"), vec![vec![1, 1]]);
}

#[test]
fn an_autocommit_write_that_lands_first_makes_the_open_transaction_lose() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    b.ok("INSERT INTO t VALUES (1, 0)");
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("UPDATE t SET v = 1 WHERE id = 1");
    assert_eq!(b.q("UPDATE t SET v = 2 WHERE id = 1"), Resp::Affected(1));
    assert!(is_err(&a.q("COMMIT"), CONFLICT));
    assert_eq!(a.ints("SELECT id, v FROM t"), vec![vec![1, 2]]);
}

#[test]
fn transactions_on_different_rows_both_commit() {
    let srv = start();
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    a.ok("INSERT INTO t VALUES (1, 0)");
    a.ok("INSERT INTO t VALUES (2, 0)");
    assert_eq!(a.q("BEGIN"), Resp::Done);
    assert_eq!(b.q("BEGIN"), Resp::Done);
    a.ok("UPDATE t SET v = 1 WHERE id = 1");
    b.ok("UPDATE t SET v = 2 WHERE id = 2");
    assert_eq!(a.q("COMMIT"), Resp::Done);
    assert_eq!(b.q("COMMIT"), Resp::Done);
    assert_eq!(a.ints("SELECT id, v FROM t"), vec![vec![1, 1], vec![2, 2]]);
}

// ---------------------------------------------------------------------
// Disconnects roll back
// ---------------------------------------------------------------------

/// Opens a transaction with a write on a fresh connection, lets `end`
/// finish the connection, and checks the write never shows up and the
/// seat and engine transaction are given back.
fn disconnect_rolls_back(end: impl FnOnce(Raw)) {
    let srv = start_with(|c| c.max_connections = 2);
    setup_table(&srv);
    let mut watcher = Raw::open(srv.addr);
    let mut a = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1, 1)");
    assert_eq!(srv.monitor.stats().open_transactions, 1);
    end(a);
    open_txns_settle(&srv, 0);
    // The seat comes back: a new connection is taken, not refused.
    let mut again = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            let mut r = Raw::connect_only(srv.addr);
            r.send(&frame(HELLO, &hello_payload()));
            match r.read() {
                Resp::Ready(_) => {
                    again = Some(r);
                    true
                }
                Resp::Error(TOO_MANY, _) | Resp::Closed => false,
                other => panic!("unexpected {other:?}"),
            }
        }),
        "the seat was never given back"
    );
    let mut again = again.unwrap();
    assert_eq!(watcher.ids("t"), Vec::<i64>::new());
    assert_eq!(again.ids("t"), Vec::<i64>::new());
    // And the key is free: no leftover lock or conflict on it.
    assert_eq!(again.q("INSERT INTO t VALUES (1, 5)"), Resp::Affected(1));
    assert_eq!(watcher.ints("SELECT id, v FROM t"), vec![vec![1, 5]]);
}

#[test]
fn a_clean_close_mid_transaction_rolls_back() {
    disconnect_rolls_back(|a| drop(a));
}

#[test]
fn a_shutdown_of_the_write_side_mid_transaction_rolls_back() {
    disconnect_rolls_back(|a| {
        a.s.shutdown(std::net::Shutdown::Write).unwrap();
        // Keep the socket until the server hangs up.
        let mut a = a;
        assert_eq!(a.read(), Resp::Closed);
    });
}

#[test]
fn an_abrupt_reset_mid_transaction_rolls_back() {
    disconnect_rolls_back(|a| drop_with_reset(a.s));
}

#[test]
fn a_half_sent_frame_then_close_rolls_back() {
    disconnect_rolls_back(|mut a| {
        let f = frame(QUERY, b"COMMIT");
        a.send(&f[..6]); // length + command + one byte of SQL
        thread::sleep(Duration::from_millis(50));
        drop(a);
    });
}

#[test]
fn a_half_sent_length_then_close_rolls_back() {
    disconnect_rolls_back(|mut a| {
        a.send(&[0, 0]);
        thread::sleep(Duration::from_millis(50));
        drop(a);
    });
}

#[test]
fn a_broken_frame_mid_transaction_gets_0x40_and_rolls_back() {
    disconnect_rolls_back(|mut a| {
        a.send(&frame(0x7E, b"junk"));
        a.expect_error_then_closed(0x40);
    });
}

#[test]
fn an_oversized_frame_mid_transaction_gets_0x41_and_rolls_back() {
    disconnect_rolls_back(|mut a| {
        let mut f = (16 * 1024 * 1024u32 + 1).to_be_bytes().to_vec();
        f.push(QUERY);
        a.send(&f);
        a.expect_error_then_closed(0x41);
    });
}

#[test]
fn a_second_hello_mid_transaction_gets_0x40_and_rolls_back() {
    disconnect_rolls_back(|mut a| {
        a.send(&frame(HELLO, &hello_payload()));
        a.expect_error_then_closed(0x40);
    });
}

// ---------------------------------------------------------------------
// Idle-in-transaction timeout (0x47) and idle timeout (0x48)
// ---------------------------------------------------------------------

const SHORT: Duration = Duration::from_millis(400);

#[test]
fn idling_inside_a_transaction_rolls_back_sends_0x47_and_closes() {
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    setup_table(&srv);
    let mut b = Raw::open(srv.addr);
    let mut a = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1, 1)");
    let t0 = Instant::now();
    let msg = a.expect_error_then_closed(IDLE_IN_TXN);
    let waited = t0.elapsed();
    assert!(waited >= SHORT * 3 / 4, "closed too early: {waited:?}");
    assert!(!msg.is_empty());
    // protocol.md: the transaction is "이미 롤백됨" when 0x47 arrives.
    assert_eq!(srv.monitor.stats().open_transactions, 0);
    assert_eq!(b.ids("t"), Vec::<i64>::new());
    assert_eq!(b.q("INSERT INTO t VALUES (1, 2)"), Resp::Affected(1));
}

#[test]
fn a_half_sent_frame_inside_a_transaction_hits_the_same_timeout() {
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    setup_table(&srv);
    let mut b = Raw::open(srv.addr);
    let mut a = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1, 1)");
    let f = frame(QUERY, b"COMMIT");
    a.send(&f[..7]);
    a.expect_error_then_closed(IDLE_IN_TXN);
    open_txns_settle(&srv, 0);
    assert_eq!(b.ids("t"), Vec::<i64>::new());
}

#[test]
fn the_in_transaction_timeout_does_not_apply_outside_a_transaction() {
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    thread::sleep(SHORT * 3);
    assert_eq!(a.q("INSERT INTO t VALUES (1, 1)"), Resp::Affected(1));
    // And after a COMMIT the clock stops again.
    assert_eq!(a.q("BEGIN"), Resp::Done);
    assert_eq!(a.q("COMMIT"), Resp::Done);
    thread::sleep(SHORT * 3);
    assert_eq!(a.ids("t"), vec![1]);
    // After a conflict-free ROLLBACK, too.
    assert_eq!(a.q("BEGIN"), Resp::Done);
    assert_eq!(a.q("ROLLBACK"), Resp::Done);
    thread::sleep(SHORT * 3);
    assert_eq!(a.ids("t"), vec![1]);
}

#[test]
fn the_timeout_counts_only_idle_time_not_the_transaction_length() {
    let limit = Duration::from_millis(1500);
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(limit));
    setup_table(&srv);
    let mut a = Raw::open(srv.addr);
    let mut b = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    let t0 = Instant::now();
    let mut i = 0;
    while t0.elapsed() < limit * 3 {
        a.ok(&format!("INSERT INTO t VALUES ({i}, {i})"));
        i += 1;
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(a.q("COMMIT"), Resp::Done);
    assert_eq!(b.ids("t").len(), i as usize);
}

#[test]
fn a_connection_that_never_begins_is_not_closed_by_default() {
    // --idle-timeout is off by default (protocol.md 2-2).
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    let mut a = Raw::open(srv.addr);
    thread::sleep(SHORT * 4);
    assert!(matches!(
        a.q("CREATE TABLE x (id INTEGER PRIMARY KEY)"),
        Resp::Done | Resp::Affected(_)
    ));
}

#[test]
fn idle_timeout_outside_a_transaction_sends_0x48_and_closes() {
    let srv = start_with(|c| c.idle_timeout = Some(SHORT));
    let mut a = Raw::open(srv.addr);
    let t0 = Instant::now();
    a.expect_error_then_closed(IDLE);
    assert!(t0.elapsed() >= SHORT * 3 / 4);
}

#[test]
fn a_client_that_stops_reading_a_big_answer_in_a_transaction_is_given_up_on() {
    // protocol.md 2-2: "응답을 쓰는데 클라이언트가 읽지 않아도 같은 제한으로
    // 포기하고 끊는다(롤백)".
    let srv = start_with(|c| {
        c.idle_in_transaction_timeout = Some(SHORT);
        c.max_connections = 3;
    });
    let mut w = Raw::open(srv.addr);
    w.ok("CREATE TABLE big (id INTEGER PRIMARY KEY, s TEXT)");
    let pad = "x".repeat(600);
    assert_eq!(w.q("BEGIN"), Resp::Done);
    for i in 0..16000 {
        w.ok(&format!("INSERT INTO big VALUES ({i}, '{pad}')"));
    }
    assert_eq!(w.q("COMMIT"), Resp::Done);

    let mut a = Raw::open(srv.addr);
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO big VALUES (-1, 'mine')");
    // Ask for ~10 MB and never read it.
    a.send(&frame(QUERY, b"SELECT * FROM big"));
    open_txns_settle(&srv, 0);
    assert!(
        wait_until(Duration::from_secs(10), || srv.monitor.connections() == 1),
        "the stuck connection still holds a seat: {}",
        srv.monitor.connections()
    );
    assert!(w.ints("SELECT id FROM big WHERE id = -1").is_empty());
    drop(a);
}

#[test]
fn the_cli_flags_set_both_timeouts() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(BIN)
        .arg("serve")
        .arg("--data")
        .arg(dir.path())
        .args([
            "--port",
            "0",
            "--idle-in-transaction-timeout",
            "1",
            "--idle-timeout",
            "2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let line = common::first_line(&mut child);
    let _guard = Kill(child);
    let addr: SocketAddr = line
        .split("listening on ")
        .nth(1)
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .split(',')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let mut a = Raw::open(addr);
    let mut b = Raw::open(addr);
    a.ok("CREATE TABLE t (id INTEGER PRIMARY KEY)");
    assert_eq!(a.q("BEGIN"), Resp::Done);
    a.ok("INSERT INTO t VALUES (1)");
    let t0 = Instant::now();
    a.expect_error_then_closed(IDLE_IN_TXN);
    let w = t0.elapsed();
    assert!(w >= Duration::from_millis(750), "0x47 after {w:?}");
    // b has been idle outside a transaction for > 1 s but < 2 s at most
    // for the first moment; it then gets 0x48 by the 2 s limit.
    let t1 = Instant::now();
    match b.read() {
        Resp::Error(IDLE, _) => assert_eq!(b.read(), Resp::Closed),
        other => panic!("idle connection -> {other:?} after {:?}", t1.elapsed()),
    }
    let mut c = Raw::open(addr);
    assert_eq!(c.ids("t"), Vec::<i64>::new());
}

// ---------------------------------------------------------------------
// The public Client follows the rules of protocol.md 2-1
// ---------------------------------------------------------------------

fn client(srv: &Srv) -> Client {
    let c = Client::connect(srv.addr).unwrap();
    c.set_timeout(Some(READ_DEADLINE)).unwrap();
    c
}

fn server_code(r: Result<oakdb_sql::Output, ClientError>) -> u8 {
    match r {
        Err(ClientError::Server { code, .. }) => code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[test]
fn client_in_transaction_follows_the_four_rules() {
    let srv = start();
    let mut a = client(&srv);
    let mut b = client(&srv);
    a.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    a.query("INSERT INTO t VALUES (1, 0)").unwrap();
    assert!(!a.in_transaction());
    a.query("BEGIN").unwrap();
    assert!(a.in_transaction());
    // Rule 1: a BEGIN that fails changes nothing.
    assert_eq!(server_code(a.query("BEGIN")), WRONG_TXN_STATE);
    assert!(a.in_transaction());
    // Rule 3: a failing statement keeps the transaction.
    assert_eq!(server_code(a.query("INSERT INTO t VALUES (1, 1)")), 0x08);
    assert!(a.in_transaction());
    assert_eq!(server_code(a.query("COMMIT now")), 0x02);
    assert!(a.in_transaction());
    // Rule 2: a COMMIT that conflicts still ends it.
    a.query("UPDATE t SET v = 1 WHERE id = 1").unwrap();
    b.query("UPDATE t SET v = 2 WHERE id = 1").unwrap();
    assert_eq!(server_code(a.query("COMMIT")), CONFLICT);
    assert!(!a.in_transaction());
    // Rule 2: COMMIT with nothing open -> 0x0B, still not in one.
    assert_eq!(server_code(a.query("ROLLBACK")), WRONG_TXN_STATE);
    assert!(!a.in_transaction());
    a.query("BEGIN TRANSACTION;").unwrap();
    assert!(a.in_transaction());
    a.query("ROLLBACK").unwrap();
    assert!(!a.in_transaction());
}

#[test]
fn client_in_transaction_is_false_after_the_server_closes_for_idling() {
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    let mut a = client(&srv);
    a.query("BEGIN").unwrap();
    assert!(a.in_transaction());
    thread::sleep(SHORT * 4);
    match a.query("SELECT 1") {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, IDLE_IN_TXN),
        Err(ClientError::Closed) | Err(ClientError::Io(_)) => {}
        other => panic!("after the timeout -> {other:?}"),
    }
    // Rule 4: a closed connection has no transaction.
    assert!(!a.in_transaction());
}

// ---------------------------------------------------------------------
// Many clients
// ---------------------------------------------------------------------

#[test]
fn eight_clients_incrementing_one_counter_with_retries_lose_no_update() {
    const CLIENTS: usize = 8;
    const EACH: i64 = 25;
    let srv = start();
    {
        let mut c = Raw::open(srv.addr);
        c.ok("CREATE TABLE counter (id INTEGER PRIMARY KEY, n INTEGER)");
        c.ok("INSERT INTO counter VALUES (1, 0)");
    }
    let barrier = Arc::new(Barrier::new(CLIENTS));
    let addr = srv.addr;
    let handles: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut c = Raw::open(addr);
                barrier.wait();
                let mut conflicts = 0u64;
                for _ in 0..EACH {
                    loop {
                        assert_eq!(c.q("BEGIN"), Resp::Done);
                        let n = c.ints("SELECT n FROM counter WHERE id = 1")[0][0];
                        let r = c.q(&format!("UPDATE counter SET n = {} WHERE id = 1", n + 1));
                        assert_eq!(r, Resp::Affected(1));
                        match c.q("COMMIT") {
                            Resp::Done => break,
                            Resp::Error(CONFLICT, _) => {
                                conflicts += 1;
                                // The transaction is over: a fresh BEGIN works.
                            }
                            other => panic!("COMMIT -> {other:?}"),
                        }
                    }
                }
                conflicts
            })
        })
        .collect();
    let conflicts: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let mut c = Raw::open(srv.addr);
    assert_eq!(
        c.ints("SELECT n FROM counter WHERE id = 1"),
        vec![vec![CLIENTS as i64 * EACH]],
        "lost updates ({conflicts} conflicts retried)"
    );
    open_txns_settle(&srv, 0);
}

#[test]
fn many_clients_hold_open_transactions_at_once_and_all_commit() {
    const N: usize = 64;
    let srv = start_with(|c| c.max_connections = N + 2);
    setup_table(&srv);
    let barrier = Arc::new(Barrier::new(N + 1));
    let addr = srv.addr;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut c = Raw::open(addr);
                assert_eq!(c.q("BEGIN"), Resp::Done);
                assert_eq!(
                    c.q(&format!("INSERT INTO t VALUES ({i}, {i})")),
                    Resp::Affected(1)
                );
                // Nobody sees anybody else's row yet.
                assert_eq!(c.ids("t"), vec![i as i64]);
                barrier.wait(); // all open
                barrier.wait(); // main checked
                assert_eq!(c.q("COMMIT"), Resp::Done);
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(srv.monitor.stats().open_transactions, N as u64);
    let mut w = Raw::open(addr);
    assert_eq!(w.ids("t"), Vec::<i64>::new());
    barrier.wait();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(w.ids("t"), (0..N as i64).collect::<Vec<_>>());
    open_txns_settle(&srv, 0);
}

// ---------------------------------------------------------------------
// One client's misbehaviour does not hurt the others
// ---------------------------------------------------------------------

#[test]
fn the_server_keeps_serving_others_while_clients_misbehave() {
    let srv = start_with(|c| c.idle_in_transaction_timeout = Some(SHORT * 2));
    setup_table(&srv);
    // A well-behaved client in the middle of a long transaction that keeps
    // talking.
    let mut good = Raw::open(srv.addr);
    assert_eq!(good.q("BEGIN"), Resp::Done);
    good.ok("INSERT INTO t VALUES (1, 1)");

    // Garbage, oversized frames, half frames, resets, idlers.
    {
        let mut x = Raw::open(srv.addr);
        x.send(&frame(0x33, b"??"));
        x.expect_error_then_closed(0x40);
    }
    good.ok("INSERT INTO t VALUES (2, 2)");
    {
        let mut x = Raw::connect_only(srv.addr);
        x.send(&frame(QUERY, b"SELECT 1"));
        x.expect_error_then_closed(0x42);
    }
    {
        let mut x = Raw::open(srv.addr);
        x.ok("BEGIN");
        x.ok("INSERT INTO t VALUES (3, 3)");
        drop_with_reset(x.s);
    }
    good.ok("INSERT INTO t VALUES (4, 4)");
    {
        let mut x = Raw::open(srv.addr);
        x.ok("BEGIN");
        x.ok("INSERT INTO t VALUES (5, 5)");
        x.send(&frame(QUERY, b"COMMIT")[..5]);
        drop(x);
    }
    let mut idler = Raw::open(srv.addr);
    idler.ok("BEGIN");
    idler.ok("INSERT INTO t VALUES (6, 6)");
    // Keep `good` talking while the idler times out.
    let t0 = Instant::now();
    while t0.elapsed() < SHORT * 4 {
        good.ok("SELECT id FROM t");
        thread::sleep(Duration::from_millis(50));
    }
    idler.expect_error_then_closed(IDLE_IN_TXN);

    assert_eq!(good.q("COMMIT"), Resp::Done);
    let mut fresh = Raw::open(srv.addr);
    assert_eq!(fresh.ids("t"), vec![1, 2, 4]);
    // Every misbehaving client's key is free again.
    for k in [3, 5, 6] {
        assert_eq!(
            fresh.q(&format!("INSERT INTO t VALUES ({k}, 0)")),
            Resp::Affected(1)
        );
    }
    open_txns_settle(&srv, 0);
}
