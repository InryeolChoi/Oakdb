//! Commits a second from many threads, Oakdb against SQLite (issue #30,
//! design 0026).
//!
//! [`run`] measures one round: it opens a fresh database in a directory,
//! puts in [`Config::preload`] rows, starts [`Config::threads`] threads
//! that each commit one transaction after another, and counts the commits
//! answered inside a time window that starts after a warm-up. Every system
//! gets the same keys, from the same seed, and the same rows to start
//! with. The binary `concurrent_write` prints one round as a line;
//! `docs/experiments/scripts/bench_concurrent.py` runs the rounds and does
//! the statistics.

use std::fmt;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Barrier, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use oakdb::fs::{Fs, FsFile, StdFile, StdFs};
use oakdb::{Db, Error, Stats, BATCH_SIZE_BUCKETS, MAX_BATCH};
use rusqlite::{Connection, ErrorCode};

/// What is measured: a database and how it is set up (design 0026 Q5-Q8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// Oakdb's key-value API, group commit on (batches of [`MAX_BATCH`]).
    Oak,
    /// Oakdb with batches of one commit: group commit off.
    OakOff,
    /// Oakdb, group commit on, with syncs that do nothing. Not durable: the
    /// ceiling of the engine when a sync is free, a reference line only.
    OakNoSync,
    /// Oakdb through its SQL layer: an autocommitted INSERT a transaction.
    OakSql,
    /// SQLite, a connection a thread, `BEGIN IMMEDIATE` and a busy
    /// timeout, with WAL, `synchronous=FULL` and `fullfsync=ON`.
    SqliteS1,
    /// SQLite with S1's settings, through one connection the threads take
    /// turns at under a mutex: SQLite's ceiling, as no thread sleeps.
    SqliteS2,
    /// S1 with SQLite's default settings, whose sync on macOS does not
    /// flush the drive's cache. Not as durable: a reference only.
    SqliteS3,
}

impl System {
    /// The name the binary takes and prints.
    pub fn name(self) -> &'static str {
        match self {
            System::Oak => "oak",
            System::OakOff => "oak-off",
            System::OakNoSync => "oak-nosync",
            System::OakSql => "oak-sql",
            System::SqliteS1 => "sqlite-s1",
            System::SqliteS2 => "sqlite-s2",
            System::SqliteS3 => "sqlite-s3",
        }
    }
}

impl FromStr for System {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        [
            System::Oak,
            System::OakOff,
            System::OakNoSync,
            System::OakSql,
            System::SqliteS1,
            System::SqliteS2,
            System::SqliteS3,
        ]
        .into_iter()
        .find(|system| system.name() == s)
        .ok_or_else(|| format!("unknown system {s}"))
    }
}

/// What each transaction does (design 0026 Q2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// W1: puts a new random 16-byte key with a 100-byte value.
    Distinct,
    /// W2: as W1 with a 4 KiB value, which goes to an overflow chain.
    Large,
    /// W3: reads one of 16 counters and writes it back plus one, again
    /// from the start after a conflict.
    Hot,
}

impl Workload {
    /// The name the binary takes and prints.
    pub fn name(self) -> &'static str {
        match self {
            Workload::Distinct => "w1",
            Workload::Large => "w2",
            Workload::Hot => "w3",
        }
    }

    fn value_len(self) -> usize {
        match self {
            Workload::Large => 4096,
            Workload::Distinct | Workload::Hot => 100,
        }
    }
}

impl FromStr for Workload {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        [Workload::Distinct, Workload::Large, Workload::Hot]
            .into_iter()
            .find(|workload| workload.name() == s)
            .ok_or_else(|| format!("unknown workload {s}"))
    }
}

/// One round.
#[derive(Debug, Clone)]
pub struct Config {
    pub system: System,
    pub workload: Workload,
    pub threads: usize,
    /// How long the threads commit before counting starts.
    pub warmup: Duration,
    /// How long commits are counted.
    pub window: Duration,
    /// Rows put in before the threads start, so the tree has two or three
    /// levels (design 0026 Q2: 10 000).
    pub preload: usize,
    /// Where the keys come from: the same seed gives every system the
    /// same keys.
    pub seed: u64,
}

/// What the engine counted in the window: the difference of two
/// [`Db::stats`], but for `largest_batch`, the largest of the round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineCounts {
    pub batches: u64,
    pub wal_syncs: u64,
    pub conflicts: u64,
    pub largest_batch: u64,
    pub batch_sizes: [u64; BATCH_SIZE_BUCKETS],
    pub wal_write_nanos: u64,
}

impl EngineCounts {
    fn between(before: Stats, after: Stats) -> Self {
        let mut batch_sizes = [0; BATCH_SIZE_BUCKETS];
        for (at, size) in batch_sizes.iter_mut().enumerate() {
            *size = after.batch_sizes[at] - before.batch_sizes[at];
        }
        EngineCounts {
            batches: after.batches - before.batches,
            wal_syncs: after.wal_syncs - before.wal_syncs,
            conflicts: after.conflicts - before.conflicts,
            largest_batch: after.largest_batch,
            batch_sizes,
            wal_write_nanos: after.wal_write_nanos - before.wal_write_nanos,
        }
    }
}

/// What one round measured.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub config: Config,
    /// Transactions answered with success inside the window.
    pub commits: u64,
    /// Every transaction answered with success, warm-up and the moment
    /// after the window included.
    pub total_commits: u64,
    /// The median and 99th percentile of the time from a transaction's
    /// start to its success, tries again included, over `commits`.
    pub p50_us: f64,
    pub p99_us: f64,
    /// Tries done again after a conflict (Oakdb) or a busy database.
    pub retries: u64,
    /// The fewest and the most commits one thread had in the window: how
    /// fairly the threads took turns.
    pub thread_commits_min: u64,
    pub thread_commits_max: u64,
    /// SQLite transactions given up after the busy timeout: a round with
    /// any is recorded as failed (design 0026 Q16).
    pub timeouts: u64,
    /// Oakdb's own counts, for Oakdb.
    pub engine: Option<EngineCounts>,
    /// For W3, what the counters add up to after the round: one a commit.
    pub hot_sum: Option<u64>,
    /// SQLite's settings as read back from a connection that commits.
    pub pragmas: Vec<(String, String)>,
}

impl Outcome {
    /// Commits a second in the window, 0 for a window of no time.
    pub fn per_second(&self) -> f64 {
        let secs = self.config.window.as_secs_f64();
        if secs == 0.0 {
            0.0
        } else {
            self.commits as f64 / secs
        }
    }

    /// The value SQLite gave for pragma `name`, or `sqlite_version`.
    pub fn pragma(&self, name: &str) -> Option<&str> {
        self.pragmas
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// One line of `name=value` fields.
    pub fn line(&self) -> String {
        let mut line = format!(
            "system={} workload={} threads={} commits={} secs={:.3} per_second={:.1} \
             p50_us={:.1} p99_us={:.1} retries={} timeouts={} total_commits={} \
             thread_min={} thread_max={}",
            self.config.system.name(),
            self.config.workload.name(),
            self.config.threads,
            self.commits,
            self.config.window.as_secs_f64(),
            self.per_second(),
            self.p50_us,
            self.p99_us,
            self.retries,
            self.timeouts,
            self.total_commits,
            self.thread_commits_min,
            self.thread_commits_max,
        );
        if let Some(e) = self.engine {
            let sizes: Vec<String> = e.batch_sizes.iter().map(u64::to_string).collect();
            line += &format!(
                " batches={} wal_syncs={} conflicts={} largest_batch={} batch_sizes={} wal_write_ms={:.1}",
                e.batches,
                e.wal_syncs,
                e.conflicts,
                e.largest_batch,
                sizes.join("/"),
                e.wal_write_nanos as f64 / 1e6,
            );
        }
        if let Some(sum) = self.hot_sum {
            line += &format!(" hot_sum={sum}");
        }
        for (name, value) in &self.pragmas {
            line += &format!(" {name}={value}");
        }
        line
    }
}

/// Measures one round of `config` in a fresh database under `dir`, which
/// it leaves as it found it.
///
/// # Panics
///
/// On an error no transaction of the workload should meet, such as a
/// failed write, and for W3 through the SQL layer, which it does not run.
pub fn run(dir: &Path, config: &Config) -> Outcome {
    let path = dir.join(format!("{}-{}", config.system.name(), std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("make the database's directory");
    let out = match config.system {
        System::Oak => run_oak(StdFs, &path, config, MAX_BATCH),
        System::OakOff => run_oak(StdFs, &path, config, 1),
        System::OakNoSync => run_oak(NoSyncFs, &path, config, MAX_BATCH),
        System::OakSql => run_oak_sql(&path, config),
        System::SqliteS1 | System::SqliteS2 | System::SqliteS3 => run_sqlite(&path, config),
    };
    let _ = std::fs::remove_dir_all(&path);
    out
}

/// A small, seeded random number generator (splitmix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn key(&mut self) -> Vec<u8> {
        [self.next().to_be_bytes(), self.next().to_be_bytes()].concat()
    }

    /// The generator thread `t` draws its keys from.
    fn for_thread(seed: u64, t: usize) -> Rng {
        Rng(seed ^ (t as u64 + 1).wrapping_mul(0xd1b5_4a32_d192_ed03))
    }
}

/// How many counters W3 shares among the threads.
const HOT_KEYS: u8 = 16;

fn hot_key(i: u8) -> Vec<u8> {
    let mut key = vec![0; 16];
    key[0] = b'h';
    key[15] = i;
    key
}

/// The rows every system starts with: `config.preload` random keys of
/// 100-byte values, and for W3 the counters at 0.
fn preload_rows(config: &Config) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut rng = Rng(config.seed);
    let mut rows: Vec<_> = (0..config.preload)
        .map(|_| (rng.key(), vec![b'p'; 100]))
        .collect();
    if config.workload == Workload::Hot {
        rows.extend((0..HOT_KEYS).map(|i| (hot_key(i), 0u64.to_be_bytes().to_vec())));
    }
    rows
}

/// How a transaction ended.
enum Ended {
    /// Answered with success, after so many tries again.
    Committed { retries: u64 },
    /// Given up: SQLite stayed busy past its timeout.
    TimedOut { retries: u64 },
}

/// What one thread counted.
#[derive(Default)]
struct Counted {
    latencies_ns: Vec<u64>,
    /// Commits in the window of each thread, one entry a thread.
    per_thread: Vec<u64>,
    total: u64,
    retries: u64,
    timeouts: u64,
}

/// The window, set once the threads are ready.
struct Window {
    from: Instant,
    to: Instant,
}

/// Runs `config.threads` threads, thread `t` doing `transaction(t, rng)`
/// over and over until the window has passed. `stats` reads the engine's
/// counters, if it has any, at both ends of the window.
fn measure<T>(
    config: &Config,
    transaction: T,
    stats: impl Fn() -> Option<Stats>,
) -> (Counted, Option<(Stats, Stats)>)
where
    T: Fn(usize, &mut Rng) -> Ended + Sync,
{
    let stop = AtomicBool::new(false);
    let ready = Barrier::new(config.threads + 1);
    let window: OnceLock<Window> = OnceLock::new();
    std::thread::scope(|s| {
        let threads: Vec<_> = (0..config.threads)
            .map(|t| {
                let (stop, ready, window, transaction) = (&stop, &ready, &window, &transaction);
                s.spawn(move || {
                    let mut rng = Rng::for_thread(config.seed, t);
                    let mut counted = Counted::default();
                    ready.wait();
                    let window = window.get().expect("set before the threads go");
                    // Acquire pairs with the Release below; the flag guards
                    // no data, so this is only for clarity.
                    while !stop.load(Ordering::Acquire) {
                        let began = Instant::now();
                        let ended = transaction(t, &mut rng);
                        let answered = Instant::now();
                        match ended {
                            Ended::Committed { retries } => {
                                counted.retries += retries;
                                counted.total += 1;
                                if window.from <= answered && answered < window.to {
                                    let ns = answered.duration_since(began).as_nanos();
                                    counted
                                        .latencies_ns
                                        .push(u64::try_from(ns).unwrap_or(u64::MAX));
                                }
                            }
                            Ended::TimedOut { retries } => {
                                counted.retries += retries;
                                counted.timeouts += 1;
                            }
                        }
                    }
                    counted
                })
            })
            .collect();
        let start = Instant::now();
        let from = start + config.warmup;
        let to = from + config.window;
        let _ = window.set(Window { from, to });
        ready.wait();
        sleep_until(from);
        let before = stats();
        sleep_until(to);
        let after = stats();
        stop.store(true, Ordering::Release);
        let mut all = Counted::default();
        for thread in threads {
            let c = thread.join().expect("a benchmark thread panicked");
            all.per_thread.push(c.latencies_ns.len() as u64);
            all.latencies_ns.extend(c.latencies_ns);
            all.total += c.total;
            all.retries += c.retries;
            all.timeouts += c.timeouts;
        }
        (all, before.zip(after))
    })
}

fn sleep_until(at: Instant) {
    let now = Instant::now();
    if at > now {
        std::thread::sleep(at - now);
    }
}

/// The `q` quantile of sorted `xs`, in microseconds, 0 if there are none.
fn quantile_us(xs: &[u64], q: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let at = ((q * xs.len() as f64).ceil() as usize).clamp(1, xs.len()) - 1;
    xs[at] as f64 / 1e3
}

fn outcome(config: &Config, mut counted: Counted, stats: Option<(Stats, Stats)>) -> Outcome {
    counted.latencies_ns.sort_unstable();
    let xs = &counted.latencies_ns;
    Outcome {
        config: config.clone(),
        commits: xs.len() as u64,
        total_commits: counted.total,
        p50_us: quantile_us(xs, 0.5),
        p99_us: quantile_us(xs, 0.99),
        retries: counted.retries,
        thread_commits_min: counted.per_thread.iter().copied().min().unwrap_or(0),
        thread_commits_max: counted.per_thread.iter().copied().max().unwrap_or(0),
        timeouts: counted.timeouts,
        engine: stats.map(|(before, after)| EngineCounts::between(before, after)),
        hot_sum: None,
        pragmas: Vec::new(),
    }
}

// ---- Oakdb ----

fn run_oak<F: Fs>(fs: F, path: &Path, config: &Config, max_batch: usize) -> Outcome {
    let db = Db::open_with_max_batch(fs, path, max_batch).expect("open Oakdb");
    for chunk in preload_rows(config).chunks(1000) {
        let mut txn = db.begin();
        for (key, value) in chunk {
            txn.put(key, value).expect("preload");
        }
        txn.commit().expect("preload");
    }
    let value = vec![b'v'; config.workload.value_len()];
    let workload = config.workload;
    let transaction = |_t: usize, rng: &mut Rng| {
        let key = match workload {
            Workload::Hot => hot_key((rng.next() % u64::from(HOT_KEYS)) as u8),
            Workload::Distinct | Workload::Large => rng.key(),
        };
        let mut retries = 0;
        loop {
            let mut txn = db.begin();
            let value = match workload {
                Workload::Hot => {
                    let now = txn.get(&key).expect("read a counter").expect("preloaded");
                    let now = u64::from_be_bytes(now.try_into().expect("8 bytes"));
                    (now + 1).to_be_bytes().to_vec()
                }
                Workload::Distinct | Workload::Large => value.clone(),
            };
            txn.put(&key, &value).expect("put");
            match txn.commit() {
                Ok(()) => return Ended::Committed { retries },
                Err(Error::Conflict { .. }) => retries += 1,
                Err(e) => panic!("commit failed: {e}"),
            }
        }
    };
    let (counted, stats) = measure(config, transaction, || Some(db.stats()));
    let mut out = outcome(config, counted, stats);
    if workload == Workload::Hot {
        let txn = db.begin();
        let sum = (0..HOT_KEYS)
            .map(|i| {
                let v = txn.get(&hot_key(i)).expect("read").expect("preloaded");
                u64::from_be_bytes(v.try_into().expect("8 bytes"))
            })
            .sum();
        out.hot_sum = Some(sum);
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run_oak_sql(path: &Path, config: &Config) -> Outcome {
    assert!(
        config.workload != Workload::Hot,
        "the SQL layer runs W1 and W2 only (design 0026 Q7)"
    );
    let db = Db::open(path).expect("open Oakdb");
    oakdb_sql::execute(&db, "CREATE TABLE kv (k BLOB PRIMARY KEY, v BLOB)").expect("create");
    for chunk in preload_rows(config).chunks(200) {
        let rows: Vec<String> = chunk
            .iter()
            .map(|(k, v)| format!("(X'{}', X'{}')", hex(k), hex(v)))
            .collect();
        let sql = format!("INSERT INTO kv VALUES {}", rows.join(", "));
        oakdb_sql::execute(&db, &sql).expect("preload");
    }
    let value = hex(&vec![b'v'; config.workload.value_len()]);
    let transaction = |_t: usize, rng: &mut Rng| {
        // Parsed every time: the SQL layer has no prepared statements.
        let sql = format!("INSERT INTO kv VALUES (X'{}', X'{value}')", hex(&rng.key()));
        let mut retries = 0;
        loop {
            match oakdb_sql::execute(&db, &sql) {
                Ok(_) => return Ended::Committed { retries },
                Err(e) if e.is_conflict() => retries += 1,
                Err(e) => panic!("INSERT failed: {e}"),
            }
        }
    };
    let (counted, stats) = measure(config, transaction, || Some(db.stats()));
    outcome(config, counted, stats)
}

/// A file system whose syncs do nothing: [`System::OakNoSync`].
#[derive(Debug, Clone, Copy)]
pub struct NoSyncFs;

/// A file of [`NoSyncFs`].
#[derive(Debug)]
pub struct NoSyncFile(StdFile);

impl Fs for NoSyncFs {
    type File = NoSyncFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<NoSyncFile> {
        StdFs.open(path).map(NoSyncFile)
    }

    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

impl FsFile for NoSyncFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.read_exact_at(buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_all_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.0.size()
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn try_lock(&self) -> io::Result<()> {
        self.0.try_lock()
    }

    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
}

// ---- SQLite ----

/// How long a SQLite connection waits for the write lock (design 0026
/// Q16).
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

const BEGIN: &str = "BEGIN IMMEDIATE";
const INSERT: &str = "INSERT INTO kv (k, v) VALUES (?1, ?2)";
const READ: &str = "SELECT v FROM kv WHERE k = ?1";
const UPDATE: &str = "UPDATE kv SET v = ?2 WHERE k = ?1";
const COMMIT: &str = "COMMIT";

/// The pragmas read back into [`Outcome::pragmas`].
const PRAGMAS: [&str; 6] = [
    "journal_mode",
    "synchronous",
    "fullfsync",
    "checkpoint_fullfsync",
    "page_size",
    "wal_autocheckpoint",
];

/// Opens a connection to the database at `file`, with the settings that
/// make SQLite as durable as Oakdb if `durable` (design 0026 Q6), and
/// SQLite's defaults if not. `synchronous` and `fullfsync` belong to the
/// connection, so every connection sets them.
fn connect(file: &Path, durable: bool) -> Connection {
    let conn = Connection::open(file).expect("open SQLite");
    conn.busy_timeout(BUSY_TIMEOUT).expect("busy timeout");
    if durable {
        conn.pragma_update(None, "page_size", 4096)
            .expect("page_size");
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal");
        conn.pragma_update(None, "synchronous", "FULL")
            .expect("synchronous");
        conn.pragma_update(None, "fullfsync", "ON")
            .expect("fullfsync");
        conn.pragma_update(None, "checkpoint_fullfsync", "ON")
            .expect("checkpoint_fullfsync");
    }
    conn
}

fn read_pragmas(conn: &Connection) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = PRAGMAS
        .iter()
        .map(|&name| {
            let value: rusqlite::types::Value = conn
                .pragma_query_value(None, name, |row| row.get(0))
                .expect("read a pragma");
            let value = match value {
                rusqlite::types::Value::Integer(n) => n.to_string(),
                rusqlite::types::Value::Text(s) => s.to_lowercase(),
                other => format!("{other:?}"),
            };
            (name.to_string(), value)
        })
        .collect();
    let version: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("sqlite_version");
    out.push(("sqlite_version".into(), version));
    out
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

/// One transaction of `workload` on `conn`, writing `value` to `key` (W1,
/// W2) or adding one to counter `key` (W3).
fn sqlite_transaction(conn: &Connection, workload: Workload, key: &[u8], value: &[u8]) -> Ended {
    let attempt = || -> rusqlite::Result<()> {
        conn.prepare_cached(BEGIN)?.execute([])?;
        let result = (|| {
            match workload {
                Workload::Hot => {
                    let now: Vec<u8> = conn.prepare_cached(READ)?.query_row([key], |r| r.get(0))?;
                    let now = u64::from_be_bytes(now.try_into().expect("8 bytes"));
                    conn.prepare_cached(UPDATE)?
                        .execute((key, (now + 1).to_be_bytes().as_slice()))?;
                }
                Workload::Distinct | Workload::Large => {
                    conn.prepare_cached(INSERT)?.execute((key, value))?;
                }
            }
            conn.prepare_cached(COMMIT)?.execute([])?;
            Ok(())
        })();
        if result.is_err() && !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        result
    };
    match attempt() {
        Ok(()) => Ended::Committed { retries: 0 },
        Err(e) if is_busy(&e) => Ended::TimedOut { retries: 0 },
        Err(e) => panic!("SQLite transaction failed: {e}"),
    }
}

fn run_sqlite(path: &Path, config: &Config) -> Outcome {
    let durable = config.system != System::SqliteS3;
    let file = path.join("bench.sqlite");
    {
        let mut setup = connect(&file, durable);
        setup
            .execute_batch("CREATE TABLE kv (k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID")
            .expect("create");
        let txn = setup.transaction().expect("begin");
        {
            let mut insert = txn.prepare(INSERT).expect("prepare");
            for (key, value) in preload_rows(config) {
                insert.execute((key, value)).expect("preload");
            }
        }
        txn.commit().expect("preload");
    }
    let value = vec![b'v'; config.workload.value_len()];
    let workload = config.workload;
    let key_of = |rng: &mut Rng| match workload {
        Workload::Hot => hot_key((rng.next() % u64::from(HOT_KEYS)) as u8),
        Workload::Distinct | Workload::Large => rng.key(),
    };
    let (counted, pragmas) = if config.system == System::SqliteS2 {
        let shared = Mutex::new(connect(&file, durable));
        let pragmas = read_pragmas(&shared.lock().unwrap_or_else(PoisonError::into_inner));
        let transaction = |_t: usize, rng: &mut Rng| {
            let key = key_of(rng);
            let conn = shared.lock().unwrap_or_else(PoisonError::into_inner);
            sqlite_transaction(&conn, workload, &key, &value)
        };
        (measure(config, transaction, || None).0, pragmas)
    } else {
        // A connection a thread, made here and each used by its thread
        // only: the mutex is never contended, and is there because a
        // connection is not `Sync`.
        let conns: Vec<Mutex<Connection>> = (0..config.threads)
            .map(|_| Mutex::new(connect(&file, durable)))
            .collect();
        let pragmas = match conns.first() {
            Some(conn) => read_pragmas(&conn.lock().unwrap_or_else(PoisonError::into_inner)),
            None => Vec::new(),
        };
        let transaction = |t: usize, rng: &mut Rng| {
            let key = key_of(rng);
            let conn = conns[t].lock().unwrap_or_else(PoisonError::into_inner);
            sqlite_transaction(&conn, workload, &key, &value)
        };
        (measure(config, transaction, || None).0, pragmas)
    };
    let mut out = outcome(config, counted, None);
    out.pragmas = pragmas;
    if workload == Workload::Hot {
        let conn = connect(&file, durable);
        let mut read = conn.prepare(READ).expect("prepare");
        let total = (0..HOT_KEYS)
            .map(|i| {
                let v: Vec<u8> = read.query_row([hot_key(i)], |r| r.get(0)).expect("read");
                u64::from_be_bytes(v.try_into().expect("8 bytes"))
            })
            .sum();
        out.hot_sum = Some(total);
    }
    out
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line())
    }
}
