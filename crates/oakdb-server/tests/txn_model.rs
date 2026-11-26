//! Three connections running random statements, BEGIN, COMMIT, ROLLBACK
//! and disconnects against one server, one step at a time, compared after
//! every step with a model (issue #37, design 0025 Q10): each response
//! (rows, counts, error codes), whether each connection is in a
//! transaction, and what is committed.
//!
//! The model is snapshot isolation with the first committer winning: a
//! transaction sees what was committed at its BEGIN plus its own writes,
//! and its COMMIT fails with 0x20 if a commit after its BEGIN wrote one of
//! the rows it wrote. Steps run one after another, so the order is the
//! model's order.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use oakdb::fs::sim::SimFs;
use oakdb::Db;
use oakdb_server::client::{Client, ClientError};
use oakdb_server::protocol::code;
use oakdb_server::server::{Config, Server};
use oakdb_sql::{Output, Value};
use proptest::prelude::*;

const CONNECTIONS: usize = 3;

/// A table's rows: id to n.
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
    /// `SELECT id, n FROM t`.
    Select,
    /// The connection closes and a new one takes its place.
    Reconnect,
}

impl Op {
    fn sql(&self, t: &str) -> String {
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
                format!("INSERT INTO {t} VALUES {}", rows.join(", "))
            }
            Op::Update { from, to, add } => {
                format!("UPDATE {t} SET n = n + {add} WHERE id BETWEEN {from} AND {to}")
            }
            Op::Delete { from, to } => {
                format!("DELETE FROM {t} WHERE id BETWEEN {from} AND {to}")
            }
            Op::Select => format!("SELECT id, n FROM {t}"),
            Op::Reconnect => unreachable!("not sent"),
        }
    }
}

/// What a statement gives: its output, or its error code.
#[derive(Debug, PartialEq)]
enum Expected {
    Done,
    Affected(u64),
    Rows(Table),
    Error(u8),
}

/// A connection's open transaction.
#[derive(Clone)]
struct Open {
    /// The last commit when it began.
    begun_after: u64,
    /// What it sees: its snapshot with its own writes.
    view: Table,
    /// The rows it wrote.
    wrote: BTreeSet<i64>,
}

#[derive(Default)]
struct Model {
    committed: Table,
    /// Commits so far that wrote anything.
    last: u64,
    /// The last commit that wrote each row.
    written_by: BTreeMap<i64, u64>,
    open: [Option<Open>; CONNECTIONS],
}

impl Model {
    fn apply(&mut self, c: usize, op: &Op) -> Option<Expected> {
        Some(match op {
            Op::Reconnect => {
                self.open[c] = None;
                return None;
            }
            Op::Begin => {
                if self.open[c].is_some() {
                    return Some(Expected::Error(code::TRANSACTION_STATE));
                }
                self.open[c] = Some(Open {
                    begun_after: self.last,
                    view: self.committed.clone(),
                    wrote: BTreeSet::new(),
                });
                Expected::Done
            }
            Op::Rollback => match self.open[c].take() {
                Some(_) => Expected::Done,
                None => Expected::Error(code::TRANSACTION_STATE),
            },
            Op::Commit => match self.open[c].take() {
                None => Expected::Error(code::TRANSACTION_STATE),
                Some(open) => {
                    let lost = open.wrote.iter().any(|id| {
                        self.written_by
                            .get(id)
                            .is_some_and(|&n| n > open.begun_after)
                    });
                    if lost {
                        Expected::Error(code::CONFLICT)
                    } else {
                        self.commit(&open.view, &open.wrote);
                        Expected::Done
                    }
                }
            },
            Op::Select => Expected::Rows(self.seen(c).clone()),
            statement => match run_statement(self.seen(c), statement) {
                Err(code) => Expected::Error(code),
                Ok((next, wrote, count)) => {
                    match &mut self.open[c] {
                        Some(open) => {
                            open.view = next;
                            open.wrote.extend(wrote);
                        }
                        None => self.commit(&next, &wrote.into_iter().collect()),
                    }
                    Expected::Affected(count)
                }
            },
        })
    }

    /// Commits the rows `wrote` as `view` has them.
    fn commit(&mut self, view: &Table, wrote: &BTreeSet<i64>) {
        if wrote.is_empty() {
            return;
        }
        self.last += 1;
        for id in wrote {
            match view.get(id) {
                Some(n) => self.committed.insert(*id, *n),
                None => self.committed.remove(id),
            };
            self.written_by.insert(*id, self.last);
        }
    }

    fn seen(&self, c: usize) -> &Table {
        self.open[c]
            .as_ref()
            .map_or(&self.committed, |open| &open.view)
    }
}

/// The table after `statement`, the rows it wrote and the count it
/// reports; computed on a copy, so a failure changes nothing.
fn run_statement(table: &Table, statement: &Op) -> Result<(Table, Vec<i64>, u64), u8> {
    let mut next = table.clone();
    match statement {
        Op::Insert(rows) => {
            for (id, n) in rows {
                if next.insert(*id, *n).is_some() {
                    return Err(code::DUPLICATE_KEY);
                }
            }
            let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            Ok((next, ids, rows.len() as u64))
        }
        Op::Update { from, to, add } => {
            if from > to {
                return Ok((next, vec![], 0));
            }
            let mut ids = Vec::new();
            for (id, n) in next.range_mut(*from..=*to) {
                ids.push(*id);
                if let Some(value) = n {
                    *value = value.checked_add(*add).ok_or(code::OTHER_STATEMENT)?;
                }
            }
            let count = ids.len() as u64;
            Ok((next, ids, count))
        }
        Op::Delete { from, to } => {
            if from > to {
                return Ok((next, vec![], 0));
            }
            let ids: Vec<i64> = next.range(*from..=*to).map(|(id, _)| *id).collect();
            for id in &ids {
                next.remove(id);
            }
            let count = ids.len() as u64;
            Ok((next, ids, count))
        }
        _ => unreachable!("not a statement"),
    }
}

fn table(rows: Vec<Vec<Value>>) -> Table {
    rows.into_iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(id), Value::Integer(n)) => (*id, Some(*n)),
            (Value::Integer(id), Value::Null) => (*id, None),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn got(result: Result<Output, ClientError>) -> Expected {
    match result {
        Ok(Output::Done) => Expected::Done,
        Ok(Output::Affected(n)) => Expected::Affected(n),
        Ok(Output::Rows { rows, .. }) => Expected::Rows(table(rows)),
        Err(ClientError::Server { code, .. }) => Expected::Error(code),
        Err(e) => panic!("{e}"),
    }
}

/// One server for every case; each case gets a table of its own.
fn server() -> SocketAddr {
    static ADDR: OnceLock<SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        let db = Db::open_with(SimFs::new(), "/oak").unwrap();
        let server = Server::bind(db, Config::default().with(|c| c.port = 0)).unwrap();
        let addr = server.local_addr();
        thread::spawn(move || server.run());
        addr
    })
}

fn connect() -> Client {
    let client = Client::connect(server()).unwrap();
    client.set_timeout(Some(Duration::from_secs(10))).unwrap();
    client
}

fn id() -> impl Strategy<Value = i64> {
    0i64..6
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
        4 => Just(Op::Begin),
        3 => Just(Op::Commit),
        1 => Just(Op::Rollback),
        4 => prop::collection::vec((id(), n()), 1..3).prop_map(Op::Insert),
        4 => (id(), id(), prop_oneof![Just(1i64), Just(2), Just(-1)])
            .prop_map(|(from, to, add)| Op::Update { from, to, add }),
        2 => (id(), id()).prop_map(|(from, to)| Op::Delete { from, to }),
        2 => Just(Op::Select),
        1 => Just(Op::Reconnect),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn three_connections_match_the_model_after_every_step(
        steps in prop::collection::vec((0..CONNECTIONS, op()), 1..48)
    ) {
        static TABLES: AtomicUsize = AtomicUsize::new(0);
        let t = format!("t{}", TABLES.fetch_add(1, Ordering::Relaxed));
        let mut observer = connect();
        observer
            .query(&format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, n INTEGER)"))
            .unwrap();
        let mut clients: Vec<Client> = (0..CONNECTIONS).map(|_| connect()).collect();
        let mut model = Model::default();
        for (c, op) in &steps {
            let c = *c;
            let Some(expected) = model.apply(c, op) else {
                // Closing rolls back whatever was open.
                clients[c] = connect();
                continue;
            };
            let sql = op.sql(&t);
            prop_assert_eq!(got(clients[c].query(&sql)), expected, "connection {}: {}", c, sql);
            for (i, client) in clients.iter().enumerate() {
                prop_assert_eq!(
                    client.in_transaction(),
                    model.open[i].is_some(),
                    "connection {} after {}", i, sql
                );
            }
            let committed = match observer.query(&format!("SELECT id, n FROM {t}")).unwrap() {
                Output::Rows { rows, .. } => table(rows),
                other => panic!("{other:?}"),
            };
            prop_assert_eq!(&committed, &model.committed, "after connection {}: {}", c, sql);
        }
    }
}
