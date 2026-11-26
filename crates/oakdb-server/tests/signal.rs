//! Real SIGTERM and SIGINT sent to `oakdb serve` (issue #38, design 0028
//! Q11): the exit codes, the second signal, an immediate restart on the
//! same directory, and shutdown under load, where no request may be cut
//! off without an answer.
#![cfg(unix)]

mod common;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use common::{first_line, DEADLINE};
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::code;
use oakdb_sql::{Output, Value};

/// The WAL after a checkpoint: its header alone.
const EMPTY_WAL: u64 = 20;

/// `oakdb serve` on `data`, with its port and everything it logs.
struct Serving {
    child: Child,
    port: u16,
    log: Arc<Mutex<String>>,
}

impl Serving {
    fn start(data: &Path, extra: &[&str]) -> Serving {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oakdb"))
            .args(["serve", "--port", "0", "--data"])
            .arg(data)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let log = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&log);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut log = sink.lock().unwrap();
                log.push_str(&line);
                log.push('\n');
            }
        });
        let line = first_line(&mut child);
        let port = line
            .split("listening on ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .and_then(|addr| addr.rsplit(':').next())
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {line:?}"));
        Serving { child, port, log }
    }

    fn connect(&self) -> Client {
        let client = Client::connect(("127.0.0.1", self.port)).unwrap();
        client.set_timeout(Some(Duration::from_secs(10))).unwrap();
        client
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: kill only sends a signal to our own child.
        let rc = unsafe { libc::kill(self.child.id() as libc::pid_t, signal) };
        assert_eq!(rc, 0);
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// Waits until the log says `what`, failing past the deadline.
    fn wait_for_log(&mut self, what: &str) {
        let deadline = Instant::now() + DEADLINE;
        while !self.log().contains(what) {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the log never said {what:?}:\n{}", self.log());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the process to end, killing it and failing past the
    /// deadline.
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader thread take the last lines.
                thread::sleep(Duration::from_millis(50));
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the server did not exit:\n{}", self.log());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ok(client: &mut Client, sql: &str) -> Output {
    client.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ids(client: &mut Client) -> BTreeSet<i64> {
    match ok(client, "SELECT id FROM t") {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match row[0] {
                Value::Integer(id) => id,
                ref other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn wal_len(data: &Path) -> u64 {
    std::fs::metadata(data.join(oakdb::WAL_FILE)).unwrap().len()
}

/// Shuts the server down with `signal` and checks it ended cleanly: exit
/// 0, the shutdown logged, the WAL checkpointed away.
fn clean_shutdown(signal: libc::c_int, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Serving::start(dir.path(), &[]);
    let mut a = server.connect();
    ok(&mut a, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    ok(&mut a, "INSERT INTO t VALUES (1)");
    server.signal(signal);
    let status = server.wait();
    let log = server.log();
    assert_eq!(status.code(), Some(0), "{status:?}\n{log}");
    assert!(log.contains(&format!("{name} received")), "{log}");
    assert!(log.contains("checkpointed and closed"), "{log}");
    assert_eq!(
        wal_len(dir.path()),
        EMPTY_WAL,
        "nothing for the next start to replay"
    );
    // The client hears why, on its next request.
    match a.query("SELECT id FROM t") {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, code::SHUTTING_DOWN),
        other => panic!("{other:?}"),
    }

    // Straight back up on the same directory: the lock is free.
    let restarted = Serving::start(dir.path(), &[]);
    assert_eq!(ids(&mut restarted.connect()), BTreeSet::from([1]));
}

#[test]
fn sigterm_shuts_down_cleanly_with_exit_0_and_the_server_restarts_at_once() {
    clean_shutdown(libc::SIGTERM, "SIGTERM");
}

#[test]
fn sigint_shuts_down_cleanly_with_exit_0() {
    clean_shutdown(libc::SIGINT, "SIGINT");
}

/// A second signal while a transaction holds the shutdown in its grace
/// stops the process at once, with 128 + the signal's number.
fn second_signal(signal: libc::c_int, exit: i32) {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Serving::start(dir.path(), &["--shutdown-grace", "60"]);
    let mut a = server.connect();
    ok(&mut a, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    ok(&mut a, "INSERT INTO t VALUES (1)");
    let mut b = server.connect();
    ok(&mut b, "BEGIN");
    ok(&mut b, "INSERT INTO t VALUES (2)");
    server.signal(signal);
    server.wait_for_log("shutting down:");
    server.signal(signal);
    let status = server.wait();
    assert_eq!(status.code(), Some(exit), "{status:?}\n{}", server.log());
    assert!(server.log().contains("again; stopping at once"));

    // The answered commit is there, the open transaction is not.
    let restarted = Serving::start(dir.path(), &[]);
    assert_eq!(ids(&mut restarted.connect()), BTreeSet::from([1]));
}

#[test]
fn a_second_sigterm_stops_at_once_with_143() {
    second_signal(libc::SIGTERM, 143);
}

#[test]
fn a_second_sigint_stops_at_once_with_130() {
    second_signal(libc::SIGINT, 130);
}

/// What became of one request under load.
#[derive(Default)]
struct Tally {
    /// Ids an answered commit wrote.
    committed: Vec<i64>,
    /// Ids whose request, or whose transaction's, was answered `0x49`.
    refused: Vec<i64>,
    /// Requests cut off with no answer at all: the promise is none.
    cut: usize,
    /// Any other error, which this load should never get.
    other: Vec<String>,
}

/// Each client writes ids of its own, as statements on their own and as
/// short transactions of two, until the server closes it.
fn load(port: u16, client: i64, tally: &Mutex<Tally>) {
    let Ok(mut c) = Client::connect(("127.0.0.1", port)) else {
        return;
    };
    c.set_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut next = client * 1_000_000;
    let fate = |result: Result<Output, ClientError>, ids: &[i64], tally: &Mutex<Tally>| {
        let mut t = tally.lock().unwrap();
        match result {
            Ok(_) => true,
            Err(ClientError::Server {
                code: code::SHUTTING_DOWN,
                ..
            }) => {
                t.refused.extend_from_slice(ids);
                false
            }
            Err(ClientError::Io(_) | ClientError::Closed) => {
                t.cut += 1;
                false
            }
            Err(e) => {
                t.other.push(e.to_string());
                false
            }
        }
    };
    loop {
        next += 1;
        if next % 3 == 0 {
            let ids = [next, next + 1];
            next += 1;
            if !fate(c.query("BEGIN"), &ids, tally) {
                return;
            }
            for id in ids {
                if !fate(
                    c.query(&format!("INSERT INTO t VALUES ({id})")),
                    &ids,
                    tally,
                ) {
                    return;
                }
            }
            if !fate(c.query("COMMIT"), &ids, tally) {
                return;
            }
            tally.lock().unwrap().committed.extend_from_slice(&ids);
        } else {
            if !fate(
                c.query(&format!("INSERT INTO t VALUES ({next})")),
                &[next],
                tally,
            ) {
                return;
            }
            tally.lock().unwrap().committed.push(next);
        }
    }
}

/// Design 0028 Q11's invariant: 8 clients write without pause; SIGTERM
/// comes at a random moment. Every answered commit survives, nothing
/// answered `0x49` was written, and no request is cut off unanswered. The
/// exit is always 0, which also shows the commit thread, busy the whole
/// time, never took the signal (the mask is set before it is made).
#[test]
fn under_load_sigterm_answers_every_request_and_loses_no_commit() {
    for round in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Serving::start(dir.path(), &["--shutdown-grace", "2"]);
        ok(
            &mut server.connect(),
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        );
        let tally = Arc::new(Mutex::new(Tally::default()));
        let clients: Vec<_> = (1..=8)
            .map(|client| {
                let tally = Arc::clone(&tally);
                let port = server.port;
                thread::spawn(move || load(port, client, &tally))
            })
            .collect();
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        thread::sleep(Duration::from_millis(50 + u64::from(nanos % 250)));
        server.signal(libc::SIGTERM);
        let status = server.wait();
        for c in clients {
            c.join().unwrap();
        }
        let log = server.log();
        assert_eq!(
            status.code(),
            Some(0),
            "round {round}: {status:?} {:?}\n{log}",
            status.signal()
        );
        assert_eq!(wal_len(dir.path()), EMPTY_WAL, "round {round}");

        let restarted = Serving::start(dir.path(), &[]);
        let present = ids(&mut restarted.connect());
        let t = tally.lock().unwrap();
        assert!(t.other.is_empty(), "round {round}: {:?}", t.other);
        assert_eq!(
            t.cut, 0,
            "round {round}: requests cut off without an answer"
        );
        assert!(!t.committed.is_empty(), "round {round}: the load ran");
        for id in &t.committed {
            assert!(
                present.contains(id),
                "round {round}: answered commit {id} lost"
            );
        }
        for id in &t.refused {
            assert!(
                !present.contains(id),
                "round {round}: {id} answered 0x49 but written"
            );
        }
    }
}
