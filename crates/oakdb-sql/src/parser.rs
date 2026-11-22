//! The parser: turns tokens into a statement (design 0017, section 3).
//!
//! It is recursive descent, one function per rule of the grammar, so the
//! code reads like the grammar. Where real SQL allows more than Oakdb does
//! (another statement, another column type, another operator) it says
//! [`SqlError::Unsupported`] and names what it found; where the statement
//! is broken (a missing parenthesis) it says [`SqlError::Syntax`] and
//! where.

use crate::lexer::{tokenize, Spanned, Token};
use crate::{ColumnType, SqlError, Value};

/// A parsed statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Statement {
    /// `CREATE TABLE name (column type [PRIMARY KEY], ...)`.
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    /// `INSERT INTO table [(column, ...)] VALUES (value, ...), ...`.
    Insert {
        table: String,
        /// The column list, or `None` for every column in table order.
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    },
    /// `SELECT * | column, ... FROM table WHERE column = value`.
    Select {
        table: String,
        /// The selected columns, or `None` for `*`.
        columns: Option<Vec<String>>,
        /// The column and value of the WHERE clause.
        filter: (String, Value),
    },
}

/// A column in CREATE TABLE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDef {
    pub(crate) name: String,
    pub(crate) column_type: ColumnType,
    pub(crate) primary_key: bool,
}

/// Parses one statement, with an optional trailing semicolon.
pub(crate) fn parse(sql: &str) -> Result<Statement, SqlError> {
    let _ = (sql, tokenize, std::mem::size_of::<Spanned>(), Token::End);
    unimplemented!("parser")
}
