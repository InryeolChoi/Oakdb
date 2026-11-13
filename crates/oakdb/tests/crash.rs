//! The power-cut experiment's harness, and the cases it found where the
//! current design breaks (issue #21, docs/experiments/0008).

#[path = "../examples/crash_sim/harness.rs"]
mod harness;

use harness::{judge, run, Case, Model, Verdict, Workload};

fn model(entries: &[(u32, &[u8])]) -> Model {
    entries.iter().map(|&(k, v)| (k, v.to_vec())).collect()
}

#[test]
fn judge_accepts_the_state_before_or_after_the_cut_commit() {
    let acked = model(&[(1, b"a"), (2, b"b")]);
    let cut = model(&[(1, b"a"), (2, b"B"), (3, b"c")]);
    assert_eq!(
        judge(&acked, &acked, Some(&cut)),
        Verdict::Intact {
            cut_commit_landed: false
        }
    );
    assert_eq!(
        judge(&cut, &acked, Some(&cut)),
        Verdict::Intact {
            cut_commit_landed: true
        }
    );
    assert_eq!(
        judge(&acked, &acked, None),
        Verdict::Intact {
            cut_commit_landed: false
        }
    );
}

#[test]
fn judge_names_each_way_of_breaking() {
    let acked = model(&[(1, b"a"), (2, b"b")]);
    let cut = model(&[(1, b"a"), (2, b"B"), (3, b"c")]);
    // Key 1 was not touched by the cut commit, so losing it loses an
    // acknowledged commit.
    assert_eq!(
        judge(&model(&[(2, b"b")]), &acked, Some(&cut)),
        Verdict::LostAcknowledged(vec![1])
    );
    // Key 3 landed, key 2 did not: half of the cut commit.
    assert_eq!(
        judge(
            &model(&[(1, b"a"), (2, b"b"), (3, b"c")]),
            &acked,
            Some(&cut)
        ),
        Verdict::HalfCommit(vec![2, 3])
    );
    assert_eq!(
        judge(&model(&[(1, b"a"), (2, b"?")]), &acked, Some(&cut)),
        Verdict::WrongValue(vec![2])
    );
    // A wrong value is worse than a half commit, and losing an
    // acknowledged commit worse than both.
    assert_eq!(
        judge(&model(&[(2, b"?"), (3, b"c")]), &acked, Some(&cut)),
        Verdict::LostAcknowledged(vec![1])
    );
}

#[test]
fn the_same_seed_gives_the_same_case_and_the_same_run() {
    for seed in 0..20 {
        let case = Case::from_seed(seed);
        assert_eq!(case, Case::from_seed(seed));
        assert!(case.cut_after < case.workload.total_ops());
        assert_eq!(
            run(&case.workload, case.cut_after, case.crash_seed),
            run(&case.workload, case.cut_after, case.crash_seed),
            "seed {seed}"
        );
    }
    assert_ne!(Case::from_seed(1).workload, Case::from_seed(2).workload);
}

#[test]
fn workloads_split_leaves_and_use_overflow_pages() {
    // Enough data for a tree of several leaves, and some long values.
    let long = (0..50)
        .map(Workload::generate)
        .flat_map(|w| w.commits.into_iter().flatten())
        .filter(|op| matches!(op, harness::Op::Put(_, v) if v.len() > 1006))
        .count();
    assert!(long > 0);
    assert!((0..50).any(|s| Workload::generate(s).commits.len() >= 20));
}

#[test]
fn without_a_power_cut_every_commit_is_there() {
    for seed in 0..20 {
        let workload = Workload::generate(seed);
        let total = workload.total_ops();
        let run = run(&workload, total, seed);
        assert_eq!(run.acked, workload.commits.len(), "seed {seed}");
        assert_eq!(run.cut_commit, None);
        assert_eq!(
            run.verdict,
            Verdict::Intact {
                cut_commit_landed: false
            }
        );
    }
}

#[test]
fn a_cut_while_opening_a_new_database_loses_nothing_it_promised() {
    for crash_seed in 0..20 {
        let run = run(&Workload::generate(7), 0, crash_seed);
        assert_eq!(run.acked, 0);
        assert_eq!(run.cut_commit, None);
        assert!(run.verdict.is_intact(), "{:?}", run.verdict);
    }
}
