//! Power cuts on the simulated file system (issue #20).
//!
//! On a crash, whatever was synced stays. Each write since the last sync is
//! kept, lost, or torn (some of its 512-byte sectors new, the rest old); a
//! write past the end of the file may land its new length alone, with
//! zeros where its bytes would be (issue #56); and a file created since its
//! directory was last synced may vanish. A seed makes every choice, so the
//! same seed always gives the same disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use oakdb::fs::sim::{FailedSync, Landing, LengthFate, SimFs, WriteFate, SECTOR};
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, DATA_FILE};

const SEEDS: u64 = 200;
const PAGE: usize = 4096;

fn dir() -> PathBuf {
    PathBuf::from("/oak")
}

/// A simulated disk with a durable directory and an empty durable file.
fn disk() -> (SimFs, PathBuf) {
    let fs = SimFs::new();
    fs.create_dir_all(&dir()).unwrap();
    let path = dir().join("data");
    fs.open(&path).unwrap().sync_data().unwrap();
    fs.sync_dir(&dir()).unwrap();
    (fs, path)
}

/// The whole file at `path`, read through a fresh handle.
fn contents(fs: &SimFs, path: &Path) -> Vec<u8> {
    let file = fs.open(path).unwrap();
    let mut buf = vec![0; file.size().unwrap() as usize];
    file.read_exact_at(&mut buf, 0).unwrap();
    buf
}

#[test]
fn synced_writes_survive_every_crash() {
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[7; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        drop(file);

        fs.crash(seed);
        assert_eq!(contents(&fs, &path), [7; PAGE], "seed {seed}");
    }
}

#[test]
fn an_unsynced_write_is_kept_lost_or_torn_by_sector() {
    let mut seen = BTreeSet::new();
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.write_all_at(&[2; PAGE], 0).unwrap();
        drop(file);

        fs.crash(seed);
        let after = contents(&fs, &path);
        assert_eq!(after.len(), PAGE, "seed {seed}");
        for sector in after.chunks(SECTOR as usize) {
            let first = sector[0];
            assert!(
                (first == 1 || first == 2) && sector.iter().all(|&b| b == first),
                "seed {seed}: a sector is neither old nor new"
            );
        }
        seen.insert(match (after.contains(&1), after.contains(&2)) {
            (true, false) => "lost",
            (false, true) => "kept",
            _ => "torn",
        });
    }
    assert_eq!(seen, BTreeSet::from(["kept", "lost", "torn"]));
}

#[test]
fn unsynced_writes_are_lost_independently_of_their_order() {
    // Two writes to different pages: some seed keeps the later one and
    // loses the earlier one, as a disk that reorders its writes would.
    let mut later_only = false;
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[0; 2 * PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.write_all_at(&[2; PAGE], PAGE as u64).unwrap();
        drop(file);

        fs.crash(seed);
        let after = contents(&fs, &path);
        later_only |= after[..PAGE] == [0; PAGE] && after[PAGE..] == [2; PAGE];
    }
    assert!(later_only);
}

#[test]
fn an_unsynced_length_change_can_be_lost() {
    let mut lengths = BTreeSet::new();
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.set_len(PAGE as u64).unwrap();
        drop(file);

        fs.crash(seed);
        lengths.insert(contents(&fs, &path).len());
    }
    assert_eq!(lengths, BTreeSet::from([0, PAGE]));
}

#[test]
fn a_file_created_before_its_directory_is_synced_may_vanish() {
    let mut outcomes = BTreeSet::new();
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        fs.create_dir_all(&dir()).unwrap();
        let path = dir().join("new");
        let file = fs.open(&path).unwrap();
        file.write_all_at(b"oak", 0).unwrap();
        // The contents are durable, but the directory entry is not.
        file.sync_data().unwrap();
        drop(file);

        fs.crash(seed);
        let exists = fs.exists(&path);
        if exists {
            assert_eq!(contents(&fs, &path), b"oak", "seed {seed}");
        }
        outcomes.insert(exists);
    }
    assert_eq!(outcomes, BTreeSet::from([false, true]));
}

#[test]
fn a_file_whose_directory_was_synced_never_vanishes() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        fs.create_dir_all(&dir()).unwrap();
        let path = dir().join("new");
        fs.open(&path).unwrap();
        fs.sync_dir(&dir()).unwrap();

        fs.crash(seed);
        assert!(fs.exists(&path), "seed {seed}");
    }
}

#[test]
fn directories_from_create_dir_all_never_vanish() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let deep = dir().join("a").join("b");
        fs.create_dir_all(&deep).unwrap();

        fs.crash(seed);
        assert!(fs.exists(&deep), "seed {seed}");
    }
}

/// Runs the same writes on a fresh disk and crashes it with `seed`.
fn crashed_disk(seed: u64) -> (Vec<u8>, bool) {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    for i in 0..20u8 {
        file.write_all_at(&[i; PAGE], u64::from(i % 5) * PAGE as u64)
            .unwrap();
        if i == 7 {
            file.sync_data().unwrap();
        }
    }
    file.set_len(3 * PAGE as u64).unwrap();
    fs.open(&dir().join("other")).unwrap();
    drop(file);

    fs.crash(seed);
    (contents(&fs, &path), fs.exists(&dir().join("other")))
}

#[test]
fn the_same_seed_always_gives_the_same_disk() {
    let mut different = BTreeSet::new();
    for seed in 0..SEEDS {
        let first = crashed_disk(seed);
        assert_eq!(first, crashed_disk(seed), "seed {seed}");
        different.insert(first);
    }
    // And the seed matters: many seeds, many different disks.
    assert!(different.len() > SEEDS as usize / 2);
}

#[test]
fn handles_from_before_a_crash_stop_working() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    file.try_lock().unwrap();

    fs.crash(1);
    let mut buf = [0u8; 1];
    assert!(file.write_all_at(b"x", 0).is_err());
    assert!(file.read_exact_at(&mut buf, 0).is_err());
    assert!(file.sync_data().is_err());
    // The process that held the lock is gone, so the lock is too.
    fs.open(&path).unwrap().try_lock().unwrap();
}

#[test]
fn fail_after_cuts_the_power_part_way() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    fs.fail_after(2);
    file.write_all_at(&[1; PAGE], 0).unwrap();
    file.sync_data().unwrap();
    // The third operation that changes the disk finds the power gone, and
    // so does everything after it, until the machine starts again.
    assert!(file.write_all_at(&[2; PAGE], 0).is_err());
    assert!(file.sync_data().is_err());
    assert!(fs.open(&path).is_err());

    fs.crash(3);
    assert_eq!(contents(&fs, &path), [1; PAGE]);
}

#[test]
fn reads_do_not_count_towards_fail_after() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    file.write_all_at(&[1; PAGE], 0).unwrap();
    fs.fail_after(1);
    let mut buf = [0u8; PAGE];
    for _ in 0..10 {
        file.read_exact_at(&mut buf, 0).unwrap();
        file.size().unwrap();
    }
    file.sync_data().unwrap();
    assert!(file.write_all_at(&[2; PAGE], 0).is_err());
}

// A disk that fails one call with the power still on (issue #24).

#[test]
fn a_failed_sync_makes_nothing_durable_and_the_disk_works_after() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    file.write_all_at(&[1; PAGE], 0).unwrap();
    file.sync_data().unwrap();
    file.write_all_at(&[2; PAGE], 0).unwrap();
    fs.fail_once_after(0);

    assert!(file.sync_data().is_err());

    // The power is on: reads see the write, and the next call works.
    assert_eq!(contents(&fs, &path), [2; PAGE]);
    file.write_all_at(&[3; 10], PAGE as u64).unwrap();
    // The write the sync failed on is still unsynced, so a crash can lose
    // it: some seed brings back the page as it was synced.
    let lost = (0..SEEDS).any(|seed| {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.write_all_at(&[2; PAGE], 0).unwrap();
        fs.fail_once_after(0);
        assert!(file.sync_data().is_err());
        fs.crash(seed);
        contents(&fs, &path) == [1; PAGE]
    });
    assert!(lost, "a write whose sync failed always survived the crash");
}

#[test]
fn a_sync_after_a_failed_one_makes_the_writes_durable() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    file.write_all_at(&[2; PAGE], 0).unwrap();
    fs.fail_once_after(0);
    assert!(file.sync_data().is_err());

    file.sync_data().unwrap();

    for seed in 0..SEEDS {
        fs.crash(seed);
        assert_eq!(contents(&fs, &path), [2; PAGE], "seed {seed}");
    }
}

/// A disk whose page 0 was synced as 1s and then written as 2s, with the
/// sync of the 2s failed on a disk that forgets (issue #59).
fn forgotten(fs: &SimFs, path: &Path) {
    fs.set_failed_sync(FailedSync::Forgets);
    let file = fs.open(path).unwrap();
    file.write_all_at(&[1; PAGE], 0).unwrap();
    file.sync_data().unwrap();
    file.write_all_at(&[2; PAGE], 0).unwrap();
    fs.fail_once_after(0);
    assert!(file.sync_data().is_err());
}

#[test]
fn a_failed_sync_that_forgets_leaves_reads_seeing_what_the_disk_lost() {
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        forgotten(&fs, &path);
        // Reads see the forgotten write, as the page cache does.
        assert_eq!(contents(&fs, &path), [2; PAGE]);
        let file = fs.open(&path).unwrap();
        // A later sync does not save it, and a later write of other
        // bytes does not either.
        file.write_all_at(&[3; 10], PAGE as u64).unwrap();
        file.sync_data().unwrap();

        let report = fs.crash(seed);

        assert!(report.writes.is_empty(), "seed {seed}: nothing waits");
        let mut want = vec![1; PAGE];
        want.extend([3; 10]);
        assert_eq!(contents(&fs, &path), want, "seed {seed}");
    }
}

#[test]
fn writing_the_forgotten_bytes_again_saves_them() {
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        forgotten(&fs, &path);
        let file = fs.open(&path).unwrap();
        let mut seen = [0; PAGE];
        file.read_exact_at(&mut seen, 0).unwrap();

        file.write_all_at(&seen, 0).unwrap();
        file.sync_data().unwrap();

        fs.crash(seed);
        assert_eq!(contents(&fs, &path), [2; PAGE], "seed {seed}");
    }
}

#[test]
fn only_a_failed_sync_forgets() {
    let (fs, path) = disk();
    fs.set_failed_sync(FailedSync::Forgets);
    let file = fs.open(&path).unwrap();
    file.write_all_at(&[1; PAGE], 0).unwrap();
    fs.fail_once_after(0);

    // A failed write changes nothing, and forgets nothing either: the
    // write before it is still there for the next sync.
    assert!(file.write_all_at(&[2; PAGE], 0).is_err());
    file.sync_data().unwrap();

    for seed in 0..SEEDS {
        fs.crash(seed);
        assert_eq!(contents(&fs, &path), [1; PAGE], "seed {seed}");
    }
}

#[test]
fn forgetting_holds_across_crashes() {
    let (fs, path) = disk();
    forgotten(&fs, &path);
    fs.crash(0);
    // Set once, before the crash.
    let file = fs.open(&path).unwrap();
    file.write_all_at(&[2; PAGE], 0).unwrap();
    fs.fail_once_after(0);
    assert!(file.sync_data().is_err());
    file.sync_data().unwrap();

    fs.crash(1);

    assert_eq!(contents(&fs, &path), [1; PAGE]);
}

#[test]
fn a_failed_write_or_length_change_changes_nothing() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    file.write_all_at(&[1; PAGE], 0).unwrap();
    file.sync_data().unwrap();

    fs.fail_once_after(0);
    assert!(file.write_all_at(&[2; PAGE], 0).is_err());
    fs.fail_once_after(0);
    assert!(file.set_len(1).is_err());

    assert_eq!(contents(&fs, &path), [1; PAGE]);
    let report = fs.crash(0);
    assert!(report.writes.is_empty() && report.lengths.is_empty());
}

#[test]
fn fail_once_after_fails_the_call_after_that_many_and_only_it() {
    let (fs, path) = disk();
    let ops = fs.ops();
    let file = fs.open(&path).unwrap();
    fs.fail_once_after(2);
    file.write_all_at(b"oak", 0).unwrap();
    // Reads do not count.
    file.read_exact_at(&mut [0; 3], 0).unwrap();
    file.sync_data().unwrap();

    let Err(err) = fs.open(&path) else {
        panic!("the third call worked");
    };
    // The error a disk gives for a failed read or write: EIO, 5 on Linux
    // and macOS alike.
    assert_eq!(err.raw_os_error(), Some(5), "the third call fails with EIO");

    // It is not counted, and the calls after it work.
    assert_eq!(fs.ops(), ops + 3);
    fs.sync_dir(&dir()).unwrap();
    file.write_all_at(b"tree", 0).unwrap();
    assert_eq!(fs.ops(), ops + 5);
}

#[test]
fn a_power_cut_comes_first_if_both_are_due() {
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    fs.fail_after(1);
    fs.fail_once_after(1);
    file.write_all_at(b"oak", 0).unwrap();
    assert!(file.sync_data().is_err());
    // The power is cut: every call fails, reads too, not just one call.
    assert!(file.size().is_err());
    assert!(file.sync_data().is_err());
    assert!(fs.open(&path).is_err());
}

#[test]
fn a_failure_a_power_cut_got_ahead_of_does_not_carry_over_to_the_next_boot() {
    // Review P4 of PR #57: the cut wins, and the failure is dropped with
    // the rest of what was due, not saved for the next boot.
    let (fs, path) = disk();
    let file = fs.open(&path).unwrap();
    fs.fail_after(0);
    fs.fail_once_after(0);
    assert!(file.write_all_at(b"oak", 0).is_err());

    fs.crash(0);

    let file = fs.open(&path).unwrap();
    file.write_all_at(b"tree", 0).unwrap();
    file.sync_data().unwrap();
}

#[test]
fn ops_counts_what_fail_after_counts() {
    let fs = SimFs::new();
    assert_eq!(fs.ops(), 0);
    fs.create_dir_all(&dir()).unwrap();
    let file = fs.open(&dir().join("data")).unwrap();
    file.write_all_at(b"oak", 0).unwrap();
    file.set_len(10).unwrap();
    file.sync_data().unwrap();
    fs.sync_dir(&dir()).unwrap();
    // Reads, sizes and locks do not count.
    file.read_exact_at(&mut [0; 3], 0).unwrap();
    file.size().unwrap();
    file.try_lock().unwrap();
    assert_eq!(fs.ops(), 6);

    // Cutting the power after the operations so far: the next one fails
    // and does not count.
    fs.fail_after(0);
    assert!(file.write_all_at(b"x", 0).is_err());
    assert_eq!(fs.ops(), 6);
    // The count goes on across a crash.
    fs.crash(0);
    fs.open(&dir().join("data")).unwrap();
    assert_eq!(fs.ops(), 7);
}

#[test]
fn a_crash_reports_what_became_of_each_unsynced_change() {
    let mut seen = BTreeSet::new();
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.write_all_at(&[2; PAGE], 0).unwrap();
        file.set_len(2 * PAGE as u64).unwrap();
        let new = dir().join("new");
        fs.open(&new).unwrap();
        drop(file);

        let report = fs.crash(seed);
        assert_eq!(report.writes.len(), 1, "seed {seed}");
        let WriteFate {
            path: written,
            offset,
            len,
            landing,
        } = &report.writes[0];
        assert_eq!((written, *offset, *len), (&path, 0, PAGE));
        // The report agrees with the disk, sector by sector.
        let after = contents(&fs, &path);
        let landed: Vec<bool> = after[..PAGE]
            .chunks(SECTOR as usize)
            .map(|sector| sector[0] == 2)
            .collect();
        match landing {
            Landing::Lost => assert!(landed.iter().all(|&l| !l), "seed {seed}"),
            Landing::Kept => assert!(landed.iter().all(|&l| l), "seed {seed}"),
            Landing::Torn { sectors } => assert_eq!(sectors, &landed, "seed {seed}"),
            Landing::LengthOnly { .. } => panic!("seed {seed}: the write was inside the file"),
            other => panic!("seed {seed}: {other:?}"),
        }
        let [LengthFate {
            path: resized,
            len,
            landed,
        }] = report.lengths.as_slice()
        else {
            panic!("seed {seed}: one length change expected");
        };
        assert_eq!((resized, *len), (&path, 2 * PAGE as u64));
        assert_eq!(*landed, after.len() == 2 * PAGE, "seed {seed}");
        assert_eq!(
            report.vanished.contains(&new),
            !fs.exists(&new),
            "seed {seed}"
        );
        seen.insert(kind(landing));
    }
    assert_eq!(seen.len(), 3);
}

/// The name of a landing, for counting which ones a test saw.
fn kind(landing: &Landing) -> &'static str {
    match landing {
        Landing::Lost => "lost",
        Landing::Kept => "kept",
        Landing::Torn { .. } => "torn",
        Landing::LengthOnly { .. } => "length only",
        _ => "unknown",
    }
}

#[test]
fn a_write_past_the_end_can_land_as_its_length_alone_with_zeros() {
    // A file system can write the new length before the bytes (ext4 in
    // writeback mode, XFS): the file grows, and what grew reads as zeros.
    let mut seen = BTreeSet::new();
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[2; PAGE], 0).unwrap();
        drop(file);

        let report = fs.crash(seed);
        let after = contents(&fs, &path);
        let [WriteFate { landing, .. }] = report.writes.as_slice() else {
            panic!("seed {seed}: one write expected");
        };
        // The report agrees with the disk.
        match landing {
            Landing::Lost => assert!(after.is_empty(), "seed {seed}"),
            Landing::Kept => assert_eq!(after, [2; PAGE], "seed {seed}"),
            Landing::Torn { sectors } => {
                let last = sectors.iter().rposition(|&s| s).map_or(0, |i| i + 1);
                assert_eq!(after.len(), last * SECTOR as usize, "seed {seed}");
            }
            Landing::LengthOnly { zeros_from } => {
                assert_eq!(*zeros_from, 0, "seed {seed}");
                assert_eq!(after, [0; PAGE], "seed {seed}");
            }
            other => panic!("seed {seed}: {other:?}"),
        }
        seen.insert(kind(landing));
    }
    assert_eq!(
        seen,
        BTreeSet::from(["kept", "length only", "lost", "torn"])
    );
}

#[test]
fn only_the_part_past_the_old_end_reads_as_zeros() {
    // A page of ones, then two pages of twos over it: the first page was
    // inside the file when written, so landing the length alone leaves it
    // as the disk had it.
    let mut length_only = 0;
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.write_all_at(&[2; 2 * PAGE], 0).unwrap();
        drop(file);

        let report = fs.crash(seed);
        if let Landing::LengthOnly { zeros_from } = report.writes[0].landing {
            length_only += 1;
            assert_eq!(zeros_from, PAGE as u64, "seed {seed}");
            let after = contents(&fs, &path);
            assert_eq!(after[..PAGE], [1; PAGE], "seed {seed}");
            assert_eq!(after[PAGE..], [0; PAGE], "seed {seed}");
        }
    }
    assert!(length_only > 0);
}

#[test]
fn a_write_beyond_the_end_reads_as_zeros_from_its_offset() {
    let mut length_only = 0;
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.write_all_at(&[2; PAGE], 2 * PAGE as u64).unwrap();
        drop(file);

        let report = fs.crash(seed);
        if let Landing::LengthOnly { zeros_from } = report.writes[0].landing {
            length_only += 1;
            assert_eq!(zeros_from, 2 * PAGE as u64, "seed {seed}");
            let after = contents(&fs, &path);
            assert_eq!(after[..PAGE], [1; PAGE], "seed {seed}");
            assert_eq!(after[PAGE..], [0; 2 * PAGE], "seed {seed}");
        }
    }
    assert!(length_only > 0);
}

#[test]
fn zeros_never_cover_bytes_the_disk_still_holds() {
    // A page of ones, emptied without a sync, then a short write at 0. A
    // file system journals the emptying before the growth, so the write's
    // length alone lands only on an emptied file: then it is 20 zeros. If
    // the emptying is lost, the write is inside the file on the disk, and
    // the old bytes are never zeroed.
    let mut zeros = 0;
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.write_all_at(&[1; PAGE], 0).unwrap();
        file.sync_data().unwrap();
        file.set_len(0).unwrap();
        file.write_all_at(&[2; 20], 0).unwrap();
        drop(file);

        let report = fs.crash(seed);
        let after = contents(&fs, &path);
        let length_only = matches!(report.writes[0].landing, Landing::LengthOnly { .. });
        if report.lengths[0].landed {
            if length_only {
                zeros += 1;
                assert_eq!(after, [0; 20], "seed {seed}");
            }
        } else {
            assert!(!length_only, "seed {seed}: zeros over the old page");
            assert_eq!(after.len(), PAGE, "seed {seed}");
            assert_eq!(after[20..], [1; PAGE - 20], "seed {seed}");
            assert!(!after[..20].contains(&0), "seed {seed}");
        }
    }
    assert!(zeros > 0);
}

#[test]
fn a_write_into_grown_space_is_inside_the_file_once_the_growth_landed() {
    // Growing a file with set_len leaves zeros, and lands or not. A write
    // into the grown part lands its length alone only if the growth was
    // lost, when it is past the end on the disk after all.
    let mut length_only_without_growth = 0;
    for seed in 0..SEEDS {
        let (fs, path) = disk();
        let file = fs.open(&path).unwrap();
        file.set_len(2 * PAGE as u64).unwrap();
        file.write_all_at(&[2; PAGE], 0).unwrap();
        drop(file);

        let report = fs.crash(seed);
        let length_only = matches!(report.writes[0].landing, Landing::LengthOnly { .. });
        if report.lengths[0].landed {
            assert!(!length_only, "seed {seed}");
        } else if length_only {
            length_only_without_growth += 1;
            assert_eq!(contents(&fs, &path), [0; PAGE], "seed {seed}");
        }
    }
    assert!(length_only_without_growth > 0);
}

// The engine on the simulated disk.

/// Commits key `i` for `i` in `0..commits`, with values long enough that
/// some commits split a leaf and change the meta page, and others do not.
fn commit_keys(db: &Db<SimFs>, commits: u32) {
    for i in 0..commits {
        let mut txn = db.begin();
        txn.put(&i.to_be_bytes(), &vec![i as u8; 300]).unwrap();
        txn.commit().unwrap();
    }
}

#[test]
fn the_engine_keeps_every_commit_through_a_crash() {
    // A crash right after each commit in turn, so the last commit before
    // it is sometimes one that changed the meta page and sometimes not.
    for commits in 1..=40 {
        for seed in 0..20 {
            let fs = SimFs::new();
            let db = Db::open_with(fs.clone(), dir()).unwrap();
            commit_keys(&db, commits);
            // Work that was never committed, cut off by the crash.
            let mut txn = db.begin();
            txn.put(b"uncommitted", b"x").unwrap();
            drop(txn);
            drop(db);

            fs.crash(seed);
            let db = Db::open_with(fs.clone(), dir()).unwrap();
            let txn = db.begin();
            for i in 0..commits {
                assert_eq!(
                    txn.get(&i.to_be_bytes()).unwrap(),
                    Some(vec![i as u8; 300]),
                    "{commits} commits, seed {seed}, key {i}"
                );
            }
            assert_eq!(txn.get(b"uncommitted").unwrap(), None);
        }
    }
}

#[test]
fn a_new_database_survives_a_crash_right_after_opening() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        drop(Db::open_with(fs.clone(), dir()).unwrap());

        fs.crash(seed);
        assert!(fs.exists(&dir().join(DATA_FILE)), "seed {seed}");
        Db::open_with(fs.clone(), dir()).unwrap();
    }
}
