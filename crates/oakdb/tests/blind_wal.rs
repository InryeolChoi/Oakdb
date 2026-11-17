//! Blind tests for the WAL (issue #22), written from the issue, ADR 0007 and
//! `docs/design/0010-wal.md` alone, without reading the implementation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, DATA_FILE, WAL_FILE};

const DIR: &str = "/oak";
/// Design 1: kind (1) + page number (8) + page (4096) + crc (4).
const PAGE_REC: u64 = 4_109;
/// Design 1: kind (1) + root, page_count, free_head, free_count (4 x 8) + crc (4).
const COMMIT_REC: u64 = 37;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn wal_path() -> PathBuf {
    Path::new(DIR).join(WAL_FILE)
}

fn data_path() -> PathBuf {
    Path::new(DIR).join(DATA_FILE)
}

fn open(fs: &SimFs) -> Db<SimFs> {
    Db::open_with(fs.clone(), DIR).expect("open")
}

fn file_size(fs: &SimFs, path: &Path) -> u64 {
    fs.open(path).unwrap().size().unwrap()
}

fn wal_size(fs: &SimFs) -> u64 {
    file_size(fs, &wal_path())
}

fn read_wal(fs: &SimFs) -> Vec<u8> {
    let f = fs.open(&wal_path()).unwrap();
    let mut buf = vec![0; f.size().unwrap() as usize];
    f.read_exact_at(&mut buf, 0).unwrap();
    buf
}

/// Replaces the WAL with `bytes`, durably.
fn write_wal(fs: &SimFs, bytes: &[u8]) {
    let f = fs.open(&wal_path()).unwrap();
    f.set_len(0).unwrap();
    f.write_all_at(bytes, 0).unwrap();
    f.sync_data().unwrap();
}

/// Crashes and makes sure no power cut is still armed afterwards.
fn crash(fs: &SimFs, seed: u64) {
    fs.crash(seed);
    fs.fail_after(1 << 40);
}

fn put(db: &Db<SimFs>, key: &[u8], value: &[u8]) {
    let mut t = db.begin();
    t.put(key, value).unwrap();
    t.commit().unwrap();
}

fn get(db: &Db<SimFs>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

fn all(db: &Db<SimFs>) -> Model {
    db.begin().scan::<&[u8]>(..).map(|e| e.unwrap()).collect()
}

/// The records a WAL of whole commits holds, as (kind, offset) pairs.
fn records(wal: &[u8]) -> Vec<(u8, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < wal.len() {
        let kind = wal[at];
        out.push((kind, at));
        at += match kind {
            1 => PAGE_REC as usize,
            2 => COMMIT_REC as usize,
            k => panic!("unknown record kind {k} at {at}"),
        };
    }
    assert_eq!(at, wal.len(), "WAL does not end on a record boundary");
    out
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// CRC-32C (Castagnoli) carried on from `crc`, as `crc32c_append` does.
fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// SplitMix64, for workloads.
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

/// One commit: `Some(value)` puts, `None` deletes.
type Commit = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn workload(seed: u64) -> Vec<Commit> {
    let mut rng = Rng(seed ^ 0xA5A5_5A5A);
    let commits = 1 + rng.below(12);
    (0..commits)
        .map(|_| {
            let ops = 1 + rng.below(8);
            (0..ops)
                .map(|_| {
                    let key = format!("k{:03}", rng.below(150)).into_bytes();
                    if rng.below(5) == 0 {
                        (key, None)
                    } else {
                        let len = if rng.below(6) == 0 {
                            1_500 + rng.below(9_000)
                        } else {
                            rng.below(400)
                        };
                        let byte = rng.next() as u8;
                        (key, Some(vec![byte; len as usize]))
                    }
                })
                .collect()
        })
        .collect()
}

fn apply(model: &mut Model, commit: &Commit) {
    for (k, v) in commit {
        match v {
            Some(v) => model.insert(k.clone(), v.clone()),
            None => model.remove(k),
        };
    }
}

fn commit(db: &Db<SimFs>, c: &Commit) -> oakdb::Result<()> {
    let mut t = db.begin();
    for (k, v) in c {
        match v {
            Some(v) => t.put(k, v)?,
            None => t.delete(k)?,
        }
    }
    t.commit()
}

/// Opens a new DB, runs `seed`'s workload with the power cut after a
/// seed-chosen number of disk changes, crashes, reopens and checks the
/// three promises. Returns an error message on a broken promise.
fn power_cut_run(seed: u64) -> Result<(), String> {
    let work = workload(seed);

    // Dry run to count the disk changes the workload makes.
    let dry = SimFs::new();
    let db = open(&dry);
    let start = dry.ops();
    for c in &work {
        commit(&db, c).unwrap();
    }
    let total = dry.ops() - start;
    drop(db);

    let fs = SimFs::new();
    let db = open(&fs);
    let mut rng = Rng(seed);
    fs.fail_after(rng.below(total + 1));
    let mut acked = Model::new();
    let mut cut = None;
    for c in &work {
        match commit(&db, c) {
            Ok(()) => apply(&mut acked, c),
            Err(_) => {
                cut = Some(c.clone());
                break;
            }
        }
    }
    drop(db);
    crash(&fs, rng.next());

    let db = Db::open_with(fs.clone(), DIR).map_err(|e| format!("reopen failed: {e:?}"))?;
    let found = all(&db);
    if found == acked {
        return Ok(());
    }
    if let Some(c) = &cut {
        let mut with_cut = acked.clone();
        apply(&mut with_cut, c);
        if found == with_cut {
            return Ok(());
        }
    }
    Err(format!(
        "state matches neither acknowledged nor acknowledged + cut commit (cut: {})",
        cut.is_some()
    ))
}

// ---------------------------------------------------------------- creating

#[test]
fn a_new_database_has_three_pages() {
    // Design 4: page_count counts free pages and the two meta pages; an
    // empty tree is one leaf.
    let fs = SimFs::new();
    let db = open(&fs);
    assert_eq!(db.page_count(), 3);
}

#[test]
fn a_new_database_is_the_first_wal_commit_and_the_data_file_stays_empty() {
    // Design decision 2.
    let fs = SimFs::new();
    let db = open(&fs);
    drop(db);
    assert_eq!(file_size(&fs, &data_path()), 0);
    let wal = read_wal(&fs);
    let recs = records(&wal);
    assert!(!recs.is_empty());
    assert_eq!(
        recs.last().unwrap().0,
        2,
        "WAL must end with a commit record"
    );
    assert_eq!(
        recs.iter().filter(|r| r.0 == 2).count(),
        1,
        "creating is exactly one commit"
    );
}

#[test]
fn creating_writes_one_page_record_for_the_empty_leaf() {
    let fs = SimFs::new();
    drop(open(&fs));
    assert_eq!(wal_size(&fs), PAGE_REC + COMMIT_REC);
}

#[test]
fn reopening_an_existing_database_does_not_add_a_commit() {
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"a", b"1");
    drop(db);
    let before = wal_size(&fs);
    let db = open(&fs);
    drop(db);
    assert_eq!(wal_size(&fs), before);
}

#[test]
fn a_power_cut_while_creating_leaves_something_that_opens_empty() {
    // Design decision 2 and experiment 0008 seeds 184 and 536.
    for cut in 0..12 {
        for seed in 0..40 {
            let fs = SimFs::new();
            fs.fail_after(cut);
            let _ = Db::open_with(fs.clone(), DIR).map(drop);
            crash(&fs, seed);
            let db = Db::open_with(fs.clone(), DIR)
                .unwrap_or_else(|e| panic!("cut {cut} seed {seed}: {e:?}"));
            assert!(all(&db).is_empty(), "cut {cut} seed {seed}");
            assert_eq!(db.page_count(), 3, "cut {cut} seed {seed}");
        }
    }
}

#[test]
fn the_wal_file_survives_a_power_cut_right_after_creating() {
    // Design 3: the directory is synced when the WAL had no commit.
    for seed in 0..200 {
        let fs = SimFs::new();
        drop(open(&fs));
        fs.crash(seed);
        assert!(fs.exists(&wal_path()), "seed {seed}");
        let db = open(&fs);
        assert!(all(&db).is_empty());
    }
}

#[test]
fn a_wal_of_zeros_with_an_empty_data_file_is_a_new_database() {
    // Design 1: START is not 0, so zeros never check out; decision 2: no
    // commit and an empty data file means a new DB.
    let fs = SimFs::new();
    drop(open(&fs));
    write_wal(&fs, &vec![0; 10_000]);
    let db = open(&fs);
    assert!(all(&db).is_empty());
    assert_eq!(db.page_count(), 3);
}

// ---------------------------------------------------------------- format

#[test]
fn each_small_commit_appends_page_records_and_one_commit_record() {
    let fs = SimFs::new();
    let db = open(&fs);
    for i in 0..20u32 {
        let before = wal_size(&fs);
        put(&db, &i.to_be_bytes(), b"value");
        let grown = wal_size(&fs) - before;
        assert!(grown >= PAGE_REC + COMMIT_REC, "commit {i} grew {grown}");
        assert_eq!(
            (grown - COMMIT_REC) % PAGE_REC,
            0,
            "commit {i} grew {grown}"
        );
    }
}

#[test]
fn a_one_leaf_commit_appends_exactly_one_page_record() {
    // While the tree is a single leaf, a small put changes only that leaf.
    let fs = SimFs::new();
    let db = open(&fs);
    let before = wal_size(&fs);
    put(&db, b"key", b"value");
    assert_eq!(wal_size(&fs) - before, PAGE_REC + COMMIT_REC);
}

#[test]
fn rollback_and_dropped_transactions_write_nothing() {
    let fs = SimFs::new();
    let db = open(&fs);
    let before = wal_size(&fs);
    let mut t = db.begin();
    t.put(b"a", b"1").unwrap();
    t.rollback();
    let mut t = db.begin();
    t.put(b"b", b"2").unwrap();
    drop(t);
    assert_eq!(wal_size(&fs), before);
}

#[test]
fn commit_records_hold_the_meta_fields() {
    // Design 1: commit record = kind 2, root, page_count, free_head,
    // free_count (u64 LE), crc. Page record = kind 1, page number, page.
    let fs = SimFs::new();
    let db = open(&fs);
    for i in 0..300u32 {
        put(&db, format!("key{i:05}").as_bytes(), &[7; 200]);
    }
    let count = db.page_count();
    drop(db);
    let wal = read_wal(&fs);
    let recs = records(&wal);
    let (kind, at) = *recs.last().unwrap();
    assert_eq!(kind, 2);
    let root = u64_at(&wal, at + 1);
    assert_eq!(u64_at(&wal, at + 9), count, "page_count field");
    let free_head = u64_at(&wal, at + 17);
    let free_count = u64_at(&wal, at + 25);
    assert!(root >= 2 && root < count, "root {root} of {count}");
    assert!(free_head < count);
    assert_eq!(free_head == 0, free_count == 0, "0 = no free list");
    for &(kind, at) in &recs {
        if kind == 1 {
            let no = u64_at(&wal, at + 1);
            assert!(no >= 2, "meta pages 0 and 1 never go to the WAL: {no}");
        }
    }
}

#[test]
fn record_crcs_are_chained() {
    // Design 1: each crc = CRC-32C carried on from the previous record's
    // crc over every byte before the crc.
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"a", b"1");
    put(&db, b"b", b"2");
    drop(db);
    let wal = read_wal(&fs);
    let recs = records(&wal);
    let len = |kind| if kind == 1 { PAGE_REC } else { COMMIT_REC } as usize;
    for w in recs.windows(2) {
        let (k0, a0) = w[0];
        let (k1, a1) = w[1];
        let prev = u32_at(&wal, a0 + len(k0) - 4);
        let body = &wal[a1..a1 + len(k1) - 4];
        assert_eq!(
            crc32c_append(prev, body),
            u32_at(&wal, a1 + len(k1) - 4),
            "record at {a1}"
        );
    }
}

// ---------------------------------------------------------------- data file

#[test]
fn commits_never_touch_the_data_file_before_checkpoints() {
    let fs = SimFs::new();
    let db = open(&fs);
    for i in 0..200u32 {
        put(&db, &i.to_be_bytes(), &[1; 300]);
    }
    db.close().unwrap();
    assert_eq!(file_size(&fs, &data_path()), 0);
    let db = open(&fs);
    assert_eq!(all(&db).len(), 200);
    drop(db);
    fs.crash(3);
    assert_eq!(file_size(&fs, &data_path()), 0);
    assert_eq!(all(&open(&fs)).len(), 200);
}

// ---------------------------------------------------------------- durability

#[test]
fn a_commit_is_durable_when_it_returns_whatever_the_crash() {
    for seed in 0..100 {
        let fs = SimFs::new();
        let db = open(&fs);
        let mut model = Model::new();
        let n = 1 + seed % 15;
        for i in 0..n {
            let k = format!("k{i}").into_bytes();
            let v = vec![i as u8; (i * 97 % 3000) as usize];
            put(&db, &k, &v);
            model.insert(k, v);
        }
        drop(db);
        fs.crash(seed);
        assert_eq!(all(&open(&fs)), model, "seed {seed}");
    }
}

#[test]
fn every_commit_survives_a_crash_right_after_it() {
    let fs = SimFs::new();
    let mut model = Model::new();
    for i in 0..40u64 {
        let db = open(&fs);
        let k = format!("key{}", i * 7 % 23).into_bytes();
        let v = vec![i as u8; (i * 131 % 2000) as usize];
        put(&db, &k, &v);
        model.insert(k, v);
        drop(db);
        fs.crash(i * 31 + 5);
        let db = open(&fs);
        assert_eq!(all(&db), model, "after commit {i}");
    }
}

#[test]
fn reopening_replays_every_commit_in_order() {
    let fs = SimFs::new();
    let db = open(&fs);
    let mut model = Model::new();
    for i in 0..500u32 {
        let k = format!("{:04}", i % 120).into_bytes();
        if i % 7 == 3 {
            let mut t = db.begin();
            t.delete(&k).unwrap();
            t.commit().unwrap();
            model.remove(&k);
        } else {
            let v = format!("v{i}").repeat((i % 40) as usize + 1).into_bytes();
            put(&db, &k, &v);
            model.insert(k, v);
        }
    }
    let count = db.page_count();
    drop(db);
    let db = open(&fs);
    assert_eq!(all(&db), model);
    assert_eq!(db.page_count(), count);
}

// ---------------------------------------------------------------- replay

/// Opens a DB, commits "a", "b", "c" one per commit, and returns the WAL
/// size after each commit (index 0 = after creating).
fn three_commits(fs: &SimFs) -> Vec<u64> {
    let db = open(fs);
    let mut ends = vec![wal_size(fs)];
    for k in [b"a", b"b", b"c"] {
        put(&db, k, b"v");
        ends.push(wal_size(fs));
    }
    ends
}

fn keys(db: &Db<SimFs>) -> Vec<Vec<u8>> {
    all(db).into_keys().collect()
}

#[test]
fn replay_stops_at_the_first_damaged_record() {
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let mut wal = read_wal(&fs);
    // A byte inside the page record of commit "b".
    wal[ends[1] as usize + 100] ^= 0xFF;
    write_wal(&fs, &wal);
    let db = open(&fs);
    assert_eq!(keys(&db), vec![b"a".to_vec()], "c follows the damage");
}

#[test]
fn a_damaged_commit_record_drops_its_commit_and_all_after() {
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let mut wal = read_wal(&fs);
    wal[ends[2] as usize - 10] ^= 0x01; // inside commit "b"'s commit record
    write_wal(&fs, &wal);
    assert_eq!(keys(&open(&fs)), vec![b"a".to_vec()]);
}

#[test]
fn page_records_without_a_commit_record_are_not_applied() {
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let wal = read_wal(&fs);
    // Keep commit "c"'s page records but not its commit record.
    write_wal(&fs, &wal[..(ends[3] - COMMIT_REC) as usize]);
    let db = open(&fs);
    assert_eq!(keys(&db), vec![b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn a_commit_record_cut_short_is_ignored() {
    for missing in 1..COMMIT_REC {
        let fs = SimFs::new();
        let ends = three_commits(&fs);
        let wal = read_wal(&fs);
        write_wal(&fs, &wal[..(ends[3] - missing) as usize]);
        let db = open(&fs);
        assert_eq!(
            keys(&db),
            vec![b"a".to_vec(), b"b".to_vec()],
            "missing {missing}"
        );
    }
}

#[test]
fn a_record_out_of_its_chain_is_refused() {
    // Design 1 "why a chain": commit "c" spliced right after "a" must not
    // replay, since its crcs carry on from "b".
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let wal = read_wal(&fs);
    let mut spliced = wal[..ends[1] as usize].to_vec();
    spliced.extend_from_slice(&wal[ends[2] as usize..]);
    write_wal(&fs, &spliced);
    assert_eq!(keys(&open(&fs)), vec![b"a".to_vec()]);
}

#[test]
fn an_unknown_record_kind_stops_replay() {
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let mut wal = read_wal(&fs);
    wal[ends[2] as usize] = 9; // first byte of commit "c"
    write_wal(&fs, &wal);
    assert_eq!(keys(&open(&fs)), vec![b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn garbage_after_the_last_commit_is_cut_off_on_opening() {
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let mut wal = read_wal(&fs);
    let mut rng = Rng(1);
    wal.extend((0..5_000).map(|_| rng.next() as u8));
    write_wal(&fs, &wal);
    let db = open(&fs);
    assert_eq!(keys(&db).len(), 3);
    drop(db);
    assert_eq!(wal_size(&fs), ends[3], "the tail is cut to the last commit");
}

#[test]
fn commits_after_a_cut_tail_survive_reopening() {
    // Design 3: without cutting, the next commit would sit after the tail
    // and replay would stop before it.
    let fs = SimFs::new();
    let ends = three_commits(&fs);
    let wal = read_wal(&fs);
    // Leave "c"'s page records as a tail without a commit record.
    write_wal(&fs, &wal[..(ends[3] - COMMIT_REC) as usize]);
    let db = open(&fs);
    put(&db, b"d", b"v");
    drop(db);
    let db = open(&fs);
    assert_eq!(keys(&db), vec![b"a".to_vec(), b"b".to_vec(), b"d".to_vec()]);
}

#[test]
fn commits_after_a_cut_tail_survive_a_power_cut() {
    for seed in 0..100 {
        let fs = SimFs::new();
        let ends = three_commits(&fs);
        let mut wal = read_wal(&fs)[..ends[2] as usize].to_vec();
        wal.extend(std::iter::repeat_n(0xAB, 9_000));
        write_wal(&fs, &wal);
        let db = open(&fs);
        put(&db, b"d", b"v");
        drop(db);
        fs.crash(seed);
        let db = open(&fs);
        assert_eq!(
            keys(&db),
            vec![b"a".to_vec(), b"b".to_vec(), b"d".to_vec()],
            "seed {seed}"
        );
    }
}

// ---------------------------------------------------------------- page_count

#[test]
fn page_count_grows_with_the_tree_and_keeps_freed_pages() {
    let fs = SimFs::new();
    let db = open(&fs);
    for i in 0..400u32 {
        put(&db, format!("key{i:05}").as_bytes(), &[3; 300]);
    }
    let grown = db.page_count();
    assert!(grown > 3);
    for i in 0..400u32 {
        let mut t = db.begin();
        t.delete(format!("key{i:05}").as_bytes()).unwrap();
        t.commit().unwrap();
    }
    assert!(all(&db).is_empty());
    assert!(db.page_count() >= 3);
    // Free pages are counted, and nothing shrinks the file before #23.
    assert_eq!(db.page_count(), grown);
    drop(db);
    assert_eq!(open(&fs).page_count(), grown);
}

// ---------------------------------------------------------------- power cuts

#[test]
fn power_cuts_keep_every_promise_over_many_seeds() {
    // Issue: passes over thousands of seeds; experiment 0008's three
    // promises: reopens, acknowledged commits all there, cut commit
    // all-or-nothing.
    let failures: Vec<String> = (0..2_000)
        .filter_map(|s| power_cut_run(s).err().map(|e| format!("seed {s}: {e}")))
        .collect();
    assert!(
        failures.is_empty(),
        "{} failures, first: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
}

#[test]
fn a_second_power_cut_while_reopening_loses_nothing() {
    // Opening may cut the tail and sync; a cut during that must not lose
    // acknowledged commits either.
    for seed in 0..300u64 {
        let fs = SimFs::new();
        let db = open(&fs);
        let mut model = Model::new();
        for i in 0..5u64 {
            let k = format!("k{i}").into_bytes();
            let v = vec![i as u8; 700];
            put(&db, &k, &v);
            model.insert(k, v);
        }
        // An unacknowledged commit cut midway.
        fs.fail_after(1 + seed % 3);
        let mut t = db.begin();
        t.put(b"zz", &[9; 5000]).unwrap();
        let cut_ok = t.commit().is_ok();
        drop(db);
        crash(&fs, seed);
        fs.fail_after(seed % 4);
        let _ = Db::open_with(fs.clone(), DIR).map(drop);
        crash(&fs, seed + 1000);
        let found = all(&open(&fs));
        let mut with_cut = model.clone();
        with_cut.insert(b"zz".to_vec(), vec![9; 5000]);
        assert!(
            found == model || found == with_cut,
            "seed {seed} (cut commit ok: {cut_ok})"
        );
        if cut_ok {
            assert_eq!(found, with_cut, "seed {seed}");
        }
    }
}

#[test]
fn values_read_after_a_crash_are_never_ones_never_committed() {
    let fs = SimFs::new();
    let db = open(&fs);
    put(&db, b"k", b"old");
    fs.fail_after(0);
    let mut t = db.begin();
    t.put(b"k", b"new").unwrap();
    assert!(t.commit().is_err(), "the power is gone at the first write");
    drop(db);
    crash(&fs, 11);
    assert_eq!(get(&open(&fs), b"k"), Some(b"old".to_vec()));
}
