//! Blind tests for checkpoints and the two meta slots (#23).
//!
//! Written from issue #23, `docs/design/0011-checkpoint.md`, `0010-wal.md`,
//! `0008-sim-fs.md`, ADR 0007 and the public API only, without reading the
//! implementation. Each test cites the sentence it comes from.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oakdb::fs::sim::{SimFile, SimFs};
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error, CHECKPOINT_AFTER, DATA_FILE, WAL_FILE};

const DIR: &str = "/oak";
const PAGE: usize = 4096;
const HEADER: usize = 20;
const START: u32 = 0x0AC0_DB10;
const WAL_MAGIC: &[u8; 8] = b"oakwal\0\0";
const META_MAGIC: &[u8; 8] = b"oakdb\0\0\0";

// ---------------------------------------------------------------- helpers

fn path(name: &str) -> PathBuf {
    Path::new(DIR).join(name)
}

fn read(fs: &SimFs, name: &str) -> Vec<u8> {
    let file = fs.open(&path(name)).unwrap();
    let len = file.size().unwrap() as usize;
    let mut buf = vec![0; len];
    file.read_exact_at(&mut buf, 0).unwrap();
    buf
}

/// Replaces the whole file and syncs it.
fn replace(fs: &SimFs, name: &str, bytes: &[u8]) {
    let file = fs.open(&path(name)).unwrap();
    file.set_len(0).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    file.sync_data().unwrap();
}

fn patch(fs: &SimFs, name: &str, offset: u64, bytes: &[u8]) {
    let file = fs.open(&path(name)).unwrap();
    file.write_all_at(bytes, offset).unwrap();
    file.sync_data().unwrap();
}

fn flip(fs: &SimFs, name: &str, offset: u64) {
    let byte = read(fs, name)[offset as usize];
    patch(fs, name, offset, &[byte ^ 0xFF]);
}

fn open(fs: &SimFs) -> oakdb::Result<Db<SimFs>> {
    Db::open_with(fs.clone(), DIR)
}

fn put<F: Fs>(db: &Db<F>, key: &str, value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key.as_bytes(), value).unwrap();
    txn.commit().unwrap();
}

fn del<F: Fs>(db: &Db<F>, key: &str) {
    let mut txn = db.begin();
    txn.delete(key.as_bytes()).unwrap();
    txn.commit().unwrap();
}

fn get<F: Fs>(db: &Db<F>, key: &str) -> Option<Vec<u8>> {
    db.begin().get(key.as_bytes()).unwrap()
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// The checkpoint number in meta slot `slot` (0 = A, 1 = B), offset 52.
fn slot_checkpoint(data: &[u8], slot: usize) -> u64 {
    u64_at(data, slot * PAGE + 52)
}

fn page_checksum_ok(data: &[u8], page_no: usize) -> bool {
    let page = &data[page_no * PAGE..(page_no + 1) * PAGE];
    let crc = crc32c::crc32c_append(crc32c::crc32c(&page[4..]), &(page_no as u64).to_le_bytes());
    u32_at(page, 0) == crc
}

fn wal_header(checkpoint: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER);
    h.extend_from_slice(WAL_MAGIC);
    h.extend_from_slice(&checkpoint.to_le_bytes());
    let crc = crc32c::crc32c_append(START, &h);
    h.extend_from_slice(&crc.to_le_bytes());
    h
}

/// Checks the WAL header and returns its checkpoint number.
fn wal_checkpoint(wal: &[u8]) -> u64 {
    assert!(
        wal.len() >= HEADER,
        "WAL is {} bytes, shorter than its header",
        wal.len()
    );
    assert_eq!(&wal[..8], WAL_MAGIC, "WAL magic");
    assert_eq!(
        u32_at(wal, 16),
        crc32c::crc32c_append(START, &wal[..16]),
        "WAL header crc"
    );
    u64_at(wal, 8)
}

fn value(i: usize, len: usize) -> Vec<u8> {
    let mut v = format!("value-{i:06}-").into_bytes();
    while v.len() < len {
        v.push(b'a' + (v.len() % 26) as u8);
    }
    v.truncate(len);
    v
}

fn key(i: usize) -> String {
    format!("key{i:05}")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A database with enough entries to span many pages and an overflow value.
fn populated(fs: &SimFs) -> Db<SimFs> {
    let db = open(fs).unwrap();
    for i in 0..150 {
        put(&db, &key(i), &value(i, 120));
    }
    put(&db, "big", &value(9999, 10_000));
    db
}

fn check_populated<F: Fs>(db: &Db<F>) {
    for i in 0..150 {
        assert_eq!(get(db, &key(i)), Some(value(i, 120)), "key {i}");
    }
    assert_eq!(get(db, "big"), Some(value(9999, 10_000)));
}

fn is_corrupt<T>(r: oakdb::Result<T>) -> bool {
    matches!(r, Err(Error::Corrupt(_)))
}

// ------------------------------------------------- after a checkpoint (5절, 7절)

// 7절: "정리 뒤 WAL은 머리말만 남고 데이터 파일 = page_count × 4096"
#[test]
fn after_a_checkpoint_the_wal_is_only_its_header() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let wal = read(&fs, WAL_FILE);
    assert_eq!(wal.len(), HEADER);
    assert_eq!(wal_checkpoint(&wal), 1);
}

// 5절: "정리 뒤 데이터 파일 길이 = page_count() × 4096. 테스트로 확인한다."
#[test]
fn after_a_checkpoint_the_data_file_is_page_count_pages_long() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    assert_eq!(
        read(&fs, DATA_FILE).len() as u64,
        db.page_count() * PAGE as u64
    );
    // Also after deletes that free pages.
    for i in 0..100 {
        del(&db, &key(i));
    }
    del(&db, "big");
    db.checkpoint().unwrap();
    assert_eq!(
        read(&fs, DATA_FILE).len() as u64,
        db.page_count() * PAGE as u64
    );
}

// 6절 "장부의 모든 페이지가 처음부터 체크섬을 가진다" (결정 6) and design 0007 1절.
#[test]
fn every_page_of_a_checkpointed_data_file_has_a_valid_checksum() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let data = read(&fs, DATA_FILE);
    for page_no in 0..data.len() / PAGE {
        assert!(page_checksum_ok(&data, page_no), "page {page_no}");
    }
}

// 2절 메타 페이지 형식 버전 4, offset 52 = checkpoint; 0001 magic at 4.
#[test]
fn meta_slots_record_format_version_4_and_the_page_count() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let data = read(&fs, DATA_FILE);
    for slot in 0..2 {
        let at = slot * PAGE;
        assert_eq!(&data[at + 4..at + 12], META_MAGIC, "slot {slot} magic");
        assert_eq!(u32_at(&data, at + 12), 4, "slot {slot} format version");
        assert_eq!(u32_at(&data, at + 16), 4096, "slot {slot} page size");
        assert_eq!(
            u64_at(&data, at + 28),
            db.page_count(),
            "slot {slot} page_count"
        );
    }
}

// 결정 6: "첫 정리: 표지 두 장 모두에 1회차를 적는다"
#[test]
fn the_first_checkpoint_writes_checkpoint_1_in_both_slots() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let data = read(&fs, DATA_FILE);
    assert_eq!(slot_checkpoint(&data, 0), 1);
    assert_eq!(slot_checkpoint(&data, 1), 1);
    // Same content; each slot sealed with its own page number.
    assert_eq!(data[4..PAGE], data[PAGE + 4..2 * PAGE]);
    assert!(page_checksum_ok(&data, 0));
    assert!(page_checksum_ok(&data, 1));
}

// 결정 6: "그 뒤로는 번갈아 쓴다 (2회차 → A, 3회차 → B, …)"; 1절 ② "슬롯 (n+1) % 2"
#[test]
fn later_checkpoints_alternate_between_the_slots() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let expected = [(2, 1), (2, 3), (4, 3), (4, 5)];
    for (round, (a, b)) in expected.into_iter().enumerate() {
        put(&db, &format!("round{round}"), b"x");
        db.checkpoint().unwrap();
        let data = read(&fs, DATA_FILE);
        assert_eq!(
            (slot_checkpoint(&data, 0), slot_checkpoint(&data, 1)),
            (a, b)
        );
        assert_eq!(wal_checkpoint(&read(&fs, WAL_FILE)), a.max(b));
    }
}

// 4절: "정리할 것이 없으면(WAL에 커밋이 없으면) 아무것도 쓰지 않는다."
#[test]
fn a_checkpoint_with_nothing_to_move_writes_nothing() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let data = read(&fs, DATA_FILE);
    let wal = read(&fs, WAL_FILE);
    db.checkpoint().unwrap();
    db.checkpoint().unwrap();
    assert_eq!(read(&fs, DATA_FILE), data);
    assert_eq!(read(&fs, WAL_FILE), wal);
}

// Same sentence, stricter: "아무것도 쓰지 않는다" counted with SimFs::ops.
#[test]
fn a_checkpoint_with_nothing_to_move_changes_nothing_on_disk() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    let before = fs.ops();
    db.checkpoint().unwrap();
    assert_eq!(fs.ops(), before);
}

// 결정 5: "close()도 정리한다. 닫고 다시 열면 WAL을 읽을 게 없다."
#[test]
fn close_checkpoints() {
    let fs = SimFs::new();
    let db = populated(&fs);
    let pages = db.page_count();
    db.close().unwrap();
    let wal = read(&fs, WAL_FILE);
    assert_eq!(wal.len(), HEADER);
    assert_eq!(wal_checkpoint(&wal), 1);
    assert_eq!(read(&fs, DATA_FILE).len() as u64, pages * PAGE as u64);
    let db = open(&fs).unwrap();
    check_populated(&db);
    assert_eq!(db.page_count(), pages);
}

// 완료 조건 1 (결정 2로 읽음): "WAL이 비워지고 sync된다" — a crash right
// after a checkpoint, whatever the seed, keeps the emptied WAL and the data.
#[test]
fn what_a_checkpoint_writes_survives_any_crash_after_it() {
    for seed in 0..40 {
        let fs = SimFs::new();
        let db = populated(&fs);
        db.checkpoint().unwrap();
        let data = read(&fs, DATA_FILE);
        drop(db);
        fs.crash(seed);
        assert_eq!(read(&fs, DATA_FILE), data, "seed {seed}");
        let wal = read(&fs, WAL_FILE);
        assert_eq!(wal.len(), HEADER, "seed {seed}");
        assert_eq!(wal_checkpoint(&wal), 1, "seed {seed}");
        check_populated(&open(&fs).unwrap());
    }
}

// 3절: "s 있음, h = s → s 위에 WAL 재생"
#[test]
fn commits_after_a_checkpoint_are_replayed_on_top_of_the_slot() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    put(&db, &key(3), b"changed");
    del(&db, &key(4));
    put(&db, "new", b"after");
    drop(db);
    fs.crash(1);
    let db = open(&fs).unwrap();
    assert_eq!(get(&db, &key(3)), Some(b"changed".to_vec()));
    assert_eq!(get(&db, &key(4)), None);
    assert_eq!(get(&db, "new"), Some(b"after".to_vec()));
    assert_eq!(get(&db, &key(5)), Some(value(5, 120)));
}

// 2절 WAL 머리말: "checkpoint: 이 WAL이 이어 쓰는 회차 (새 DB는 0)"
#[test]
fn a_new_database_wal_starts_with_a_checkpoint_0_header() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    put(&db, "a", b"1");
    drop(db);
    let wal = read(&fs, WAL_FILE);
    assert!(wal.len() > HEADER);
    assert_eq!(wal_checkpoint(&wal), 0);
}

// ------------------------------------------------------ choosing a slot (3절)

// 완료 조건 3 "메타 슬롯 하나가 깨지면 다른 슬롯으로 연다" / 6절 "메타 A·B 어느
// 쪽이 깨져도 열린다 (첫 정리가 두 장 다 쓰므로)"
#[test]
fn after_the_first_checkpoint_either_damaged_slot_still_opens() {
    for slot in 0..2u64 {
        let fs = SimFs::new();
        populated(&fs).close().unwrap();
        flip(&fs, DATA_FILE, slot * PAGE as u64 + 2000);
        let db = open(&fs).unwrap_or_else(|e| panic!("slot {slot}: {e}"));
        check_populated(&db);
    }
}

// 결정 3: "형식 자리(magic·버전)가 깨진 표지도 똑같이 '깨진 표지'로 본다"
#[test]
fn a_slot_with_damaged_format_fields_counts_as_damaged() {
    for (offset, what) in [(4, "magic"), (12, "version"), (16, "page size")] {
        for slot in 0..2u64 {
            let fs = SimFs::new();
            populated(&fs).close().unwrap();
            flip(&fs, DATA_FILE, slot * PAGE as u64 + offset);
            let db = open(&fs).unwrap_or_else(|e| panic!("slot {slot} {what}: {e}"));
            check_populated(&db);
        }
    }
}

// 3절 "표지가 깨졌는데 열리는 경우: ... 최신이 아닌 쪽 표지가 깨진 경우"
#[test]
fn a_damaged_older_slot_does_not_stop_opening() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap(); // 1 in A and B
    put(&db, "second", b"2");
    db.close().unwrap(); // 2 in A
    let data = read(&fs, DATA_FILE);
    assert_eq!(
        (slot_checkpoint(&data, 0), slot_checkpoint(&data, 1)),
        (2, 1)
    );
    flip(&fs, DATA_FILE, PAGE as u64 + 2000); // B, the older one
    let db = open(&fs).unwrap();
    check_populated(&db);
    assert_eq!(get(&db, "second"), Some(b"2".to_vec()));
}

// 결정 3 / 3절 "s 있음, h > s → 거절 (Corrupt): 최신 표지가 깨졌다";
// 7절 "최신 표지가 정리 뒤 깨지면 거절"
#[test]
fn a_damaged_newest_slot_after_its_checkpoint_is_refused() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    put(&db, "second", b"2");
    db.close().unwrap(); // A = 2, B = 1, WAL header 2
    flip(&fs, DATA_FILE, 2000); // A
    assert!(is_corrupt(open(&fs)));
}

// 3절 "성한 슬롯 중 회차가 큰 쪽 = s": with B newer (3) and A older (2).
#[test]
fn the_newest_valid_slot_is_used_when_it_is_slot_b() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    put(&db, "second", b"2");
    db.checkpoint().unwrap();
    put(&db, "third", b"3");
    db.close().unwrap();
    let data = read(&fs, DATA_FILE);
    assert_eq!(
        (slot_checkpoint(&data, 0), slot_checkpoint(&data, 1)),
        (2, 3)
    );
    let db = open(&fs).unwrap();
    assert_eq!(get(&db, "third"), Some(b"3".to_vec()));
    drop(db);
    // Damage the older A: still opens on B.
    flip(&fs, DATA_FILE, 2000);
    let db = open(&fs).unwrap();
    assert_eq!(get(&db, "third"), Some(b"3".to_vec()));
    drop(db);
    // Damage B too: nothing matches the WAL header 3.
    flip(&fs, DATA_FILE, PAGE as u64 + 2000);
    assert!(open(&fs).is_err());
}

// 3절 "그 밖 → 거절: 슬롯 A의 오류를 돌려줌": both slots damaged.
#[test]
fn both_slots_damaged_after_a_checkpoint_is_refused() {
    let fs = SimFs::new();
    populated(&fs).close().unwrap();
    flip(&fs, DATA_FILE, 2000);
    flip(&fs, DATA_FILE, PAGE as u64 + 2000);
    assert!(is_corrupt(open(&fs)));
}

// 2절 "버전 3 파일은 버전으로 거절한다"; 3절 "그 밖 → 슬롯 A의 오류".
#[test]
fn a_version_3_data_file_is_refused_by_version() {
    let fs = SimFs::new();
    populated(&fs).close().unwrap();
    patch(&fs, DATA_FILE, 12, &3u32.to_le_bytes());
    patch(&fs, DATA_FILE, PAGE as u64 + 12, &3u32.to_le_bytes());
    let Err(err) = open(&fs) else {
        panic!("version 3 opened")
    };
    assert!(err.to_string().contains("version"), "{err}");
}

// 3절 "s 있음, h < s → WAL은 옛것: 무시하고 머리말 s로 새로 씀" (1절 표
// "② 뒤 ③ 전 | 표지 n+1, WAL n")
#[test]
fn an_older_wal_is_ignored_and_rewritten() {
    let fs = SimFs::new();
    let db = populated(&fs);
    put(&db, "k", b"old");
    let old_wal = read(&fs, WAL_FILE); // header 0, ends with k = old
    put(&db, "k", b"new");
    db.close().unwrap(); // slots 1, WAL header 1
    replace(&fs, WAL_FILE, &old_wal);
    let db = open(&fs).unwrap();
    assert_eq!(get(&db, "k"), Some(b"new".to_vec()), "old WAL was replayed");
    check_populated(&db);
    drop(db);
    let wal = read(&fs, WAL_FILE);
    assert_eq!(wal_checkpoint(&wal), 1);
    assert_eq!(wal.len(), HEADER);
}

// 3절 "s 있음, ... WAL이 머리말보다 짧음 → 옛것 ... 머리말 s로 새로 씀"
// (1절 표 "③ 도중 | 표지 n+1, 빈 WAL" → "빈 WAL이면 머리말을 쓴다")
#[test]
fn a_wal_shorter_than_its_header_gets_a_new_header() {
    for len in [0, 1, 8, 16, 19] {
        let fs = SimFs::new();
        populated(&fs).close().unwrap();
        let wal = read(&fs, WAL_FILE);
        replace(&fs, WAL_FILE, &wal[..len]);
        let db = open(&fs).unwrap_or_else(|e| panic!("len {len}: {e}"));
        check_populated(&db);
        put(&db, "after", b"1");
        drop(db);
        let wal = read(&fs, WAL_FILE);
        assert_eq!(wal_checkpoint(&wal), 1, "len {len}");
        let db = open(&fs).unwrap();
        assert_eq!(get(&db, "after"), Some(b"1".to_vec()), "len {len}");
    }
}

// 3절 "머리말이 찢어진 WAL: 머리말 길이 이하라면 ... 머리말이 없는 것으로 본다"
#[test]
fn a_torn_header_no_longer_than_the_header_counts_as_no_header() {
    let fs = SimFs::new();
    populated(&fs).close().unwrap();
    flip(&fs, WAL_FILE, 10); // 20 bytes, crc no longer matches
    let db = open(&fs).unwrap();
    check_populated(&db);
    drop(db);
    assert_eq!(wal_checkpoint(&read(&fs, WAL_FILE)), 1);
}

// 3절 "그보다 길면 손상이라 거절한다."
#[test]
fn a_torn_header_followed_by_records_is_refused() {
    let fs = SimFs::new();
    let db = populated(&fs);
    db.checkpoint().unwrap();
    put(&db, "after", b"1");
    drop(db);
    assert!(read(&fs, WAL_FILE).len() > HEADER);
    flip(&fs, WAL_FILE, 10);
    assert!(is_corrupt(open(&fs)));
}

// 3절 "s 있음, h > s → 거절 (Corrupt)" with a WAL header that is valid.
#[test]
fn a_wal_header_newer_than_every_slot_is_refused() {
    let fs = SimFs::new();
    populated(&fs).close().unwrap();
    replace(&fs, WAL_FILE, &wal_header(2));
    assert!(is_corrupt(open(&fs)));
}

// 2절 "첫 레코드의 사슬은 START가 아니라 머리말의 crc에서 시작한다. 그래서
// 다른 회차의 WAL에서 온 레코드는 이 머리말 뒤에서 통과하지 못한다."
#[test]
fn records_from_another_checkpoints_wal_do_not_replay() {
    let fs = SimFs::new();
    let db = populated(&fs);
    put(&db, "k", b"old");
    let old_wal = read(&fs, WAL_FILE); // header 0 + records ending k = old
    put(&db, "k", b"new");
    db.close().unwrap(); // checkpoint 1
    let mut forged = wal_header(1);
    forged.extend_from_slice(&old_wal[HEADER..]);
    replace(&fs, WAL_FILE, &forged);
    let db = open(&fs).unwrap();
    assert_eq!(get(&db, "k"), Some(b"new".to_vec()));
    check_populated(&db);
}

// 3절 "s 없음, h = 0, 커밋 있음 → 장부는 무시하고 WAL만 재생 (첫 정리 도중 정전)"
#[test]
fn a_garbage_data_file_before_the_first_checkpoint_is_ignored() {
    let fs = SimFs::new();
    let db = populated(&fs);
    drop(db); // no checkpoint: data file empty, WAL header 0 with commits
    assert_eq!(wal_checkpoint(&read(&fs, WAL_FILE)), 0);
    // A half-written first checkpoint: pages of junk, no valid slot.
    let junk: Vec<u8> = (0..5 * PAGE).map(|i| (i * 7 + 3) as u8).collect();
    replace(&fs, DATA_FILE, &junk);
    let db = open(&fs).unwrap();
    check_populated(&db);
}

// 3절 "s 없음, 데이터 파일 비어 있음, 커밋 없음 → 새 DB"
// + "머리말 길이 이하라면 만들거나 비우는 도중의 정전이다"
#[test]
fn a_torn_header_with_no_data_file_is_a_new_database() {
    for len in [0, 5, 19, 20] {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new(DIR)).unwrap();
        let mut torn = wal_header(0);
        torn[3] ^= 0x55;
        replace(&fs, WAL_FILE, &torn[..len]);
        let db = open(&fs).unwrap_or_else(|e| panic!("len {len}: {e}"));
        assert_eq!(get(&db, "x"), None);
        put(&db, "x", b"1");
        drop(db);
        assert_eq!(get(&open(&fs).unwrap(), "x"), Some(b"1".to_vec()));
    }
}

// 3절 "새 DB (WAL이 머리말 + 첫 커밋보다 길면 거절)"
#[test]
fn a_long_wal_with_no_commit_and_no_data_file_is_refused() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).unwrap();
    let mut wal = wal_header(0);
    wal.extend(std::iter::repeat_n(0u8, 3 * PAGE));
    replace(&fs, WAL_FILE, &wal);
    assert!(is_corrupt(open(&fs)));
}

// 3절 "데이터 파일이 표지 s의 page_count보다 짧으면 거절"
#[test]
fn a_data_file_shorter_than_the_slot_page_count_is_refused() {
    let fs = SimFs::new();
    let db = populated(&fs);
    let pages = db.page_count();
    db.close().unwrap();
    let file = fs.open(&path(DATA_FILE)).unwrap();
    file.set_len((pages - 1) * PAGE as u64).unwrap();
    file.sync_data().unwrap();
    drop(file);
    assert!(is_corrupt(open(&fs)));
}

// ---------------------------------------------- power cuts in a checkpoint (1절)

/// Counts the operations that change the disk in one checkpoint run.
fn checkpoint_ops(setup: &dyn Fn(&SimFs) -> Db<SimFs>) -> u64 {
    let fs = SimFs::new();
    let db = setup(&fs);
    let before = fs.ops();
    db.checkpoint().unwrap();
    fs.ops() - before
}

fn cut_every_point(setup: &dyn Fn(&SimFs) -> Db<SimFs>, check: &dyn Fn(&Db<SimFs>)) {
    let total = checkpoint_ops(setup);
    assert!(total > 0);
    for n in 0..=total {
        for seed in 0..6 {
            let fs = SimFs::new();
            let db = setup(&fs);
            fs.fail_after(n);
            let result = db.checkpoint();
            assert_eq!(result.is_ok(), n >= total, "n {n}");
            drop(db);
            fs.crash(seed);
            let db = open(&fs).unwrap_or_else(|e| panic!("n {n} seed {seed}: {e}"));
            check(&db);
            // The engine goes on: a commit, a checkpoint, a clean reopen.
            put(&db, "later", b"L");
            db.close()
                .unwrap_or_else(|e| panic!("n {n} seed {seed}: close {e}"));
            let db = open(&fs).unwrap_or_else(|e| panic!("n {n} seed {seed}: reopen {e}"));
            check(&db);
            assert_eq!(get(&db, "later"), Some(b"L".to_vec()));
        }
    }
}

// 완료 조건 2 "체크포인트 도중 정전이 나도 복구된다"; 7절 "정리 도중 모든
// 시점에서 정전(+시드 여러 개) 뒤 약속 지킴" — the first checkpoint.
#[test]
fn a_power_cut_anywhere_in_the_first_checkpoint_loses_nothing() {
    cut_every_point(&|fs| populated(fs), &|db| check_populated(db));
}

// Same, for a later checkpoint n → n+1 that overwrites pages in place.
#[test]
fn a_power_cut_anywhere_in_a_later_checkpoint_loses_nothing() {
    let setup = |fs: &SimFs| {
        let db = populated(fs);
        db.checkpoint().unwrap();
        db.checkpoint().unwrap(); // no-op
        put(&db, "k", b"v1");
        db.checkpoint().unwrap(); // 2 → A
        for i in 0..150 {
            if i % 3 == 0 {
                del(&db, &key(i));
            } else if i % 3 == 1 {
                put(&db, &key(i), &value(i + 1000, 120));
            }
        }
        put(&db, "k", b"v2");
        db
    };
    let check = |db: &Db<SimFs>| {
        for i in 0..150 {
            let want = match i % 3 {
                0 => None,
                1 => Some(value(i + 1000, 120)),
                _ => Some(value(i, 120)),
            };
            assert_eq!(get(db, &key(i)), want, "key {i}");
        }
        assert_eq!(get(db, "k"), Some(b"v2".to_vec()));
        assert_eq!(get(db, "big"), Some(value(9999, 10_000)));
    };
    cut_every_point(&setup, &check);
}

// 3절 "검사 끝. 이제 바꾸기: ... WAL 새로 쓰기" — a power cut while opening
// rewrites an old WAL (1절 "② 뒤 ③ 전") must not lose anything either.
#[test]
fn a_power_cut_while_opening_rewrites_an_old_wal_loses_nothing() {
    let build = || {
        let fs = SimFs::new();
        let db = populated(&fs);
        put(&db, "k", b"old");
        let old_wal = read(&fs, WAL_FILE);
        put(&db, "k", b"new");
        db.close().unwrap();
        replace(&fs, WAL_FILE, &old_wal);
        fs
    };
    let total = {
        let fs = build();
        let before = fs.ops();
        drop(open(&fs).unwrap());
        fs.ops() - before
    };
    for n in 0..=total {
        for seed in 0..4 {
            let fs = build();
            fs.fail_after(n);
            let _ = open(&fs).map(drop);
            fs.crash(seed);
            let db = open(&fs).unwrap_or_else(|e| panic!("n {n} seed {seed}: {e}"));
            assert_eq!(get(&db, "k"), Some(b"new".to_vec()), "n {n} seed {seed}");
            check_populated(&db);
        }
    }
}

// ------------------------------------------------------ the 4 MiB bound (4절)

// 결정 1 "커밋 뒤 WAL이 4MiB를 넘으면 그 커밋이 정리까지 하고 돌아온다";
// 완료 조건 4 "재시작 시간이 WAL 길이에 비례해 제한된다"
#[test]
fn the_wal_never_stays_longer_than_checkpoint_after() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    let mut checkpointed = false;
    let mut longest = 0;
    for i in 0..1500 {
        put(&db, &key(i % 400), &value(i, 900));
        let len = read(&fs, WAL_FILE).len() as u64;
        assert!(len <= CHECKPOINT_AFTER, "commit {i}: WAL is {len} bytes");
        longest = longest.max(len);
        if len == HEADER as u64 {
            checkpointed = true;
        }
    }
    assert!(checkpointed, "no commit checkpointed");
    assert!(longest > CHECKPOINT_AFTER / 2, "test never filled the WAL");
    let data = read(&fs, DATA_FILE);
    assert!(slot_checkpoint(&data, 0).max(slot_checkpoint(&data, 1)) >= 1);
    drop(db);
    let db = open(&fs).unwrap();
    for i in 1100..1500 {
        assert_eq!(get(&db, &key(i % 400)), Some(value(i, 900)));
    }
}

// 4절 "10MB 값 같은 커밋 하나가 그보다 크면 그만큼 길어질 수 있다" — and the
// commit that made it longer checkpoints before returning.
#[test]
fn a_single_commit_bigger_than_the_bound_checkpoints_at_once() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    let big = value(1, 5 * 1024 * 1024);
    put(&db, "huge", &big);
    assert_eq!(read(&fs, WAL_FILE).len(), HEADER);
    assert_eq!(
        read(&fs, DATA_FILE).len() as u64,
        db.page_count() * PAGE as u64
    );
    drop(db);
    fs.crash(3);
    assert_eq!(get(&open(&fs).unwrap(), "huge"), Some(big));
}

// ------------------------------------------- deleted values leave the disk (06)

// 6절 "지운 값이 WAL에 남음 → 정리가 WAL을 비우면 사라진다. 테스트가 WAL과
// 데이터 파일 둘 다 확인한다." (06 guarantee covers freed pages, i.e. overflow
// values; 0006 설계: "리프 안에서 지운 작은 값은 여전히 남는다".)
#[test]
fn a_deleted_overflow_value_leaves_neither_file_after_a_checkpoint() {
    let secret = b"SECRET-OVERFLOW-VALUE-7f3a9";
    let big: Vec<u8> = secret.iter().copied().cycle().take(20_000).collect();
    for checkpoint_between in [false, true] {
        let fs = SimFs::new();
        let db = populated(&fs);
        put(&db, "secret", &big);
        if checkpoint_between {
            db.checkpoint().unwrap();
            assert!(contains(&read(&fs, DATA_FILE), secret));
        }
        del(&db, "secret");
        db.checkpoint().unwrap();
        assert!(
            !contains(&read(&fs, DATA_FILE), secret),
            "data, {checkpoint_between}"
        );
        assert!(
            !contains(&read(&fs, WAL_FILE), secret),
            "WAL, {checkpoint_between}"
        );
        check_populated(&db);
    }
}

// ----------------------------------- a failed checkpoint stops the engine (결정 7)

/// A file system that fails one chosen `sync_data` call with the power on.
#[derive(Clone)]
struct FailSync {
    inner: SimFs,
    /// Sync calls to let through before failing one; `None` = never fail.
    left: Arc<Mutex<Option<u64>>>,
}

impl FailSync {
    fn new(inner: SimFs) -> Self {
        FailSync {
            inner,
            left: Arc::new(Mutex::new(None)),
        }
    }
    fn fail_sync_after(&self, n: u64) {
        *self.left.lock().unwrap() = Some(n);
    }
}

struct FailSyncFile {
    inner: SimFile,
    left: Arc<Mutex<Option<u64>>>,
}

impl Fs for FailSync {
    type File = FailSyncFile;
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn open(&self, path: &Path) -> io::Result<FailSyncFile> {
        Ok(FailSyncFile {
            inner: self.inner.open(path)?,
            left: Arc::clone(&self.left),
        })
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.inner.sync_dir(dir)
    }
}

impl FsFile for FailSyncFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.inner.write_all_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
    fn try_lock(&self) -> io::Result<()> {
        self.inner.try_lock()
    }
    fn sync_data(&self) -> io::Result<()> {
        let mut left = self.left.lock().unwrap();
        match *left {
            Some(0) => {
                *left = None;
                Err(io::Error::other("injected sync failure"))
            }
            Some(n) => {
                *left = Some(n - 1);
                drop(left);
                self.inner.sync_data()
            }
            None => {
                drop(left);
                self.inner.sync_data()
            }
        }
    }
}

fn open_failing(fs: &FailSync) -> Db<FailSync> {
    Db::open_with(fs.clone(), DIR).unwrap()
}

// 결정 7 "정리가 실패하면 엔진이 멈춘다. 그 뒤 커밋과 정리는 모두 오류를 낸다.
// 다시 열면 된다"; Db::checkpoint doc "Every commit acknowledged before is
// still there then."
#[test]
fn a_failed_checkpoint_stops_the_engine_until_reopened() {
    // Try failing each of the first few syncs of the checkpoint.
    for (prior, which) in (0..2).flat_map(|p| (0..4).map(move |w| (p == 1, w))) {
        let which: u64 = which;
        let sim = SimFs::new();
        let fs = FailSync::new(sim.clone());
        let db = open_failing(&fs);
        for i in 0..50 {
            put(&db, &key(i), &value(i, 120));
        }
        if prior {
            db.checkpoint().unwrap();
            put(&db, "second", b"2");
        }
        fs.fail_sync_after(which);
        if db.checkpoint().is_ok() {
            continue; // this checkpoint had fewer syncs
        }
        let mut txn = db.begin();
        txn.put(b"after-stop", b"x").unwrap();
        assert!(
            txn.commit().is_err(),
            "which {which}: commit after a failed checkpoint"
        );
        assert!(
            db.checkpoint().is_err(),
            "which {which}: checkpoint after a failed one"
        );
        let mut txn = db.begin();
        txn.put(b"after-stop-2", b"x").unwrap();
        assert!(
            txn.commit().is_err(),
            "which {which}: second commit after stop"
        );
        drop(db);
        sim.crash(which);
        let db = open_failing(&fs);
        for i in 0..50 {
            assert_eq!(
                get(&db, &key(i)),
                Some(value(i, 120)),
                "which {which} key {i}"
            );
        }
        if prior {
            assert_eq!(get(&db, "second"), Some(b"2".to_vec()));
        }
        put(&db, "works", b"again");
        db.checkpoint().unwrap();
    }
}

// Txn::commit doc: "A commit that leaves the WAL longer than CHECKPOINT_AFTER
// checkpoints; if that fails, the commit is durable and visible but Err is
// returned, and the engine stops"; 4절 same.
#[test]
fn a_commit_whose_checkpoint_fails_is_durable_visible_and_stops_the_engine() {
    let sim = SimFs::new();
    let fs = FailSync::new(sim.clone());
    let db = open_failing(&fs);
    put(&db, "small", b"1");
    let big = value(7, (CHECKPOINT_AFTER + 1000) as usize);
    // Let the commit's own WAL sync through, fail the checkpoint's first.
    fs.fail_sync_after(1);
    let mut txn = db.begin();
    txn.put(b"big", &big).unwrap();
    assert!(txn.commit().is_err());
    assert_eq!(get(&db, "big").as_deref(), Some(&big[..]), "not visible");
    let mut txn = db.begin();
    txn.put(b"next", b"x").unwrap();
    assert!(txn.commit().is_err());
    drop(db);
    sim.crash(5);
    let db = open_failing(&fs);
    assert_eq!(get(&db, "big").as_deref(), Some(&big[..]), "not durable");
    assert_eq!(get(&db, "small"), Some(b"1".to_vec()));
}

// 5절 "Db::close() 이제 정리를 한다. 실패하면 Err"
#[test]
fn close_reports_a_failed_checkpoint() {
    let sim = SimFs::new();
    let fs = FailSync::new(sim.clone());
    let db = open_failing(&fs);
    put(&db, "a", b"1");
    fs.fail_sync_after(0);
    assert!(db.close().is_err());
    sim.crash(2);
    assert_eq!(get(&open_failing(&fs), "a"), Some(b"1".to_vec()));
}
