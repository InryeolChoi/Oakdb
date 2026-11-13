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

#[test]
fn no_seed_loses_an_acknowledged_commit_or_makes_up_a_value() {
    // What the current design does keep: a commit is only acknowledged
    // after its pages are synced, so a power cut can break the database
    // but has not been seen to lose an acknowledged commit silently.
    for seed in 0..2000 {
        let verdict = Case::from_seed(seed).run().verdict;
        assert!(
            !matches!(
                verdict,
                Verdict::LostAcknowledged(_) | Verdict::WrongValue(_)
            ),
            "seed {seed}: {verdict:?}"
        );
    }
}

// Seeds where the current design breaks (docs/experiments/0008). Each is
// the shortest example of its kind in seeds 0..600. They must all pass
// once the WAL (#22) lands; until then they are ignored.
// Replay one with `cargo run --example crash_sim -- one <seed>`.

fn stays_intact(seed: u64) {
    let run = Case::from_seed(seed).run();
    assert!(run.verdict.is_intact(), "seed {seed}: {:?}", run.verdict);
}

/// Cut while creating the database: the first leaf landed, the meta page
/// did not, and the file is no longer empty, so it is opened as a database
/// and refused ("not an Oakdb data file").
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_cut_while_creating_leaves_a_file_that_can_be_opened() {
    stays_intact(184);
}

/// Cut while creating: the meta page landed, the leaf did not ("data file
/// is shorter than its meta page says").
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_cut_while_creating_after_the_meta_page_is_written() {
    stays_intact(536);
}

/// The first commit's leaf landed, but the overflow pages it points to,
/// past the old end of the file, did not ("page number is outside the
/// file").
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_leaf_lands_without_its_new_overflow_pages() {
    stays_intact(213);
}

/// A torn leaf: the checksum notices, but nothing can repair it yet
/// ("page 2 has a wrong checksum").
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_torn_page_is_repaired() {
    stays_intact(495);
}

/// A split part way: leaves landed, lost and torn, so the sibling chain
/// breaks ("leaves do not point to the next leaf").
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_split_cut_part_way() {
    stays_intact(319);
}

/// A commit over two leaves: one landed, the other was never written, so
/// half its keys are new ("half a commit").
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_commit_over_two_leaves_lands_whole_or_not_at_all() {
    stays_intact(1680);
    stays_intact(406);
}

/// Every page write was synced and nothing was lost or torn; the power
/// went as the meta page was about to be written. Overwritten pages and
/// the old meta page disagree ("page is reached twice"): the design breaks
/// even on a disk that never loses a write.
#[test]
#[ignore = "needs the WAL (#22)"]
fn a_cut_between_the_page_sync_and_the_meta_page() {
    stays_intact(159);
}

/// The meta page write was lost, so the old free list head now names a
/// page that has since been reused. Opening checks the free list, but
/// `successors` reads each page's next before the check sees the page, so
/// a debug build panics on a `debug_assert` instead of refusing the file.
/// Not a WAL matter: found by this experiment, waiting on a decision.
#[test]
#[ignore = "engine bug found by this experiment (PR for #21)"]
fn a_free_list_head_on_a_page_in_use_is_refused_without_panicking() {
    let run = Case::from_seed(529).run();
    assert!(
        !matches!(run.verdict, Verdict::Panicked(_)),
        "{:?}",
        run.verdict
    );
}
