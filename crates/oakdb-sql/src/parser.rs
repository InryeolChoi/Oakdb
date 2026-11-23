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
/// `BETWEEN`, `IN` and `LIKE` are not among them: they are only read where
/// an operator stands, so they can still name a column (design 0019, Q1).
const RESERVED: [&str; 15] = [
    "CREATE", "TABLE", "INSERT", "INTO", "VALUES", "SELECT", "FROM", "WHERE", "PRIMARY", "KEY",
    "NULL", "AND", "OR", "NOT", "IS",
];

/// Operators of real SQL that Oakdb does not support, where an operator
/// stands in WHERE.
const UNSUPPORTED_OPERATORS: [&str; 4] = ["LIKE", "GLOB", "REGEXP", "MATCH"];

/// What an unsupported operator's error says Oakdb supports.
const OPERATORS_SUPPORTED: &str = "only comparisons, IS [NOT] NULL, BETWEEN and IN are supported";

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
        let filter = match &self.peek().token {
            Token::End | Token::Symbol(";") => None,
            Token::Word(word) if word.eq_ignore_ascii_case("WHERE") => {
                self.next();
                Some(self.expr()?)
            }
            Token::Word(word) => {
                return Err(SqlError::Unsupported(format!(
                    "`{word}` in SELECT; only WHERE is supported after FROM"
                )))
            }
            _ => return Err(self.expected("`WHERE`")),
        };
        Ok(Statement::Select {
            table,
            columns,
            filter,
        })
    }

    /// expr := and_expr (OR and_expr)*
    fn expr(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.and_expr()?;
        while self.at_keyword("OR") {
            self.next();
            left = Expr::Or(Box::new(left), Box::new(self.and_expr()?));
        }
        Ok(left)
    }

    /// and_expr := not_expr (AND not_expr)*
    fn and_expr(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.not_expr()?;
        while self.at_keyword("AND") {
            self.next();
            left = Expr::And(Box::new(left), Box::new(self.not_expr()?));
        }
        Ok(left)
    }

    /// not_expr := NOT not_expr | predicate
    fn not_expr(&mut self) -> Result<Expr, SqlError> {
        if self.at_keyword("NOT") {
            self.next();
            return Ok(Expr::Not(Box::new(self.not_expr()?)));
        }
        self.predicate()
    }

    /// predicate := sum [ compare_op sum
    ///                  | IS [NOT] NULL
    ///                  | [NOT] BETWEEN sum AND sum
    ///                  | [NOT] IN "(" expr ("," expr)* ")" ]
    ///
    /// The ends of BETWEEN are sums, so the AND between them is never read
    /// as a logical AND. A comparison cannot be followed by another
    /// (`a < b < c`): that is unsupported rather than read as comparing a
    /// condition.
    fn predicate(&mut self) -> Result<Expr, SqlError> {
        let left = Box::new(self.sum()?);
        let predicate = match self.peek().token.clone() {
            Token::Symbol(symbol) => match compare_op(symbol) {
                Some(op) => {
                    self.next();
                    Expr::Compare {
                        op,
                        left,
                        right: Box::new(self.sum()?),
                    }
                }
                None => return Ok(*left),
            },
            Token::Word(word) if word.eq_ignore_ascii_case("IS") => {
                self.next();
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
                Expr::IsNull {
                    operand: left,
                    negated,
                }
            }
            Token::Word(word)
                if ["NOT", "BETWEEN", "IN"]
                    .iter()
                    .any(|k| word.eq_ignore_ascii_case(k)) =>
            {
                let negated = word.eq_ignore_ascii_case("NOT");
                if negated {
                    self.next();
                }
                if self.at_keyword("BETWEEN") {
                    self.next();
                    let low = Box::new(self.sum()?);
                    self.expect_keyword("AND")?;
                    let high = Box::new(self.sum()?);
                    Expr::Between {
                        operand: left,
                        low,
                        high,
                        negated,
                    }
                } else if self.at_keyword("IN") {
                    self.next();
                    self.expect_symbol("(")?;
                    let mut list = vec![self.expr()?];
                    while self.comma_or_close(false)? {
                        list.push(self.expr()?);
                    }
                    Expr::In {
                        operand: left,
                        list,
                        negated,
                    }
                } else {
                    self.unsupported_operator()?;
                    return Err(self.expected("`BETWEEN` or `IN` after `NOT`"));
                }
            }
            Token::Word(_) => {
                self.unsupported_operator()?;
                return Ok(*left);
            }
            _ => return Ok(*left),
        };
        if let Token::Symbol(symbol) = self.peek().token {
            if compare_op(symbol).is_some() {
                return Err(SqlError::Unsupported(format!(
                    "`{symbol}` after a comparison; comparisons cannot be chained"
                )));
            }
        }
        Ok(predicate)
    }

    /// Fails as unsupported if the next token is an operator of real SQL
    /// that Oakdb does not support, such as LIKE.
    fn unsupported_operator(&self) -> Result<(), SqlError> {
        match &self.peek().token {
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

    /// sum := product (("+" | "-") product)*
    fn sum(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.product()?;
        loop {
            let op = match self.peek().token {
                Token::Symbol("+") => ArithOp::Add,
                Token::Symbol("-") => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.next();
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(self.product()?),
            };
        }
    }

    /// product := unary (("*" | "/" | "%") unary)*
    fn product(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek().token {
                Token::Symbol("*") => ArithOp::Mul,
                Token::Symbol("/") => ArithOp::Div,
                Token::Symbol("%") => ArithOp::Rem,
                _ => return Ok(left),
            };
            self.next();
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(self.unary()?),
            };
        }
    }

    /// unary := "-" unary | primary
    ///
    /// A `-` right before digits is a negative literal, as in INSERT, so
    /// `-9223372036854775808` is i64::MIN rather than the negation of a
    /// number too big for 64 bits.
    fn unary(&mut self) -> Result<Expr, SqlError> {
        if !self.at_symbol("-") {
            return self.primary();
        }
        let minus = self.next();
        if let Token::Integer(digits) = self.peek().token.clone() {
            self.next();
            return Ok(Expr::Literal(integer(&digits, true, minus.position)?));
        }
        Ok(Expr::Neg(Box::new(self.unary()?)))
    }

    /// primary := literal | name | "(" expr ")"
    fn primary(&mut self) -> Result<Expr, SqlError> {
        match &self.peek().token {
            Token::Symbol("(") => {
                self.next();
                let inner = self.expr()?;
                self.expect_symbol(")")?;
                Ok(inner)
            }
            Token::Word(word) if word.eq_ignore_ascii_case("NULL") => {
                Ok(Expr::Literal(self.literal()?))
            }
            Token::Word(_) => Ok(Expr::Column(self.name()?)),
            Token::Integer(_) | Token::Text(_) | Token::Blob(_) => {
                Ok(Expr::Literal(self.literal()?))
            }
            _ => Err(self.expected("an expression")),
        }
    }
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
}
