//! Every page on disk carries a CRC-32C of its bytes and its page number
//! (#19), so a page that changed, was half written, or was written in the
//! wrong place is reported as corrupt instead of being read.

use std::path::Path;

use oakdb::{Db, Error, DATA_FILE};

const PAGE: usize = 4096;

fn data_file(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join(DATA_FILE)).unwrap()
}

fn write_data_file(dir: &Path, bytes: &[u8]) {
    std::fs::write(dir.join(DATA_FILE), bytes).unwrap();
}

/// The checksum page `page_no` should carry: CRC-32C of its bytes after the
/// checksum field, then of the page number as 8 little-endian bytes.
fn expected_checksum(page_no: u64, page: &[u8]) -> u32 {
    let crc = crc32c::crc32c(&page[4..]);
    crc32c::crc32c_append(crc, &page_no.to_le_bytes())
}

fn stored_checksum(page: &[u8]) -> u32 {
    u32::from_le_bytes(page[..4].try_into().unwrap())
}

/// A database with leaves, internal pages, a long value in overflow pages,
/// and free pages left by a deleted long value.
fn every_kind_of_page(dir: &Path) {
    let db = Db::open(dir).unwrap();
    let mut txn = db.begin();
    for i in 0u32..200 {
        txn.put(&i.to_be_bytes(), &[i as u8; 100]).unwrap();
    }
    txn.put(b"long", &vec![7; 20_000]).unwrap();
    txn.put(b"gone", &vec![8; 10_000]).unwrap();
    txn.commit().unwrap();
    let mut txn = db.begin();
    txn.delete(b"gone").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();
}

/// Opens `dir` and expects it to be refused as corrupt, with a reason that
/// mentions `words`.
fn assert_refused(dir: &Path, words: &str) {
    match Db::open(dir) {
        Err(Error::Corrupt(reason)) => {
            assert!(reason.contains(words), "reason {reason:?} lacks {words:?}")
        }
        other => panic!("expected Corrupt mentioning {words:?}, got {other:?}"),
    }
}

#[test]
fn every_page_on_disk_carries_the_checksum_of_its_bytes_and_its_number() {
    let root = tempfile::tempdir().unwrap();
    every_kind_of_page(root.path());

    let file = data_file(root.path());

    assert!(file.len() / PAGE > 10);
    for (page_no, page) in file.chunks(PAGE).enumerate() {
        let page_no = page_no as u64;
        assert_eq!(
            stored_checksum(page),
            expected_checksum(page_no, page),
            "page {page_no}"
        );
    }
}

#[test]
fn a_new_database_already_has_its_checksums() {
    let root = tempfile::tempdir().unwrap();
    Db::open(root.path()).unwrap().close().unwrap();

    let file = data_file(root.path());

    for (page_no, page) in file.chunks(PAGE).enumerate() {
        assert_eq!(
            stored_checksum(page),
            expected_checksum(page_no as u64, page)
        );
        assert_ne!(stored_checksum(page), 0, "page {page_no}");
    }
}

#[test]
fn a_changed_byte_in_a_value_is_reported_with_its_page() {
    // Experiment 0001 changed one byte of a value and nothing noticed.
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();
    let mut file = data_file(root.path());
    let at = file.windows(4).rposition(|w| w == b"tree").unwrap();
    assert_eq!(at / PAGE, 2);

    file[at] = b'f';
    write_data_file(root.path(), &file);

    assert_refused(root.path(), "page 2");
}

#[test]
fn a_page_copied_to_another_place_is_reported() {
    // A whole, valid page written where another page belongs: only the page
    // number in its checksum gives it away.
    let root = tempfile::tempdir().unwrap();
    every_kind_of_page(root.path());
    let mut file = data_file(root.path());
    let (first, second) = (3 * PAGE, 4 * PAGE);
    let copy = file[first..first + PAGE].to_vec();

    file[second..second + PAGE].copy_from_slice(&copy);
    write_data_file(root.path(), &file);

    assert_refused(root.path(), "page 4");
}

#[test]
fn a_half_written_page_is_reported() {
    let root = tempfile::tempdir().unwrap();
    every_kind_of_page(root.path());
    let mut file = data_file(root.path());
    let last = file.len() / PAGE - 1;

    // The second half of the last page never reached the disk.
    file[last * PAGE + PAGE / 2..(last + 1) * PAGE].fill(0xAA);
    write_data_file(root.path(), &file);

    assert_refused(root.path(), &format!("page {last}"));
}

#[test]
fn a_changed_byte_in_any_kind_of_page_is_reported() {
    let root = tempfile::tempdir().unwrap();
    every_kind_of_page(root.path());
    let clean = data_file(root.path());

    // Flip one bit in every page in turn, meta slot A included: in the
    // header field right after the checksum, in the middle, and at the
    // end. Meta slot B (page 1) is not read until checkpoints alternate
    // between the slots (#23), so damage there shows only then.
    for page_no in (0..clean.len() / PAGE).filter(|&p| p != 1) {
        for offset in [4, PAGE / 2, PAGE - 1] {
            let mut file = clean.clone();
            file[page_no * PAGE + offset] ^= 0x10;
            write_data_file(root.path(), &file);

            assert!(
                matches!(Db::open(root.path()), Err(Error::Corrupt(_))),
                "page {page_no}, byte {offset}"
            );
        }
    }
    write_data_file(root.path(), &clean);
    Db::open(root.path()).unwrap();
}

#[test]
fn a_changed_checksum_is_reported() {
    let root = tempfile::tempdir().unwrap();
    every_kind_of_page(root.path());
    let mut file = data_file(root.path());

    file[2 * PAGE] ^= 1;
    write_data_file(root.path(), &file);

    assert_refused(root.path(), "page 2");
}

#[test]
fn a_file_of_an_older_format_is_refused_by_its_version() {
    let root = tempfile::tempdir().unwrap();
    Db::open(root.path()).unwrap().close().unwrap();
    let mut file = data_file(root.path());

    // Version 2 files had no checksums: the field holds 0.
    file[12..16].copy_from_slice(&2u32.to_le_bytes());
    for page in file.chunks_mut(PAGE) {
        page[..4].fill(0);
    }
    write_data_file(root.path(), &file);

    assert_refused(root.path(), "version 2");
}
