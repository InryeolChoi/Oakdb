//! The executor: runs a parsed statement as one engine transaction
//! (design 0017, section 5).

use oakdb::fs::Fs;
use oakdb::{Db, Txn, MAX_VALUE_LEN};

use crate::catalog::{self, Column, Table};
use crate::encoding::{decode_row, encode_row, row_key};
use crate::parser::{ColumnDef, Statement};
use crate::{Output, SqlError, Value};

/// Runs `statement` in a transaction of its own, committing it if the
/// statement succeeds. On an error the transaction is dropped without
/// committing, which throws away everything the statement wrote: a
/// statement takes effect whole or not at all.
pub(crate) fn run<F: Fs>(db: &Db<F>, statement: Statement) -> Result<Output, SqlError> {
    let mut txn = db.begin();
    let output = match statement {
        Statement::CreateTable { name, columns } => create_table(&mut txn, name, columns)?,
        Statement::Insert {
            table,
            columns,
            rows,
        } => insert(&mut txn, &table, columns, rows)?,
        Statement::Select {
            table,
            columns,
            filter,
        } => select(&txn, &table, columns, filter)?,
    };
    // A SELECT wrote nothing, so its commit writes nothing either.
    txn.commit()?;
    Ok(output)
}

fn create_table<F: Fs>(
    txn: &mut Txn<'_, F>,
    name: String,
    defs: Vec<ColumnDef>,
) -> Result<Output, SqlError> {
    // The catalog stores counts and name lengths in two bytes.
    if defs.len() > usize::from(u16::MAX) {
        return Err(SqlError::Unsupported(format!(
            "a table with {} columns; at most {} are supported",
            defs.len(),
            u16::MAX
        )));
    }
    for (i, def) in defs.iter().enumerate() {
        if def.name.len() > usize::from(u16::MAX) {
            return Err(SqlError::Unsupported(format!(
                "a column name of {} bytes; at most {} are supported",
                def.name.len(),
                u16::MAX
            )));
        }
        if defs[..i].iter().any(|earlier| earlier.name == def.name) {
            return Err(SqlError::DuplicateColumn(def.name.clone()));
        }
    }
    let keys: Vec<usize> = (0..defs.len()).filter(|&i| defs[i].primary_key).collect();
    let primary_key = match keys[..] {
        [primary_key] => primary_key,
        [] => {
            return Err(SqlError::Unsupported(
                "a table without a PRIMARY KEY column; every table needs one".into(),
            ))
        }
        _ => {
            return Err(SqlError::Unsupported(
                "more than one PRIMARY KEY column; only a single-column key is supported".into(),
            ))
        }
    };
    if catalog::find(txn, &name)?.is_some() {
        return Err(SqlError::TableExists(name));
    }
    let table = Table {
        name,
        id: catalog::next_table_id(txn)?,
        columns: defs
            .into_iter()
            .map(|def| Column {
                name: def.name,
                column_type: def.column_type,
            })
            .collect(),
        primary_key,
    };
    catalog::put(txn, &table)?;
    Ok(Output::Done)
}

fn insert<F: Fs>(
    txn: &mut Txn<'_, F>,
    table: &str,
    columns: Option<Vec<String>>,
    rows: Vec<Vec<Value>>,
) -> Result<Output, SqlError> {
    let table = catalog::get(txn, table)?;
    // Which column each value of a row goes to.
    let targets: Vec<usize> = match columns {
        None => (0..table.columns.len()).collect(),
        Some(names) => {
            let mut targets = Vec::with_capacity(names.len());
            for name in &names {
                let column = table
                    .column(name)
                    .ok_or_else(|| table.no_such_column(name))?;
                if targets.contains(&column) {
                    return Err(SqlError::DuplicateColumn(name.clone()));
                }
                targets.push(column);
            }
            targets
        }
    };
    let count = rows.len();
    for values in rows {
        if values.len() != targets.len() {
            return Err(SqlError::ValueCount {
                expected: targets.len(),
                found: values.len(),
            });
        }
        // Columns the INSERT does not name are NULL.
        let mut row = vec![Value::Null; table.columns.len()];
        for (&column, value) in targets.iter().zip(values) {
            row[column] = value;
        }
        check_row(&table, &row)?;
        let key = row_key(table.id, &row[table.primary_key]);
        // The transaction sees its own writes, so this also catches a key
        // that an earlier row of the same INSERT used.
        if txn.get(&key)?.is_some() {
            return Err(SqlError::DuplicateKey {
                table: table.name.clone(),
            });
        }
        txn.put(&key, &encode_row(&row))?;
    }
    Ok(Output::Affected(count as u64))
}

/// Checks that each value fits its column: of the column's type or NULL,
/// and never NULL in the primary key. Also refuses a value the row
/// encoding cannot hold, which the engine would refuse anyway.
fn check_row(table: &Table, row: &[Value]) -> Result<(), SqlError> {
    for (i, (column, value)) in table.columns.iter().zip(row).enumerate() {
        let found = value.column_type();
        let fits = match found {
            None => i != table.primary_key,
            Some(found) => found == column.column_type,
        };
        if !fits {
            return Err(SqlError::TypeMismatch {
                column: column.name.clone(),
                expected: column.column_type,
                found,
            });
        }
        let len = match value {
            Value::Text(text) => text.len(),
            Value::Blob(bytes) => bytes.len(),
            Value::Null | Value::Integer(_) => 0,
        };
        if len > MAX_VALUE_LEN {
            return Err(SqlError::Engine(oakdb::Error::ValueTooLarge {
                len,
                max: MAX_VALUE_LEN,
            }));
        }
    }
    Ok(())
}

fn select<F: Fs>(
    txn: &Txn<'_, F>,
    table: &str,
    columns: Option<Vec<String>>,
    (filter_column, filter_value): (String, Value),
) -> Result<Output, SqlError> {
    let table = catalog::get(txn, table)?;
    let selected: Vec<usize> = match &columns {
        None => (0..table.columns.len()).collect(),
        Some(names) => names
            .iter()
            .map(|name| table.column(name).ok_or_else(|| table.no_such_column(name)))
            .collect::<Result<_, _>>()?,
    };
    let filter = table
        .column(&filter_column)
        .ok_or_else(|| table.no_such_column(&filter_column))?;
    if filter != table.primary_key {
        return Err(SqlError::Unsupported(format!(
            "WHERE on `{filter_column}`, which is not the primary key; \
             only `<primary key> = <value>` is supported"
        )));
    }
    let key_column = &table.columns[table.primary_key];
    let rows = match filter_value.column_type() {
        // `= NULL` is never true in SQL, so nothing matches.
        None => Vec::new(),
        Some(found) if found != key_column.column_type => {
            return Err(SqlError::TypeMismatch {
                column: key_column.name.clone(),
                expected: key_column.column_type,
                found: Some(found),
            })
        }
        Some(_) => match txn.get(&row_key(table.id, &filter_value))? {
            None => Vec::new(),
            Some(bytes) => {
                let row = decode_row(&bytes)?;
                if row.len() != table.columns.len() {
                    return Err(SqlError::Corrupt(format!(
                        "a row of {} holds {} values for {} columns",
                        table.name,
                        row.len(),
                        table.columns.len()
                    )));
                }
                vec![selected.iter().map(|&i| row[i].clone()).collect()]
            }
        },
    };
    Ok(Output::Rows {
        columns: selected
            .iter()
            .map(|&i| table.columns[i].name.clone())
            .collect(),
        rows,
    })
}
