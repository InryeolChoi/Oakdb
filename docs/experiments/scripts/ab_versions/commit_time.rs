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
//! `commit_time <dir> threads <n> [t]` n commits of new small keys from
//!                                   t threads at once (4 if not given),
//!                                   after a warm-up (issue #27)
//! `commit_time <dir> threads_tput <n> [t]` n commits of new small keys
//!                                   from each of t threads at once (4 if
//!                                   not given), after a warm-up: how many
//!                                   commits a second (issue #28)
//!
//! Prints the mean and the median commit time in microseconds, or for
//! `reopen` the time opening took, twice; for `begin_busy`, the mean and
//! median time of a read-only transaction; for `threads`, the mean and
//! median time of one commit, from asking to answer, over every thread.
//! `ab_versions.py` compares `threads` by the median: one slow sync that
//! every waiting thread sits through moves the mean of a round a lot.
//! For `threads_tput`, it prints commits a second, from the moment every
//! thread is let go to the last answer, twice: higher is better.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Barrier, Mutex};
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
    let threads: u32 = args.get(4).map_or(4, |t| t.parse().unwrap());
    if mode == "threads_tput" {
        const WARM_UP: u32 = 10;
        // The threads and this one, which starts the clock.
        let start = Barrier::new(threads as usize + 1);
        let mut began = Instant::now();
        std::thread::scope(|s| {
            for t in 0..threads {
                let (db, start) = (&db, &start);
                s.spawn(move || {
                    for i in 0..WARM_UP {
                        let mut txn = db.begin();
                        txn.put(&[0xff, t as u8, i as u8], b"warm").unwrap();
                        txn.commit().unwrap();
                    }
                    start.wait();
                    for i in 0..n {
                        let mut txn = db.begin();
                        txn.put(&[&t.to_be_bytes()[..], &i.to_be_bytes()].concat(), b"v")
                            .unwrap();
                        txn.commit().unwrap();
                    }
                });
            }
            start.wait();
            began = Instant::now();
        });
        let per_second = f64::from(n * threads) / began.elapsed().as_secs_f64();
        println!("{per_second:.1} {per_second:.1}");
        return;
    }
    if mode == "threads" {
        let threads_n = threads;
        // Untimed commits per thread first, so every thread is running and
        // the WAL file has grown before timing starts.
        const WARM_UP: u32 = 10;
        let start = Barrier::new(threads_n as usize);
        let times = Mutex::new(Vec::with_capacity(n as usize));
        std::thread::scope(|s| {
            for t in 0..threads_n {
                let (db, start, times) = (&db, &start, &times);
                s.spawn(move || {
                    for i in 0..WARM_UP {
                        let mut txn = db.begin();
                        txn.put(&[0xff, t as u8, i as u8], b"warm").unwrap();
                        txn.commit().unwrap();
                    }
                    start.wait();
                    let mut mine = Vec::new();
                    for i in (t..n).step_by(threads_n as usize) {
                        let mut txn = db.begin();
                        txn.put(&i.to_be_bytes(), b"v").unwrap();
                        let started = Instant::now();
                        txn.commit().unwrap();
                        mine.push(started.elapsed().as_secs_f64() * 1e6);
                    }
                    times.lock().unwrap().extend(mine);
                });
            }
        });
        let mut times = times.into_inner().unwrap();
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        times.sort_by(f64::total_cmp);
        println!("{mean:.1} {:.1}", times[times.len() / 2]);
        return;
    }
    let mut times = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mut txn = db.begin();
        match mode {
            "grow" => txn.put(&i.to_be_bytes(), &[i as u8; 1000]).unwrap(),
            "overwrite" => txn.put(b"k", &i.to_be_bytes()).unwrap(),
            _ => panic!("mode is grow, overwrite, reopen, closed, begin_busy, threads or threads_tput"),
        }
        let start = Instant::now();
        txn.commit().unwrap();
        times.push(start.elapsed().as_secs_f64() * 1e6);
    }
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    println!("{mean:.1} {:.1}", times[times.len() / 2]);
}
