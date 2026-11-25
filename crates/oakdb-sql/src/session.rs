//! Sessions: BEGIN, COMMIT and ROLLBACK (issue #35, design 0023).

use oakdb::fs::{Fs, StdFs};
use oakdb::{Db, Txn};

use crate::error::TransactionError;
use crate::executor;
use crate::parser::{self, Command};
use crate::{Output, SqlError};

/// Runs SQL statements one after another, and keeps a transaction open
/// between them from BEGIN to COMMIT or ROLLBACK. Outside a transaction
/// each statement commits on its own (autocommit), as [`crate::execute`]
/// does. A server keeps one session per connection.
///
/// It borrows the database, so the borrow checker sees that it, and the
/// transaction in it, end before the database does (design 0023, T1).
///
/// Only COMMIT and ROLLBACK end a transaction (T6). A statement that fails
/// inside one is undone alone and the transaction stays open (T5). COMMIT
/// ends it even when it fails: on [`SqlError::TransactionConflict`]
/// nothing was written and the transaction has to run again from BEGIN. Dropping the
/// session rolls an open transaction back (T10).
///
/// ```
/// use oakdb::{fs::sim::SimFs, Db};
/// use oakdb_sql::{execute, Output, Session};
///
/// let db = Db::open_with(SimFs::new(), "/db").unwrap();
/// let mut session = Session::new(&db);
/// session.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
/// session.execute("BEGIN").unwrap();
/// session.execute("INSERT INTO t VALUES (1)").unwrap();
/// // Not committed yet: a statement of its own does not see the row.
/// let out = execute(&db, "SELECT * FROM t").unwrap();
/// assert_eq!(out, Output::Rows { columns: vec!["id".into()], rows: vec![] });
/// session.execute("COMMIT").unwrap();
/// ```
pub struct Session<'db, F: Fs = StdFs> {
    db: &'db Db<F>,
    /// The transaction BEGIN opened, until COMMIT or ROLLBACK.
    txn: Option<Txn<'db, F>>,
}

impl<'db, F: Fs> Session<'db, F> {
    /// A session on `db`, with no transaction open.
    pub fn new(db: &'db Db<F>) -> Self {
        Session { db, txn: None }
    }

    /// Runs one statement or transaction command, with an optional
    /// trailing semicolon. BEGIN, COMMIT and ROLLBACK return
    /// [`Output::Done`]; one that does not fit the session's state fails
    /// with [`SqlError::Transaction`] and leaves the open transaction, if
    /// any, as it was.
    pub fn execute(&mut self, sql: &str) -> Result<Output, SqlError> {
        match parser::parse_command(sql)? {
            Command::Begin => {
                if self.txn.is_some() {
                    return Err(SqlError::Transaction(TransactionError::AlreadyBegun));
                }
                // A stopped engine can commit nothing, so BEGIN says so
                // at once, as every statement after it would (design 0023,
                // section 5).
                self.db.check_running()?;
                // The transaction reads the database as committed now
                // (T7).
                self.txn = Some(self.db.begin());
                Ok(Output::Done)
            }
            Command::Commit => {
                let txn = self.take_open()?;
                txn.commit().map_err(|e| match e {
                    // All of it, from BEGIN, has to run again (5-2).
                    e @ oakdb::Error::Conflict { .. } => SqlError::TransactionConflict(e),
                    e => SqlError::Engine(e),
                })?;
                Ok(Output::Done)
            }
            Command::Rollback => {
                self.take_open()?.rollback();
                Ok(Output::Done)
            }
            Command::Run(statement) => match &mut self.txn {
                Some(txn) => executor::apply(txn, statement),
                None => executor::run(self.db, statement),
            },
        }
    }

    /// Whether a transaction is open: BEGIN ran, and COMMIT or ROLLBACK
    /// has not yet.
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Takes the open transaction out of the session, which ends it there
    /// whatever becomes of it.
    fn take_open(&mut self) -> Result<Txn<'db, F>, SqlError> {
        self.txn
            .take()
            .ok_or(SqlError::Transaction(TransactionError::NotBegun))
    }
}

impl<F: Fs> std::fmt::Debug for Session<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("in_transaction", &self.in_transaction())
            .finish_non_exhaustive()
    }
}
