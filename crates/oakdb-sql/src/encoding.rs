//! How SQL values become engine keys and values (design 0017, section 2).
//!
//! The engine sorts keys bytewise, so a primary key is encoded so that
//! comparing the bytes gives the same answer as comparing the values
//! (C4): that is what lets a range of primary keys (#32) be one scan.
//! Row values never get compared, so they are encoded only to be read
//! back (C6).

use crate::{ColumnType, SqlError, Value};

/// First byte of every catalog key: `[0x00]` alone holds the next table
/// id, `[0x00][table name]` holds a table's description.
pub(crate) const CATALOG_PREFIX: u8 = 0x00;

/// First byte of every row key: `[0x01][table id][primary key]`.
pub(crate) const ROW_PREFIX: u8 = 0x01;

/// Appends `value`, a primary key, to `out` so that keys sort as values
/// do. Integers sort among integers, and text and blobs bytewise among
/// themselves; a table's primary key holds one type, so values of
/// different types are never compared. NULL is never a primary key.
///
/// - An integer is its 8 big-endian bytes with the sign bit flipped. In
///   two's complement a negative number starts with a 1 bit, so left alone
///   it would sort after every positive one; flipped, negatives start with
///   0 and positives with 1, and within each the bytes already sort.
/// - Text and blobs are their bytes, with each 0x00 written as `00 FF`,
///   then `00 00` to end. The end sorts below any byte the value could go
///   on with (`00 00` < `00 FF` < `01`), so a value sorts before every
///   longer value it starts. The key ends here today, so the end marker is
///   not yet needed for order, but it keeps order once something follows
///   the key, as in a secondary index.
pub(crate) fn encode_key(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Integer(i) => out.extend_from_slice(&(*i as u64 ^ (1 << 63)).to_be_bytes()),
        Value::Text(text) => escape(text.as_bytes(), out),
        Value::Blob(bytes) => escape(bytes, out),
        // The executor rejects a NULL primary key before it makes a key,
        // and `WHERE pk = NULL` finds nothing without looking.
        Value::Null => unreachable!("NULL is never a primary key"),
    }
}

/// Appends `bytes` with 0x00 escaped as `00 FF`, then the end `00 00`.
fn escape(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        out.push(b);
        if b == 0x00 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0x00, 0x00]);
}

/// The key of the row of table `table_id` whose primary key is `key`.
///
/// The table id is big-endian, so all the rows of one table sit together
/// in key order, before every row of the next table.
pub(crate) fn row_key(table_id: u32, key: &Value) -> Vec<u8> {
    let mut out = vec![ROW_PREFIX];
    out.extend_from_slice(&table_id.to_be_bytes());
    encode_key(key, &mut out);
    out
}

/// Tag byte of each value type in a row, and of each column type in the
/// catalog.
pub(crate) const TAG_NULL: u8 = 0x00;
pub(crate) const TAG_INTEGER: u8 = 0x01;
pub(crate) const TAG_TEXT: u8 = 0x02;
pub(crate) const TAG_BLOB: u8 = 0x03;

/// The column type `tag` stands for in the catalog, or `None` for a tag
/// that is no column type (NULL's, or none at all).
pub(crate) fn column_type_of(tag: u8) -> Option<ColumnType> {
    match tag {
        TAG_INTEGER => Some(ColumnType::Integer),
        TAG_TEXT => Some(ColumnType::Text),
        TAG_BLOB => Some(ColumnType::Blob),
        _ => None,
    }
}

/// The tag of `column_type`: the same byte a value of that type has in a
/// row.
pub(crate) fn tag_of(column_type: ColumnType) -> u8 {
    match column_type {
        ColumnType::Integer => TAG_INTEGER,
        ColumnType::Text => TAG_TEXT,
        ColumnType::Blob => TAG_BLOB,
    }
}

/// Encodes a row's values, one per column in column order: a tag byte,
/// then an integer as 8 big-endian bytes, or text or a blob as a 4-byte
/// big-endian length and the bytes, or nothing for NULL.
///
/// A text or blob longer than that length field holds (`u32::MAX` bytes,
/// the engine's [`oakdb::MAX_VALUE_LEN`] too) is refused here, where the
/// field is: the engine would refuse the row anyway, but encoding comes
/// first.
pub(crate) fn encode_row(values: &[Value]) -> Result<Vec<u8>, SqlError> {
    let mut out = Vec::new();
    for value in values {
        match value {
            Value::Null => out.push(TAG_NULL),
            Value::Integer(i) => {
                out.push(TAG_INTEGER);
                out.extend_from_slice(&i.to_be_bytes());
            }
            Value::Text(text) => {
                out.push(TAG_TEXT);
                put_bytes(text.as_bytes(), &mut out)?;
            }
            Value::Blob(bytes) => {
                out.push(TAG_BLOB);
                put_bytes(bytes, &mut out)?;
            }
        }
    }
    Ok(out)
}

/// Appends a 4-byte big-endian length and `bytes`, or refuses bytes too
/// long for the length field.
fn put_bytes(bytes: &[u8], out: &mut Vec<u8>) -> Result<(), SqlError> {
    let len = u32::try_from(bytes.len()).map_err(|_| {
        SqlError::Engine(oakdb::Error::ValueTooLarge {
            len: bytes.len(),
            max: u32::MAX as usize,
        })
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Decodes a row that [`encode_row`] encoded.
pub(crate) fn decode_row(bytes: &[u8]) -> Result<Vec<Value>, SqlError> {
    let mut reader = Reader::new(bytes, "row");
    let mut values = Vec::new();
    while !reader.is_empty() {
        let value = match reader.u8()? {
            TAG_NULL => Value::Null,
            TAG_INTEGER => Value::Integer(i64::from_be_bytes(reader.array()?)),
            TAG_TEXT => {
                let len = u32::from_be_bytes(reader.array()?) as usize;
                Value::Text(reader.text(len)?)
            }
            TAG_BLOB => {
                let len = u32::from_be_bytes(reader.array()?) as usize;
                Value::Blob(reader.bytes(len)?.to_vec())
            }
            tag => return Err(reader.corrupt(&format!("unknown value tag {tag:#04x}"))),
        };
        values.push(value);
    }
    Ok(values)
}

/// Reads stored bytes front to back, turning a short or malformed input
/// into [`SqlError::Corrupt`] instead of a panic: what is in the engine
/// may have been written by a broken build.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    /// What is being read, for the error.
    what: &'static str,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Reader { bytes, what }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The next `len` bytes.
    pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], SqlError> {
        if self.bytes.len() < len {
            return Err(self.corrupt("it ends too soon"));
        }
        let (taken, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(taken)
    }

    /// The next `N` bytes, as an array for `from_be_bytes`.
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], SqlError> {
        let bytes = self.bytes(N)?;
        Ok(bytes.try_into().expect("bytes returned N bytes"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8, SqlError> {
        Ok(self.array::<1>()?[0])
    }

    /// The next `len` bytes, which must be UTF-8.
    pub(crate) fn text(&mut self, len: usize) -> Result<String, SqlError> {
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| self.corrupt("text is not UTF-8"))
    }

    pub(crate) fn corrupt(&self, reason: &str) -> SqlError {
        SqlError::Corrupt(format!("{}: {reason}", self.what))
    }
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
            prop_assert_eq!(decode_row(&encode_row(&row).unwrap()).unwrap(), row);
        }

        #[test]
        fn a_cut_row_is_corrupt_not_a_panic(
            row in prop::collection::vec(any_value(), 1..6),
            cut in any::<prop::sample::Index>(),
        ) {
            let bytes = encode_row(&row).unwrap();
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
            encode_row(&row).unwrap(),
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
