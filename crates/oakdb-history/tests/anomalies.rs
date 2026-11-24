//! The checker on histories written by hand: one for each anomaly it must
//! name, and ones it must pass, write skew among them (design 0024 V3).

use std::collections::BTreeMap;

use oakdb_history::{check, Elem, History, Kind, Op, Outcome, Report, Txn, TxnId};

fn t(thread: u32, seq: u32) -> TxnId {
    TxnId::new(thread, seq)
}

fn e(txn: TxnId) -> Elem {
    Elem::new(txn, 0)
}

fn r(key: &str, value: &[Elem]) -> Op {
    Op::Read {
        key: key.into(),
        value: value.to_vec(),
    }
}

fn a(key: &str, elem: Elem) -> Op {
    Op::Append {
        key: key.into(),
        elem,
    }
}

fn txn(id: TxnId, outcome: Outcome, begin: u64, end: u64, ops: Vec<Op>) -> Txn {
    Txn {
        id,
        outcome,
        begin,
        end,
        ops,
    }
}

fn last(keys: &[(&str, &[Elem])]) -> Option<BTreeMap<String, Vec<Elem>>> {
    Some(
        keys.iter()
            .map(|(k, v)| (k.to_string(), v.to_vec()))
            .collect(),
    )
}

fn kinds(report: &Report) -> Vec<Kind> {
    let mut kinds: Vec<Kind> = report.anomalies.iter().map(|a| a.kind()).collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

fn assert_finds(history: &History, kind: Kind) -> Report {
    let report = check(history);
    assert!(
        kinds(&report).contains(&kind),
        "expected {kind:?}, found {:?}\n{history}",
        report.anomalies
    );
    // Every anomaly explains itself in a line.
    for anomaly in &report.anomalies {
        assert!(!anomaly.to_string().is_empty());
    }
    report
}

fn assert_clean(history: &History) -> Report {
    let report = check(history);
    assert!(
        report.anomalies.is_empty(),
        "expected none, found {:?}\n{history}",
        report.anomalies
    );
    report
}

#[test]
fn transactions_one_after_another_are_clean() {
    let (t1, t2, t3) = (t(0, 0), t(1, 0), t(0, 1));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(
                t2,
                Outcome::Ok,
                20,
                30,
                vec![r("x", &[e(t1)]), a("x", e(t2)), r("y", &[])],
            ),
            txn(t3, Outcome::Ok, 40, 50, vec![r("x", &[e(t1), e(t2)])]),
        ],
        last: last(&[("x", &[e(t1), e(t2)]), ("y", &[])]),
    };
    let report = assert_clean(&history);
    assert_eq!(report.write_skews, 0);
}

#[test]
fn a_conflict_that_lost_and_wrote_nothing_is_clean() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Fail, 1, 11, vec![r("x", &[]), a("x", e(t2))]),
        ],
        last: last(&[("x", &[e(t1)])]),
    };
    assert_clean(&history);
}

#[test]
fn two_appends_on_the_same_old_list_are_a_lost_update() {
    // The picture book's scene 1-6 with the conflict check off.
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 1, 11, vec![r("x", &[]), a("x", e(t2))]),
        ],
        last: last(&[("x", &[e(t2)])]),
    };
    assert_finds(&history, Kind::LostUpdate);
}

#[test]
fn two_lists_that_part_ways_are_a_lost_update_without_last_reads() {
    let (t1, t2, t3, t4) = (t(0, 0), t(1, 0), t(2, 0), t(3, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 1, 11, vec![r("x", &[]), a("x", e(t2))]),
            txn(t3, Outcome::Ok, 20, 30, vec![r("x", &[e(t1)])]),
            txn(t4, Outcome::Ok, 40, 50, vec![r("x", &[e(t2)])]),
        ],
        last: None,
    };
    assert_finds(&history, Kind::LostUpdate);
}

#[test]
fn reading_what_a_conflict_wrote_is_an_aborted_read() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Fail, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 5, 15, vec![r("x", &[e(t1)])]),
        ],
        last: last(&[("x", &[])]),
    };
    assert_finds(&history, Kind::AbortedRead);
}

#[test]
fn reading_a_commit_of_unknown_outcome_that_is_gone_at_the_end_is_an_aborted_read() {
    // A commit published before its sync, which then failed (planted bug
    // 3): someone read it, and it is not there once opened again.
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Info, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 5, 8, vec![r("x", &[e(t1)])]),
        ],
        last: last(&[("x", &[])]),
    };
    assert_finds(&history, Kind::AbortedRead);
}

#[test]
fn a_commit_of_unknown_outcome_still_there_at_the_end_counts_as_committed() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Info, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 20, 30, vec![r("x", &[e(t1)])]),
        ],
        last: last(&[("x", &[e(t1)])]),
    };
    assert_clean(&history);
}

#[test]
fn reading_a_name_no_one_wrote_is_an_aborted_read() {
    let history = History {
        txns: vec![txn(
            t(0, 0),
            Outcome::Ok,
            0,
            10,
            vec![r("x", &[e(t(9, 9))])],
        )],
        last: None,
    };
    assert_finds(&history, Kind::AbortedRead);
}

#[test]
fn reading_the_first_of_two_appends_of_another_transaction_is_an_intermediate_read() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let second = Elem::new(t1, 1);
    let history = History {
        txns: vec![
            txn(
                t1,
                Outcome::Ok,
                0,
                10,
                vec![r("x", &[]), a("x", e(t1)), r("x", &[e(t1)]), a("x", second)],
            ),
            txn(t2, Outcome::Ok, 20, 30, vec![r("x", &[e(t1)])]),
        ],
        last: last(&[("x", &[e(t1), second])]),
    };
    assert_finds(&history, Kind::IntermediateRead);
}

#[test]
fn a_key_read_twice_that_changed_in_between_is_a_non_repeatable_read() {
    // The picture book's scene 1-6 with reads of the latest tree.
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 30, vec![r("y", &[]), r("y", &[e(t2)])]),
            txn(t2, Outcome::Ok, 5, 10, vec![r("y", &[]), a("y", e(t2))]),
        ],
        last: last(&[("y", &[e(t2)])]),
    };
    assert_finds(&history, Kind::NonRepeatableRead);
}

#[test]
fn not_seeing_its_own_append_is_a_non_repeatable_read() {
    let t1 = t(0, 0);
    let history = History {
        txns: vec![txn(
            t1,
            Outcome::Ok,
            0,
            10,
            vec![r("x", &[]), a("x", e(t1)), r("x", &[])],
        )],
        last: last(&[("x", &[e(t1)])]),
    };
    assert_finds(&history, Kind::NonRepeatableRead);
}

#[test]
fn seeing_one_key_before_a_commit_and_another_after_it_is_read_skew() {
    // t1 reads x before t2 and y after it: no single moment holds both.
    let (t1, t2) = (t(0, 0), t(1, 0));
    let y2 = Elem::new(t2, 1);
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 40, vec![r("x", &[]), r("y", &[y2])]),
            txn(
                t2,
                Outcome::Ok,
                5,
                10,
                vec![r("x", &[]), a("x", e(t2)), r("y", &[]), a("y", y2)],
            ),
        ],
        last: last(&[("x", &[e(t2)]), ("y", &[y2])]),
    };
    let report = assert_finds(&history, Kind::GSingle);
    assert_eq!(report.write_skews, 0);
}

#[test]
fn seeing_half_of_another_transactions_writes_is_a_fractured_read() {
    // t2 wrote x and y; t1 sees its y but an older x, after t3 also wrote
    // y on top of t2.
    let (t1, t2, t3) = (t(0, 0), t(1, 0), t(2, 0));
    let y2 = Elem::new(t2, 1);
    let history = History {
        txns: vec![
            txn(
                t2,
                Outcome::Ok,
                0,
                10,
                vec![r("x", &[]), a("x", e(t2)), r("y", &[]), a("y", y2)],
            ),
            txn(t3, Outcome::Ok, 1, 12, vec![r("y", &[y2]), a("y", e(t3))]),
            txn(
                t1,
                Outcome::Ok,
                2,
                20,
                vec![r("y", &[y2, e(t3)]), r("x", &[])],
            ),
        ],
        last: last(&[("x", &[e(t2)]), ("y", &[y2, e(t3)])]),
    };
    assert_finds(&history, Kind::GSingle);
}

#[test]
fn write_skew_is_allowed_and_counted() {
    // Both read x and y as empty; one appends to x, the other to y.
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(
                t1,
                Outcome::Ok,
                0,
                10,
                vec![r("x", &[]), r("y", &[]), a("x", e(t1))],
            ),
            txn(
                t2,
                Outcome::Ok,
                1,
                11,
                vec![r("x", &[]), r("y", &[]), a("y", e(t2))],
            ),
        ],
        last: last(&[("x", &[e(t1)]), ("y", &[e(t2)])]),
    };
    let report = assert_clean(&history);
    assert!(report.write_skews >= 1, "{report:?}");
}

#[test]
fn a_commit_that_came_back_before_a_transaction_began_must_be_seen_by_it() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 20, 30, vec![r("x", &[])]),
        ],
        last: last(&[("x", &[e(t1)])]),
    };
    assert_finds(&history, Kind::StaleRead);
}

#[test]
fn a_commit_still_under_way_when_a_transaction_began_need_not_be_seen() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![r("x", &[]), a("x", e(t1))]),
            txn(t2, Outcome::Ok, 9, 30, vec![r("x", &[])]),
        ],
        last: last(&[("x", &[e(t1)])]),
    };
    assert_clean(&history);
}

#[test]
fn each_reading_what_the_other_wrote_is_a_commit_cycle() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let history = History {
        txns: vec![
            txn(
                t1,
                Outcome::Ok,
                0,
                10,
                vec![r("y", &[e(t2)]), r("x", &[]), a("x", e(t1))],
            ),
            txn(
                t2,
                Outcome::Ok,
                0,
                10,
                vec![r("x", &[e(t1)]), r("y", &[]), a("y", e(t2))],
            ),
        ],
        last: last(&[("x", &[e(t1)]), ("y", &[e(t2)])]),
    };
    assert_finds(&history, Kind::CommitCycle);
}

#[test]
fn two_keys_written_in_opposite_orders_are_a_commit_cycle() {
    let (t1, t2) = (t(0, 0), t(1, 0));
    let (y1, y2) = (Elem::new(t1, 1), Elem::new(t2, 1));
    let history = History {
        txns: vec![
            txn(t1, Outcome::Ok, 0, 10, vec![a("x", e(t1)), a("y", y1)]),
            txn(t2, Outcome::Ok, 0, 10, vec![a("x", e(t2)), a("y", y2)]),
        ],
        last: last(&[("x", &[e(t1), e(t2)]), ("y", &[y2, y1])]),
    };
    assert_finds(&history, Kind::CommitCycle);
}
