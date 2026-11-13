//! Fault experiment helper (docs/experiments/0006-damaged-pages.md).
//!
//! `checksum_faults <dir>` damages a closed database file in seven ways,
//! one copy each, and reports what a fresh open makes of it: refused, or
//! opened, and then whether every key still reads its own value. It uses
//! only the public API and the file's bytes, so it runs the same with or
//! without page checksums.

use std::path::Path;

use oakdb::{Db, DATA_FILE};

const PAGE: usize = 4096;
/// Keys with 100-byte values: about 160 leaves, then a long value.
const KEYS: u32 = 400;
const LONG: usize = 20_000;

/// A way to damage a copy of the file, and its name.
type Damage = (&'static str, Box<dyn Fn(&mut Vec<u8>)>);

fn value(i: u32, version: u8) -> Vec<u8> {
    let mut v = vec![version; 100];
    v[..4].copy_from_slice(&i.to_le_bytes());
    v
}

fn build(dir: &Path) {
    let db = Db::open(dir).unwrap();
    let mut txn = db.begin();
    for i in 0..KEYS {
        txn.put(&i.to_be_bytes(), &value(i, 1)).unwrap();
    }
    txn.put(b"long", &vec![9; LONG]).unwrap();
    txn.put(b"gone", &vec![8; LONG]).unwrap();
    txn.commit().unwrap();
    let mut txn = db.begin();
    txn.delete(b"gone").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();
}

/// The page holding the bytes of key `i`'s value.
fn page_of_key(file: &[u8], i: u32) -> usize {
    let mut needle = i.to_le_bytes().to_vec();
    needle.extend_from_slice(&[1; 8]);
    file.windows(needle.len())
        .position(|w| w == needle)
        .unwrap()
        / PAGE
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("usage: checksum_faults <dir>");
    let root = Path::new(&root);
    let clean_dir = root.join("clean");
    build(&clean_dir);
    let clean = std::fs::read(clean_dir.join(DATA_FILE)).unwrap();
    let pages = clean.len() / PAGE;
    let leaf = page_of_key(&clean, 123);
    // The overflow page that holds the middle of the long value.
    let overflow = (0..pages)
        .find(|&p| clean[p * PAGE + 40..p * PAGE + 140].iter().all(|&b| b == 9))
        .unwrap();
    // A free page: all zero past its header.
    let free = (2..pages)
        .find(|&p| clean[p * PAGE + 40..(p + 1) * PAGE].iter().all(|&b| b == 0))
        .unwrap();

    // A newer version of `leaf`, from a commit that changed key 123.
    let newer_dir = root.join("newer");
    std::fs::create_dir_all(&newer_dir).unwrap();
    std::fs::write(newer_dir.join(DATA_FILE), &clean).unwrap();
    {
        let db = Db::open(&newer_dir).unwrap();
        let mut txn = db.begin();
        txn.put(&123u32.to_be_bytes(), &value(123, 2)).unwrap();
        txn.commit().unwrap();
    }
    let newer = std::fs::read(newer_dir.join(DATA_FILE)).unwrap();

    println!("file: {pages} pages; leaf of key 123: {leaf}; overflow: {overflow}; free: {free}");
    let cases: Vec<Damage> = vec![
        (
            "1 one byte of a value changes (experiment 0001)",
            Box::new(move |f| {
                let at = page_of_key(f, 123) * PAGE;
                let pos = f[at..at + PAGE]
                    .windows(12)
                    .position(|w| w[..4] == 123u32.to_le_bytes() && w[4..] == [1; 8])
                    .unwrap();
                f[at + pos + 8] = 7;
            }),
        ),
        (
            "2 torn page: first half new, second half old",
            Box::new(move |f| {
                let at = leaf * PAGE;
                f[at..at + PAGE / 2].copy_from_slice(&newer[at..at + PAGE / 2]);
            }),
        ),
        (
            "3 a valid page written in the wrong place",
            Box::new(move |f| {
                let other = if leaf + 1 < pages { leaf + 1 } else { leaf - 1 };
                let copy = f[other * PAGE..(other + 1) * PAGE].to_vec();
                f[leaf * PAGE..(leaf + 1) * PAGE].copy_from_slice(&copy);
            }),
        ),
        (
            "4 one bit flips in the long value's overflow page",
            Box::new(move |f| f[overflow * PAGE + 2000] ^= 0x01),
        ),
        (
            "5 one bit flips in a free page",
            Box::new(move |f| f[free * PAGE + 3000] ^= 0x01),
        ),
        (
            "6 one bit flips in meta slot A, past its fields",
            Box::new(|f| f[1000] ^= 0x01),
        ),
        (
            "7 one bit flips in meta slot B",
            Box::new(|f| f[PAGE + 1000] ^= 0x01),
        ),
    ];
    for (name, damage) in cases {
        let dir = root.join(name.split(' ').next().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let mut file = clean.clone();
        damage(&mut file);
        std::fs::write(dir.join(DATA_FILE), &file).unwrap();
        println!("== {name}");
        match Db::open(&dir) {
            Err(e) => println!("   open fails: {e}"),
            Ok(db) => {
                let txn = db.begin();
                let wrong = (0..KEYS)
                    .filter(|&i| txn.get(&i.to_be_bytes()).ok().flatten() != Some(value(i, 1)))
                    .count();
                let long_ok = txn.get(b"long").ok().flatten() == Some(vec![9; LONG]);
                println!(
                    "   opens; keys with a wrong or missing value: {wrong} of {KEYS}; long value intact: {long_ok}"
                );
            }
        }
    }
}
