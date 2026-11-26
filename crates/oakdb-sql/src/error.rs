//! Errors a SQL statement can fail with (design 0017, section 4).

use std::fmt;

use crate::ColumnType;

/// Everything that can go wrong running a SQL statement.
///
/// The kinds are flat (design 0027 E6): each is one `catch` a client may
/// want, and the groups they fall into (syntax, names, types, values out of
/// range, constraints, transaction state, run again, engine) are the ranges
/// of the wire codes in `docs/protocol.md`, section 6. The enum is
/// `non_exhaustive`: callers match the kinds they care about and keep a
/// catch-all arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SqlError {
    /// The statement could not be read: a token is malformed or out of
    /// place.
    Syntax {
        /// Byte offset in the statement, from 0, where the problem starts.
        position: usize,
        /// The line of `position`, from 1. Lines end at `\n`.
        line: usize,
        /// The column of `position` in its line, from 1, counted in
        /// characters, so `'김'` is one and so is a tab (design 0027 E11).
        column: usize,
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
    /// A value does not fit its column: it is of another type. A NULL in
    /// the primary key is [`SqlError::NullKey`] (design 0027 E3).
    TypeMismatch {
        /// The column.
        column: String,
        /// The column's type.
        expected: ColumnType,
        /// The value's type. Always `Some` since issue #34; the `Option`
        /// stays so the field keeps its shape (design 0027, small question
        /// 1).
        found: Option<ColumnType>,
    },
    /// An expression mixes types where it cannot (design 0019, section
    /// 3): arithmetic on text, a condition that is not true-or-false, two
    /// sides of a comparison of different types where neither side is a
    /// plain column (that case is [`SqlError::TypeMismatch`]). Found
    /// before any row is read.
    Type(String),
    /// A value is out of range (design 0027 E4): integer arithmetic
    /// overflowed or divided by zero (design 0019, section 2-2), or an
    /// integer literal does not fit in 64 bits. The text says which, and
    /// for a literal where it is.
    Arithmetic(String),
    /// A NULL where a column cannot hold one: the primary key (design
    /// 0027 E3). A constraint, like [`SqlError::DuplicateKey`], not a type.
    NullKey {
        /// The column.
        column: String,
    },
    /// What the SQL layer stored in the engine could not be read back. The
    /// text says what is wrong.
    Corrupt(String),
    /// A transaction command does not fit the session's state (design
    /// 0023, section 4). The session's transaction, if one is open, is
    /// left as it was.
    Transaction(TransactionError),
    /// The COMMIT of a transaction BEGIN opened lost a conflict with a
    /// transaction that committed first (design 0023, 5-2): the whole
    /// transaction was rolled back and has to run again from BEGIN. It
    /// holds the engine's [`oakdb::Error::Conflict`]. A statement that ran
    /// on its own gets [`SqlError::Engine`] with that conflict instead;
    /// [`SqlError::is_conflict`] is true for both.
    TransactionConflict(oakdb::Error),
    /// The engine failed: a key too long, a failed disk, a stopped engine,
    /// or a conflict with a transaction that committed first
    /// ([`oakdb::Error::Conflict`]) in a statement that ran on its own,
    /// which rolls it back: run it again.
    Engine(oakdb::Error),
}

impl SqlError {
    /// Whether this is a conflict with a transaction that committed first:
    /// nothing was written, and running the statement again, or the
    /// transaction again from BEGIN, may succeed.
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            SqlError::TransactionConflict(_) | SqlError::Engine(oakdb::Error::Conflict { .. })
        )
    }
}

/// One error of each kind of [`SqlError`], and of each known kind of
/// engine error inside [`SqlError::Engine`], so a test outside this crate
/// can check what each maps to (design 0027 E13: no known kind falls to
/// the server's "other statement error" code). Not for use otherwise.
///
/// A unit test below matches every kind of this crate without a catch-all
/// arm, so a new kind does not compile until it is added here too.
#[doc(hidden)]
pub fn every_kind() -> Vec<SqlError> {
    let io = || std::sync::Arc::new(std::io::Error::other("sample"));
    let mut kinds = vec![
        SqlError::Syntax {
            position: 0,
            line: 1,
            column: 1,
            message: "sample".into(),
        },
        SqlError::Unsupported("sample".into()),
        SqlError::NoSuchTable("t".into()),
        SqlError::NoSuchColumn {
            table: "t".into(),
            column: "c".into(),
        },
        SqlError::TableExists("t".into()),
        SqlError::DuplicateColumn("c".into()),
        SqlError::ValueCount {
            expected: 1,
            found: 2,
        },
        SqlError::DuplicateKey { table: "t".into() },
        SqlError::TypeMismatch {
            column: "c".into(),
            expected: ColumnType::Integer,
            found: Some(ColumnType::Text),
        },
        SqlError::Type("sample".into()),
        SqlError::Arithmetic("sample".into()),
        SqlError::NullKey { column: "c".into() },
        SqlError::Corrupt("sample".into()),
        SqlError::Transaction(TransactionError::AlreadyBegun),
        SqlError::Transaction(TransactionError::NotBegun),
        SqlError::Transaction(TransactionError::NeedsSession),
        SqlError::TransactionConflict(oakdb::Error::Conflict { key: vec![0] }),
    ];
    kinds.extend(
        [
            oakdb::Error::Io(io()),
            oakdb::Error::OutcomeUnknown(io()),
            oakdb::Error::Stopped(std::sync::Arc::new(oakdb::Error::Io(io()))),
            oakdb::Error::KeyTooLarge { len: 513, max: 512 },
            oakdb::Error::ValueTooLarge { len: 2, max: 1 },
            oakdb::Error::PageSizeMismatch {
                expected: 4096,
                found: 512,
            },
            oakdb::Error::Corrupt("sample".into()),
            oakdb::Error::AlreadyOpen,
            oakdb::Error::Conflict { key: vec![0] },
        ]
        .into_iter()
        .map(SqlError::Engine),
    );
    kinds
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
            SqlError::Syntax {
                line,
                column,
                message,
                ..
            } => write!(f, "syntax error at line {line}, column {column}: {message}"),
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
            SqlError::NullKey { column } => {
                write!(f, "column {column} is the primary key and cannot be NULL")
            }
            SqlError::Corrupt(reason) => write!(f, "stored SQL data is corrupt: {reason}"),
            SqlError::Transaction(e) => write!(f, "transaction error: {e}"),
            // Not the engine's text, which names an internal key and says
            // to "begin again" (PR #78 review S2, P3).
            SqlError::TransactionConflict(_) => f.write_str(
                "conflict: another transaction changed the same data first, so the whole \
                 transaction was rolled back; run it again from BEGIN",
            ),
            SqlError::Engine(oakdb::Error::Conflict { .. }) => f.write_str(
                "conflict: another transaction changed the same data first, so this \
                 statement was rolled back; run it again",
            ),
            SqlError::Engine(e) => write!(f, "engine error: {e}"),
        }
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqlError::Engine(e) | SqlError::TransactionConflict(e) => Some(e),
            _ => None,
        }
    }
}

impl From<oakdb::Error> for SqlError {
    fn from(e: oakdb::Error) -> Self {
        SqlError::Engine(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which kind `e` is. No catch-all arm for this crate's kinds: a new
    /// kind of [`SqlError`] or [`TransactionError`] fails to compile here
    /// until [`every_kind`] holds it.
    fn kind(e: &SqlError) -> &'static str {
        match e {
            SqlError::Syntax { .. } => "Syntax",
            SqlError::Unsupported(_) => "Unsupported",
            SqlError::NoSuchTable(_) => "NoSuchTable",
            SqlError::NoSuchColumn { .. } => "NoSuchColumn",
            SqlError::TableExists(_) => "TableExists",
            SqlError::DuplicateColumn(_) => "DuplicateColumn",
            SqlError::ValueCount { .. } => "ValueCount",
            SqlError::DuplicateKey { .. } => "DuplicateKey",
            SqlError::TypeMismatch { .. } => "TypeMismatch",
            SqlError::Type(_) => "Type",
            SqlError::Arithmetic(_) => "Arithmetic",
            SqlError::NullKey { .. } => "NullKey",
            SqlError::Corrupt(_) => "Corrupt",
            SqlError::Transaction(TransactionError::AlreadyBegun) => "AlreadyBegun",
            SqlError::Transaction(TransactionError::NotBegun) => "NotBegun",
            SqlError::Transaction(TransactionError::NeedsSession) => "NeedsSession",
            SqlError::TransactionConflict(_) => "TransactionConflict",
            SqlError::Engine(e) => match e {
                oakdb::Error::Io(_) => "Io",
                oakdb::Error::OutcomeUnknown(_) => "OutcomeUnknown",
                oakdb::Error::Stopped(_) => "Stopped",
                oakdb::Error::KeyTooLarge { .. } => "KeyTooLarge",
                oakdb::Error::ValueTooLarge { .. } => "ValueTooLarge",
                oakdb::Error::PageSizeMismatch { .. } => "PageSizeMismatch",
                oakdb::Error::Corrupt(_) => "EngineCorrupt",
                oakdb::Error::AlreadyOpen => "AlreadyOpen",
                oakdb::Error::Conflict { .. } => "Conflict",
                // The engine's enum is `non_exhaustive` to this crate, so a
                // new engine kind is caught by the server's own check that
                // it gets an engine code, not here.
                _ => "OtherEngine",
            },
        }
    }

    #[test]
    fn every_kind_holds_each_kind_once() {
        let kinds: Vec<&str> = every_kind().iter().map(kind).collect();
        let distinct: std::collections::HashSet<&str> = kinds.iter().copied().collect();
        assert_eq!(distinct.len(), kinds.len(), "{kinds:?}");
        // 17 SQL kinds (three of them transaction errors) and 9 engine ones.
        assert_eq!(kinds.len(), 26, "{kinds:?}");
    }

    /// A statement that ran on its own and lost a conflict says to run it
    /// again, in one SQL sentence, without the engine's advice to "begin
    /// again" or its internal key (PR #78 review S2, P3).
    #[test]
    fn an_autocommit_conflict_says_to_run_the_statement_again() {
        let e = SqlError::Engine(oakdb::Error::Conflict { key: vec![0] });
        assert!(e.is_conflict());
        let message = e.to_string();
        assert!(message.starts_with("conflict: "), "{message}");
        assert!(message.contains("run it again"), "{message}");
        assert!(!message.contains("BEGIN"), "{message}");
        assert!(!message.contains("0x"), "{message}");
        assert!(!message.contains("begin again and retry"), "{message}");
    }
}
