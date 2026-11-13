//! Power cuts on the simulated file system (issue #20).
//!
//! On a crash, whatever was synced stays. Each write since the last sync is
//! kept, lost, or torn (some of its 512-byte sectors new, the rest old), and
//! a file created since its directory was last synced may vanish. A seed
//! makes every choice, so the same seed always gives the same disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, DATA_FILE};

const SEEDS: u64 = 200;
const PAGE: usize = 4096;
const SECTOR: usize = 512;

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
        for sector in after.chunks(SECTOR) {
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
