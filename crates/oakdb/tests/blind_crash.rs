//! Blind tests for ticket 09 (issue #21), written from the documents only:
//! docs/design/0009-crash-experiment.md, docs/experiments/0008-power-cuts.md,
//! docs/design/0008-sim-fs.md and the public doc comments of `fs::sim` and
//! the `crash_sim` harness. The implementation bodies were not read.

#[path = "../examples/crash_sim/harness.rs"]
mod harness;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use harness::{judge, run, Case, Model, Op, Verdict, Workload, KEYS};
use oakdb::fs::sim::{CrashReport, Landing, SimFs, SECTOR};
use oakdb::fs::{Fs, FsFile};
use oakdb::Db;

fn m(pairs: &[(u32, &[u8])]) -> Model {
    pairs.iter().map(|(k, v)| (*k, v.to_vec())).collect()
}

fn apply(model: &mut Model, commit: &[Op]) {
    for op in commit {
        match op {
            Op::Put(k, v) => {
                model.insert(*k, v.clone());
            }
            Op::Delete(k) => {
                model.remove(k);
            }
        }
    }
}

fn state_after(workload: &Workload, commits: usize) -> Model {
    let mut model = Model::new();
    for c in &workload.commits[..commits] {
        apply(&mut model, c);
    }
    model
}

// ---------------------------------------------------------------- ops()

/// 0008 §4 / 0009 §2: ops() counts writes, length changes, syncs, directory
/// creation and opens (existing files too); reads, sizes and locks do not.
#[test]
fn ops_counts_exactly_what_fail_after_counts() {
    let fs = SimFs::new();
    assert_eq!(fs.ops(), 0);
    fs.create_dir_all(Path::new("/d")).unwrap();
    assert_eq!(fs.ops(), 1);
    let f = fs.open(Path::new("/d/f")).unwrap();
    assert_eq!(fs.ops(), 2);
    f.write_all_at(b"hello", 0).unwrap();
    assert_eq!(fs.ops(), 3);
    let mut buf = [0u8; 5];
    f.read_exact_at(&mut buf, 0).unwrap();
    f.size().unwrap();
    f.try_lock().unwrap();
    assert_eq!(fs.ops(), 3, "reads, sizes and locks do not count");
    f.set_len(10).unwrap();
    assert_eq!(fs.ops(), 4);
    f.sync_data().unwrap();
    assert_eq!(fs.ops(), 5);
    fs.sync_dir(Path::new("/d")).unwrap();
    assert_eq!(fs.ops(), 6);
    drop(f);
    let _again = fs.open(Path::new("/d/f")).unwrap();
    assert_eq!(fs.ops(), 7, "opening an existing file counts too");
}

/// fail_after(n) counts from now ("n more operations"); the op that finds
/// the power gone fails and is not counted; every call after it fails.
#[test]
fn a_failed_operation_does_not_count() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/d")).unwrap();
    let f = fs.open(Path::new("/d/f")).unwrap();
    let before = fs.ops();
    fs.fail_after(2);
    f.write_all_at(b"a", 0).unwrap();
    f.write_all_at(b"b", 1).unwrap();
    assert!(f.write_all_at(b"c", 2).is_err());
    assert!(f.sync_data().is_err());
    let mut buf = [0u8; 1];
    assert!(
        f.read_exact_at(&mut buf, 0).is_err(),
        "reads fail too once it is down"
    );
    assert_eq!(fs.ops(), before + 2);
}

/// 0009 §4: "정전 뒤에도 이어 셈" — the counter is not reset by a crash.
#[test]
fn ops_keeps_counting_after_a_crash() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/d")).unwrap();
    let f = fs.open(Path::new("/d/f")).unwrap();
    fs.fail_after(1);
    f.write_all_at(b"a", 0).unwrap();
    assert!(f.write_all_at(b"b", 1).is_err());
    let before = fs.ops();
    fs.crash(1);
    assert_eq!(
        fs.ops(),
        before,
        "a crash does not reset or add to the count"
    );
    let g = fs.open(Path::new("/d/f")).unwrap();
    g.write_all_at(b"x", 0).unwrap();
    assert_eq!(fs.ops(), before + 2);
}

/// `Workload::total_ops` agrees with what the engine API really does on a
/// fresh SimFs.
#[test]
fn total_ops_matches_a_real_run_through_the_engine() {
    for seed in [0u64, 7, 42, 1000, 1680] {
        let workload = Workload::generate(seed);
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), harness::DIR).unwrap();
        for commit in &workload.commits {
            let mut txn = db.begin();
            for op in commit {
                match op {
                    Op::Put(k, v) => txn.put(&k.to_be_bytes(), v).unwrap(),
                    Op::Delete(k) => txn.delete(&k.to_be_bytes()).unwrap(),
                }
            }
            txn.commit().unwrap();
        }
        assert_eq!(fs.ops(), workload.total_ops(), "seed {seed}");
    }
}

// ---------------------------------------------------------------- crash report

fn read_file(fs: &SimFs, path: &Path) -> Vec<u8> {
    let f = fs.open(path).unwrap();
    let len = f.size().unwrap() as usize;
    let mut buf = vec![0u8; len];
    f.read_exact_at(&mut buf, 0).unwrap();
    buf
}

/// Every unsynced write in the report agrees with the bytes on the disk
/// afterwards, torn ones sector by sector (sectors counted from the file
/// start, 0008 §3).
#[test]
fn crash_report_agrees_with_the_disk_sector_by_sector() {
    let path = PathBuf::from("/d/f");
    // Non-overlapping writes inside an already synced 4096-byte file.
    let writes: [(u64, usize, u8); 3] = [(100, 1000, 0xA1), (1536, 512, 0xB2), (2100, 1900, 0xC3)];
    let mut seen_torn = 0;
    for seed in 0..200u64 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let f = fs.open(&path).unwrap();
        f.write_all_at(&vec![0u8; 4096], 0).unwrap();
        f.sync_data().unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        for (off, len, fill) in writes {
            f.write_all_at(&vec![fill; len], off).unwrap();
        }
        drop(f);
        let report = fs.crash(seed);
        assert!(report.vanished.is_empty());
        assert!(report.lengths.is_empty());
        assert_eq!(report.writes.len(), 3, "seed {seed}");
        let disk = read_file(&fs, &path);
        assert_eq!(disk.len(), 4096);
        for (fate, (off, len, fill)) in report.writes.iter().zip(writes) {
            assert_eq!(
                (fate.path.as_path(), fate.offset, fate.len),
                (path.as_path(), off, len),
                "oldest first"
            );
            let end = off + len as u64;
            let first = off / SECTOR;
            let last = (end - 1) / SECTOR;
            for byte in off..end {
                let landed = match &fate.landing {
                    Landing::Kept => true,
                    Landing::Lost => false,
                    Landing::Torn { sectors } => {
                        assert_eq!(sectors.len() as u64, last - first + 1, "sectors it touches");
                        sectors[(byte / SECTOR - first) as usize]
                    }
                };
                let want = if landed { fill } else { 0 };
                assert_eq!(
                    disk[byte as usize], want,
                    "seed {seed} byte {byte} {:?}",
                    fate.landing
                );
            }
            if matches!(fate.landing, Landing::Torn { .. }) {
                seen_torn += 1;
            }
        }
    }
    assert!(seen_torn > 0);
}

/// Unsynced length changes: `landed` agrees with the size afterwards.
#[test]
fn crash_report_length_changes_agree_with_the_size() {
    let mut both = BTreeSet::new();
    for seed in 0..100u64 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let f = fs.open(Path::new("/d/f")).unwrap();
        f.write_all_at(&[7u8; 1024], 0).unwrap();
        f.sync_data().unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        f.set_len(3000).unwrap();
        drop(f);
        let report = fs.crash(seed);
        assert_eq!(report.lengths.len(), 1);
        let fate = &report.lengths[0];
        assert_eq!(fate.len, 3000);
        let size = fs.open(Path::new("/d/f")).unwrap().size().unwrap();
        assert_eq!(size, if fate.landed { 3000 } else { 1024 }, "seed {seed}");
        both.insert(fate.landed);
    }
    assert_eq!(both.len(), 2, "length changes both land and vanish");
}

/// A file whose directory was never synced vanishes or not; the report's
/// `vanished` agrees with `exists`, and a vanished file has no write fates.
#[test]
fn crash_report_vanished_files_agree_with_exists() {
    let mut outcomes = BTreeSet::new();
    for seed in 0..100u64 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let kept = fs.open(Path::new("/d/a")).unwrap();
        kept.sync_data().unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        let fresh = fs.open(Path::new("/d/b")).unwrap();
        fresh.write_all_at(b"data", 0).unwrap();
        fresh.sync_data().unwrap();
        drop((kept, fresh));
        let report = fs.crash(seed);
        let gone = !fs.exists(Path::new("/d/b"));
        assert_eq!(
            report.vanished,
            if gone {
                vec![PathBuf::from("/d/b")]
            } else {
                vec![]
            },
            "seed {seed}"
        );
        assert!(fs.exists(Path::new("/d/a")));
        assert!(report
            .writes
            .iter()
            .all(|w| w.path != Path::new("/d/b") || !gone));
        outcomes.insert(gone);
    }
    assert_eq!(outcomes.len(), 2);
}

/// Synced writes never show in the report; unsynced ones list files in path
/// order.
#[test]
fn crash_report_lists_only_unsynced_writes_in_path_order() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/d")).unwrap();
    let b = fs.open(Path::new("/d/b")).unwrap();
    let a = fs.open(Path::new("/d/a")).unwrap();
    fs.sync_dir(Path::new("/d")).unwrap();
    b.write_all_at(b"synced", 0).unwrap();
    b.sync_data().unwrap();
    b.write_all_at(b"B", 0).unwrap();
    a.write_all_at(b"A", 0).unwrap();
    drop((a, b));
    let report = fs.crash(3);
    let paths: Vec<_> = report.writes.iter().map(|w| w.path.clone()).collect();
    assert_eq!(paths, vec![PathBuf::from("/d/a"), PathBuf::from("/d/b")]);
    assert!(report.writes.iter().all(|w| w.len == 1));
}

/// 0008 §3: a torn write past the end leaves the file "마지막으로 남은 칸의 끝까지".
#[test]
#[ignore = "보류: 문서가 '마지막으로 남은 칸의 끝'이 칸 경계(512 배수)인지 쓰기의 끝까지로 자른 것인지 말하지 않음"]
fn torn_write_past_the_end_sets_the_length_to_the_last_landed_sector() {
    for seed in 0..200u64 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let f = fs.open(Path::new("/d/f")).unwrap();
        f.sync_data().unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        f.write_all_at(&[9u8; 1000], 100).unwrap(); // 100..1100: sectors 0,1,2
        drop(f);
        let report: CrashReport = fs.crash(seed);
        if let Landing::Torn { sectors } = &report.writes[0].landing {
            let size = fs.open(Path::new("/d/f")).unwrap().size().unwrap();
            let want = match sectors.iter().rposition(|&s| s) {
                Some(i) => ((i as u64 + 1) * SECTOR).min(1100),
                None => 0,
            };
            assert_eq!(size, want, "seed {seed} {sectors:?}");
        }
    }
}

/// Same seed, same disk and same report; different seeds mostly differ.
#[test]
fn same_crash_seed_gives_the_same_report() {
    let make = || {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let f = fs.open(Path::new("/d/f")).unwrap();
        for i in 0..6u64 {
            f.write_all_at(&[i as u8 + 1; 2048], i * 2048).unwrap();
        }
        drop(f);
        fs
    };
    let mut distinct = BTreeSet::new();
    for seed in 0..20u64 {
        let (x, y) = (make(), make());
        let (rx, ry) = (x.crash(seed), y.crash(seed));
        assert_eq!(rx, ry);
        assert_eq!(x.exists(Path::new("/d/f")), y.exists(Path::new("/d/f")));
        if x.exists(Path::new("/d/f")) {
            assert_eq!(
                read_file(&x, Path::new("/d/f")),
                read_file(&y, Path::new("/d/f"))
            );
        }
        distinct.insert(format!("{rx:?}"));
    }
    assert!(distinct.len() > 10, "different seeds mostly differ");
}

// ---------------------------------------------------------------- Case / Run

#[test]
fn same_seed_same_case_and_run_different_seeds_differ() {
    for seed in [0u64, 1, 159, 1680] {
        assert_eq!(Case::from_seed(seed), Case::from_seed(seed));
        assert_eq!(Case::from_seed(seed).run(), Case::from_seed(seed).run());
    }
    let cases: BTreeSet<String> = (0..50u64)
        .map(|s| format!("{:?}", Case::from_seed(s)))
        .collect();
    assert_eq!(cases.len(), 50);
    let a = Case::from_seed(10);
    let b = Case::from_seed(11);
    assert_ne!(a.workload, b.workload);
    assert_ne!(a.crash_seed, b.crash_seed);
}

/// Design decision 3: 1~30 commits, 1~8 ops each, keys 0..200, about 1/6 of
/// values long enough for overflow pages; cut point inside the total.
#[test]
fn cases_stay_inside_the_documented_shape() {
    let (mut values, mut long) = (0usize, 0usize);
    for seed in 0..500u64 {
        let case = Case::from_seed(seed);
        let w = &case.workload;
        assert!((1..=30).contains(&w.commits.len()), "seed {seed}");
        for c in &w.commits {
            assert!((1..=8).contains(&c.len()), "seed {seed}");
            for op in c {
                let k = match op {
                    Op::Put(k, v) => {
                        values += 1;
                        if v.len() > 4096 {
                            long += 1;
                        }
                        *k
                    }
                    Op::Delete(k) => *k,
                };
                assert!(k < KEYS);
            }
        }
        assert!(
            case.cut_after <= w.total_ops(),
            "seed {seed}: cut inside the total"
        );
    }
    let share = long as f64 / values as f64;
    assert!((0.08..0.30).contains(&share), "long values {share}");
}

/// Run's states agree with the workload: acked_state is the first `acked`
/// commits; cut_commit, if any, is the next one and cut_state includes it;
/// an Intact verdict means `after` is one of the two.
#[test]
fn run_states_agree_with_the_workload() {
    for seed in 0..150u64 {
        let case = Case::from_seed(seed);
        let r = case.run();
        assert_eq!(
            r.acked_state,
            state_after(&case.workload, r.acked),
            "seed {seed}"
        );
        match r.cut_commit {
            Some(c) => {
                assert_eq!(c, r.acked, "seed {seed}");
                assert_eq!(
                    r.cut_state.as_ref(),
                    Some(&state_after(&case.workload, c + 1))
                );
            }
            None => assert!(r.cut_state.is_none()),
        }
        if let Verdict::Intact { cut_commit_landed } = r.verdict {
            let want = if cut_commit_landed {
                r.cut_state.as_ref().unwrap()
            } else {
                &r.acked_state
            };
            assert_eq!(r.after.as_ref(), Some(want), "seed {seed}");
        }
        assert_eq!(r.verdict, judge_or_keep(&r));
    }
}

fn judge_or_keep(r: &harness::Run) -> Verdict {
    match &r.after {
        Some(after) => judge(after, &r.acked_state, r.cut_state.as_ref()),
        None => r.verdict.clone(),
    }
}

#[test]
fn run_without_a_power_cut_keeps_every_commit() {
    for seed in [0u64, 7, 42, 1000] {
        let w = Workload::generate(seed);
        let total = w.total_ops();
        for cut_after in [total, total + 1, total + 100, u64::MAX] {
            let r = run(&w, cut_after, seed);
            assert_eq!(r.acked, w.commits.len(), "seed {seed} cut {cut_after}");
            assert_eq!(r.cut_commit, None);
            assert_eq!(
                r.verdict,
                Verdict::Intact {
                    cut_commit_landed: false
                }
            );
            assert_eq!(r.after.as_ref(), Some(&state_after(&w, w.commits.len())));
            assert!(r.report.vanished.is_empty());
        }
    }
}

/// One op short of the total cuts the last commit.
#[test]
fn cut_one_short_of_the_total_cuts_the_last_commit() {
    for seed in [0u64, 7, 42] {
        let w = Workload::generate(seed);
        let r = run(&w, w.total_ops() - 1, seed);
        assert_eq!(r.acked, w.commits.len() - 1, "seed {seed}");
        assert_eq!(r.cut_commit, Some(w.commits.len() - 1));
    }
}

/// 0009 §4: "만드는 도중 맨 처음 끊기면 잃는 것 없음". The experiment's
/// shortest intact seed 124 is "cut after 0".
#[test]
fn cut_at_the_very_start_loses_nothing() {
    let w = Workload::generate(3);
    let r = run(&w, 0, 99);
    assert_eq!(r.acked, 0);
    assert_eq!(r.cut_commit, None);
    assert!(r.verdict.is_intact(), "{:?}", r.verdict);
    assert_eq!(r.after, Some(Model::new()));
    let c = Case::from_seed(124);
    assert_eq!(c.cut_after, 0);
    assert!(c.run().verdict.is_intact());
}

// ---------------------------------------------------------------- judge

#[test]
fn judge_accepts_old_and_new_states() {
    let acked = m(&[(1, b"a"), (2, b"b")]);
    let cut = m(&[(1, b"A"), (2, b"b"), (3, b"c")]);
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
    let acked = m(&[(1, b"a"), (2, b"b"), (5, b"e")]);
    let cut = m(&[(1, b"A"), (2, b"B"), (5, b"e")]);
    // Untouched key 5 lost / changed.
    assert_eq!(
        judge(&m(&[(1, b"a"), (2, b"b")]), &acked, Some(&cut)),
        Verdict::LostAcknowledged(vec![5])
    );
    assert_eq!(
        judge(&m(&[(1, b"a"), (2, b"b"), (5, b"x")]), &acked, Some(&cut)),
        Verdict::LostAcknowledged(vec![5])
    );
    // Touched key 2 holds neither old nor new.
    assert_eq!(
        judge(&m(&[(1, b"a"), (2, b"zz"), (5, b"e")]), &acked, Some(&cut)),
        Verdict::WrongValue(vec![2])
    );
    // Touched key 2 gone: neither old nor new.
    assert_eq!(
        judge(&m(&[(1, b"a"), (5, b"e")]), &acked, Some(&cut)),
        Verdict::WrongValue(vec![2])
    );
    // Mixed old and new.
    match judge(&m(&[(1, b"A"), (2, b"b"), (5, b"e")]), &acked, Some(&cut)) {
        Verdict::HalfCommit(keys) => {
            assert!(keys.contains(&1) && keys.contains(&2), "{keys:?}");
            assert!(
                !keys.contains(&5),
                "an untouched key is not part of the cut commit"
            );
        }
        v => panic!("{v:?}"),
    }
}

#[test]
fn judge_reports_the_worst_finding() {
    let acked = m(&[(1, b"a"), (2, b"b"), (3, b"c"), (9, b"z")]);
    let cut = m(&[(1, b"A"), (2, b"B"), (3, b"C"), (9, b"z")]);
    // All three: half (1 new, 2 old), wrong (3), lost (9).
    let all = m(&[(1, b"A"), (2, b"b"), (3, b"?")]);
    assert_eq!(
        judge(&all, &acked, Some(&cut)),
        Verdict::LostAcknowledged(vec![9])
    );
    // Wrong and half.
    let two = m(&[(1, b"A"), (2, b"b"), (3, b"?"), (9, b"z")]);
    assert_eq!(
        judge(&two, &acked, Some(&cut)),
        Verdict::WrongValue(vec![3])
    );
}

#[test]
fn judge_with_a_cut_commit_that_only_deletes() {
    let acked = m(&[(1, b"a"), (2, b"b"), (3, b"c")]);
    let cut = m(&[(3, b"c")]);
    assert_eq!(
        judge(&cut, &acked, Some(&cut)),
        Verdict::Intact {
            cut_commit_landed: true
        }
    );
    assert_eq!(
        judge(&acked, &acked, Some(&cut)),
        Verdict::Intact {
            cut_commit_landed: false
        }
    );
    assert!(matches!(
        judge(&m(&[(1, b"a"), (3, b"c")]), &acked, Some(&cut)),
        Verdict::HalfCommit(_)
    ));
}

/// A rewrite with the same value is invisible in the models: the key counts
/// as untouched, so it never makes a half commit.
#[test]
fn judge_with_a_same_value_rewrite() {
    let acked = m(&[(1, b"a"), (2, b"b")]);
    let cut = m(&[(1, b"A"), (2, b"b")]); // commit put 1=A and 2=b (same)
    assert_eq!(
        judge(&cut, &acked, Some(&cut)),
        Verdict::Intact {
            cut_commit_landed: true
        }
    );
    assert_eq!(
        judge(&acked, &acked, Some(&cut)),
        Verdict::Intact {
            cut_commit_landed: false
        }
    );
    // Cut commit rewrites only with the same values: both states are one.
    assert!(judge(&acked, &acked, Some(&acked)).is_intact());
}

/// With no cut commit, every key is untouched.
#[test]
fn judge_without_a_cut_commit() {
    let acked = m(&[(1, b"a"), (2, b"b")]);
    assert_eq!(
        judge(&m(&[(1, b"a")]), &acked, None),
        Verdict::LostAcknowledged(vec![2])
    );
    assert_eq!(
        judge(&m(&[(1, b"x"), (2, b"b")]), &acked, None),
        Verdict::LostAcknowledged(vec![1])
    );
    assert!(judge(&Model::new(), &Model::new(), None).is_intact());
}

/// A key absent in both states that shows up after the crash. Design table:
/// LostAcknowledged = "건드리지 않은 키가 응답한 값과 다름"; Verdict doc:
/// WrongValue = "Keys hold a value no commit ever gave them".
#[test]
#[ignore = "보류: 건드리지 않은 키에 없던 값이 생기면 설계 표로는 LostAcknowledged, Verdict 주석으로는 WrongValue"]
fn judge_untouched_key_that_appears_from_nowhere() {
    let acked = m(&[(1, b"a")]);
    let cut = m(&[(1, b"A")]);
    let after = m(&[(1, b"a"), (7, b"ghost")]);
    assert_eq!(
        judge(&after, &acked, Some(&cut)),
        Verdict::WrongValue(vec![7])
    );
}

// ---------------------------------------------------------------- the experiment record

fn open_failed_with(v: &Verdict, text: &str) -> bool {
    matches!(v, Verdict::OpenFailed(e) if e.contains(text))
}

/// The shortest seeds named in docs/experiments/0008-power-cuts.md.
#[test]
fn the_seeds_named_in_the_experiment_replay_as_described() {
    // 2. torn page 2 (seed 84): "sectors #.###..#", wrong checksum.
    let r = Case::from_seed(84).run();
    assert!(
        open_failed_with(&r.verdict, "page 2 has a wrong checksum"),
        "{:?}",
        r.verdict
    );
    assert_eq!(
        r.report
            .writes
            .iter()
            .map(|w| w.landing.clone())
            .collect::<Vec<_>>(),
        vec![Landing::Torn {
            sectors: "#.###..#".chars().map(|c| c == '#').collect()
        }]
    );
    assert!(open_failed_with(
        &Case::from_seed(495).run().verdict,
        "wrong checksum"
    ));

    // 1. half-made database (184: after 3 ops, not an Oakdb file; 536: shorter).
    let c = Case::from_seed(184);
    assert_eq!(c.cut_after, 3);
    let r = c.run();
    assert_eq!(r.cut_commit, None);
    assert!(
        open_failed_with(&r.verdict, "not an Oakdb data file"),
        "{:?}",
        r.verdict
    );
    let r = Case::from_seed(536).run();
    assert_eq!(r.cut_commit, None);
    assert!(
        open_failed_with(&r.verdict, "shorter than its meta page says"),
        "{:?}",
        r.verdict
    );

    // 3. leaf kept, the page it points to gone (213, 319).
    let r = Case::from_seed(213).run();
    assert_eq!(r.cut_commit, Some(0));
    assert!(
        open_failed_with(&r.verdict, "page number is outside the file"),
        "{:?}",
        r.verdict
    );
    let r = Case::from_seed(319).run();
    assert!(
        open_failed_with(&r.verdict, "leaves do not point to the next leaf"),
        "{:?}",
        r.verdict
    );
    let kinds: Vec<_> = r
        .report
        .writes
        .iter()
        .map(|w| std::mem::discriminant(&w.landing))
        .collect();
    assert_eq!(kinds.len(), 4, "four pages 2..5");
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == std::mem::discriminant(&Landing::Kept))
            .count(),
        2
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == std::mem::discriminant(&Landing::Lost))
            .count(),
        1
    );

    // 4. half commit (1680): commit 5, keys 24 55 new, 105 106 140 old.
    let r = Case::from_seed(1680).run();
    assert_eq!((r.acked, r.cut_commit), (5, Some(5)));
    assert_eq!(r.verdict, Verdict::HalfCommit(vec![24, 55, 105, 106, 140]));
    let after = r.after.as_ref().unwrap();
    let cut = r.cut_state.as_ref().unwrap();
    for k in [24, 55] {
        assert_eq!(after.get(&k), cut.get(&k));
    }
    for k in [105, 106, 140] {
        assert_eq!(after.get(&k), r.acked_state.get(&k));
    }
    assert_eq!(after[&24].len(), 225);
    assert_eq!(r.acked_state[&106].len(), 310);

    // 5. a perfect disk still breaks (159): no unsynced writes, commit 11.
    let r = Case::from_seed(159).run();
    assert_eq!(r.cut_commit, Some(11));
    assert!(r.report.writes.is_empty() && r.report.lengths.is_empty());
    assert!(
        open_failed_with(&r.verdict, "page is reached twice"),
        "{:?}",
        r.verdict
    );
}

/// The engine bug: seeds 529 and 7872 panic on reopening in a debug build,
/// and fail to open in release.
#[test]
fn the_panic_seeds_panic_in_debug_and_fail_in_release() {
    for seed in [529u64, 7872] {
        let v = Case::from_seed(seed).run().verdict;
        if cfg!(debug_assertions) {
            assert!(matches!(v, Verdict::Panicked(_)), "seed {seed}: {v:?}");
        } else {
            assert!(matches!(v, Verdict::OpenFailed(_)), "seed {seed}: {v:?}");
        }
    }
}

/// The "every cut point" table: workload seed → commits, cut points.
#[test]
fn the_every_table_workloads_have_the_documented_size() {
    for (seed, commits, points) in [(7u64, 28usize, 228u64), (42, 14, 97), (1000, 17, 109)] {
        let w = Workload::generate(seed);
        assert_eq!(w.commits.len(), commits, "workload {seed}");
        assert!(
            w.total_ops() == points || w.total_ops() + 1 == points,
            "workload {seed}: {} ops vs {points} cut points",
            w.total_ops()
        );
    }
}

/// The record's headline: no seed loses an acknowledged commit or invents a
/// value (0009 §4 keeps seeds 0..1999 as a standing test).
#[test]
fn no_seed_loses_an_acknowledged_commit_or_invents_a_value() {
    let mut intact = 0;
    for seed in 0..2000u64 {
        let v = Case::from_seed(seed).run().verdict;
        assert!(
            !matches!(v, Verdict::LostAcknowledged(_) | Verdict::WrongValue(_)),
            "seed {seed}: {v:?}"
        );
        if v.is_intact() {
            intact += 1;
        }
    }
    // The record says about half keep the promise.
    assert!((800..1200).contains(&intact), "{intact} of 2000 intact");
}
