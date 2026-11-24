//! Random transactions on many threads, written down as a history for the
//! checker in `oakdb-history` (issue #29, design 0024 V2, V11).
//!
//! Each key holds a list of names; a write reads the list and puts it back
//! with a name of its own added. A seed fixes what each thread does; how
//! the threads interleave is the scheduler's.

#![allow(dead_code)] // Each test uses part of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use oakdb::fs::Fs;
use oakdb::{Db, Error, Txn};
use oakdb_history::{check, Elem, History, Op, Outcome, Report, Txn as Record, TxnId};

/// What a run does.
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    pub threads: u32,
    /// Transactions in all, shared out among the threads.
    pub txns: u32,
    pub keys: u32,
    pub seed: u64,
}

impl Plan {
    /// The run design 0024 V2 settled on: 8 keys, about 2000 transactions.
    pub fn new(threads: u32, seed: u64) -> Self {
        Plan {
            threads,
            txns: 2000,
            keys: 8,
            seed,
        }
    }
}

/// The seeds a test runs: `default`, or as many as `OAKDB_HISTORY_SEEDS`
/// says, from 1.
pub fn seeds(default: &[u64]) -> Vec<u64> {
    match std::env::var("OAKDB_HISTORY_SEEDS") {
        Ok(n) => (1..=n.parse().expect("OAKDB_HISTORY_SEEDS is a count")).collect(),
        Err(_) => default.to_vec(),
    }
}

/// A small, fixed random number generator (splitmix64), so a seed gives
/// the same plan whatever crate versions are in use (design 0024 Q6).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn key(i: u32) -> String {
    format!("k{i:02}")
}

pub fn encode(list: &[Elem]) -> Vec<u8> {
    let names: Vec<String> = list.iter().map(Elem::to_string).collect();
    names.join(",").into_bytes()
}

pub fn decode(value: Option<&[u8]>) -> Vec<Elem> {
    match value {
        None | Some(b"") => Vec::new(),
        Some(bytes) => std::str::from_utf8(bytes)
            .expect("a list is text")
            .split(',')
            .map(|name| name.parse().expect("a list holds names"))
            .collect(),
    }
}

/// Nanoseconds since `clock`.
pub fn now(clock: Instant) -> u64 {
    clock.elapsed().as_nanos() as u64
}

/// Reads `key` in `txn`, noting it.
pub fn read<F: Fs>(txn: &Txn<'_, F>, ops: &mut Vec<Op>, key: &str) -> oakdb::Result<Vec<Elem>> {
    let value = decode(txn.get(key.as_bytes())?.as_deref());
    ops.push(Op::Read {
        key: key.to_string(),
        value: value.clone(),
    });
    Ok(value)
}

/// Reads `key` in `txn` and writes it back with `elem` added, noting both.
pub fn append<F: Fs>(
    txn: &mut Txn<'_, F>,
    ops: &mut Vec<Op>,
    key: &str,
    elem: Elem,
) -> oakdb::Result<()> {
    let mut list = read(txn, ops, key)?;
    list.push(elem);
    txn.put(key.as_bytes(), &encode(&list))?;
    ops.push(Op::Append {
        key: key.to_string(),
        elem,
    });
    Ok(())
}

/// How a commit came back, as the history says it.
pub fn outcome(result: oakdb::Result<()>) -> Outcome {
    match result {
        Ok(()) => Outcome::Ok,
        Err(Error::Conflict { .. }) => Outcome::Fail,
        Err(Error::OutcomeUnknown(_)) => Outcome::Info,
        Err(e) => panic!("a commit failed with {e}"),
    }
}

/// One transaction of `plan`: one to four steps, each a read (45%), an
/// append (45%) or a scan of a range of keys (10%), half of them followed
/// by a pause of up to 200µs, about a sync.
fn transaction<F: Fs>(db: &Db<F>, plan: &Plan, rng: &mut Rng, id: TxnId, clock: Instant) -> Record {
    let steps = 1 + rng.below(4);
    let begin = now(clock);
    let mut txn = db.begin();
    let mut ops = Vec::new();
    let mut appends = 0;
    for _ in 0..steps {
        let roll = rng.below(100);
        let k = rng.below(u64::from(plan.keys)) as u32;
        if roll < 45 {
            read(&txn, &mut ops, &key(k)).unwrap();
        } else if roll < 90 {
            append(&mut txn, &mut ops, &key(k), Elem::new(id, appends)).unwrap();
            appends += 1;
        } else {
            let end = k + 1 + rng.below(u64::from(plan.keys - k)) as u32;
            let found: BTreeMap<Vec<u8>, Vec<u8>> = txn
                .scan(key(k).as_bytes()..key(end).as_bytes())
                .collect::<oakdb::Result<_>>()
                .unwrap();
            for i in k..end {
                ops.push(Op::Read {
                    key: key(i),
                    value: decode(found.get(key(i).as_bytes()).map(Vec::as_slice)),
                });
            }
        }
        // Half the steps are followed by a pause, as a client's work or
        // network is, so commits land in the middle of transactions.
        if rng.below(2) == 0 {
            std::thread::sleep(Duration::from_micros(rng.below(200)));
        }
    }
    let result = txn.commit();
    Record {
        id,
        outcome: outcome(result),
        begin,
        end: now(clock),
        ops,
    }
}

/// Runs `plan` on `db` and returns what happened, with every key read at
/// the end.
pub fn run<F: Fs>(db: &Db<F>, plan: &Plan) -> History {
    let clock = Instant::now();
    let per_thread = plan.txns / plan.threads;
    let mut txns: Vec<Record> = std::thread::scope(|s| {
        let threads: Vec<_> = (0..plan.threads)
            .map(|t| {
                s.spawn(move || {
                    let mut rng = Rng::new(
                        plan.seed ^ (u64::from(t) + 1).wrapping_mul(0xA24B_AED4_963E_E407),
                    );
                    (0..per_thread)
                        .map(|seq| transaction(db, plan, &mut rng, TxnId::new(t, seq), clock))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect()
    });
    txns.sort_by_key(|t| t.begin);
    History {
        txns,
        last: Some(last_reads(db, plan.keys)),
    }
}

/// Every key, read in a transaction of its own.
pub fn last_reads<F: Fs>(db: &Db<F>, keys: u32) -> BTreeMap<String, Vec<Elem>> {
    let txn = db.begin();
    (0..keys)
        .map(|i| {
            let value = decode(txn.get(key(i).as_bytes()).unwrap().as_deref());
            (key(i), value)
        })
        .collect()
}

/// Checks `history`. If it holds anomalies, keeps it in
/// `target/tmp/history-failures/<name>.txt` and says where, so it can be
/// checked again (design 0024 V11).
pub fn checked(name: &str, history: &History) -> Report {
    let started = Instant::now();
    let report = check(history);
    eprintln!(
        "{name}: {} txns, {} committed, {} anomalies, {} write skews, checked in {:.1?}",
        report.txns,
        report.committed,
        report.anomalies.len(),
        report.write_skews,
        started.elapsed()
    );
    if !report.anomalies.is_empty() {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("history-failures");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.txt"));
        std::fs::write(&path, history.to_string()).unwrap();
        // The first of each kind.
        let mut kinds = Vec::new();
        for anomaly in &report.anomalies {
            if !kinds.contains(&anomaly.kind()) {
                kinds.push(anomaly.kind());
                eprintln!("  {anomaly}");
            }
        }
        eprintln!("  history kept in {}", path.display());
    }
    report
}
