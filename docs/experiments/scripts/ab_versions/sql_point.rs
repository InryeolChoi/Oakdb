//! `sql_point <dir> <mode> <n>`: one table of 1000 rows, then n
//! `SELECT * FROM t WHERE id = k` statements, printing the mean time per
//! statement in microseconds. `hit` looks up keys that exist, `miss` keys
//! that do not. Used for the A/B test of issue #32 (W9: a primary key
//! equality runs as a one-point range scan instead of `get`).
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (dir, mode, n): (&str, &str, usize) = (&args[1], &args[2], args[3].parse().unwrap());
    let db = oakdb::Db::open(std::path::Path::new(dir).join("db")).unwrap();
    oakdb_sql::execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    let rows: Vec<String> = (1..=1000).map(|i| format!("({i}, 'value number {i}')")).collect();
    oakdb_sql::execute(&db, &format!("INSERT INTO t VALUES {}", rows.join(", "))).unwrap();
    let mut x: u64 = 12345;
    let keys: Vec<i64> = (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let k = (x >> 33) as i64 % 1000 + 1;
            if mode == "miss" { k + 5000 } else { k }
        })
        .collect();
    let sqls: Vec<String> = keys.iter().map(|k| format!("SELECT * FROM t WHERE id = {k}")).collect();
    let start = Instant::now();
    for s in &sqls {
        std::hint::black_box(oakdb_sql::execute(&db, s).unwrap());
    }
    println!("{:.3}", start.elapsed().as_secs_f64() * 1e6 / n as f64);
}
