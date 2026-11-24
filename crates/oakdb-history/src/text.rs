//! A history as text, one transaction to a line, so a run that found an
//! anomaly can be kept and checked again (design 0024 V11):
//!
//! ```text
//! t2#17 ok 1203000 1977000 r(k3)=[t0#4,t1#9] a(k3,t2#17)
//! last r(k0)=[t2#17] r(k1)=[]
//! ```
//!
//! A transaction's line holds its name, outcome (`ok`, `fail`, `info`),
//! when it began and ended, and its steps: `r(key)=[names]` for a read,
//! `a(key,name)` for an append. Keys hold no space, comma or bracket.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::{Elem, History, Op, Outcome, Txn, TxnId};

/// Why text is not a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn list(f: &mut fmt::Formatter<'_>, elems: &[Elem]) -> fmt::Result {
    write!(f, "[")?;
    for (i, elem) in elems.iter().enumerate() {
        if i > 0 {
            write!(f, ",")?;
        }
        write!(f, "{elem}")?;
    }
    write!(f, "]")
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Op::Read { key, value } => {
                write!(f, "r({key})=")?;
                list(f, value)
            }
            Op::Append { key, elem } => write!(f, "a({key},{elem})"),
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Outcome::Ok => "ok",
            Outcome::Fail => "fail",
            Outcome::Info => "info",
        })
    }
}

impl fmt::Display for Txn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.id, self.outcome, self.begin, self.end
        )?;
        for op in &self.ops {
            write!(f, " {op}")?;
        }
        Ok(())
    }
}

impl fmt::Display for History {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for txn in &self.txns {
            writeln!(f, "{txn}")?;
        }
        if let Some(last) = &self.last {
            write!(f, "last")?;
            for (key, value) in last {
                write!(f, " r({key})=")?;
                list(f, value)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

fn txn_id(s: &str) -> Option<TxnId> {
    let (thread, seq) = s.strip_prefix('t')?.split_once('#')?;
    Some(TxnId::new(thread.parse().ok()?, seq.parse().ok()?))
}

fn elem(s: &str) -> Option<Elem> {
    match s.split_once('/') {
        Some((txn, n)) => Some(Elem::new(txn_id(txn)?, n.parse().ok()?)),
        None => Some(Elem::new(txn_id(s)?, 0)),
    }
}

fn op(s: &str) -> Option<Op> {
    if let Some(rest) = s.strip_prefix("r(") {
        let (key, value) = rest.split_once(")=")?;
        let value = value.strip_prefix('[')?.strip_suffix(']')?;
        let value = match value {
            "" => Vec::new(),
            names => names.split(',').map(elem).collect::<Option<_>>()?,
        };
        Some(Op::Read {
            key: key.to_string(),
            value,
        })
    } else {
        let rest = s.strip_prefix("a(")?.strip_suffix(')')?;
        let (key, name) = rest.split_once(',')?;
        Some(Op::Append {
            key: key.to_string(),
            elem: elem(name)?,
        })
    }
}

impl FromStr for History {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let mut history = History::default();
        for (at, line) in s.lines().enumerate() {
            let fail = |message: &str| ParseError {
                line: at + 1,
                message: message.to_string(),
            };
            let mut words = line.split_whitespace();
            let Some(first) = words.next() else { continue };
            if first == "last" {
                let mut last = BTreeMap::new();
                for word in words {
                    match op(word) {
                        Some(Op::Read { key, value }) => {
                            last.insert(key, value);
                        }
                        _ => return Err(fail("the last reads hold only reads")),
                    }
                }
                history.last = Some(last);
                continue;
            }
            let id = txn_id(first).ok_or_else(|| fail("no transaction name"))?;
            let outcome = match words.next() {
                Some("ok") => Outcome::Ok,
                Some("fail") => Outcome::Fail,
                Some("info") => Outcome::Info,
                _ => return Err(fail("no outcome")),
            };
            let mut time = || words.next().and_then(|w| w.parse().ok());
            let begin = time().ok_or_else(|| fail("no begin time"))?;
            let end = time().ok_or_else(|| fail("no end time"))?;
            let ops = words
                .map(op)
                .collect::<Option<_>>()
                .ok_or_else(|| fail("a step that is neither r(..)=[..] nor a(..,..)"))?;
            history.txns.push(Txn {
                id,
                outcome,
                begin,
                end,
                ops,
            });
        }
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_reads_back_as_it_was_written() {
        let t = TxnId::new(2, 17);
        let history = History {
            txns: vec![Txn {
                id: t,
                outcome: Outcome::Info,
                begin: 5,
                end: 9,
                ops: vec![
                    Op::Read {
                        key: "k3".into(),
                        value: vec![
                            Elem::new(TxnId::new(0, 4), 0),
                            Elem::new(TxnId::new(1, 9), 1),
                        ],
                    },
                    Op::Append {
                        key: "k3".into(),
                        elem: Elem::new(t, 0),
                    },
                    Op::Read {
                        key: "k5".into(),
                        value: vec![],
                    },
                ],
            }],
            last: Some(BTreeMap::from([("k3".to_string(), vec![Elem::new(t, 0)])])),
        };
        let text = history.to_string();
        assert_eq!(
            text,
            "t2#17 info 5 9 r(k3)=[t0#4,t1#9/1] a(k3,t2#17) r(k5)=[]\nlast r(k3)=[t2#17]\n"
        );
        assert_eq!(text.parse::<History>().unwrap(), history);
    }

    #[test]
    fn a_line_that_is_not_a_transaction_is_refused_with_its_number() {
        let e = "t0#0 ok 1 2\nt0#1 maybe 1 2\n"
            .parse::<History>()
            .unwrap_err();
        assert_eq!(e.line, 2);
    }
}
