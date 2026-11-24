//! Errors a SQL statement can fail with (design 0017, section 4).

use std::fmt;

use crate::ColumnType;

/// Everything that can go wrong running a SQL statement.
///
/// Issue #34 will refine these, so the enum is `non_exhaustive`: callers
/// match the kinds they care about and keep a catch-all arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SqlError {
    /// The statement could not be read: a token is malformed or out of
    /// place.
    Syntax {
        /// Byte offset in the statement, from 0, where the problem starts.
        position: usize,
        /// What was expected or what is wrong.
        message: String,
    },
    /// The statement is SQL that Oakdb does not support (yet). The text
    /// names what was found.
    Unsupported(String),
    /// No table has this name.
    NoSuchTable(String),
    /// The table has no column with this name.
    NoSuchColumn {
        /// The table.
        table: String,
        /// The column that is not there.
        column: String,
    },
    /// A table with this name already exists.
    TableExists(String),
    /// A column is named twice, in CREATE TABLE or in an INSERT's column
    /// list.
    DuplicateColumn(String),
    /// An INSERT row holds a different number of values than there are
    /// columns to fill.
    ValueCount {
        /// How many columns the row fills.
        expected: usize,
        /// How many values the row holds.
        found: usize,
    },
    /// A row with this primary key already exists in the table, or the
    /// same INSERT adds it twice.
    DuplicateKey {
        /// The table.
        table: String,
    },
    /// A value does not fit its column: it is of another type, or it is
    /// NULL in the primary key.
    TypeMismatch {
        /// The column.
        column: String,
        /// The column's type.
        expected: ColumnType,
        /// The value's type, `None` for NULL.
        found: Option<ColumnType>,
    },
    /// An expression mixes types where it cannot (design 0019, section
    /// 3): arithmetic on text, a condition that is not true-or-false, two
    /// sides of a comparison of different types where neither side is a
    /// plain column (that case is [`SqlError::TypeMismatch`]). Found
    /// before any row is read.
    Type(String),
    /// Integer arithmetic overflowed or divided by zero (design 0019,
    /// section 2-2).
    Arithmetic(String),
    /// What the SQL layer stored in the engine could not be read back. The
    /// text says what is wrong.
    Corrupt(String),
    /// A transaction command does not fit the session's state (design
    /// 0023, section 4). The session's transaction, if one is open, is
    /// left as it was.
    Transaction(TransactionError),
    /// The engine failed: a key too long, a failed disk, a stopped engine,
    /// or a conflict with a transaction that committed first
    /// ([`oakdb::Error::Conflict`]), which ends the transaction: run it
    /// again.
    Engine(oakdb::Error),
}

/// How a transaction command does not fit the session's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransactionError {
    /// BEGIN while a transaction is already open.
    AlreadyBegun,
    /// COMMIT or ROLLBACK with no transaction open.
    NotBegun,
    /// BEGIN, COMMIT or ROLLBACK given to [`crate::execute`], which runs
    /// each statement in a transaction of its own; a
    /// [`crate::Session`] keeps one open between statements.
    NeedsSession,
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TransactionError::AlreadyBegun => {
                "a transaction is already open; COMMIT or ROLLBACK it first"
            }
            TransactionError::NotBegun => "no transaction is open",
            TransactionError::NeedsSession => {
                "BEGIN, COMMIT and ROLLBACK need a session, which keeps the transaction open \
                 between statements"
            }
        })
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Syntax { position, message } => {
                write!(f, "syntax error at byte {position}: {message}")
            }
            SqlError::Unsupported(what) => write!(f, "unsupported: {what}"),
            SqlError::NoSuchTable(table) => write!(f, "no such table: {table}"),
            SqlError::NoSuchColumn { table, column } => {
                write!(f, "no such column: {table}.{column}")
            }
            SqlError::TableExists(table) => write!(f, "table {table} already exists"),
            SqlError::DuplicateColumn(column) => write!(f, "column {column} is named twice"),
            SqlError::ValueCount { expected, found } => write!(
                f,
                "a row holds {found} values, but {expected} columns are filled"
            ),
            SqlError::DuplicateKey { table } => {
                write!(f, "a row with this primary key already exists in {table}")
            }
            SqlError::TypeMismatch {
                column,
                expected,
                found: Some(found),
            } => write!(f, "column {column} is {expected}, but the value is {found}"),
            SqlError::TypeMismatch {
                column,
                found: None,
                ..
            } => write!(f, "column {column} is the primary key and cannot be NULL"),
            SqlError::Type(what) => write!(f, "type error: {what}"),
            SqlError::Arithmetic(what) => write!(f, "arithmetic error: {what}"),
            SqlError::Corrupt(reason) => write!(f, "stored SQL data is corrupt: {reason}"),
            SqlError::Transaction(e) => write!(f, "transaction error: {e}"),
            // The engine's text says to begin again; a transaction that
            // failed at COMMIT has to run again from BEGIN, all of it
            // (design 0023, Q4).
            SqlError::Engine(e @ oakdb::Error::Conflict { .. }) => write!(
                f,
                "conflict: {e}; the transaction wrote nothing (if it began with BEGIN, \
                 run it again from BEGIN)"
            ),
            SqlError::Engine(e) => write!(f, "engine error: {e}"),
        }
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqlError::Engine(e) => Some(e),
            _ => None,
        }
    }
}

impl From<oakdb::Error> for SqlError {
    fn from(e: oakdb::Error) -> Self {
        SqlError::Engine(e)
    }
}
