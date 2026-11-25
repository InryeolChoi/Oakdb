//! A session running random statements mixed with BEGIN, COMMIT and
//! ROLLBACK, compared after every statement with a model (issue #35,
//! design 0023, section 10): what the session sees, what is committed,
//! whether a transaction is open, and which statements fail.

use std::collections::BTreeMap;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_sql::{execute, Output, Session, SqlError, TransactionError, Value};
use proptest::prelude::*;

/// `t`'s rows: id to n.
type Table = BTreeMap<i64, Option<i64>>;

#[derive(Debug, Clone)]
enum Op {
    Begin,
    Commit,
    Rollback,
    /// `INSERT INTO t VALUES (id, n), ...`.
    Insert(Vec<(i64, Option<i64>)>),
    /// `UPDATE t SET n = n + add WHERE id BETWEEN from AND to`.
    Update {
        from: i64,
        to: i64,
        add: i64,
    },
    /// `DELETE FROM t WHERE id BETWEEN from AND to`.
    Delete {
        from: i64,
        to: i64,
    },
}

impl Op {
    fn sql(&self) -> String {
        match self {
            Op::Begin => "BEGIN".into(),
            Op::Commit => "COMMIT".into(),
            Op::Rollback => "ROLLBACK".into(),
            Op::Insert(rows) => {
                let rows: Vec<String> = rows
                    .iter()
                    .map(|(id, n)| match n {
                        Some(n) => format!("({id}, {n})"),
                        None => format!("({id}, NULL)"),
                    })
                    .collect();
                format!("INSERT INTO t VALUES {}", rows.join(", "))
            }
            Op::Update { from, to, add } => {
                format!("UPDATE t SET n = n + {add} WHERE id BETWEEN {from} AND {to}")
            }
            Op::Delete { from, to } => format!("DELETE FROM t WHERE id BETWEEN {from} AND {to}"),
        }
    }
}

/// What a statement should give: its output, or the kind of its error.
#[derive(Debug, PartialEq)]
enum Expected {
    Done,
    Affected(u64),
    AlreadyBegun,
    NotBegun,
    DuplicateKey,
    Arithmetic,
}

#[derive(Default)]
struct Model {
    committed: Table,
    /// The open transaction's view of the table.
    open: Option<Table>,
}

impl Model {
    /// Applies `op`; a statement that fails changes nothing.
    fn apply(&mut self, op: &Op) -> Expected {
        match op {
            Op::Begin => {
                if self.open.is_some() {
                    return Expected::AlreadyBegun;
                }
                self.open = Some(self.committed.clone());
                Expected::Done
            }
            Op::Commit => match self.open.take() {
                Some(table) => {
                    self.committed = table;
                    Expected::Done
                }
                None => Expected::NotBegun,
            },
            Op::Rollback => match self.open.take() {
                Some(_) => Expected::Done,
                None => Expected::NotBegun,
            },
            statement => {
                let table = self.open.as_mut().unwrap_or(&mut self.committed);
                match run_statement(table, statement) {
                    Ok((next, count)) => {
                        *table = next;
                        Expected::Affected(count)
                    }
                    Err(e) => e,
                }
            }
        }
    }

    fn seen(&self) -> &Table {
        self.open.as_ref().unwrap_or(&self.committed)
    }
}

/// The table after `statement` and the rows it affected, computed on a
/// copy, so a failure leaves `table` as it was.
fn run_statement(table: &Table, statement: &Op) -> Result<(Table, u64), Expected> {
    let mut next = table.clone();
    match statement {
        Op::Insert(rows) => {
            for (id, n) in rows {
                if next.insert(*id, *n).is_some() {
                    return Err(Expected::DuplicateKey);
                }
            }
            Ok((next, rows.len() as u64))
        }
        Op::Update { from, to, add } => {
            let mut count = 0;
            if from > to {
                return Ok((next, 0));
            }
            for (_, n) in next.range_mut(*from..=*to) {
                count += 1;
                if let Some(value) = n {
                    *value = value.checked_add(*add).ok_or(Expected::Arithmetic)?;
                }
            }
            Ok((next, count))
        }
        Op::Delete { from, to } => {
            if from > to {
                return Ok((next, 0));
            }
            let ids: Vec<i64> = next.range(*from..=*to).map(|(id, _)| *id).collect();
            for id in &ids {
                next.remove(id);
            }
            Ok((next, ids.len() as u64))
        }
        _ => unreachable!("not a statement"),
    }
}

fn kind(result: &Result<Output, SqlError>) -> Expected {
    match result {
        Ok(Output::Done) => Expected::Done,
        Ok(Output::Affected(n)) => Expected::Affected(*n),
        Err(SqlError::Transaction(TransactionError::AlreadyBegun)) => Expected::AlreadyBegun,
        Err(SqlError::Transaction(TransactionError::NotBegun)) => Expected::NotBegun,
        Err(SqlError::DuplicateKey { .. }) => Expected::DuplicateKey,
        Err(SqlError::Arithmetic(_)) => Expected::Arithmetic,
        other => panic!("unexpected {other:?}"),
    }
}

fn read(output: Output) -> Table {
    match output {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Integer(id), Value::Integer(n)) => (*id, Some(*n)),
                (Value::Integer(id), Value::Null) => (*id, None),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn id() -> impl Strategy<Value = i64> {
    0i64..8
}

fn n() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![
        4 => (0i64..5).prop_map(Some),
        1 => Just(Some(i64::MAX - 1)),
        1 => Just(None),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Begin),
        2 => Just(Op::Commit),
        2 => Just(Op::Rollback),
        4 => prop::collection::vec((id(), n()), 1..4).prop_map(Op::Insert),
        3 => (id(), id(), prop_oneof![Just(1i64), Just(2), Just(-1)])
            .prop_map(|(from, to, add)| Op::Update { from, to, add }),
        2 => (id(), id()).prop_map(|(from, to)| Op::Delete { from, to }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn a_session_matches_the_model_after_every_statement(ops in prop::collection::vec(op(), 1..40)) {
        let db = Db::open_with(SimFs::new(), "/oak").unwrap();
        execute(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").unwrap();
        let mut session = Session::new(&db);
        let mut model = Model::default();
        for op in &ops {
            let sql = op.sql();
            let expected = model.apply(op);
            let result = session.execute(&sql);
            prop_assert_eq!(kind(&result), expected, "{}", sql);
            prop_assert_eq!(session.in_transaction(), model.open.is_some(), "{}", sql);
            // What the session sees: its open transaction, or what is
            // committed. A failed statement left it as it was.
            let seen = read(session.execute("SELECT id, n FROM t").unwrap());
            prop_assert_eq!(&seen, model.seen(), "after {}", sql);
            // What is committed, read in a transaction of its own: the
            // open transaction's writes are not there.
            let committed = read(execute(&db, "SELECT id, n FROM t").unwrap());
            prop_assert_eq!(&committed, &model.committed, "after {}", sql);
        }
    }
}
