//! Blind tests for the first server pass (#36), written from issue #36,
//! design 0020, ADR 0010, docs/protocol.md, design 0017 and the `--help`
//! text only, without reading the implementation.
//!
//! The frames are built and read here by hand from `docs/protocol.md`, not
//! with the crate's protocol module, so the documented bytes are what is
//! tested. Most tests run the real `oakdb` binary as a child process on
//! port 0 and read the address it prints.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_oakdb");
const MAX_FRAME: u32 = 16 * 1024 * 1024;

// Commands (protocol.md section 3).
const HELLO: u8 = 0x01;
const QUERY: u8 = 0x02;
const READY: u8 = 0x81;
const ROWS: u8 = 0x82;
const AFFECTED: u8 = 0x83;
const DONE: u8 = 0x84;
const ERROR: u8 = 0x85;

// ---------------------------------------------------------------------
// A server child process
// ---------------------------------------------------------------------

struct Srv {
    child: Child,
    addr: SocketAddr,
    line: String,
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct StartFailure {
    status: ExitStatus,
    stderr: String,
}

/// Starts `oakdb serve --data <dir> --port 0 <extra>` and reads the one
/// line it prints with the real address.
fn start_in(dir: &Path, extra: &[&str]) -> Result<Srv, StartFailure> {
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
    let mut line = String::new();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let n = out.read_line(&mut line).unwrap_or(0);
        tx.send((n, line)).ok();
        // Keep draining so the server never blocks on a full pipe.
        io::copy(&mut out, &mut io::sink()).ok();
    });
    drop(reader);
    let (n, line) = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server printed nothing within 30 s");
    if n == 0 {
        let status = child.wait().unwrap();
        thread::sleep(Duration::from_millis(100));
        let stderr = stderr.lock().unwrap().clone();
        return Err(StartFailure { status, stderr });
    }
    let addr = parse_listening(&line);
    Ok(Srv { child, addr, line })
}

/// "oakdb 0.1.0 listening on 127.0.0.1:54321, data ./mydb" -> the address.
fn parse_listening(line: &str) -> SocketAddr {
    let rest = line
        .split("listening on ")
        .nth(1)
        .unwrap_or_else(|| panic!("no 'listening on' in {line:?}"));
    let addr = rest.split(',').next().unwrap().trim();
    addr.parse()
        .unwrap_or_else(|_| panic!("bad address in {line:?}"))
}

fn start(extra: &[&str]) -> (Srv, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_in(dir.path(), extra).expect("server should start");
    (srv, dir)
}

/// Where a client connects to: loopback on the printed port, even if the
/// server listens on 0.0.0.0.
fn loopback(srv: &Srv) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], srv.addr.port()))
}

// ---------------------------------------------------------------------
// My own frames, from docs/protocol.md
// ---------------------------------------------------------------------

fn frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + data.len());
    f.extend_from_slice(&((1 + data.len()) as u32).to_be_bytes());
    f.push(cmd);
    f.extend_from_slice(data);
    f
}

fn hello_frame() -> Vec<u8> {
    frame(HELLO, b"OAKDB\x00\x01")
}

fn query_frame(sql: &str) -> Vec<u8> {
    frame(QUERY, sql.as_bytes())
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
    assert!(len >= 1, "server sent a zero-length frame");
    assert!(len <= MAX_FRAME, "server sent a frame over 16 MiB");
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body)?;
    let cmd = body.remove(0);
    Ok((cmd, body))
}

/// The raw bytes of one whole frame, length included.
fn read_raw_frame(s: &mut TcpStream) -> Vec<u8> {
    let (cmd, data) = read_frame(s).expect("a frame");
    frame(cmd, &data)
}

/// Connects and says HELLO; asserts the documented READY.
fn open(addr: SocketAddr) -> TcpStream {
    let mut s = connect(addr);
    s.write_all(&hello_frame()).unwrap();
    let (cmd, data) = read_frame(&mut s).expect("READY");
    assert_eq!((cmd, data.as_slice()), (READY, &[0x00, 0x01][..]));
    s
}

fn query(s: &mut TcpStream, sql: &str) -> (u8, Vec<u8>) {
    s.write_all(&query_frame(sql)).unwrap();
    read_frame(s).expect("an answer")
}

fn expect_done(s: &mut TcpStream, sql: &str) {
    let (cmd, data) = query(s, sql);
    assert_eq!(cmd, DONE, "{sql}: got {cmd:#x} {}", lossy(&data));
    assert!(data.is_empty());
}

fn expect_affected(s: &mut TcpStream, sql: &str) -> u64 {
    let (cmd, data) = query(s, sql);
    assert_eq!(cmd, AFFECTED, "{sql}: got {cmd:#x} {}", lossy(&data));
    assert_eq!(data.len(), 8, "AFFECTED carries a u64");
    u64::from_be_bytes(data.try_into().unwrap())
}

fn expect_rows(s: &mut TcpStream, sql: &str) -> (Vec<String>, Vec<Vec<V>>) {
    let (cmd, data) = query(s, sql);
    assert_eq!(cmd, ROWS, "{sql}: got {cmd:#x} {}", lossy(&data));
    decode_rows(&data)
}

/// An ERROR's code and message.
fn expect_error(s: &mut TcpStream, sql: &str) -> (u8, String) {
    let (cmd, data) = query(s, sql);
    assert_eq!(cmd, ERROR, "{sql}: expected ERROR, got {cmd:#x}");
    split_error(&data)
}

fn split_error(data: &[u8]) -> (u8, String) {
    assert!(!data.is_empty(), "ERROR has a code byte");
    let msg = String::from_utf8(data[1..].to_vec()).expect("message is UTF-8");
    (data[0], msg)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Reads the next frame, which must be ERROR with `code`, and then the end
/// of the stream.
fn expect_error_then_closed(s: &mut TcpStream, code: u8) -> String {
    let (cmd, data) = read_frame(s).expect("an ERROR frame before the close");
    assert_eq!(cmd, ERROR, "expected ERROR, got {cmd:#x} {}", lossy(&data));
    let (got, msg) = split_error(&data);
    assert_eq!(got, code, "expected code {code:#x}, got {got:#x} ({msg})");
    expect_closed(s);
    msg
}

fn expect_closed(s: &mut TcpStream) {
    let mut rest = Vec::new();
    match s.read_to_end(&mut rest) {
        Ok(_) => assert!(rest.is_empty(), "bytes after the close: {rest:?}"),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("connection was not closed: {e}"),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum V {
    Null,
    Int(i64),
    Text(String),
    Blob(Vec<u8>),
}

struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        assert!(self.0.len() >= n, "ROWS ends early");
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        a
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take(2).try_into().unwrap())
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }
}

/// ROWS data, protocol.md section 4.
fn decode_rows(data: &[u8]) -> (Vec<String>, Vec<Vec<V>>) {
    let mut c = Cur(data);
    let ncols = c.u16() as usize;
    let mut cols = Vec::new();
    for _ in 0..ncols {
        let n = c.u16() as usize;
        cols.push(String::from_utf8(c.take(n).to_vec()).unwrap());
    }
    let nrows = c.u32() as usize;
    let mut rows = Vec::new();
    for _ in 0..nrows {
        let mut row = Vec::new();
        for _ in 0..ncols {
            row.push(match c.u8() {
                0x00 => V::Null,
                0x01 => V::Int(i64::from_be_bytes(c.take(8).try_into().unwrap())),
                0x02 => {
                    let n = c.u32() as usize;
                    V::Text(String::from_utf8(c.take(n).to_vec()).unwrap())
                }
                0x03 => {
                    let n = c.u32() as usize;
                    V::Blob(c.take(n).to_vec())
                }
                t => panic!("unknown value type {t:#x}"),
            });
        }
        rows.push(row);
    }
    assert!(c.0.is_empty(), "bytes left after ROWS: {:?}", c.0);
    if ncols == 0 {
        assert!(rows.is_empty(), "no columns means no rows");
    }
    (cols, rows)
}

fn t(s: &str) -> V {
    V::Text(s.to_string())
}

/// Raises this test process's soft open-file limit (macOS starts at 256).
fn raise_my_fd_limit() {
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        let want: libc::rlim_t = 4096;
        let want = want.min(lim.rlim_max);
        if lim.rlim_cur < want {
            lim.rlim_cur = want;
            assert_eq!(
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim),
                0,
                "could not raise the open-file limit"
            );
        }
    }
}

// ---------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------

#[test]
fn the_server_prints_one_line_with_the_real_loopback_address_when_given_port_zero() {
    let (srv, _dir) = start(&[]);
    assert_eq!(srv.addr.ip().to_string(), "127.0.0.1", "line: {}", srv.line);
    assert_ne!(srv.addr.port(), 0);
    let _ = open(srv.addr);
}

#[test]
fn rows_survive_killing_the_server_and_starting_it_again_on_the_same_directory() {
    let dir = tempfile::tempdir().unwrap();
    {
        let srv = start_in(dir.path(), &[]).unwrap();
        let mut s = open(srv.addr);
        expect_done(&mut s, "CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT)");
        assert_eq!(
            expect_affected(&mut s, "INSERT INTO kv VALUES (7, 'seven')"),
            1
        );
    } // killed here
    let srv = start_in(dir.path(), &[]).unwrap();
    let mut s = open(srv.addr);
    let (_, rows) = expect_rows(&mut s, "SELECT v FROM kv WHERE k = 7");
    assert_eq!(rows, vec![vec![t("seven")]]);
}

#[test]
fn a_second_server_on_the_same_data_directory_refuses_to_start_with_exit_code_1() {
    let (_first, dir) = start(&[]);
    let err = start_in(dir.path(), &[])
        .err()
        .expect("second server must not start");
    assert_eq!(err.status.code(), Some(1), "stderr: {}", err.stderr);
}

#[test]
fn a_server_on_a_port_already_in_use_refuses_to_start_with_exit_code_1() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let dir = tempfile::tempdir().unwrap();
    let out = common::output_within(
        Command::new(BIN)
            .arg("serve")
            .arg("--data")
            .arg(dir.path())
            .args(["--port", &port]),
        "",
    );
    assert_eq!(out.status.code(), Some(1), "stderr: {}", lossy(&out.stderr));
}

#[test]
fn serve_without_data_does_not_start() {
    let out = common::output_within(Command::new(BIN).args(["serve", "--port", "0"]), "");
    assert!(!out.status.success());
    assert!(!lossy(&out.stdout).contains("listening on"));
}

#[test]
fn serve_help_explains_that_the_allowlist_is_not_authentication() {
    let out = common::output_within(Command::new(BIN).args(["serve", "--help"]), "");
    let all = format!("{}{}", lossy(&out.stdout), lossy(&out.stderr));
    assert!(all.contains("NOT AUTHENTICATION"), "output: {all}");
    assert!(
        out.status.success(),
        "`oakdb serve --help` should succeed, got {:?}: {all}",
        out.status
    );
}

// ---------------------------------------------------------------------
// Handshake and the documented example
// ---------------------------------------------------------------------

#[test]
fn hello_is_answered_with_exactly_the_documented_ready_bytes() {
    let (srv, _dir) = start(&[]);
    let mut s = connect(srv.addr);
    s.write_all(&[0, 0, 0, 8, 0x01, 0x4f, 0x41, 0x4b, 0x44, 0x42, 0x00, 0x01])
        .unwrap();
    let mut ready = [0u8; 7];
    s.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [0, 0, 0, 3, 0x81, 0x00, 0x01]);
}

#[test]
fn the_example_conversation_in_the_protocol_doc_gives_its_documented_bytes() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(
        &mut s,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
    );
    assert_eq!(
        expect_affected(&mut s, "INSERT INTO users VALUES (1, 'kim')"),
        1
    );

    let sql = "SELECT name FROM users WHERE id = 1";
    let q = query_frame(sql);
    assert_eq!(&q[..5], &[0, 0, 0, 0x24, 0x02], "doc says length 0x24");
    s.write_all(&q).unwrap();
    assert_eq!(
        read_raw_frame(&mut s),
        vec![
            0, 0, 0, 0x15, 0x82, 0, 1, 0, 4, b'n', b'a', b'm', b'e', 0, 0, 0, 1, 0x02, 0, 0, 0, 3,
            b'k', b'i', b'm'
        ]
    );

    s.write_all(&query_frame("SELECT * FROM nope WHERE id = 1"))
        .unwrap();
    let mut want = vec![0, 0, 0, 0x15, 0x85, 0x03];
    want.extend_from_slice(b"no such table: nope");
    assert_eq!(read_raw_frame(&mut s), want);
}

#[test]
fn create_table_is_answered_with_done_and_no_data() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    s.write_all(&query_frame("CREATE TABLE a (id INTEGER PRIMARY KEY)"))
        .unwrap();
    assert_eq!(read_raw_frame(&mut s), vec![0, 0, 0, 1, 0x84]);
}

#[test]
fn a_multi_row_insert_is_answered_with_its_row_count_as_a_big_endian_u64() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(&mut s, "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)");
    s.write_all(&query_frame(
        "INSERT INTO a VALUES (1, 'x'), (2, 'y'), (3, 'z');",
    ))
    .unwrap();
    assert_eq!(
        read_raw_frame(&mut s),
        vec![0, 0, 0, 9, 0x83, 0, 0, 0, 0, 0, 0, 0, 3]
    );
}

// ---------------------------------------------------------------------
// SQL round trips
// ---------------------------------------------------------------------

#[test]
fn every_value_type_comes_back_in_rows_as_documented() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(
        &mut s,
        "CREATE TABLE v (id INTEGER PRIMARY KEY, n INTEGER, s TEXT, b BLOB)",
    );
    assert_eq!(
        expect_affected(
            &mut s,
            "INSERT INTO v VALUES \
             (1, -9223372036854775808, 'it''s; fine', X'00ff10'), \
             (2, 9223372036854775807, '한글 ✓', X''), \
             (3, -1, '', NULL), \
             (4, NULL, NULL, NULL)"
        ),
        4
    );
    let cols = vec!["id", "n", "s", "b"];
    let cases = [
        (
            1,
            vec![
                V::Int(1),
                V::Int(i64::MIN),
                t("it's; fine"),
                V::Blob(vec![0x00, 0xff, 0x10]),
            ],
        ),
        (
            2,
            vec![V::Int(2), V::Int(i64::MAX), t("한글 ✓"), V::Blob(vec![])],
        ),
        (3, vec![V::Int(3), V::Int(-1), t(""), V::Null]),
        (4, vec![V::Int(4), V::Null, V::Null, V::Null]),
    ];
    for (id, want) in cases {
        let (c, rows) = expect_rows(&mut s, &format!("SELECT * FROM v WHERE id = {id}"));
        assert_eq!(c, cols);
        assert_eq!(rows, vec![want]);
    }
    // Picked columns come back in the order asked.
    let (c, rows) = expect_rows(&mut s, "SELECT b, id FROM v WHERE id = 1;");
    assert_eq!(c, vec!["b", "id"]);
    assert_eq!(rows, vec![vec![V::Blob(vec![0x00, 0xff, 0x10]), V::Int(1)]]);
}

#[test]
fn a_select_that_finds_nothing_gets_rows_with_its_columns_and_zero_rows() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(&mut s, "CREATE TABLE e (id INTEGER PRIMARY KEY, v TEXT)");
    s.write_all(&query_frame("SELECT * FROM e WHERE id = 42"))
        .unwrap();
    assert_eq!(
        read_raw_frame(&mut s),
        vec![0, 0, 0, 14, 0x82, 0, 2, 0, 2, b'i', b'd', 0, 1, b'v', 0, 0, 0, 0]
    );
}

#[test]
fn a_one_mib_blob_makes_the_round_trip() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(&mut s, "CREATE TABLE big (id INTEGER PRIMARY KEY, b BLOB)");
    let blob: Vec<u8> = (0..1024 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        expect_affected(&mut s, &format!("INSERT INTO big VALUES (1, X'{hex}')")),
        1
    );
    let (_, rows) = expect_rows(&mut s, "SELECT b FROM big WHERE id = 1");
    assert_eq!(rows, vec![vec![V::Blob(blob)]]);
}

// ---------------------------------------------------------------------
// Statement and engine errors: code, and the connection stays open
// ---------------------------------------------------------------------

#[test]
fn each_statement_error_gets_its_documented_code_and_the_connection_stays_open() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(&mut s, "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)");
    assert_eq!(
        expect_affected(&mut s, "INSERT INTO u VALUES (1, 'kim')"),
        1
    );

    let long_key = "k".repeat(600);
    expect_done(&mut s, "CREATE TABLE tk (k TEXT PRIMARY KEY)");

    let cases: Vec<(String, u8)> = vec![
        ("SELECT * FROM u WHERE id = 'unclosed".into(), 0x01),
        ("SELEC * FROM u WHERE id = 1".into(), 0x02), // unknown first word: unsupported (0017 C7)
        ("UPDATE u SET name = 'x' WHERE id = 1".into(), 0x02),
        ("SELECT * FROM u WHERE id > 1".into(), 0x02),
        ("SELECT * FROM u".into(), 0x02),
        ("SELECT * FROM nope WHERE id = 1".into(), 0x03),
        ("INSERT INTO nope VALUES (1)".into(), 0x03),
        ("SELECT age FROM u WHERE id = 1".into(), 0x04),
        ("CREATE TABLE u (id INTEGER PRIMARY KEY)".into(), 0x05),
        (
            "CREATE TABLE d (a INTEGER PRIMARY KEY, a TEXT)".into(),
            0x06,
        ),
        ("INSERT INTO u (id, id) VALUES (5, 5)".into(), 0x06),
        ("INSERT INTO u VALUES (2)".into(), 0x07),
        ("INSERT INTO u VALUES (2, 'a', 'b')".into(), 0x07),
        ("INSERT INTO u VALUES (1, 'lee')".into(), 0x08),
        ("INSERT INTO u VALUES (3, 'a'), (3, 'b')".into(), 0x08),
        ("INSERT INTO u VALUES ('one', 'kim')".into(), 0x09),
        ("INSERT INTO u VALUES (4, 5)".into(), 0x09),
        ("INSERT INTO u VALUES (NULL, 'kim')".into(), 0x09),
        ("SELECT * FROM u WHERE id = 'x'".into(), 0x09),
        (format!("INSERT INTO tk VALUES ('{long_key}')"), 0x21),
    ];
    for (sql, code) in &cases {
        let (got, msg) = expect_error(&mut s, sql);
        assert_eq!(got, *code, "{sql}: got {got:#x} ({msg})");
        // Still talking: the row is still there.
        let (_, rows) = expect_rows(&mut s, "SELECT name FROM u WHERE id = 1");
        assert_eq!(rows, vec![vec![t("kim")]], "after {sql}");
    }
    // The failing multi-row insert put in nothing.
    let (_, rows) = expect_rows(&mut s, "SELECT * FROM u WHERE id = 3");
    assert!(rows.is_empty());
}

#[test]
fn a_syntax_error_message_names_the_byte_position() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    let (code, msg) = expect_error(&mut s, "SELECT * FROM t WHERE id = 'oops");
    assert_eq!(code, 0x01);
    assert!(msg.contains("byte"), "message: {msg}");
}

#[test]
fn sql_that_is_not_utf8_gets_0x43_and_the_connection_stays_open() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    s.write_all(&frame(QUERY, b"SELECT \xff\xfe FROM t WHERE id = 1"))
        .unwrap();
    let (cmd, data) = read_frame(&mut s).unwrap();
    assert_eq!(cmd, ERROR);
    assert_eq!(data[0], 0x43);
    expect_done(&mut s, "CREATE TABLE ok (id INTEGER PRIMARY KEY)");
}

// ---------------------------------------------------------------------
// Protocol errors: code, then the server hangs up
// ---------------------------------------------------------------------

#[test]
fn a_first_frame_that_is_not_hello_gets_0x42_and_the_connection_is_closed() {
    let (srv, _dir) = start(&[]);
    let mut s = connect(srv.addr);
    s.write_all(&query_frame("SELECT * FROM t WHERE id = 1"))
        .unwrap();
    expect_error_then_closed(&mut s, 0x42);
}

#[test]
fn a_hello_with_the_wrong_magic_gets_0x42_and_the_connection_is_closed() {
    let (srv, _dir) = start(&[]);
    let mut s = connect(srv.addr);
    s.write_all(&frame(HELLO, b"OAKDX\x00\x01")).unwrap();
    expect_error_then_closed(&mut s, 0x42);
}

#[test]
fn a_hello_with_another_version_gets_0x42_and_the_connection_is_closed() {
    let (srv, _dir) = start(&[]);
    for v in [[0x00, 0x00], [0x00, 0x02], [0x01, 0x00], [0xff, 0xff]] {
        let mut s = connect(srv.addr);
        let mut data = b"OAKDB".to_vec();
        data.extend_from_slice(&v);
        s.write_all(&frame(HELLO, &data)).unwrap();
        expect_error_then_closed(&mut s, 0x42);
    }
}

#[test]
fn a_hello_of_the_wrong_size_is_refused_as_a_broken_frame_or_failed_handshake() {
    let (srv, _dir) = start(&[]);
    for data in [&b""[..], b"OAKDB", b"OAKDB\x00", b"OAKDB\x00\x01\x00"] {
        let mut s = connect(srv.addr);
        s.write_all(&frame(HELLO, data)).unwrap();
        let (cmd, body) = read_frame(&mut s).expect("an ERROR");
        assert_eq!(cmd, ERROR, "HELLO data {data:?}");
        assert!(
            body[0] == 0x40 || body[0] == 0x42,
            "HELLO data {data:?}: got {:#x}",
            body[0]
        );
        expect_closed(&mut s);
    }
}

#[test]
fn a_zero_length_frame_gets_0x40_and_the_connection_is_closed() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    s.write_all(&[0, 0, 0, 0]).unwrap();
    expect_error_then_closed(&mut s, 0x40);
    // Also as the very first frame.
    let mut s = connect(srv.addr);
    s.write_all(&[0, 0, 0, 0]).unwrap();
    let (cmd, body) = read_frame(&mut s).unwrap();
    assert_eq!(cmd, ERROR);
    assert!(body[0] == 0x40 || body[0] == 0x42, "got {:#x}", body[0]);
    expect_closed(&mut s);
}

#[test]
fn a_length_over_16_mib_gets_0x41_without_the_body_being_read() {
    let (srv, _dir) = start(&[]);
    for len in [MAX_FRAME + 1, u32::MAX, 0x4745_5420 /* "GET " */] {
        let mut s = open(srv.addr);
        // Only the length and the command: the server must not wait for
        // the body.
        let mut head = len.to_be_bytes().to_vec();
        head.push(QUERY);
        s.write_all(&head).unwrap();
        expect_error_then_closed(&mut s, 0x41);
    }
}

#[test]
fn an_http_request_sent_instead_of_hello_is_turned_away_without_waiting() {
    let (srv, _dir) = start(&[]);
    let mut s = connect(srv.addr);
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let (cmd, body) = read_frame(&mut s).expect("an ERROR");
    assert_eq!(cmd, ERROR);
    assert!((0x40..=0x5f).contains(&body[0]), "got {:#x}", body[0]);
}

#[test]
fn a_frame_of_exactly_16_mib_is_read_and_answered() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    expect_done(&mut s, "CREATE TABLE f (id INTEGER PRIMARY KEY, v TEXT)");
    assert_eq!(
        expect_affected(&mut s, "INSERT INTO f VALUES (1, 'one')"),
        1
    );
    let mut sql = b"SELECT v FROM f WHERE id = 1".to_vec();
    sql.resize((MAX_FRAME - 1) as usize, b' ');
    let f = frame(QUERY, &sql);
    assert_eq!(&f[..4], &MAX_FRAME.to_be_bytes());
    s.write_all(&f).unwrap();
    let (cmd, data) = read_frame(&mut s).unwrap();
    assert_eq!(cmd, ROWS, "got {cmd:#x} {}", lossy(&data));
    assert_eq!(decode_rows(&data).1, vec![vec![t("one")]]);
}

#[test]
fn an_unknown_command_after_hello_gets_0x40_and_the_connection_is_closed() {
    let (srv, _dir) = start(&[]);
    for cmd in [0x00, 0x03, 0x7f, READY, ROWS, AFFECTED, DONE, ERROR, 0xff] {
        let mut s = open(srv.addr);
        s.write_all(&frame(cmd, b"SELECT 1")).unwrap();
        expect_error_then_closed(&mut s, 0x40);
    }
}

#[test]
fn a_second_hello_gets_0x40_and_the_connection_is_closed() {
    let (srv, _dir) = start(&[]);
    let mut s = open(srv.addr);
    s.write_all(&hello_frame()).unwrap();
    expect_error_then_closed(&mut s, 0x40);
}

#[test]
fn a_conversation_written_one_byte_at_a_time_is_answered_normally() {
    let (srv, _dir) = start(&[]);
    let mut s = connect(srv.addr);
    let mut bytes = hello_frame();
    bytes.extend(query_frame(
        "CREATE TABLE slow (id INTEGER PRIMARY KEY, v TEXT)",
    ));
    bytes.extend(query_frame("INSERT INTO slow VALUES (1, 'drip')"));
    bytes.extend(query_frame("SELECT v FROM slow WHERE id = 1"));
    for b in bytes {
        s.write_all(&[b]).unwrap();
        s.flush().unwrap();
        thread::sleep(Duration::from_micros(200));
    }
    assert_eq!(read_frame(&mut s).unwrap(), (READY, vec![0, 1]));
    assert_eq!(read_frame(&mut s).unwrap().0, DONE);
    assert_eq!(
        read_frame(&mut s).unwrap(),
        (AFFECTED, 1u64.to_be_bytes().to_vec())
    );
    let (cmd, data) = read_frame(&mut s).unwrap();
    assert_eq!(cmd, ROWS);
    assert_eq!(decode_rows(&data).1, vec![vec![t("drip")]]);
}

#[test]
fn several_frames_in_one_write_are_answered_in_the_order_sent() {
    let (srv, _dir) = start(&[]);
    let mut s = connect(srv.addr);
    let mut bytes = hello_frame();
    bytes.extend(query_frame(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT)",
    ));
    bytes.extend(query_frame("INSERT INTO p VALUES (1, 'a'), (2, 'b')"));
    bytes.extend(query_frame("SELECT * FROM nope WHERE id = 1"));
    bytes.extend(query_frame("SELECT v FROM p WHERE id = 2"));
    s.write_all(&bytes).unwrap();
    assert_eq!(read_frame(&mut s).unwrap(), (READY, vec![0, 1]));
    assert_eq!(read_frame(&mut s).unwrap().0, DONE);
    assert_eq!(
        read_frame(&mut s).unwrap(),
        (AFFECTED, 2u64.to_be_bytes().to_vec())
    );
    let (cmd, data) = read_frame(&mut s).unwrap();
    assert_eq!((cmd, data[0]), (ERROR, 0x03));
    let (cmd, data) = read_frame(&mut s).unwrap();
    assert_eq!(cmd, ROWS);
    assert_eq!(decode_rows(&data).1, vec![vec![t("b")]]);
}

#[test]
fn clients_that_leave_mid_frame_do_not_disturb_the_server_or_other_clients() {
    let (srv, _dir) = start(&[]);
    let mut keeper = open(srv.addr);
    expect_done(
        &mut keeper,
        "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
    );
    assert_eq!(
        expect_affected(&mut keeper, "INSERT INTO m VALUES (1, 'x')"),
        1
    );

    let half_hello = hello_frame()[..6].to_vec();
    let q = query_frame("INSERT INTO m VALUES (2, 'never')");
    let cuts: Vec<(bool, Vec<u8>)> = vec![
        (false, vec![0, 0]),               // half a length, before HELLO
        (false, half_hello),               // half a HELLO
        (true, vec![0, 0]),                // half a length after READY
        (true, q[..4].to_vec()),           // length only
        (true, q[..q.len() - 3].to_vec()), // all but the end of the SQL
        (true, {
            let mut v = 100u32.to_be_bytes().to_vec();
            v.push(QUERY);
            v.extend_from_slice(b"SELECT");
            v
        }),
    ];
    for (after_hello, bytes) in cuts {
        let mut s = if after_hello {
            open(srv.addr)
        } else {
            connect(srv.addr)
        };
        s.write_all(&bytes).unwrap();
        drop(s);
    }
    thread::sleep(Duration::from_millis(100));

    // The old connection still works, the cut INSERT did not happen, and a
    // new client gets in.
    let (_, rows) = expect_rows(&mut keeper, "SELECT v FROM m WHERE id = 1");
    assert_eq!(rows, vec![vec![t("x")]]);
    let mut fresh = open(srv.addr);
    let (_, rows) = expect_rows(&mut fresh, "SELECT v FROM m WHERE id = 2");
    assert!(rows.is_empty(), "a half-sent INSERT must not run");
    assert_eq!(
        expect_affected(&mut fresh, "INSERT INTO m VALUES (2, 'y')"),
        1
    );
}

// ---------------------------------------------------------------------
// Connection limit and many connections
// ---------------------------------------------------------------------

#[test]
fn a_connection_over_the_limit_gets_0x45_before_hello_and_a_freed_seat_is_reused() {
    let (srv, _dir) = start(&["--max-connections", "4"]);
    let mut held: Vec<TcpStream> = (0..4).map(|_| open(srv.addr)).collect();

    // The fifth gets ERROR 0x45 without saying HELLO.
    let mut fifth = connect(srv.addr);
    let msg = expect_error_then_closed(&mut fifth, 0x45);
    assert!(msg.contains("too many connections"), "message: {msg}");

    // The four still work.
    for s in &mut held {
        assert_eq!(expect_error(s, "SELECT * FROM nope WHERE id = 1").0, 0x03);
    }
    // Close one; soon a new client is let in.
    drop(held.pop());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut s = connect(srv.addr);
        s.write_all(&hello_frame()).unwrap();
        match read_frame(&mut s) {
            Ok((READY, _)) => break,
            Ok((ERROR, d)) if d[0] == 0x45 => {}
            other => panic!("unexpected {other:?}"),
        }
        assert!(Instant::now() < deadline, "a freed seat was never reused");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn three_hundred_connections_open_at_once_each_get_their_own_answer() {
    raise_my_fd_limit();
    const N: usize = 300;
    let (srv, _dir) = start(&[]);
    let mut setup = open(srv.addr);
    expect_done(
        &mut setup,
        "CREATE TABLE many (id INTEGER PRIMARY KEY, who TEXT)",
    );

    // All 300 finish the handshake before any of them asks anything.
    let conns: Vec<TcpStream> = (0..N).map(|_| open(srv.addr)).collect();
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = conns
        .into_iter()
        .enumerate()
        .map(|(i, mut s)| {
            let barrier = Arc::clone(&barrier);
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    barrier.wait();
                    let n = expect_affected(
                        &mut s,
                        &format!("INSERT INTO many VALUES ({i}, 'conn-{i}')"),
                    );
                    assert_eq!(n, 1);
                    let (_, rows) =
                        expect_rows(&mut s, &format!("SELECT who FROM many WHERE id = {i}"));
                    assert_eq!(rows, vec![vec![t(&format!("conn-{i}"))]]);
                    s
                })
                .unwrap()
        })
        .collect();
    let conns: Vec<TcpStream> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(conns.len(), N);
    // Everything is visible from yet another connection.
    for i in [0, N / 2, N - 1] {
        let (_, rows) = expect_rows(&mut setup, &format!("SELECT who FROM many WHERE id = {i}"));
        assert_eq!(rows, vec![vec![t(&format!("conn-{i}"))]]);
    }
}

// ---------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------

#[test]
fn an_entry_outside_loopback_opens_every_address_and_still_lets_loopback_in() {
    let (srv, _dir) = start(&["--allow", "10.0.0.5", "--allow", "192.168.1.0/24"]);
    assert_eq!(srv.addr.ip().to_string(), "0.0.0.0", "line: {}", srv.line);
    let mut s = open(loopback(&srv));
    expect_done(&mut s, "CREATE TABLE a (id INTEGER PRIMARY KEY)");
}

#[test]
fn an_allowlist_of_only_loopback_entries_keeps_listening_on_127_0_0_1() {
    let (srv, _dir) = start(&["--allow", "127.0.0.1", "--allow", "127.0.0.0/8"]);
    assert_eq!(srv.addr.ip().to_string(), "127.0.0.1", "line: {}", srv.line);
    let _ = open(srv.addr);
}

#[test]
fn allow_everyone_starts_with_a_warning_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_in(dir.path(), &["--allow", "0.0.0.0/0"]).unwrap();
    assert_eq!(srv.addr.ip().to_string(), "0.0.0.0");
    let _ = open(loopback(&srv));
}

#[test]
fn a_malformed_allow_entry_stops_the_server_from_starting() {
    for bad in [
        "10.0.0.5/24", // host bits after the prefix
        "10.0.0.0/33", // prefix too long
        "10.0.0.256",  // not an address
        "10.0.0",      // too short
        "10.0.0.0/",   // empty prefix
        "banana",      // nonsense
        "::1",         // IPv6
        "fe80::/10",   // IPv6 block
    ] {
        let dir = tempfile::tempdir().unwrap();
        match start_in(dir.path(), &["--allow", bad]) {
            Ok(srv) => panic!("--allow {bad} started the server: {}", srv.line),
            Err(e) => {
                assert!(!e.status.success(), "--allow {bad}");
                if bad.contains(':') {
                    assert!(
                        e.stderr.contains("IPv6 is not supported yet"),
                        "--allow {bad}: stderr {}",
                        e.stderr
                    );
                }
            }
        }
    }
}

#[test]
fn a_bad_line_in_the_allow_file_stops_startup_and_names_the_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("allow");
    std::fs::write(&file, "# office\n\n  10.0.0.0/24   # lan\n10.1.2.3/16\n").unwrap();
    let data = dir.path().join("db");
    let err = start_in(&data, &["--allow-file", file.to_str().unwrap()])
        .err()
        .expect("must not start");
    assert!(!err.status.success());
    let place = format!("{}:4", file.display());
    assert!(err.stderr.contains(&place), "stderr: {}", err.stderr);
    assert!(err.stderr.contains("10.1.2.3/16"), "stderr: {}", err.stderr);
}

#[test]
fn an_allow_file_with_comments_blank_lines_and_spaces_is_merged_with_allow_flags() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("allow");
    std::fs::write(&file, "# office\n\n   10.0.0.0/24   # lan\n\t172.16.0.9\n").unwrap();
    let data = dir.path().join("db");
    let srv = start_in(
        &data,
        &[
            "--allow-file",
            file.to_str().unwrap(),
            "--allow",
            "127.0.0.1",
        ],
    )
    .expect("a well-formed file starts");
    assert_eq!(srv.addr.ip().to_string(), "0.0.0.0");
    let _ = open(loopback(&srv));
}

#[test]
fn a_strict_allowlist_turns_a_loopback_client_away_with_0x46_before_hello() {
    use oakdb_server::allow::Allowlist;
    use oakdb_server::server::{Config, Server};
    let dir = tempfile::tempdir().unwrap();
    let db = oakdb::Db::open(dir.path()).unwrap();
    let server = Server::bind(
        db,
        Config {
            port: 0,
            allow: Allowlist::strict(vec![]),
            max_connections: 8,
        },
    )
    .unwrap();
    let addr = server.local_addr();
    thread::spawn(move || server.run());
    let mut s = connect(SocketAddr::from(([127, 0, 0, 1], addr.port())));
    let msg = expect_error_then_closed(&mut s, 0x46);
    assert!(msg.contains("127.0.0.1"), "message: {msg}");
}

// ---------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------

struct CliRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn cli(srv: &Srv, script: &str) -> CliRun {
    let mut child = Command::new(BIN)
        .args(["cli", "--port", &srv.addr.port().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        tx.send(child.wait_with_output()).ok();
    });
    let out = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("cli did not finish within 30 s")
        .unwrap();
    CliRun {
        code: out.status.code(),
        stdout: lossy(&out.stdout),
        stderr: lossy(&out.stderr),
    }
}

#[test]
fn the_cli_runs_the_documented_piped_script_and_prints_the_documented_table() {
    let (srv, _dir) = start(&[]);
    let run = cli(
        &srv,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, photo BLOB);\n\
         INSERT INTO users VALUES\n\
         (1, 'kim', X'ab01'), (2, 'it''s; fine', NULL);\n\
         SELECT * FROM users WHERE id = 2;\n",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    let want = "OK\n\
                INSERT 2\n \
                id | name       | photo\n\
                ----+------------+-------\n  \
                2 | it's; fine | NULL\n\
                (1 row)\n";
    assert_eq!(run.stdout, want);
    assert!(!run.stdout.contains("oakdb>"), "no prompt when piped");
    assert!(
        !run.stdout.contains("connected to"),
        "no greeting when piped"
    );
}

#[test]
fn the_cli_shows_blobs_as_sql_literals_and_right_aligns_integers() {
    let (srv, _dir) = start(&[]);
    let run = cli(
        &srv,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, n INTEGER, photo BLOB);\
         INSERT INTO b VALUES (1, -12345, X'ab01');\
         SELECT * FROM b WHERE id = 1;",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("X'ab01'"), "stdout: {}", run.stdout);
    let row = run.stdout.lines().find(|l| l.contains("X'ab01'")).unwrap();
    assert!(row.contains("-12345"), "row: {row}");
    let header = run.stdout.lines().find(|l| l.contains("photo")).unwrap();
    let col = |l: &str, i: usize| l.split('|').nth(i).unwrap().to_string();
    // Integers right-aligned: "id" column value "1" ends where the column
    // ends.
    assert_eq!(col(header, 0).len(), col(row, 0).len());
    assert!(col(row, 0).trim_end_matches(' ').ends_with('1'));
    assert!(col(row, 0).starts_with(' '));
}

#[test]
fn the_cli_reports_zero_rows() {
    let (srv, _dir) = start(&[]);
    let run = cli(
        &srv,
        "CREATE TABLE z (id INTEGER PRIMARY KEY);\nSELECT * FROM z WHERE id = 1;\n",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stdout.contains("(0 rows)"), "stdout: {}", run.stdout);
}

#[test]
fn a_failing_piped_statement_prints_its_code_on_stderr_keeps_going_and_exits_with_1() {
    let (srv, _dir) = start(&[]);
    let run = cli(
        &srv,
        "SELECT * FROM nope WHERE id = 1;\n\
         CREATE TABLE after (id INTEGER PRIMARY KEY);\n",
    );
    assert_eq!(
        run.code,
        Some(1),
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("ERROR 0x03 no such table: nope"),
        "stderr: {}",
        run.stderr
    );
    assert!(!run.stdout.contains("ERROR"), "errors go to stderr");
    assert_eq!(
        run.stdout, "OK\n",
        "the statement after the error still ran"
    );
}

#[test]
fn a_semicolon_inside_quotes_does_not_end_the_statement_across_lines() {
    let (srv, _dir) = start(&[]);
    let run = cli(
        &srv,
        "CREATE TABLE q (id INTEGER PRIMARY KEY, v TEXT);\n\
         INSERT INTO q VALUES (1, 'a;\nb'';c;');\n\
         SELECT v FROM q\nWHERE id = 1\n;\n",
    );
    assert_eq!(
        run.code,
        Some(0),
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(run.stdout.contains("INSERT 1"), "stdout: {}", run.stdout);
    assert!(run.stdout.contains("(1 row)"), "stdout: {}", run.stdout);
    assert!(run.stdout.contains("b';c;"), "stdout: {}", run.stdout);
}

#[test]
fn backslash_q_ends_the_cli_and_nothing_after_it_runs() {
    let (srv, _dir) = start(&[]);
    let run = cli(
        &srv,
        "CREATE TABLE first (id INTEGER PRIMARY KEY);\n\\q\nCREATE TABLE second (id INTEGER PRIMARY KEY);\n",
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "OK\n");
    let mut s = open(srv.addr);
    let (code, _) = expect_error(&mut s, "SELECT * FROM second WHERE id = 1");
    assert_eq!(code, 0x03, "second must not exist");
}

#[test]
fn the_cli_exits_with_1_when_the_server_is_turned_away_by_the_connection_limit() {
    let (srv, _dir) = start(&["--max-connections", "1"]);
    let _held = open(srv.addr);
    let run = cli(&srv, "CREATE TABLE x (id INTEGER PRIMARY KEY);\n");
    assert_ne!(run.code, Some(0), "stdout: {}", run.stdout);
    assert!(!run.stdout.contains("OK"));
}
