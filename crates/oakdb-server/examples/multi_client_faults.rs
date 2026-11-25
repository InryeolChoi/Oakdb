//! Fault experiments and memory measurements for issue #37 (design 0025
//! Q12; docs/experiments/0019-multi-client-transactions.md). Build in
//! release mode with the `oakdb` binary:
//!
//!     cargo build --release -p oakdb-server --bin oakdb --example multi_client_faults
//!     target/release/examples/multi_client_faults kill [rounds]
//!     target/release/examples/multi_client_faults sigstop
//!     target/release/examples/multi_client_faults memory held|none [commits]
//!     target/release/examples/multi_client_faults write-set [mib]
//!
//! `kill` runs a real `oakdb serve` on a real directory and SIGKILLs it
//! while clients are in transactions; `sigstop` stops a real `oakdb cli`
//! in the middle of a transaction; `memory` and `write-set` measure the
//! server's RSS with an in-process server on an in-memory file system.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_server::client::{Client, ClientError};
use oakdb_server::server::{Config, Monitor, Server};
use oakdb_sql::Output;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize, default: u64| args.get(i).map_or(default, |a| a.parse().unwrap());
    match args.first().map(String::as_str) {
        Some("kill") => kill(n(1, 20)),
        Some("sigstop") => sigstop(),
        Some("memory") => memory(args.get(1).map(String::as_str) == Some("held"), n(2, 100_000)),
        Some("write-set") => write_set(n(1, 64)),
        _ => eprintln!("usage: multi_client_faults kill [rounds] | sigstop | memory held|none [commits] | write-set [mib]"),
    }
}

/// The `oakdb` binary next to this example's directory.
fn oakdb_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join("oakdb")
}

/// A pseudo-random number generator, so runs repeat (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn spawn_server(data: &Path, extra: &[&str]) -> (Child, u16) {
    let mut child = Command::new(oakdb_binary())
        .args(["serve", "--port", "0", "--data"])
        .arg(data)
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.as_mut().unwrap()),
        &mut line,
    )
    .unwrap();
    let port = line
        .split("listening on ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|addr| addr.rsplit(':').next())
        .unwrap_or_else(|| panic!("{line}"))
        .parse()
        .unwrap();
    (child, port)
}

fn connect(addr: impl std::net::ToSocketAddrs) -> Client {
    let client = Client::connect(addr).unwrap();
    client.set_timeout(Some(Duration::from_secs(30))).unwrap();
    client
}

/// How many rows `sql`, a SELECT, finds.
fn count(client: &mut Client, sql: &str) -> i64 {
    match client.query(sql).unwrap() {
        Output::Rows { rows, .. } => rows.len() as i64,
        other => panic!("{other:?}"),
    }
}

/// What one client of the `kill` experiment got done before the kill.
#[derive(Default, Clone, Copy)]
struct Progress {
    /// Transactions whose COMMIT was answered DONE.
    acknowledged: u64,
    /// Whether a COMMIT was sent and not answered: that transaction may
    /// be there or not, but whole.
    in_flight: bool,
}

const WRITERS: u64 = 8;
const ROWS_PER_TXN: u64 = 10;

/// Writers commit transactions of ten rows each; a ninth client holds a
/// transaction open the whole time. The server is killed with SIGKILL at
/// a random moment and started again on the same directory.
fn kill(rounds: u64) {
    let mut rng = Rng(0x0025);
    let mut totals = [0u64; 5];
    println!("round | acked | in flight | of those present | partial | open txn rows | ok");
    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let (mut server, port) = spawn_server(dir.path(), &[]);
        let addr = ("127.0.0.1", port);
        // id = writer * 1e9 + txn * ROWS_PER_TXN + row
        connect(addr)
            .query("CREATE TABLE t (id INTEGER PRIMARY KEY, w INTEGER, k INTEGER)")
            .unwrap();
        let mut holder = connect(addr);
        holder.query("BEGIN").unwrap();
        holder
            .query("INSERT INTO t VALUES (-1, -1, -1), (-2, -1, -1)")
            .unwrap();

        let progress: Vec<Arc<std::sync::Mutex<Progress>>> = (0..WRITERS)
            .map(|_| Arc::new(std::sync::Mutex::new(Progress::default())))
            .collect();
        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let progress = Arc::clone(&progress[w as usize]);
                thread::spawn(move || {
                    let Ok(mut client) = Client::connect(addr) else {
                        return;
                    };
                    for k in 0.. {
                        let rows: Vec<String> = (0..ROWS_PER_TXN)
                            .map(|r| {
                                format!("({}, {w}, {k})", w * 1_000_000_000 + k * ROWS_PER_TXN + r)
                            })
                            .collect();
                        let ok = client.query("BEGIN").is_ok()
                            && client
                                .query(&format!("INSERT INTO t VALUES {}", rows[..5].join(", ")))
                                .is_ok()
                            && client
                                .query(&format!("INSERT INTO t VALUES {}", rows[5..].join(", ")))
                                .is_ok();
                        if !ok {
                            return;
                        }
                        progress.lock().unwrap().in_flight = true;
                        match client.query("COMMIT") {
                            Ok(_) => {
                                let mut p = progress.lock().unwrap();
                                p.acknowledged += 1;
                                p.in_flight = false;
                            }
                            Err(ClientError::Server { .. }) => panic!("a commit failed"),
                            Err(_) => return,
                        }
                    }
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(100 + rng.below(1400)));
        // SAFETY: kill only sends a signal to our own child.
        unsafe { libc::kill(server.id() as libc::pid_t, libc::SIGKILL) };
        server.wait().unwrap();
        for w in writers {
            w.join().unwrap();
        }
        drop(holder);

        let (mut server, port) = spawn_server(dir.path(), &[]);
        let mut client = connect(("127.0.0.1", port));
        let mut acked = 0;
        let mut flying = 0;
        let mut extra = 0;
        let mut partial = 0;
        for (w, p) in progress.iter().enumerate() {
            let p = *p.lock().unwrap();
            acked += p.acknowledged;
            flying += u64::from(p.in_flight);
            // Rows of each transaction this writer may have committed.
            let mut k = 0u64;
            loop {
                let found = count(
                    &mut client,
                    &format!("SELECT id FROM t WHERE w = {w} AND k = {k}"),
                );
                if k < p.acknowledged {
                    if found != ROWS_PER_TXN as i64 {
                        partial += 1;
                    }
                } else if found == ROWS_PER_TXN as i64 && k == p.acknowledged && p.in_flight {
                    extra += 1;
                } else if found != 0 {
                    partial += 1;
                }
                if k > p.acknowledged {
                    break;
                }
                k += 1;
            }
        }
        let open_rows = count(&mut client, "SELECT id FROM t WHERE w = -1");
        let ok = partial == 0 && open_rows == 0;
        println!(
            "{round:>5} | {acked:>5} | {flying:>9} | {extra:>16} | {partial:>7} | {open_rows:>13} | {ok}"
        );
        totals[0] += acked;
        totals[1] += flying;
        totals[2] += extra;
        totals[3] += partial;
        totals[4] += open_rows as u64;
        let _ = server.kill();
        let _ = server.wait();
    }
    println!(
        "total | {:>5} | {:>9} | {:>16} | {:>7} | {:>13} |",
        totals[0], totals[1], totals[2], totals[3], totals[4]
    );
}

fn in_process(config: Config) -> (SocketAddr, Monitor<SimFs>) {
    let db = Db::open_with(SimFs::new(), "/oak").unwrap();
    let server = Server::bind(db, Config { port: 0, ..config }).unwrap();
    let addr = server.local_addr();
    let monitor = server.monitor();
    thread::spawn(move || server.run());
    (addr, monitor)
}

/// A real `oakdb cli` opens a transaction and is stopped with SIGSTOP: its
/// socket stays open and says nothing, as when a laptop sleeps. Another
/// connection keeps committing. The server's stats are printed every
/// quarter second.
fn sigstop() {
    let timeout = Duration::from_secs(2);
    let (addr, monitor) = in_process(Config {
        idle_in_transaction_timeout: Some(timeout),
        ..Config::default()
    });
    let mut writer = connect(addr);
    writer
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .unwrap();
    writer.query("INSERT INTO t VALUES (0, 0)").unwrap();

    let mut cli = Command::new(oakdb_binary())
        .args(["cli", "--port", &addr.port().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = cli.stdin.take().unwrap();
    stdin
        .write_all(b"BEGIN;\nINSERT INTO t VALUES (1, 1);\n")
        .unwrap();
    stdin.flush().unwrap();
    while monitor.stats().open_transactions == 0 {
        thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: signals to our own child.
    unsafe { libc::kill(cli.id() as libc::pid_t, libc::SIGSTOP) };
    let stopped = Instant::now();

    let stop = Arc::new(AtomicBool::new(false));
    let committer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut commits = 0u64;
            while !stop.load(Ordering::Relaxed) {
                writer.query("UPDATE t SET n = n + 1 WHERE id = 0").unwrap();
                commits += 1;
            }
            commits
        })
    };
    println!("t (s) | connections | open transactions | kept commits");
    let mut last = None;
    while stopped.elapsed() < timeout * 2 {
        let s = monitor.stats();
        let row = (monitor.connections(), s.open_transactions, s.kept_commits);
        println!(
            "{:>5.2} | {:>11} | {:>17} | {:>12}",
            stopped.elapsed().as_secs_f64(),
            row.0,
            row.1,
            row.2
        );
        last = Some(row);
        thread::sleep(Duration::from_millis(250));
    }
    stop.store(true, Ordering::Relaxed);
    let commits = committer.join().unwrap();
    println!("commits by the other connection meanwhile: {commits}; last {last:?}");

    // SAFETY: as above.
    unsafe { libc::kill(cli.id() as libc::pid_t, libc::SIGCONT) };
    stdin.write_all(b"SELECT * FROM t;\n").unwrap();
    drop(stdin);
    let out = cli.wait_with_output().unwrap();
    println!(
        "cli exit {:?}\nstdout:\n{}stderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut check = connect(addr);
    println!(
        "row 1 after: {:?}",
        check.query("SELECT * FROM t WHERE id = 1").unwrap()
    );
}

/// The process's resident memory in KiB, from `ps`.
fn rss_kib() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// One transaction stays open (`hold`) or none does while another
/// connection commits `commits` single-row UPDATEs spread over 10 000
/// rows; then it ends. The in-memory file system grows with the WAL in
/// both cases, so the two runs, each a process of its own, are compared.
fn memory(hold: bool, commits: u64) {
    {
        let (addr, monitor) = in_process(Config::default());
        let mut writer = connect(addr);
        writer
            .query("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, pad TEXT)")
            .unwrap();
        let pad = "x".repeat(100);
        for chunk in 0..100 {
            let rows: Vec<String> = (0..100)
                .map(|i| format!("({}, 0, '{pad}')", chunk * 100 + i))
                .collect();
            writer
                .query(&format!("INSERT INTO t VALUES {}", rows.join(", ")))
                .unwrap();
        }
        let case = if hold { "held" } else { "none" };
        let show = |step: &str, monitor: &Monitor<SimFs>| {
            let s = monitor.stats();
            println!(
                "{case} | {step} | {:.1} | {} | {}",
                rss_kib() as f64 / 1024.0,
                s.open_transactions,
                s.kept_commits
            );
        };
        show("10k rows loaded", &monitor);
        let mut holder = connect(addr);
        if hold {
            holder.query("BEGIN").unwrap();
            holder.query("SELECT id FROM t WHERE id = 0").unwrap();
        }
        let mut rng = Rng(7);
        let started = Instant::now();
        for i in 1..=commits {
            let id = rng.below(10_000);
            writer
                .query(&format!("UPDATE t SET n = n + 1 WHERE id = {id}"))
                .unwrap();
            if i % (commits / 4) == 0 {
                show(&format!("{i} commits"), &monitor);
            }
        }
        let secs = started.elapsed().as_secs_f64();
        if hold {
            holder.query("COMMIT").unwrap();
        }
        // One more commit lets pruning run.
        writer.query("UPDATE t SET n = 0 WHERE id = 0").unwrap();
        show("transaction ended", &monitor);
        println!(
            "{case} | {commits} commits in {secs:.1}s ({:.0}/s)",
            commits as f64 / secs
        );
    }
}

/// One transaction inserts `mib` MiB of 1 KiB rows without committing; RSS
/// is read every eighth of the way (design 0025 Q6).
fn write_set(mib: u64) {
    let (addr, monitor) = in_process(Config::default());
    let mut client = connect(addr);
    client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)")
        .unwrap();
    let before = rss_kib();
    println!("written MiB | RSS MiB | growth / written");
    client.query("BEGIN").unwrap();
    let hex = "ab".repeat(1024);
    let rows_per_stmt = 256; // 256 KiB per statement
    let statements = mib * 4;
    for s in 0..statements {
        let rows: Vec<String> = (0..rows_per_stmt)
            .map(|r| format!("({}, X'{hex}')", s * rows_per_stmt + r))
            .collect();
        client
            .query(&format!("INSERT INTO t VALUES {}", rows.join(", ")))
            .unwrap();
        if (s + 1) % (statements / 8).max(1) == 0 {
            let written = (s + 1) as f64 / 4.0;
            let grown = (rss_kib() - before) as f64 / 1024.0;
            println!(
                "{written:>11.0} | {:>7.1} | {:.2}",
                rss_kib() as f64 / 1024.0,
                grown / written
            );
        }
    }
    let started = Instant::now();
    client.query("ROLLBACK").unwrap();
    println!(
        "ROLLBACK took {:?}; RSS after {:.1} MiB; open {}",
        started.elapsed(),
        rss_kib() as f64 / 1024.0,
        monitor.stats().open_transactions
    );
}
