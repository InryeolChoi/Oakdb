//! The executor: runs a parsed statement as one engine transaction
//! (design 0017, section 5).

use std::collections::HashSet;

use oakdb::fs::Fs;
use oakdb::{Db, Txn};

use crate::catalog::{self, Column, Table};
use crate::encoding::{decode_row, encode_row, row_key, row_range};
use crate::expr::{Expr, Truth};
use crate::parser::{ColumnDef, Statement};
use crate::planner::{self, Assignment, Plan};
use crate::{Output, SqlError, Value};

/// Runs `statement` in a transaction of its own, committing it if the
/// statement succeeds. On an error the transaction is dropped without
/// committing, which throws away everything the statement wrote: a
/// statement takes effect whole or not at all.
pub(crate) fn run<F: Fs>(db: &Db<F>, statement: Statement) -> Result<Output, SqlError> {
    let mut txn = db.begin();
    let output = apply(&mut txn, statement)?;
    // A SELECT wrote nothing, so its commit writes nothing either.
    txn.commit()?;
    Ok(output)
}

/// Runs `statement` inside `txn`, which the caller commits or drops.
///
/// A statement that fails leaves `txn` as it was (design 0023, T5): each
/// one checks everything that can fail before it writes, so that inside
/// BEGIN only the failed statement is undone. The writes themselves can
/// then fail only if the engine stopped, and a stopped engine commits
/// nothing.
pub(crate) fn apply<F: Fs>(txn: &mut Txn<'_, F>, statement: Statement) -> Result<Output, SqlError> {
    Ok(match statement {
        Statement::CreateTable { name, columns } => create_table(txn, name, columns)?,
        Statement::Insert {
            table,
            columns,
            rows,
        } => insert(txn, &table, columns, rows)?,
        Statement::Select {
            table,
            columns,
            filter,
        } => select(txn, &table, columns, filter)?,
        Statement::Update {
            table,
            assignments,
            filter,
        } => update(txn, &table, assignments, filter)?,
        Statement::Delete { table, filter } => delete(txn, &table, filter)?,
    })
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
        Some(names) => table.resolve_columns(names.iter().map(String::as_str))?,
    };
    // Step 1: every row is checked before any is written, so a row that
    // fails leaves none of the statement's rows behind (design 0023, T5).
    let count = rows.len();
    let mut checked: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(count);
    let mut keys: HashSet<Vec<u8>> = HashSet::with_capacity(count);
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
        let value = encode_row(&row)?;
        check_fits(&key, &value)?;
        // The rows of this INSERT are not written yet, so `keys` catches
        // one it adds twice.
        if txn.get(&key)?.is_some() || !keys.insert(key.clone()) {
            return Err(SqlError::DuplicateKey {
                table: table.name.clone(),
            });
        }
        checked.push((key, value));
    }
    // Step 2: nothing is left to fail but the engine itself.
    for (key, value) in &checked {
        txn.put(key, value)?;
    }
    Ok(Output::Affected(count as u64))
}

/// Checks a row's key and encoded value against the engine's size rules
/// before anything is written, so that the writes after cannot fail on
/// them (design 0023 T5, design 0022 U10). INSERT and UPDATE share it.
fn check_fits(key: &[u8], value: &[u8]) -> Result<(), SqlError> {
    oakdb::check_key(key)?;
    oakdb::check_value_len(value.len())?;
    Ok(())
}

/// Checks that each value fits its column: of the column's type or NULL,
/// and never NULL in the primary key. Also refuses a value the row
/// encoding cannot hold, which the engine would refuse anyway.
fn check_row(table: &Table, row: &[Value]) -> Result<(), SqlError> {
    for (column, value) in row.iter().enumerate() {
        table.check_fits(column, value.column_type())?;
    }
    Ok(())
}

/// Scans the primary keys the plan's range holds (design 0019) and keeps
/// the rows its filter makes true. `WHERE pk = v` is a range of one key,
/// so it runs the same way.
fn select<F: Fs>(
    txn: &Txn<'_, F>,
    table: &str,
    columns: Option<Vec<String>>,
    filter: Option<Expr>,
) -> Result<Output, SqlError> {
    let table = catalog::get(txn, table)?;
    let selected: Vec<usize> = match &columns {
        None => (0..table.columns.len()).collect(),
        Some(names) => names
            .iter()
            .map(|name| table.column(name).ok_or_else(|| table.no_such_column(name)))
            .collect::<Result<_, _>>()?,
    };
    // Unknown columns and type errors fail here, before any row is read.
    let plan = planner::plan(&table, filter)?;
    let rows = matching(txn, &table, &plan)?
        .into_iter()
        .map(|matched| selected.iter().map(|&i| matched.row[i].clone()).collect())
        .collect();
    Ok(Output::Rows {
        columns: selected
            .iter()
            .map(|&i| table.columns[i].name.clone())
            .collect(),
        rows,
    })
}

/// Changes the rows WHERE matches (design 0022), in two steps. The first
/// reads every matched row, computes its new value from the old one and
/// checks everything that can fail; the second only writes. Collecting
/// first is what the borrows ask anyway (a scan borrows the transaction
/// shared, a write exclusively), and it changes each row once even when
/// its new primary key lies ahead of the scan (the Halloween problem,
/// section 1).
fn update<F: Fs>(
    txn: &mut Txn<'_, F>,
    table: &str,
    assignments: Vec<(String, Expr)>,
    filter: Option<Expr>,
) -> Result<Output, SqlError> {
    let table = catalog::get(txn, table)?;
    // Unknown columns and type errors fail here, before any row is read.
    let assignments = planner::assignments(&table, assignments)?;
    let plan = planner::plan(&table, filter)?;
    let matched = matching(txn, &table, &plan)?;

    // Step 1: the new rows, each computed from its old row only (U7).
    let mut changes = Vec::with_capacity(matched.len());
    for Matched { key, row: old } in &matched {
        let new = assign(&assignments, old)?;
        check_row(&table, &new)?;
        let new_key = row_key(table.id, &new[table.primary_key]);
        let value = encode_row(&new)?;
        check_fits(&new_key, &value)?;
        changes.push(Change {
            key: key.as_slice(),
            new_key,
            value,
        });
    }
    no_duplicate_keys(txn, &table, &changes)?;

    // Step 2: nothing is left to fail but the engine itself (U10). The
    // rows that move leave their old keys first, so that one may move
    // onto a key another one leaves.
    for change in &changes {
        if change.moves() {
            txn.delete(change.key)?;
        }
    }
    for change in &changes {
        txn.put(&change.new_key, &change.value)?;
    }
    Ok(Output::Affected(matched.len() as u64))
}

/// What UPDATE does to one matched row.
struct Change<'a> {
    /// The engine key the row was read under.
    key: &'a [u8],
    /// The key its new primary key gives.
    new_key: Vec<u8>,
    /// The new row, encoded.
    value: Vec<u8>,
}

impl Change<'_> {
    /// Whether the row's primary key changes, so that it moves.
    fn moves(&self) -> bool {
        self.key != self.new_key.as_slice()
    }
}

/// The row `old` becomes under the SET list: every expression reads the
/// old row, so `SET a = b, b = a` swaps them.
fn assign(assignments: &[Assignment], old: &[Value]) -> Result<Vec<Value>, SqlError> {
    let mut new = old.to_vec();
    for Assignment { column, value } in assignments {
        new[*column] = value.eval(old)?.into_owned();
    }
    Ok(new)
}

/// Checks the primary keys after an UPDATE against the table as it will
/// be once the whole statement has run (design 0022, section 3-1), not
/// row by row: `SET id = id + 1` on 1, 2 and 3 gives 2, 3 and 4 and is
/// fine, whatever order the rows move in. A new key is a duplicate if
/// another changed row gets it too, or if a row stays under it: one the
/// statement did not match, or one it did that keeps its key.
fn no_duplicate_keys<F: Fs>(
    txn: &Txn<'_, F>,
    table: &Table,
    changes: &[Change<'_>],
) -> Result<(), SqlError> {
    let leaving: HashSet<&[u8]> = changes
        .iter()
        .filter(|change| change.moves())
        .map(|change| change.key)
        .collect();
    let mut taken: HashSet<&[u8]> = HashSet::with_capacity(changes.len());
    for change in changes {
        let new_key = change.new_key.as_slice();
        // A row that keeps its key stays where it is: only another row
        // getting the same key can clash with it, which `taken` sees.
        let duplicate = !taken.insert(new_key)
            || (change.moves() && !leaving.contains(new_key) && txn.get(new_key)?.is_some());
        if duplicate {
            return Err(SqlError::DuplicateKey {
                table: table.name.clone(),
            });
        }
    }
    Ok(())
}

/// Deletes the rows WHERE matches, collected first as UPDATE does.
fn delete<F: Fs>(
    txn: &mut Txn<'_, F>,
    table: &str,
    filter: Option<Expr>,
) -> Result<Output, SqlError> {
    let table = catalog::get(txn, table)?;
    let plan = planner::plan(&table, filter)?;
    let matched = matching(txn, &table, &plan)?;
    for Matched { key, .. } in &matched {
        txn.delete(key)?;
    }
    Ok(Output::Affected(matched.len() as u64))
}

/// A row WHERE matched.
struct Matched {
    /// The engine key it was read under.
    key: Vec<u8>,
    /// Its values, one per column.
    row: Vec<Value>,
}

/// The rows in the plan's range that its filter makes true, each with the
/// engine key it was read under, in key order. UPDATE and DELETE write to
/// that key rather than one made again from the row, so that a row whose
/// stored key and primary key disagree is never left behind. They are all
/// held in memory at once (design 0022, section 1-1).
fn matching<F: Fs>(txn: &Txn<'_, F>, table: &Table, plan: &Plan) -> Result<Vec<Matched>, SqlError> {
    let mut rows = Vec::new();
    for entry in txn.scan(row_range(table.id, plan.range.clone())) {
        let (key, bytes) = entry?;
        let row = decode_row(&bytes)?;
        // The filter reads columns by position, so a short row would be
        // read out of bounds.
        if row.len() != table.columns.len() {
            return Err(SqlError::Corrupt(format!(
                "a row of {} holds {} values for {} columns",
                table.name,
                row.len(),
                table.columns.len()
            )));
        }
        if let Some(filter) = &plan.filter {
            if filter.eval(&row)? != Truth::True {
                continue;
            }
        }
        rows.push(Matched { key, row });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use oakdb::fs::sim::SimFs;

    use super::*;
    use crate::execute;
    use crate::parser::{parse_command, Command};

    fn parsed(sql: &str) -> Statement {
        match parse_command(sql).unwrap() {
            Command::Run(statement) => statement,
            other => panic!("{other:?} is not a statement"),
        }
    }

    /// An UPDATE writes every row it matches, even one whose value stays
    /// the same (design 0022, U9), so a transaction that began before it
    /// and writes the same row loses at commit; and the loser's conflict
    /// reaches SQL as the engine's conflict error (U11).
    #[test]
    fn an_update_conflicts_with_a_transaction_writing_the_same_row() {
        let db = Db::open_with(SimFs::new(), "/oak").unwrap();
        execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
        execute(&db, "INSERT INTO t VALUES (1, 0), (2, 0)").unwrap();

        let mut earlier = db.begin();
        let statement = parsed("UPDATE t SET n = n + 1 WHERE id = 1");
        assert_eq!(apply(&mut earlier, statement).unwrap(), Output::Affected(1));
        // Another statement commits a write to the same row first.
        execute(&db, "UPDATE t SET n = n WHERE id = 1").unwrap();
        let e = SqlError::from(earlier.commit().unwrap_err());
        assert!(
            matches!(e, SqlError::Engine(oakdb::Error::Conflict { .. })),
            "{e:?}"
        );

        // Different rows do not conflict.
        let mut earlier = db.begin();
        let statement = parsed("DELETE FROM t WHERE id = 2");
        apply(&mut earlier, statement).unwrap();
        execute(&db, "UPDATE t SET n = 5 WHERE id = 1").unwrap();
        earlier.commit().unwrap();
    }
}
