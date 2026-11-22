//! How SQL values become engine keys and values (design 0017, section 2).
//!
//! The engine sorts keys bytewise, so a primary key is encoded so that
//! comparing the bytes gives the same answer as comparing the values
//! (C4): that is what lets a range of primary keys (#32) be one scan.
//! Row values never get compared, so they are encoded only to be read
//! back (C6).

use crate::{SqlError, Value};

/// First byte of every catalog key: `[0x00]` alone holds the next table
/// id, `[0x00][table name]` holds a table's description.
pub(crate) const CATALOG_PREFIX: u8 = 0x00;

/// First byte of every row key: `[0x01][table id][primary key]`.
pub(crate) const ROW_PREFIX: u8 = 0x01;

/// Appends `value`, a primary key, to `out` so that keys sort as values
/// do. Integers sort among integers, and text and blobs bytewise among
/// themselves; a table's primary key holds one type, so values of
/// different types are never compared. NULL is never a primary key.
pub(crate) fn encode_key(value: &Value, out: &mut Vec<u8>) {
    let _ = (value, out);
    unimplemented!("key encoding")
}

/// The key of the row of table `table_id` whose primary key is `key`.
pub(crate) fn row_key(table_id: u32, key: &Value) -> Vec<u8> {
    let _ = (table_id, key);
    unimplemented!("row key")
}

/// Encodes a row's values, one per column in column order.
pub(crate) fn encode_row(values: &[Value]) -> Vec<u8> {
    let _ = values;
    unimplemented!("row encoding")
}

/// Decodes a row that [`encode_row`] encoded.
pub(crate) fn decode_row(bytes: &[u8]) -> Result<Vec<Value>, SqlError> {
    let _ = bytes;
    unimplemented!("row decoding")
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use proptest::prelude::*;

    use super::*;

    fn key(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        encode_key(value, &mut out);
        out
    }

    /// Bytes where 0x00 and 0xFF, the bytes the escaping is about, come up
    /// often, along with a few ordinary ones.
    fn tricky_bytes() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop_oneof![
                Just(0x00u8),
                Just(0xFF),
                Just(0x01),
                Just(0xFE),
                any::<u8>()
            ],
            0..12,
        )
    }

    fn any_value() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<i64>().prop_map(Value::Integer),
            ".{0,20}".prop_map(Value::Text),
            tricky_bytes().prop_map(Value::Blob),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5000))]

        #[test]
        fn integer_keys_sort_as_the_integers_do(a in any::<i64>(), b in any::<i64>()) {
            prop_assert_eq!(key(&Value::Integer(a)).cmp(&key(&Value::Integer(b))), a.cmp(&b));
        }

        #[test]
        fn integer_keys_near_zero_and_the_ends_sort_too(
            a in prop_oneof![-3i64..3, Just(i64::MIN), Just(i64::MAX), Just(i64::MIN + 1), Just(i64::MAX - 1)],
            b in prop_oneof![-3i64..3, Just(i64::MIN), Just(i64::MAX), Just(i64::MIN + 1), Just(i64::MAX - 1)],
        ) {
            prop_assert_eq!(key(&Value::Integer(a)).cmp(&key(&Value::Integer(b))), a.cmp(&b));
        }

        #[test]
        fn blob_keys_sort_as_the_bytes_do(a in tricky_bytes(), b in tricky_bytes()) {
            let order = key(&Value::Blob(a.clone())).cmp(&key(&Value::Blob(b.clone())));
            prop_assert_eq!(order, a.cmp(&b));
        }

        #[test]
        fn text_keys_sort_as_the_text_does(a in ".{0,12}", b in ".{0,12}") {
            let order = key(&Value::Text(a.clone())).cmp(&key(&Value::Text(b.clone())));
            prop_assert_eq!(order, a.cmp(&b));
        }

        #[test]
        fn text_with_nul_characters_sorts_too(
            a in "[a\u{0}b]{0,8}",
            b in "[a\u{0}b]{0,8}",
        ) {
            let order = key(&Value::Text(a.clone())).cmp(&key(&Value::Text(b.clone())));
            prop_assert_eq!(order, a.cmp(&b));
        }

        #[test]
        fn row_keys_of_one_table_sort_as_their_primary_keys(
            table in any::<u32>(),
            a in any::<i64>(),
            b in any::<i64>(),
        ) {
            let order = row_key(table, &Value::Integer(a)).cmp(&row_key(table, &Value::Integer(b)));
            prop_assert_eq!(order, a.cmp(&b));
        }

        #[test]
        fn every_row_of_a_table_sorts_before_every_row_of_the_next(
            table in 0u32..u32::MAX,
            a in tricky_bytes(),
            b in tricky_bytes(),
        ) {
            let first = row_key(table, &Value::Blob(a));
            let next = row_key(table + 1, &Value::Blob(b));
            prop_assert_eq!(first.cmp(&next), Ordering::Less);
        }

        #[test]
        fn a_row_reads_back_as_it_was_written(row in prop::collection::vec(any_value(), 0..8)) {
            prop_assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
        }

        #[test]
        fn a_cut_row_is_corrupt_not_a_panic(
            row in prop::collection::vec(any_value(), 1..6),
            cut in any::<prop::sample::Index>(),
        ) {
            let bytes = encode_row(&row);
            let len = cut.index(bytes.len());
            // Cutting before the end loses at least part of the last value,
            // unless it falls between whole values; either way it must not
            // panic.
            let _ = decode_row(&bytes[..len]);
        }
    }

    #[test]
    fn integer_keys_are_big_endian_with_the_sign_bit_flipped() {
        assert_eq!(key(&Value::Integer(0)), [0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(key(&Value::Integer(1)), [0x80, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            key(&Value::Integer(-1)),
            [0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(key(&Value::Integer(i64::MIN)), [0; 8]);
    }

    #[test]
    fn text_keys_escape_nul_and_end_with_two_nuls() {
        assert_eq!(key(&Value::Text("a".into())), [0x61, 0x00, 0x00]);
        assert_eq!(
            key(&Value::Text("a\0".into())),
            [0x61, 0x00, 0xFF, 0x00, 0x00]
        );
        assert_eq!(key(&Value::Blob(vec![])), [0x00, 0x00]);
    }

    #[test]
    fn a_row_key_is_the_prefix_the_table_id_and_the_key() {
        assert_eq!(
            row_key(1, &Value::Integer(1)),
            [0x01, 0, 0, 0, 1, 0x80, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn a_row_is_a_tag_and_a_payload_per_column() {
        let row = [
            Value::Integer(1),
            Value::Text("kim".into()),
            Value::Null,
            Value::Blob(vec![0xab]),
        ];
        assert_eq!(
            encode_row(&row),
            [
                0x01, 0, 0, 0, 0, 0, 0, 0, 1, // INTEGER 1
                0x02, 0, 0, 0, 3, b'k', b'i', b'm', // TEXT 'kim'
                0x00, // NULL
                0x03, 0, 0, 0, 1, 0xab, // BLOB X'ab'
            ]
        );
    }

    #[test]
    fn an_unknown_tag_or_bad_utf8_is_corrupt() {
        assert!(matches!(decode_row(&[0x09]), Err(SqlError::Corrupt(_))));
        assert!(matches!(
            decode_row(&[0x02, 0, 0, 0, 1, 0xFF]),
            Err(SqlError::Corrupt(_))
        ));
        assert!(matches!(
            decode_row(&[0x01, 0, 0]),
            Err(SqlError::Corrupt(_))
        ));
    }
}
