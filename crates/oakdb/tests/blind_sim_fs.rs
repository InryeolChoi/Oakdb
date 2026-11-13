//! Blind tests for the simulated file system (ticket 08, issue #20).
//!
//! Written only from issue #20, spec #12, `docs/design/0008-sim-fs.md`, the
//! ADRs, the `Fs`/`FsFile` trait docs and the public doc comments of
//! `SimFs`. The implementation and the existing tests were not read.

use std::io::ErrorKind;
use std::path::Path;

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::Db;

const SECTOR: usize = 512;
const SEEDS: u64 = 200;

/// The contents of `path` after a crash, or `None` if the file vanished.
fn contents(fs: &SimFs, path: &str) -> Option<Vec<u8>> {
    if !fs.exists(Path::new(path)) {
        return None;
    }
    let f = fs.open(Path::new(path)).unwrap();
    let len = f.size().unwrap() as usize;
    let mut buf = vec![0; len];
    f.read_exact_at(&mut buf, 0).unwrap();
    Some(buf)
}

/// A file `/d/f` whose entry is durable and whose contents are `initial`,
/// synced.
fn durable_file(initial: &[u8]) -> (SimFs, <SimFs as Fs>::File) {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/d")).unwrap();
    let f = fs.open(Path::new("/d/f")).unwrap();
    if !initial.is_empty() {
        f.write_all_at(initial, 0).unwrap();
    }
    f.sync_data().unwrap();
    fs.sync_dir(Path::new("/d")).unwrap();
    (fs, f)
}

// ---------------------------------------------------------------- before a crash

#[test]
fn reads_see_unsynced_writes_and_eof_is_unexpected_eof() {
    let (_fs, f) = durable_file(b"");
    f.write_all_at(b"hello", 0).unwrap();
    let mut buf = [0; 5];
    f.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"hello");
    let mut past = [0; 1];
    let err = f.read_exact_at(&mut past, 5).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
}

#[test]
fn writing_past_the_end_leaves_a_zero_hole() {
    let (_fs, f) = durable_file(b"ab");
    f.write_all_at(b"z", 10).unwrap();
    assert_eq!(f.size().unwrap(), 11);
    let mut buf = [0xff; 11];
    f.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"ab\0\0\0\0\0\0\0\0z");
}

#[test]
fn clones_share_one_disk() {
    let fs = SimFs::new();
    let other = fs.clone();
    fs.create_dir_all(Path::new("/d")).unwrap();
    let f = fs.open(Path::new("/d/f")).unwrap();
    f.write_all_at(b"x", 0).unwrap();
    assert!(other.exists(Path::new("/d/f")));
    let g = other.open(Path::new("/d/f")).unwrap();
    let mut buf = [0; 1];
    g.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"x");
}

#[test]
fn open_in_missing_directory_is_not_found() {
    let fs = SimFs::new();
    let err = fs.open(Path::new("/nope/f")).err().expect("must fail");
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn lock_is_exclusive_and_released_on_drop() {
    let (fs, f) = durable_file(b"");
    f.try_lock().unwrap();
    let g = fs.open(Path::new("/d/f")).unwrap();
    assert_eq!(g.try_lock().unwrap_err().kind(), ErrorKind::WouldBlock);
    drop(f);
    g.try_lock().unwrap();
}

// ---------------------------------------------------------------- crash: contents

#[test]
fn synced_data_survives_every_seed() {
    let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(&data);
        drop(f);
        fs.crash(seed);
        assert_eq!(
            contents(&fs, "/d/f").as_deref(),
            Some(&data[..]),
            "seed {seed}"
        );
    }
}

#[test]
fn crash_with_nothing_pending_changes_nothing() {
    let data = vec![7u8; 1500];
    for seed in 0..50 {
        let (fs, f) = durable_file(&data);
        drop(f);
        fs.crash(seed);
        fs.crash(seed.wrapping_mul(31) + 1);
        assert_eq!(contents(&fs, "/d/f"), Some(data.clone()), "seed {seed}");
    }
}

#[test]
fn unsynced_overwrite_is_kept_lost_or_torn_by_sector() {
    let (mut all_new, mut all_old, mut mixed) = (false, false, false);
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(&[0x11; 8 * SECTOR]);
        f.write_all_at(&[0x22; 8 * SECTOR], 0).unwrap();
        drop(f);
        fs.crash(seed);
        let got = contents(&fs, "/d/f").expect("entry was synced");
        assert_eq!(got.len(), 8 * SECTOR, "seed {seed}");
        let mut new_sectors = 0;
        for s in got.chunks(SECTOR) {
            assert!(
                s.iter().all(|&b| b == 0x11) || s.iter().all(|&b| b == 0x22),
                "seed {seed}: a sector is half old, half new"
            );
            if s[0] == 0x22 {
                new_sectors += 1;
            }
        }
        match new_sectors {
            8 => all_new = true,
            0 => all_old = true,
            _ => mixed = true,
        }
    }
    assert!(
        all_new && all_old && mixed,
        "new {all_new} old {all_old} torn {mixed}"
    );
}

#[test]
fn unaligned_write_tears_on_absolute_sector_boundaries() {
    // The write covers 100..1100: parts of sectors 0, 1 and 2.
    let (start, len) = (100usize, 1000usize);
    let mut saw_mixed = false;
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(&[0x11; 4 * SECTOR]);
        f.write_all_at(&vec![0x22; len], start as u64).unwrap();
        drop(f);
        fs.crash(seed);
        let got = contents(&fs, "/d/f").unwrap();
        assert_eq!(got.len(), 4 * SECTOR);
        assert!(
            got[..start].iter().all(|&b| b == 0x11),
            "seed {seed}: before write touched"
        );
        assert!(
            got[start + len..].iter().all(|&b| b == 0x11),
            "seed {seed}: after write touched"
        );
        let mut kinds = Vec::new();
        for sector in 0..3 {
            let lo = (sector * SECTOR).max(start);
            let hi = ((sector + 1) * SECTOR).min(start + len);
            let part = &got[lo..hi];
            assert!(
                part.iter().all(|&b| b == part[0]),
                "seed {seed}: sector {sector} is split inside"
            );
            kinds.push(part[0]);
        }
        if kinds.contains(&0x11) && kinds.contains(&0x22) {
            saw_mixed = true;
        }
    }
    assert!(saw_mixed, "an unaligned write never tore");
}

#[test]
fn unsynced_write_past_end_lands_sector_by_sector() {
    let (mut full, mut none, mut part) = (false, false, false);
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(b"");
        f.write_all_at(&[0xab; 4 * SECTOR], 0).unwrap();
        drop(f);
        fs.crash(seed);
        let got = contents(&fs, "/d/f").unwrap();
        assert!(got.len() <= 4 * SECTOR);
        let mut new = 0;
        for s in got.chunks(SECTOR) {
            assert!(
                s.iter().all(|&b| b == 0xab) || s.iter().all(|&b| b == 0),
                "seed {seed}: sector neither new nor empty"
            );
            if s[0] == 0xab {
                new += 1;
            }
        }
        match new {
            4 => full = true,
            0 => none = true,
            _ => part = true,
        }
    }
    assert!(full && none && part, "full {full} none {none} part {part}");
}

#[test]
fn later_write_can_land_while_earlier_is_lost() {
    let mut reordered = false;
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(&[0; 2 * SECTOR]);
        f.write_all_at(&[1; SECTOR], 0).unwrap();
        f.write_all_at(&[2; SECTOR], SECTOR as u64).unwrap();
        drop(f);
        fs.crash(seed);
        let got = contents(&fs, "/d/f").unwrap();
        if got[0] == 0 && got[SECTOR] == 2 {
            reordered = true;
        }
    }
    assert!(reordered, "the disk never reordered two writes");
}

#[test]
fn shrink_then_crash_keeps_old_or_new_length() {
    let (mut kept, mut lost) = (false, false);
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(&[0x11; 4 * SECTOR]);
        f.set_len(SECTOR as u64).unwrap();
        assert_eq!(f.size().unwrap(), SECTOR as u64);
        drop(f);
        fs.crash(seed);
        let got = contents(&fs, "/d/f").unwrap();
        assert!(got.iter().all(|&b| b == 0x11), "seed {seed}");
        match got.len() {
            l if l == SECTOR => kept = true,
            l if l == 4 * SECTOR => lost = true,
            l => panic!("seed {seed}: length {l} is neither old nor new"),
        }
    }
    assert!(kept && lost, "kept {kept} lost {lost}");
}

#[test]
fn sync_through_another_handle_makes_writes_durable() {
    for seed in 0..50 {
        let (fs, f) = durable_file(b"");
        let g = fs.open(Path::new("/d/f")).unwrap();
        f.write_all_at(&[9; 1000], 0).unwrap();
        g.sync_data().unwrap();
        drop((f, g));
        fs.crash(seed);
        assert_eq!(contents(&fs, "/d/f"), Some(vec![9; 1000]), "seed {seed}");
    }
}

// ---------------------------------------------------------------- crash: entries

#[test]
fn file_before_dir_sync_may_vanish_after_never() {
    let (mut vanished, mut stayed) = (false, false);
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/a/b")).unwrap();
        let f = fs.open(Path::new("/a/b/new")).unwrap();
        f.write_all_at(b"data", 0).unwrap();
        f.sync_data().unwrap(); // contents synced, entry not
        let g = fs.open(Path::new("/a/b/kept")).unwrap();
        g.write_all_at(b"data", 0).unwrap();
        g.sync_data().unwrap();
        drop((f, g));
        fs.sync_dir(Path::new("/a/b")).unwrap();
        // A third file, created after the directory sync.
        let h = fs.open(Path::new("/a/b/late")).unwrap();
        h.sync_data().unwrap();
        drop(h);
        fs.crash(seed);
        assert!(
            fs.exists(Path::new("/a/b")),
            "directory vanished, seed {seed}"
        );
        assert_eq!(contents(&fs, "/a/b/new").as_deref(), Some(&b"data"[..]));
        assert_eq!(contents(&fs, "/a/b/kept").as_deref(), Some(&b"data"[..]));
        if fs.exists(Path::new("/a/b/late")) {
            stayed = true;
        } else {
            vanished = true;
        }
    }
    assert!(vanished && stayed, "vanished {vanished} stayed {stayed}");
}

#[test]
fn directories_never_vanish() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/x/y/z")).unwrap();
        fs.crash(seed);
        for p in ["/x", "/x/y", "/x/y/z"] {
            assert!(fs.exists(Path::new(p)), "{p} vanished, seed {seed}");
        }
    }
}

#[test]
fn relative_paths_behave_like_absolute_ones() {
    for seed in 0..50 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("rel/dir")).unwrap();
        let f = fs.open(Path::new("rel/dir/f")).unwrap();
        f.write_all_at(b"rel", 0).unwrap();
        f.sync_data().unwrap();
        drop(f);
        fs.sync_dir(Path::new("rel/dir")).unwrap();
        fs.crash(seed);
        assert_eq!(
            contents(&fs, "rel/dir/f").as_deref(),
            Some(&b"rel"[..]),
            "seed {seed}"
        );
    }
}

// ---------------------------------------------------------------- determinism

/// Many files with unsynced work, some entries not synced. Returns the disk
/// after `crash(seed)`.
fn busy_disk(seed: u64) -> Vec<Option<Vec<u8>>> {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).unwrap();
    let names = ["/m/a", "/m/b", "/m/c", "/m/d"];
    for (i, n) in names.iter().enumerate() {
        let f = fs.open(Path::new(n)).unwrap();
        f.write_all_at(&vec![i as u8 + 1; 3 * SECTOR], 0).unwrap();
        f.sync_data().unwrap();
        if i == 1 {
            fs.sync_dir(Path::new("/m")).unwrap();
        }
        f.write_all_at(&vec![0xf0 | i as u8; 2 * SECTOR + 7], 300)
            .unwrap();
        f.set_len(4 * SECTOR as u64 - 3).unwrap();
        f.write_all_at(&[0xee; 100], 50).unwrap();
    }
    fs.crash(seed);
    names.iter().map(|n| contents(&fs, n)).collect()
}

#[test]
fn same_seed_same_disk_and_seeds_differ() {
    let mut distinct = std::collections::HashSet::new();
    for seed in 0..100 {
        let a = busy_disk(seed);
        assert_eq!(a, busy_disk(seed), "seed {seed} gave two disks");
        distinct.insert(a);
    }
    assert!(
        distinct.len() > 50,
        "only {} distinct disks from 100 seeds",
        distinct.len()
    );
}

// ---------------------------------------------------------------- handles and locks

#[test]
fn handles_from_before_a_crash_fail_and_locks_are_released() {
    let (fs, f) = durable_file(b"abc");
    f.try_lock().unwrap();
    fs.crash(1);
    let mut buf = [0; 1];
    assert!(f.read_exact_at(&mut buf, 0).is_err());
    assert!(f.write_all_at(b"x", 0).is_err());
    assert!(f.size().is_err());
    assert!(f.set_len(0).is_err());
    assert!(f.sync_data().is_err());
    let g = fs.open(Path::new("/d/f")).unwrap();
    g.try_lock().unwrap();
    // Dropping the dead handle must not release the new boot's lock.
    drop(f);
    let h = fs.open(Path::new("/d/f")).unwrap();
    assert_eq!(h.try_lock().unwrap_err().kind(), ErrorKind::WouldBlock);
    assert_eq!(contents(&fs, "/d/f").as_deref(), Some(&b"abc"[..]));
}

// ---------------------------------------------------------------- fail_after

#[test]
fn fail_after_counts_only_disk_changes() {
    let fs = SimFs::new();
    fs.fail_after(5);
    fs.create_dir_all(Path::new("/d")).unwrap(); // 1
    let f = fs.open(Path::new("/d/f")).unwrap(); // 2
    f.write_all_at(b"abcd", 0).unwrap(); // 3
                                         // Not counted.
    let mut buf = [0; 4];
    for _ in 0..20 {
        f.read_exact_at(&mut buf, 0).unwrap();
        f.size().unwrap();
    }
    f.try_lock().unwrap();
    f.set_len(4).unwrap(); // 4
    f.sync_data().unwrap(); // 5
    assert!(
        fs.sync_dir(Path::new("/d")).is_err(),
        "6th change must fail"
    );
    // Everything fails now, reads included.
    assert!(f.read_exact_at(&mut buf, 0).is_err());
    assert!(f.size().is_err());
    assert!(fs.open(Path::new("/d/f")).is_err());
    assert!(fs.create_dir_all(Path::new("/e")).is_err());
}

#[test]
fn fail_after_zero_fails_the_next_change_and_crash_restores_power() {
    let (fs, f) = durable_file(b"keep");
    fs.fail_after(0);
    assert!(f.write_all_at(b"lost", 0).is_err());
    drop(f);
    fs.crash(3);
    // Power is back and stays on: no limit any more.
    let g = fs.open(Path::new("/d/f")).unwrap();
    for i in 0..50 {
        g.write_all_at(b"z", i).unwrap();
    }
    g.sync_data().unwrap();
    let got = contents(&fs, "/d/f").unwrap();
    assert_eq!(got.len(), 50);
}

#[test]
fn write_refused_by_fail_after_never_reaches_disk() {
    for seed in 0..SEEDS {
        let (fs, f) = durable_file(&[0x11; 2 * SECTOR]);
        fs.fail_after(0);
        let _ = f.write_all_at(&[0x22; 2 * SECTOR], 0);
        drop(f);
        fs.crash(seed);
        assert_eq!(
            contents(&fs, "/d/f"),
            Some(vec![0x11; 2 * SECTOR]),
            "seed {seed}"
        );
    }
}

#[test]
#[ignore = "보류: 문서가 fail_after(0) 직후 읽기(세지 않는 호출)가 실패해야 하는지 정하지 않는다"]
fn fail_after_zero_cuts_reads_immediately() {
    let (fs, f) = durable_file(b"x");
    fs.fail_after(0);
    let mut buf = [0; 1];
    assert!(f.read_exact_at(&mut buf, 0).is_err());
}

// ---------------------------------------------------------------- the engine

#[test]
fn engine_keeps_every_commit_across_crashes() {
    for seed in 0..20 {
        let fs = SimFs::new();
        for round in 0..10u32 {
            let db = Db::open_with(fs.clone(), "/oak").unwrap();
            {
                let t = db.begin();
                for k in 0..round {
                    let key = format!("k{k:03}");
                    assert_eq!(
                        t.get(key.as_bytes()).unwrap().as_deref(),
                        Some(format!("v{k}").as_bytes()),
                        "seed {seed} round {round} lost {key}"
                    );
                }
            }
            let mut t = db.begin();
            t.put(
                format!("k{round:03}").as_bytes(),
                format!("v{round}").as_bytes(),
            )
            .unwrap();
            t.commit().unwrap();
            let mut dropped = db.begin();
            dropped.put(b"uncommitted", b"no").unwrap();
            drop(dropped);
            // The Db is left open: the crash kills it like a process.
            fs.crash(seed * 1000 + round as u64);
            drop(db);
        }
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        let t = db.begin();
        assert_eq!(t.get(b"uncommitted").unwrap(), None);
        assert_eq!(t.get(b"k009").unwrap().as_deref(), Some(&b"v9"[..]));
    }
}

#[test]
fn new_database_survives_a_crash_right_after_open() {
    for seed in 0..50 {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        fs.crash(seed);
        drop(db);
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        assert_eq!(db.begin().get(b"a").unwrap(), None);
    }
}

#[test]
fn engine_large_values_survive_crashes() {
    let big: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 256) as u8).collect();
    for seed in 0..20 {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        let mut t = db.begin();
        t.put(b"big", &big).unwrap();
        t.commit().unwrap();
        fs.crash(seed);
        drop(db);
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        assert_eq!(
            db.begin().get(b"big").unwrap(),
            Some(big.clone()),
            "seed {seed}"
        );
    }
}

#[test]
fn engine_commit_fails_cleanly_under_fail_after_zero() {
    for seed in 0..20 {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        let mut t = db.begin();
        t.put(b"old", b"1").unwrap();
        t.commit().unwrap();
        fs.fail_after(0);
        let mut t = db.begin();
        t.put(b"new", b"2").unwrap();
        assert!(t.commit().is_err(), "commit with the power gone must fail");
        fs.crash(seed);
        drop(db);
        let db = Db::open_with(fs.clone(), "/oak").unwrap();
        let t = db.begin();
        assert_eq!(t.get(b"old").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(t.get(b"new").unwrap(), None);
    }
}

#[test]
#[ignore = "보류: 커밋 도중 아무 시점 정전 뒤의 복구는 WAL(09~10번) 몫이고 08 문서는 약속하지 않는다"]
fn engine_survives_crash_in_the_middle_of_a_commit() {
    for n in 0..30 {
        for seed in 0..10 {
            let fs = SimFs::new();
            let db = Db::open_with(fs.clone(), "/oak").unwrap();
            let mut t = db.begin();
            t.put(b"old", b"1").unwrap();
            t.commit().unwrap();
            fs.fail_after(n);
            let mut t = db.begin();
            for k in 0..200u32 {
                t.put(format!("k{k:04}").as_bytes(), &[0x5a; 64]).unwrap();
            }
            let _ = t.commit();
            fs.crash(seed);
            drop(db);
            let db = Db::open_with(fs.clone(), "/oak").unwrap();
            assert_eq!(db.begin().get(b"old").unwrap().as_deref(), Some(&b"1"[..]));
        }
    }
}
