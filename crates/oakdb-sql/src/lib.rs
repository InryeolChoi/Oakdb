//! The SQL layer of Oakdb (issue #31, design 0017; WHERE: issue #32,
//! design 0019; UPDATE and DELETE: issue #33, design 0022; BEGIN, COMMIT
//! and ROLLBACK: issue #35, design 0023).
//!
//! It turns a SQL string into calls on the key-value engine: the lexer cuts
//! the string into tokens, the parser turns the tokens into a statement,
//! the planner turns a WHERE clause into a range of primary keys and a
//! condition, and the executor runs the statement in an engine
//! transaction: one of its own, or the one a [`Session`] keeps open from
//! BEGIN to COMMIT. The engine knows nothing about SQL; everything about
//! tables, columns and rows lives here, stored in the engine as keys and
//! values that sort in the right order (design 0017, sections 1 and 2).
//!
//! ```
//! use oakdb::{fs::sim::SimFs, Db};
//! use oakdb_sql::{execute, Output, Value};
//!
//! let db = Db::open_with(SimFs::new(), "/db").unwrap();
//! execute(&db, "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
//! execute(&db, "INSERT INTO users VALUES (1, 'kim')").unwrap();
//! let out = execute(&db, "SELECT name FROM users WHERE id = 1").unwrap();
//! assert_eq!(
//!     out,
//!     Output::Rows {
//!         columns: vec!["name".into()],
//!         rows: vec![vec![Value::Text("kim".into())]],
//!     }
//! );
//! ```

mod catalog;
mod encoding;
mod error;
mod executor;
mod expr;
mod lexer;
mod parser;
mod planner;
mod session;

use std::fmt;

use oakdb::fs::Fs;
use oakdb::Db;

pub use error::{SqlError, TransactionError};
pub use session::Session;

/// Runs one SQL statement on `db` and returns what it produced.
///
/// The statement runs in its own engine transaction (autocommit): it
/// commits if the statement succeeds, and is thrown away whole if any part
/// of it fails, so a three-row INSERT whose third row is rejected inserts
/// nothing. An optional trailing semicolon is allowed; a second statement
/// is not.
///
/// BEGIN, COMMIT and ROLLBACK fail with
/// [`TransactionError::NeedsSession`]: a transaction this call opened
/// would end with it. Use a [`Session`] to keep one open.
pub fn execute<F: Fs>(db: &Db<F>, sql: &str) -> Result<Output, SqlError> {
    match parser::parse_command(sql)? {
        parser::Command::Run(statement) => executor::run(db, statement),
        _ => Err(SqlError::Transaction(TransactionError::NeedsSession)),
    }
}

/// BEGIN, COMMIT or ROLLBACK: the statements that start or end a
/// session's transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionCommand {
    /// `BEGIN [TRANSACTION]`.
    Begin,
    /// `COMMIT [TRANSACTION]`.
    Commit,
    /// `ROLLBACK [TRANSACTION]`.
    Rollback,
}

/// Which of BEGIN, COMMIT and ROLLBACK `sql` is, as a [`Session`] would
/// read it, or `None` for any other statement and for SQL that does not
/// parse. A client uses it to follow whether its connection is in a
/// transaction without asking the server (issue #37, design 0025 Q3):
/// BEGIN answered `DONE` opens one, COMMIT or ROLLBACK ends it whatever
/// the answer, and nothing else changes it.
///
/// ```
/// use oakdb_sql::{transaction_command, TransactionCommand};
///
/// assert_eq!(transaction_command("begin;"), Some(TransactionCommand::Begin));
/// assert_eq!(transaction_command("COMMIT later"), None);
/// assert_eq!(transaction_command("SELECT * FROM t"), None);
/// ```
pub fn transaction_command(sql: &str) -> Option<TransactionCommand> {
    parser::transaction_command(sql)
}

/// What a statement produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The rows a SELECT found, each holding one value per column, in the
    /// order of `columns`.
    Rows {
        /// Names of the selected columns.
        columns: Vec<String>,
        /// The rows found.
        rows: Vec<Vec<Value>>,
    },
    /// How many rows an INSERT added, or an UPDATE or DELETE matched:
    /// every matched row counts, even one an UPDATE leaves as it was
    /// (design 0022, U9).
    Affected(u64),
    /// The statement produced nothing to show (CREATE TABLE).
    Done,
}

/// A value stored in, or read from, a column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// No value. Allowed in any column but the primary key.
    Null,
    /// A 64-bit signed integer.
    Integer(i64),
    /// UTF-8 text.
    Text(String),
    /// Raw bytes.
    Blob(Vec<u8>),
}

impl Value {
    /// The column type this value belongs in, or `None` for NULL, which
    /// belongs in any column.
    pub fn column_type(&self) -> Option<ColumnType> {
        match self {
            Value::Null => None,
            Value::Integer(_) => Some(ColumnType::Integer),
            Value::Text(_) => Some(ColumnType::Text),
            Value::Blob(_) => Some(ColumnType::Blob),
        }
    }
}

/// The type of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// [`Value::Integer`].
    Integer,
    /// [`Value::Text`].
    Text,
    /// [`Value::Blob`].
    Blob,
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ColumnType::Integer => "INTEGER",
            ColumnType::Text => "TEXT",
            ColumnType::Blob => "BLOB",
        })
    }
}
