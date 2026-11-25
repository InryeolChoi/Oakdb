//! The checker (design 0024 V4): direct checks on the values first, then a
//! graph of who depends on whom, whose cycles are sorted by Adya's rules
//! for snapshot isolation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use crate::{Elem, History, Op, Outcome, TxnId};

/// Cycles reported of each kind at most: past that, more say nothing new.
const MAX_CYCLES: usize = 100;

/// The kind of an [`Anomaly`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// G1a: a read saw a write that never committed.
    AbortedRead,
    /// G1b: a read saw a transaction's write that it later wrote over.
    /// The same read also makes a [`Kind::GSingle`] cycle (it read the
    /// first write and missed the last), so both are reported.
    IntermediateRead,
    /// A committed write is gone: another was made on the list it saw
    /// before it.
    LostUpdate,
    /// A transaction read a key twice and saw it change, though it did not
    /// write it in between, or did and did not see that.
    NonRepeatableRead,
    /// A transaction that began after a commit came back did not see it.
    StaleRead,
    /// G0, G1c: a cycle of writes and reads alone.
    CommitCycle,
    /// G-single: a cycle with one anti-dependency, as read skew or a
    /// fractured read makes.
    GSingle,
}

/// How one transaction depends on another, in a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The next one wrote the key right after this one.
    Ww,
    /// The next one read what this one wrote.
    Wr,
    /// The next one wrote over what this one read: an anti-dependency.
    Rw,
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Edge::Ww => "ww",
            Edge::Wr => "wr",
            Edge::Rw => "rw",
        })
    }
}

/// What snapshot isolation forbids, as found in a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anomaly {
    AbortedRead {
        reader: TxnId,
        key: String,
        elem: Elem,
    },
    IntermediateRead {
        reader: TxnId,
        key: String,
        elem: Elem,
    },
    /// `lost` is not in the key's history; `instead` is what took its
    /// place there, if anything did.
    LostUpdate {
        key: String,
        lost: Elem,
        instead: Option<Elem>,
    },
    NonRepeatableRead {
        txn: TxnId,
        key: String,
        expected: Vec<Elem>,
        read: Vec<Elem>,
    },
    StaleRead {
        reader: TxnId,
        writer: TxnId,
        key: String,
    },
    /// Each transaction with how it leads to the next; the last leads to
    /// the first.
    CommitCycle {
        cycle: Vec<(TxnId, Edge)>,
    },
    GSingle {
        cycle: Vec<(TxnId, Edge)>,
    },
}

impl Anomaly {
    pub fn kind(&self) -> Kind {
        match self {
            Anomaly::AbortedRead { .. } => Kind::AbortedRead,
            Anomaly::IntermediateRead { .. } => Kind::IntermediateRead,
            Anomaly::LostUpdate { .. } => Kind::LostUpdate,
            Anomaly::NonRepeatableRead { .. } => Kind::NonRepeatableRead,
            Anomaly::StaleRead { .. } => Kind::StaleRead,
            Anomaly::CommitCycle { .. } => Kind::CommitCycle,
            Anomaly::GSingle { .. } => Kind::GSingle,
        }
    }
}

/// A list as text, only its last few names if it is long.
fn list(elems: &[Elem]) -> String {
    const SHOWN: usize = 4;
    let from = elems.len().saturating_sub(SHOWN);
    let names: Vec<String> = elems[from..].iter().map(Elem::to_string).collect();
    let more = if from > 0 {
        format!("{from} more,")
    } else {
        String::new()
    };
    format!("[{more}{}]", names.join(","))
}

fn cycle(f: &mut fmt::Formatter<'_>, cycle: &[(TxnId, Edge)]) -> fmt::Result {
    for (txn, edge) in cycle {
        write!(f, "{txn} -{edge}-> ")?;
    }
    match cycle.first() {
        Some((first, _)) => write!(f, "{first}"),
        None => Ok(()),
    }
}

impl fmt::Display for Anomaly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Anomaly::AbortedRead { reader, key, elem } => write!(
                f,
                "aborted read: {reader} read {elem} in {key}, which was never committed"
            ),
            Anomaly::IntermediateRead { reader, key, elem } => write!(
                f,
                "intermediate read: {reader} read {key} ending in {elem}, which its writer wrote over"
            ),
            Anomaly::LostUpdate {
                key,
                lost,
                instead: Some(instead),
            } => write!(
                f,
                "lost update: in {key}, {lost} and {instead} were appended to the same list"
            ),
            Anomaly::LostUpdate {
                key,
                lost,
                instead: None,
            } => write!(
                f,
                "lost update: {lost} committed, but {key} does not hold it at the end"
            ),
            Anomaly::NonRepeatableRead {
                txn,
                key,
                expected,
                read,
            } => write!(
                f,
                "non-repeatable read: {txn} should have read {key} as {}, but read {}",
                list(expected),
                list(read)
            ),
            Anomaly::StaleRead {
                reader,
                writer,
                key,
            } => write!(
                f,
                "stale read: {reader} began after {writer} committed, but did not see its write to {key}"
            ),
            Anomaly::CommitCycle { cycle: c } => {
                write!(f, "commit cycle (G0/G1c): ")?;
                cycle(f, c)
            }
            Anomaly::GSingle { cycle: c } => {
                write!(f, "read skew (G-single): ")?;
                cycle(f, c)
            }
        }
    }
}

/// What [`check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub anomalies: Vec<Anomaly>,
    /// Transactions in the history, and those that committed.
    pub txns: usize,
    pub committed: usize,
    /// Anti-dependencies on a cycle with at least two of them: the shape
    /// write skew makes, which snapshot isolation allows. Counted to show
    /// the run made some.
    pub write_skews: usize,
}

/// A transaction as the checker sees it; the last reads are one too.
struct View {
    id: TxnId,
    outcome: Outcome,
    begin: u64,
    end: u64,
    ops: Vec<Op>,
}

/// A transaction's first read of a key before it wrote it: what its
/// snapshot held there.
struct Snap<'h> {
    txn: usize,
    key: &'h str,
    value: &'h [Elem],
}

/// Why a history cannot be checked: it is not one a run could write down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidHistory {
    /// Two transactions share an id, or one uses [`TxnId::LAST`].
    SameId(TxnId),
    /// A transaction appended a name that is another transaction's.
    NotItsOwn { txn: TxnId, elem: Elem },
    /// Two appends used the same name.
    AppendedTwice(Elem),
}

impl fmt::Display for InvalidHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidHistory::SameId(id) => {
                write!(f, "malformed history: transaction id {id} is used twice")
            }
            InvalidHistory::NotItsOwn { txn, elem } => write!(
                f,
                "malformed history: {txn} appended {elem}, a name of another transaction"
            ),
            InvalidHistory::AppendedTwice(elem) => {
                write!(f, "malformed history: {elem} is appended twice")
            }
        }
    }
}

impl std::error::Error for InvalidHistory {}

/// Every transaction id once, and every append a name of its own
/// transaction used once: what [`Checker`] relies on.
fn validate(history: &History) -> Result<(), InvalidHistory> {
    let mut ids = HashSet::from([TxnId::LAST]);
    let mut names = HashSet::new();
    for txn in &history.txns {
        if !ids.insert(txn.id) {
            return Err(InvalidHistory::SameId(txn.id));
        }
        for op in &txn.ops {
            if let Op::Append { elem, .. } = op {
                if elem.txn != txn.id {
                    return Err(InvalidHistory::NotItsOwn {
                        txn: txn.id,
                        elem: *elem,
                    });
                }
                if !names.insert(*elem) {
                    return Err(InvalidHistory::AppendedTwice(*elem));
                }
            }
        }
    }
    Ok(())
}

/// Checks `history` for what snapshot isolation forbids.
///
/// # Errors
///
/// [`InvalidHistory`] if no run could have written it down, for example a
/// hand-edited file where two appends use the same name.
pub fn check(history: &History) -> Result<Report, InvalidHistory> {
    validate(history)?;
    let mut txns: Vec<View> = history
        .txns
        .iter()
        .map(|t| View {
            id: t.id,
            outcome: t.outcome,
            begin: t.begin,
            end: t.end,
            ops: t.ops.clone(),
        })
        .collect();
    if let Some(last) = &history.last {
        let at = txns.iter().map(|t| t.end).max().unwrap_or(0) + 1;
        txns.push(View {
            id: TxnId::LAST,
            outcome: Outcome::Ok,
            begin: at,
            end: at,
            ops: last
                .iter()
                .map(|(key, value)| Op::Read {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        });
    }
    let mut checker = Checker::new(&txns, history);
    checker.reads();
    checker.orders();
    checker.stale_reads();
    checker.cycles();
    Ok(Report {
        txns: history.txns.len(),
        committed: checker.committed.iter().filter(|c| **c).count()
            - usize::from(history.last.is_some()),
        write_skews: checker.write_skews,
        anomalies: checker.anomalies,
    })
}

struct Checker<'h> {
    txns: &'h [View],
    history: &'h History,
    /// Who appended each name, and to which key.
    writer: HashMap<Elem, (usize, &'h str)>,
    /// Names their transaction appended to the same key again after.
    written_over: HashSet<Elem>,
    committed: Vec<bool>,
    snaps: Vec<Snap<'h>>,
    /// Every read of a committed transaction, by key.
    reads_by_key: HashMap<&'h str, Vec<&'h [Elem]>>,
    /// Each key's history: its longest list read that holds only
    /// committed names.
    order: HashMap<&'h str, &'h [Elem]>,
    anomalies: Vec<Anomaly>,
    reported: HashSet<(usize, Elem)>,
    write_skews: usize,
}

impl<'h> Checker<'h> {
    fn new(txns: &'h [View], history: &'h History) -> Self {
        let mut writer = HashMap::new();
        let mut written_over = HashSet::new();
        for (at, txn) in txns.iter().enumerate() {
            let mut latest: HashMap<&str, Elem> = HashMap::new();
            for op in &txn.ops {
                if let Op::Append { key, elem } = op {
                    // `validate` made every name appear once.
                    writer.insert(*elem, (at, key.as_str()));
                    if let Some(earlier) = latest.insert(key, *elem) {
                        written_over.insert(earlier);
                    }
                }
            }
        }
        // A commit of unknown outcome counts as committed if its writes are
        // there at the end, or, with no reads at the end, if anyone saw them.
        let seen: HashSet<Elem> = match &history.last {
            Some(last) => last.values().flatten().copied().collect(),
            None => txns
                .iter()
                .flat_map(|t| &t.ops)
                .filter_map(|op| match op {
                    Op::Read { value, .. } => Some(value),
                    Op::Append { .. } => None,
                })
                .flatten()
                .copied()
                .collect(),
        };
        let committed = txns
            .iter()
            .map(|txn| match txn.outcome {
                Outcome::Ok => true,
                Outcome::Fail => false,
                Outcome::Info => txn.ops.iter().any(|op| match op {
                    Op::Append { elem, .. } => seen.contains(elem),
                    Op::Read { .. } => false,
                }),
            })
            .collect();
        Checker {
            txns,
            history,
            writer,
            written_over,
            committed,
            snaps: Vec::new(),
            reads_by_key: HashMap::new(),
            order: HashMap::new(),
            anomalies: Vec::new(),
            reported: HashSet::new(),
            write_skews: 0,
        }
    }

    /// Whether `elem` was written by `reader` itself or a committed
    /// transaction.
    fn readable(&self, reader: usize, elem: &Elem) -> bool {
        matches!(self.writer.get(elem), Some(&(w, _)) if w == reader || self.committed[w])
    }

    /// Checks each read on its own and within its transaction, and notes
    /// what each snapshot held.
    fn reads(&mut self) {
        for (at, txn) in self.txns.iter().enumerate() {
            // What the transaction should read next under each key: its
            // last read with its own appends since. `None` once it
            // appended to a key it had not read.
            let mut expected: HashMap<&str, Option<Vec<Elem>>> = HashMap::new();
            for op in &txn.ops {
                match op {
                    Op::Read { key, value } => {
                        for elem in value {
                            if !self.readable(at, elem) && self.reported.insert((at, *elem)) {
                                self.anomalies.push(Anomaly::AbortedRead {
                                    reader: txn.id,
                                    key: key.clone(),
                                    elem: *elem,
                                });
                            }
                        }
                        if let Some(&elem) = value.last() {
                            if let Some(&(w, _)) = self.writer.get(&elem) {
                                if w != at
                                    && self.committed[w]
                                    && self.written_over.contains(&elem)
                                    && self.reported.insert((at, elem))
                                {
                                    self.anomalies.push(Anomaly::IntermediateRead {
                                        reader: txn.id,
                                        key: key.clone(),
                                        elem,
                                    });
                                }
                            }
                        }
                        match expected.get(key.as_str()) {
                            Some(Some(want)) if want != value => {
                                self.anomalies.push(Anomaly::NonRepeatableRead {
                                    txn: txn.id,
                                    key: key.clone(),
                                    expected: want.clone(),
                                    read: value.clone(),
                                });
                            }
                            Some(_) => {}
                            None => self.snaps.push(Snap {
                                txn: at,
                                key,
                                value,
                            }),
                        }
                        expected.insert(key, Some(value.clone()));
                        if self.committed[at] {
                            self.reads_by_key.entry(key).or_default().push(value);
                        }
                    }
                    Op::Append { key, elem } => {
                        let next = expected.get(key.as_str()).cloned().flatten().map(|mut v| {
                            v.push(*elem);
                            v
                        });
                        expected.insert(key, next);
                    }
                }
            }
        }
    }

    /// Finds each key's history, and the lists that disagree with it.
    fn orders(&mut self) {
        let mut keys: Vec<&str> = self.reads_by_key.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let lists: Vec<&[Elem]> = self.reads_by_key[key]
                .iter()
                .copied()
                .filter(|list| {
                    list.iter()
                        .all(|e| self.writer.get(e).is_some_and(|&(w, _)| self.committed[w]))
                })
                .collect();
            let longest = lists.iter().copied().max_by_key(|l| l.len()).unwrap_or(&[]);
            for list in &lists {
                if let Some(d) = (0..list.len()).find(|&d| longest.get(d) != Some(&list[d])) {
                    let (lost, instead) = (list[d], longest.get(d).copied());
                    if self.reported.insert((usize::MAX, lost)) {
                        self.anomalies.push(Anomaly::LostUpdate {
                            key: key.to_string(),
                            lost,
                            instead,
                        });
                    }
                }
            }
            self.order.insert(key, longest);
        }
        // With reads at the end, every committed write must be there.
        if let Some(last) = &self.history.last {
            for (at, txn) in self.txns.iter().enumerate() {
                if !self.committed[at] || txn.id == TxnId::LAST {
                    continue;
                }
                for op in &txn.ops {
                    if let Op::Append { key, elem } = op {
                        let there = last.get(key).is_some_and(|l| l.contains(elem));
                        if !there && self.reported.insert((usize::MAX, *elem)) {
                            self.anomalies.push(Anomaly::LostUpdate {
                                key: key.clone(),
                                lost: *elem,
                                instead: None,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Whether `snap` is part of its key's history.
    fn agrees(&self, snap: &Snap<'_>) -> bool {
        self.order
            .get(snap.key)
            .is_some_and(|order| order.starts_with(snap.value))
    }

    /// Finds snapshots that miss a commit that came back before they began.
    fn stale_reads(&mut self) {
        // Per key: each committed write's end and place in the history.
        let mut place: HashMap<Elem, usize> = HashMap::new();
        for order in self.order.values() {
            for (at, elem) in order.iter().enumerate() {
                place.insert(*elem, at);
            }
        }
        let mut writes: HashMap<&str, Vec<(u64, usize, usize)>> = HashMap::new();
        for (at, txn) in self.txns.iter().enumerate() {
            if txn.outcome != Outcome::Ok || txn.id == TxnId::LAST {
                continue;
            }
            for op in &txn.ops {
                if let Op::Append { key, elem } = op {
                    if let Some(&p) = place.get(elem) {
                        writes.entry(key).or_default().push((txn.end, p, at));
                    }
                }
            }
        }
        // Sorted by end, each with the furthest place up to it.
        for list in writes.values_mut() {
            list.sort_unstable();
            let mut furthest: Option<(usize, usize)> = None;
            for entry in list.iter_mut() {
                let (place, writer) = match furthest {
                    Some(f) if f.0 >= entry.1 => f,
                    _ => (entry.1, entry.2),
                };
                furthest = Some((place, writer));
                (entry.1, entry.2) = (place, writer);
            }
        }
        for snap in &self.snaps {
            if !self.agrees(snap) {
                continue;
            }
            let Some(list) = writes.get(snap.key) else {
                continue;
            };
            let begin = self.txns[snap.txn].begin;
            let before = list.partition_point(|&(end, _, _)| end < begin);
            if let Some(&(_, furthest, writer)) = before.checked_sub(1).map(|i| &list[i]) {
                if furthest >= snap.value.len() && writer != snap.txn {
                    self.anomalies.push(Anomaly::StaleRead {
                        reader: self.txns[snap.txn].id,
                        writer: self.txns[writer].id,
                        key: snap.key.to_string(),
                    });
                }
            }
        }
    }

    /// Builds the graph and sorts its cycles. Its writes are the committed
    /// ones; its reads are every transaction's snapshot, those that did
    /// not commit too: an aborted transaction writes nothing, but it read a
    /// snapshot all the same, which must be one moment. So a transaction
    /// that lost a conflict and read two moments is found too.
    fn cycles(&mut self) {
        let n = self.txns.len();
        let mut edges: Vec<Vec<(usize, Edge)>> = vec![Vec::new(); n];
        for order in self.order.values() {
            for pair in order.windows(2) {
                let (a, b) = (self.writer[&pair[0]].0, self.writer[&pair[1]].0);
                if a != b {
                    edges[a].push((b, Edge::Ww));
                }
            }
        }
        for snap in &self.snaps {
            if !self.agrees(snap) {
                continue;
            }
            let r = snap.txn;
            if let Some(last) = snap.value.last() {
                let w = self.writer[last].0;
                if w != r {
                    edges[w].push((r, Edge::Wr));
                }
            }
            if let Some(next) = self.order[snap.key].get(snap.value.len()) {
                let w = self.writer[next].0;
                if w != r {
                    edges[r].push((w, Edge::Rw));
                }
            }
        }
        let strong: Vec<Vec<(usize, Edge)>> = edges
            .iter()
            .map(|out| out.iter().copied().filter(|e| e.1 != Edge::Rw).collect())
            .collect();

        // Cycles of writes and reads alone.
        let (strong_scc, count) = components(&strong);
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (node, &c) in strong_scc.iter().enumerate() {
            members[c].push(node);
        }
        let mut cycles = 0;
        for group in members.iter().filter(|g| g.len() > 1) {
            if cycles == MAX_CYCLES {
                break;
            }
            let start = group[0];
            let within = |node: usize| strong_scc[node] == strong_scc[start];
            for &(next, edge) in &strong[start] {
                if let Some(path) = path(&strong, next, start, within) {
                    let mut cycle = vec![(self.txns[start].id, edge)];
                    cycle.extend(path.into_iter().map(|(v, e)| (self.txns[v].id, e)));
                    self.anomalies.push(Anomaly::CommitCycle { cycle });
                    cycles += 1;
                    break;
                }
            }
        }

        // Cycles with exactly one anti-dependency are G-single; those with
        // more are allowed. An anti-dependency r -> w is on a cycle if both
        // are in one component of the whole graph, and on a G-single one
        // if w reaches r by writes and reads alone. Where those form no
        // cycle, a topological order rules most out at once.
        let (scc, _) = components(&edges);
        let topo = topological(&strong);
        let mut singles = 0;
        for r in 0..n {
            for &(w, edge) in &edges[r] {
                if edge != Edge::Rw || scc[r] != scc[w] {
                    continue;
                }
                let found = match &topo {
                    Some(rank) if rank[w] >= rank[r] => None,
                    Some(rank) => path(&strong, w, r, |v| scc[v] == scc[r] && rank[v] <= rank[r]),
                    None => path(&strong, w, r, |v| scc[v] == scc[r]),
                };
                match found {
                    Some(path) if singles < MAX_CYCLES => {
                        let mut cycle = vec![(self.txns[r].id, Edge::Rw)];
                        cycle.extend(path.into_iter().map(|(v, e)| (self.txns[v].id, e)));
                        self.anomalies.push(Anomaly::GSingle { cycle });
                        singles += 1;
                    }
                    Some(_) => {}
                    None => self.write_skews += 1,
                }
            }
        }
    }
}

/// The strongly connected component of each node (Tarjan), and how many.
fn components(edges: &[Vec<(usize, Edge)>]) -> (Vec<usize>, usize) {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut component = vec![usize::MAX; n];
    let mut count = 0;
    let mut next = 0;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // Each frame: a node and how many of its edges are done.
        let mut frames = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut done)) = frames.last_mut() {
            if let Some(&(w, _)) = edges[v].get(*done) {
                *done += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component[w] = count;
                    if w == v {
                        break;
                    }
                }
                count += 1;
            }
        }
    }
    (component, count)
}

/// Each node's place in a topological order, if the graph has no cycle.
fn topological(edges: &[Vec<(usize, Edge)>]) -> Option<Vec<usize>> {
    let n = edges.len();
    let mut into = vec![0usize; n];
    for out in edges {
        for &(w, _) in out {
            into[w] += 1;
        }
    }
    let mut ready: VecDeque<usize> = (0..n).filter(|&v| into[v] == 0).collect();
    let mut rank = vec![0; n];
    let mut placed = 0;
    while let Some(v) = ready.pop_front() {
        rank[v] = placed;
        placed += 1;
        for &(w, _) in &edges[v] {
            into[w] -= 1;
            if into[w] == 0 {
                ready.push_back(w);
            }
        }
    }
    (placed == n).then_some(rank)
}

/// A shortest path from `from` to `to` through nodes `within` allows, as
/// each node with the edge it leaves by. Its last edge leads to `to`.
fn path(
    edges: &[Vec<(usize, Edge)>],
    from: usize,
    to: usize,
    within: impl Fn(usize) -> bool,
) -> Option<Vec<(usize, Edge)>> {
    let mut came: HashMap<usize, Option<(usize, Edge)>> = HashMap::from([(from, None)]);
    let mut queue = VecDeque::from([from]);
    while let Some(v) = queue.pop_front() {
        if v == to {
            let mut path = Vec::new();
            let mut at = to;
            while let Some(&Some((before, edge))) = came.get(&at) {
                path.push((before, edge));
                at = before;
            }
            path.reverse();
            return Some(path);
        }
        for &(w, edge) in &edges[v] {
            if within(w) && !came.contains_key(&w) {
                came.insert(w, Some((v, edge)));
                queue.push_back(w);
            }
        }
    }
    None
}
