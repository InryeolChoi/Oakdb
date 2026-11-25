//! Write conflicts (issue #26, design 0016, ADR 0006): which keys recent
//! commits wrote, so a commit can tell whether another transaction wrote
//! one of its keys since it began. The first to commit wins; the other
//! gets [`crate::Error::Conflict`].
//!
//! Commits are numbered in memory from 1. A transaction remembers the
//! number of the last commit when it began (its snapshot), and conflicts
//! if a commit numbered higher wrote one of its keys. A commit's keys are
//! kept only while an open transaction began before it: no one else can
//! conflict with it. So the table stays as small as the commits made since
//! the oldest open transaction began. The numbers are not saved: after a
//! restart no transaction is open, so none is compared across it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

/// The keys commits wrote, by commit number, for as long as an open
/// transaction could conflict with them.
#[derive(Default)]
pub(crate) struct RecentWrites {
    /// The number of the last commit that wrote anything.
    last: u64,
    /// Each key a kept commit wrote, with the last such commit. The key is
    /// shared with `commits`, so each is stored once.
    by_key: HashMap<Arc<[u8]>, u64>,
    /// The kept commits, oldest first, with the keys each wrote: what
    /// pruning goes through.
    commits: VecDeque<(u64, Vec<Arc<[u8]>>)>,
    /// How many open transactions began after each commit number.
    open: BTreeMap<u64, usize>,
}

impl RecentWrites {
    /// A transaction begins: returns the number of the last commit, which
    /// its snapshot holds, and counts it as open until
    /// [`RecentWrites::end`]. A transaction that never ends (one passed to
    /// `mem::forget`) keeps every commit after it here: conflicts are still
    /// found right, but the table only grows.
    pub(crate) fn begin(&mut self) -> u64 {
        *self.open.entry(self.last).or_default() += 1;
        self.last
    }

    /// A transaction that began after commit `begun_after` ends,
    /// committed or not.
    pub(crate) fn end(&mut self, begun_after: u64) {
        if let Some(count) = self.open.get_mut(&begun_after) {
            *count -= 1;
            if *count == 0 {
                self.open.remove(&begun_after);
            }
        }
        self.prune();
    }

    /// A key of `keys` that a commit after `begun_after` wrote, if any.
    pub(crate) fn conflict<'k>(
        &self,
        begun_after: u64,
        mut keys: impl Iterator<Item = &'k [u8]>,
    ) -> Option<&'k [u8]> {
        keys.find(|key| self.by_key.get(*key).is_some_and(|&n| n > begun_after))
    }

    /// A commit wrote `keys`: gives it the next number.
    pub(crate) fn record(&mut self, keys: impl IntoIterator<Item = Vec<u8>>) {
        self.last += 1;
        let keys: Vec<Arc<[u8]>> = keys.into_iter().map(Arc::from).collect();
        for key in &keys {
            self.by_key.insert(Arc::clone(key), self.last);
        }
        self.commits.push_back((self.last, keys));
        self.prune();
    }

    /// Drops the commits no open transaction began before: every open one
    /// has seen them, so none can conflict with them.
    fn prune(&mut self) {
        let oldest = self.open.keys().next().copied().unwrap_or(self.last);
        while let Some((n, _)) = self.commits.front() {
            if *n > oldest {
                break;
            }
            let (n, keys) = self.commits.pop_front().expect("checked above");
            for key in keys {
                // A later commit may have written the key since; then it
                // stays, under that commit.
                if self.by_key.get(&key) == Some(&n) {
                    self.by_key.remove(&key);
                }
            }
        }
    }

    /// How many transactions are open: begun and not yet ended.
    pub(crate) fn open_transactions(&self) -> usize {
        self.open.values().sum()
    }

    /// How many commits are kept because an open transaction began before
    /// them (issue #37, design 0025 Q7).
    pub(crate) fn kept_commits(&self) -> usize {
        self.commits.len()
    }

    /// How many keys are kept, for tests of pruning.
    #[cfg(test)]
    fn kept(&self) -> usize {
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|k| k.as_bytes().to_vec()).collect()
    }

    fn conflict(writes: &RecentWrites, snapshot: u64, names: &[&str]) -> Option<Vec<u8>> {
        let keys = keys(names);
        writes
            .conflict(snapshot, keys.iter().map(Vec::as_slice))
            .map(<[u8]>::to_vec)
    }

    #[test]
    fn a_commit_after_a_snapshot_conflicts_on_its_keys_only() {
        let mut writes = RecentWrites::default();
        let early = writes.begin();
        writes.record(keys(&["a", "b"]));
        let late = writes.begin();

        assert_eq!(conflict(&writes, early, &["x", "b"]), Some(b"b".to_vec()));
        assert_eq!(conflict(&writes, early, &["x"]), None);
        assert_eq!(conflict(&writes, late, &["a", "b"]), None);
    }

    #[test]
    fn with_no_transaction_open_nothing_is_kept() {
        let mut writes = RecentWrites::default();
        for i in 0..100 {
            let snapshot = writes.begin();
            writes.record(keys(&[&format!("k{i}")]));
            writes.end(snapshot);
        }
        assert_eq!(writes.kept(), 0);
    }

    #[test]
    fn commits_are_kept_while_a_transaction_that_began_before_them_is_open() {
        let mut writes = RecentWrites::default();
        let long = writes.begin();
        for i in 0..10 {
            writes.record(keys(&[&format!("k{i}")]));
        }
        assert_eq!(writes.kept(), 10);
        assert_eq!(conflict(&writes, long, &["k3"]), Some(b"k3".to_vec()));

        writes.end(long);
        assert_eq!(writes.kept(), 0);
    }

    #[test]
    fn pruning_keeps_a_key_a_later_commit_wrote_again() {
        let mut writes = RecentWrites::default();
        let first = writes.begin();
        writes.record(keys(&["k"]));
        let second = writes.begin();
        writes.record(keys(&["k"]));
        // Commit 1 goes once `first` ends; commit 2 wrote "k" too and
        // `second` began before it.
        writes.end(first);
        assert_eq!(conflict(&writes, second, &["k"]), Some(b"k".to_vec()));
        writes.end(second);
        assert_eq!(writes.kept(), 0);
    }

    #[test]
    fn transactions_that_began_together_are_counted_apart() {
        let mut writes = RecentWrites::default();
        let a = writes.begin();
        let b = writes.begin();
        writes.record(keys(&["k"]));
        writes.end(a);
        assert_eq!(conflict(&writes, b, &["k"]), Some(b"k".to_vec()));
    }
}
