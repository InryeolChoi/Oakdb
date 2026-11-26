//! The lexer: cuts a SQL string into tokens (design 0017, section 3).
//!
//! It knows nothing about grammar. Keywords come out as plain words, so the
//! parser decides what a word means where it stands; the lexer only cares
//! where one token ends and the next begins. Every token carries the byte
//! offset it starts at, so errors can point at it.

use crate::SqlError;

/// One token of a SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    /// A keyword or a name, as written: `SELECT`, `users`, `x1`. Keywords
    /// are told apart by the parser, ignoring case.
    Word(String),
    /// The digits of an integer, without a sign. The parser joins a minus
    /// in front and checks the range, so `-9223372036854775808` fits.
    Integer(String),
    /// A quoted text literal, with each `''` turned into one `'`.
    Text(String),
    /// A blob literal, `X'ab01'`, as its bytes.
    Blob(Vec<u8>),
    /// Punctuation or an operator: `(`, `)`, `,`, `;`, comparisons,
    /// arithmetic, and `.`, which Oakdb does not support yet, so the
    /// parser can name it in its error.
    Symbol(&'static str),
    /// The end of the statement.
    End,
}

/// A token and the byte offset, from 0, where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spanned {
    pub(crate) token: Token,
    pub(crate) position: usize,
}

/// Cuts `sql` into tokens, ending with [`Token::End`].
///
/// It walks the bytes: everything that ends a token in SQL is ASCII, so
/// text between quotes can be copied as it is, whatever letters it holds.
pub(crate) fn tokenize(sql: &str) -> Result<Vec<Spanned>, SqlError> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let b = bytes[at];
        let token = if b.is_ascii_whitespace() {
            at += 1;
            continue;
        } else if let Some(opener) = comment(&bytes[at..]) {
            // Design 0027 E10: a comment is standard SQL that Oakdb does
            // not read yet, so it is unsupported, not broken.
            return Err(SqlError::Unsupported(format!(
                "comments (`{opener}` at {}); remove the comment",
                at_line_column(sql, start)
            )));
        } else if (b == b'x' || b == b'X') && bytes.get(at + 1) == Some(&b'\'') {
            let (text, end) = quoted(sql, at + 1, start)?;
            at = end;
            Token::Blob(hex(&text, start)?)
        } else if b.is_ascii_alphabetic() || b == b'_' {
            at = run_of(bytes, at, is_word_byte);
            Token::Word(sql[start..at].to_owned())
        } else if b.is_ascii_digit() {
            at = run_of(bytes, at, |b| b.is_ascii_digit());
            // `12ab` is neither a number nor a name; saying so here gives a
            // clearer error than letting the parser trip over two tokens.
            if bytes.get(at).is_some_and(|&b| is_word_byte(b)) {
                return Err(syntax(start, "a number runs into letters"));
            }
            Token::Integer(sql[start..at].to_owned())
        } else if b == b'\'' {
            let (text, end) = quoted(sql, at, start)?;
            at = end;
            Token::Text(text)
        } else if let Some(symbol) = symbol(&bytes[at..]) {
            at += symbol.len();
            Token::Symbol(symbol)
        } else {
            let found = sql[at..].chars().next().unwrap_or_default();
            return Err(syntax(start, &format!("unexpected character `{found}`")));
        };
        tokens.push(Spanned {
            token,
            position: start,
        });
    }
    tokens.push(Spanned {
        token: Token::End,
        position: bytes.len(),
    });
    Ok(tokens)
}

/// The opener of the comment `rest` starts with, if it starts with one.
fn comment(rest: &[u8]) -> Option<&'static str> {
    ["--", "/*"]
        .into_iter()
        .find(|opener| rest.starts_with(opener.as_bytes()))
}

/// The line and column of byte `position` of `sql`, both from 1: lines end
/// at `\n`, and the column counts characters, so a letter of several bytes
/// or a tab is one (design 0027 E11). A position past the end is just after
/// the last character.
pub(crate) fn line_column(sql: &str, position: usize) -> (usize, usize) {
    let mut end = position.min(sql.len());
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    let before = &sql[..end];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = before.matches('\n').count() + 1;
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// `line L, column C` for byte `position` of `sql`.
pub(crate) fn at_line_column(sql: &str, position: usize) -> String {
    let (line, column) = line_column(sql, position);
    format!("line {line}, column {column}")
}

/// Whether `b` can be part of a name or keyword after its first byte.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Where the run of bytes matching `keep` that starts at `from` ends.
fn run_of(bytes: &[u8], from: usize, keep: impl Fn(u8) -> bool) -> usize {
    bytes[from..]
        .iter()
        .position(|&b| !keep(b))
        .map_or(bytes.len(), |len| from + len)
}

/// The text of the quoted literal whose opening quote is at `open`, with
/// `''` turned into `'`, and the offset just past its closing quote. An
/// unclosed literal is an error at `start`, where the token starts.
fn quoted(sql: &str, open: usize, start: usize) -> Result<(String, usize), SqlError> {
    let bytes = sql.as_bytes();
    let mut text = String::new();
    let mut from = open + 1;
    loop {
        let Some(len) = bytes[from..].iter().position(|&b| b == b'\'') else {
            return Err(syntax(start, "the quote is never closed"));
        };
        let quote = from + len;
        text.push_str(&sql[from..quote]);
        if bytes.get(quote + 1) == Some(&b'\'') {
            text.push('\'');
            from = quote + 2;
        } else {
            return Ok((text, quote + 1));
        }
    }
}

/// The bytes that `digits`, the inside of a blob literal at `start`, spell
/// in hexadecimal, two digits a byte.
fn hex(digits: &str, start: usize) -> Result<Vec<u8>, SqlError> {
    let digits = digits.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err(syntax(start, "a blob needs two hex digits per byte"));
    }
    digits
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16);
            let low = (pair[1] as char).to_digit(16);
            match (high, low) {
                // Two hex digits always fit in a byte.
                (Some(high), Some(low)) => Ok((high * 16 + low) as u8),
                _ => Err(syntax(start, "a blob holds only hex digits")),
            }
        })
        .collect()
}

/// The symbol `rest` starts with, longest first, so `<=` is not read as
/// `<` and `=`.
fn symbol(rest: &[u8]) -> Option<&'static str> {
    const SYMBOLS: [&str; 17] = [
        "<=", ">=", "<>", "!=", "(", ")", ",", ";", "*", "=", "-", "<", ">", ".", "+", "/", "%",
    ];
    SYMBOLS
        .into_iter()
        .find(|symbol| rest.starts_with(symbol.as_bytes()))
}

/// A syntax error at byte `position`. Its line and column are 0 until
/// [`locate`] fills them in from the statement.
pub(crate) fn syntax(position: usize, message: &str) -> SqlError {
    SqlError::Syntax {
        position,
        line: 0,
        column: 0,
        message: message.to_owned(),
    }
}

/// `e` with its line and column filled in, if it is a syntax error in
/// `sql`.
pub(crate) fn locate(e: SqlError, sql: &str) -> SqlError {
    match e {
        SqlError::Syntax {
            position, message, ..
        } => {
            let (line, column) = line_column(sql, position);
            SqlError::Syntax {
                position,
                line,
                column,
                message,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(sql: &str) -> Vec<Token> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .map(|spanned| spanned.token)
            .collect()
    }

    fn word(w: &str) -> Token {
        Token::Word(w.to_owned())
    }

    fn syntax_position(sql: &str) -> usize {
        match tokenize(sql) {
            Err(SqlError::Syntax { position, .. }) => position,
            other => panic!("expected a syntax error for {sql:?}, got {other:?}"),
        }
    }

    #[test]
    fn words_keep_their_case_and_the_statement_ends_with_end() {
        assert_eq!(
            tokens("select Name FROM t1"),
            vec![
                word("select"),
                word("Name"),
                word("FROM"),
                word("t1"),
                Token::End
            ]
        );
    }

    #[test]
    fn punctuation_is_cut_with_or_without_spaces() {
        assert_eq!(
            tokens("(a,b);*="),
            vec![
                Token::Symbol("("),
                word("a"),
                Token::Symbol(","),
                word("b"),
                Token::Symbol(")"),
                Token::Symbol(";"),
                Token::Symbol("*"),
                Token::Symbol("="),
                Token::End,
            ]
        );
    }

    #[test]
    fn a_minus_is_its_own_token_before_the_digits() {
        assert_eq!(
            tokens("-42"),
            vec![Token::Symbol("-"), Token::Integer("42".into()), Token::End]
        );
    }

    #[test]
    fn two_quotes_inside_text_are_one_quote() {
        assert_eq!(
            tokens("'it''s' ''"),
            vec![
                Token::Text("it's".into()),
                Token::Text(String::new()),
                Token::End
            ]
        );
    }

    #[test]
    fn text_keeps_non_ascii_letters() {
        assert_eq!(
            tokens("'김치'"),
            vec![Token::Text("김치".into()), Token::End]
        );
    }

    #[test]
    fn a_blob_is_x_and_hex_digits_in_quotes_in_either_case() {
        assert_eq!(
            tokens("X'ab01' x'FF' X''"),
            vec![
                Token::Blob(vec![0xab, 0x01]),
                Token::Blob(vec![0xff]),
                Token::Blob(vec![]),
                Token::End
            ]
        );
    }

    #[test]
    fn an_x_not_followed_by_a_quote_is_a_word() {
        assert_eq!(tokens("x xy"), vec![word("x"), word("xy"), Token::End]);
    }

    #[test]
    fn operators_come_out_as_symbols() {
        assert_eq!(
            tokens("< <= > >= <> != . + %"),
            vec![
                Token::Symbol("<"),
                Token::Symbol("<="),
                Token::Symbol(">"),
                Token::Symbol(">="),
                Token::Symbol("<>"),
                Token::Symbol("!="),
                Token::Symbol("."),
                Token::Symbol("+"),
                Token::Symbol("%"),
                Token::End
            ]
        );
    }

    #[test]
    fn each_token_knows_the_byte_where_it_starts() {
        let positions: Vec<usize> = tokenize("SELECT  'é', x")
            .unwrap()
            .into_iter()
            .map(|spanned| spanned.position)
            .collect();
        // 'é' is two bytes, so the comma after the closing quote is at 12.
        assert_eq!(positions, vec![0, 8, 12, 14, 15]);
    }

    #[test]
    fn an_unclosed_quote_is_a_syntax_error_where_it_opens() {
        assert_eq!(syntax_position("SELECT 'abc"), 7);
        assert_eq!(syntax_position("x'ab"), 0);
    }

    #[test]
    fn a_blob_with_an_odd_or_non_hex_digit_is_a_syntax_error() {
        assert_eq!(syntax_position("  X'abc'"), 2);
        assert_eq!(syntax_position("X'zz'"), 0);
    }

    #[test]
    fn a_character_sql_has_no_use_for_is_a_syntax_error() {
        assert_eq!(syntax_position("SELECT # FROM t"), 7);
        assert_eq!(syntax_position("a ! b"), 2);
    }

    #[test]
    fn lines_and_columns_count_characters_from_one() {
        assert_eq!(line_column("abc", 0), (1, 1));
        assert_eq!(line_column("abc", 3), (1, 4));
        assert_eq!(line_column("a\nbc", 3), (2, 2));
        assert_eq!(line_column("'김'x", 5), (1, 4));
        assert_eq!(line_column("\t\tx", 2), (1, 3));
        assert_eq!(line_column("a\r\nb", 3), (2, 1));
        // Inside a character it counts the character it is in.
        assert_eq!(line_column("김", 1), (1, 1));
        assert_eq!(line_column("ab", 99), (1, 3));
    }

    #[test]
    fn comments_are_unsupported_where_they_open() {
        for (sql, opener) in [("-- x", "--"), ("a /* b */", "/*"), ("a\n--", "--")] {
            match tokenize(sql) {
                Err(SqlError::Unsupported(message)) => {
                    assert!(message.contains(opener), "{message}")
                }
                other => panic!("{sql:?}: {other:?}"),
            }
        }
        // A minus before a minus with a space between is two minuses.
        assert_eq!(
            tokens("- -1"),
            vec![
                Token::Symbol("-"),
                Token::Symbol("-"),
                Token::Integer("1".into()),
                Token::End
            ]
        );
    }

    #[test]
    fn digits_run_into_letters_is_a_syntax_error() {
        assert_eq!(syntax_position("SELECT 12ab"), 7);
    }
}
