//! Transactions over the network (issue #37, design 0025): every
//! connection keeps a session of its own, so clients run BEGIN ... COMMIT
//! at the same time, see none of each other's uncommitted writes, and
//! lose an open transaction whichever way their connection ends.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::{
    code, command, read_frame, write_frame, Frame, Request, Response, VERSION,
};
use oakdb_server::rlimit::raise_fd_limit;
use oakdb_server::server::{Config, Monitor, Server};
use oakdb_sql::{Output, Value};

/// How long a test waits on the server before failing instead of hanging.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The idle limit the timeout tests use, short so they run fast.
const SHORT: Duration = Duration::from_millis(200);

fn config() -> Config {
    Config::default().with(|c| c.port = 0)
}

/// A server on a fresh in-memory database holding
/// `t (id INTEGER PRIMARY KEY, n INTEGER)`, and a way to read its numbers.
fn start(config: Config) -> (SocketAddr, Monitor<SimFs>) {
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    let server = Server::bind(db, config).unwrap();
    let addr = server.local_addr();
    let monitor = server.monitor();
    thread::spawn(move || server.run());
    connect(addr)
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    (addr, monitor)
}

fn connect(addr: SocketAddr) -> Client {
    let client = Client::connect(addr).unwrap();
    client.set_timeout(Some(TIMEOUT)).unwrap();
    client
}

fn ok(client: &mut Client, sql: &str) -> Output {
    client.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn server_error(result: Result<Output, ClientError>) -> u8 {
    match result {
        Err(ClientError::Server { code, .. }) => code,
        other => panic!("expected an error from the server, got {other:?}"),
    }
}

/// `t`'s rows as (id, n), read in a statement of its own.
fn rows(client: &mut Client) -> Vec<(i64, i64)> {
    match ok(client, "SELECT id, n FROM t") {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Integer(id), Value::Integer(n)) => (*id, *n),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// Waits until `done` holds, failing the test past [`TIMEOUT`]: the
/// server sees a connection end a moment after the client does.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A socket that has said HELLO and read READY, for sending raw frames.
fn raw(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write_frame(&mut stream, &Request::Hello { version: VERSION }.encode()).unwrap();
    let ready = read_frame(&mut stream).unwrap().unwrap();
    assert_eq!(
        Response::decode(ready).unwrap(),
        Response::Ready { version: VERSION }
    );
    stream
}

fn send_raw(stream: &mut TcpStream, sql: &str) -> Response {
    write_frame(stream, &Request::Query(sql.to_string()).encode()).unwrap();
    Response::decode(read_frame(stream).unwrap().expect("a response")).unwrap()
}

// --- Two connections, one step at a time (Q10 ①–④) ---

#[test]
fn an_uncommitted_insert_is_seen_by_its_own_connection_only_until_commit() {
    let (addr, _) = start(config());
    let mut a = connect(addr);
    let mut b = connect(addr);
    assert_eq!(ok(&mut a, "BEGIN"), Output::Done);
    assert!(a.in_transaction());
    assert_eq!(
        ok(&mut a, "INSERT INTO t VALUES (1, 10)"),
        Output::Affected(1)
    );
    assert_eq!(rows(&mut a), vec![(1, 10)]);
    assert_eq!(rows(&mut b), vec![]);
    assert_eq!(ok(&mut a, "COMMIT"), Output::Done);
    assert!(!a.in_transaction());
    assert_eq!(rows(&mut b), vec![(1, 10)]);
}

#[test]
fn a_transaction_does_not_see_what_another_connection_commits_after_its_begin() {
    let (addr, _) = start(config());
    let mut a = connect(addr);
    let mut b = connect(addr);
    ok(&mut a, "BEGIN");
    ok(&mut b, "INSERT INTO t VALUES (2, 20)");
    assert_eq!(rows(&mut a), vec![]);
    ok(&mut a, "ROLLBACK");
    assert_eq!(rows(&mut a), vec![(2, 20)]);
}

#[test]
fn of_two_connections_updating_one_row_the_later_commit_gets_0x20_and_ends_its_transaction() {
    let (addr, monitor) = start(config());
    let mut a = connect(addr);
    let mut b = connect(addr);
    ok(&mut a, "INSERT INTO t VALUES (1, 0)");
    ok(&mut a, "BEGIN");
    ok(&mut b, "BEGIN");
    ok(&mut a, "UPDATE t SET n = 1 WHERE id = 1");
    ok(&mut b, "UPDATE t SET n = 2 WHERE id = 1");
    assert_eq!(ok(&mut b, "COMMIT"), Output::Done);
    match a.query("COMMIT") {
        Err(ClientError::Server { code, message }) => {
            assert_eq!(code, code::CONFLICT);
            assert!(message.contains("BEGIN"), "{message}");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    // The transaction is over on both ends: the next statement commits on
    // its own.
    assert!(!a.in_transaction());
    assert_eq!(rows(&mut a), vec![(1, 2)]);
    assert_eq!(monitor.stats().open_transactions, 0);
    assert_eq!(server_error(a.query("ROLLBACK")), code::TRANSACTION_STATE);
}

#[test]
fn a_command_out_of_place_gets_0x0b_and_leaves_the_open_transaction_as_it_was() {
    let (addr, _) = start(config());
    let mut a = connect(addr);
    let mut b = connect(addr);
    assert_eq!(server_error(a.query("COMMIT")), code::TRANSACTION_STATE);
    assert_eq!(server_error(a.query("ROLLBACK")), code::TRANSACTION_STATE);
    assert!(!a.in_transaction());
    ok(&mut a, "BEGIN");
    ok(&mut a, "INSERT INTO t VALUES (1, 1)");
    match a.query("BEGIN") {
        Err(ClientError::Server { code, message }) => {
            assert_eq!(code, code::TRANSACTION_STATE);
            assert!(message.contains("already"), "{message}");
        }
        other => panic!("expected 0x0b, got {other:?}"),
    }
    // Still open, still holding the insert, still unseen by others.
    assert!(a.in_transaction());
    assert_eq!(rows(&mut a), vec![(1, 1)]);
    assert_eq!(rows(&mut b), vec![]);
    ok(&mut a, "COMMIT");
    assert_eq!(rows(&mut b), vec![(1, 1)]);
}

#[test]
fn a_failed_statement_inside_a_transaction_keeps_it_open() {
    let (addr, _) = start(config());
    let mut a = connect(addr);
    ok(&mut a, "BEGIN");
    ok(&mut a, "INSERT INTO t VALUES (1, 1)");
    assert_eq!(
        server_error(a.query("INSERT INTO t VALUES (1, 2)")),
        code::DUPLICATE_KEY
    );
    assert!(a.in_transaction());
    // A COMMIT the parser refuses is not a COMMIT: still open.
    assert_eq!(server_error(a.query("COMMIT now")), code::UNSUPPORTED);
    assert!(a.in_transaction());
    ok(&mut a, "COMMIT");
    assert_eq!(rows(&mut a), vec![(1, 1)]);
}

// --- Every way a connection ends rolls its transaction back (Q1) ---

/// Opens a transaction on a raw socket that inserts row 7, then lets
/// `end` finish the connection, and checks the row never shows, the
/// engine has no transaction open, and the seat is free again.
fn ends_in_rollback(config: Config, end: impl FnOnce(TcpStream)) {
    let (addr, monitor) = start(config.with(|c| c.max_connections = 2));
    let mut stream = raw(addr);
    assert_eq!(send_raw(&mut stream, "BEGIN"), Response::Done);
    assert_eq!(
        send_raw(&mut stream, "INSERT INTO t VALUES (7, 7)"),
        Response::Affected(1)
    );
    assert_eq!(monitor.stats().open_transactions, 1);
    end(stream);
    eventually("the transaction ended", || {
        monitor.stats().open_transactions == 0
    });
    eventually("the connection's thread ended", || {
        monitor.connections() == 0
    });
    // Both seats are free: this one and one more.
    let mut first = connect(addr);
    let _second = connect(addr);
    assert_eq!(rows(&mut first), vec![]);
}

#[test]
fn closing_the_socket_rolls_the_transaction_back() {
    ends_in_rollback(config(), drop);
}

#[test]
fn ending_mid_frame_rolls_the_transaction_back() {
    ends_in_rollback(config(), |mut stream| {
        stream.write_all(&[0, 0, 0, 9, command::QUERY]).unwrap();
        drop(stream);
    });
}

#[test]
fn a_malformed_frame_closes_the_connection_and_rolls_the_transaction_back() {
    ends_in_rollback(config(), |mut stream| {
        write_frame(
            &mut stream,
            &Frame {
                command: 0x7e,
                payload: vec![],
            },
        )
        .unwrap();
        let response = Response::decode(read_frame(&mut stream).unwrap().unwrap()).unwrap();
        assert!(
            matches!(
                response,
                Response::Error {
                    code: code::MALFORMED,
                    ..
                }
            ),
            "{response:?}"
        );
    });
}

#[test]
fn idling_in_a_transaction_gets_0x47_then_the_close_and_a_rollback() {
    let config = config().with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    ends_in_rollback(config, |mut stream| {
        let started = Instant::now();
        let frame = read_frame(&mut stream).unwrap().expect("an ERROR first");
        match Response::decode(frame).unwrap() {
            Response::Error { code, message } => {
                assert_eq!(code, code::IDLE_IN_TRANSACTION);
                assert!(message.contains("rolled back"), "{message}");
            }
            other => panic!("expected 0x47, got {other:?}"),
        }
        assert!(started.elapsed() < TIMEOUT);
        assert!(matches!(read_frame(&mut stream), Ok(None) | Err(_)));
    });
}

#[test]
fn stopping_halfway_through_a_frame_in_a_transaction_also_times_out() {
    let config = config().with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    ends_in_rollback(config, |mut stream| {
        // Half a frame, then nothing: the client is stuck, not gone.
        stream.write_all(&[0, 0, 0, 9, command::QUERY]).unwrap();
        let frame = read_frame(&mut stream).unwrap().expect("an ERROR first");
        let response = Response::decode(frame).unwrap();
        assert!(
            matches!(
                response,
                Response::Error {
                    code: code::IDLE_IN_TRANSACTION,
                    ..
                }
            ),
            "{response:?}"
        );
    });
}

#[test]
fn a_client_that_stops_reading_a_large_result_in_a_transaction_is_cut_off() {
    // The write timeout (Q5): a SELECT whose rows the client never reads
    // fills the socket buffers, and the server gives up on the write
    // instead of holding the transaction forever.
    let config = config().with(|c| c.idle_in_transaction_timeout = Some(SHORT));
    ends_in_rollback(config, |mut stream| {
        // Half a MiB per row, in a table of their own.
        let blob = "ab".repeat(512 * 1024);
        assert_eq!(
            send_raw(
                &mut stream,
                "CREATE TABLE big (id INTEGER PRIMARY KEY, b BLOB)"
            ),
            Response::Done
        );
        for id in 0..24 {
            let sql = format!("INSERT INTO big VALUES ({id}, X'{blob}')");
            assert_eq!(send_raw(&mut stream, &sql), Response::Affected(1));
        }
        // Twelve MiB of rows, more than both socket buffers hold, and the
        // client reads none of them.
        write_frame(
            &mut stream,
            &Request::Query("SELECT b FROM big".into()).encode(),
        )
        .unwrap();
        // Held open, unread, for longer than the test waits: only the
        // server giving up on the write ends the transaction in time.
        thread::spawn(move || {
            thread::sleep(TIMEOUT * 2);
            drop(stream);
        });
    });
}

#[test]
fn outside_a_transaction_an_idle_connection_stays_unless_told_otherwise() {
    let (addr, _) = start(config().with(|c| c.idle_in_transaction_timeout = Some(SHORT)));
    let mut a = connect(addr);
    thread::sleep(SHORT * 3);
    assert_eq!(rows(&mut a), vec![]);

    let (addr, monitor) = start(config().with(|c| c.idle_timeout = Some(SHORT)));
    let mut stream = raw(addr);
    let frame = read_frame(&mut stream).unwrap().expect("an ERROR first");
    let response = Response::decode(frame).unwrap();
    assert!(
        matches!(
            response,
            Response::Error {
                code: code::IDLE,
                ..
            }
        ),
        "{response:?}"
    );
    eventually("the connection's thread ended", || {
        monitor.connections() == 0
    });
}

#[test]
fn the_default_idle_limit_in_a_transaction_is_a_minute_and_none_outside() {
    let config = Config::default();
    assert_eq!(
        config.idle_in_transaction_timeout,
        Some(Duration::from_secs(60))
    );
    assert_eq!(config.idle_timeout, None);
}

#[test]
fn a_transaction_that_keeps_working_is_not_idle() {
    // The limit is on the wait for the next statement, not on the age of
    // the transaction.
    let (addr, _) = start(config().with(|c| c.idle_in_transaction_timeout = Some(SHORT)));
    let mut a = connect(addr);
    ok(&mut a, "BEGIN");
    for i in 0..8 {
        thread::sleep(SHORT / 4);
        ok(&mut a, &format!("INSERT INTO t VALUES ({i}, {i})"));
    }
    ok(&mut a, "COMMIT");
    assert_eq!(rows(&mut a).len(), 8);
}

// --- Many connections at once (Q10) ---

#[test]
fn eight_connections_incrementing_one_counter_lose_no_update() {
    const CONNECTIONS: i64 = 8;
    const EACH: i64 = 25;
    let (addr, monitor) = start(config());
    ok(&mut connect(addr), "INSERT INTO t VALUES (0, 0)");
    let workers: Vec<_> = (0..CONNECTIONS)
        .map(|_| {
            thread::spawn(move || {
                let mut client = connect(addr);
                let mut retries = 0;
                for _ in 0..EACH {
                    loop {
                        ok(&mut client, "BEGIN");
                        let n = match ok(&mut client, "SELECT n FROM t WHERE id = 0") {
                            Output::Rows { rows, .. } => match rows[0][0] {
                                Value::Integer(n) => n,
                                ref other => panic!("{other:?}"),
                            },
                            other => panic!("{other:?}"),
                        };
                        ok(
                            &mut client,
                            &format!("UPDATE t SET n = {} WHERE id = 0", n + 1),
                        );
                        match client.query("COMMIT") {
                            Ok(_) => break,
                            Err(ClientError::Server {
                                code: code::CONFLICT,
                                ..
                            }) => {
                                // From BEGIN again.
                                assert!(!client.in_transaction());
                                retries += 1;
                            }
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
                retries
            })
        })
        .collect();
    let retries: u64 = workers.into_iter().map(|w| w.join().unwrap()).sum();
    assert_eq!(
        rows(&mut connect(addr)),
        vec![(0, CONNECTIONS * EACH)],
        "after {retries} retries"
    );
    let stats = monitor.stats();
    assert_eq!(stats.open_transactions, 0);
    assert_eq!(stats.conflicts, retries);
}

#[test]
fn three_hundred_connections_hold_transactions_open_at_once_and_all_commit() {
    // Server and clients share this process: 300 sockets on each side.
    raise_fd_limit(2048).unwrap();
    const CLIENTS: usize = 300;
    let (addr, monitor) = start(config());
    let clients: Vec<Client> = (0..CLIENTS).map(|_| connect(addr)).collect();
    let all_open = Arc::new(Barrier::new(CLIENTS + 1));
    let commit = Arc::new(Barrier::new(CLIENTS + 1));
    let workers: Vec<_> = clients
        .into_iter()
        .enumerate()
        .map(|(i, mut client)| {
            let all_open = Arc::clone(&all_open);
            let commit = Arc::clone(&commit);
            // The test's own threads: small stacks, like the server's.
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    ok(&mut client, "BEGIN");
                    ok(&mut client, &format!("INSERT INTO t VALUES ({i}, {i})"));
                    all_open.wait();
                    commit.wait();
                    assert_eq!(ok(&mut client, "COMMIT"), Output::Done);
                })
                .unwrap()
        })
        .collect();
    all_open.wait();
    // Every transaction is open and none has committed.
    assert_eq!(monitor.stats().open_transactions, CLIENTS as u64);
    assert_eq!(rows(&mut connect(addr)), vec![]);
    commit.wait();
    for w in workers {
        w.join().unwrap();
    }
    let found = rows(&mut connect(addr));
    assert_eq!(found.len(), CLIENTS);
    assert_eq!(monitor.stats().open_transactions, 0);
}
