//! Graceful shutdown in this process (issue #38, design 0028 Q11): the
//! handle is pulled by the test, not by a signal, so each scene is
//! deterministic. `signal.rs` sends the real signals to the binary.

use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, WAL_FILE};
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::{code, read_frame, write_frame, Request, Response, VERSION};
use oakdb_server::server::{
    Config, Monitor, Server, Shutdown, ShutdownHandle, SHUTDOWN_MARGIN, SHUTTING_DOWN_MESSAGE,
};
use oakdb_sql::{Output, Value};

/// How long a test waits on the server before failing instead of hanging.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A grace short enough for tests that wait it out.
const SHORT: Duration = Duration::from_millis(200);

/// A server on an in-memory disk, running on a thread of its own.
struct Running {
    addr: SocketAddr,
    handle: ShutdownHandle,
    monitor: Monitor<SimFs>,
    run: JoinHandle<Shutdown>,
    fs: SimFs,
}

impl Running {
    /// Pulls the handle and waits for `run` to return.
    fn shut_down(self) -> (Shutdown, SimFs) {
        self.handle.shutdown();
        (self.run.join().unwrap(), self.fs)
    }
}

fn config(grace: Duration) -> Config {
    Config::default().with(|c| {
        c.port = 0;
        c.shutdown_grace = grace;
    })
}

/// A server on a fresh disk holding `t (id INTEGER PRIMARY KEY)`.
fn start(config: Config) -> Running {
    start_on(SimFs::new(), config)
}

fn start_on(fs: SimFs, config: Config) -> Running {
    let db = Db::open_with(fs.clone(), "/oak").unwrap();
    oakdb_sql::execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    let server = Server::bind(db, config).unwrap();
    let addr = server.local_addr();
    let handle = server.shutdown_handle();
    let monitor = server.monitor();
    let run = thread::spawn(move || server.run());
    Running {
        addr,
        handle,
        monitor,
        run,
        fs,
    }
}

fn connect(addr: SocketAddr) -> Client {
    let client = Client::connect(addr).unwrap();
    client.set_timeout(Some(TIMEOUT)).unwrap();
    client
}

fn ok(client: &mut Client, sql: &str) -> Output {
    client.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// A socket that has said HELLO and read READY.
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

fn send(stream: &mut TcpStream, sql: &str) -> Response {
    write_frame(stream, &Request::Query(sql.to_string()).encode()).unwrap();
    next(stream)
}

fn next(stream: &mut TcpStream) -> Response {
    Response::decode(read_frame(stream).unwrap().expect("a frame")).unwrap()
}

/// Reads `0x49`, sent without a request, and then the end of the
/// connection: no reset.
fn expect_shutting_down(stream: &mut TcpStream) {
    assert_eq!(
        next(stream),
        Response::Error {
            code: code::SHUTTING_DOWN,
            message: SHUTTING_DOWN_MESSAGE.into()
        }
    );
    assert!(matches!(read_frame(stream), Ok(None)), "then the close");
}

fn shutting_down(result: Result<Output, ClientError>) {
    match result {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, code::SHUTTING_DOWN),
        other => panic!("expected ERROR 0x49, got {other:?}"),
    }
}

/// `t`'s ids, read from the disk the server left, opened again.
fn ids_after(fs: &SimFs) -> Vec<i64> {
    let db = Db::open_with(fs.clone(), "/oak").unwrap();
    match oakdb_sql::execute(&db, "SELECT id FROM t").unwrap() {
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

/// The WAL's length: its header alone (20 bytes) after a checkpoint.
fn wal_len(fs: &SimFs) -> u64 {
    fs.open(&Path::new("/oak").join(WAL_FILE))
        .unwrap()
        .size()
        .unwrap()
}

/// On macOS loopback finishes the handshake asynchronously: `connect`
/// can return before the server's side is in the accept queue, and on a
/// busy machine the sweep then finds nobody, so closing the listener
/// resets the client (a connection after the door closed). A moment lets
/// the clients settle in the queue first.
fn settle_in_the_queue() {
    thread::sleep(Duration::from_millis(100));
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

// --- Q11 ①, ②, ⑤: an idle connection, the door, the report ---

#[test]
fn an_idle_connection_gets_0x49_unasked_and_the_server_checkpoints_and_closes() {
    let server = start(config(SHORT));
    let mut a = connect(server.addr);
    ok(&mut a, "INSERT INTO t VALUES (1)");
    let mut idle = raw(server.addr);
    let monitor = server.monitor.clone();
    eventually("both seated", || monitor.connections() == 2);

    let (report, fs) = server.shut_down();
    expect_shutting_down(&mut idle);
    shutting_down(a.query("SELECT id FROM t"));
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.connections, 2);
    assert_eq!(report.closed, 2);
    assert_eq!(report.rolled_back, 0);
    assert_eq!(report.busy, 0);
    assert_eq!(wal_len(&fs), 20, "the final checkpoint emptied the WAL");
    assert_eq!(ids_after(&fs), vec![1]);
}

#[test]
fn after_the_shutdown_new_connections_are_refused() {
    let server = start(config(SHORT));
    let addr = server.addr;
    let (report, _) = server.shut_down();
    assert!(report.is_clean());
    let refused = TcpStream::connect(addr).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
}

#[test]
fn a_monitor_reads_zeros_once_the_server_is_gone_and_does_not_keep_it_open() {
    let server = start(config(SHORT));
    let monitor = server.monitor.clone();
    let (report, _) = server.shut_down();
    assert!(report.is_clean(), "a monitor held does not block the close");
    assert_eq!(monitor.connections(), 0);
    assert_eq!(monitor.stats().commits, 0);
}

// --- Q11 ⑥: before HELLO ---

#[test]
fn a_connection_that_has_not_said_hello_gets_0x49() {
    let server = start(config(SHORT));
    let mut silent = TcpStream::connect(server.addr).unwrap();
    silent.set_read_timeout(Some(TIMEOUT)).unwrap();
    let monitor = server.monitor.clone();
    eventually("seated", || monitor.connections() == 1);
    let (report, _) = server.shut_down();
    expect_shutting_down(&mut silent);
    assert!(report.is_clean());
}

#[test]
fn clients_waiting_at_the_door_get_0x49_not_a_reset() {
    // Nobody accepts before `run`, so these wait in the system's queue;
    // pulled first, `run` shuts down at once and turns them away.
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), "/oak").unwrap();
    let server = Server::bind(db, config(SHORT)).unwrap();
    let mut waiting: Vec<TcpStream> = (0..5)
        .map(|_| {
            let s = TcpStream::connect(server.local_addr()).unwrap();
            s.set_read_timeout(Some(TIMEOUT)).unwrap();
            s
        })
        .collect();
    settle_in_the_queue();
    let handle = server.shutdown_handle();
    handle.shutdown();
    let report = server.run();
    assert!(report.is_clean(), "{report:?}");
    for stream in &mut waiting {
        expect_shutting_down(stream);
    }
}

#[test]
fn a_handle_made_before_bind_and_pulled_early_shuts_the_server_down_at_once() {
    let handle = ShutdownHandle::new();
    handle.shutdown();
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    let server = Server::bind_with(db, config(SHORT), handle.clone()).unwrap();
    let report = server.run();
    assert!(report.is_clean());
    assert_eq!(report.connections, 0);
}

// --- Q11 ③, ④: transactions and the grace ---

#[test]
fn a_transaction_finished_within_the_grace_commits_and_then_gets_0x49() {
    let server = start(config(TIMEOUT));
    let mut a = raw(server.addr);
    assert_eq!(send(&mut a, "BEGIN"), Response::Done);
    assert_eq!(
        send(&mut a, "INSERT INTO t VALUES (1)"),
        Response::Affected(1)
    );
    let handle = server.handle.clone();
    handle.shutdown();
    // Inside the grace the connection goes on.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        send(&mut a, "INSERT INTO t VALUES (2)"),
        Response::Affected(1)
    );
    assert_eq!(send(&mut a, "COMMIT"), Response::Done);
    expect_shutting_down(&mut a);
    let (report, fs) = server.shut_down();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.in_transaction, 1);
    assert_eq!(report.rolled_back, 0);
    assert_eq!(ids_after(&fs), vec![1, 2]);
}

#[test]
fn a_transaction_past_the_grace_is_rolled_back_and_gets_0x49() {
    let server = start(config(SHORT));
    let mut a = raw(server.addr);
    assert_eq!(send(&mut a, "BEGIN"), Response::Done);
    assert_eq!(
        send(&mut a, "INSERT INTO t VALUES (1)"),
        Response::Affected(1)
    );
    let started = Instant::now();
    server.handle.shutdown();
    expect_shutting_down(&mut a);
    assert!(started.elapsed() >= SHORT, "not before the grace");
    let (report, fs) = server.shut_down();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.rolled_back, 1);
    assert_eq!(report.closed, 1);
    assert_eq!(ids_after(&fs), Vec::<i64>::new());
}

#[test]
fn a_grace_of_zero_rolls_transactions_back_at_once() {
    let server = start(config(Duration::ZERO));
    let mut a = raw(server.addr);
    assert_eq!(send(&mut a, "BEGIN"), Response::Done);
    assert_eq!(
        send(&mut a, "INSERT INTO t VALUES (1)"),
        Response::Affected(1)
    );
    let (report, fs) = server.shut_down();
    expect_shutting_down(&mut a);
    assert_eq!(report.rolled_back, 1);
    assert!(report.took < SHUTDOWN_MARGIN);
    assert_eq!(ids_after(&fs), Vec::<i64>::new());
}

#[test]
fn a_statement_sent_after_the_last_answer_of_a_closing_connection_is_not_run() {
    let server = start(config(SHORT));
    let mut a = connect(server.addr);
    ok(&mut a, "INSERT INTO t VALUES (1)");
    let monitor = server.monitor.clone();
    server.handle.shutdown();
    // A statement that reaches the connection before it sees the flag is
    // run and answered, as any statement under way. One sent once it has
    // closed is answered by the 0x49 waiting in the socket, and not run.
    eventually("the connection closed", || monitor.connections() == 0);
    shutting_down(a.query("INSERT INTO t VALUES (2)"));
    let (_, fs) = server.shut_down();
    assert_eq!(ids_after(&fs), vec![1]);
}

#[test]
fn the_client_knows_no_transaction_is_open_after_0x49() {
    let server = start(config(Duration::ZERO));
    let mut a = connect(server.addr);
    ok(&mut a, "BEGIN");
    assert!(a.in_transaction());
    let monitor = server.monitor.clone();
    server.handle.shutdown();
    eventually("the connection closed", || monitor.connections() == 0);
    shutting_down(a.query("COMMIT"));
    assert!(!a.in_transaction());
    let (_, fs) = server.shut_down();
    assert_eq!(ids_after(&fs), Vec::<i64>::new());
}

// --- A commit under way (Q11, SimFs::hold_syncs) ---

#[test]
fn a_commit_under_way_finishes_and_is_answered_before_the_0x49() {
    let server = start(config(SHORT));
    let mut a = raw(server.addr);
    let held = server.fs.hold_syncs();
    let committing = thread::spawn(move || {
        let answer = send(&mut a, "INSERT INTO t VALUES (7)");
        (answer, a)
    });
    held.wait_for_sync();
    server.handle.shutdown();
    // Past the grace the commit is still held, and the server waits on.
    thread::sleep(SHORT * 2);
    assert!(!server.run.is_finished());
    held.release();
    let (answer, mut a) = committing.join().unwrap();
    assert_eq!(answer, Response::Affected(1));
    expect_shutting_down(&mut a);
    let (report, fs) = server.shut_down();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(ids_after(&fs), vec![7]);
}

#[test]
fn a_commit_stuck_past_the_grace_and_margin_ends_the_shutdown_without_a_checkpoint() {
    let server = start(config(SHORT));
    let mut a = raw(server.addr);
    let held = server.fs.hold_syncs();
    let committing = thread::spawn(move || {
        let answer = send(&mut a, "INSERT INTO t VALUES (7)");
        (answer, a)
    });
    held.wait_for_sync();
    server.handle.shutdown();
    let report = server.run.join().unwrap();
    assert_eq!(report.busy, 1);
    assert!(report.checkpoint.is_none());
    assert!(!report.is_clean());
    assert!(report.took >= SHORT + SHUTDOWN_MARGIN);
    // Let it go, so the test ends cleanly: the commit is answered, then
    // the connection gets its 0x49.
    held.release();
    let (answer, mut a) = committing.join().unwrap();
    assert_eq!(answer, Response::Affected(1));
    expect_shutting_down(&mut a);
}

// --- A stopped engine (Q11, design 0028 Q7) ---

#[test]
fn on_a_stopped_engine_the_final_checkpoint_fails_and_answered_commits_survive() {
    let server = start(config(SHORT));
    let mut a = connect(server.addr);
    ok(&mut a, "INSERT INTO t VALUES (1)");
    // One sync fails; the engine stops, the disk works on.
    server.fs.fail_once_after(0);
    let failed = a.query("INSERT INTO t VALUES (2)");
    assert!(
        matches!(failed, Err(ClientError::Server { code, .. }) if code == code::OUTCOME_UNKNOWN),
        "{failed:?}"
    );
    let (report, fs) = server.shut_down();
    assert!(matches!(report.checkpoint, Some(Err(_))), "{report:?}");
    assert!(!report.is_clean());
    let ids = ids_after(&fs);
    assert!(ids.contains(&1), "{ids:?}");
}

// --- Review fixes (#84 review D1, D2) ---

/// The first byte of a QUERY frame's four-byte length: a client that sends
/// it and stops sits halfway through a frame.
fn half_a_frame(stream: &mut TcpStream) {
    stream.write_all(&[0]).unwrap();
    // Let the server see the byte and start reading the rest.
    thread::sleep(Duration::from_millis(50));
}

/// A shutdown that waited on nobody: every connection gone, checkpointed,
/// well within the margin.
fn assert_prompt_and_clean(report: &Shutdown) {
    assert!(
        report.is_clean() && report.took < SHUTDOWN_MARGIN,
        "took {:?}, busy {}, clean {}",
        report.took,
        report.busy,
        report.is_clean()
    );
}

#[test]
fn a_client_stalled_halfway_through_a_frame_gets_0x49_and_does_not_hold_up_the_shutdown() {
    // Review S1: the rest of a frame was read with no limit outside a
    // transaction, so one such client made the shutdown wait out the
    // grace and the margin and end without the checkpoint.
    let server = start(config(SHORT));
    let mut a = raw(server.addr);
    half_a_frame(&mut a);
    server.handle.shutdown();
    let report = server.run.join().unwrap();
    assert_prompt_and_clean(&report);
    assert_eq!(report.closed, 1);
    expect_shutting_down(&mut a);
}

#[test]
fn a_client_stalled_halfway_through_its_hello_gets_0x49_and_does_not_hold_up_the_shutdown() {
    let server = start(config(SHORT));
    let mut a = TcpStream::connect(server.addr).unwrap();
    a.set_read_timeout(Some(TIMEOUT)).unwrap();
    half_a_frame(&mut a);
    server.handle.shutdown();
    let report = server.run.join().unwrap();
    assert_prompt_and_clean(&report);
    expect_shutting_down(&mut a);
}

#[test]
fn a_transaction_stalled_halfway_through_a_frame_is_rolled_back_after_the_grace() {
    let server = start(config(SHORT));
    let mut a = raw(server.addr);
    assert_eq!(send(&mut a, "BEGIN"), Response::Done);
    assert_eq!(
        send(&mut a, "INSERT INTO t VALUES (1)"),
        Response::Affected(1)
    );
    half_a_frame(&mut a);
    let started = Instant::now();
    server.handle.shutdown();
    let report = server.run.join().unwrap();
    assert!(started.elapsed() >= SHORT, "not before the grace");
    assert_prompt_and_clean(&report);
    assert_eq!(report.rolled_back, 1);
    expect_shutting_down(&mut a);
    assert_eq!(ids_after(&server.fs), Vec::<i64>::new());
}

#[test]
fn a_frame_finished_within_the_grace_inside_a_transaction_is_run() {
    let server = start(config(TIMEOUT));
    let mut a = raw(server.addr);
    assert_eq!(send(&mut a, "BEGIN"), Response::Done);
    let frame = Request::Query("INSERT INTO t VALUES (1)".into()).encode();
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &frame).unwrap();
    a.write_all(&bytes[..1]).unwrap();
    thread::sleep(Duration::from_millis(50));
    server.handle.shutdown();
    // Well past a tick, inside the grace: the frame is still waited for.
    thread::sleep(Duration::from_millis(300));
    a.write_all(&bytes[1..]).unwrap();
    assert_eq!(next(&mut a), Response::Affected(1));
    assert_eq!(send(&mut a, "COMMIT"), Response::Done);
    expect_shutting_down(&mut a);
    let (report, fs) = server.shut_down();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(ids_after(&fs), vec![1]);
}

#[test]
fn the_moment_0x49_is_read_the_transaction_is_already_rolled_back() {
    // Review S3, mutation M14: 0x49 sent first and the rollback after the
    // linger would leave the transaction open while the client reads it.
    let server = start(config(SHORT));
    let mut a = raw(server.addr);
    assert_eq!(send(&mut a, "BEGIN"), Response::Done);
    assert_eq!(
        send(&mut a, "INSERT INTO t VALUES (1)"),
        Response::Affected(1)
    );
    let monitor = server.monitor.clone();
    assert_eq!(monitor.stats().open_transactions, 1);
    server.handle.shutdown();
    // The socket stays open, so the server is still lingering on it.
    assert_eq!(
        next(&mut a),
        Response::Error {
            code: code::SHUTTING_DOWN,
            message: SHUTTING_DOWN_MESSAGE.into()
        }
    );
    assert_eq!(monitor.stats().open_transactions, 0);
    assert_eq!(monitor.connections(), 1, "still lingering");
    drop(a);
    let (report, _) = server.shut_down();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn a_client_waiting_at_the_door_may_go_on_sending_after_its_0x49_without_a_reset() {
    // Review S8, mutation M6: on macOS a socket accepted from the
    // nonblocking listener is nonblocking too. Lingering on it would not
    // wait, so bytes the client sends after reading its 0x49 reach a
    // closed socket and reset the connection.
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    let server = Server::bind(db, config(SHORT)).unwrap();
    let mut waiting = TcpStream::connect(server.local_addr()).unwrap();
    waiting.set_read_timeout(Some(TIMEOUT)).unwrap();
    write_frame(&mut waiting, &Request::Hello { version: VERSION }.encode()).unwrap();
    settle_in_the_queue();
    let handle = server.shutdown_handle();
    handle.shutdown();
    let run = thread::spawn(move || server.run());
    assert_eq!(
        next(&mut waiting),
        Response::Error {
            code: code::SHUTTING_DOWN,
            message: SHUTTING_DOWN_MESSAGE.into()
        }
    );
    let query = Request::Query("SELECT 1".into()).encode();
    for _ in 0..3 {
        write_frame(&mut waiting, &query).unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    let mut rest = [0u8; 16];
    let end = std::io::Read::read(&mut waiting, &mut rest);
    assert!(
        matches!(end, Ok(0)),
        "a clean close, not {end:?} (a reset is {:?})",
        ErrorKind::ConnectionReset
    );
    assert!(run.join().unwrap().is_clean());
}

#[test]
fn a_hello_finished_after_the_shutdown_started_gets_0x49_not_ready() {
    // Mutation M11: the flag looked at between HELLO and READY. The HELLO
    // began before the shutdown and ends just after it.
    let server = start(config(SHORT));
    let mut a = TcpStream::connect(server.addr).unwrap();
    a.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut hello = Vec::new();
    write_frame(&mut hello, &Request::Hello { version: VERSION }.encode()).unwrap();
    a.write_all(&hello[..1]).unwrap();
    thread::sleep(Duration::from_millis(50));
    server.handle.shutdown();
    a.write_all(&hello[1..]).unwrap();
    expect_shutting_down(&mut a);
    let (report, _) = server.shut_down();
    assert!(report.is_clean(), "{report:?}");
}
