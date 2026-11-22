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
fn workloads_checkpoint_along_the_way() {
    let with = (0..50)
        .filter(|&s| !Workload::generate(s).checkpoints.is_empty())
        .count();
    assert!(with > 25, "{with} of 50 workloads checkpoint");
}

#[test]
fn without_a_power_cut_every_commit_is_there() {
    for seed in 0..20 {
        let workload = Workload::generate(seed);
        let total = workload.total_ops();
        let run = run(&workload, total, seed);
        assert_eq!(run.acked_commits, workload.commits.len(), "seed {seed}");
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
        assert_eq!(run.acked_commits, 0);
        assert_eq!(run.cut_commit, None);
        assert!(run.verdict.is_intact(), "{:?}", run.verdict);
    }
}

#[test]
fn every_seed_keeps_the_promise() {
    // With the WAL (#22): every acknowledged commit is there, and the cut
    // commit is all there or not at all, for every seed.
    for seed in 0..2000 {
        let verdict = Case::from_seed(seed).run().verdict;
        assert!(verdict.is_intact(), "seed {seed}: {verdict:?}");
    }
}

// Pinned seeds. The first nine broke the design before the WAL
// (docs/experiments/0008), one of each way of breaking; the engine has
// changed since, so the same seed now cuts somewhere else, and each comment
// says what it does today (`crash_sim one <seed>`, issue #56, and again for
// #59, whose opening takes more operations). They stay as seeds that once
// broke and must keep the promise.

fn stays_intact(seed: u64) {
    let run = Case::from_seed(seed).run();
    assert!(run.verdict.is_intact(), "seed {seed}: {:?}", run.verdict);
}

/// Once: a cut while creating left a file opened as a database and
/// refused. Now: the power goes while opening writes the new WAL, before
/// the directory is synced, and both WAL files vanish. Opening creates the
/// database again.
#[test]
fn a_cut_while_creating_leaves_a_file_that_can_be_opened() {
    stays_intact(184);
}

/// Once: a cut after the meta page while creating. Now: the power goes
/// in commit 1, whose WAL write is lost; the acknowledged commit is
/// there.
#[test]
fn a_cut_while_creating_after_the_meta_page_is_written() {
    stays_intact(536);
}

/// Once: a commit's leaf landed without the new pages it points to. Now:
/// the power goes in the checkpoint after commit 0, with nothing
/// unsynced.
#[test]
fn a_page_lands_without_the_new_pages_it_points_to() {
    stays_intact(213);
}

/// Once: a torn leaf. Now: the power goes in commit 4, whose WAL write is
/// torn; the four acknowledged commits are there.
#[test]
fn a_torn_page_is_repaired() {
    stays_intact(84);
}

/// Once: a split cut part way. Now: the power goes in the checkpoint
/// after commit 13 while it moves pages into the data file; some are
/// kept, some lost or torn, and one past the end lands its length alone.
/// The WAL still holds them all.
#[test]
fn a_split_cut_part_way() {
    stays_intact(319);
}

/// Once: a commit over two leaves landed half. Now: 1680 cuts in commit
/// 6, whose WAL write of pages 4..11 lands whole, so the cut commit is
/// there in full; 406 cuts in the checkpoint after commit 19 while it
/// moves pages, and the WAL still holds them all.
#[test]
fn a_commit_over_two_leaves_lands_whole_or_not_at_all() {
    stays_intact(1680);
    stays_intact(406);
}

/// Once: a cut between the page sync and the meta page. Now: a cut in the
/// checkpoint after commit 17, while it moves pages into the data file;
/// some are lost or torn, and the WAL still holds them all.
#[test]
fn a_cut_between_the_page_sync_and_the_meta_page() {
    stays_intact(159);
}

/// Once: a free list head on a page in use made opening panic on a
/// `debug_assert`. Now: a cut in commit 10, whose WAL write is lost.
#[test]
fn a_free_list_head_on_a_page_in_use_is_refused_without_panicking() {
    let run = Case::from_seed(529).run();
    assert!(
        !matches!(run.verdict, Verdict::Panicked(_)),
        "{:?}",
        run.verdict
    );
}

/// A cut at the sync after a checkpoint wrote the WAL header, which
/// landed its length alone: 20 bytes of zeros (issue #56). Opening takes
/// them for no header and writes it again; refused as a damaged header
/// before review P1 of PR #55 (mutant C16). Seed 109 did this until #59
/// changed how many operations opening takes; 99 does it now.
#[test]
fn a_wal_header_that_landed_as_zeros_opens() {
    stays_intact(99);
}
