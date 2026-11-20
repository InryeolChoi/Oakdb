//! Blind tests for issue #56 (length-only writes), written from the issue,
//! `docs/design/0013-length-only-writes.md`, `0008-sim-fs.md` and
//! `0011-checkpoint.md` and the public API only, without reading the
//! implementation. Each test cites the sentence it checks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oakdb::fs::sim::{CrashReport, Landing, SimFs, SECTOR};
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error, WAL_FILE};

const DIR: &str = "/d";
const FILE: &str = "/d/f";

// ---------------------------------------------------------------------------
// Helpers

#[derive(Clone, Debug)]
enum Op {
    Write(u64, Vec<u8>),
    SetLen(u64),
}

fn file_path() -> PathBuf {
    PathBuf::from(FILE)
}

/// A disk holding `/d/f` with `base` synced (and its directory entry synced),
/// then `ops` applied without a sync. Returns the disk and the open handle.
fn prepare(base: &[u8], ops: &[Op]) -> SimFs {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).unwrap();
    let f = fs.open(Path::new(FILE)).unwrap();
    fs.sync_dir(Path::new(DIR)).unwrap();
    if !base.is_empty() {
        f.write_all_at(base, 0).unwrap();
    }
    f.sync_data().unwrap();
    for op in ops {
        match op {
            Op::Write(off, data) => f.write_all_at(data, *off).unwrap(),
            Op::SetLen(len) => f.set_len(*len).unwrap(),
        }
    }
    drop(f);
    fs
}

fn read_all(fs: &SimFs, path: &Path) -> Vec<u8> {
    let f = fs.open(path).unwrap();
    let len = f.size().unwrap() as usize;
    let mut buf = vec![0; len];
    f.read_exact_at(&mut buf, 0).unwrap();
    buf
}

fn run(base: &[u8], ops: &[Op], seed: u64) -> (CrashReport, Vec<u8>) {
    let fs = prepare(base, ops);
    let report = fs.crash(seed);
    let disk = read_all(&fs, Path::new(FILE));
    (report, disk)
}

fn put(disk: &mut Vec<u8>, at: u64, bytes: &[u8]) {
    let at = at as usize;
    if disk.len() < at + bytes.len() {
        disk.resize(at + bytes.len(), 0);
    }
    disk[at..at + bytes.len()].copy_from_slice(bytes);
}

/// The disk the report describes, built from the spec alone (design 0008
/// section 3 and design 0013 section 2). Also checks the report's fields
/// against the operations and the rule for `zeros_from`.
fn model(base: &[u8], ops: &[Op], report: &CrashReport) -> Vec<u8> {
    let mut disk = base.to_vec();
    let mut seen_len = base.len() as u64; // the length the engine saw when writing
    let mut writes = report.writes.iter();
    let mut lengths = report.lengths.iter();
    for op in ops {
        match op {
            Op::Write(off, data) => {
                let fate = writes.next().expect("every unsynced write is reported");
                assert_eq!(fate.path, file_path());
                assert_eq!(fate.offset, *off);
                assert_eq!(fate.len, data.len());
                let end = off + data.len() as u64;
                match &fate.landing {
                    Landing::Kept => put(&mut disk, *off, data),
                    Landing::Lost => {}
                    Landing::Torn { sectors } => {
                        let first = off / SECTOR;
                        let last = (end - 1) / SECTOR;
                        assert_eq!(sectors.len() as u64, last - first + 1);
                        for (i, landed) in sectors.iter().enumerate() {
                            if !landed {
                                continue;
                            }
                            let s = first + i as u64;
                            let from = (s * SECTOR).max(*off);
                            let to = ((s + 1) * SECTOR).min(end);
                            let piece = &data[(from - off) as usize..(to - off) as usize];
                            put(&mut disk, from, piece);
                        }
                    }
                    Landing::LengthOnly { zeros_from } => {
                        assert!(
                            end > seen_len,
                            "only a write past the end as written lands length-only"
                        );
                        assert_eq!(
                            *zeros_from,
                            (*off).max(seen_len),
                            "zeros_from = max(offset, end when written)"
                        );
                        let zeros = vec![0u8; (end - zeros_from) as usize];
                        put(&mut disk, *zeros_from, &zeros);
                    }
                }
                seen_len = seen_len.max(end);
            }
            Op::SetLen(len) => {
                let fate = lengths
                    .next()
                    .expect("every unsynced length change is reported");
                assert_eq!(fate.path, file_path());
                assert_eq!(fate.len, *len);
                if fate.landed {
                    disk.resize(*len as usize, 0);
                }
                seen_len = *len;
            }
        }
    }
    assert!(writes.next().is_none());
    assert!(lengths.next().is_none());
    disk
}

fn landing_of(report: &CrashReport, i: usize) -> &Landing {
    &report.writes[i].landing
}

fn is_length_only(l: &Landing) -> bool {
    matches!(l, Landing::LengthOnly { .. })
}

// ---------------------------------------------------------------------------
// SimFs: the fate itself

/// Issue AC 1: "파일 끝을 넘는 동기화 안 된 쓰기가 정전 때 '길이만 닿음(늘어난
/// 자리는 0)'으로 끝날 수 있다. `CrashReport`에 그 운명이 보인다".
#[test]
fn an_append_can_land_as_length_only_with_zeros() {
    let base = vec![0xAA; 1000];
    let ops = [Op::Write(1000, vec![0xBB; 100])];
    let mut found = 0;
    for seed in 0..400 {
        let (report, disk) = run(&base, &ops, seed);
        if let Landing::LengthOnly { zeros_from } = landing_of(&report, 0) {
            found += 1;
            assert_eq!(*zeros_from, 1000);
            assert_eq!(disk.len(), 1100, "the file is as long as the write's end");
            assert!(
                disk[..1000].iter().all(|&b| b == 0xAA),
                "synced part unchanged"
            );
            assert!(
                disk[1000..].iter().all(|&b| b == 0),
                "the new part reads as zeros"
            );
        }
    }
    assert!(found > 0, "no seed in 0..400 gave a length-only write");
}

/// Design 0013 section 4: `WriteFate` carries path, offset and len next to
/// `Landing::LengthOnly { zeros_from }`.
#[test]
fn the_report_names_the_write_that_landed_length_only() {
    let base = vec![1; 300];
    let ops = [Op::Write(300, vec![2; 50])];
    let seed = (0..400)
        .find(|&s| is_length_only(landing_of(&run(&base, &ops, s).0, 0)))
        .expect("a length-only seed");
    let (report, _) = run(&base, &ops, seed);
    assert_eq!(report.writes.len(), 1);
    let w = &report.writes[0];
    assert_eq!(w.path, file_path());
    assert_eq!(w.offset, 300);
    assert_eq!(w.len, 50);
    assert_eq!(w.landing, Landing::LengthOnly { zeros_from: 300 });
    assert!(report.vanished.is_empty());
    assert!(report.lengths.is_empty());
}

/// Design 0013 section 6: "끝을 넘는 쓰기는 남음·사라짐·찢김·길이만 넷 다 나오고
/// 보고가 디스크와 맞음".
#[test]
fn an_append_shows_all_four_fates_and_the_report_matches_the_disk() {
    let base = vec![0x11; 700];
    let ops = [Op::Write(700, vec![0x22; 900])]; // spans sectors 1..=3
    let (mut kept, mut lost, mut torn, mut len_only) = (0, 0, 0, 0);
    for seed in 0..600 {
        let (report, disk) = run(&base, &ops, seed);
        match landing_of(&report, 0) {
            Landing::Kept => kept += 1,
            Landing::Lost => lost += 1,
            Landing::Torn { .. } => torn += 1,
            Landing::LengthOnly { .. } => len_only += 1,
        }
        assert_eq!(disk, model(&base, &ops, &report), "seed {seed}");
    }
    assert!(
        kept > 0 && lost > 0 && torn > 0 && len_only > 0,
        "{kept} {lost} {torn} {len_only}"
    );
}

/// Design 0013 decision 1: "쓸 때의 파일 끝을 넘는 쓰기" only; a write wholly
/// inside the old end never lands length-only.
#[test]
fn a_write_inside_the_end_never_lands_length_only() {
    let base = vec![0x33; 2000];
    let ops = [Op::Write(100, vec![0x44; 1000])];
    let mut lost = 0;
    for seed in 0..600 {
        let (report, disk) = run(&base, &ops, seed);
        let l = landing_of(&report, 0);
        assert!(!is_length_only(l), "seed {seed}: {l:?}");
        if *l == Landing::Lost {
            lost += 1;
        }
        assert_eq!(disk.len(), 2000);
        assert_eq!(disk, model(&base, &ops, &report), "seed {seed}");
    }
    assert!(lost > 0);
}

/// Decision 1 boundary: a write that ends exactly at the old end does not go
/// past it.
#[test]
fn a_write_ending_exactly_at_the_end_never_lands_length_only() {
    let base = vec![0x55; 1024];
    let ops = [Op::Write(1000, vec![0x66; 24])];
    for seed in 0..400 {
        let (report, disk) = run(&base, &ops, seed);
        assert!(!is_length_only(landing_of(&report, 0)), "seed {seed}");
        assert_eq!(disk.len(), 1024);
    }
}

/// Decision 2: "끝을 넘는 부분만 0. 끝 안쪽 부분은 디스크에 있던 그대로" and
/// `zeros_from = max(쓰기 시작, 쓸 때의 끝)`.
#[test]
fn a_write_partly_inside_zeroes_only_the_part_past_the_end() {
    let base = vec![0x77; 1000];
    let ops = [Op::Write(900, vec![0x88; 200])];
    let mut found = 0;
    for seed in 0..400 {
        let (report, disk) = run(&base, &ops, seed);
        if let Landing::LengthOnly { zeros_from } = landing_of(&report, 0) {
            found += 1;
            assert_eq!(*zeros_from, 1000);
            assert_eq!(disk.len(), 1100);
            assert!(
                disk[..1000].iter().all(|&b| b == 0x77),
                "inside part stays as on disk"
            );
            assert!(disk[1000..].iter().all(|&b| b == 0));
        }
        assert_eq!(disk, model(&base, &ops, &report), "seed {seed}");
    }
    assert!(found > 0);
}

/// Design 0013 section 6: "끝보다 먼 쓰기는 offset부터 0".
#[test]
fn a_write_past_a_gap_zeroes_from_its_offset() {
    let base = vec![0x99; 1000];
    let ops = [Op::Write(3000, vec![0xAB; 100])];
    let mut found = 0;
    for seed in 0..400 {
        let (report, disk) = run(&base, &ops, seed);
        if let Landing::LengthOnly { zeros_from } = landing_of(&report, 0) {
            found += 1;
            assert_eq!(*zeros_from, 3000);
            assert_eq!(disk.len(), 3100);
            assert!(disk[..1000].iter().all(|&b| b == 0x99));
            assert!(
                disk[1000..].iter().all(|&b| b == 0),
                "gap and write read as zeros"
            );
        }
        assert_eq!(disk, model(&base, &ops, &report), "seed {seed}");
    }
    assert!(found > 0);
}

/// The WAL-header case from the issue: a 20-byte write into an empty file
/// can leave 20 zero bytes.
#[test]
fn a_header_into_an_empty_file_can_leave_twenty_zero_bytes() {
    let header: Vec<u8> = (1..=20).collect();
    let ops = [Op::Write(0, header)];
    let mut found = false;
    for seed in 0..400 {
        let (report, disk) = run(&[], &ops, seed);
        if let Landing::LengthOnly { zeros_from } = landing_of(&report, 0) {
            assert_eq!(*zeros_from, 0);
            assert_eq!(disk, vec![0; 20]);
            found = true;
        }
    }
    assert!(found);
}

/// Decision 1 and section 2 (가): the end is measured "쓸 때 엔진이 본 길이",
/// so if an earlier `set_len(0)` is lost, the zeros cover the old contents
/// ("0이 옛 내용을 덮는다") and the bytes after stay.
#[test]
fn the_end_is_measured_when_written_so_zeros_cover_old_contents() {
    let base = vec![0xCD; 1000];
    let ops = [Op::SetLen(0), Op::Write(0, vec![0xEF; 20])];
    let (mut lost_trunc, mut kept_trunc) = (0, 0);
    for seed in 0..1000 {
        let (report, disk) = run(&base, &ops, seed);
        assert_eq!(disk, model(&base, &ops, &report), "seed {seed}");
        if let Landing::LengthOnly { zeros_from } = landing_of(&report, 0) {
            assert_eq!(*zeros_from, 0);
            if report.lengths[0].landed {
                kept_trunc += 1;
                assert_eq!(disk, vec![0; 20]);
            } else {
                lost_trunc += 1;
                assert_eq!(disk.len(), 1000, "the file only grows, never shrinks");
                assert!(disk[..20].iter().all(|&b| b == 0));
                assert!(disk[20..].iter().all(|&b| b == 0xCD));
            }
        }
    }
    assert!(
        lost_trunc > 0 && kept_trunc > 0,
        "{lost_trunc} {kept_trunc}"
    );
}

/// Decision 4 and section 2: "그 자리에 쓰는 쓰기는 쓸 때 끝 안쪽이므로
/// '길이만'이 되지 않는다" (after `set_len` grows the file).
#[test]
fn a_write_into_space_grown_by_set_len_never_lands_length_only() {
    let base = vec![0x10; 1000];
    let ops = [Op::SetLen(4000), Op::Write(2000, vec![0x20; 600])];
    for seed in 0..600 {
        let (report, disk) = run(&base, &ops, seed);
        assert!(!is_length_only(landing_of(&report, 0)), "seed {seed}");
        assert_eq!(disk, model(&base, &ops, &report), "seed {seed}");
    }
}

/// Decision 4 with a synced `set_len`: the grown file's end is the new
/// length, so writing inside it still cannot land length-only.
#[test]
fn a_write_inside_a_synced_grown_file_never_lands_length_only() {
    for seed in 0..300 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new(DIR)).unwrap();
        let f = fs.open(Path::new(FILE)).unwrap();
        fs.sync_dir(Path::new(DIR)).unwrap();
        f.set_len(4096).unwrap();
        f.sync_data().unwrap();
        f.write_all_at(&[7; 96], 4000).unwrap();
        drop(f);
        let report = fs.crash(seed);
        assert!(!is_length_only(landing_of(&report, 0)), "seed {seed}");
        assert_eq!(read_all(&fs, Path::new(FILE)).len(), 4096);
    }
}

/// Section 2: "적용 순서: 다른 변경과 똑같이, 파일의 waiting 목록 순서대로";
/// several waiting writes each measure the end as it was when written.
#[test]
fn several_waiting_writes_each_use_their_own_end() {
    let ops = [
        Op::Write(0, vec![0xA1; 100]),   // past end 0
        Op::Write(100, vec![0xA2; 100]), // past end 100
        Op::Write(50, vec![0xA3; 100]),  // inside end 200
        Op::Write(150, vec![0xA4; 100]), // partly past end 200
    ];
    let mut seen = [false; 4];
    for seed in 0..2000 {
        let (report, disk) = run(&[], &ops, seed);
        assert_eq!(report.writes.len(), 4);
        // Oldest first, as CrashReport promises.
        let offs: Vec<u64> = report.writes.iter().map(|w| w.offset).collect();
        assert_eq!(offs, vec![0, 100, 50, 150]);
        assert!(
            !is_length_only(landing_of(&report, 2)),
            "seed {seed}: write inside the end"
        );
        for (i, s) in seen.iter_mut().enumerate() {
            if let Landing::LengthOnly { zeros_from } = landing_of(&report, i) {
                *s = true;
                let want = [0, 100, 0, 200][i];
                assert_eq!(*zeros_from, want, "seed {seed} write {i}");
            }
        }
        assert_eq!(disk, model(&[], &ops, &report), "seed {seed}");
    }
    assert!(seen[0] && seen[1] && seen[3], "{seen:?}");
}

/// Section 2: "0이 되는 자리에 디스크가 다른 내용을 들고 있었으면 그것도 0으로
/// 덮는다": a later length-only write zeroes bytes an earlier kept write put.
#[test]
fn a_later_length_only_write_zeroes_an_earlier_kept_write() {
    // Write 2 starts at the end write 1 saw... measured when written: write 1
    // grows to 100; truncation to 0 lost or not, write 2 at 0 is past end 0.
    let ops = [
        Op::Write(0, vec![0xB1; 100]),
        Op::SetLen(0),
        Op::Write(0, vec![0xB2; 60]),
    ];
    let mut found = false;
    for seed in 0..3000 {
        let (report, disk) = run(&[], &ops, seed);
        assert_eq!(disk, model(&[], &ops, &report), "seed {seed}");
        if *landing_of(&report, 0) == Landing::Kept
            && !report.lengths[0].landed
            && is_length_only(landing_of(&report, 1))
        {
            found = true;
            assert_eq!(disk.len(), 100);
            assert!(disk[..60].iter().all(|&b| b == 0));
            assert!(disk[60..].iter().all(|&b| b == 0xB1));
        }
    }
    assert!(found);
}

// ---------------------------------------------------------------------------
// Dice and determinism

/// Design 0008 section 3: "같은 시드면 매번 같은 메모가 날아간다".
#[test]
fn the_same_seed_and_operations_give_the_same_disk_and_report() {
    let base = vec![3; 900];
    let ops = [
        Op::Write(900, vec![4; 700]),
        Op::SetLen(100),
        Op::Write(100, vec![5; 30]),
        Op::Write(5000, vec![6; 10]),
    ];
    for seed in 0..300 {
        let a = run(&base, &ops, seed);
        let b = run(&base, &ops, seed);
        assert_eq!(a, b, "seed {seed}");
    }
}

/// Design 0008 section 3, "sync한 쓰기는 200개 시드 모두에서 남음": a synced
/// append is never zeroed.
#[test]
fn synced_data_never_changes() {
    for seed in 0..300 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new(DIR)).unwrap();
        let f = fs.open(Path::new(FILE)).unwrap();
        fs.sync_dir(Path::new(DIR)).unwrap();
        f.write_all_at(&[9; 20], 0).unwrap();
        f.sync_data().unwrap();
        f.write_all_at(&[8; 1000], 20).unwrap();
        f.sync_data().unwrap();
        drop(f);
        let report = fs.crash(seed);
        assert!(report.writes.is_empty() && report.lengths.is_empty());
        let disk = read_all(&fs, Path::new(FILE));
        assert_eq!(&disk[..20], &[9; 20]);
        assert_eq!(&disk[20..], &[8; 1000][..]);
    }
}

/// Decision 5 / section 3 (다): "남음 1/3, 찢김 1/3, 사라짐 1/6, 길이만 1/6".
#[test]
fn an_append_lands_with_the_designed_frequencies() {
    let base = vec![1; 100];
    let ops = [Op::Write(100, vec![2; 1000])];
    let n = 6000;
    let mut c = [0u32; 4];
    for seed in 0..n {
        let (report, _) = run(&base, &ops, seed);
        let i = match landing_of(&report, 0) {
            Landing::Kept => 0,
            Landing::Torn { .. } => 1,
            Landing::Lost => 2,
            Landing::LengthOnly { .. } => 3,
        };
        c[i] += 1;
    }
    let f = |x: u32| x as f64 / n as f64;
    let want = [1.0 / 3.0, 1.0 / 3.0, 1.0 / 6.0, 1.0 / 6.0];
    for i in 0..4 {
        assert!((f(c[i]) - want[i]).abs() < 0.04, "counts {c:?}");
    }
}

/// Decision 5: a write inside the end keeps "각 1/3" (kept, lost, torn) and
/// never lands length-only.
#[test]
fn a_write_inside_lands_a_third_each() {
    let base = vec![1; 2000];
    let ops = [Op::Write(100, vec![2; 1000])];
    let n = 6000;
    let mut c = [0u32; 3];
    for seed in 0..n {
        let (report, _) = run(&base, &ops, seed);
        match landing_of(&report, 0) {
            Landing::Kept => c[0] += 1,
            Landing::Torn { .. } => c[1] += 1,
            Landing::Lost => c[2] += 1,
            other => panic!("seed {seed}: {other:?}"),
        }
    }
    for x in c {
        assert!(
            (x as f64 / n as f64 - 1.0 / 3.0).abs() < 0.04,
            "counts {c:?}"
        );
    }
}

/// Decision 5: "주사위를 더 던지지 않는다 ... 끝을 넘는 쓰기가 '사라짐'을 뽑았을
/// 때만 바뀜". The same write placed inside the end draws the same die: kept
/// and torn stay identical, and lost-or-length-only becomes lost.
#[test]
fn length_only_reuses_the_lost_roll_and_throws_no_extra_dice() {
    let data = vec![0x5A; 100];
    let append_base = vec![1; 1000]; // write 1000..1100 is past the end
    let inside_base = vec![1; 1100]; // the same write is inside the end
    let ops = [Op::Write(1000, data)];
    for seed in 0..1000 {
        let (a, _) = run(&append_base, &ops, seed);
        let (b, _) = run(&inside_base, &ops, seed);
        let (la, lb) = (landing_of(&a, 0), landing_of(&b, 0));
        match la {
            Landing::Lost | Landing::LengthOnly { .. } => {
                assert_eq!(*lb, Landing::Lost, "seed {seed}")
            }
            _ => assert_eq!(la, lb, "seed {seed}"),
        }
    }
}

/// Decision 5 consequence: a length-only roll does not shift the dice of the
/// writes after it (another file, later in path order).
#[test]
fn a_length_only_write_does_not_shift_later_fates() {
    fn two_files(first_inside: bool, seed: u64) -> CrashReport {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new(DIR)).unwrap();
        let a = fs.open(Path::new("/d/a")).unwrap();
        let b = fs.open(Path::new("/d/b")).unwrap();
        fs.sync_dir(Path::new(DIR)).unwrap();
        a.write_all_at(&vec![1; if first_inside { 1100 } else { 1000 }], 0)
            .unwrap();
        a.sync_data().unwrap();
        b.write_all_at(&[2; 1000], 0).unwrap();
        b.sync_data().unwrap();
        a.write_all_at(&[3; 100], 1000).unwrap();
        b.write_all_at(&[4; 1500], 1000).unwrap(); // append on b
        b.write_all_at(&[5; 300], 200).unwrap(); // inside b
        drop((a, b));
        fs.crash(seed)
    }
    for seed in 0..1000 {
        let x = two_files(false, seed);
        let y = two_files(true, seed);
        assert_eq!(x.writes.len(), 3);
        assert_eq!(x.writes[0].path, PathBuf::from("/d/a"));
        assert_eq!(x.writes[1..], y.writes[1..], "seed {seed}");
    }
}

/// Issue memo / decision 6: seeds whose report has no length-only write give
/// the old disk. Here: most seeds have no length-only write at all.
#[test]
fn most_seeds_have_no_length_only_write() {
    let base = vec![1; 100];
    let ops = [Op::Write(100, vec![2; 50]), Op::Write(150, vec![3; 50])];
    let with = (0..2000)
        .filter(|&s| {
            run(&base, &ops, s)
                .0
                .writes
                .iter()
                .any(|w| is_length_only(&w.landing))
        })
        .count();
    // Each write is length-only 1/6 of the time: 1 - (5/6)^2 ≈ 0.31.
    let frac = with as f64 / 2000.0;
    assert!((frac - 11.0 / 36.0).abs() < 0.05, "{frac}");
}

/// Design 0008 section 2: a file whose directory was never synced may
/// vanish; the length-only fate never brings it back.
#[test]
fn a_vanished_file_reports_no_writes() {
    for seed in 0..300 {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new(DIR)).unwrap();
        let f = fs.open(Path::new(FILE)).unwrap();
        f.write_all_at(&[1; 20], 0).unwrap();
        f.sync_data().unwrap();
        drop(f);
        let report = fs.crash(seed);
        if report.vanished.contains(&file_path()) {
            assert!(!fs.exists(Path::new(FILE)));
            assert!(report.writes.iter().all(|w| w.path != file_path()));
        }
    }
}

// ---------------------------------------------------------------------------
// Engine: the promise under this fate

fn oak() -> PathBuf {
    PathBuf::from("/oak")
}

fn wal() -> PathBuf {
    oak().join(WAL_FILE)
}

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

fn contents(db: &Db<SimFs>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let txn = db.begin();
    txn.scan::<&[u8]>(..).map(|r| r.unwrap()).collect()
}

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// Runs one power-cut round: open with a cut, commit and checkpoint until
/// the cut, crash. Returns the acknowledged model and the commit in flight.
fn round(
    fs: &SimFs,
    rng: &mut Rng,
    model: &Model,
    round_no: u64,
) -> (Model, Option<Model>, CrashReport) {
    let mut acked = model.clone();
    let mut pending = None;
    fs.fail_after(rng.below(70));
    if let Ok(db) = Db::open_with(fs.clone(), oak()) {
        for step in 0..10 {
            if rng.below(3) == 0 {
                if db.checkpoint().is_err() {
                    break;
                }
                continue;
            }
            let mut txn = db.begin();
            let mut writes = Model::new();
            for _ in 0..=rng.below(3) {
                let key = format!("k{:02}", rng.below(24)).into_bytes();
                let len = [5usize, 300, 1500][rng.below(3) as usize];
                let value = vec![(round_no * 16 + step) as u8; len];
                txn.put(&key, &value).unwrap();
                writes.insert(key, value);
            }
            match txn.commit() {
                Ok(()) => acked.extend(writes),
                Err(_) => {
                    let mut after = acked.clone();
                    after.extend(writes);
                    pending = Some(after);
                    break;
                }
            }
        }
        drop(db);
    }
    let report = fs.crash(rng.next());
    (acked, pending, report)
}

/// Issue "왜" + design 0013 section 6: the database keeps every acknowledged
/// commit across power cuts at any point (commits and checkpoints), with
/// length-only writes in the mix.
#[test]
fn every_acknowledged_commit_survives_power_cuts_with_length_only_writes() {
    let mut length_only_on_wal = 0;
    for seed in 0..300u64 {
        let fs = SimFs::new();
        let mut rng = Rng(seed);
        let mut model = Model::new();
        for r in 0..4 {
            let (acked, pending, report) = round(&fs, &mut rng, &model, r);
            length_only_on_wal += report
                .writes
                .iter()
                .filter(|w| w.path == wal() && is_length_only(&w.landing))
                .count();
            let db = Db::open_with(fs.clone(), oak()).unwrap_or_else(|e| {
                panic!("seed {seed} round {r}: open failed: {e:?}\n{report:?}")
            });
            let got = contents(&db);
            if got == acked {
                model = acked;
            } else if pending.as_ref() == Some(&got) {
                model = got;
            } else {
                panic!("seed {seed} round {r}: lost or invented data\n{report:?}");
            }
            drop(db);
        }
    }
    assert!(
        length_only_on_wal > 0,
        "the fate never touched the WAL: the test is vacuous"
    );
}

/// Design 0013 section 6 (checkpoint.rs plan) and design 0011 section 1:
/// a power cut at every operation of a checkpoint, several seeds each,
/// keeps every commit. This is where C3 / C16 live.
#[test]
fn a_power_cut_anywhere_in_a_checkpoint_keeps_every_commit() {
    fn setup() -> (SimFs, Model) {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), oak()).unwrap();
        let mut model = Model::new();
        for i in 0..12u8 {
            let mut txn = db.begin();
            let k = vec![b'k', i];
            let v = vec![i; 700];
            txn.put(&k, &v).unwrap();
            txn.commit().unwrap();
            model.insert(k, v);
        }
        drop(db);
        (fs, model)
    }
    // Count the operations of one checkpoint.
    let (fs, _) = setup();
    let db = Db::open_with(fs.clone(), oak()).unwrap();
    let before = fs.ops();
    db.checkpoint().unwrap();
    let ops = fs.ops() - before;
    drop(db);
    assert!(ops > 0);

    let mut length_only_wal_header = 0;
    for cut in 0..=ops {
        for seed in 0..48 {
            let (fs, model) = setup();
            let db = Db::open_with(fs.clone(), oak()).unwrap();
            fs.fail_after(cut);
            let _ = db.checkpoint();
            drop(db);
            let report = fs.crash(seed);
            if report.writes.iter().any(|w| {
                w.path == wal()
                    && w.offset == 0
                    && w.landing == Landing::LengthOnly { zeros_from: 0 }
            }) {
                length_only_wal_header += 1;
            }
            let db = Db::open_with(fs.clone(), oak())
                .unwrap_or_else(|e| panic!("cut {cut} seed {seed}: {e:?}\n{report:?}"));
            assert_eq!(contents(&db), model, "cut {cut} seed {seed}\n{report:?}");
            // And it keeps working.
            let mut txn = db.begin();
            txn.put(b"after", b"x").unwrap();
            txn.commit().unwrap();
            drop(db);
            fs.crash(seed + 1000);
            let db = Db::open_with(fs.clone(), oak()).unwrap();
            assert_eq!(
                db.begin().get(b"after").unwrap(),
                Some(b"x".to_vec()),
                "cut {cut} seed {seed}"
            );
        }
    }
    assert!(
        length_only_wal_header > 0,
        "no power cut left a length-only WAL header"
    );
}

/// Design 0013 section 6: "머리말 sync에서 끊으면 정전이 20바이트 0 WAL을
/// 만들고 DB가 열림 (P1을 손으로 만들지 않고 재현)".
#[test]
fn a_power_cut_can_leave_a_twenty_zero_byte_wal_and_the_database_opens() {
    let mut found = 0;
    for cut in 0..40 {
        for seed in 0..32 {
            let fs = SimFs::new();
            let db = Db::open_with(fs.clone(), oak()).unwrap();
            let mut txn = db.begin();
            txn.put(b"a", b"1").unwrap();
            txn.commit().unwrap();
            fs.fail_after(cut);
            let _ = db.checkpoint();
            drop(db);
            fs.crash(seed);
            let f = fs.open(&wal()).unwrap();
            let len = f.size().unwrap();
            let mut buf = vec![0; len as usize];
            f.read_exact_at(&mut buf, 0).unwrap();
            drop(f);
            if len == 20 && buf.iter().all(|&b| b == 0) {
                found += 1;
                let db = Db::open_with(fs.clone(), oak()).unwrap();
                assert_eq!(db.begin().get(b"a").unwrap(), Some(b"1".to_vec()));
            }
        }
    }
    assert!(found > 0, "no power cut produced a 20-byte zero WAL");
}

fn checkpointed_db_then_wal(wal_bytes: &[u8]) -> SimFs {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), oak()).unwrap();
    let mut txn = db.begin();
    txn.put(b"key", b"value").unwrap();
    txn.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let f = fs.open(&wal()).unwrap();
    f.set_len(0).unwrap();
    f.sync_data().unwrap();
    if !wal_bytes.is_empty() {
        f.write_all_at(wal_bytes, 0).unwrap();
    }
    f.sync_data().unwrap();
    drop(f);
    fs
}

/// Design 0011 section 3 / 9 P1: "정확히 20바이트인데 머리말이 틀린 경우 ...
/// 20바이트 이하면 머리말 없음으로 보고 새로 씀".
#[test]
fn a_wal_of_exactly_twenty_zero_bytes_after_a_checkpoint_opens() {
    let fs = checkpointed_db_then_wal(&[0; 20]);
    let db =
        Db::open_with(fs.clone(), oak()).expect("a 20-byte zero WAL is a header that never landed");
    assert_eq!(db.begin().get(b"key").unwrap(), Some(b"value".to_vec()));
    let mut txn = db.begin();
    txn.put(b"more", b"data").unwrap();
    txn.commit().unwrap();
    drop(db);
    fs.crash(0);
    let db = Db::open_with(fs.clone(), oak()).unwrap();
    assert_eq!(db.begin().get(b"key").unwrap(), Some(b"value".to_vec()));
    assert_eq!(db.begin().get(b"more").unwrap(), Some(b"data".to_vec()));
}

/// Design 0011 section 3: "머리말 길이(20바이트) 이하라면 ... 새로 쓴다" — also
/// shorter zero WALs and an empty one.
#[test]
fn a_wal_shorter_than_a_header_after_a_checkpoint_opens() {
    for n in [0usize, 1, 7, 19] {
        let fs = checkpointed_db_then_wal(&vec![0; n]);
        let db = Db::open_with(fs.clone(), oak()).unwrap_or_else(|e| panic!("{n} bytes: {e:?}"));
        assert_eq!(
            db.begin().get(b"key").unwrap(),
            Some(b"value".to_vec()),
            "{n}"
        );
    }
}

/// Design 0011 section 3: "그보다 길면 손상이라 거절한다".
#[test]
fn a_longer_wal_with_a_zeroed_header_is_rejected() {
    let fs = checkpointed_db_then_wal(&[0; 40]);
    match Db::open_with(fs.clone(), oak()) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt, got {e:?}"),
        Ok(_) => panic!("a 40-byte WAL with a zeroed header opened"),
    }
}

/// Design 0011 section 3: the same 20-zero-byte WAL on a brand-new database
/// (never checkpointed) is also "no header".
#[test]
fn a_twenty_zero_byte_wal_on_a_new_database_opens() {
    let fs = SimFs::new();
    drop(Db::open_with(fs.clone(), oak()).unwrap());
    let f = fs.open(&wal()).unwrap();
    f.set_len(0).unwrap();
    f.write_all_at(&[0; 20], 0).unwrap();
    f.sync_data().unwrap();
    drop(f);
    let db = Db::open_with(fs.clone(), oak()).unwrap();
    assert!(contents(&db).is_empty());
}
