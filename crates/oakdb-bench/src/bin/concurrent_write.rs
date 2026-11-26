//! One round of the concurrent write benchmark (issue #30, design 0026),
//! printed as one line of `name=value` fields.
//!
//! ```text
//! cargo run --release -p oakdb-bench --bin concurrent_write -- \
//!     <dir> <system> <workload> <threads> [warmup s] [window s] [preload] [seed]
//! ```
//!
//! `system` is one of `oak`, `oak-off`, `oak-nosync`, `oak-sql`,
//! `sqlite-s1`, `sqlite-s2`, `sqlite-s3`; `workload` one of `w1`, `w2`,
//! `w3`. By default 0.5 s of warm-up, a 3 s window, 10 000 rows put in
//! first, and seed 1. The database goes in a fresh directory under `<dir>`,
//! removed afterwards.

use std::path::PathBuf;
use std::time::Duration;

use oakdb_bench::{run, Config};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!(
            "usage: concurrent_write <dir> <system> <workload> <threads> \
             [warmup s] [window s] [preload] [seed]"
        );
        std::process::exit(2);
    }
    let arg = |at: usize, default: &str| args.get(at).cloned().unwrap_or(default.into());
    let secs = |s: String| Duration::from_secs_f64(s.parse().expect("seconds"));
    let config = Config {
        system: args[1].parse().unwrap_or_else(|e: String| panic!("{e}")),
        workload: args[2].parse().unwrap_or_else(|e: String| panic!("{e}")),
        threads: args[3].parse().expect("threads is a number"),
        warmup: secs(arg(4, "0.5")),
        window: secs(arg(5, "3")),
        preload: arg(6, "10000").parse().expect("preload is a number"),
        seed: arg(7, "1").parse().expect("seed is a number"),
    };
    let out = run(&PathBuf::from(&args[0]), &config);
    println!("{}", out.line());
}
