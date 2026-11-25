//! The `oakdb` executable (design 0020, sections 6 to 8): `oakdb serve`
//! and `oakdb cli` run as child processes, plus how the CLI cuts input into
//! statements and prints results.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output as ProcessOutput, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use oakdb_server::cli::{format_output, Splitter};
use oakdb_server::protocol::{read_frame, write_frame, Request, Response, VERSION};

mod common;
use common::{first_line, output_within};
use oakdb_sql::{Output, Value};

#[test]
fn a_statement_ends_at_a_semicolon_outside_quotes() {
    let mut s = Splitter::new();
    assert!(s.push_line("INSERT INTO t VALUES").is_empty());
    assert!(!s.is_empty());
    assert_eq!(
        s.push_line("(1, 'it''s; fine');"),
        vec!["INSERT INTO t VALUES\n(1, 'it''s; fine')".to_string()]
    );
    assert!(s.is_empty());
}

#[test]
fn a_line_can_hold_several_statements_and_start_another() {
    let mut s = Splitter::new();
    assert_eq!(
        s.push_line("SELECT a FROM t WHERE id = 1; ; SELECT b FROM t WHERE id = 2; SELECT"),
        vec![
            "SELECT a FROM t WHERE id = 1".to_string(),
            "SELECT b FROM t WHERE id = 2".to_string(),
        ]
    );
    assert_eq!(
        s.push_line(" c FROM t WHERE id = 3;"),
        vec!["SELECT\n c FROM t WHERE id = 3".to_string()]
    );
}

#[test]
fn a_quote_left_open_carries_over_to_the_next_line() {
    let mut s = Splitter::new();
    assert!(s.push_line("INSERT INTO t VALUES (1, 'a;").is_empty());
    assert_eq!(
        s.push_line("b');"),
        vec!["INSERT INTO t VALUES (1, 'a;\nb')".to_string()]
    );
}

#[test]
fn what_is_left_at_the_end_is_the_last_statement() {
    let mut s = Splitter::new();
    assert!(s.push_line("SELECT a FROM t WHERE id = 1").is_empty());
    assert_eq!(s.finish(), Some("SELECT a FROM t WHERE id = 1".to_string()));
    assert_eq!(Splitter::new().finish(), None);
    let mut blank = Splitter::new();
    blank.push_line("   ");
    assert_eq!(blank.finish(), None);
}

#[test]
fn rows_print_as_a_table_with_a_count() {
    let out = Output::Rows {
        columns: vec!["id".into(), "name".into(), "photo".into()],
        rows: vec![
            vec![
                Value::Integer(2),
                Value::Text("it's; fine".into()),
                Value::Null,
            ],
            vec![
                Value::Integer(10),
                Value::Text("kim".into()),
                Value::Blob(vec![0xab, 0x01]),
            ],
        ],
    };
    assert_eq!(
        format_output("SELECT * FROM users WHERE id = 2", &out),
        " id | name       | photo\n\
         ----+------------+---------\n\
         \x20 2 | it's; fine | NULL\n\
         \x2010 | kim        | X'ab01'\n\
         (2 rows)\n"
    );
}

#[test]
fn one_row_and_no_rows_are_counted_in_words() {
    let one = Output::Rows {
        columns: vec!["v".into()],
        rows: vec![vec![Value::Integer(1)]],
    };
    assert!(format_output("SELECT", &one).ends_with("(1 row)\n"));
    let none = Output::Rows {
        columns: vec!["v".into()],
        rows: vec![],
    };
    assert_eq!(format_output("SELECT", &none), " v\n---\n(0 rows)\n");
}

#[test]
fn other_results_print_one_word() {
    assert_eq!(
        format_output("insert into t values (1), (2)", &Output::Affected(2)),
        "INSERT 2\n"
    );
    assert_eq!(
        format_output("CREATE TABLE t (id INTEGER PRIMARY KEY)", &Output::Done),
        "OK\n"
    );
}

/// A server child process, killed when dropped.
struct ServerProcess {
    child: Child,
    port: u16,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn oakdb() -> Command {
    Command::new(env!("CARGO_BIN_EXE_oakdb"))
}

fn serve(data: &std::path::Path) -> ServerProcess {
    let mut child = oakdb()
        .args(["serve", "--port", "0", "--data"])
        .arg(data)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let port = port_of(&first_line(&mut child));
    ServerProcess { child, port }
}

/// The port in the server's first line,
/// "oakdb 0.1.0 listening on 127.0.0.1:54321, data ...".
fn port_of(line: &str) -> u16 {
    let addr = line
        .split("listening on ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_else(|| panic!("unexpected first line: {line:?}"));
    assert!(addr.starts_with("127.0.0.1:"), "{line}");
    addr.rsplit(':').next().unwrap().parse().unwrap()
}

fn cli(port: u16, input: &str) -> ProcessOutput {
    output_within(oakdb().args(["cli", "--port", &port.to_string()]), input)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn the_cli_runs_a_piped_script_against_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path());
    let out = cli(
        server.port,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
         INSERT INTO users VALUES\n  (1, 'kim'), (2, 'lee');\n\
         SELECT * FROM users WHERE id = 2;\n",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "OK\nINSERT 2\n id | name\n----+------\n  2 | lee\n(1 row)\n"
    );
    assert_eq!(text(&out.stderr), "");
}

#[test]
fn an_error_in_a_script_is_printed_and_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path());
    let out = cli(
        server.port,
        "SELECT * FROM nope WHERE id = 1;\nCREATE TABLE t (id INTEGER PRIMARY KEY);\n",
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stderr), "ERROR 0x03 no such table: nope\n");
    // The script went on after the error.
    assert_eq!(text(&out.stdout), "OK\n");
}

#[test]
fn data_stays_when_the_server_is_killed_and_started_again() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(dir.path());
    let out = cli(
        server.port,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (7, 'kept');",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    drop(server);

    let server = serve(dir.path());
    let out = cli(server.port, "SELECT v FROM t WHERE id = 7;");
    assert_eq!(text(&out.stdout), " v\n------\n kept\n(1 row)\n");
}

#[test]
fn the_cli_says_when_it_cannot_connect() {
    // Bind and drop to find a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = cli(port, "");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("cannot connect"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn serve_refuses_to_start_on_a_bad_allowlist_line() {
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("allow");
    std::fs::write(&list, "10.0.0.5\n10.0.0.5/24\n").unwrap();
    let out = output_within(
        oakdb()
            .args(["serve", "--port", "0", "--data"])
            .arg(dir.path().join("db"))
            .arg("--allow-file")
            .arg(&list),
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains(":2:") && err.contains("10.0.0.5/24"), "{err}");
}

#[test]
fn serve_refuses_a_bad_allow_flag_and_needs_a_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = output_within(
        oakdb()
            .args(["serve", "--port", "0", "--allow", "::1", "--data"])
            .arg(dir.path()),
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("IPv6"), "{}", text(&out.stderr));

    let out = output_within(oakdb().args(["serve", "--port", "0"]), "");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("--data"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn an_unknown_subcommand_prints_the_usage() {
    let out = output_within(oakdb().arg("frobnicate"), "");
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(
        err.contains("oakdb serve") && err.contains("oakdb cli"),
        "{err}"
    );
}

#[test]
fn help_after_a_subcommand_prints_the_usage_and_succeeds() {
    for args in [
        &["serve", "--help"][..],
        &["cli", "--help"],
        &["serve", "--port", "0", "-h"],
        &["--help"],
    ] {
        let out = output_within(oakdb().args(args), "");
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        let usage = text(&out.stdout);
        assert!(usage.contains("NOT AUTHENTICATION"), "{args:?}: {usage}");
    }
}

#[test]
fn the_server_logs_each_connection_opening_and_closing() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = oakdb()
        .args(["serve", "--port", "0", "--data"])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let server = ServerProcess {
        port: port_of(&first_line(&mut child)),
        child,
    };
    let mut server = server;
    let (tx, rx) = mpsc::channel();
    let stderr = BufReader::new(server.child.stderr.take().unwrap());
    thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            tx.send(line).ok();
        }
    });

    let out = cli(server.port, "CREATE TABLE t (id INTEGER PRIMARY KEY);");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let deadline = Instant::now() + common::DEADLINE;
    let mut seen = Vec::new();
    while !seen.iter().any(|l: &String| l.ends_with(" disconnected")) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => panic!("no closing line within the deadline: {seen:?}"),
        }
    }
    let opened = seen.iter().find(|l| l.ends_with(" connected"));
    let opened = opened.unwrap_or_else(|| panic!("no opening line: {seen:?}"));
    assert!(opened.starts_with("oakdb: 127.0.0.1:"), "{opened}");
}

/// Runs `command` with its soft limit on open files set to `soft` (and the
/// hard limit to `hard`, if given), as a shell's `ulimit -n` would.
fn with_fd_limit(command: &mut Command, soft: u64, hard: Option<u64>) -> &mut Command {
    // SAFETY: the closure runs in the child between fork and exec and only
    // calls getrlimit and setrlimit, which are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = soft as libc::rlim_t;
            if let Some(hard) = hard {
                limit.rlim_max = hard as libc::rlim_t;
            }
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

#[test]
fn a_server_started_with_a_soft_limit_of_256_files_takes_300_clients() {
    // macOS starts processes at 256; the server must raise it itself
    // (review S14: on a machine with a high default, nothing else checks
    // this).
    oakdb_server::rlimit::raise_fd_limit(2048).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut child = with_fd_limit(
        oakdb()
            .args(["serve", "--port", "0", "--data"])
            .arg(dir.path()),
        256,
        None,
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let server = ServerProcess {
        port: port_of(&first_line(&mut child)),
        child,
    };

    let streams: Vec<TcpStream> = (0..300)
        .map(|i| {
            let mut s = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            write_frame(&mut s, &Request::Hello { version: VERSION }.encode()).unwrap();
            let ready = read_frame(&mut s)
                .unwrap_or_else(|e| panic!("client {i}: {e}"))
                .unwrap_or_else(|| panic!("client {i}: closed"));
            assert_eq!(
                Response::decode(ready).unwrap(),
                Response::Ready { version: VERSION },
                "client {i}"
            );
            s
        })
        .collect();
    assert_eq!(streams.len(), 300);
}

#[test]
fn a_server_whose_hard_limit_is_too_low_refuses_to_start_and_says_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let out = output_within(
        with_fd_limit(
            oakdb()
                .args(["serve", "--port", "0", "--data"])
                .arg(dir.path()),
            300,
            Some(300),
        ),
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("ulimit -n") && err.contains("--max-connections"),
        "{err}"
    );
}

// --- Transactions in the CLI (issue #37, design 0025 Q9) ---

/// A server in this process on an in-memory database, for `cli::run`.
fn in_process(config: oakdb_server::server::Config) -> std::net::SocketAddr {
    let db = oakdb::Db::open_with(oakdb::fs::sim::SimFs::new(), "/oak").unwrap();
    let server =
        oakdb_server::server::Server::bind(db, oakdb_server::server::Config { port: 0, ..config })
            .unwrap();
    let addr = server.local_addr();
    thread::spawn(move || server.run());
    addr
}

/// Runs `cli::run` as a person would, prompts included, on `input`.
fn run_cli(addr: std::net::SocketAddr, input: impl BufRead) -> (u8, String, String) {
    let mut client = oakdb_server::client::Client::connect(addr).unwrap();
    client.set_timeout(Some(Duration::from_secs(10))).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = oakdb_server::cli::run(&mut client, input, &mut out, &mut err, true).unwrap();
    (code, text(&out), text(&err))
}

#[test]
fn the_prompt_shows_a_star_while_a_transaction_is_open() {
    let addr = in_process(Default::default());
    let input = "CREATE TABLE t (id INTEGER PRIMARY KEY);\nBEGIN;\nINSERT INTO t VALUES (1);\n\
                 BEGIN;\nCOMMIT;\nSELECT * FROM t;\n";
    let (code, out, err) = run_cli(addr, input.as_bytes());
    // BEGIN twice is an error, so the run fails, but the transaction lives.
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("ERROR 0x0b"), "{err}");
    // One prompt per statement and one at the end of input, in order;
    // errors go to `err`, so some share a line.
    let prompts: Vec<&str> = out
        .match_indices("oakdb")
        .map(|(at, _)| {
            if out[at..].starts_with("oakdb*> ") {
                "*"
            } else {
                ">"
            }
        })
        .collect();
    assert_eq!(prompts, [">", ">", "*", "*", "*", ">", ">"], "{out}");
    assert!(!err.contains("rolled back"), "{err}");
}

#[test]
fn leaving_inside_a_transaction_says_it_is_rolled_back() {
    let addr = in_process(Default::default());
    let input = "CREATE TABLE t (id INTEGER PRIMARY KEY);\nBEGIN;\nINSERT INTO t VALUES (1);\n";
    let (code, _, err) = run_cli(addr, input.as_bytes());
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("rolled back"), "{err}");
    // And it was.
    let (_, out, _) = run_cli(addr, "SELECT * FROM t;\n".as_bytes());
    assert!(out.contains("(0 rows)"), "{out}");
}

/// Input that hands over its first part, then waits `pause` before the
/// rest: a person who stops typing in the middle of a transaction.
struct Pausing {
    parts: Vec<(Duration, Vec<u8>)>,
}

impl std::io::Read for Pausing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some((pause, mut part)) = (!self.parts.is_empty()).then(|| self.parts.remove(0)) else {
            return Ok(0);
        };
        thread::sleep(pause);
        let n = part.len().min(buf.len());
        buf[..n].copy_from_slice(&part[..n]);
        if n < part.len() {
            self.parts.insert(0, (Duration::ZERO, part.split_off(n)));
        }
        Ok(n)
    }
}

#[test]
fn when_the_server_closes_on_a_transaction_the_cli_says_it_was_rolled_back_and_exits_1() {
    let addr = in_process(oakdb_server::server::Config {
        idle_in_transaction_timeout: Some(Duration::from_millis(100)),
        ..Default::default()
    });
    let input = Pausing {
        parts: vec![
            (
                Duration::ZERO,
                b"CREATE TABLE t (id INTEGER PRIMARY KEY);\nBEGIN;\nINSERT INTO t VALUES (1);\n"
                    .to_vec(),
            ),
            (Duration::from_millis(600), b"COMMIT;\nSELECT 1;\n".to_vec()),
        ],
    };
    let (code, _, err) = run_cli(addr, BufReader::new(input));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("ERROR 0x47"), "{err}");
    assert!(
        err.contains("connection closed by server; the open transaction was rolled back"),
        "{err}"
    );
    let (_, out, _) = run_cli(addr, "SELECT * FROM t;\n".as_bytes());
    assert!(out.contains("(0 rows)"), "{out}");
}

#[test]
fn the_server_logs_an_old_transaction_and_an_idle_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = oakdb()
        .args([
            "serve",
            "--port",
            "0",
            "--idle-in-transaction-timeout",
            "0.5",
            "--warn-transaction-age",
            "0.1",
            "--data",
        ])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = ServerProcess {
        port: port_of(&first_line(&mut child)),
        child,
    };
    let (tx, rx) = mpsc::channel();
    let stderr = BufReader::new(server.child.stderr.take().unwrap());
    thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            tx.send(line).ok();
        }
    });

    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream.set_read_timeout(Some(common::DEADLINE)).unwrap();
    write_frame(&mut stream, &Request::Hello { version: VERSION }.encode()).unwrap();
    read_frame(&mut stream).unwrap().unwrap();
    write_frame(&mut stream, &Request::Query("BEGIN".into()).encode()).unwrap();
    assert_eq!(
        Response::decode(read_frame(&mut stream).unwrap().unwrap()).unwrap(),
        Response::Done
    );
    // Nothing more: the warning comes first, then the close.
    let deadline = Instant::now() + common::DEADLINE;
    let mut seen = Vec::new();
    while !seen
        .iter()
        .any(|l: &String| l.contains("rolled back and closed"))
    {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => panic!("no idle line within the deadline: {seen:?}"),
        }
    }
    let warned = seen
        .iter()
        .position(|l| l.contains("warning") && l.contains("transaction open for"));
    let closed = seen
        .iter()
        .position(|l| l.contains("idle in a transaction for 0.5s; rolled back and closed"));
    assert!(
        matches!((warned, closed), (Some(w), Some(c)) if w < c),
        "{seen:?}"
    );
}
