//! The parser: turns tokens into a statement (design 0017, section 3).
//!
//! It is recursive descent, one function per rule of the grammar, so the
//! code reads like the grammar. Where real SQL allows more than Oakdb does
//! (another statement, another column type, another operator) it says
//! [`SqlError::Unsupported`] and names what it found; where the statement
//! is broken (a missing parenthesis) it says [`SqlError::Syntax`] and
//! where.

use crate::expr::{ArithOp, CompareOp, Expr};
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
    /// `SELECT * | column, ... FROM table [WHERE condition]`.
    Select {
        table: String,
        /// The selected columns, or `None` for `*`.
        columns: Option<Vec<String>>,
        /// The WHERE condition, or `None` for every row.
        filter: Option<Expr>,
    },
    /// `UPDATE table SET column = expr, ... [WHERE condition]` (design
    /// 0022).
    Update {
        table: String,
        /// Each column SET names and the expression it gets, in order.
        assignments: Vec<(String, Expr)>,
        /// The WHERE condition, or `None` for every row.
        filter: Option<Expr>,
    },
    /// `DELETE FROM table [WHERE condition]` (design 0022).
    Delete {
        table: String,
        /// The WHERE condition, or `None` for every row.
        filter: Option<Expr>,
    },
}

/// A parsed command: a transaction command, which a session handles, or a
/// statement, which runs inside a transaction (design 0023, T3). Keeping
/// them apart means the executor, which only takes a [`Statement`], has
/// no BEGIN to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// `BEGIN [TRANSACTION]`.
    Begin,
    /// `COMMIT [TRANSACTION]`.
    Commit,
    /// `ROLLBACK [TRANSACTION]`.
    Rollback,
    /// Any other statement.
    Run(Statement),
}

/// A column in CREATE TABLE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDef {
    pub(crate) name: String,
    pub(crate) column_type: ColumnType,
    pub(crate) primary_key: bool,
}

/// Parses one command, with an optional trailing semicolon.
pub(crate) fn parse_command(sql: &str) -> Result<Command, SqlError> {
    let mut parser = Parser {
        tokens: tokenize(sql)?,
        at: 0,
        depth: 0,
    };
    let command = parser.command()?;
    parser.finish()?;
    Ok(command)
}

/// Parses one statement that is not a transaction command.
#[cfg(test)]
pub(crate) fn parse(sql: &str) -> Result<Statement, SqlError> {
    match parse_command(sql)? {
        Command::Run(statement) => Ok(statement),
        other => panic!("{other:?} is not a statement"),
    }
}

/// Words that shape the grammar, so they cannot name a table or column.
/// Type names are not among them: `text TEXT` is a fine column.
/// `BETWEEN`, `IN` and `LIKE` are not among them: they are only read where
/// an operator stands, so they can still name a column (design 0019, Q1).
/// `TRANSACTION` is not among them either: it is optional, and only read
/// right after BEGIN, COMMIT or ROLLBACK (design 0023, Q1).
const RESERVED: [&str; 21] = [
    "CREATE", "TABLE", "INSERT", "INTO", "VALUES", "SELECT", "FROM", "WHERE", "PRIMARY", "KEY",
    "NULL", "AND", "OR", "NOT", "IS", "UPDATE", "SET", "DELETE", "BEGIN", "COMMIT", "ROLLBACK",
];

/// Operators of real SQL that Oakdb does not support, where an operator
/// stands in WHERE.
const UNSUPPORTED_OPERATORS: [&str; 4] = ["LIKE", "GLOB", "REGEXP", "MATCH"];

/// What an unsupported operator's error says Oakdb supports.
const OPERATORS_SUPPORTED: &str = "only comparisons, IS [NOT] NULL, BETWEEN and IN are supported";

/// How deep an expression may nest (PR #69 review S1, design 0019
/// section 1). Parsing, planning, computing and dropping an expression
/// each recurse over its tree, so a tree deep enough would overflow the
/// stack and abort the process. The parser refuses one taller than this,
/// or that would make it recurse deeper than this, before building it.
pub(crate) const MAX_DEPTH: usize = 200;

struct Parser {
    /// The tokens, ending with [`Token::End`], which [`Parser::next`]
    /// never moves past.
    tokens: Vec<Spanned>,
    at: usize,
    /// How many expressions, NOTs and unary minuses the parser is inside
    /// of right now.
    depth: usize,
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

    /// command := (BEGIN | COMMIT | ROLLBACK) [TRANSACTION] | statement
    ///
    /// Whatever else follows, such as SQLite's `BEGIN IMMEDIATE` or
    /// `ROLLBACK TO`, is left for [`Parser::finish`] to call unsupported.
    fn command(&mut self) -> Result<Command, SqlError> {
        for (keyword, command) in [
            ("BEGIN", Command::Begin),
            ("COMMIT", Command::Commit),
            ("ROLLBACK", Command::Rollback),
        ] {
            if self.at_keyword(keyword) {
                self.next();
                if self.at_keyword("TRANSACTION") {
                    self.next();
                }
                return Ok(command);
            }
        }
        self.statement().map(Command::Run)
    }

    /// statement := create | insert | select | update | delete
    fn statement(&mut self) -> Result<Statement, SqlError> {
        match &self.peek().token {
            Token::Word(word) if word.eq_ignore_ascii_case("CREATE") => self.create(),
            Token::Word(word) if word.eq_ignore_ascii_case("INSERT") => self.insert(),
            Token::Word(word) if word.eq_ignore_ascii_case("SELECT") => self.select(),
            Token::Word(word) if word.eq_ignore_ascii_case("UPDATE") => self.update(),
            Token::Word(word) if word.eq_ignore_ascii_case("DELETE") => self.delete(),
            Token::Word(word) => Err(SqlError::Unsupported(format!(
                "statement `{word}`; only CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, BEGIN, \
                 COMMIT and ROLLBACK are supported"
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

    /// select := SELECT ("*" | name ("," name)*) FROM name [WHERE expr]
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
        let filter = self.filter("SELECT", "after FROM")?;
        Ok(Statement::Select {
            table,
            columns,
            filter,
        })
    }

    /// update := UPDATE name SET name "=" expr ("," name "=" expr)* [WHERE expr]
    fn update(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword("UPDATE")?;
        if self.at_keyword("OR") {
            return Err(SqlError::Unsupported(
                "UPDATE OR ...; conflicts cannot be resolved another way".into(),
            ));
        }
        let table = self.name()?;
        self.expect_keyword_or_unsupported("SET", |word| {
            format!("`{word}` in UPDATE; only SET is supported after the table")
        })?;
        let mut assignments = vec![self.assignment()?];
        while self.at_symbol(",") {
            self.next();
            assignments.push(self.assignment()?);
        }
        let filter = self.filter("UPDATE", "after SET")?;
        Ok(Statement::Update {
            table,
            assignments,
            filter,
        })
    }

    /// One `column = expr` of SET.
    fn assignment(&mut self) -> Result<(String, Expr), SqlError> {
        if self.at_symbol("(") {
            return Err(SqlError::Unsupported(
                "a list of columns in SET; set one column at a time".into(),
            ));
        }
        let column = self.name()?;
        self.expect_symbol("=")?;
        if self.at_keyword("DEFAULT") {
            return Err(SqlError::Unsupported(
                "DEFAULT in SET; columns have no default value".into(),
            ));
        }
        Ok((column, self.expr()?.expr))
    }

    /// delete := DELETE FROM name [WHERE expr]
    fn delete(&mut self) -> Result<Statement, SqlError> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword_or_unsupported("FROM", |word| {
            format!("`{word}` after DELETE; only DELETE FROM is supported")
        })?;
        let table = self.name()?;
        let filter = self.filter("DELETE", "after the table")?;
        Ok(Statement::Delete { table, filter })
    }

    /// An optional `WHERE expr` ending `statement`. Another word there is
    /// a clause Oakdb does not support (`ORDER BY`, `LIMIT`, `RETURNING`,
    /// `FROM`); `place` says where WHERE stands in the error.
    fn filter(&mut self, statement: &str, place: &str) -> Result<Option<Expr>, SqlError> {
        match &self.peek().token {
            Token::End | Token::Symbol(";") => Ok(None),
            Token::Word(word) if word.eq_ignore_ascii_case("WHERE") => {
                self.next();
                Ok(Some(self.expr()?.expr))
            }
            Token::Word(word) => Err(SqlError::Unsupported(format!(
                "`{word}` in {statement}; only WHERE is supported {place}"
            ))),
            _ => Err(self.expected("`WHERE`")),
        }
    }

    /// expr := a whole expression, as in WHERE and each item of an IN
    /// list.
    ///
    /// Operators are read by precedence climbing (design 0019, section 1)
    /// rather than one function per level of precedence: a level of
    /// parentheses then costs two stack frames instead of eight, and runs
    /// of NOT or unary minus cost none, which matters on a server thread's
    /// small stack (PR #69 review S1). The grammar is the same:
    ///
    /// ```text
    /// expr      := and_expr (OR and_expr)*
    /// and_expr  := not_expr (AND not_expr)*
    /// not_expr  := NOT not_expr | predicate
    /// predicate := sum [ compare_op sum | IS [NOT] NULL
    ///                  | [NOT] BETWEEN sum AND sum | [NOT] IN "(" expr ("," expr)* ")" ]
    /// sum       := product (("+" | "-") product)*
    /// product   := unary (("*" | "/" | "%") unary)*
    /// unary     := "-" unary | primary
    /// primary   := literal | name | "(" expr ")"
    /// ```
    fn expr(&mut self) -> Result<Tree, SqlError> {
        self.descend()?;
        let tree = self.climb(Precedence::Or)?;
        self.ascend();
        Ok(tree)
    }

    /// An expression whose infix operators all bind at least as tightly
    /// as `min`. Operators of one level join left to right (`a - b - c` is
    /// `(a - b) - c`) in a loop, without recursing.
    fn climb(&mut self, min: Precedence) -> Result<Tree, SqlError> {
        let mut left = self.prefix(min)?;
        while let Some(op) = self.infix()? {
            if op.precedence() < min {
                break;
            }
            left = self.infixed(left, op)?;
            if op.precedence() == Precedence::Predicate {
                self.no_second_predicate()?;
            }
        }
        Ok(left)
    }

    /// The operand an infix operator stands after: a run of NOTs (only
    /// where `min` lets NOT stand) over a predicate, or a run of unary
    /// minuses over a primary. Runs are read in a loop and wrapped from
    /// the inside out, so they do not recurse.
    fn prefix(&mut self, min: Precedence) -> Result<Tree, SqlError> {
        let mut nots = Vec::new();
        while min <= Precedence::Not && self.at_keyword("NOT") {
            nots.push(self.next().position);
        }
        if !nots.is_empty() {
            let mut tree = self.climb(Precedence::Predicate)?;
            for &at in nots.iter().rev() {
                tree = self.node(at, &[tree.height], Expr::Not(Box::new(tree.expr)))?;
            }
            return Ok(tree);
        }
        let mut minuses = Vec::new();
        while self.at_symbol("-") {
            minuses.push(self.next().position);
        }
        let mut tree = match minuses.last() {
            Some(&at) if matches!(self.peek().token, Token::Integer(_)) => {
                minuses.pop();
                self.negative_literal(at)?
            }
            _ => self.primary()?,
        };
        for &at in minuses.iter().rev() {
            tree = self.node(at, &[tree.height], Expr::Neg(Box::new(tree.expr)))?;
        }
        Ok(tree)
    }

    /// primary := literal | name | "(" expr ")"
    fn primary(&mut self) -> Result<Tree, SqlError> {
        match &self.peek().token {
            Token::Symbol("(") => {
                self.next();
                self.descend()?;
                let inner = self.climb(Precedence::Or)?;
                self.ascend();
                self.expect_symbol(")")?;
                Ok(inner)
            }
            Token::Word(word) if !word.eq_ignore_ascii_case("NULL") => {
                Ok(Tree::leaf(Expr::Column(self.name()?)))
            }
            Token::Word(_) | Token::Integer(_) | Token::Text(_) | Token::Blob(_) => {
                Ok(Tree::leaf(Expr::Literal(self.literal()?)))
            }
            _ => Err(self.expected("an expression")),
        }
    }

    /// The literal of the digits after a `-` at `at`. A `-` right before
    /// digits is a negative literal, as in INSERT, so
    /// `-9223372036854775808` is i64::MIN rather than the negation of a
    /// number too big for 64 bits.
    #[inline(never)]
    fn negative_literal(&mut self, at: usize) -> Result<Tree, SqlError> {
        match self.next().token {
            Token::Integer(digits) => Ok(Tree::leaf(Expr::Literal(integer(&digits, true, at)?))),
            found => Err(syntax(
                at,
                &format!("expected digits after `-`, found {}", describe(&found)),
            )),
        }
    }

    /// The infix operator at the next token, if one is there. Fails as
    /// unsupported on an operator of real SQL that Oakdb does not support,
    /// such as LIKE, and as a syntax error on NOT followed by neither
    /// BETWEEN nor IN.
    #[inline(never)]
    fn infix(&self) -> Result<Option<Infix>, SqlError> {
        let keyword = |token: &Token, keyword: &str| matches!(token, Token::Word(word) if word.eq_ignore_ascii_case(keyword));
        let token = &self.peek().token;
        Ok(Some(match token {
            Token::Symbol("+") => Infix::Arith(ArithOp::Add),
            Token::Symbol("-") => Infix::Arith(ArithOp::Sub),
            Token::Symbol("*") => Infix::Arith(ArithOp::Mul),
            Token::Symbol("/") => Infix::Arith(ArithOp::Div),
            Token::Symbol("%") => Infix::Arith(ArithOp::Rem),
            Token::Symbol(symbol) => match compare_op(symbol) {
                Some(op) => Infix::Compare(op),
                None => return Ok(None),
            },
            _ if keyword(token, "OR") => Infix::Or,
            _ if keyword(token, "AND") => Infix::And,
            _ if keyword(token, "IS") => Infix::Is,
            _ if keyword(token, "BETWEEN") => Infix::Between { negated: false },
            _ if keyword(token, "IN") => Infix::In { negated: false },
            _ if keyword(token, "NOT") => {
                let after = &self.tokens[(self.at + 1).min(self.tokens.len() - 1)];
                if keyword(&after.token, "BETWEEN") {
                    Infix::Between { negated: true }
                } else if keyword(&after.token, "IN") {
                    Infix::In { negated: true }
                } else {
                    unsupported_operator(&after.token)?;
                    return Err(syntax(
                        after.position,
                        &format!(
                            "expected `BETWEEN` or `IN` after `NOT`, found {}",
                            describe(&after.token)
                        ),
                    ));
                }
            }
            Token::Word(_) => {
                unsupported_operator(token)?;
                return Ok(None);
            }
            _ => return Ok(None),
        }))
    }

    /// Reads infix operator `op` and what follows it, after `left`.
    fn infixed(&mut self, left: Tree, op: Infix) -> Result<Tree, SqlError> {
        match op {
            Infix::Or => self.binary(left, op, Expr::Or),
            Infix::And => self.binary(left, op, Expr::And),
            Infix::Compare(compare) => self.binary(left, op, |left, right| Expr::Compare {
                op: compare,
                left,
                right,
            }),
            Infix::Arith(arith) => self.binary(left, op, |left, right| Expr::Arith {
                op: arith,
                left,
                right,
            }),
            Infix::Is => self.is_null(left),
            Infix::Between { negated } => self.between(left, negated),
            Infix::In { negated } => self.in_list(left, negated),
        }
    }

    /// The rest of `left op right` for a two-sided operator, at the
    /// operator. The right side binds tighter than `op`, so operators of
    /// one level join left to right.
    fn binary(
        &mut self,
        left: Tree,
        op: Infix,
        make: impl FnOnce(Box<Expr>, Box<Expr>) -> Expr,
    ) -> Result<Tree, SqlError> {
        let at = self.next().position;
        let right = self.climb(op.precedence().tighter())?;
        self.join(at, left, right, make)
    }

    /// The rest of `left IS [NOT] NULL`, at `IS`.
    #[inline(never)]
    fn is_null(&mut self, left: Tree) -> Result<Tree, SqlError> {
        let at = self.next().position;
        let negated = self.at_keyword("NOT");
        if negated {
            self.next();
        }
        if !self.at_keyword("NULL") {
            return Err(SqlError::Unsupported(format!(
                "`IS` before {}; only IS NULL and IS NOT NULL are supported",
                describe(&self.peek().token)
            )));
        }
        self.next();
        self.node(
            at,
            &[left.height],
            Expr::IsNull {
                operand: Box::new(left.expr),
                negated,
            },
        )
    }

    /// The rest of `left [NOT] BETWEEN sum AND sum`, at `NOT` or
    /// `BETWEEN`. The ends are sums, so the AND between them is never read
    /// as a logical AND.
    #[inline(never)]
    fn between(&mut self, left: Tree, negated: bool) -> Result<Tree, SqlError> {
        let at = self.next().position;
        if negated {
            self.next();
        }
        let low = self.climb(Precedence::Sum)?;
        self.expect_keyword("AND")?;
        let high = self.climb(Precedence::Sum)?;
        self.node(
            at,
            &[left.height, low.height, high.height],
            Expr::Between {
                operand: Box::new(left.expr),
                low: Box::new(low.expr),
                high: Box::new(high.expr),
                negated,
            },
        )
    }

    /// The rest of `left [NOT] IN (expr, ...)`, at `NOT` or `IN`.
    #[inline(never)]
    fn in_list(&mut self, left: Tree, negated: bool) -> Result<Tree, SqlError> {
        let at = self.next().position;
        if negated {
            self.next();
        }
        self.expect_symbol("(")?;
        let mut items = vec![self.expr()?];
        while self.comma_or_close(false)? {
            items.push(self.expr()?);
        }
        let mut heights: Vec<usize> = items.iter().map(|item| item.height).collect();
        heights.push(left.height);
        self.node(
            at,
            &heights,
            Expr::In {
                operand: Box::new(left.expr),
                list: items.into_iter().map(|item| item.expr).collect(),
                negated,
            },
        )
    }

    /// Fails as unsupported if another comparison, IS, BETWEEN or IN
    /// follows one (`a < b < c`): that would compare a condition.
    fn no_second_predicate(&self) -> Result<(), SqlError> {
        match self.infix()? {
            Some(op) if op.precedence() == Precedence::Predicate => {
                Err(SqlError::Unsupported(format!(
                    "{} after a comparison; comparisons cannot be chained",
                    describe(&self.peek().token)
                )))
            }
            _ => Ok(()),
        }
    }

    /// One more level of parentheses or IN lists, or a syntax error at
    /// the next token past [`MAX_DEPTH`] levels.
    fn descend(&mut self) -> Result<(), SqlError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(too_deep(self.peek().position));
        }
        Ok(())
    }

    /// Back out of a level [`Parser::descend`] entered.
    fn ascend(&mut self) {
        self.depth -= 1;
    }

    /// Node `expr` over children of heights `children`, or a syntax error
    /// at `position` if that makes the tree taller than [`MAX_DEPTH`].
    /// Chains such as `1 + 1 + ...` and runs of NOT grow the tree without
    /// the parser recursing, so the tree's height is checked as well as
    /// the parser's depth.
    fn node(&self, position: usize, children: &[usize], expr: Expr) -> Result<Tree, SqlError> {
        let height = 1 + children.iter().copied().max().unwrap_or(0);
        if height > MAX_DEPTH {
            return Err(too_deep(position));
        }
        Ok(Tree { expr, height })
    }

    /// The node `make` builds over `left` and `right`.
    fn join(
        &self,
        position: usize,
        left: Tree,
        right: Tree,
        make: impl FnOnce(Box<Expr>, Box<Expr>) -> Expr,
    ) -> Result<Tree, SqlError> {
        let children = [left.height, right.height];
        self.node(
            position,
            &children,
            make(Box::new(left.expr), Box::new(right.expr)),
        )
    }
}

/// How tightly an operator binds, loosest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    Or,
    And,
    Not,
    /// Comparisons, IS, BETWEEN and IN.
    Predicate,
    Sum,
    Product,
    /// Unary minus and primaries: no infix operator binds this tightly.
    Unary,
}

impl Precedence {
    /// The next tighter level: the right side of a left-associative
    /// operator binds at least this tightly.
    fn tighter(self) -> Self {
        match self {
            Precedence::Or => Precedence::And,
            Precedence::And => Precedence::Not,
            Precedence::Not => Precedence::Predicate,
            Precedence::Predicate => Precedence::Sum,
            Precedence::Sum => Precedence::Product,
            Precedence::Product | Precedence::Unary => Precedence::Unary,
        }
    }
}

/// An infix operator.
#[derive(Debug, Clone, Copy)]
enum Infix {
    Or,
    And,
    Compare(CompareOp),
    Is,
    Between { negated: bool },
    In { negated: bool },
    Arith(ArithOp),
}

impl Infix {
    fn precedence(self) -> Precedence {
        match self {
            Infix::Or => Precedence::Or,
            Infix::And => Precedence::And,
            Infix::Compare(_) | Infix::Is | Infix::Between { .. } | Infix::In { .. } => {
                Precedence::Predicate
            }
            Infix::Arith(ArithOp::Add | ArithOp::Sub) => Precedence::Sum,
            Infix::Arith(ArithOp::Mul | ArithOp::Div | ArithOp::Rem) => Precedence::Product,
        }
    }
}

/// Fails as unsupported if `token` is an operator of real SQL that Oakdb
/// does not support, such as LIKE.
fn unsupported_operator(token: &Token) -> Result<(), SqlError> {
    match token {
        Token::Word(word)
            if UNSUPPORTED_OPERATORS
                .iter()
                .any(|k| word.eq_ignore_ascii_case(k)) =>
        {
            Err(SqlError::Unsupported(format!(
                "operator `{}`; {OPERATORS_SUPPORTED}",
                word.to_ascii_uppercase()
            )))
        }
        _ => Ok(()),
    }
}

/// An expression and the height of its tree, 1 for a literal or a column.
struct Tree {
    expr: Expr,
    height: usize,
}

impl Tree {
    fn leaf(expr: Expr) -> Self {
        Tree { expr, height: 1 }
    }
}

/// The error for an expression nested deeper than [`MAX_DEPTH`].
fn too_deep(position: usize) -> SqlError {
    syntax(
        position,
        &format!("the expression nests more than {MAX_DEPTH} levels deep"),
    )
}

/// The comparison operator `symbol` stands for, if any.
fn compare_op(symbol: &str) -> Option<CompareOp> {
    Some(match symbol {
        "=" => CompareOp::Eq,
        "!=" | "<>" => CompareOp::Ne,
        "<" => CompareOp::Lt,
        "<=" => CompareOp::Le,
        ">" => CompareOp::Gt,
        ">=" => CompareOp::Ge,
        _ => return None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The WHERE clause of `SELECT * FROM t WHERE <condition>`.
    fn filter(condition: &str) -> Expr {
        match parse(&format!("SELECT * FROM t WHERE {condition}")).unwrap() {
            Statement::Select {
                filter: Some(filter),
                ..
            } => filter,
            other => panic!("{other:?}"),
        }
    }

    fn col(name: &str) -> Box<Expr> {
        Box::new(Expr::Column(name.into()))
    }

    fn int(i: i64) -> Box<Expr> {
        Box::new(Expr::Literal(Value::Integer(i)))
    }

    fn cmp(op: CompareOp, left: Box<Expr>, right: Box<Expr>) -> Box<Expr> {
        Box::new(Expr::Compare { op, left, right })
    }

    fn arith(op: ArithOp, left: Box<Expr>, right: Box<Expr>) -> Box<Expr> {
        Box::new(Expr::Arith { op, left, right })
    }

    #[test]
    fn and_binds_tighter_than_or_and_not_tighter_than_and() {
        let eq = |name: &str, i| cmp(CompareOp::Eq, col(name), int(i));
        assert_eq!(
            filter("a = 1 OR b = 2 AND c = 3"),
            Expr::Or(eq("a", 1), Box::new(Expr::And(eq("b", 2), eq("c", 3))))
        );
        assert_eq!(
            filter("NOT a = 1 AND b = 2"),
            Expr::And(Box::new(Expr::Not(eq("a", 1))), eq("b", 2))
        );
        assert_eq!(
            filter("(a = 1 OR b = 2) AND c = 3"),
            Expr::And(Box::new(Expr::Or(eq("a", 1), eq("b", 2))), eq("c", 3))
        );
    }

    #[test]
    fn arithmetic_binds_tighter_than_comparison_and_is_left_associative() {
        assert_eq!(
            filter("a + 2 * 3 = 7"),
            *cmp(
                CompareOp::Eq,
                arith(ArithOp::Add, col("a"), arith(ArithOp::Mul, int(2), int(3))),
                int(7)
            )
        );
        assert_eq!(
            filter("10 - a - 2 < 0"),
            *cmp(
                CompareOp::Lt,
                arith(ArithOp::Sub, arith(ArithOp::Sub, int(10), col("a")), int(2)),
                int(0)
            )
        );
        assert_eq!(
            filter("-a % 2 >= -1"),
            *cmp(
                CompareOp::Ge,
                arith(ArithOp::Rem, Box::new(Expr::Neg(col("a"))), int(2)),
                int(-1)
            )
        );
    }

    #[test]
    fn the_and_of_between_is_not_a_logical_and() {
        assert_eq!(
            filter("a NOT BETWEEN 1 AND 5 AND b <> 2"),
            Expr::And(
                Box::new(Expr::Between {
                    operand: col("a"),
                    low: int(1),
                    high: int(5),
                    negated: true,
                }),
                cmp(CompareOp::Ne, col("b"), int(2))
            )
        );
    }

    #[test]
    fn in_and_is_null_read_their_operands() {
        assert_eq!(
            filter("a NOT IN (1, b + 1)"),
            Expr::In {
                operand: col("a"),
                list: vec![*int(1), *arith(ArithOp::Add, col("b"), int(1))],
                negated: true,
            }
        );
        assert_eq!(
            filter("a IS NOT NULL"),
            Expr::IsNull {
                operand: col("a"),
                negated: true
            }
        );
        assert_eq!(filter("a != 1"), filter("a <> 1"));
    }

    #[test]
    fn a_minus_before_digits_is_a_literal_and_otherwise_a_negation() {
        assert_eq!(
            filter("a = -9223372036854775808"),
            *cmp(CompareOp::Eq, col("a"), int(i64::MIN))
        );
        assert_eq!(
            filter("a = - -4"),
            *cmp(CompareOp::Eq, col("a"), Box::new(Expr::Neg(int(-4))))
        );
    }

    #[test]
    fn a_select_without_where_has_no_filter() {
        assert!(matches!(
            parse("SELECT * FROM t;").unwrap(),
            Statement::Select { filter: None, .. }
        ));
    }

    #[test]
    fn update_reads_its_assignments_in_order_and_an_optional_where() {
        assert_eq!(
            parse("UPDATE t SET a = a + 1, b = 'x' WHERE id = 1").unwrap(),
            Statement::Update {
                table: "t".into(),
                assignments: vec![
                    ("a".into(), *arith(ArithOp::Add, col("a"), int(1))),
                    ("b".into(), Expr::Literal(Value::Text("x".into()))),
                ],
                filter: Some(*cmp(CompareOp::Eq, col("id"), int(1))),
            }
        );
        assert!(matches!(
            parse("update t set a = 1;").unwrap(),
            Statement::Update { filter: None, .. }
        ));
    }

    #[test]
    fn delete_reads_its_table_and_an_optional_where() {
        assert_eq!(
            parse("DELETE FROM t WHERE id > 1").unwrap(),
            Statement::Delete {
                table: "t".into(),
                filter: Some(*cmp(CompareOp::Gt, col("id"), int(1))),
            }
        );
        assert_eq!(
            parse("delete from t").unwrap(),
            Statement::Delete {
                table: "t".into(),
                filter: None,
            }
        );
    }

    #[test]
    fn a_set_expression_is_nested_no_deeper_than_where() {
        let deep = format!(
            "UPDATE t SET a = {}1{}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        assert!(matches!(parse(&deep), Err(SqlError::Syntax { .. })));
    }
}
