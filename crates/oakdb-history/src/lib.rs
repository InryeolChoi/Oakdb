//! Checks a history of transactions for snapshot isolation anomalies
//! (issue #29, design 0024).
//!
//! A test runs transactions on many threads and writes down what each one
//! read and wrote: its [`History`]. [`check`] then looks only at that
//! record, never at the engine, for what snapshot isolation forbids: lost
//! updates, reads of data that was never committed, reads of another
//! transaction's middle state, reads that change within a transaction, and
//! reads that mix two moments. Write skew, which snapshot isolation allows
//! (ADR 0006), is counted, not flagged.
//!
//! Every write appends a name no other write uses to a list kept under the
//! key: the transaction reads the list and writes it back one name longer.
//! So a read says who wrote what it saw (the list's last name), and the
//! longest list read says in what order the key was written, as in Elle's
//! list-append workload (Kingsbury and Alvaro, 2020).

mod check;
mod text;

use std::collections::BTreeMap;
use std::fmt;

pub use check::{check, Anomaly, Edge, InvalidHistory, Kind, Report};
pub use text::ParseError;

/// A transaction: the thread that ran it, and its number among that
/// thread's transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TxnId {
    pub thread: u32,
    pub seq: u32,
}

impl TxnId {
    /// The reads made after every transaction ended: [`History::last`].
    pub const LAST: TxnId = TxnId {
        thread: u32::MAX,
        seq: 0,
    };

    pub fn new(thread: u32, seq: u32) -> Self {
        TxnId { thread, seq }
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == TxnId::LAST {
            write!(f, "last")
        } else {
            write!(f, "t{}#{}", self.thread, self.seq)
        }
    }
}

/// A name a transaction appends to a key's list. A transaction that
/// appends more than once numbers its names from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Elem {
    pub txn: TxnId,
    pub n: u32,
}

impl Elem {
    pub fn new(txn: TxnId, n: u32) -> Self {
        Elem { txn, n }
    }
}

impl fmt::Display for Elem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.n {
            0 => write!(f, "{}", self.txn),
            n => write!(f, "{}/{n}", self.txn),
        }
    }
}

/// One step of a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// It read `key` and saw `value`; a key not there reads as empty.
    Read { key: String, value: Vec<Elem> },
    /// It wrote `key` as the list it last saw there with `elem` added.
    Append { key: String, elem: Elem },
}

/// How a transaction's commit came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Committed.
    Ok,
    /// Certainly not committed: a conflict.
    Fail,
    /// Perhaps committed: the outcome is unknown. [`check`] decides from
    /// [`History::last`] if there is one, or else from whether anyone saw
    /// its writes.
    Info,
}

/// What one transaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Txn {
    pub id: TxnId,
    pub outcome: Outcome,
    /// When it began and when its commit came back, in nanoseconds from
    /// any start the whole history shares.
    pub begin: u64,
    pub end: u64,
    pub ops: Vec<Op>,
}

/// What every transaction of a run did, and, if the run read every key at
/// the end, what it read then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub txns: Vec<Txn>,
    /// Every key read after every transaction ended, perhaps after the
    /// database was opened again. It decides which transactions with an
    /// unknown outcome committed.
    pub last: Option<BTreeMap<String, Vec<Elem>>>,
}
