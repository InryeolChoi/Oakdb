//! Fault experiment helper (docs/experiments/0001-single-page-storage.md).
//!
//! `commit_loop write <dir>` commits key i = value i (u32, big-endian) one
//! transaction at a time and prints i after each commit returns, for as
//! many keys as one page held before pages could split.
//! `commit_loop check <dir> <last>` checks that keys 0..=last are all there
//! with the right values and prints how many keys exist.

use oakdb::Db;

/// Entries of a 4-byte key and value that fill one leaf.
const ONE_LEAF: u32 = 253;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write") => write(&args[2]),
        Some("check") => check(&args[2], args[3].parse().unwrap()),
        _ => eprintln!("usage: commit_loop write <dir> | check <dir> <last>"),
    }
}

fn write(dir: &str) {
    let db = Db::open(dir).unwrap();
    for i in 0..ONE_LEAF {
        let mut txn = db.begin();
        txn.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
        txn.commit().unwrap();
        println!("{i}");
    }
}

fn check(dir: &str, last: u32) {
    let db = Db::open(dir).unwrap();
    let txn = db.begin();
    for i in 0..=last {
        let value = txn.get(&i.to_be_bytes()).unwrap();
        assert_eq!(value, Some(i.to_be_bytes().to_vec()), "key {i} lost");
    }
    let present = (0u32..1000)
        .filter(|i| txn.get(&i.to_be_bytes()).unwrap().is_some())
        .count();
    println!("ok: 0..={last} all present, {present} keys in total");
}
