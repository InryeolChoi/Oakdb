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
    /// Punctuation or an operator: `(`, `)`, `,`, `;`, `*`, `=`, `-`, and
    /// the ones Oakdb does not support yet (`<`, `>=`, `.`, ...), so the
    /// parser can name them in its error.
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
pub(crate) fn tokenize(sql: &str) -> Result<Vec<Spanned>, SqlError> {
    let _ = sql;
    unimplemented!("lexer")
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
    fn operators_oakdb_does_not_support_still_come_out_as_symbols() {
        assert_eq!(
            tokens("< <= > >= <> != . +"),
            vec![
                Token::Symbol("<"),
                Token::Symbol("<="),
                Token::Symbol(">"),
                Token::Symbol(">="),
                Token::Symbol("<>"),
                Token::Symbol("!="),
                Token::Symbol("."),
                Token::Symbol("+"),
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
    fn digits_run_into_letters_is_a_syntax_error() {
        assert_eq!(syntax_position("SELECT 12ab"), 7);
    }
}
