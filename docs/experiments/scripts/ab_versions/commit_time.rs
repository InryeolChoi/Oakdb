//! Times commits through the public API only, so the same file builds
//! against any engine version (docs/experiments/scripts/ab_versions.py).
//!
//! `commit_time <dir> grow <n>`      n commits, each a new key with a
//!                                   1000-byte value: leaves split often
//! `commit_time <dir> overwrite <n>` n commits overwriting one small key
//! `commit_time <dir> reopen <n>`    n commits as in `grow`, then the
//!                                   database is dropped without closing,
//!                                   as a crash leaves it, and opened again
//! `commit_time <dir> closed <n>`    as `reopen`, but closed first, so
//!                                   opening finds an empty WAL
//! `commit_time <dir> begin_busy <n>` n read-only transactions (begin,
//!                                   one get, end) on one thread while
//!                                   another commits nonstop: how long a
//!                                   reader waits for others' commits
//!                                   (issue #27)
//! `commit_time <dir> threads <n>`   n commits of new small keys from 4
//!                                   threads at once (issue #27)
//!
//! Prints the mean and the median commit time in microseconds, or for
//! `reopen` the time opening took, twice; for `begin_busy`, the mean and
//! median time of a read-only transaction; for `threads`, the wall time
//! divided by n, twice.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use oakdb::Db;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (dir, mode, n) = (&args[1], args[2].as_str(), args[3].parse::<u32>().unwrap());
    let db = Db::open(dir).unwrap();
    if mode == "reopen" || mode == "closed" {
        for i in 0..n {
            let mut txn = db.begin();
            txn.put(&i.to_be_bytes(), &[i as u8; 1000]).unwrap();
            txn.commit().unwrap();
        }
        if mode == "closed" {
            db.close().unwrap();
        } else {
            drop(db);
        }
        let start = Instant::now();
        let db = Db::open(dir).unwrap();
        let us = start.elapsed().as_secs_f64() * 1e6;
        drop(db);
        println!("{us:.1} {us:.1}");
        return;
    }
    if mode == "begin_busy" {
        let mut txn = db.begin();
        txn.put(b"k", b"0").unwrap();
        txn.commit().unwrap();
        let done = AtomicBool::new(false);
        let mut times = Vec::with_capacity(n as usize);
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut i = 0u32;
                while !done.load(Ordering::Relaxed) {
                    let mut txn = db.begin();
                    txn.put(b"k", &i.to_be_bytes()).unwrap();
                    txn.commit().unwrap();
                    i += 1;
                }
            });
            for _ in 0..n {
                let start = Instant::now();
                let txn = db.begin();
                std::hint::black_box(txn.get(b"k").unwrap());
                drop(txn);
                times.push(start.elapsed().as_secs_f64() * 1e6);
            }
            done.store(true, Ordering::Relaxed);
        });
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        times.sort_by(f64::total_cmp);
        println!("{mean:.1} {:.1}", times[times.len() / 2]);
        return;
    }
    if mode == "threads" {
        const THREADS: u32 = 4;
        let start = Instant::now();
        std::thread::scope(|s| {
            for t in 0..THREADS {
                let db = &db;
                s.spawn(move || {
                    for i in (t..n).step_by(THREADS as usize) {
                        let mut txn = db.begin();
                        txn.put(&i.to_be_bytes(), b"v").unwrap();
                        txn.commit().unwrap();
                    }
                });
            }
        });
        let us = start.elapsed().as_secs_f64() * 1e6 / f64::from(n);
        println!("{us:.1} {us:.1}");
        return;
    }
    let mut times = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mut txn = db.begin();
        match mode {
            "grow" => txn.put(&i.to_be_bytes(), &[i as u8; 1000]).unwrap(),
            "overwrite" => txn.put(b"k", &i.to_be_bytes()).unwrap(),
            _ => panic!("mode is grow, overwrite, reopen, closed, begin_busy or threads"),
        }
        let start = Instant::now();
        txn.commit().unwrap();
        times.push(start.elapsed().as_secs_f64() * 1e6);
    }
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    println!("{mean:.1} {:.1}", times[times.len() / 2]);
}
