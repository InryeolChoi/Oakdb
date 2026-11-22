//! The catalog: what tables exist and what columns they have, kept in the
//! engine under keys that start with [`CATALOG_PREFIX`] (design 0017,
//! sections 1 and 2-3).
//!
//! It is read from the engine at every statement, inside the statement's
//! transaction, rather than cached: a cache would need rules for when it
//! goes stale once statements can share a transaction (#35).

use oakdb::fs::Fs;
use oakdb::Txn;

use crate::encoding::CATALOG_PREFIX;
use crate::{ColumnType, SqlError};

/// A table's description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    /// The name, as written in CREATE TABLE.
    pub(crate) name: String,
    /// The number its rows are stored under, so row keys stay short and
    /// do not depend on the name.
    pub(crate) id: u32,
    /// The columns, in the order of CREATE TABLE.
    pub(crate) columns: Vec<Column>,
    /// Which of `columns` is the primary key.
    pub(crate) primary_key: usize,
}

/// A column of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) column_type: ColumnType,
}

impl Table {
    /// Where column `name` is, if the table has it.
    pub(crate) fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// The error for a column the table does not have.
    pub(crate) fn no_such_column(&self, name: &str) -> SqlError {
        SqlError::NoSuchColumn {
            table: self.name.clone(),
            column: name.to_owned(),
        }
    }
}

/// The key of table `name`'s description.
pub(crate) fn table_key(name: &str) -> Vec<u8> {
    let _ = name;
    unimplemented!("catalog key")
}

/// Encodes a table's description, all but its name, which is in the key.
pub(crate) fn encode(table: &Table) -> Vec<u8> {
    let _ = table;
    unimplemented!("catalog encoding")
}

/// Decodes the description of table `name`.
pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<Table, SqlError> {
    let _ = (name, bytes);
    unimplemented!("catalog decoding")
}

/// Table `name`, if it exists.
pub(crate) fn find<F: Fs>(txn: &Txn<'_, F>, name: &str) -> Result<Option<Table>, SqlError> {
    let _ = (txn, name);
    unimplemented!("catalog lookup")
}

/// Takes the next table id, moving the counter on.
pub(crate) fn next_table_id<F: Fs>(txn: &mut Txn<'_, F>) -> Result<u32, SqlError> {
    let _ = txn;
    unimplemented!("table ids")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Table {
        Table {
            name: "users".into(),
            id: 1,
            columns: vec![
                Column {
                    name: "id".into(),
                    column_type: ColumnType::Integer,
                },
                Column {
                    name: "name".into(),
                    column_type: ColumnType::Text,
                },
                Column {
                    name: "photo".into(),
                    column_type: ColumnType::Blob,
                },
            ],
            primary_key: 0,
        }
    }

    #[test]
    fn a_table_reads_back_as_it_was_written() {
        let table = users();
        assert_eq!(decode("users", &encode(&table)).unwrap(), table);
    }

    #[test]
    fn a_table_is_encoded_as_design_0017_shows() {
        assert_eq!(
            encode(&users()),
            [
                0, 0, 0, 1, 0, 3, 0, 0, //
                0, 2, b'i', b'd', 1, //
                0, 4, b'n', b'a', b'm', b'e', 2, //
                0, 5, b'p', b'h', b'o', b't', b'o', 3,
            ]
        );
        assert_eq!(table_key("users"), b"\0users");
    }

    #[test]
    fn a_cut_description_is_corrupt_not_a_panic() {
        let bytes = encode(&users());
        for len in 0..bytes.len() {
            assert!(matches!(
                decode("users", &bytes[..len]),
                Err(SqlError::Corrupt(_))
            ));
        }
    }
}
