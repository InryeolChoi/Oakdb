//! Blind tests for the history checker (issue #29, design 0024 V3/V4,
//! ADR 0006), written from the issue, the design and the experiment
//! records by a tester who did not read the implementation (PR #79's
//! verification). Every history here is built by hand.

use std::collections::{BTreeMap, BTreeSet};

use oakdb_history::{check, Anomaly, Edge, Elem, History, Kind, Op, Outcome, Report, Txn, TxnId};

// ---------- builders ----------

fn id(thread: u32, seq: u32) -> TxnId {
    TxnId::new(thread, seq)
}

/// The first (n = 0) name transaction `t<thread>#<seq>` appends.
fn e(thread: u32, seq: u32) -> Elem {
    Elem::new(id(thread, seq), 0)
}

fn en(thread: u32, seq: u32, n: u32) -> Elem {
    Elem::new(id(thread, seq), n)
}

fn r(key: &str, value: &[Elem]) -> Op {
    Op::Read {
        key: key.to_string(),
        value: value.to_vec(),
    }
}

fn a(key: &str, elem: Elem) -> Op {
    Op::Append {
        key: key.to_string(),
        elem,
    }
}

fn txn(tid: TxnId, outcome: Outcome, begin: u64, end: u64, ops: Vec<Op>) -> Txn {
    Txn {
        id: tid,
        outcome,
        begin,
        end,
        ops,
    }
}

fn last(pairs: &[(&str, &[Elem])]) -> Option<BTreeMap<String, Vec<Elem>>> {
    Some(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_vec()))
            .collect(),
    )
}

fn kinds(report: &Report) -> BTreeSet<Kind> {
    report.anomalies.iter().map(Anomaly::kind).collect()
}

fn assert_clean(report: &Report) {
    assert!(
        report.anomalies.is_empty(),
        "expected no anomalies, got {:#?}",
        report.anomalies
    );
}

/// The expected kind is reported, and every kind reported is one of
/// `allowed` (the design: "the expected name came out, and every name that
/// came out is one this bug can make").
fn assert_kinds(report: &Report, expected: Kind, allowed: &[Kind]) {
    let got = kinds(report);
    assert!(
        got.contains(&expected),
        "expected {expected:?} among {got:?}: {:#?}",
        report.anomalies
    );
    for k in &got {
        assert!(
            *k == expected || allowed.contains(k),
            "unexpected {k:?} (expected {expected:?}, allowed {allowed:?}): {:#?}",
            report.anomalies
        );
    }
}

fn cycle_of(report: &Report, kind: Kind) -> Vec<(TxnId, Edge)> {
    for an in &report.anomalies {
        match an {
            Anomaly::CommitCycle { cycle } if kind == Kind::CommitCycle => return cycle.clone(),
            Anomaly::GSingle { cycle } if kind == Kind::GSingle => return cycle.clone(),
            _ => {}
        }
    }
    panic!("no {kind:?} in {:#?}", report.anomalies)
}

const OK: Outcome = Outcome::Ok;
const FAIL: Outcome = Outcome::Fail;
const INFO: Outcome = Outcome::Info;

// ---------- histories snapshot isolation allows ----------

#[test]
fn an_empty_history_is_clean() {
    let report = check(&History::default());
    assert_clean(&report);
    assert_eq!(report.txns, 0);
    assert_eq!(report.committed, 0);
    assert_eq!(report.write_skews, 0);
}

#[test]
fn a_serial_history_is_clean_and_counted() {
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 10, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(
                id(1, 0),
                OK,
                20,
                30,
                vec![r("x", &[e(0, 0)]), a("x", e(1, 0))],
            ),
            txn(
                id(0, 1),
                OK,
                40,
                50,
                vec![r("x", &[e(0, 0), e(1, 0)]), r("y", &[])],
            ),
        ],
        last: last(&[("x", &[e(0, 0), e(1, 0)]), ("y", &[])]),
    };
    let report = check(&h);
    assert_clean(&report);
    assert_eq!(report.txns, 3);
    assert_eq!(report.committed, 3);
    assert_eq!(report.write_skews, 0);
}

#[test]
fn a_serial_history_without_final_reads_is_clean() {
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 10, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(
                id(1, 0),
                OK,
                20,
                30,
                vec![r("x", &[e(0, 0)]), a("x", e(1, 0))],
            ),
            txn(id(2, 0), OK, 40, 50, vec![r("x", &[e(0, 0), e(1, 0)])]),
        ],
        last: None,
    };
    assert_clean(&check(&h));
}

#[test]
fn reading_an_older_snapshot_while_a_commit_overlaps_is_clean() {
    // t1#0 began before t0#0's commit came back, so it may read x empty.
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 100, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 50, 150, vec![r("x", &[]), r("y", &[])]),
        ],
        last: last(&[("x", &[e(0, 0)]), ("y", &[])]),
    };
    assert_clean(&check(&h));
}

#[test]
fn reading_your_own_appends_within_a_transaction_is_clean() {
    let h = History {
        txns: vec![
            txn(
                id(0, 0),
                OK,
                0,
                10,
                vec![
                    r("x", &[]),
                    a("x", en(0, 0, 0)),
                    r("x", &[en(0, 0, 0)]),
                    a("x", en(0, 0, 1)),
                    r("x", &[en(0, 0, 0), en(0, 0, 1)]),
                ],
            ),
            txn(
                id(1, 0),
                OK,
                20,
                30,
                vec![r("x", &[en(0, 0, 0), en(0, 0, 1)])],
            ),
        ],
        last: last(&[("x", &[en(0, 0, 0), en(0, 0, 1)])]),
    };
    assert_clean(&check(&h));
}

#[test]
fn a_conflict_loser_that_nobody_saw_is_clean() {
    // First committer wins: both appended to x on the empty list, t1#0 lost.
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 100, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), FAIL, 10, 110, vec![r("x", &[]), a("x", e(1, 0))]),
            txn(id(2, 0), OK, 200, 210, vec![r("x", &[e(0, 0)])]),
        ],
        last: last(&[("x", &[e(0, 0)])]),
    };
    let report = check(&h);
    assert_clean(&report);
    assert_eq!(report.txns, 3);
    assert_eq!(report.committed, 2);
}

#[test]
fn write_skew_is_allowed_but_counted() {
    // ADR 0006: T1 reads x,y and appends x; T2 reads x,y and appends y, both
    // on the same old snapshot. Two rw edges, one each way.
    let h = History {
        txns: vec![
            txn(
                id(1, 0),
                OK,
                0,
                100,
                vec![r("x", &[]), r("y", &[]), a("x", e(1, 0))],
            ),
            txn(
                id(2, 0),
                OK,
                10,
                110,
                vec![r("x", &[]), r("y", &[]), a("y", e(2, 0))],
            ),
        ],
        last: last(&[("x", &[e(1, 0)]), ("y", &[e(2, 0)])]),
    };
    let report = check(&h);
    assert_clean(&report);
    assert!(
        report.write_skews >= 1,
        "write skew not counted: {report:?}"
    );
    assert_eq!(report.committed, 2);
}

#[test]
fn a_three_transaction_write_skew_cycle_is_allowed() {
    // T1 reads y appends x, T2 reads z appends y, T3 reads x appends z, all
    // on the empty snapshot: a cycle of three rw edges, which SI allows.
    let h = History {
        txns: vec![
            txn(
                id(1, 0),
                OK,
                0,
                100,
                vec![r("y", &[]), r("x", &[]), a("x", e(1, 0))],
            ),
            txn(
                id(2, 0),
                OK,
                5,
                105,
                vec![r("z", &[]), r("y", &[]), a("y", e(2, 0))],
            ),
            txn(
                id(3, 0),
                OK,
                10,
                110,
                vec![r("x", &[]), r("z", &[]), a("z", e(3, 0))],
            ),
        ],
        last: last(&[("x", &[e(1, 0)]), ("y", &[e(2, 0)]), ("z", &[e(3, 0)])]),
    };
    let report = check(&h);
    assert_clean(&report);
    assert!(report.write_skews >= 1, "{report:?}");
}

#[test]
fn an_unknown_outcome_seen_in_the_final_reads_counts_as_committed() {
    let h = History {
        txns: vec![
            txn(id(0, 0), INFO, 0, 10, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 20, 30, vec![r("x", &[e(0, 0)])]),
        ],
        last: last(&[("x", &[e(0, 0)])]),
    };
    let report = check(&h);
    assert_clean(&report);
    assert_eq!(report.committed, 2);
}

#[test]
fn an_unknown_outcome_nobody_saw_is_clean_and_not_committed() {
    let h = History {
        txns: vec![
            txn(id(0, 0), INFO, 0, 10, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 20, 30, vec![r("x", &[])]),
        ],
        last: last(&[("x", &[])]),
    };
    let report = check(&h);
    assert_clean(&report);
    assert_eq!(report.committed, 1);
}

#[test]
fn an_unknown_outcome_seen_by_a_reader_without_final_reads_is_clean() {
    // No `last`: whether anyone saw its writes decides.
    let h = History {
        txns: vec![
            txn(id(0, 0), INFO, 0, 10, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 20, 30, vec![r("x", &[e(0, 0)])]),
        ],
        last: None,
    };
    let report = check(&h);
    assert_clean(&report);
    assert_eq!(report.committed, 2);
}

// ---------- anomalies ----------

#[test]
fn lost_update_two_appends_on_the_same_list_both_committed() {
    let h = History {
        txns: vec![
            txn(id(1, 0), OK, 0, 100, vec![r("x", &[]), a("x", e(1, 0))]),
            txn(id(2, 0), OK, 10, 110, vec![r("x", &[]), a("x", e(2, 0))]),
        ],
        last: last(&[("x", &[e(2, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::LostUpdate, &[]);
    assert!(report.anomalies.iter().any(|an| matches!(an,
        Anomaly::LostUpdate { key, lost, .. } if key == "x" && *lost == e(1, 0))));
}

#[test]
fn lost_update_from_diverging_reads_without_final_reads() {
    // Design 3: "two lists of one key are not prefixes of each other".
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 10, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(
                id(1, 0),
                OK,
                20,
                100,
                vec![r("x", &[e(0, 0)]), a("x", e(1, 0))],
            ),
            txn(
                id(2, 0),
                OK,
                30,
                110,
                vec![r("x", &[e(0, 0)]), a("x", e(2, 0))],
            ),
            txn(id(3, 0), OK, 200, 210, vec![r("x", &[e(0, 0), e(1, 0)])]),
            txn(id(4, 0), OK, 200, 210, vec![r("x", &[e(0, 0), e(2, 0)])]),
        ],
        last: None,
    };
    let report = check(&h);
    // A diverging history may also show stale reads or cycles; lost update
    // must be among them.
    assert_kinds(
        &report,
        Kind::LostUpdate,
        &[
            Kind::StaleRead,
            Kind::GSingle,
            Kind::CommitCycle,
            Kind::NonRepeatableRead,
        ],
    );
}

#[test]
fn lost_update_a_committed_write_missing_from_the_final_reads() {
    let h = History {
        txns: vec![txn(id(0, 0), OK, 0, 10, vec![r("x", &[]), a("x", e(0, 0))])],
        last: last(&[("x", &[])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::LostUpdate, &[]);
}

#[test]
fn aborted_read_g1a_of_a_failed_transaction() {
    let h = History {
        txns: vec![
            txn(id(0, 0), FAIL, 0, 100, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 50, 150, vec![r("x", &[e(0, 0)])]),
        ],
        last: last(&[("x", &[])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::AbortedRead, &[]);
    assert!(report.anomalies.iter().any(|an| matches!(an,
        Anomaly::AbortedRead { reader, key, elem }
            if *reader == id(1, 0) && key == "x" && *elem == e(0, 0))));
}

#[test]
fn aborted_read_g1a_of_an_unknown_outcome_gone_after_reopening() {
    // Design 3: an `info` name was read, and the final list after reopening
    // does not have it (planted bug 3's shape).
    let h = History {
        txns: vec![
            txn(id(0, 0), INFO, 0, 100, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 50, 150, vec![r("x", &[e(0, 0)])]),
        ],
        last: last(&[("x", &[])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::AbortedRead, &[]);
}

#[test]
fn aborted_read_of_a_name_nobody_wrote() {
    let h = History {
        txns: vec![txn(id(1, 0), OK, 0, 10, vec![r("x", &[e(9, 9)])])],
        last: last(&[("x", &[])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::AbortedRead, &[Kind::LostUpdate]);
}

#[test]
fn intermediate_read_g1b() {
    // t0#0 appends twice to x; t1#0 read the list with only the first.
    let h = History {
        txns: vec![
            txn(
                id(0, 0),
                OK,
                0,
                100,
                vec![
                    r("x", &[]),
                    a("x", en(0, 0, 0)),
                    r("x", &[en(0, 0, 0)]),
                    a("x", en(0, 0, 1)),
                ],
            ),
            txn(id(1, 0), OK, 50, 150, vec![r("x", &[en(0, 0, 0)])]),
        ],
        last: last(&[("x", &[en(0, 0, 0), en(0, 0, 1)])]),
    };
    let report = check(&h);
    // The middle read also closes a t1 -rw-> t0 -wr-> t1 cycle.
    assert_kinds(&report, Kind::IntermediateRead, &[Kind::GSingle]);
    assert!(report.anomalies.iter().any(|an| matches!(an,
        Anomaly::IntermediateRead { reader, key, elem }
            if *reader == id(1, 0) && key == "x" && *elem == en(0, 0, 0))));
}

#[test]
fn non_repeatable_read_a_key_changes_between_two_reads() {
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 10, 20, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 0, 100, vec![r("x", &[]), r("x", &[e(0, 0)])]),
        ],
        last: last(&[("x", &[e(0, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::NonRepeatableRead, &[Kind::GSingle]);
    assert!(report.anomalies.iter().any(|an| matches!(an,
        Anomaly::NonRepeatableRead { txn, key, .. } if *txn == id(1, 0) && key == "x")));
}

#[test]
fn non_repeatable_read_not_seeing_your_own_append() {
    // "or did and did not see that" (Kind::NonRepeatableRead's doc).
    let h = History {
        txns: vec![txn(
            id(0, 0),
            OK,
            0,
            10,
            vec![r("x", &[]), a("x", e(0, 0)), r("x", &[])],
        )],
        last: last(&[("x", &[e(0, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::NonRepeatableRead, &[]);
}

#[test]
fn fractured_read_g_single_one_writer_seen_on_one_key_only() {
    // t2#0 appends to x and y; t1#0 sees its x but not its y.
    let h = History {
        txns: vec![
            txn(
                id(2, 0),
                OK,
                0,
                100,
                vec![
                    r("x", &[]),
                    r("y", &[]),
                    a("x", en(2, 0, 0)),
                    a("y", en(2, 0, 1)),
                ],
            ),
            txn(id(1, 0), OK, 50, 150, vec![r("x", &[e(2, 0)]), r("y", &[])]),
        ],
        last: last(&[("x", &[e(2, 0)]), ("y", &[en(2, 0, 1)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::GSingle, &[]);
    let cycle = cycle_of(&report, Kind::GSingle);
    let rws = cycle.iter().filter(|(_, edge)| *edge == Edge::Rw).count();
    assert_eq!(rws, 1, "G-single has exactly one rw edge: {cycle:?}");
    let members: BTreeSet<TxnId> = cycle.iter().map(|(t, _)| *t).collect();
    assert_eq!(members, BTreeSet::from([id(1, 0), id(2, 0)]));
}

#[test]
fn read_skew_g_single_across_three_transactions() {
    // t1#0 reads x old (before t2#0), but reads y as written by t3#0, which
    // saw t2#0. Cycle: t1 -rw-> t2 -wr-> t3 -wr-> t1.
    let h = History {
        txns: vec![
            txn(id(1, 0), OK, 0, 300, vec![r("x", &[]), r("y", &[e(3, 0)])]),
            txn(id(2, 0), OK, 10, 100, vec![r("x", &[]), a("x", e(2, 0))]),
            txn(
                id(3, 0),
                OK,
                20,
                200,
                vec![r("x", &[e(2, 0)]), r("y", &[]), a("y", e(3, 0))],
            ),
        ],
        last: last(&[("x", &[e(2, 0)]), ("y", &[e(3, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::GSingle, &[]);
    let cycle = cycle_of(&report, Kind::GSingle);
    assert_eq!(
        cycle.iter().filter(|(_, edge)| *edge == Edge::Rw).count(),
        1
    );
    assert_eq!(report.write_skews, 0, "a one-rw cycle is not write skew");
}

#[test]
fn commit_cycle_g1c_two_transactions_saw_each_other() {
    // t1#0 read y with t2#0's name and appended to x; t2#0 read x with
    // t1#0's name and appended to y. wr both ways.
    let h = History {
        txns: vec![
            txn(
                id(1, 0),
                OK,
                0,
                100,
                vec![r("x", &[]), r("y", &[e(2, 0)]), a("x", e(1, 0))],
            ),
            txn(
                id(2, 0),
                OK,
                10,
                110,
                vec![r("y", &[]), r("x", &[e(1, 0)]), a("y", e(2, 0))],
            ),
        ],
        last: last(&[("x", &[e(1, 0)]), ("y", &[e(2, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::CommitCycle, &[]);
    let cycle = cycle_of(&report, Kind::CommitCycle);
    assert!(cycle.iter().all(|(_, edge)| *edge != Edge::Rw), "{cycle:?}");
}

#[test]
fn commit_cycle_g0_writes_in_opposite_orders_on_two_keys() {
    // x was written t1 then t2, y was written t2 then t1.
    let h = History {
        txns: vec![
            txn(
                id(1, 0),
                OK,
                0,
                100,
                vec![
                    r("x", &[]),
                    a("x", e(1, 0)),
                    r("y", &[en(2, 0, 1)]),
                    a("y", en(1, 0, 1)),
                ],
            ),
            txn(
                id(2, 0),
                OK,
                10,
                110,
                vec![
                    r("x", &[e(1, 0)]),
                    a("x", e(2, 0)),
                    r("y", &[]),
                    a("y", en(2, 0, 1)),
                ],
            ),
        ],
        last: last(&[
            ("x", &[e(1, 0), e(2, 0)]),
            ("y", &[en(2, 0, 1), en(1, 0, 1)]),
        ]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::CommitCycle, &[]);
}

#[test]
fn stale_read_the_next_transaction_on_a_thread_missed_its_commit() {
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 100, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(0, 1), OK, 200, 300, vec![r("x", &[])]),
        ],
        last: last(&[("x", &[e(0, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::StaleRead, &[Kind::GSingle]);
    assert!(report.anomalies.iter().any(|an| matches!(an,
        Anomaly::StaleRead { reader, writer, key }
            if *reader == id(0, 1) && *writer == id(0, 0) && key == "x")));
}

#[test]
fn stale_read_another_thread_began_after_the_commit_came_back() {
    // Design Q4: other threads too, by wall clock.
    let h = History {
        txns: vec![
            txn(id(0, 0), OK, 0, 100, vec![r("x", &[]), a("x", e(0, 0))]),
            txn(id(1, 0), OK, 101, 300, vec![r("x", &[])]),
        ],
        last: last(&[("x", &[e(0, 0)])]),
    };
    let report = check(&h);
    assert_kinds(&report, Kind::StaleRead, &[Kind::GSingle]);
}

#[test]
fn a_failed_transactions_reads_are_checked_too() {
    // Implementation note 3: a conflict loser also read one snapshot, so a
    // fractured read inside it is still a violation.
    let h = History {
        txns: vec![
            txn(
                id(2, 0),
                OK,
                0,
                100,
                vec![
                    r("x", &[]),
                    r("y", &[]),
                    a("x", en(2, 0, 0)),
                    a("y", en(2, 0, 1)),
                ],
            ),
            txn(
                id(1, 0),
                FAIL,
                50,
                150,
                vec![r("x", &[e(2, 0)]), r("y", &[]), a("y", e(1, 0))],
            ),
        ],
        last: last(&[("x", &[e(2, 0)]), ("y", &[en(2, 0, 1)])]),
    };
    let report = check(&h);
    assert!(
        kinds(&report).contains(&Kind::GSingle)
            || kinds(&report).contains(&Kind::NonRepeatableRead),
        "{:#?}",
        report.anomalies
    );
}

// ---------- the API around it ----------

#[test]
fn every_anomaly_reports_the_kind_it_names() {
    let x = "x".to_string();
    let cases = vec![
        (
            Anomaly::AbortedRead {
                reader: id(0, 0),
                key: x.clone(),
                elem: e(1, 0),
            },
            Kind::AbortedRead,
        ),
        (
            Anomaly::IntermediateRead {
                reader: id(0, 0),
                key: x.clone(),
                elem: e(1, 0),
            },
            Kind::IntermediateRead,
        ),
        (
            Anomaly::LostUpdate {
                key: x.clone(),
                lost: e(1, 0),
                instead: None,
            },
            Kind::LostUpdate,
        ),
        (
            Anomaly::NonRepeatableRead {
                txn: id(0, 0),
                key: x.clone(),
                expected: vec![],
                read: vec![e(1, 0)],
            },
            Kind::NonRepeatableRead,
        ),
        (
            Anomaly::StaleRead {
                reader: id(0, 1),
                writer: id(0, 0),
                key: x.clone(),
            },
            Kind::StaleRead,
        ),
        (
            Anomaly::CommitCycle {
                cycle: vec![(id(0, 0), Edge::Wr), (id(1, 0), Edge::Wr)],
            },
            Kind::CommitCycle,
        ),
        (
            Anomaly::GSingle {
                cycle: vec![(id(0, 0), Edge::Rw), (id(1, 0), Edge::Wr)],
            },
            Kind::GSingle,
        ),
    ];
    for (an, kind) in cases {
        assert_eq!(an.kind(), kind);
        assert!(!an.to_string().is_empty());
    }
}

#[test]
fn a_history_survives_its_text_form_and_checks_the_same() {
    // Design V11: a failing history is saved as text and checked again later.
    let h = History {
        txns: vec![
            txn(id(1, 0), OK, 0, 100, vec![r("x", &[]), a("x", e(1, 0))]),
            txn(id(2, 0), OK, 10, 110, vec![r("x", &[]), a("x", e(2, 0))]),
            txn(
                id(3, 0),
                FAIL,
                5,
                50,
                vec![r("y", &[]), a("y", en(3, 0, 0)), a("y", en(3, 0, 1))],
            ),
            txn(id(4, 7), INFO, 200, 210, vec![r("y", &[])]),
        ],
        last: last(&[("x", &[e(2, 0)]), ("y", &[])]),
    };
    let text = h.to_string();
    let back: History = text.parse().unwrap_or_else(|err| panic!("{err}\n{text}"));
    assert_eq!(back, h, "{text}");
    assert_eq!(check(&back), check(&h));

    let no_last = History { last: None, ..h };
    let back: History = no_last.to_string().parse().unwrap();
    assert_eq!(back, no_last);
}

#[test]
fn garbage_text_is_a_parse_error() {
    assert!("this is not a history".parse::<History>().is_err());
}
