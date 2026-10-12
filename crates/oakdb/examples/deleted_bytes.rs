//! Fault experiment helper (docs/experiments/0003-deleted-bytes-on-disk.md).
//!
//! `deleted_bytes <dir>` stores a recognisable value, deletes it, and then
//! reports after each step whether its bytes can still be found in the data
//! file, even though the engine no longer returns it.

use oakdb::Db;

const SECRET: &[u8] = b"OAKDB-SECRET-VALUE-0123456789";

fn main() {
    let dir = std::env::args().nth(1).expect("usage: deleted_bytes <dir>");
    let db = Db::open(&dir).unwrap();

    let mut txn = db.begin();
    txn.put(b"secret", SECRET).unwrap();
    txn.put(b"other", &[b'.'; 100]).unwrap();
    txn.commit().unwrap();
    report(&db, &dir, "after put");

    let mut txn = db.begin();
    txn.delete(b"secret").unwrap();
    txn.commit().unwrap();
    report(&db, &dir, "after delete");

    // Small puts fit in the free space and do not compact the page.
    for i in 0u8..20 {
        let mut txn = db.begin();
        txn.put(&[b'k', i], &[i; 10]).unwrap();
        txn.commit().unwrap();
    }
    report(&db, &dir, "after 20 small puts");

    // The free gap is now 3504 bytes and the deleted cell left 39 bytes of
    // fragments, so a 3520 byte value only fits by compacting the page.
    let mut txn = db.begin();
    txn.put(b"big", &[b'#'; 3520]).unwrap();
    txn.commit().unwrap();
    report(&db, &dir, "after a put that compacts");
}

fn report(db: &Db, dir: &str, step: &str) {
    let visible = db.begin().get(b"secret").unwrap().is_some();
    let file = std::fs::read(std::path::Path::new(dir).join("oakdb.data")).unwrap();
    let on_disk = file.windows(SECRET.len()).any(|w| w == SECRET);
    println!("{step:28} get: {visible:5}  bytes in file: {on_disk}");
}
