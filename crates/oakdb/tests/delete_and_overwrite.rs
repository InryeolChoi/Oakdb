//! Deleting keys and overwriting values through the engine API.

use std::collections::BTreeMap;

use oakdb::{Db, Error, MAX_KEY_LEN};

#[test]
fn a_deleted_key_reads_as_missing() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin();
    txn.delete(b"oak").unwrap();
    assert_eq!(txn.get(b"oak").unwrap(), None);
    txn.commit().unwrap();

    assert_eq!(db.begin().get(b"oak").unwrap(), None);
}

#[test]
fn a_deleted_key_stays_deleted_after_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    txn.put(b"elm", b"tree").unwrap();
    txn.commit().unwrap();
    let mut txn = db.begin();
    txn.delete(b"oak").unwrap();
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();

    assert_eq!(txn.get(b"oak").unwrap(), None);
    assert_eq!(txn.get(b"elm").unwrap(), Some(b"tree".to_vec()));
}

#[test]
fn deleting_a_missing_key_is_not_an_error() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();

    txn.delete(b"never-written").unwrap();
    txn.commit().unwrap();

    assert_eq!(db.begin().get(b"never-written").unwrap(), None);
}

#[test]
fn the_last_write_to_a_key_in_a_transaction_wins() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"kept", b"old").unwrap();
    txn.put(b"gone", b"old").unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin();
    txn.delete(b"kept").unwrap();
    txn.put(b"kept", b"new").unwrap();
    txn.put(b"gone", b"new").unwrap();
    txn.delete(b"gone").unwrap();
    assert_eq!(txn.get(b"kept").unwrap(), Some(b"new".to_vec()));
    assert_eq!(txn.get(b"gone").unwrap(), None);
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    assert_eq!(txn.get(b"kept").unwrap(), Some(b"new".to_vec()));
    assert_eq!(txn.get(b"gone").unwrap(), None);
}

#[test]
fn a_rolled_back_delete_leaves_the_value() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    txn.put(b"oak", b"tree").unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin();
    txn.delete(b"oak").unwrap();
    txn.rollback();

    assert_eq!(db.begin().get(b"oak").unwrap(), Some(b"tree".to_vec()));
}

#[test]
fn deleting_a_key_longer_than_512_bytes_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();

    let err = db.begin().delete(&[7; MAX_KEY_LEN + 1]).unwrap_err();

    assert!(matches!(err, Error::KeyTooLarge { len: 513, max: 512 }));
}

/// Fills most of the page with ten 350-byte values under keys 0..10.
fn fill_with_ten_values(db: &Db) {
    let mut txn = db.begin();
    for key in 0u8..10 {
        txn.put(&[key], &[key; 350]).unwrap();
    }
    txn.commit().unwrap();
}

#[test]
fn space_freed_by_deletes_takes_new_keys_after_reopening() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    fill_with_ten_values(&db);
    let mut txn = db.begin();
    for key in 1u8..10 {
        txn.delete(&[key]).unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let mut txn = db.begin();
    // Only fits if the space of the nine deleted values is reused.
    txn.put(b"new", &[1; 3000]).unwrap();
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    assert_eq!(txn.get(b"new").unwrap(), Some(vec![1; 3000]));
    assert_eq!(txn.get(&[0]).unwrap(), Some(vec![0; 350]));
    for key in 1u8..10 {
        assert_eq!(txn.get(&[key]).unwrap(), None);
    }
}

#[test]
fn a_commit_fits_if_its_end_result_fits_whatever_the_key_order() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    fill_with_ten_values(&db);

    // The new key sorts before every deleted or shrunk key, so it only fits
    // if their space is given back before it goes in.
    let mut txn = db.begin();
    txn.put(b"\x00new", &[1; 2000]).unwrap();
    for key in 1u8..5 {
        txn.delete(&[key]).unwrap();
    }
    for key in 5u8..10 {
        txn.put(&[key], &[key]).unwrap();
    }
    txn.commit().unwrap();
    db.close().unwrap();

    let db = Db::open(root.path()).unwrap();
    let txn = db.begin();
    assert_eq!(txn.get(b"\x00new").unwrap(), Some(vec![1; 2000]));
    assert_eq!(txn.get(&[2]).unwrap(), None);
    assert_eq!(txn.get(&[7]).unwrap(), Some(vec![7]));
}

#[test]
fn a_commit_that_does_not_fit_keeps_its_deletes_out_too() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(root.path()).unwrap();
    fill_with_ten_values(&db);

    let mut txn = db.begin();
    txn.delete(&[0]).unwrap();
    txn.put(b"huge", &[1; 4000]).unwrap();
    let err = txn.commit().unwrap_err();

    assert!(matches!(err, Error::PageFull));
    assert_eq!(db.begin().get(&[0]).unwrap(), Some(vec![0; 350]));
}

/// A small deterministic random number generator (xorshift), so a failure
/// replays exactly.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Whether `entries` fit in the database's only page: each takes a 4-byte
/// slot and a 4-byte cell header besides its key and value, out of the 4056
/// bytes after the page header. Goes away once pages can split (#15).
fn fits_in_one_page(entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> bool {
    let bytes: usize = entries.iter().map(|(k, v)| 8 + k.len() + v.len()).sum();
    bytes <= 4096 - 40
}

#[test]
fn random_puts_and_deletes_match_a_btreemap() {
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(root.path()).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(0x0a4b_d0b1);

    for round in 0..400 {
        let mut txn = db.begin();
        let mut pending = model.clone();
        for _ in 0..1 + rng.below(8) {
            let key = vec![rng.below(30) as u8];
            if rng.below(3) == 0 {
                txn.delete(&key).unwrap();
                pending.remove(&key);
            } else {
                let value = vec![round as u8; rng.below(300) as usize];
                txn.put(&key, &value).unwrap();
                pending.insert(key, value);
            }
        }
        match txn.commit() {
            Ok(()) => model = pending,
            Err(Error::PageFull) => {
                assert!(
                    !fits_in_one_page(&pending),
                    "round {round}: spurious PageFull"
                )
            }
            Err(e) => panic!("round {round}: {e}"),
        }
        if round % 50 == 49 {
            db.close().unwrap();
            db = Db::open(root.path()).unwrap();
        }
        let txn = db.begin();
        for key in 0u8..30 {
            assert_eq!(
                txn.get(&[key]).unwrap().as_ref(),
                model.get(&vec![key]),
                "round {round}, key {key}"
            );
        }
    }
}
