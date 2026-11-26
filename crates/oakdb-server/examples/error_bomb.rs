//! The error bomb (issue #34, design 0027 E13, experiment 0020).
//!
//! Part A: one connection to a real server on a real disk gets a random mix
//! of statements, most of them failing, from every error group. Each
//! failing statement must come back with its group's code, the connection
//! must never close (no code 0x40 or above), and the normal statements in
//! between must answer as a model (a `BTreeMap`) says.
//!
//! Part B: the same mix inside BEGIN ... COMMIT on a SQL session, a few
//! dozen statements per transaction: a failing statement undoes only
//! itself (design 0023 T5), whatever its kind, so after COMMIT the table is
//! the model.
//!
//! `cargo run --release -p oakdb-server --example error_bomb -- [statements] [seed]`

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::thread;
use std::time::Instant;

use oakdb::Db;
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::{code, error_code};
use oakdb_server::server::{Config, Server};
use oakdb_sql::{Output, Session, Value};

/// A small xorshift generator: the run is the same for the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Statements that fail, and the code each must fail with. Run on
/// `t (id INTEGER PRIMARY KEY, name TEXT)` with any rows.
const BOMBS: &[(&str, u8)] = &[
    ("SELECT * FORM t", code::SYNTAX),
    ("SELECT * FROM t WHERE (id = 1", code::SYNTAX),
    ("INSERT INTO t VALUES (1, 'never closed)", code::SYNTAX),
    ("SELECT *\nFROM t\nWHERE id = = 1", code::SYNTAX),
    ("DROP TABLE t", code::UNSUPPORTED),
    ("SELECT COUNT(*) FROM t", code::UNSUPPORTED),
    ("SELECT t.id FROM t", code::UNSUPPORTED),
    (
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)",
        code::UNSUPPORTED,
    ),
    ("-- a comment\nSELECT * FROM t", code::UNSUPPORTED),
    ("SELECT * FROM t ORDER BY id", code::UNSUPPORTED),
    ("SELECT * FROM nope", code::NO_SUCH_TABLE),
    ("SELECT age FROM t", code::NO_SUCH_COLUMN),
    ("UPDATE t SET age = 1", code::NO_SUCH_COLUMN),
    (
        "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        code::TABLE_EXISTS,
    ),
    (
        "INSERT INTO t (id, id) VALUES (1, 1)",
        code::DUPLICATE_COLUMN,
    ),
    ("INSERT INTO t VALUES (1)", code::VALUE_COUNT),
    ("INSERT INTO t VALUES ('one', 'a')", code::TYPE_MISMATCH),
    ("SELECT * FROM t WHERE name = 1", code::TYPE_MISMATCH),
    ("SELECT * FROM t WHERE name + 1 = 2", code::TYPE_MISMATCH),
    ("DELETE FROM t WHERE id", code::TYPE_MISMATCH),
    ("UPDATE t SET name = id", code::TYPE_MISMATCH),
    (
        "SELECT * FROM t WHERE id = 99999999999999999999",
        code::ARITHMETIC,
    ),
    (
        "INSERT INTO t VALUES (-9223372036854775809, 'a')",
        code::ARITHMETIC,
    ),
    ("INSERT INTO t VALUES (NULL, 'a')", code::NULL_KEY),
    ("UPDATE t SET id = NULL WHERE id = 1", code::NULL_KEY),
];

/// What the model expects of a statement.
enum Expect {
    Error(u8),
    Out(Output),
}

/// The next statement and what it should give, changing `model` as the
/// statement would. Ids are 0..50, so inserts often hit a duplicate.
fn step(rng: &mut Rng, model: &mut BTreeMap<i64, String>) -> (String, Expect) {
    // Six in ten statements fail on purpose.
    if rng.below(10) < 6 {
        let (sql, c) = BOMBS[rng.below(BOMBS.len() as u64) as usize];
        // Every bomb fails whatever the rows: even `UPDATE t SET id = NULL
        // WHERE id = 1` with no row 1, since a NULL literal for the key is
        // refused before any row is read (design 0027 E1).
        return (sql.into(), Expect::Error(c));
    }
    let id = rng.below(50) as i64;
    match rng.below(4) {
        0 => {
            let name = format!("n{}", rng.below(1000));
            let sql = format!("INSERT INTO t VALUES ({id}, '{name}')");
            match model.entry(id) {
                Entry::Occupied(_) => (sql, Expect::Error(code::DUPLICATE_KEY)),
                Entry::Vacant(slot) => {
                    slot.insert(name);
                    (sql, Expect::Out(Output::Affected(1)))
                }
            }
        }
        1 => {
            let name = format!("u{}", rng.below(1000));
            let sql = format!("UPDATE t SET name = '{name}' WHERE id = {id}");
            let n = match model.get_mut(&id) {
                Some(old) => {
                    *old = name;
                    1
                }
                None => 0,
            };
            (sql, Expect::Out(Output::Affected(n)))
        }
        2 => {
            let sql = format!("DELETE FROM t WHERE id = {id}");
            let n = u64::from(model.remove(&id).is_some());
            (sql, Expect::Out(Output::Affected(n)))
        }
        _ => {
            let sql = format!("SELECT * FROM t WHERE id >= {id} AND id < {}", id + 5);
            (sql, Expect::Out(rows(model, id, id + 5)))
        }
    }
}

fn rows(model: &BTreeMap<i64, String>, from: i64, to: i64) -> Output {
    Output::Rows {
        columns: vec!["id".into(), "name".into()],
        rows: model
            .range(from..to)
            .map(|(id, name)| vec![Value::Integer(*id), Value::Text(name.clone())])
            .collect(),
    }
}

/// Counts of one part's run.
#[derive(Default, Debug)]
struct Tally {
    statements: u64,
    errors: BTreeMap<u8, u64>,
    ok: u64,
    wrong: Vec<String>,
}

fn part_a(statements: u64, seed: u64) -> Tally {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
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
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    let mut rng = Rng(seed);
    let mut model = BTreeMap::new();
    let mut tally = Tally::default();
    for _ in 0..statements {
        let (sql, expect) = step(&mut rng, &mut model);
        tally.statements += 1;
        match (client.query(&sql), expect) {
            (Err(ClientError::Server { code: got, message }), Expect::Error(want)) => {
                *tally.errors.entry(got).or_default() += 1;
                if got != want {
                    tally.wrong.push(format!(
                        "{sql:?}: 0x{got:02x} ({message}), want 0x{want:02x}"
                    ));
                }
            }
            (Ok(out), Expect::Out(want)) if out == want => tally.ok += 1,
            (got, Expect::Out(want)) => {
                tally.wrong.push(format!("{sql:?}: {got:?}, want {want:?}"))
            }
            (got, Expect::Error(want)) => tally
                .wrong
                .push(format!("{sql:?}: {got:?}, want 0x{want:02x}")),
        }
        if tally.wrong.len() > 20 {
            break;
        }
    }
    // The connection is still open and the table is the model.
    match client.query("SELECT * FROM t") {
        Ok(out) if out == rows(&model, i64::MIN, i64::MAX) => {}
        other => tally.wrong.push(format!("final table: {other:?}")),
    }
    tally
}

fn part_b(statements: u64, seed: u64) -> Tally {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let mut session = Session::new(&db);
    session
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    let mut rng = Rng(seed ^ 0x9e37_79b9_7f4a_7c15);
    let mut model = BTreeMap::new();
    let mut tally = Tally::default();
    let mut done = 0;
    while done < statements && tally.wrong.len() <= 20 {
        session.execute("BEGIN").unwrap();
        let length = 10 + rng.below(40);
        for _ in 0..length {
            let (sql, expect) = step(&mut rng, &mut model);
            tally.statements += 1;
            done += 1;
            match (session.execute(&sql), expect) {
                (Err(e), Expect::Error(want)) => {
                    let got = error_code(&e);
                    *tally.errors.entry(got).or_default() += 1;
                    if got != want {
                        tally.wrong.push(format!("{sql:?}: {e}, want 0x{want:02x}"));
                    }
                }
                (Ok(out), Expect::Out(want)) if out == want => tally.ok += 1,
                (got, _) => tally.wrong.push(format!("{sql:?}: {got:?}")),
            }
        }
        if let Err(e) = session.execute("COMMIT") {
            tally.wrong.push(format!("COMMIT: {e}"));
        }
        // A new session sees exactly the model.
        let mut reader = Session::new(&db);
        match reader.execute("SELECT * FROM t") {
            Ok(out) if out == rows(&model, i64::MIN, i64::MAX) => {}
            other => tally.wrong.push(format!("after COMMIT: {other:?}")),
        }
    }
    tally
}

fn report(name: &str, tally: &Tally, seconds: f64) {
    println!(
        "{name}: {} statements, {} answered as the model, {} errors in {seconds:.2}s",
        tally.statements,
        tally.ok,
        tally.errors.values().sum::<u64>()
    );
    for (c, n) in &tally.errors {
        println!("  0x{c:02x}: {n}");
    }
    if tally.errors.keys().any(|&c| c >= 0x40) {
        println!("  A CODE THAT CLOSES THE CONNECTION CAME BACK");
    }
    for w in &tally.wrong {
        println!("  WRONG {w}");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let statements: u64 = args.next().map_or(10_000, |a| a.parse().unwrap());
    let seed: u64 = args.next().map_or(34, |a| a.parse().unwrap());
    println!("seed {seed}");
    let start = Instant::now();
    let a = part_a(statements, seed.max(1));
    report("A server", &a, start.elapsed().as_secs_f64());
    let start = Instant::now();
    let b = part_b(statements, seed.max(1));
    report("B session", &b, start.elapsed().as_secs_f64());
    let pass = a.wrong.is_empty() && b.wrong.is_empty();
    println!("{}", if pass { "PASS" } else { "FAIL" });
    if !pass {
        std::process::exit(1);
    }
}
