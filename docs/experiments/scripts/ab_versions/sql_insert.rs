//! `sql_insert <dir> insert <n>`: one table, then one
//! `INSERT INTO t VALUES (...), ...` of n rows (an integer key and a
//! 32-byte text), printing how long that one statement took in
//! microseconds, its commit included. Used for the before/after timing of
//! issue #35 (design 0023 section 10: INSERT now checks every row before
//! writing any; docs/benchmarks/0008-two-phase-insert.md).
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (dir, n): (&str, usize) = (&args[1], args[3].parse().unwrap());
    let db = oakdb::Db::open(std::path::Path::new(dir).join("db")).unwrap();
    oakdb_sql::execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)").unwrap();
    let rows: Vec<String> = (0..n).map(|i| format!("({i}, '{i:032}')")).collect();
    let sql = format!("INSERT INTO t VALUES {}", rows.join(", "));
    let start = Instant::now();
    std::hint::black_box(oakdb_sql::execute(&db, &sql).unwrap());
    println!("{:.3}", start.elapsed().as_secs_f64() * 1e6);
}
