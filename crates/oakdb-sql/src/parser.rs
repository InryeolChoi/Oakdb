//! The parser: turns tokens into a statement (design 0017, section 3).
//!
//! It is recursive descent, one function per rule of the grammar, so the
//! code reads like the grammar. Where real SQL allows more than Oakdb does
//! (another statement, another column type, another operator) it says
//! [`SqlError::Unsupported`] and names what it found; where the statement
//! is broken (a missing parenthesis) it says [`SqlError::Syntax`] and
//! where.

use crate::lexer::{syntax, tokenize, Spanned, Token};
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
    let mut parser = Parser {
        tokens: tokenize(sql)?,
        at: 0,
    };
    let statement = parser.statement()?;
    parser.finish()?;
    Ok(statement)
}

/// Words that shape the grammar, so they cannot name a table or column.
/// Type names are not among them: `text TEXT` is a fine column.
const RESERVED: [&str; 11] = [
    "CREATE", "TABLE", "INSERT", "INTO", "VALUES", "SELECT", "FROM", "WHERE", "PRIMARY", "KEY",
    "NULL",
];

/// What every WHERE error says Oakdb supports.
const WHERE_SUPPORTED: &str = "only `<primary key> = <value>` is supported";

struct Parser {
    /// The tokens, ending with [`Token::End`], which [`Parser::next`]
    /// never moves past.
    tokens: Vec<Spanned>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> &Spanned {
        &self.tokens[self.at]
    }

    fn next(&mut self) -> Spanned {
        let spanned = self.tokens[self.at].clone();
        if spanned.token != Token::End {
            self.at += 1;
        }
        spanned
    }

    /// Whether the next token is keyword `keyword`, in any case.
    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(&self.peek().token, Token::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    /// Whether the next token is symbol `symbol`.
    fn at_symbol(&self, symbol: &str) -> bool {
        matches!(self.peek().token, Token::Symbol(found) if found == symbol)
    }

    /// Takes keyword `keyword`, or fails with a syntax error.
    fn expect_keyword(&mut self, keyword: &str) -> Result<(), SqlError> {
        if self.at_keyword(keyword) {
            self.next();
            Ok(())
        } else {
            Err(self.expected(&format!("`{keyword}`")))
        }
    }

    /// Takes symbol `symbol`, or fails with a syntax error.
    fn expect_symbol(&mut self, symbol: &str) -> Result<(), SqlError> {
        if self.at_symbol(symbol) {
            self.next();
            Ok(())
        } else {
            Err(self.expected(&format!("`{symbol}`")))
        }
    }

    /// Takes keyword `keyword`, failing as unsupported if another word
    /// stands there (real SQL has other words in that place) and as a
    /// syntax error if something else does. `unsupported` turns the word
    /// found into the error's text.
    fn expect_keyword_or_unsupported(
        &mut self,
        keyword: &str,
        unsupported: impl FnOnce(&str) -> String,
    ) -> Result<(), SqlError> {
        if let Token::Word(word) = &self.peek().token {
            if !word.eq_ignore_ascii_case(keyword) {
                return Err(SqlError::Unsupported(unsupported(word)));
            }
        }
        self.expect_keyword(keyword)
    }

    /// A syntax error at the next token: `what` was expected there.
    fn expected(&self, what: &str) -> SqlError {
        let found = self.peek();
        syntax(
            found.position,
            &format!("expected {what}, found {}", describe(&found.token)),
        )
    }

    /// Takes a table or column name.
    fn name(&mut self) -> Result<String, SqlError> {
        let found = self.peek();
        match &found.token {
            Token::Word(word) if RESERVED.iter().any(|k| word.eq_ignore_ascii_case(k)) => {
                Err(syntax(
                    found.position,
                    &format!("`{word}` is a keyword, not a name"),
                ))
            }
            Token::Word(word) => {
                let word = word.clone();
                self.next();
                Ok(word)
            }
            _ => Err(self.expected("a name")),
        }
    }

    /// statement := create | insert | select
    fn statement(&mut self) -> Result<Statement, SqlError> {
        match &self.peek().token {
            Token::Word(word) if word.eq_ignore_ascii_case("CREATE") => self.create(),
            Token::Word(word) if word.eq_ignore_ascii_case("INSERT") => self.insert(),
            Token::Word(word) if word.eq_ignore_ascii_case("SELECT") => self.select(),
            Token::Word(word) => Err(SqlError::Unsupported(format!(
                "statement `{word}`; only CREATE TABLE, INSERT and SELECT are supported"
            ))),
            _ => Err(self.expected("a statement")),
        }
    }

    /// The end of the statement: an optional `;`, then nothing. Anything
    /// more is unsupported: a second statement, or a clause such as `AND`
    /// or `ORDER BY` after a complete one.
    fn finish(&mut self) -> Result<(), SqlError> {
        let semicolon = self.at_symbol(";");
        if semicolon {
            self.next();
        }
        match &self.peek().token {
            Token::End => Ok(()),
            found if semicolon => Err(SqlError::Unsupported(format!(
                "more than one statement: {} after `;`",
                describe(found)
            ))),
            found => Err(SqlError::Unsupported(format!(
                "{} where the statement should end",
                describe(found)
            ))),
        }
    }

    /// create := CREATE TABLE name "(" column ("," column)* ")"
    fn create(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword("CREATE")?;
        self.expect_keyword_or_unsupported("TABLE", |word| {
            format!("CREATE {word}; only CREATE TABLE is supported")
        })?;
        let name = self.name()?;
        self.expect_symbol("(")?;
        let mut columns = vec![self.column()?];
        while self.comma_or_close(false)? {
            columns.push(self.column()?);
        }
        Ok(Statement::CreateTable { name, columns })
    }

    /// column := name type [PRIMARY KEY]
    fn column(&mut self) -> Result<ColumnDef, SqlError> {
        let name = self.name()?;
        let column_type = match &self.peek().token {
            Token::Word(word) => match word.to_ascii_uppercase().as_str() {
                "INTEGER" => ColumnType::Integer,
                "TEXT" => ColumnType::Text,
                "BLOB" => ColumnType::Blob,
                _ => {
                    return Err(SqlError::Unsupported(format!(
                        "column type `{word}`; only INTEGER, TEXT and BLOB are supported"
                    )))
                }
            },
            _ => return Err(self.expected("a column type")),
        };
        self.next();
        let primary_key = self.at_keyword("PRIMARY");
        if primary_key {
            self.next();
            self.expect_keyword("KEY")?;
        }
        if let Token::Word(word) = &self.peek().token {
            return Err(SqlError::Unsupported(format!(
                "column constraint `{word}`; only PRIMARY KEY is supported"
            )));
        }
        Ok(ColumnDef {
            name,
            column_type,
            primary_key,
        })
    }

    /// insert := INSERT INTO name ["(" name ("," name)* ")"] VALUES row ("," row)*
    fn insert(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword_or_unsupported("INTO", |word| {
            format!("INSERT {word}; only INSERT INTO is supported")
        })?;
        let table = self.name()?;
        let columns = if self.at_symbol("(") {
            self.next();
            let mut columns = vec![self.name()?];
            while self.comma_or_close(false)? {
                columns.push(self.name()?);
            }
            Some(columns)
        } else {
            None
        };
        self.expect_keyword_or_unsupported("VALUES", |word| {
            format!("`{word}` in INSERT; only VALUES is supported")
        })?;
        let mut rows = vec![self.row()?];
        while self.at_symbol(",") {
            self.next();
            rows.push(self.row()?);
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    /// row := "(" literal ("," literal)* ")"
    fn row(&mut self) -> Result<Vec<Value>, SqlError> {
        self.expect_symbol("(")?;
        let mut values = vec![self.literal()?];
        while self.comma_or_close(true)? {
            values.push(self.literal()?);
        }
        Ok(values)
    }

    /// After an item of a parenthesized list: takes `,` and says `true`
    /// (another item follows), or takes `)` and says `false`. In a list of
    /// values an operator there means an expression, which is unsupported
    /// rather than broken.
    fn comma_or_close(&mut self, values: bool) -> Result<bool, SqlError> {
        match &self.peek().token {
            Token::Symbol(",") => {
                self.next();
                Ok(true)
            }
            Token::Symbol(")") => {
                self.next();
                Ok(false)
            }
            Token::Symbol(symbol) if values => Err(SqlError::Unsupported(format!(
                "`{symbol}` after a value; only plain values are supported, \
                 not expressions or decimals"
            ))),
            _ => Err(self.expected("`,` or `)`")),
        }
    }

    /// literal := ["-"] integer | text | blob | NULL
    fn literal(&mut self) -> Result<Value, SqlError> {
        let start = self.next();
        match start.token {
            Token::Symbol("-") => match self.peek().token.clone() {
                Token::Integer(digits) => {
                    self.next();
                    integer(&digits, true, start.position)
                }
                _ => Err(self.expected("digits after `-`")),
            },
            Token::Integer(digits) => integer(&digits, false, start.position),
            Token::Text(text) => Ok(Value::Text(text)),
            Token::Blob(bytes) => Ok(Value::Blob(bytes)),
            Token::Word(word) if word.eq_ignore_ascii_case("NULL") => Ok(Value::Null),
            Token::Word(word) => Err(SqlError::Unsupported(format!(
                "`{word}` as a value; only literals are supported"
            ))),
            found => Err(syntax(
                start.position,
                &format!("expected a value, found {}", describe(&found)),
            )),
        }
    }

    /// select := SELECT ("*" | name ("," name)*) FROM name WHERE name "=" literal
    fn select(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword("SELECT")?;
        let columns = if self.at_symbol("*") {
            self.next();
            None
        } else {
            let mut columns = vec![self.name()?];
            while self.at_symbol(",") {
                self.next();
                columns.push(self.name()?);
            }
            Some(columns)
        };
        self.expect_keyword("FROM")?;
        let table = self.name()?;
        if matches!(self.peek().token, Token::End | Token::Symbol(";")) {
            return Err(SqlError::Unsupported(format!(
                "SELECT without WHERE; {WHERE_SUPPORTED}"
            )));
        }
        self.expect_keyword_or_unsupported("WHERE", |word| {
            format!("`{word}` in SELECT; only WHERE is supported after FROM")
        })?;
        let column = self.name()?;
        match &self.peek().token {
            Token::Symbol("=") => {}
            Token::Symbol(op) => {
                return Err(SqlError::Unsupported(format!(
                    "operator `{op}` in WHERE; {WHERE_SUPPORTED}"
                )))
            }
            Token::Word(op) => {
                return Err(SqlError::Unsupported(format!(
                    "operator `{op}` in WHERE; {WHERE_SUPPORTED}"
                )))
            }
            _ => return Err(self.expected("`=`")),
        }
        self.next();
        let value = self.literal()?;
        Ok(Statement::Select {
            table,
            columns,
            filter: (column, value),
        })
    }
}

/// The integer that `digits` spell, negated if `negative`, or a syntax
/// error at `position` if it does not fit in an i64. The digits are read
/// as an i128 first so that `-9223372036854775808`, whose digits alone do
/// not fit in an i64, still does once negated.
fn integer(digits: &str, negative: bool, position: usize) -> Result<Value, SqlError> {
    let out_of_range = || syntax(position, "the integer does not fit in 64 bits");
    let magnitude: i128 = digits.parse().map_err(|_| out_of_range())?;
    let value = if negative { -magnitude } else { magnitude };
    i64::try_from(value)
        .map(Value::Integer)
        .map_err(|_| out_of_range())
}

/// How an error names a token.
fn describe(token: &Token) -> String {
    match token {
        Token::Word(word) => format!("`{word}`"),
        Token::Integer(digits) => format!("`{digits}`"),
        Token::Text(_) => "a text value".into(),
        Token::Blob(_) => "a blob value".into(),
        Token::Symbol(symbol) => format!("`{symbol}`"),
        Token::End => "the end of the statement".into(),
    }
}
