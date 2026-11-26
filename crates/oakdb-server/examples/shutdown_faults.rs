//! Fault experiments and reference measurements for graceful shutdown
//! (issue #38, design 0028 Q13; docs/experiments/0021-graceful-shutdown.md,
//! docs/benchmarks/0012-graceful-shutdown.md). Build in release mode with
//! the `oakdb` binary:
//!
//!     cargo build --release -p oakdb-server --bin oakdb --example shutdown_faults
//!     target/release/examples/shutdown_faults sigterm [rounds]
//!     target/release/examples/shutdown_faults kill9 [rounds]
//!     target/release/examples/shutdown_faults second [rounds]
//!     target/release/examples/shutdown_faults tick [connections] [seconds]
//!     target/release/examples/shutdown_faults restart [rounds]
//!
//! Every mode runs a real `oakdb serve` on a real directory and sends it
//! real signals.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::{code, read_frame, write_frame, Request, VERSION};
use oakdb_sql::{Output, Value};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize, default: u64| args.get(i).map_or(default, |a| a.parse().unwrap());
    match args.first().map(String::as_str) {
        Some("sigterm") => sigterm(n(1, 100)),
        Some("kill9") => kill9(n(1, 50)),
        Some("second") => second(n(1, 20)),
        Some("tick") => tick(n(1, 512) as usize, n(2, 10)),
        Some("restart") => restart(n(1, 5)),
        _ => eprintln!(
            "usage: shutdown_faults sigterm [rounds] | kill9 [rounds] | second [rounds] | \
             tick [connections] [seconds] | restart [rounds]"
        ),
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

/// `oakdb serve`, its port, and the lines it logs.
struct Serving {
    child: Child,
    port: u16,
    log: mpsc::Receiver<String>,
    /// From the spawn to the "listening" line.
    started_in: Duration,
}

impl Serving {
    fn start(data: &Path, extra: &[&str]) -> Serving {
        let begun = Instant::now();
        let mut child = Command::new(oakdb_binary())
            .args(["serve", "--port", "0", "--data"])
            .arg(data)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        let started_in = begun.elapsed();
        thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
        let port = line
            .split("listening on ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .and_then(|addr| addr.rsplit(':').next())
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {line:?}"));
        let (tx, log) = mpsc::channel();
        let err = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Serving {
            child,
            port,
            log,
            started_in,
        }
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: kill only sends a signal to our own child.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, signal);
        }
    }

    fn wait_for_log(&self, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match self.log.recv_timeout(Duration::from_millis(100)) {
                Ok(line) if line.contains(what) => return,
                _ => {}
            }
        }
        panic!("the log never said {what:?}");
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the server did not exit");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn client(&self) -> Client {
        let c = Client::connect(("127.0.0.1", self.port)).unwrap();
        c.set_timeout(Some(Duration::from_secs(20))).unwrap();
        c
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ids(client: &mut Client) -> BTreeSet<i64> {
    match client.query("SELECT id FROM t").unwrap() {
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
    std::fs::metadata(data.join(oakdb::WAL_FILE)).map_or(0, |m| m.len())
}

/// What the load's requests came to.
#[derive(Default)]
struct Tally {
    committed: Vec<i64>,
    refused: Vec<i64>,
    /// Requests cut off with no answer.
    cut: usize,
    /// Commits whose answer never came: may or may not be there.
    unknown: Vec<i64>,
    other: Vec<String>,
    requests: u64,
}

/// Each client writes ids of its own, alone and in transactions of two,
/// until the server closes it.
fn load(port: u16, client: i64, tally: &Mutex<Tally>) {
    let Ok(mut c) = Client::connect(("127.0.0.1", port)) else {
        return;
    };
    c.set_timeout(Some(Duration::from_secs(30))).unwrap();
    let fate = |result: Result<Output, ClientError>, ids: &[i64], commit: bool| {
        let mut t = tally.lock().unwrap();
        t.requests += 1;
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
                if commit {
                    t.unknown.extend_from_slice(ids);
                }
                false
            }
            Err(e) => {
                t.other.push(e.to_string());
                false
            }
        }
    };
    let mut next = client * 10_000_000;
    loop {
        next += 1;
        if next % 3 == 0 {
            let ids = [next, next + 1];
            next += 1;
            if !fate(c.query("BEGIN"), &ids, false) {
                return;
            }
            for id in ids {
                if !fate(
                    c.query(&format!("INSERT INTO t VALUES ({id})")),
                    &ids,
                    false,
                ) {
                    return;
                }
            }
            if !fate(c.query("COMMIT"), &ids, true) {
                return;
            }
            tally.lock().unwrap().committed.extend_from_slice(&ids);
        } else {
            if !fate(
                c.query(&format!("INSERT INTO t VALUES ({next})")),
                &[next],
                true,
            ) {
                return;
            }
            tally.lock().unwrap().committed.push(next);
        }
    }
}

fn run_load(server: &Serving, clients: i64) -> (Arc<Mutex<Tally>>, Vec<thread::JoinHandle<()>>) {
    let tally = Arc::new(Mutex::new(Tally::default()));
    let handles = (1..=clients)
        .map(|client| {
            let tally = Arc::clone(&tally);
            let port = server.port;
            thread::spawn(move || load(port, client, &tally))
        })
        .collect();
    (tally, handles)
}

/// Experiment 1: SIGTERM at a random moment under load from 8 clients.
fn sigterm(rounds: u64) {
    let mut rng = Rng(0x0026_0001);
    let (mut lost, mut written, mut cut, mut unclean) = (0, 0, 0, 0);
    let (mut commits, mut refused, mut requests) = (0, 0, 0u64);
    let mut took = Vec::new();
    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Serving::start(dir.path(), &["--shutdown-grace", "2"]);
        server
            .client()
            .query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let (tally, handles) = run_load(&server, 8);
        thread::sleep(Duration::from_millis(50 + rng.below(450)));
        let signalled = Instant::now();
        server.signal(libc::SIGTERM);
        let status = server.wait();
        took.push(signalled.elapsed());
        for h in handles {
            h.join().unwrap();
        }
        if status.code() != Some(0) || wal_len(dir.path()) != 20 {
            unclean += 1;
            println!("round {round}: {status:?} signal {:?}", status.signal());
        }
        let restarted = Serving::start(dir.path(), &[]);
        let present = ids(&mut restarted.client());
        let t = tally.lock().unwrap();
        lost += t
            .committed
            .iter()
            .filter(|id| !present.contains(id))
            .count();
        written += t.refused.iter().filter(|id| present.contains(id)).count();
        cut += t.cut;
        commits += t.committed.len();
        refused += t.refused.len();
        requests += t.requests;
        assert!(t.other.is_empty(), "round {round}: {:?}", t.other);
    }
    took.sort();
    println!(
        "sigterm: {rounds} rounds, {requests} requests, {commits} ids committed, {refused} ids \
         answered 0x49; exit != 0 or WAL not empty: {unclean}; answered commits lost: {lost}; \
         0x49 ids written: {written}; requests cut without an answer: {cut}"
    );
    println!(
        "shutdown took (signal to exit): min {:?}, median {:?}, max {:?}",
        took[0],
        took[took.len() / 2],
        took[took.len() - 1]
    );
}

/// Experiment 2: SIGTERM, then SIGKILL at a random moment of the shutdown.
fn kill9(rounds: u64) {
    let mut rng = Rng(0x0026_0002);
    let (mut lost, mut opened, mut cut, mut unknown_kept) = (0, 0, 0, 0usize);
    let mut unknown_total = 0;
    for _ in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Serving::start(dir.path(), &["--shutdown-grace", "0.3"]);
        server
            .client()
            .query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        // A transaction kept open, so the grace is part of the shutdown.
        let mut open = server.client();
        open.query("BEGIN").unwrap();
        open.query("INSERT INTO t VALUES (-1)").unwrap();
        let (tally, handles) = run_load(&server, 8);
        thread::sleep(Duration::from_millis(100 + rng.below(200)));
        server.signal(libc::SIGTERM);
        // Somewhere in the grace (0-300 ms) or the checkpoint after it.
        thread::sleep(Duration::from_micros(rng.below(450_000)));
        server.signal(libc::SIGKILL);
        server.wait();
        for h in handles {
            h.join().unwrap();
        }
        let restarted = Serving::start(dir.path(), &[]);
        opened += 1;
        let present = ids(&mut restarted.client());
        let t = tally.lock().unwrap();
        lost += t
            .committed
            .iter()
            .filter(|id| !present.contains(id))
            .count();
        cut += t.cut;
        unknown_total += t.unknown.len();
        unknown_kept += t.unknown.iter().filter(|id| present.contains(id)).count();
        assert!(!present.contains(&-1), "an open transaction survived");
    }
    println!(
        "kill9: {rounds} rounds, restarted {opened}; answered commits lost: {lost}; \
         requests cut without an answer: {cut}; of {unknown_total} ids whose commit got no \
         answer, {unknown_kept} are there"
    );
}

/// Experiment 3: a second SIGINT while a transaction holds the grace.
fn second(rounds: u64) {
    let (mut exits, mut intact) = (BTreeSet::new(), 0);
    for _ in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Serving::start(dir.path(), &["--shutdown-grace", "60"]);
        let mut a = server.client();
        a.query("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
        a.query("INSERT INTO t VALUES (1)").unwrap();
        let mut b = server.client();
        b.query("BEGIN").unwrap();
        b.query("INSERT INTO t VALUES (2)").unwrap();
        server.signal(libc::SIGINT);
        server.wait_for_log("shutting down:");
        let second = Instant::now();
        server.signal(libc::SIGINT);
        let status = server.wait();
        let gone = second.elapsed();
        exits.insert((status.code(), gone.as_millis() < 1000));
        let restarted = Serving::start(dir.path(), &[]);
        if ids(&mut restarted.client()) == BTreeSet::from([1]) {
            intact += 1;
        }
    }
    println!(
        "second: {rounds} rounds; (exit code, gone within 1 s): {exits:?}; restarted with \
         exactly the answered commit: {intact}"
    );
}

/// CPU the children waited for so far have used, in seconds.
fn children_cpu() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage fills the struct it is given.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr());
        usage.assume_init()
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// Reference 1: what the tick costs, with `connections` idle connections
/// for `seconds`, against none.
fn tick(connections: usize, seconds: u64) {
    for count in [0, connections] {
        let before = children_cpu();
        let dir = tempfile::tempdir().unwrap();
        let mut server = Serving::start(
            dir.path(),
            &["--max-connections", &(connections + 8).to_string()],
        );
        let idle: Vec<TcpStream> = (0..count)
            .map(|_| {
                let mut s = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
                write_frame(&mut s, &Request::Hello { version: VERSION }.encode()).unwrap();
                read_frame(&mut s).unwrap().unwrap();
                s
            })
            .collect();
        thread::sleep(Duration::from_secs(seconds));
        let signalled = Instant::now();
        server.signal(libc::SIGTERM);
        server.wait();
        let shutdown = signalled.elapsed();
        drop(idle);
        let cpu = children_cpu() - before;
        println!(
            "tick: {count} idle connections for {seconds}s: server CPU {cpu:.3}s \
             ({:.2}% of one core); SIGTERM to exit {shutdown:?}",
            cpu / seconds as f64 * 100.0
        );
    }
}

/// Reference 2: restart time after a clean shutdown and after SIGKILL,
/// with nearly 4 MiB in the WAL (just under the automatic checkpoint).
fn restart(rounds: u64) {
    let blob = "ab".repeat(2000);
    for how in ["SIGTERM", "SIGKILL"] {
        let mut times = Vec::new();
        let mut wal = 0;
        for _ in 0..rounds {
            let dir = tempfile::tempdir().unwrap();
            let mut server = Serving::start(dir.path(), &[]);
            let mut c = server.client();
            c.query("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)")
                .unwrap();
            let mut id = 0;
            // Stop a commit short of the 4 MiB that checkpoints by itself.
            while wal_len(dir.path()) < 3_600_000 {
                id += 1;
                c.query(&format!("INSERT INTO t VALUES ({id}, X'{blob}')"))
                    .unwrap();
            }
            wal = wal_len(dir.path());
            drop(c);
            server.signal(if how == "SIGTERM" {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            });
            server.wait();
            let restarted = Serving::start(dir.path(), &[]);
            times.push(restarted.started_in);
        }
        times.sort();
        println!(
            "restart after {how} (WAL {wal} bytes before): min {:?}, median {:?}, max {:?}",
            times[0],
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }
}
