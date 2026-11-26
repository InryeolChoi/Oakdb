//! Blind tests for graceful shutdown (#38, ticket 26), written from issue
//! #38, design 0028, docs/protocol.md (section 2-4 and the 0x49 row),
//! ADRs 0003/0010 and the `oakdb serve --help` text only, without reading
//! the implementation or its tests.
//!
//! Every test runs the real `oakdb` binary as a child process, talks to it
//! with frames built by hand from docs/protocol.md, and stops it with a
//! real signal (`libc::kill`).

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_oakdb");

const HELLO: u8 = 0x01;
const QUERY: u8 = 0x02;
const READY: u8 = 0x81;
const ROWS: u8 = 0x82;
const AFFECTED: u8 = 0x83;
const DONE: u8 = 0x84;
const ERROR: u8 = 0x85;

const SHUTTING_DOWN: u8 = 0x49;

/// Generous bound for anything that should be "quick" on a slow CI runner.
const SLOW_CI: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------
// Server child process
// ---------------------------------------------------------------------

struct Srv {
    child: Option<Child>,
    port: u16,
    stderr: Arc<Mutex<String>>,
}

impl Drop for Srv {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Srv {
    fn start(dir: &Path, extra: &[&str]) -> Srv {
        let mut child = Command::new(BIN)
            .arg("serve")
            .arg("--data")
            .arg(dir)
            .args(["--port", "0"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oakdb serve");
        let stderr = Arc::new(Mutex::new(String::new()));
        {
            let mut err = child.stderr.take().unwrap();
            let stderr = Arc::clone(&stderr);
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match err.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => stderr
                            .lock()
                            .unwrap()
                            .push_str(&String::from_utf8_lossy(&buf[..n])),
                    }
                }
            });
        }
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let n = out.read_line(&mut line).unwrap_or(0);
            tx.send((n, line)).ok();
            io::copy(&mut out, &mut io::sink()).ok();
        });
        let (n, line) = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(v) => v,
            Err(_) => {
                let _ = child.kill();
                panic!("server printed nothing within 30 s");
            }
        };
        if n == 0 {
            let st = child.wait().unwrap();
            thread::sleep(Duration::from_millis(200));
            panic!(
                "server did not start ({st}); stderr: {}",
                stderr.lock().unwrap()
            );
        }
        let addr = line
            .split("listening on ")
            .nth(1)
            .unwrap_or_else(|| panic!("no 'listening on' in {line:?}"))
            .split(',')
            .next()
            .unwrap()
            .trim()
            .to_string();
        let addr: SocketAddr = addr.parse().unwrap();
        Srv {
            child: Some(child),
            port: addr.port(),
            stderr,
        }
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    fn pid(&self) -> libc::pid_t {
        self.child.as_ref().unwrap().id() as libc::pid_t
    }

    fn signal(&self, sig: libc::c_int) {
        let r = unsafe { libc::kill(self.pid(), sig) };
        assert_eq!(r, 0, "kill failed");
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Waits for the server to have logged `needle` on stderr.
    fn wait_log(&self, needle: &str, within: Duration) {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if self.stderr().contains(needle) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "no {needle:?} on stderr within {within:?}: {}",
            self.stderr()
        );
    }

    /// Waits for the process to exit; kills it and fails if it does not.
    fn wait_exit(&mut self, within: Duration) -> ExitStatus {
        let end = Instant::now() + within;
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(st) = child.try_wait().unwrap() {
                self.child = None;
                // Let the stderr reader catch up.
                thread::sleep(Duration::from_millis(100));
                return st;
            }
            if Instant::now() >= end {
                let _ = child.kill();
                let _ = child.wait();
                self.child = None;
                panic!(
                    "server did not exit within {within:?}; stderr: {}",
                    self.stderr()
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.as_mut().unwrap().try_wait().unwrap().is_none()
    }
}

// ---------------------------------------------------------------------
// Frames, from docs/protocol.md
// ---------------------------------------------------------------------

fn frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + data.len());
    f.extend_from_slice(&((1 + data.len()) as u32).to_be_bytes());
    f.push(cmd);
    f.extend_from_slice(data);
    f
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.set_nodelay(true).unwrap();
    s
}

fn read_frame(s: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len);
    assert!(len >= 1);
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body)?;
    let cmd = body.remove(0);
    Ok((cmd, body))
}

fn open(addr: SocketAddr) -> TcpStream {
    let mut s = connect(addr);
    s.write_all(&frame(HELLO, b"OAKDB\x00\x01")).unwrap();
    let (cmd, data) = read_frame(&mut s).expect("READY");
    assert_eq!((cmd, data.as_slice()), (READY, &[0x00, 0x01][..]));
    s
}

fn query(s: &mut TcpStream, sql: &str) -> io::Result<(u8, Vec<u8>)> {
    s.write_all(&frame(QUERY, sql.as_bytes()))?;
    read_frame(s)
}

fn ok(s: &mut TcpStream, sql: &str) {
    let (cmd, data) = query(s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(
        cmd == DONE || cmd == AFFECTED,
        "{sql}: got {cmd:#x} {}",
        String::from_utf8_lossy(&data)
    );
}

fn error_code(cmd: u8, data: &[u8]) -> Option<u8> {
    (cmd == ERROR && !data.is_empty()).then(|| data[0])
}

/// The stream must be at its end (EOF or reset) with nothing more on it.
fn expect_closed(s: &mut TcpStream) {
    let mut rest = Vec::new();
    match s.read_to_end(&mut rest) {
        Ok(_) => assert!(rest.is_empty(), "bytes after the close: {rest:?}"),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("connection was not closed: {e}"),
    }
}

/// Reads the next frame: it must be ERROR 0x49, then the stream ends.
fn expect_shutdown_then_closed(s: &mut TcpStream) {
    let (cmd, data) = read_frame(s).expect("ERROR 0x49 before the close");
    assert_eq!(
        error_code(cmd, &data),
        Some(SHUTTING_DOWN),
        "expected ERROR 0x49, got {cmd:#x} {}",
        String::from_utf8_lossy(&data)
    );
    expect_closed(s);
}

fn ids_in(s: &mut TcpStream, table: &str) -> BTreeSet<i64> {
    let (cmd, data) = query(s, &format!("SELECT id FROM {table}")).unwrap();
    assert_eq!(
        cmd,
        ROWS,
        "SELECT: {cmd:#x} {}",
        String::from_utf8_lossy(&data)
    );
    // ROWS: ncols u16, (name len u16, name)*, nrows u32, values.
    let mut p = 0;
    let rd = |p: &mut usize, n: usize| {
        let v = &data[*p..*p + n];
        *p += n;
        v.to_vec()
    };
    let ncols = u16::from_be_bytes(rd(&mut p, 2).try_into().unwrap());
    assert_eq!(ncols, 1);
    let nl = u16::from_be_bytes(rd(&mut p, 2).try_into().unwrap()) as usize;
    rd(&mut p, nl);
    let nrows = u32::from_be_bytes(rd(&mut p, 4).try_into().unwrap());
    let mut out = BTreeSet::new();
    for _ in 0..nrows {
        assert_eq!(rd(&mut p, 1)[0], 0x01, "id is INTEGER");
        out.insert(i64::from_be_bytes(rd(&mut p, 8).try_into().unwrap()));
    }
    out
}

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("oakdb.wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn setup_table(srv: &Srv) {
    let mut s = open(srv.addr());
    ok(&mut s, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
}

// ---------------------------------------------------------------------
// Exit codes and restart
// ---------------------------------------------------------------------

#[test]
fn sigterm_exits_0_and_logs_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &[]);
    srv.signal(libc::SIGTERM);
    let st = srv.wait_exit(SLOW_CI);
    assert_eq!(st.code(), Some(0), "status {st}; stderr: {}", srv.stderr());
    assert!(
        srv.stderr().to_lowercase().contains("shut"),
        "a shutdown log line: {}",
        srv.stderr()
    );
}

#[test]
fn sigint_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &[]);
    srv.signal(libc::SIGINT);
    let st = srv.wait_exit(SLOW_CI);
    assert_eq!(st.code(), Some(0), "status {st}; stderr: {}", srv.stderr());
}

#[test]
fn immediate_restart_on_the_same_directory_keeps_committed_rows_and_empties_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    // Header-only WAL size: a fresh database shut down cleanly.
    let empty_wal = {
        let d = tempfile::tempdir().unwrap();
        let mut s = Srv::start(d.path(), &[]);
        s.signal(libc::SIGTERM);
        assert_eq!(s.wait_exit(SLOW_CI).code(), Some(0));
        wal_len(d.path())
    };

    let mut srv = Srv::start(dir.path(), &[]);
    setup_table(&srv);
    {
        let mut s = open(srv.addr());
        for i in 0..200 {
            ok(&mut s, &format!("INSERT INTO t VALUES ({i}, 'v{i}')"));
        }
    }
    assert!(wal_len(dir.path()) > empty_wal, "writes went to the WAL");
    srv.signal(libc::SIGTERM);
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0), "{}", srv.stderr());
    // protocol.md 2-4: after a clean exit the WAL is empty.
    assert_eq!(
        wal_len(dir.path()),
        empty_wal,
        "the WAL should be header-only after a clean shutdown"
    );

    // Immediately, no sleep: the lock must already be released.
    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    let ids = ids_in(&mut s, "t");
    assert_eq!(ids, (0..200).collect::<BTreeSet<_>>());
    drop(s);
    again.signal(libc::SIGTERM);
    assert_eq!(again.wait_exit(SLOW_CI).code(), Some(0));
}

// ---------------------------------------------------------------------
// Connections at shutdown
// ---------------------------------------------------------------------

#[test]
fn idle_connection_outside_a_transaction_gets_0x49_then_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &[]);
    let mut idle = open(srv.addr());
    let mut idle2 = open(srv.addr());
    ok(
        &mut idle2,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    );
    srv.signal(libc::SIGTERM);
    let t0 = Instant::now();
    expect_shutdown_then_closed(&mut idle);
    expect_shutdown_then_closed(&mut idle2);
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "idle connections are let go at once, not after the 10 s grace: {:?}",
        t0.elapsed()
    );
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0), "{}", srv.stderr());
}

#[test]
fn connection_before_hello_gets_0x49_then_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &[]);
    let mut raw = connect(srv.addr());
    // Make sure the server has taken it (a HELLO'd one beside it).
    let _other = open(srv.addr());
    thread::sleep(Duration::from_millis(200));
    srv.signal(libc::SIGTERM);
    expect_shutdown_then_closed(&mut raw);
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0));
}

#[test]
fn new_connections_are_refused_once_shutdown_starts() {
    let dir = tempfile::tempdir().unwrap();
    // A transaction held open keeps the server in its grace for a while.
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "30"]);
    setup_table(&srv);
    let mut tx = open(srv.addr());
    ok(&mut tx, "BEGIN");
    ok(&mut tx, "INSERT INTO t VALUES (1, 'a')");
    srv.signal(libc::SIGTERM);
    srv.wait_log("shutting down", SLOW_CI);

    let end = Instant::now() + Duration::from_secs(10);
    let mut refused = false;
    while Instant::now() < end {
        match TcpStream::connect(srv.addr()) {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused, "{e}");
                refused = true;
                break;
            }
            Ok(mut s) => {
                // Still in the backlog sweep: it must be turned away with
                // 0x49, never served.
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let _ = s.write_all(&frame(HELLO, b"OAKDB\x00\x01"));
                match read_frame(&mut s) {
                    Ok((cmd, data)) => assert_eq!(
                        error_code(cmd, &data),
                        Some(SHUTTING_DOWN),
                        "a new connection during shutdown got {cmd:#x}"
                    ),
                    Err(e) => panic!("no 0x49 for a late connection: {e}"),
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(refused, "connect was never refused during the grace");
    assert!(
        srv.is_running(),
        "still in the grace (the transaction is open)"
    );

    // Finish it so the test ends quickly.
    ok(&mut tx, "ROLLBACK");
    expect_shutdown_then_closed(&mut tx);
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0));
}

#[test]
fn a_transaction_can_commit_within_the_grace_and_the_rows_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "30"]);
    setup_table(&srv);
    let mut tx = open(srv.addr());
    ok(&mut tx, "BEGIN");
    ok(&mut tx, "INSERT INTO t VALUES (1, 'before signal')");
    srv.signal(libc::SIGTERM);
    srv.wait_log("shutting down", SLOW_CI);
    thread::sleep(Duration::from_millis(500));

    // Still inside the grace: more statements are served.
    let (cmd, data) = query(&mut tx, "INSERT INTO t VALUES (2, 'during grace')").unwrap();
    assert_eq!(cmd, AFFECTED, "{cmd:#x} {}", String::from_utf8_lossy(&data));
    let (cmd, data) = query(&mut tx, "COMMIT").unwrap();
    assert_eq!(
        cmd,
        DONE,
        "COMMIT: {cmd:#x} {}",
        String::from_utf8_lossy(&data)
    );
    // Then 0x49 without a request, then the close.
    expect_shutdown_then_closed(&mut tx);

    let t0 = Instant::now();
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0), "{}", srv.stderr());
    assert!(
        t0.elapsed() < Duration::from_secs(15),
        "the server should not wait out the 30 s grace once nothing is open"
    );

    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    assert_eq!(ids_in(&mut s, "t"), BTreeSet::from([1, 2]));
    drop(s);
    again.signal(libc::SIGTERM);
    again.wait_exit(SLOW_CI);
}

#[test]
fn a_transaction_open_past_the_grace_is_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "1"]);
    setup_table(&srv);
    {
        let mut s = open(srv.addr());
        ok(&mut s, "INSERT INTO t VALUES (100, 'committed')");
    }
    let mut tx = open(srv.addr());
    ok(&mut tx, "BEGIN");
    ok(&mut tx, "INSERT INTO t VALUES (1, 'never committed')");
    let t0 = Instant::now();
    srv.signal(libc::SIGTERM);
    // 0x49 arrives on its own after the grace.
    expect_shutdown_then_closed(&mut tx);
    let waited = t0.elapsed();
    assert!(
        waited >= Duration::from_millis(800),
        "0x49 came before the 1 s grace ended: {waited:?}"
    );
    assert_eq!(
        srv.wait_exit(SLOW_CI).code(),
        Some(0),
        "rollback is still exit 0"
    );

    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    assert_eq!(ids_in(&mut s, "t"), BTreeSet::from([100]));
    drop(s);
    again.signal(libc::SIGTERM);
    again.wait_exit(SLOW_CI);
}

#[test]
fn a_commit_sent_after_the_grace_is_answered_with_0x49_and_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "1"]);
    setup_table(&srv);
    let mut tx = open(srv.addr());
    ok(&mut tx, "BEGIN");
    ok(&mut tx, "INSERT INTO t VALUES (1, 'x')");
    srv.signal(libc::SIGTERM);
    thread::sleep(Duration::from_secs(3));
    // The 0x49 may already be waiting; a COMMIT now must not commit.
    let _ = tx.write_all(&frame(QUERY, b"COMMIT"));
    let (cmd, data) = read_frame(&mut tx).expect("an answer: 0x49");
    assert_eq!(error_code(cmd, &data), Some(SHUTTING_DOWN), "got {cmd:#x}");
    expect_closed(&mut tx);
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0));

    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    assert!(ids_in(&mut s, "t").is_empty());
    drop(s);
    again.signal(libc::SIGTERM);
    again.wait_exit(SLOW_CI);
}

#[test]
fn shutdown_grace_0_rolls_back_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "0"]);
    setup_table(&srv);
    let mut tx = open(srv.addr());
    ok(&mut tx, "BEGIN");
    ok(&mut tx, "INSERT INTO t VALUES (1, 'x')");
    let t0 = Instant::now();
    srv.signal(libc::SIGTERM);
    expect_shutdown_then_closed(&mut tx);
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "grace 0 should not wait: {:?}",
        t0.elapsed()
    );
    assert_eq!(srv.wait_exit(SLOW_CI).code(), Some(0));

    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    assert!(ids_in(&mut s, "t").is_empty());
    drop(s);
    again.signal(libc::SIGTERM);
    again.wait_exit(SLOW_CI);
}

fn second_signal_forces_exit(sig: libc::c_int, code: i32) {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "60"]);
    setup_table(&srv);
    {
        let mut s = open(srv.addr());
        ok(&mut s, "INSERT INTO t VALUES (7, 'acked')");
    }
    let mut tx = open(srv.addr());
    ok(&mut tx, "BEGIN");
    ok(&mut tx, "INSERT INTO t VALUES (8, 'open')");
    srv.signal(sig);
    srv.wait_log("shutting down", SLOW_CI);
    thread::sleep(Duration::from_millis(300));
    assert!(srv.is_running(), "waiting in the 60 s grace");
    let t0 = Instant::now();
    srv.signal(sig);
    let st = srv.wait_exit(SLOW_CI);
    assert_eq!(
        st.code(),
        Some(code),
        "status {st}; stderr: {}",
        srv.stderr()
    );
    assert!(t0.elapsed() < Duration::from_secs(10), "at once");

    // Restart at once: the lock is gone, the acked commit is there, the
    // open transaction is not.
    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    assert_eq!(ids_in(&mut s, "t"), BTreeSet::from([7]));
    drop(s);
    again.signal(libc::SIGTERM);
    assert_eq!(again.wait_exit(SLOW_CI).code(), Some(0));
}

#[test]
fn a_second_sigterm_stops_at_once_with_143() {
    second_signal_forces_exit(libc::SIGTERM, 143);
}

#[test]
fn a_second_sigint_stops_at_once_with_130() {
    second_signal_forces_exit(libc::SIGINT, 130);
}

// ---------------------------------------------------------------------
// Under load: no request is left without a reply
// ---------------------------------------------------------------------

#[derive(Default, Debug)]
struct Tally {
    acked: Vec<i64>,   // reply said it was committed
    refused: Vec<i64>, // reply was 0x49: must be absent
    other_errors: Vec<(i64, u8)>,
    no_reply: Vec<(i64, String)>, // promise broken
}

/// One client: autocommit INSERTs, and every fifth round a short
/// BEGIN/INSERT/INSERT/COMMIT. Records the fate of every request.
fn hammer(addr: SocketAddr, base: i64, stop: Arc<AtomicBool>) -> Tally {
    let mut t = Tally::default();
    let mut s = open(addr);
    let mut next = base;
    let mut round = 0u64;
    // A request that got no reply: EOF or reset where a frame should be.
    let ask = |s: &mut TcpStream, sql: &str| -> Result<(u8, Vec<u8>), String> {
        let w = s.write_all(&frame(QUERY, sql.as_bytes()));
        match read_frame(s) {
            Ok(f) => Ok(f),
            Err(e) => Err(format!("{sql}: write {w:?}, read {e}")),
        }
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            return t;
        }
        round += 1;
        if round.is_multiple_of(5) {
            let ids = [next, next + 1];
            next += 2;
            let mut seq = vec!["BEGIN".to_string()];
            for id in ids {
                seq.push(format!("INSERT INTO t VALUES ({id}, 'tx')"));
            }
            seq.push("COMMIT".to_string());
            for (i, sql) in seq.iter().enumerate() {
                match ask(&mut s, sql) {
                    Err(e) => {
                        t.no_reply.push((ids[0], e));
                        return t;
                    }
                    Ok((cmd, data)) => match error_code(cmd, &data) {
                        Some(SHUTTING_DOWN) => {
                            t.refused.extend(ids);
                            return t;
                        }
                        Some(c) => {
                            t.other_errors.push((ids[0], c));
                            if i > 0 && i < seq.len() - 1 {
                                let _ = ask(&mut s, "ROLLBACK");
                            }
                            break;
                        }
                        None if i == seq.len() - 1 => {
                            assert_eq!(cmd, DONE);
                            t.acked.extend(ids);
                        }
                        None => {}
                    },
                }
            }
        } else {
            let id = next;
            next += 1;
            match ask(&mut s, &format!("INSERT INTO t VALUES ({id}, 'auto')")) {
                Err(e) => {
                    t.no_reply.push((id, e));
                    return t;
                }
                Ok((cmd, data)) => match error_code(cmd, &data) {
                    Some(SHUTTING_DOWN) => {
                        t.refused.push(id);
                        // Nothing may follow 0x49.
                        expect_closed(&mut s);
                        return t;
                    }
                    Some(c) => t.other_errors.push((id, c)),
                    None => {
                        assert_eq!(cmd, AFFECTED);
                        t.acked.push(id);
                    }
                },
            }
        }
    }
}

fn one_load_round(round: u64) -> (usize, usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut srv = Srv::start(dir.path(), &["--shutdown-grace", "2"]);
    setup_table(&srv);
    let stop = Arc::new(AtomicBool::new(false));
    let clients: Vec<_> = (0..8)
        .map(|c| {
            let addr = srv.addr();
            let stop = Arc::clone(&stop);
            thread::spawn(move || hammer(addr, (c as i64 + 1) * 1_000_000, stop))
        })
        .collect();
    // A pseudo-random moment between 200 and 1400 ms.
    let ms = 200 + (round.wrapping_mul(2_654_435_761) % 1200);
    thread::sleep(Duration::from_millis(ms));
    srv.signal(libc::SIGTERM);
    let st = srv.wait_exit(Duration::from_secs(40));
    // A safety stop for any client that never heard from the server.
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        st.code(),
        Some(0),
        "round {round}: SIGTERM under load must be a clean exit, got {st}; stderr: {}",
        srv.stderr()
    );
    let tallies: Vec<Tally> = clients.into_iter().map(|h| h.join().unwrap()).collect();

    let mut again = Srv::start(dir.path(), &[]);
    let mut s = open(again.addr());
    let present = ids_in(&mut s, "t");
    drop(s);
    again.signal(libc::SIGTERM);
    assert_eq!(again.wait_exit(SLOW_CI).code(), Some(0));

    let mut acked = 0;
    let mut refused = 0;
    for (c, t) in tallies.iter().enumerate() {
        assert!(
            t.no_reply.is_empty(),
            "round {round} client {c}: requests left without a reply: {:?}",
            t.no_reply
        );
        assert!(
            !t.refused.is_empty(),
            "round {round} client {c}: never told 0x49 (tally {t:?})"
        );
        for id in &t.acked {
            assert!(present.contains(id), "round {round}: acked id {id} lost");
        }
        for id in &t.refused {
            assert!(
                !present.contains(id),
                "round {round}: 0x49'd id {id} was written"
            );
        }
        assert!(
            t.other_errors.is_empty(),
            "round {round} client {c}: unexpected errors {:?}",
            t.other_errors
        );
        acked += t.acked.len();
        refused += t.refused.len();
    }
    // Everything present was acknowledged (nothing unacked slipped in).
    let all_acked: BTreeSet<i64> = tallies.iter().flat_map(|t| t.acked.clone()).collect();
    assert_eq!(
        present, all_acked,
        "round {round}: present rows == acked rows"
    );
    (acked, refused)
}

#[test]
fn under_load_every_request_gets_a_reply_and_every_acked_commit_survives() {
    let mut total = (0, 0);
    for round in 1..=5 {
        let (a, r) = one_load_round(round);
        assert!(a > 0, "round {round}: some writes were acknowledged");
        total.0 += a;
        total.1 += r;
    }
    eprintln!("load: {} acked, {} answered with 0x49", total.0, total.1);
}
