//! Blind randomized history test (issue #29, design 0024 V1/V2), written
//! from the spec by a tester who did not read the implementation or the
//! PR's own workload (PR #79's verification). Threads run random
//! list-append transactions on the real engine; the checker must find no
//! violation, and should count some write skew.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use oakdb::fs::sim::SimFs;
use oakdb::{Db, Error};
use oakdb_history::{check, Elem, History, Op, Outcome, Txn, TxnId};

const KEYS: usize = 8;

fn key(i: usize) -> String {
    format!("k{i}")
}

/// splitmix64.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn encode(list: &[Elem]) -> Vec<u8> {
    list.iter()
        .map(|e| format!("{}.{}.{}", e.txn.thread, e.txn.seq, e.n))
        .collect::<Vec<_>>()
        .join(",")
        .into_bytes()
}

fn decode(bytes: Option<Vec<u8>>) -> Vec<Elem> {
    let Some(bytes) = bytes else { return vec![] };
    let s = String::from_utf8(bytes).unwrap();
    if s.is_empty() {
        return vec![];
    }
    s.split(',')
        .map(|part| {
            let mut it = part.split('.').map(|n| n.parse::<u32>().unwrap());
            let (t, q, n) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            Elem::new(TxnId::new(t, q), n)
        })
        .collect()
}

fn run(seed: u64, threads: u32, per_thread: u32) -> (History, oakdb_history::Report) {
    let db: Db<SimFs> = Db::open_with(SimFs::new(), "/oak").unwrap();
    let start = Instant::now();
    let txns = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for th in 0..threads {
            let db = &db;
            let txns = &txns;
            s.spawn(move || {
                let mut rng = Rng(seed ^ (u64::from(th) + 1).wrapping_mul(0xA24B_AED4_963E_E407));
                for seq in 0..per_thread {
                    let me = TxnId::new(th, seq);
                    let steps = 1 + rng.below(4);
                    let begin = start.elapsed().as_nanos() as u64;
                    let mut t = db.begin();
                    let mut ops = Vec::new();
                    let mut appended = 0;
                    for step in 0..steps {
                        let roll = rng.below(100);
                        if roll < 45 {
                            let k = key(rng.below(KEYS as u64) as usize);
                            let v = decode(t.get(k.as_bytes()).unwrap());
                            ops.push(Op::Read { key: k, value: v });
                        } else if roll < 90 {
                            let k = key(rng.below(KEYS as u64) as usize);
                            let mut v = decode(t.get(k.as_bytes()).unwrap());
                            ops.push(Op::Read {
                                key: k.clone(),
                                value: v.clone(),
                            });
                            let elem = Elem::new(me, appended);
                            appended += 1;
                            v.push(elem);
                            t.put(k.as_bytes(), &encode(&v)).unwrap();
                            ops.push(Op::Append { key: k, elem });
                        } else {
                            let lo = rng.below(KEYS as u64) as usize;
                            let hi = lo + 1 + rng.below((KEYS - lo) as u64) as usize;
                            let (lk, hk) = (key(lo), key(hi));
                            let found: BTreeMap<Vec<u8>, Vec<u8>> = t
                                .scan(lk.as_bytes()..hk.as_bytes())
                                .map(Result::unwrap)
                                .collect();
                            for i in lo..hi {
                                let k = key(i);
                                let v = decode(found.get(k.as_bytes()).cloned());
                                ops.push(Op::Read { key: k, value: v });
                            }
                        }
                        if step == steps / 2 {
                            std::thread::sleep(Duration::from_micros(rng.below(150)));
                        }
                    }
                    let outcome = match t.commit() {
                        Ok(()) => Outcome::Ok,
                        Err(Error::Conflict { .. }) => Outcome::Fail,
                        Err(other) => panic!("unexpected error {other:?}"),
                    };
                    let end = start.elapsed().as_nanos() as u64;
                    txns.lock().unwrap().push(Txn {
                        id: me,
                        outcome,
                        begin,
                        end,
                        ops,
                    });
                }
            });
        }
    });
    let mut last = BTreeMap::new();
    let t = db.begin();
    for i in 0..KEYS {
        let k = key(i);
        last.insert(k.clone(), decode(t.get(k.as_bytes()).unwrap()));
    }
    let history = History {
        txns: txns.into_inner().unwrap(),
        last: Some(last),
    };
    let report = check(&history);
    (history, report)
}

#[test]
fn random_list_append_histories_on_the_real_engine_have_no_violations() {
    let mut skews = 0;
    for seed in [1u64, 2, 3, 0xDEAD_BEEF, 42] {
        for threads in [4, 8] {
            let (history, report) = run(seed, threads, 60);
            assert!(
                report.anomalies.is_empty(),
                "seed {seed}, {threads} threads: {} anomalies, first {}\n{history}",
                report.anomalies.len(),
                report.anomalies[0]
            );
            assert_eq!(report.txns, (threads * 60) as usize);
            skews += report.write_skews;
            eprintln!(
                "seed {seed} threads {threads}: committed {} / {}, write skews {}",
                report.committed, report.txns, report.write_skews
            );
        }
    }
    assert!(skews > 0, "the workload never made write skew");
}

#[test]
fn a_tampered_engine_history_is_caught() {
    // Control for the run above: take a real history and drop one committed
    // append from the final reads; the checker must call it a lost update.
    let (mut history, report) = run(7, 4, 40);
    assert!(report.anomalies.is_empty());
    let last = history.last.as_mut().unwrap();
    let (_, list) = last
        .iter_mut()
        .find(|(_, l)| !l.is_empty())
        .expect("some key written");
    list.pop();
    let kinds: Vec<_> = check(&history).anomalies.iter().map(|a| a.kind()).collect();
    assert!(
        kinds.contains(&oakdb_history::Kind::LostUpdate),
        "{kinds:?}"
    );
}
