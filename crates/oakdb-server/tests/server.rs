//! The server end to end, in this process (design 0020, section 10): a
//! server bound to port 0 on a real database, clients connecting over
//! loopback TCP and running SQL.

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::Db;
use oakdb_server::allow::Allowlist;
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::{
    code, command, read_frame, write_frame, Frame, Request, Response, MAX_FRAME_LEN, VERSION,
};
use oakdb_server::rlimit::raise_fd_limit;
use oakdb_server::server::{Config, Server};
use oakdb_sql::{execute, Output, Value};

const TIMEOUT: Duration = Duration::from_secs(30);

fn config() -> Config {
    Config {
        port: 0,
        ..Config::default()
    }
}

fn start<F>(db: Db<F>, config: Config) -> SocketAddr
where
    F: Fs + 'static,
    Db<F>: Send + Sync,
{
    let server = Server::bind(db, config).unwrap();
    let addr = server.local_addr();
    thread::spawn(move || server.run());
    addr
}

fn start_on(dir: &Path) -> SocketAddr {
    start(Db::open(dir).unwrap(), config())
}

fn connect(addr: SocketAddr) -> Client {
    let client = Client::connect(addr).unwrap();
    client.set_timeout(Some(TIMEOUT)).unwrap();
    client
}

fn server_error(result: Result<Output, ClientError>) -> u8 {
    match result {
        Err(ClientError::Server { code, .. }) => code,
        other => panic!("expected an error from the server, got {other:?}"),
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

fn read_response(stream: &mut TcpStream) -> Response {
    let frame = read_frame(stream).unwrap().expect("a response");
    Response::decode(frame).unwrap()
}

fn error_code(response: Response) -> u8 {
    match response {
        Response::Error { code, .. } => code,
        other => panic!("expected ERROR, got {other:?}"),
    }
}

/// The server closed the connection: reading finds the end.
fn assert_closed(stream: &mut TcpStream) {
    let mut rest = Vec::new();
    let read = stream.read_to_end(&mut rest);
    assert!(
        matches!(read, Ok(0)) || read.is_err(),
        "the connection is still open: {read:?} {rest:?}"
    );
}

#[test]
fn a_client_creates_inserts_and_selects_over_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = connect(start_on(dir.path()));
    assert_eq!(
        client
            .query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, photo BLOB)")
            .unwrap(),
        Output::Done
    );
    assert_eq!(
        client
            .query("INSERT INTO users VALUES (1, 'kim', X'ab01'), (2, 'lee', NULL)")
            .unwrap(),
        Output::Affected(2)
    );
    assert_eq!(
        client.query("SELECT * FROM users WHERE id = 1;").unwrap(),
        Output::Rows {
            columns: vec!["id".into(), "name".into(), "photo".into()],
            rows: vec![vec![
                Value::Integer(1),
                Value::Text("kim".into()),
                Value::Blob(vec![0xab, 0x01]),
            ]],
        }
    );
    assert_eq!(
        client.query("SELECT name FROM users WHERE id = 3").unwrap(),
        Output::Rows {
            columns: vec!["name".into()],
            rows: vec![],
        }
    );
}

#[test]
fn a_failed_statement_returns_its_kind_and_the_connection_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = connect(start_on(dir.path()));
    client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    client.query("INSERT INTO t VALUES (1, 'a')").unwrap();

    assert_eq!(
        server_error(client.query("INSERT INTO t VALUES (3, 'c'")),
        code::SYNTAX
    );
    assert_eq!(server_error(client.query("TRUNCATE t")), code::UNSUPPORTED);
    assert_eq!(
        server_error(client.query("SELECT * FROM nope WHERE id = 1")),
        code::NO_SUCH_TABLE
    );
    assert_eq!(
        server_error(client.query("SELECT nope FROM t WHERE id = 1")),
        code::NO_SUCH_COLUMN
    );
    assert_eq!(
        server_error(client.query("CREATE TABLE t (id INTEGER PRIMARY KEY)")),
        code::TABLE_EXISTS
    );
    assert_eq!(
        server_error(client.query("INSERT INTO t VALUES (1, 'again')")),
        code::DUPLICATE_KEY
    );
    assert_eq!(
        server_error(client.query("INSERT INTO t VALUES ('x', 'a')")),
        code::TYPE_MISMATCH
    );
    assert_eq!(
        server_error(client.query("INSERT INTO t VALUES (2)")),
        code::VALUE_COUNT
    );

    // Still connected, and the failed statements wrote nothing.
    assert_eq!(
        client.query("SELECT v FROM t WHERE id = 1").unwrap(),
        Output::Rows {
            columns: vec!["v".into()],
            rows: vec![vec![Value::Text("a".into())]],
        }
    );
}

#[test]
fn the_error_message_is_the_sql_layers_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = connect(start_on(dir.path()));
    match client.query("SELECT * FROM nope WHERE id = 1") {
        Err(ClientError::Server { message, .. }) => assert_eq!(message, "no such table: nope"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_first_frame_that_is_not_hello_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_on(dir.path());
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write_frame(&mut stream, &Request::Query("SELECT 1".into()).encode()).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::HANDSHAKE);
    assert_closed(&mut stream);
}

#[test]
fn a_hello_with_another_version_or_magic_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_on(dir.path());

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write_frame(&mut stream, &Request::Hello { version: 99 }.encode()).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::HANDSHAKE);
    assert_closed(&mut stream);

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let frame = Frame {
        command: command::HELLO,
        payload: b"PGSQL\x00\x01".to_vec(),
    };
    write_frame(&mut stream, &frame).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::HANDSHAKE);
    assert_closed(&mut stream);
}

#[test]
fn a_frame_of_length_zero_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut stream = raw(start_on(dir.path()));
    std::io::Write::write_all(&mut stream, &[0, 0, 0, 0]).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::MALFORMED);
    assert_closed(&mut stream);
}

#[test]
fn an_unknown_command_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut stream = raw(start_on(dir.path()));
    let frame = Frame {
        command: 0x7e,
        payload: b"hi".to_vec(),
    };
    write_frame(&mut stream, &frame).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::MALFORMED);
    assert_closed(&mut stream);
}

#[test]
fn a_second_hello_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut stream = raw(start_on(dir.path()));
    write_frame(&mut stream, &Request::Hello { version: VERSION }.encode()).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::MALFORMED);
    assert_closed(&mut stream);
}

#[test]
fn a_length_over_the_limit_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut stream = raw(start_on(dir.path()));
    std::io::Write::write_all(&mut stream, &(MAX_FRAME_LEN + 1).to_be_bytes()).unwrap();
    assert_eq!(
        error_code(read_response(&mut stream)),
        code::FRAME_TOO_LARGE
    );
    assert_closed(&mut stream);
}

#[test]
fn a_query_that_is_not_utf8_is_refused_but_the_connection_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut stream = raw(start_on(dir.path()));
    let frame = Frame {
        command: command::QUERY,
        payload: vec![0xff, 0xfe],
    };
    write_frame(&mut stream, &frame).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::NOT_UTF8);
    write_frame(
        &mut stream,
        &Request::Query("CREATE TABLE t (id INTEGER PRIMARY KEY)".into()).encode(),
    )
    .unwrap();
    assert_eq!(read_response(&mut stream), Response::Done);
}

#[test]
fn a_megabyte_blob_goes_there_and_back() {
    // Also runs the deepest path of a connection thread with its small
    // stack (ADR 0010): a long statement, overflow pages, a large row.
    let dir = tempfile::tempdir().unwrap();
    let mut client = connect(start_on(dir.path()));
    client
        .query("CREATE TABLE f (id INTEGER PRIMARY KEY, data BLOB)")
        .unwrap();
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    client
        .query(&format!("INSERT INTO f VALUES (1, X'{hex}')"))
        .unwrap();
    assert_eq!(
        client.query("SELECT data FROM f WHERE id = 1").unwrap(),
        Output::Rows {
            columns: vec!["data".into()],
            rows: vec![vec![Value::Blob(data)]],
        }
    );
}

#[test]
fn a_result_over_the_frame_limit_is_an_error_and_the_connection_goes_on() {
    let db = Db::open_with(SimFs::new(), "/db").unwrap();
    execute(&db, "CREATE TABLE f (id INTEGER PRIMARY KEY, data BLOB)").unwrap();
    // Too big to send as a query, so put it in before the server starts.
    let hex = "ab".repeat(MAX_FRAME_LEN as usize);
    execute(&db, &format!("INSERT INTO f VALUES (1, X'{hex}')")).unwrap();
    drop(hex);
    let mut client = connect(start(db, config()));
    assert_eq!(
        server_error(client.query("SELECT data FROM f WHERE id = 1")),
        code::RESULT_TOO_LARGE
    );
    assert_eq!(
        client.query("SELECT id FROM f WHERE id = 1").unwrap(),
        Output::Rows {
            columns: vec!["id".into()],
            rows: vec![vec![Value::Integer(1)]],
        }
    );
}

#[test]
fn several_clients_write_at_the_same_time() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_on(dir.path());
    let workers: Vec<_> = (0..8)
        .map(|c| {
            thread::spawn(move || {
                let mut client = connect(addr);
                // Every CREATE TABLE writes the table counter, so of two at
                // once the later loses (design 0017, section 6): retry.
                let create = format!("CREATE TABLE t{c} (id INTEGER PRIMARY KEY, v INTEGER)");
                loop {
                    match client.query(&create) {
                        Ok(_) => break,
                        Err(ClientError::Server {
                            code: code::CONFLICT,
                            ..
                        }) => continue,
                        Err(e) => panic!("{e:?}"),
                    }
                }
                for i in 0..20 {
                    client
                        .query(&format!("INSERT INTO t{c} VALUES ({i}, {})", i * c))
                        .unwrap();
                }
                for i in 0..20 {
                    assert_eq!(
                        client
                            .query(&format!("SELECT v FROM t{c} WHERE id = {i}"))
                            .unwrap(),
                        Output::Rows {
                            columns: vec!["v".into()],
                            rows: vec![vec![Value::Integer(i * c)]],
                        }
                    );
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
}

#[test]
fn three_hundred_clients_stay_connected_and_all_get_their_answers() {
    // Server and clients share this process: 300 sockets on each side, more
    // than macOS's default soft limit of 256 descriptors.
    raise_fd_limit(2048).unwrap();
    const CLIENTS: usize = 300;
    let dir = tempfile::tempdir().unwrap();
    let addr = start_on(dir.path());
    connect(addr)
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();

    // Every client is connected, with its handshake done, before any runs
    // a statement: all 300 are open at once.
    let clients: Vec<Client> = (0..CLIENTS).map(|_| connect(addr)).collect();
    let start = Arc::new(Barrier::new(CLIENTS));
    let workers: Vec<_> = clients
        .into_iter()
        .enumerate()
        .map(|(i, mut client)| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                assert_eq!(
                    client
                        .query(&format!("INSERT INTO t VALUES ({i}, 'client {i}')"))
                        .unwrap(),
                    Output::Affected(1)
                );
                assert_eq!(
                    client
                        .query(&format!("SELECT v FROM t WHERE id = {i}"))
                        .unwrap(),
                    Output::Rows {
                        columns: vec!["v".into()],
                        rows: vec![vec![Value::Text(format!("client {i}"))]],
                    }
                );
                client
            })
        })
        .collect();
    let clients: Vec<Client> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(clients.len(), CLIENTS);
}

#[test]
fn a_client_over_the_connection_limit_is_refused_until_a_seat_frees() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(
        Db::open(dir.path()).unwrap(),
        Config {
            max_connections: 4,
            ..config()
        },
    );
    let mut seated: Vec<Client> = (0..4).map(|_| connect(addr)).collect();
    match Client::connect(addr) {
        Err(ClientError::Server { code, message }) => {
            assert_eq!(code, code::TOO_MANY_CONNECTIONS);
            assert!(message.contains('4'), "{message}");
        }
        other => panic!("the fifth client got in: {other:?}"),
    }
    // The four seated ones still work.
    for client in &mut seated {
        client.query("SELECT 1").unwrap_err();
    }

    drop(seated.pop());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match Client::connect(addr) {
            Ok(_) => break,
            Err(ClientError::Server {
                code: code::TOO_MANY_CONNECTIONS,
                ..
            }) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("{e:?}"),
        }
    }
}

#[test]
fn a_client_not_on_the_allowlist_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    // Loopback clients are always let in through the command line, so only
    // a strict list, built here, can turn 127.0.0.1 away.
    let addr = start(
        Db::open(dir.path()).unwrap(),
        Config {
            allow: Allowlist::strict(vec!["127.0.0.2".parse().unwrap()]),
            ..config()
        },
    );
    match Client::connect(addr) {
        Err(ClientError::Server { code, message }) => {
            assert_eq!(code, code::NOT_ALLOWED);
            assert!(message.contains("127.0.0.1"), "{message}");
        }
        other => panic!("127.0.0.1 got in: {other:?}"),
    }

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    assert_eq!(error_code(read_response(&mut stream)), code::NOT_ALLOWED);
    assert_closed(&mut stream);
}

#[test]
fn after_the_disk_fails_every_statement_says_the_engine_stopped() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), "/db").unwrap();
    let addr = start(db, config());
    let mut client = connect(addr);
    client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .unwrap();

    fs.fail_once_after(0);
    assert_eq!(
        server_error(client.query("INSERT INTO t VALUES (1)")),
        code::OUTCOME_UNKNOWN
    );
    assert_eq!(
        server_error(client.query("SELECT id FROM t WHERE id = 1")),
        code::STOPPED
    );
    // The server stays up and tells new clients the same.
    let mut other = connect(addr);
    assert_eq!(
        server_error(other.query("INSERT INTO t VALUES (2)")),
        code::STOPPED
    );
}

#[test]
fn a_dropped_connection_frees_its_thread_without_harm() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(
        Db::open(dir.path()).unwrap(),
        Config {
            max_connections: 2,
            ..config()
        },
    );
    for i in 0..10 {
        // Half a frame, then gone.
        let mut stream = raw(addr);
        std::io::Write::write_all(&mut stream, &[0, 0, 0, 9, command::QUERY]).unwrap();
        drop(stream);
        // The seat comes back once the server sees the end; until then the
        // limit of two may turn a client away.
        let deadline = Instant::now() + TIMEOUT;
        let mut client = loop {
            match Client::connect(addr) {
                Ok(client) => break client,
                Err(ClientError::Server {
                    code: code::TOO_MANY_CONNECTIONS,
                    ..
                }) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("{e:?}"),
            }
        };
        assert_eq!(
            client
                .query(&format!("CREATE TABLE t{i} (id INTEGER PRIMARY KEY)"))
                .unwrap(),
            Output::Done
        );
    }
}

#[test]
fn refused_clients_read_their_error_every_time() {
    // The refusal must arrive before the close, never as a reset (review
    // P3), however the client's HELLO and the close cross.
    let dir = tempfile::tempdir().unwrap();
    let addr = start(
        Db::open(dir.path()).unwrap(),
        Config {
            max_connections: 1,
            ..config()
        },
    );
    let _seated = connect(addr);
    for _ in 0..100 {
        match Client::connect(addr) {
            Err(ClientError::Server {
                code: code::TOO_MANY_CONNECTIONS,
                ..
            }) => {}
            other => panic!("expected 0x45, got {other:?}"),
        }
    }
}

/// A disk that panics on the next write once armed: a bug inside the
/// server, as seen from a connection thread.
#[derive(Clone)]
struct PanickingFs {
    inner: SimFs,
    armed: Arc<AtomicBool>,
}

struct PanickingFile {
    inner: <SimFs as Fs>::File,
    armed: Arc<AtomicBool>,
}

impl Fs for PanickingFs {
    type File = PanickingFile;

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }

    fn open(&self, path: &Path) -> std::io::Result<PanickingFile> {
        Ok(PanickingFile {
            inner: self.inner.open(path)?,
            armed: Arc::clone(&self.armed),
        })
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.inner.sync_dir(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }
}

impl FsFile for PanickingFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        if self.armed.swap(false, Ordering::SeqCst) {
            panic!("a bug in the server (planted by the test)");
        }
        self.inner.write_all_at(buf, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }

    fn try_lock(&self) -> std::io::Result<()> {
        self.inner.try_lock()
    }

    fn sync_data(&self) -> std::io::Result<()> {
        self.inner.sync_data()
    }
}

#[test]
fn a_panic_in_the_commit_thread_reaches_the_client_as_an_engine_error_and_the_server_goes_on() {
    let fs = PanickingFs {
        inner: SimFs::new(),
        armed: Arc::new(AtomicBool::new(false)),
    };
    let armed = Arc::clone(&fs.armed);
    let addr = start(Db::open_with(fs, "/db").unwrap(), config());

    let mut stream = raw(addr);
    armed.store(true, Ordering::SeqCst);
    let create = Request::Query("CREATE TABLE t (id INTEGER PRIMARY KEY)".into());
    write_frame(&mut stream, &create.encode()).unwrap();
    // The commit thread turns its panic into the engine's outcome-unknown
    // error (#27), so the client gets that code, not INTERNAL (0x7F is
    // tested in server.rs, where the connection thread itself panics).
    let got = error_code(read_response(&mut stream));
    assert_ne!(got, code::INTERNAL);
    assert!(
        (0x20..=0x3F).contains(&got),
        "an engine error, got {got:#x}"
    );

    // Other clients still get in.
    raw(addr);
}

#[test]
fn a_statement_cut_off_by_ctrl_d_runs_from_a_script_but_not_from_a_person() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start_on(dir.path());
    let run = |input: &str, interactive: bool| {
        let mut client = connect(addr);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = oakdb_server::cli::run(
            &mut client,
            input.as_bytes(),
            &mut out,
            &mut err,
            interactive,
        )
        .unwrap();
        (code, String::from_utf8(out).unwrap())
    };

    let (code, _) = run("CREATE TABLE typed (id INTEGER PRIMARY KEY)", true);
    assert_eq!(code, 0);
    let (code, _) = run("SELECT id FROM typed WHERE id = 1;", false);
    assert_eq!(code, 1, "the half-typed CREATE ran");

    let (code, out) = run("CREATE TABLE piped (id INTEGER PRIMARY KEY)", false);
    assert_eq!((code, out.as_str()), (0, "OK\n"));
}

#[test]
fn a_where_nested_twenty_thousand_deep_gets_an_error_on_a_connection_thread_and_the_server_goes_on()
{
    // The connection thread has a small stack (CONNECTION_STACK); the
    // parser's depth limit (#32) must stop the input before it does.
    let addr = start(Db::open_with(SimFs::new(), "/db").unwrap(), config());
    let mut stream = raw(addr);
    let send = |stream: &mut TcpStream, sql: String| {
        write_frame(stream, &Request::Query(sql).encode()).unwrap();
        read_response(stream)
    };
    assert!(matches!(
        send(
            &mut stream,
            "CREATE TABLE t (id INTEGER PRIMARY KEY)".into()
        ),
        Response::Done
    ));
    let nots = "NOT ".repeat(20_000);
    let parens = format!("{}id = 1{}", "(".repeat(5_000), ")".repeat(5_000));
    let chain = format!("id{} = 1", " + 1".repeat(20_000));
    for clause in [format!("{nots}id = 1"), parens, chain] {
        let got = error_code(send(&mut stream, format!("SELECT * FROM t WHERE {clause}")));
        assert_eq!(got, code::SYNTAX, "a depth error, not a crash");
    }
    // Same connection, still served; a new one gets in too.
    assert!(matches!(
        send(&mut stream, "SELECT * FROM t WHERE id = 1".into()),
        Response::Rows { .. }
    ));
    raw(addr);
}

#[test]
fn update_and_delete_return_their_row_counts_over_the_connection() {
    // Issue #33, PR #73 review P4: the count goes through as Affected.
    let dir = tempfile::tempdir().unwrap();
    let mut client = connect(start_on(dir.path()));
    client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    client
        .query("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    assert_eq!(
        client.query("UPDATE t SET id = id + 1, v = 'x'").unwrap(),
        Output::Affected(3)
    );
    assert_eq!(
        client.query("DELETE FROM t WHERE id >= 3").unwrap(),
        Output::Affected(2)
    );
    assert_eq!(
        client.query("SELECT * FROM t").unwrap(),
        Output::Rows {
            columns: vec!["id".into(), "v".into()],
            rows: vec![vec![Value::Integer(2), Value::Text("x".into())]],
        }
    );
}

/// Design 0027 E13: one statement per error group, sent to a real server,
/// comes back with its group's code, and after each error the same
/// connection runs the next statement as usual.
#[test]
fn each_error_group_comes_back_with_its_code_and_the_connection_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = connect(start_on(dir.path()));
    client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    client.query("INSERT INTO t VALUES (1, 'a')").unwrap();
    for (sql, expected) in [
        ("SELECT * FORM t", code::SYNTAX),
        ("SELECT COUNT(*) FROM t", code::UNSUPPORTED),
        ("-- note\nSELECT * FROM t", code::UNSUPPORTED),
        ("SELECT * FROM nope", code::NO_SUCH_TABLE),
        ("SELECT nope FROM t", code::NO_SUCH_COLUMN),
        (
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
            code::TABLE_EXISTS,
        ),
        (
            "INSERT INTO t (id, id) VALUES (2, 2)",
            code::DUPLICATE_COLUMN,
        ),
        ("INSERT INTO t VALUES (2)", code::VALUE_COUNT),
        ("INSERT INTO t VALUES (1, 'b')", code::DUPLICATE_KEY),
        ("SELECT * FROM t WHERE name = 1", code::TYPE_MISMATCH),
        ("SELECT * FROM t WHERE name + 1 = 2", code::TYPE_MISMATCH),
        ("SELECT * FROM t WHERE 1 / 0 = 1", code::ARITHMETIC),
        (
            "SELECT * FROM t WHERE id = 99999999999999999999",
            code::ARITHMETIC,
        ),
        ("INSERT INTO t VALUES (NULL, 'b')", code::NULL_KEY),
    ] {
        let result = client.query(sql);
        assert_eq!(server_error(result), expected, "{sql}");
        assert_eq!(
            client.query("SELECT id FROM t").unwrap(),
            Output::Rows {
                columns: vec!["id".into()],
                rows: vec![vec![Value::Integer(1)]],
            },
            "after {sql}"
        );
    }
}
