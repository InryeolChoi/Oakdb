//! Blind tests for ticket 07 (issue #19, page checksums).
//!
//! Written from the issue, the v1 spec, ADR 0008, design 0007 and the public
//! API only. The on-disk rule under test (design 0007, section 1):
//!
//! ```text
//! checksum = CRC-32C( page[4..4096] ‖ page_no as 8 little-endian bytes )
//! page[0..4] = checksum, little-endian
//! ```
//!
//! Pages 0 and 1 are meta slots A and B; only A is read when opening.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use oakdb::{Db, Error, DATA_FILE};
use proptest::prelude::*;

const PAGE: usize = 4096;

// ---------------------------------------------------------------- helpers

fn data_path(dir: &Path) -> PathBuf {
    dir.join(DATA_FILE)
}

fn read_file(dir: &Path) -> Vec<u8> {
    fs::read(data_path(dir)).expect("read data file")
}

fn write_file(dir: &Path, bytes: &[u8]) {
    fs::write(data_path(dir), bytes).expect("write data file");
}

fn expected_checksum(page_no: u64, page: &[u8]) -> u32 {
    assert_eq!(page.len(), PAGE);
    let crc = crc32c::crc32c(&page[4..]);
    crc32c::crc32c_append(crc, &page_no.to_le_bytes())
}

fn stored_checksum(page: &[u8]) -> u32 {
    u32::from_le_bytes(page[0..4].try_into().unwrap())
}

fn page_count(bytes: &[u8]) -> usize {
    assert_eq!(
        bytes.len() % PAGE,
        0,
        "file length {} is not whole pages",
        bytes.len()
    );
    bytes.len() / PAGE
}

/// Every page of the closed database carries the checksum the design asks
/// for. Returns the number of pages.
fn assert_all_checksums(dir: &Path) -> usize {
    let bytes = read_file(dir);
    let n = page_count(&bytes);
    assert!(
        n >= 2,
        "file has {n} pages, expected at least the two meta slots"
    );
    for p in 0..n {
        let page = &bytes[p * PAGE..(p + 1) * PAGE];
        assert_eq!(
            stored_checksum(page),
            expected_checksum(p as u64, page),
            "page {p} of {n} has the wrong checksum"
        );
    }
    n
}

/// Writes the checksum the design asks for into page `p` of `bytes`.
fn reseal(bytes: &mut [u8], p: usize) {
    let page = &mut bytes[p * PAGE..(p + 1) * PAGE];
    let c = expected_checksum(p as u64, page);
    page[0..4].copy_from_slice(&c.to_le_bytes());
}

fn put_all(db: &Db, entries: &[(Vec<u8>, Vec<u8>)]) {
    let mut t = db.begin();
    for (k, v) in entries {
        t.put(k, v).unwrap();
    }
    t.commit().unwrap();
}

fn key(i: usize) -> Vec<u8> {
    format!("key-{i:06}").into_bytes()
}

fn value(i: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| (i.wrapping_mul(31).wrapping_add(j * 7) % 251) as u8)
        .collect()
}

/// A database with several leaves, a long value over many pages and some
/// freed pages: something every page kind appears in.
fn build_rich_db(dir: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut model = BTreeMap::new();
    let db = Db::open(dir).unwrap();
    let entries: Vec<_> = (0..300).map(|i| (key(i), value(i, 100))).collect();
    put_all(&db, &entries);
    model.extend(entries);
    // Long values over many overflow pages.
    let long: Vec<_> = (0..3)
        .map(|i| (format!("long-{i}").into_bytes(), value(i, 20_000)))
        .collect();
    put_all(&db, &long);
    model.extend(long);
    // Delete a long value and a run of keys so pages are freed.
    let mut t = db.begin();
    t.delete(b"long-1").unwrap();
    model.remove(&b"long-1"[..]);
    for i in 100..200 {
        t.delete(&key(i)).unwrap();
        model.remove(&key(i));
    }
    t.commit().unwrap();
    db.close().unwrap();
    model
}

fn assert_contents(db: &Db, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
    let t = db.begin();
    let got: Vec<(Vec<u8>, Vec<u8>)> = t.scan::<&[u8]>(..).map(|r| r.unwrap()).collect();
    let want: Vec<(Vec<u8>, Vec<u8>)> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(got.len(), want.len(), "entry count differs");
    assert!(got == want, "contents differ from the model");
}

/// Opening must fail with `Error::Corrupt` naming page `p`.
fn assert_refused_naming(dir: &Path, p: usize, what: &str) {
    match Db::open(dir) {
        Ok(db) => {
            let _ = db.close();
            panic!("{what}: open succeeded, expected Corrupt naming page {p}");
        }
        Err(Error::Corrupt(msg)) => assert!(
            names_page(&msg, p),
            "{what}: Corrupt message {msg:?} does not name page {p}"
        ),
        Err(e) => panic!("{what}: expected Corrupt naming page {p}, got {e:?}"),
    }
}

/// Opening must fail. Used for damage to meta A's header (bytes 4..52),
/// which design 0007 section 3 checks (magic, page size, version) before
/// the checksum, so the error need not name the page.
fn assert_refused(dir: &Path, what: &str) {
    match Db::open(dir) {
        Ok(db) => {
            let _ = db.close();
            panic!("{what}: open succeeded, expected a refusal");
        }
        Err(Error::Corrupt(_)) | Err(Error::PageSizeMismatch { .. }) => {}
        Err(e) => panic!("{what}: expected Corrupt or PageSizeMismatch, got {e:?}"),
    }
}

/// Damage at `offset` of page `p`: refused naming the page, or, for meta A's
/// header, refused by the header check.
fn assert_refused_at(dir: &Path, p: usize, offset: usize, what: &str) {
    if p == 0 && (4..52).contains(&offset) {
        assert_refused(dir, what);
    } else {
        assert_refused_naming(dir, p, what);
    }
}

/// `msg` contains "page {p}" not followed by another digit.
fn names_page(msg: &str, p: usize) -> bool {
    let needle = format!("page {p}");
    msg.match_indices(&needle)
        .any(|(i, _)| !msg[i + needle.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

// ---------------------------------------------------- written checksums

#[test]
fn new_empty_database_has_checksums_from_the_start() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path()).unwrap().close().unwrap();
    assert_all_checksums(dir.path());
}

#[test]
fn both_meta_slots_carry_their_own_page_number() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_all(&db, &[(b"a".to_vec(), b"1".to_vec())]);
    db.close().unwrap();
    let bytes = read_file(dir.path());
    let a = &bytes[0..PAGE];
    let b = &bytes[PAGE..2 * PAGE];
    assert_eq!(stored_checksum(a), expected_checksum(0, a), "meta A");
    assert_eq!(stored_checksum(b), expected_checksum(1, b), "meta B");
    if a[4..] == b[4..] {
        // Same content, different page numbers: the checksums must differ.
        assert_ne!(
            stored_checksum(a),
            stored_checksum(b),
            "page number not mixed in"
        );
    }
}

#[test]
fn every_page_has_the_right_checksum_after_puts_splits_long_values_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let model = build_rich_db(dir.path());
    let n = assert_all_checksums(dir.path());
    assert!(
        n > 10,
        "expected splits and overflow pages, file has {n} pages"
    );
    let db = Db::open(dir.path()).unwrap();
    assert_contents(&db, &model);
    db.close().unwrap();
}

#[test]
fn checksums_stay_right_after_reopen_and_more_commits() {
    let dir = tempfile::tempdir().unwrap();
    let mut model = build_rich_db(dir.path());
    for round in 0..3 {
        let db = Db::open(dir.path()).unwrap();
        assert_contents(&db, &model);
        // Reuse freed pages and grow the file again.
        let entries: Vec<_> = (0..150)
            .map(|i| (key(1000 * (round + 1) + i), value(i + round, 300)))
            .collect();
        put_all(&db, &entries);
        model.extend(entries);
        let long = (format!("long-r{round}").into_bytes(), value(round, 9_000));
        put_all(&db, std::slice::from_ref(&long));
        model.insert(long.0, long.1);
        db.close().unwrap();
        assert_all_checksums(dir.path());
    }
}

#[test]
fn pages_added_as_the_file_grows_carry_checksums() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path()).unwrap().close().unwrap();
    let mut last = assert_all_checksums(dir.path());
    let db = Db::open(dir.path()).unwrap();
    let mut grew = 0;
    for step in 0..20 {
        let entries: Vec<_> = (0..40)
            .map(|i| (key(step * 40 + i), value(i, 200)))
            .collect();
        put_all(&db, &entries);
        // Inspect the file while the database is still open: each commit
        // is durable, so what is on disk must already be sealed.
        let bytes = read_file(dir.path());
        let n = page_count(&bytes);
        for p in 0..n {
            let page = &bytes[p * PAGE..(p + 1) * PAGE];
            assert_eq!(
                stored_checksum(page),
                expected_checksum(p as u64, page),
                "after commit {step}: page {p} of {n} has the wrong checksum"
            );
        }
        if n > last {
            grew += 1;
        }
        last = n;
    }
    db.close().unwrap();
    assert!(grew > 0, "the file never grew");
}

#[test]
fn a_whole_long_value_fills_many_pages_all_with_checksums() {
    let dir = tempfile::tempdir().unwrap();
    let big = value(7, 300_000);
    let db = Db::open(dir.path()).unwrap();
    put_all(&db, &[(b"big".to_vec(), big.clone())]);
    db.close().unwrap();
    let n = assert_all_checksums(dir.path());
    assert!(n >= 300_000 / PAGE, "only {n} pages for a 300 KB value");
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.begin().get(b"big").unwrap(), Some(big));
    db.close().unwrap();
}

// ------------------------------------------------------- detecting damage

#[test]
fn a_changed_byte_in_any_page_read_at_open_is_refused_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let model = build_rich_db(dir.path());
    let good = read_file(dir.path());
    let n = page_count(&good);
    // Page 1 (meta B) is not read at open; see the meta B test.
    let pages: Vec<usize> = std::iter::once(0).chain(2..n).collect();
    for p in pages {
        for offset in [0usize, 3, 4, 5, 100, PAGE / 2, PAGE - 1] {
            let mut bad = good.clone();
            bad[p * PAGE + offset] ^= 0x01;
            write_file(dir.path(), &bad);
            assert_refused_at(dir.path(), p, offset, &format!("page {p} byte {offset}"));
        }
    }
    // Detection did not change the file: the good copy still opens.
    write_file(dir.path(), &good);
    let db = Db::open(dir.path()).unwrap();
    assert_contents(&db, &model);
    db.close().unwrap();
}

#[test]
fn every_single_byte_position_of_a_leaf_page_is_covered() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_all(&db, &[(b"k".to_vec(), b"v".to_vec())]);
    db.close().unwrap();
    let good = read_file(dir.path());
    let n = page_count(&good);
    let p = n - 1; // a tree page, not a meta slot
    assert!(p >= 2);
    for offset in 0..PAGE {
        let mut bad = good.clone();
        bad[p * PAGE + offset] ^= 0x80;
        write_file(dir.path(), &bad);
        assert_refused_naming(dir.path(), p, &format!("byte {offset}"));
    }
}

#[test]
fn a_value_byte_changed_on_disk_is_never_returned() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    put_all(
        &db,
        &[(b"needle".to_vec(), b"HAYSTACK-VALUE-0123456789".to_vec())],
    );
    db.close().unwrap();
    let mut bytes = read_file(dir.path());
    let at = bytes
        .windows(b"HAYSTACK".len())
        .position(|w| w == b"HAYSTACK")
        .expect("value bytes are in the file");
    bytes[at] = b'X';
    write_file(dir.path(), &bytes);
    assert_refused_naming(dir.path(), at / PAGE, "value byte");
}

#[test]
fn a_changed_checksum_field_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    let p = page_count(&good) - 1;
    let mut bad = good.clone();
    let c = stored_checksum(&good[p * PAGE..]).wrapping_add(1);
    bad[p * PAGE..p * PAGE + 4].copy_from_slice(&c.to_le_bytes());
    write_file(dir.path(), &bad);
    assert_refused_naming(dir.path(), p, "checksum field +1");
    // A zero checksum is not "unchecked".
    let mut bad = good.clone();
    bad[p * PAGE..p * PAGE + 4].fill(0);
    write_file(dir.path(), &bad);
    assert_refused_naming(dir.path(), p, "checksum field zeroed");
}

#[test]
fn an_all_zero_page_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    let n = page_count(&good);
    for p in [0, 2, n / 2, n - 1] {
        let mut bad = good.clone();
        bad[p * PAGE..(p + 1) * PAGE].fill(0);
        write_file(dir.path(), &bad);
        // Zeroing meta A also zeroes its magic, caught by the header check.
        let offset = if p == 0 { 4 } else { 0 };
        assert_refused_at(dir.path(), p, offset, &format!("zeroed page {p}"));
    }
}

#[test]
fn a_valid_page_copied_to_another_page_number_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    let n = page_count(&good);
    assert!(n > 6);
    for (from, to) in [(2, 3), (n - 1, 2), (3, n - 1), (n / 2, n / 2 + 1)] {
        if good[from * PAGE..(from + 1) * PAGE] == good[to * PAGE..(to + 1) * PAGE] {
            continue;
        }
        let mut bad = good.clone();
        bad.copy_within(from * PAGE..(from + 1) * PAGE, to * PAGE);
        write_file(dir.path(), &bad);
        assert_refused_naming(dir.path(), to, &format!("page {from} copied to {to}"));
    }
}

#[test]
fn two_valid_pages_swapped_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    let n = page_count(&good);
    let (a, b) = (2, n - 1);
    let mut bad = good.clone();
    bad[a * PAGE..(a + 1) * PAGE].copy_from_slice(&good[b * PAGE..(b + 1) * PAGE]);
    bad[b * PAGE..(b + 1) * PAGE].copy_from_slice(&good[a * PAGE..(a + 1) * PAGE]);
    write_file(dir.path(), &bad);
    match Db::open(dir.path()) {
        Ok(_) => panic!("swapped pages {a} and {b} were accepted"),
        Err(Error::Corrupt(msg)) => assert!(
            names_page(&msg, a) || names_page(&msg, b),
            "message {msg:?} names neither {a} nor {b}"
        ),
        Err(e) => panic!("expected Corrupt, got {e:?}"),
    }
}

#[test]
fn meta_slot_b_copied_over_slot_a_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    let mut bad = good.clone();
    bad.copy_within(PAGE..2 * PAGE, 0);
    write_file(dir.path(), &bad);
    assert_refused_naming(dir.path(), 0, "meta B copied to A");
}

#[test]
fn a_half_written_page_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let entries: Vec<_> = (0..50).map(|i| (key(i), value(i, 50))).collect();
    put_all(&db, &entries);
    db.close().unwrap();
    let old = read_file(dir.path());

    let db = Db::open(dir.path()).unwrap();
    let entries: Vec<_> = (0..50).map(|i| (key(i), value(i + 1, 50))).collect();
    put_all(&db, &entries);
    db.close().unwrap();
    let new = read_file(dir.path());

    let n = page_count(&old).min(page_count(&new));
    let mut tested = 0;
    for p in std::iter::once(0).chain(2..n) {
        let (o, w) = (
            &old[p * PAGE..(p + 1) * PAGE],
            &new[p * PAGE..(p + 1) * PAGE],
        );
        for cut in [512, PAGE / 2, PAGE - 512] {
            // New head, old tail: a write stopped part way.
            let mut torn = w.to_vec();
            torn[cut..].copy_from_slice(&o[cut..]);
            if torn == w || torn == o {
                continue;
            }
            let mut bad = new.clone();
            bad[p * PAGE..(p + 1) * PAGE].copy_from_slice(&torn);
            write_file(dir.path(), &bad);
            assert_refused_naming(dir.path(), p, &format!("page {p} torn at {cut}"));
            tested += 1;
        }
        // New head, zero tail: the end never reached the disk.
        let mut torn = w.to_vec();
        torn[PAGE / 2..].fill(0);
        if torn != w {
            let mut bad = new.clone();
            bad[p * PAGE..(p + 1) * PAGE].copy_from_slice(&torn);
            write_file(dir.path(), &bad);
            assert_refused_naming(dir.path(), p, &format!("page {p} zero tail"));
            tested += 1;
        }
    }
    assert!(tested > 0, "no page changed between the two versions");
}

#[test]
fn damage_to_meta_slot_b_does_not_stop_opening() {
    let dir = tempfile::tempdir().unwrap();
    let model = build_rich_db(dir.path());
    let good = read_file(dir.path());
    let mut bad = good.clone();
    bad[PAGE + 100] ^= 0xFF;
    bad[PAGE + 2000] ^= 0xFF;
    write_file(dir.path(), &bad);
    let db = Db::open(dir.path()).expect("damage to meta B must not stop opening (left to #23)");
    assert_contents(&db, &model);
    db.close().unwrap();
}

#[test]
fn a_resealed_page_is_accepted_so_the_rule_matches_the_design() {
    // Guards against the tests above passing for the wrong reason: a page
    // changed and then given the design's checksum opens. A meta B byte is
    // the change, since its content is not interpreted at open.
    let dir = tempfile::tempdir().unwrap();
    let model = build_rich_db(dir.path());
    let mut bytes = read_file(dir.path());
    let n = page_count(&bytes);
    // Reseal every page (no-op if the engine follows the rule).
    for p in 0..n {
        reseal(&mut bytes, p);
    }
    write_file(dir.path(), &bytes);
    let db = Db::open(dir.path()).expect("pages sealed by the design's rule must open");
    assert_contents(&db, &model);
    db.close().unwrap();
}

#[test]
fn error_message_names_the_page() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    let p = page_count(&good) - 2;
    let mut bad = good.clone();
    bad[p * PAGE + 77] ^= 0x10;
    write_file(dir.path(), &bad);
    let err = match Db::open(dir.path()) {
        Ok(_) => panic!("damaged page {p} was accepted"),
        Err(e) => e,
    };
    let shown = err.to_string();
    assert!(
        names_page(&shown, p),
        "Display {shown:?} does not name page {p}"
    );
    assert!(
        matches!(err, Error::Corrupt(_)),
        "expected Corrupt, got {err:?}"
    );
}

// --------------------------------------------------------- format version

fn set_version(bytes: &mut [u8], version: u32) {
    // design 0001: bytes 12..16 of the meta page are the format version.
    bytes[12..16].copy_from_slice(&version.to_le_bytes());
}

#[test]
fn current_files_are_format_version_3() {
    let dir = tempfile::tempdir().unwrap();
    Db::open(dir.path()).unwrap().close().unwrap();
    let bytes = read_file(dir.path());
    assert_eq!(&bytes[4..12], b"oakdb\0\0\0", "magic");
    assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 3);
}

#[test]
fn older_versions_are_refused_by_version_not_by_checksum() {
    let dir = tempfile::tempdir().unwrap();
    build_rich_db(dir.path());
    let good = read_file(dir.path());
    for version in [1u32, 2] {
        for sealed in [false, true] {
            let mut bad = good.clone();
            set_version(&mut bad, version);
            if sealed {
                reseal(&mut bad, 0);
            } else {
                // An old file had no checksums at all.
                bad[0..4].fill(0);
            }
            write_file(dir.path(), &bad);
            let err = match Db::open(dir.path()) {
                Ok(_) => panic!("version {version} file (sealed={sealed}) was accepted"),
                Err(e) => e,
            };
            let shown = format!("{err} {err:?}");
            assert!(
                shown.contains("version") && shown.contains(&version.to_string()),
                "version {version} (sealed={sealed}) refused with {shown:?}, not by version"
            );
            assert!(
                !shown.contains("checksum"),
                "version {version} (sealed={sealed}) refused for its checksum: {shown:?}"
            );
        }
    }
}

// ------------------------------------------------------------ proptest

#[derive(Debug, Clone)]
enum Op {
    Put(u16, usize),
    Delete(u16),
    Commit,
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u16..400, prop_oneof![3 => 0usize..200, 2 => 200usize..1500, 1 => 1500usize..12_000])
            .prop_map(|(k, l)| Op::Put(k, l)),
        3 => (0u16..400).prop_map(Op::Delete),
        2 => Just(Op::Commit),
        1 => Just(Op::Reopen),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn random_operations_leave_every_page_sealed(ops in prop::collection::vec(op(), 1..120)) {
        let dir = tempfile::tempdir().unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut db = Db::open(dir.path()).unwrap();
        let mut pending: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let flush = |db: &Db, pending: &mut Vec<(Vec<u8>, Option<Vec<u8>>)>,
                     model: &mut BTreeMap<Vec<u8>, Vec<u8>>| {
            let mut t = db.begin();
            for (k, v) in pending.iter() {
                match v {
                    Some(v) => t.put(k, v).unwrap(),
                    None => t.delete(k).unwrap(),
                }
            }
            t.commit().unwrap();
            for (k, v) in pending.drain(..) {
                match v {
                    Some(v) => { model.insert(k, v); }
                    None => { model.remove(&k); }
                }
            }
        };
        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Put(k, len) => pending.push((key(*k as usize), Some(value(i, *len)))),
                Op::Delete(k) => pending.push((key(*k as usize), None)),
                Op::Commit => flush(&db, &mut pending, &mut model),
                Op::Reopen => {
                    flush(&db, &mut pending, &mut model);
                    db.close().unwrap();
                    assert_all_checksums(dir.path());
                    db = Db::open(dir.path()).unwrap();
                    assert_contents(&db, &model);
                }
            }
        }
        flush(&db, &mut pending, &mut model);
        db.close().unwrap();
        assert_all_checksums(dir.path());
        let db = Db::open(dir.path()).unwrap();
        assert_contents(&db, &model);
        db.close().unwrap();
    }
}
