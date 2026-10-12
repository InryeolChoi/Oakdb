//! Storing data in a single leaf page through the engine API.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use oakdb::fs::{Fs, StdFs};
use oakdb::{Db, Error, DATA_FILE, MAX_KEY_LEN};

#[test]
fn opening_creates_a_missing_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("nested").join("db");

    Db::open(&dir).unwrap();

    assert!(dir.is_dir());
}

#[test]
fn a_transaction_reads_its_own_writes() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();

    assert_eq!(txn.get(b"oak").unwrap(), Some(b"tree".to_vec()));
    assert_eq!(txn.get(b"elm").unwrap(), None);
}

#[test]
fn committed_values_survive_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    txn.put(b"acorn", b"seed").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();

    assert_eq!(txn.get(b"oak").unwrap(), Some(b"tree".to_vec()));
    assert_eq!(txn.get(b"acorn").unwrap(), Some(b"seed".to_vec()));
}

#[test]
fn values_from_several_commits_are_all_kept() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    // Out of key order, so every commit has to insert between existing keys.
    for key in [5u8, 1, 9, 3, 7, 0, 8, 2, 6, 4] {
        let mut txn = db.begin();
        txn.put(&[key], &[key; 10]).unwrap();
        txn.commit().unwrap();
    }
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    for key in 0u8..10 {
        assert_eq!(txn.get(&[key]).unwrap(), Some(vec![key; 10]));
    }
}

#[test]
fn putting_a_key_again_keeps_the_newest_value() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"first").unwrap();
    txn.put(b"oak", b"second").unwrap();
    txn.commit().unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"third").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();

    assert_eq!(db.begin().get(b"oak").unwrap(), Some(b"third".to_vec()));
}

#[test]
fn a_dropped_transaction_leaves_no_trace() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    drop(txn);
    let mut txn = db.begin();
    txn.put(b"elm", b"tree").unwrap();
    txn.rollback();

    assert_eq!(db.begin().get(b"oak").unwrap(), None);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    assert_eq!(db.begin().get(b"oak").unwrap(), None);
    assert_eq!(db.begin().get(b"elm").unwrap(), None);
}

#[test]
fn keys_longer_than_512_bytes_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();

    txn.put(&[7; MAX_KEY_LEN], b"fits").unwrap();
    let err = txn.put(&[7; MAX_KEY_LEN + 1], b"too long").unwrap_err();

    assert!(matches!(err, Error::KeyTooLarge { len: 513, max: 512 }));
    assert_eq!(txn.get(&[7; MAX_KEY_LEN]).unwrap(), Some(b"fits".to_vec()));
}

#[test]
fn reading_a_key_longer_than_512_bytes_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    let err = db.begin().get(&[7; MAX_KEY_LEN + 1]).unwrap_err();

    assert!(matches!(err, Error::KeyTooLarge { len: 513, max: 512 }));
}

#[test]
fn overwriting_one_key_again_and_again_never_fills_the_page() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    for i in 0..2000usize {
        let mut txn = db.begin();
        txn.put(b"counter", &vec![7; 100 + i % 50]).unwrap();
        txn.commit().unwrap();
    }

    assert_eq!(db.begin().get(b"counter").unwrap(), Some(vec![7; 149]));
}

#[test]
fn space_freed_by_shrinking_values_takes_new_keys_after_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    for key in 0u8..10 {
        txn.put(&[key], &[key; 350]).unwrap();
    }
    txn.commit().unwrap();
    let mut txn = db.begin();
    for key in 0u8..10 {
        txn.put(&[key], &[key]).unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"new", &[1; 3000]).unwrap();
    txn.commit().unwrap();

    let txn = db.begin();
    assert_eq!(txn.get(b"new").unwrap(), Some(vec![1; 3000]));
    for key in 0u8..10 {
        assert_eq!(txn.get(&[key]).unwrap(), Some(vec![key]));
    }
}

#[test]
fn a_database_that_is_already_open_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    let err = Db::open(root.path()).unwrap_err();

    assert!(matches!(err, Error::AlreadyOpen));
    db.close().unwrap();
    Db::open(root.path()).unwrap();
}

#[test]
fn a_commit_that_does_not_fit_in_the_page_fails_and_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"kept", b"value").unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin();
    for i in 0u32..200 {
        txn.put(&i.to_be_bytes(), &[0; 32]).unwrap();
    }
    let err = txn.commit().unwrap_err();

    assert!(matches!(err, Error::PageFull));
    assert_eq!(db.begin().get(&0u32.to_be_bytes()).unwrap(), None);
    db.close().unwrap();
    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    assert_eq!(txn.get(b"kept").unwrap(), Some(b"value".to_vec()));
    assert_eq!(txn.get(&0u32.to_be_bytes()).unwrap(), None);
}

#[test]
fn a_page_holds_many_small_entries() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    // Each entry takes 4 (slot) + 4 (cell header) + 4 (key) + 8 (value) = 20
    // bytes, so 200 of them fit in the 4056 bytes left after the header.
    for i in 0u32..200 {
        txn.put(&i.to_be_bytes(), &u64::from(i).to_le_bytes())
            .unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    for i in 0u32..200 {
        let value = u64::from(i).to_le_bytes().to_vec();
        assert_eq!(txn.get(&i.to_be_bytes()).unwrap(), Some(value));
    }
}

#[test]
fn a_file_that_is_not_an_oakdb_file_is_refused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(DATA_FILE), [0xAB; 8192]).unwrap();

    let err = Db::open(root.path()).unwrap_err();

    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}

/// What the engine asked the file system to do.
#[derive(Debug, PartialEq)]
enum Call {
    CreateDirAll(PathBuf),
    Open(PathBuf),
    SyncDir(PathBuf),
}

/// Passes every call to the real file system and records it.
#[derive(Clone, Default)]
struct RecordingFs(Rc<RefCell<Vec<Call>>>);

impl Fs for RecordingFs {
    type File = <StdFs as Fs>::File;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.0.borrow_mut().push(Call::CreateDirAll(path.into()));
        StdFs.create_dir_all(path)
    }

    fn open(&self, path: &Path) -> io::Result<Self::File> {
        self.0.borrow_mut().push(Call::Open(path.into()));
        StdFs.open(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.0.borrow_mut().push(Call::SyncDir(dir.into()));
        StdFs.sync_dir(dir)
    }
}

#[test]
fn file_access_goes_through_the_given_file_system() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("db");
    let fs = RecordingFs::default();

    let db = Db::open_with(fs.clone(), &dir).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();

    assert_eq!(
        *fs.0.borrow(),
        [
            Call::CreateDirAll(dir.clone()),
            Call::Open(dir.join(DATA_FILE)),
            // A new file is only durable once its directory is synced.
            Call::SyncDir(dir.clone()),
        ]
    );
}
