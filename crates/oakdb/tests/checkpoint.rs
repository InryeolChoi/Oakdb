//! Checkpoints (issue #23, design 0011): the pages the WAL holds go to the
//! data file, a new meta page goes to the older of the two meta slots, and
//! the WAL starts again under a header naming the new checkpoint.

use std::collections::BTreeMap;
use std::path::PathBuf;

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error, CHECKPOINT_AFTER, DATA_FILE, WAL_FILE};

const PAGE: u64 = 4096;
/// Bytes of the WAL header: magic, checkpoint number, crc.
const WAL_HEADER: u64 = 20;
/// Where a meta page keeps the checkpoint number.
const CHECKPOINT_AT: u64 = 52;

fn dir() -> PathBuf {
    PathBuf::from("/oak")
}

fn put<F: Fs>(db: &Db<F>, key: &[u8], value: &[u8]) {
    let mut txn = db.begin();
    txn.put(key, value).unwrap();
    txn.commit().unwrap();
}

fn delete<F: Fs>(db: &Db<F>, key: &[u8]) {
    let mut txn = db.begin();
    txn.delete(key).unwrap();
    txn.commit().unwrap();
}

fn get<F: Fs>(db: &Db<F>, key: &[u8]) -> Option<Vec<u8>> {
    db.begin().get(key).unwrap()
}

fn size(fs: &SimFs, name: &str) -> u64 {
    fs.open(&dir().join(name)).unwrap().size().unwrap()
}

fn contents(fs: &SimFs, name: &str) -> Vec<u8> {
    let file = fs.open(&dir().join(name)).unwrap();
    let mut buf = vec![0; file.size().unwrap() as usize];
    file.read_exact_at(&mut buf, 0).unwrap();
    buf
}

fn overwrite(fs: &SimFs, name: &str, bytes: &[u8]) {
    let file = fs.open(&dir().join(name)).unwrap();
    file.set_len(0).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    file.sync_data().unwrap();
}

/// Flips one bit in the middle of meta slot `slot` (0 = A, 1 = B).
fn damage_slot(fs: &SimFs, slot: u64) {
    let file = fs.open(&dir().join(DATA_FILE)).unwrap();
    let at = slot * PAGE + PAGE / 2;
    let mut byte = [0];
    file.read_exact_at(&mut byte, at).unwrap();
    file.write_all_at(&[byte[0] ^ 0x10], at).unwrap();
    file.sync_data().unwrap();
}

/// The checkpoint number meta slot `slot` holds.
fn slot_checkpoint(fs: &SimFs, slot: u64) -> u64 {
    let file = fs.open(&dir().join(DATA_FILE)).unwrap();
    let mut buf = [0; 8];
    file.read_exact_at(&mut buf, slot * PAGE + CHECKPOINT_AT)
        .unwrap();
    u64::from_le_bytes(buf)
}

/// Keys 0..n with values that differ by key.
fn fill<F: Fs>(db: &Db<F>, n: u32) -> BTreeMap<u32, Vec<u8>> {
    let mut model = BTreeMap::new();
    for i in 0..n {
        let value = vec![i as u8; 50 + (i as usize * 37) % 300];
        put(db, &i.to_be_bytes(), &value);
        model.insert(i, value);
    }
    model
}

/// Keys 0..n as [`fill`] writes them, in one commit.
fn fill_in_one<F: Fs>(db: &Db<F>, n: u32) -> BTreeMap<u32, Vec<u8>> {
    let mut model = BTreeMap::new();
    let mut txn = db.begin();
    for i in 0..n {
        let value = vec![i as u8; 50 + (i as usize * 37) % 300];
        txn.put(&i.to_be_bytes(), &value).unwrap();
        model.insert(i, value);
    }
    txn.commit().unwrap();
    model
}

fn assert_holds<F: Fs>(db: &Db<F>, model: &BTreeMap<u32, Vec<u8>>, what: &str) {
    let got: BTreeMap<u32, Vec<u8>> = db
        .begin()
        .scan::<&[u8]>(..)
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (u32::from_be_bytes(key.try_into().unwrap()), value)
        })
        .collect();
    assert_eq!(&got, model, "{what}");
}

#[test]
fn a_checkpoint_moves_the_wal_into_the_data_file() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let model = fill(&db, 300);

    db.checkpoint().unwrap();

    assert_eq!(size(&fs, WAL_FILE), WAL_HEADER, "the WAL is emptied");
    assert_eq!(size(&fs, DATA_FILE), db.page_count() * PAGE);
    drop(db);
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_holds(&db, &model, "after reopening");
}

#[test]
fn closing_checkpoints() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let model = fill(&db, 50);
    let pages = db.page_count();

    db.close().unwrap();

    assert_eq!(size(&fs, WAL_FILE), WAL_HEADER);
    assert_eq!(size(&fs, DATA_FILE), pages * PAGE);
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_holds(&db, &model, "after reopening");
}

#[test]
fn a_checkpoint_with_nothing_to_move_changes_nothing() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    fill(&db, 10);
    db.checkpoint().unwrap();
    let ops = fs.ops();

    db.checkpoint().unwrap();

    assert_eq!(fs.ops(), ops);
}

#[test]
fn commits_after_a_checkpoint_go_to_the_wal_and_are_replayed() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let mut model = fill(&db, 100);
    db.checkpoint().unwrap();
    let data = contents(&fs, DATA_FILE);

    put(&db, &5u32.to_be_bytes(), b"changed");
    delete(&db, &6u32.to_be_bytes());
    model.insert(5, b"changed".to_vec());
    model.remove(&6);

    assert_eq!(
        contents(&fs, DATA_FILE),
        data,
        "commits leave the data file alone"
    );
    assert!(size(&fs, WAL_FILE) > WAL_HEADER);
    drop(db);
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_holds(&db, &model, "after reopening");
}

#[test]
fn checkpoints_alternate_between_the_meta_slots() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    fill(&db, 10);

    db.checkpoint().unwrap();
    // The first checkpoint fills both slots.
    assert_eq!((slot_checkpoint(&fs, 0), slot_checkpoint(&fs, 1)), (1, 1));

    put(&db, b"a", b"1");
    db.checkpoint().unwrap();
    assert_eq!((slot_checkpoint(&fs, 0), slot_checkpoint(&fs, 1)), (2, 1));

    put(&db, b"b", b"2");
    db.checkpoint().unwrap();
    assert_eq!((slot_checkpoint(&fs, 0), slot_checkpoint(&fs, 1)), (2, 3));
}

#[test]
fn either_meta_slot_can_be_damaged_after_the_first_checkpoint() {
    for slot in [0, 1] {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        let model = fill(&db, 100);
        db.close().unwrap();

        damage_slot(&fs, slot);

        let db =
            Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("slot {slot} damaged: {e}"));
        assert_holds(&db, &model, &format!("slot {slot} damaged"));
    }
}

#[test]
fn damage_to_the_older_slot_does_not_stop_opening() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let mut model = fill(&db, 100);
    db.checkpoint().unwrap();
    put(&db, b"\0\0\0\x07", b"new");
    model.insert(7, b"new".to_vec());
    db.close().unwrap();
    // Slot A holds checkpoint 2, slot B checkpoint 1.

    damage_slot(&fs, 1);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_holds(&db, &model, "older slot damaged");
}

#[test]
fn damage_to_the_newest_slot_once_its_wal_is_gone_is_refused() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    fill(&db, 100);
    db.checkpoint().unwrap();
    put(&db, b"\0\0\0\x07", b"new");
    db.close().unwrap();
    // Slot A holds checkpoint 2 and the WAL starts after it. Slot B, at
    // checkpoint 1, does not know of the commit the WAL of checkpoint 1
    // held, and that WAL is gone: opening with B would silently lose it.
    let wal = contents(&fs, WAL_FILE);

    damage_slot(&fs, 0);

    let err = Db::open_with(fs.clone(), dir()).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    assert_eq!(
        contents(&fs, WAL_FILE),
        wal,
        "a refused open changes nothing"
    );
}

#[test]
fn a_wal_from_before_the_last_checkpoint_is_not_replayed() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    fill(&db, 20);
    db.checkpoint().unwrap();
    put(&db, b"k", b"old");
    // The WAL of checkpoint 1, holding k = old.
    let stale = contents(&fs, WAL_FILE);
    db.checkpoint().unwrap();
    put(&db, b"k", b"new");
    db.close().unwrap();

    // As if the WAL had not been emptied when the power went right after
    // a checkpoint wrote its meta page.
    overwrite(&fs, WAL_FILE, &stale);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_eq!(get(&db, b"k"), Some(b"new".to_vec()));
    assert_eq!(size(&fs, WAL_FILE), WAL_HEADER, "the stale WAL is emptied");
    put(&db, b"after", b"1");
    drop(db);
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_eq!(get(&db, b"after"), Some(b"1".to_vec()));
    assert_eq!(get(&db, b"k"), Some(b"new".to_vec()));
}

#[test]
fn a_torn_wal_header_after_emptying_opens() {
    for len in [0, 1, WAL_HEADER - 1] {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        let model = fill(&db, 30);
        db.close().unwrap();
        let wal = fs.open(&dir().join(WAL_FILE)).unwrap();
        wal.set_len(len).unwrap();
        wal.sync_data().unwrap();

        let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("len {len}: {e}"));
        assert_holds(&db, &model, &format!("len {len}"));
        assert_eq!(size(&fs, WAL_FILE), WAL_HEADER);
    }
}

#[test]
fn a_wal_of_one_header_length_whose_header_is_wrong_opens() {
    // Review P1: writing the header can be cut so that the file's new
    // length lands but its bytes do not, leaving 20 bytes of zeros. The
    // WAL was emptied and synced before, so nothing follows the header and
    // nothing is lost by writing it again.
    for damage in [[0u8; WAL_HEADER as usize], [0xAB; WAL_HEADER as usize]] {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        let model = fill(&db, 30);
        db.close().unwrap();
        let header = contents(&fs, WAL_FILE);

        overwrite(&fs, WAL_FILE, &damage);

        let db =
            Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{:x?}: {e}", &damage[..2]));
        assert_holds(&db, &model, "after reopening");
        assert_eq!(
            contents(&fs, WAL_FILE),
            header,
            "the header is written again"
        );
    }
}

#[test]
fn a_new_database_whose_wal_header_is_cut_that_way_opens() {
    let fs = SimFs::new();
    drop(Db::open_with(fs.clone(), dir()).unwrap());
    overwrite(&fs, WAL_FILE, &[0; WAL_HEADER as usize]);

    let db = Db::open_with(fs.clone(), dir()).unwrap();

    assert_eq!(db.page_count(), 3);
}

#[test]
fn a_damaged_wal_header_with_records_after_it_is_refused() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    fill(&db, 30);
    db.checkpoint().unwrap();
    put(&db, b"a", b"1");
    drop(db);
    let mut wal = contents(&fs, WAL_FILE);

    wal[3] ^= 1;
    overwrite(&fs, WAL_FILE, &wal);

    assert!(matches!(
        Db::open_with(fs.clone(), dir()),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn the_wal_stays_near_the_checkpoint_size_so_restarts_stay_short() {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    // One commit here touches a few pages at most; give it 16.
    let largest_commit = 16 * (PAGE + 13) + 37;
    let mut longest = 0;
    for i in 0u32..2500 {
        put(&db, &(i * 7919 % 2500).to_be_bytes(), &[i as u8; 700]);
        longest = longest.max(size(&fs, WAL_FILE));
    }

    assert!(
        longest > CHECKPOINT_AFTER / 2,
        "the test never fills the WAL"
    );
    assert!(
        longest <= CHECKPOINT_AFTER + largest_commit,
        "WAL reached {longest} bytes"
    );
    assert!(size(&fs, DATA_FILE) > 0, "no checkpoint ran");
}

/// Builds a database with every kind of page and history, checkpointed
/// `before` times, with commits after the last one, and returns what it
/// holds.
fn history(db: &Db<SimFs>, before: u32) -> BTreeMap<u32, Vec<u8>> {
    let mut model = fill_in_one(db, 120);
    for round in 0..before {
        db.checkpoint().unwrap();
        let mut txn = db.begin();
        for i in (round * 10)..(round * 10 + 10) {
            txn.put(&i.to_be_bytes(), &vec![round as u8; 3000]).unwrap();
            model.insert(i, vec![round as u8; 3000]);
        }
        txn.commit().unwrap();
        delete(db, &(100 + round).to_be_bytes());
        model.remove(&(100 + round));
    }
    put(db, &500u32.to_be_bytes(), &vec![9; 20_000]);
    model.insert(500, vec![9; 20_000]);
    model
}

#[test]
fn a_power_cut_anywhere_in_a_checkpoint_keeps_every_commit() {
    // The first checkpoint writes both slots; later ones write one. Cut
    // the power after each operation of either, with several crash seeds.
    for before in [0, 2] {
        let ops = {
            let fs = SimFs::new();
            let db = Db::open_with(fs.clone(), dir()).unwrap();
            history(&db, before);
            let start = fs.ops();
            db.checkpoint().unwrap();
            fs.ops() - start
        };
        assert!(ops > 4, "a checkpoint takes {ops} operations");
        for cut in 0..=ops {
            for seed in 0..8 {
                let fs = SimFs::new();
                let db = Db::open_with(fs.clone(), dir()).unwrap();
                let mut model = history(&db, before);
                fs.fail_after(cut);
                let _ = db.checkpoint();
                drop(db);

                fs.crash(seed);

                let what = format!("{before} checkpoints before, cut {cut}, seed {seed}");
                let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_holds(&db, &model, &what);
                // And it goes on: commit, checkpoint, crash, reopen.
                put(&db, &900u32.to_be_bytes(), b"later");
                model.insert(900, b"later".to_vec());
                db.checkpoint().unwrap();
                put(&db, &901u32.to_be_bytes(), b"later still");
                model.insert(901, b"later still".to_vec());
                drop(db);
                fs.crash(seed + 1000);
                let db = Db::open_with(fs.clone(), dir())
                    .unwrap_or_else(|e| panic!("{what}, then: {e}"));
                assert_holds(&db, &model, &format!("{what}, then"));
            }
        }
    }
}

/// Seeds for the cuts while the WAL starts again. The landing that leaves
/// a length and zeros comes one crash in twelve or so, so many seeds.
const START_SEEDS: u64 = 64;

/// The operations that end a checkpoint by starting the WAL again
/// (`Wal::start`): empty it, sync, write the header, sync. The tests below
/// check that cutting there does leave a header of zeros, so they notice
/// if a change to the checkpoint moves these operations.
const WAL_START_OPS: u64 = 4;

/// How many operations a checkpoint of what `setup` commits takes.
fn checkpoint_ops(setup: impl Fn(&Db<SimFs>)) -> u64 {
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    setup(&db);
    let start = fs.ops();
    db.checkpoint().unwrap();
    fs.ops() - start
}

/// Whether the WAL is one header long and all zeros.
fn wal_is_a_header_of_zeros(fs: &SimFs) -> bool {
    contents(fs, WAL_FILE) == [0; WAL_HEADER as usize]
}

#[test]
fn a_power_cut_while_the_wal_starts_again_keeps_every_commit() {
    // Issue #56. A checkpoint ends by emptying the WAL, syncing, writing
    // the header and syncing. The header is written past the end of the
    // emptied file, so a crash can land its length alone: 20 bytes of
    // zeros (review P1 of PR #55), which opening must take for no header.
    for before in [0, 2] {
        let ops = checkpoint_ops(|db| {
            history(db, before);
        });
        let mut zeros = 0;
        for cut in ops - WAL_START_OPS..=ops {
            for seed in 0..START_SEEDS {
                let fs = SimFs::new();
                let db = Db::open_with(fs.clone(), dir()).unwrap();
                let model = history(&db, before);
                fs.fail_after(cut);
                let _ = db.checkpoint();
                drop(db);

                fs.crash(seed);

                zeros += u32::from(wal_is_a_header_of_zeros(&fs));
                let what = format!("{before} checkpoints before, cut {cut}, seed {seed}");
                let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_holds(&db, &model, &what);
            }
        }
        assert!(zeros > 0, "{before} checkpoints before: no header of zeros");
    }
}

#[test]
fn a_cut_while_writing_the_wal_header_can_leave_its_length_and_zeros() {
    // The case review P1 found by reading the code, now made by a crash:
    // the WAL is one header long and all zeros, and the database opens.
    let ops = checkpoint_ops(|db| {
        fill(db, 30);
    });
    let mut zeros = 0;
    for seed in 0..START_SEEDS {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        let model = fill(&db, 30);
        // The power goes at the sync after the header, the last operation.
        fs.fail_after(ops - 1);
        assert!(db.checkpoint().is_err());
        drop(db);

        fs.crash(seed);

        zeros += u32::from(wal_is_a_header_of_zeros(&fs));
        let db = Db::open_with(fs.clone(), dir()).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_holds(&db, &model, &format!("seed {seed}"));
    }
    assert!(zeros > 0, "no seed left a header of zeros");
}

/// A file system whose next `sync_data` fails once when armed, with the
/// power still on.
#[derive(Clone)]
struct FlakyFs {
    sim: SimFs,
    fail_next_sync: Rc<Cell<bool>>,
}

struct FlakyFile {
    file: SimFile,
    fail_next_sync: Rc<Cell<bool>>,
}

impl Fs for FlakyFs {
    type File = FlakyFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.sim.create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<FlakyFile> {
        Ok(FlakyFile {
            file: self.sim.open(path)?,
            fail_next_sync: Rc::clone(&self.fail_next_sync),
        })
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.sim.sync_dir(dir)
    }
}

impl FsFile for FlakyFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn try_lock(&self) -> io::Result<()> {
        self.file.try_lock()
    }

    fn sync_data(&self) -> io::Result<()> {
        if self.fail_next_sync.replace(false) {
            return Err(io::Error::other("sync failed"));
        }
        self.file.sync_data()
    }
}

#[test]
fn a_failed_checkpoint_stops_the_engine() {
    let fs = FlakyFs {
        sim: SimFs::new(),
        fail_next_sync: Rc::new(Cell::new(false)),
    };
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let model = fill(&db, 50);

    fs.fail_next_sync.set(true);
    assert!(matches!(db.checkpoint(), Err(Error::Io(_))));

    // The disk works again, but the engine does not trust it.
    let mut txn = db.begin();
    txn.put(b"after", b"1").unwrap();
    assert!(matches!(txn.commit(), Err(Error::Io(_))));
    assert!(matches!(db.checkpoint(), Err(Error::Io(_))));
    assert_eq!(get(&db, b"after"), None);
    drop(db);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_holds(&db, &model, "after reopening");
    put(&db, b"after", b"1");
    assert_eq!(get(&db, b"after"), Some(b"1".to_vec()));
}

#[test]
fn a_data_file_shorter_than_its_meta_slot_says_is_refused() {
    // Mutant C14: nothing cut the data file short before.
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    fill(&db, 100);
    db.close().unwrap();
    let data = fs.open(&dir().join(DATA_FILE)).unwrap();
    data.set_len(size(&fs, DATA_FILE) - PAGE).unwrap();
    data.sync_data().unwrap();

    match Db::open_with(fs.clone(), dir()) {
        Err(Error::Corrupt(reason)) => assert!(reason.contains("shorter"), "{reason}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn only_damage_lets_a_cut_first_checkpoint_pass_over_the_data_file() {
    // Review P2: before the first checkpoint, a data file whose meta slots
    // are damaged is passed over, since the WAL holds every commit. A data
    // file of another page size is not damage: it is refused.
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, b"a", b"1");
    drop(db);
    let mut page = vec![0; 2 * PAGE as usize];
    page[4..12].copy_from_slice(b"oakdb\0\0\0");
    page[12..16].copy_from_slice(&4u32.to_le_bytes());
    page[16..20].copy_from_slice(&8192u32.to_le_bytes());
    overwrite(&fs, DATA_FILE, &page);

    let err = Db::open_with(fs.clone(), dir()).unwrap_err();

    assert!(
        matches!(err, Error::PageSizeMismatch { found: 8192, .. }),
        "{err:?}"
    );
}

// Numbers read from disk that arithmetic must not overflow on (review S3).

/// Rewrites meta slot `slot` with `change` applied to its bytes, and its
/// checksum filled in again, as a slot written with those values would be.
fn rewrite_slot(fs: &SimFs, slot: u64, change: impl Fn(&mut [u8])) {
    let file = fs.open(&dir().join(DATA_FILE)).unwrap();
    let mut page = vec![0; PAGE as usize];
    file.read_exact_at(&mut page, slot * PAGE).unwrap();
    change(&mut page);
    let crc = crc32c::crc32c_append(crc32c::crc32c(&page[4..]), &slot.to_le_bytes());
    page[..4].copy_from_slice(&crc.to_le_bytes());
    file.write_all_at(&page, slot * PAGE).unwrap();
    file.sync_data().unwrap();
}

/// The header of a WAL that follows `checkpoint`.
fn wal_header(checkpoint: u64) -> Vec<u8> {
    let mut header = b"oakwal\0\0".to_vec();
    header.extend_from_slice(&checkpoint.to_le_bytes());
    let crc = crc32c::crc32c_append(0x0AC0_DB10, &header);
    header.extend_from_slice(&crc.to_le_bytes());
    header
}

#[test]
fn a_meta_slot_with_more_pages_than_any_file_holds_is_refused() {
    // page_count × 4096 does not fit in a u64: the check that the data file
    // is long enough must still refuse it, not panic or wrap around.
    for page_count in [u64::MAX, u64::MAX / 4096 + 1, (1 << 52) + 3] {
        let fs = SimFs::new();
        let db = Db::open_with(fs.clone(), dir()).unwrap();
        fill(&db, 10);
        db.close().unwrap();
        for slot in [0, 1] {
            rewrite_slot(&fs, slot, |page| {
                page[28..36].copy_from_slice(&page_count.to_le_bytes())
            });
        }

        match Db::open_with(fs.clone(), dir()) {
            Err(Error::Corrupt(reason)) => assert!(reason.contains("shorter"), "{reason}"),
            other => panic!("page count {page_count}: expected Corrupt, got {other:?}"),
        }
    }
}

#[test]
fn a_checkpoint_number_that_cannot_grow_stops_the_checkpoint_not_the_data() {
    // The next checkpoint number must not wrap around to 0: a slot of
    // checkpoint 0 would lose to the other one, and opening would drop the
    // WAL as older than it, with every commit in it.
    let fs = SimFs::new();
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    let mut model = fill(&db, 10);
    db.close().unwrap();
    for slot in [0, 1] {
        rewrite_slot(&fs, slot, |page| {
            page[52..60].copy_from_slice(&u64::MAX.to_le_bytes())
        });
    }
    overwrite(&fs, WAL_FILE, &wal_header(u64::MAX));
    let db = Db::open_with(fs.clone(), dir()).unwrap();
    put(&db, &77u32.to_be_bytes(), b"acknowledged");
    model.insert(77, b"acknowledged".to_vec());

    assert!(db.checkpoint().is_err());
    drop(db);

    let db = Db::open_with(fs.clone(), dir()).unwrap();
    assert_holds(&db, &model, "after reopening");
}
