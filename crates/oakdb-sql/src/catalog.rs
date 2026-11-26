//! The catalog: what tables exist and what columns they have, kept in the
//! engine under keys that start with [`CATALOG_PREFIX`] (design 0017,
//! sections 1 and 2-3).
//!
//! It is read from the engine at every statement, inside the statement's
//! transaction, rather than cached: a cache would need rules for when it
//! goes stale once statements can share a transaction (#35).

use oakdb::fs::Fs;
use oakdb::Txn;

use crate::encoding::{column_type_of, tag_of, Reader, CATALOG_PREFIX};
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

    /// Where each column `names` lists is, in the same order: an INSERT's
    /// column list or the columns an UPDATE sets. A name the table does not
    /// have, or one named twice, is an error.
    pub(crate) fn resolve_columns<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<usize>, SqlError> {
        let mut columns: Vec<usize> = Vec::new();
        for name in names {
            let column = self.column(name).ok_or_else(|| self.no_such_column(name))?;
            if columns.contains(&column) {
                return Err(SqlError::DuplicateColumn(name.to_owned()));
            }
            columns.push(column);
        }
        Ok(columns)
    }

    /// Whether a value of type `found` (`None` for NULL) fits column
    /// `column`: of its type, or NULL anywhere but the primary key. If not,
    /// the error says so: a NULL key is [`SqlError::NullKey`], a constraint,
    /// and a value of another type is [`SqlError::TypeMismatch`] (design
    /// 0027 E3).
    pub(crate) fn check_fits(
        &self,
        column: usize,
        found: Option<ColumnType>,
    ) -> Result<(), SqlError> {
        let expected = self.columns[column].column_type;
        match found {
            None if column == self.primary_key => Err(SqlError::NullKey {
                column: self.columns[column].name.clone(),
            }),
            Some(found) if found != expected => Err(SqlError::TypeMismatch {
                column: self.columns[column].name.clone(),
                expected,
                found: Some(found),
            }),
            _ => Ok(()),
        }
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
    let mut key = vec![CATALOG_PREFIX];
    key.extend_from_slice(name.as_bytes());
    key
}

/// The key of the counter that holds the next table id: the catalog prefix
/// alone. No table description can have this key, as a table name is
/// never empty.
const NEXT_TABLE_ID_KEY: [u8; 1] = [CATALOG_PREFIX];

/// Encodes a table's description, all but its name, which is in the key:
/// the table id (u32), the column count (u16), the primary key's column
/// (u16), then per column its name's length (u16), the name, and its type
/// tag, all big-endian.
///
/// Panics past 65535 columns or a 65535-byte column name; CREATE TABLE
/// refuses those first.
pub(crate) fn encode(table: &Table) -> Vec<u8> {
    let u16_of = |n: usize| u16::try_from(n).expect("CREATE TABLE checks the limits");
    let mut out = Vec::new();
    out.extend_from_slice(&table.id.to_be_bytes());
    out.extend_from_slice(&u16_of(table.columns.len()).to_be_bytes());
    out.extend_from_slice(&u16_of(table.primary_key).to_be_bytes());
    for column in &table.columns {
        out.extend_from_slice(&u16_of(column.name.len()).to_be_bytes());
        out.extend_from_slice(column.name.as_bytes());
        out.push(tag_of(column.column_type));
    }
    out
}

/// Decodes the description of table `name`.
pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<Table, SqlError> {
    let mut reader = Reader::new(bytes, "table description");
    let id = u32::from_be_bytes(reader.array()?);
    let count = u16::from_be_bytes(reader.array()?) as usize;
    let primary_key = u16::from_be_bytes(reader.array()?) as usize;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u16::from_be_bytes(reader.array()?) as usize;
        let name = reader.text(len)?;
        let tag = reader.u8()?;
        let column_type = column_type_of(tag)
            .ok_or_else(|| reader.corrupt(&format!("unknown column type {tag:#04x}")))?;
        columns.push(Column { name, column_type });
    }
    if !reader.is_empty() {
        return Err(reader.corrupt("bytes left after the last column"));
    }
    if primary_key >= columns.len() {
        return Err(reader.corrupt("the primary key is not one of the columns"));
    }
    Ok(Table {
        name: name.to_owned(),
        id,
        columns,
        primary_key,
    })
}

/// Table `name`, if it exists.
pub(crate) fn find<F: Fs>(txn: &Txn<'_, F>, name: &str) -> Result<Option<Table>, SqlError> {
    match txn.get(&table_key(name))? {
        None => Ok(None),
        Some(bytes) => decode(name, &bytes).map(Some),
    }
}

/// Table `name`, or [`SqlError::NoSuchTable`].
pub(crate) fn get<F: Fs>(txn: &Txn<'_, F>, name: &str) -> Result<Table, SqlError> {
    find(txn, name)?.ok_or_else(|| SqlError::NoSuchTable(name.to_owned()))
}

/// Stores a new table's description.
pub(crate) fn put<F: Fs>(txn: &mut Txn<'_, F>, table: &Table) -> Result<(), SqlError> {
    txn.put(&table_key(&table.name), &encode(table))?;
    Ok(())
}

/// Takes the next table id, moving the counter on. Ids start at 1. A
/// CREATE TABLE that fails throws the counter's move away along with the
/// table, so its id goes to the next table; no row was ever stored under
/// it.
pub(crate) fn next_table_id<F: Fs>(txn: &mut Txn<'_, F>) -> Result<u32, SqlError> {
    let id = match txn.get(&NEXT_TABLE_ID_KEY)? {
        None => 1,
        Some(bytes) => u32::from_be_bytes(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SqlError::Corrupt("table id counter: not 4 bytes".into()))?,
        ),
    };
    let next = id
        .checked_add(1)
        .ok_or_else(|| SqlError::Unsupported("more than 4294967294 tables".into()))?;
    txn.put(&NEXT_TABLE_ID_KEY, &next.to_be_bytes())?;
    Ok(id)
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
