//! The round trip of a one-row autocommit INSERT over loopback, for
//! docs/benchmarks/0009-session-autocommit-roundtrip.md (issue #37,
//! design 0025 Q12). Since #37 every statement runs through the
//! connection's `Session` instead of `oakdb_sql::execute`; this times the
//! path a statement outside BEGIN takes, before and after. It uses only
//! what the server had before #37, so the same file builds on both.
//!
//!     cargo run --release -p oakdb-server --example autocommit_roundtrip -- [mem|disk] [n]
//!
//! Prints the median, p90 and p99 of the time from sending the QUERY to
//! reading its AFFECTED, in microseconds.

use std::thread;
use std::time::{Duration, Instant};

use oakdb::fs::sim::SimFs;
use oakdb::fs::Fs;
use oakdb::Db;
use oakdb_server::client::Client;
use oakdb_server::server::{Config, Server};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let disk = args.first().map(String::as_str) == Some("disk");
    let n: usize = args
        .get(1)
        .map_or(if disk { 2_000 } else { 20_000 }, |a| a.parse().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let times = if disk {
        run(Db::open(dir.path()).unwrap(), n)
    } else {
        run(Db::open_with(SimFs::new(), "/oak").unwrap(), n)
    };
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e6;
    println!(
        "{} n={n} p50={:.1}us p90={:.1}us p99={:.1}us",
        if disk { "disk" } else { "mem" },
        at(0.5),
        at(0.9),
        at(0.99)
    );
}

fn run<F: Fs + 'static>(db: Db<F>, n: usize) -> Vec<Duration>
where
    Db<F>: Send + Sync,
{
    let server = Server::bind(
        db,
        Config {
            port: 0,
            ..Config::default()
        },
    )
    .unwrap();
    let addr = server.local_addr();
    thread::spawn(move || server.run());
    let mut client = Client::connect(addr).unwrap();
    client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    // Warm up: the first statements fill caches and grow buffers.
    for i in 0..200 {
        client
            .query(&format!("INSERT INTO t VALUES ({}, 'warm')", -1 - i))
            .unwrap();
    }
    let mut times: Vec<Duration> = (0..n)
        .map(|i| {
            let sql = format!("INSERT INTO t VALUES ({i}, 'row {i}')");
            let started = Instant::now();
            client.query(&sql).unwrap();
            started.elapsed()
        })
        .collect();
    times.sort();
    times
}
