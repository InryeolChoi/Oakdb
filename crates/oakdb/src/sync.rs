//! The locks and channels the commit thread and its callers share (issue
//! #29, design 0024 V9): std's, or loom's when built with
//! `RUSTFLAGS="--cfg loom"`, so loom can run every order the threads can
//! take through [`crate::commit_queue`]. The engine takes its locks,
//! condition variables and channels from here. `Arc`, `OnceLock` and the
//! atomic counters still come from `std::sync`: the loom models drive only
//! the queue, never a `Db`.
//!
//! Under `--cfg loom` these locks panic outside a loom model, so the lib's
//! tests that build a `Db` are compiled only without it (`cfg(not(loom))`).

#[cfg(loom)]
pub(crate) use loom::sync::{mpsc, Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
pub(crate) use std::sync::{mpsc, Condvar, Mutex, MutexGuard};
