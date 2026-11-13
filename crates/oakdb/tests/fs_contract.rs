//! What every file system the engine runs on must do, checked on the real
//! one and on the simulated one alike. Without a crash, the simulation must
//! be indistinguishable from the real thing.

use std::io;
use std::path::Path;

use oakdb::fs::sim::SimFs;
use oakdb::fs::{Fs, FsFile, StdFs};

fn written_data_reads_back_after_reopen(fs: impl Fs, dir: &Path) {
    let path = dir.join("data");
    let file = fs.open(&path).unwrap();
    file.write_all_at(b"oak", 4096).unwrap();
    file.sync_data().unwrap();
    fs.sync_dir(dir).unwrap();
    drop(file);

    let file = fs.open(&path).unwrap();
    let mut buf = [0u8; 3];
    file.read_exact_at(&mut buf, 4096).unwrap();
    assert_eq!(&buf, b"oak");
    assert_eq!(file.size().unwrap(), 4099);
    // Writing past the end fills the gap with zeros.
    let mut gap = [1u8; 8];
    file.read_exact_at(&mut gap, 100).unwrap();
    assert_eq!(gap, [0; 8]);
}

fn unsynced_writes_are_read_back(fs: impl Fs, dir: &Path) {
    let file = fs.open(&dir.join("data")).unwrap();
    file.write_all_at(b"tree", 0).unwrap();
    file.write_all_at(b"ee", 1).unwrap();
    let mut buf = [0u8; 4];
    file.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"teee");
}

fn reading_past_the_end_fails(fs: impl Fs, dir: &Path) {
    let file = fs.open(&dir.join("data")).unwrap();
    file.write_all_at(b"oak", 0).unwrap();
    let mut buf = [0u8; 4];
    let err = file.read_exact_at(&mut buf, 0).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

fn opening_keeps_existing_contents(fs: impl Fs, dir: &Path) {
    let path = dir.join("data");
    fs.open(&path).unwrap().write_all_at(b"keep", 0).unwrap();

    let file = fs.open(&path).unwrap();
    let mut buf = [0u8; 4];
    file.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"keep");
}

fn opening_in_a_missing_directory_fails(fs: impl Fs, dir: &Path) {
    let err = fs.open(&dir.join("missing").join("data")).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

fn create_dir_all_creates_missing_parents(fs: impl Fs, dir: &Path) {
    let dir = dir.join("a").join("b");
    fs.create_dir_all(&dir).unwrap();
    fs.create_dir_all(&dir).unwrap();
    fs.open(&dir.join("data")).unwrap();
}

fn create_dir_all_fails_when_a_file_is_in_the_way(fs: impl Fs, dir: &Path) {
    let file = dir.join("file");
    fs.open(&file).unwrap();
    assert!(fs.create_dir_all(&file).is_err());
    assert!(fs.create_dir_all(&file.join("db")).is_err());
}

fn a_locked_file_cannot_be_locked_again_until_it_is_closed(fs: impl Fs, dir: &Path) {
    let path = dir.join("data");
    let first = fs.open(&path).unwrap();
    first.try_lock().unwrap();

    let second = fs.open(&path).unwrap();
    let err = second.try_lock().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

    drop(first);
    second.try_lock().unwrap();
}

fn set_len_grows_with_zeros_and_truncates(fs: impl Fs, dir: &Path) {
    let file = fs.open(&dir.join("data")).unwrap();
    file.write_all_at(&[1; 100], 0).unwrap();
    file.set_len(10).unwrap();
    assert_eq!(file.size().unwrap(), 10);
    file.set_len(20).unwrap();
    let mut buf = [9u8; 20];
    file.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(buf[..10], [1; 10]);
    assert_eq!(buf[10..], [0; 10]);
}

/// Runs each check once on the real file system, in a fresh temporary
/// directory, and once on a fresh simulated one.
macro_rules! on_both {
    ($($check:ident),* $(,)?) => {
        mod real {
            $(
                #[test]
                fn $check() {
                    let dir = tempfile::tempdir().unwrap();
                    super::$check(super::StdFs, dir.path());
                }
            )*
        }
        mod sim {
            $(
                #[test]
                fn $check() {
                    let fs = super::SimFs::new();
                    let dir = std::path::Path::new("/tmp/oak");
                    super::Fs::create_dir_all(&fs, dir).unwrap();
                    super::$check(fs, dir);
                }
            )*
        }
    };
}

on_both!(
    written_data_reads_back_after_reopen,
    unsynced_writes_are_read_back,
    reading_past_the_end_fails,
    opening_keeps_existing_contents,
    opening_in_a_missing_directory_fails,
    create_dir_all_creates_missing_parents,
    create_dir_all_fails_when_a_file_is_in_the_way,
    a_locked_file_cannot_be_locked_again_until_it_is_closed,
    set_len_grows_with_zeros_and_truncates,
);
