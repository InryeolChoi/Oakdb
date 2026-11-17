//! The write-ahead log (issue #22, ADR 0007): a commit is on disk in the
//! WAL before it is acknowledged, and reopening replays every whole commit
//! and nothing else.

use std::path::{Path, PathBuf};

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, DATA_FILE, WAL_FILE};

const SEEDS: u64 = 200;

fn dir() -> PathBuf {
    PathBuf::from("/oak")
}

fn put(db: &Db<SimFs>, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn get(db: &Db<SimFs>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

fn size(fs: &SimFs, path: &Path) -> u64 {
    fs.open(path).unwrap().size().unwrap()
}

fn wal_size(fs: &SimFs) -> u64 {
    size(fs, &dir().join(WAL_FILE))
}

#[test]
fn a_new_database_is_the_first_commit_of_its_wal() {
    let fs = SimFs::new();
    Db::open_with(fs.clone(), dir()).unwrap();

    assert!(wal_size(&fs) > 0);
    assert_eq!(size(&fs, &dir().join(DATA_FILE)), 0);
}

#[test]
fn commits_go_to_the_wal_and_leave_the_data_file_alone() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let before = wal_size(&fs);

    for i in 0u32..300 {
        put(&db, &i.to_be_bytes(), &[7; 100]);
    }

    assert!(wal_size(&fs) > before);
    assert_eq!(size(&fs, &dir().join(DATA_FILE)), 0);
}

#[test]
fn an_acknowledged_commit_survives_any_crash() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        put(&db, b"oak", b"tree");
        drop(db);

        fs.crash(seed);

        let db = Db::open_with(fs.clone(), dir()).unwrap();
        assert_eq!(get(&db, b"oak"), Some(b"tree".to_vec()), "seed {seed}");
    }
}

#[test]
fn reopening_replays_every_commit() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    for i in 0u32..300 {
        put(&db, &i.to_be_bytes(), &i.to_le_bytes());
    }
    put(&db, b"long", &vec![3; 20_000]);
    let pages = db.page_count();
    drop(db);

    let db = Db::open_with(fs.clone(), dir()).unwrap();

    assert_eq!(db.page_count(), pages);
    for i in 0u32..300 {
        assert_eq!(get(&db, &i.to_be_bytes()), Some(i.to_le_bytes().to_vec()));
    }
    assert_eq!(get(&db, b"long"), Some(vec![3; 20_000]));
}

/// Writes `bytes` into the WAL at `offset` and syncs, as a torn or stale
/// write could have left them.
fn scribble(fs: &SimFs, offset: u64, bytes: &[u8]) {
    let wal = fs.open(&dir().join(WAL_FILE)).unwrap();
    wal.write_all_at(bytes, offset).unwrap();
    wal.sync_data().unwrap();
}

#[test]
fn replay_stops_at_the_first_damaged_record() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    let after_a = wal_size(&fs);
    put(&db, b"b", b"2");
    put(&db, b"c", b"3");
    drop(db);

    // One byte inside the commit of "b".
    scribble(&fs, after_a + 20, &[0xFF]);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), None);
    assert_eq!(get(&db, b"c"), None, "replay went past the damage");
}

#[test]
fn a_commit_cut_before_its_commit_record_is_not_applied() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    put(&db, b"b", b"2");
    drop(db);
    let wal = fs.open(&dir().join(WAL_FILE)).unwrap();
    wal.set_len(wal_size(&fs) - 1).unwrap();
    wal.sync_data().unwrap();

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), None);
}

#[test]
fn garbage_after_the_last_commit_is_cut_off_so_later_commits_count() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    drop(db);
    let end = wal_size(&fs);
    // Half a record that never got its commit, as a power cut leaves it.
    scribble(&fs, end, &[0xAB; 5000]);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_eq!(wal_size(&fs), end, "the garbage is cut off on opening");
    put(&db, b"b", b"2");
    drop(db);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_eq!(get(&db, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"b"), Some(b"2".to_vec()));
}

#[test]
fn cutting_off_the_garbage_survives_a_crash() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        put(&db, b"a", b"1");
        drop(db);
        scribble(&fs, wal_size(&fs), &[0xAB; 5000]);
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        put(&db, b"b", b"2");
        drop(db);

        fs.crash(seed);

        let db = Db::open_with(fs.clone(), dir()).unwrap();
        assert_eq!(get(&db, b"a"), Some(b"1".to_vec()), "seed {seed}");
        assert_eq!(get(&db, b"b"), Some(b"2".to_vec()), "seed {seed}");
    }
}

#[test]
fn a_power_cut_while_creating_leaves_a_database_that_opens() {
    // Experiment 0008, seeds 184 and 536: creating never answered, so a
    // cut part way may leave nothing, but must leave something that opens.
    for cut in 0..8 {
        for seed in 0..20 {
            let fs = SimFs::new();
            fs.fail_after(cut);
            let _ = Db::open_with(fs.clone(), dir());
            fs.crash(seed);

            let db = Db::open_with(fs.clone(), dir())
                .unwrap_or_else(|e| panic!("cut {cut}, seed {seed}: {e}"));
            put(&db, b"oak", b"tree");
            assert_eq!(get(&db, b"oak"), Some(b"tree".to_vec()));
        }
    }
}

// Decisions after the review of PR #54.

/// Where opening keeps what it cuts off the WAL.
const CUT_FILE: &str = "oakdb.wal.cut";

/// The bytes a new database's first commit takes: one page record for its
/// empty leaf and a commit record.
const CREATE_LEN: u64 = 4109 + 37;

fn contents(fs: &SimFs, path: &Path) -> Vec<u8> {
    let file = fs.open(path).unwrap();
    let mut buf = vec![0; file.size().unwrap() as usize];
    file.read_exact_at(&mut buf, 0).unwrap();
    buf
}

#[test]
fn what_is_cut_off_the_wal_is_kept_in_the_cut_file() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    let after_a = wal_size(&fs);
    put(&db, b"b", b"2");
    drop(db);
    // Damage in the middle: the commit of "b", acknowledged, is after it.
    scribble(&fs, after_a + 20, &[0xFF]);
    let tail = contents(&fs, &dir().join(WAL_FILE))[after_a as usize..].to_vec();

    let db = Db::open_with(fs.clone(), dir()).unwrap();

    assert_eq!(get(&db, b"b"), None);
    assert_eq!(wal_size(&fs), after_a);
    assert_eq!(contents(&fs, &dir().join(CUT_FILE)), tail);
}

#[test]
fn the_cut_file_survives_a_crash() {
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        put(&db, b"a", b"1");
        drop(db);
        let end = wal_size(&fs);
        scribble(&fs, end, &[0xAB; 100]);
        drop(Db::open_with(fs.clone(), dir()).unwrap());

        fs.crash(seed);

        assert_eq!(
            contents(&fs, &dir().join(CUT_FILE)),
            [0xAB; 100],
            "seed {seed}"
        );
    }
}

#[test]
fn a_wal_longer_than_a_new_database_with_no_whole_commit_is_refused() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    put(&db, b"b", b"2");
    drop(db);
    // The first record damaged: no commit checks out any more.
    scribble(&fs, 20, &[0xFF]);
    let before = contents(&fs, &dir().join(WAL_FILE));

    let err = Db::open_with(fs.clone(), dir()).unwrap_err();

    assert!(matches!(err, oakdb::Error::Corrupt(_)), "{err:?}");
    assert_eq!(contents(&fs, &dir().join(WAL_FILE)), before, "nothing cut");
}

#[test]
fn a_wal_cut_short_while_creating_still_makes_a_new_database() {
    for len in [1, 100, CREATE_LEN - 1] {
        let fs = SimFs::new();
        drop(Db::open_with(fs.clone(), dir()).unwrap());
        let wal = fs.open(&dir().join(WAL_FILE)).unwrap();
        wal.set_len(len).unwrap();
        wal.sync_data().unwrap();

        let db = Db::open_with(fs.clone(), dir()).unwrap();
        assert_eq!(db.page_count(), 3, "len {len}");
    }
}

#[test]
fn a_data_file_that_is_refused_leaves_the_wal_as_it_was() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    drop(db);
    scribble(&fs, wal_size(&fs), &[0xAB; 100]);
    let data = fs.open(&dir().join(DATA_FILE)).unwrap();
    data.write_all_at(&[0xCD; 8192], 0).unwrap();
    data.sync_data().unwrap();
    let before = contents(&fs, &dir().join(WAL_FILE));

    assert!(Db::open_with(fs.clone(), dir()).is_err());
    assert_eq!(contents(&fs, &dir().join(WAL_FILE)), before);
    assert!(!fs.exists(&dir().join(CUT_FILE)));
}

#[test]
fn a_commit_that_changes_nothing_writes_nothing() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    let before = wal_size(&fs);

    let mut txn = db.begin();
    txn.delete(b"not there").unwrap();
    txn.commit().unwrap();

    assert_eq!(wal_size(&fs), before);
}
