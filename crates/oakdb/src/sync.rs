//! The locks and channels the commit thread and its callers share (issue
//! #29, design 0024 V9): std's, or loom's when built with
//! `RUSTFLAGS="--cfg loom"`, so loom can run every order the threads can
//! take through [`crate::commit_queue`]. The engine takes these from here,
//! never from `std::sync` directly.

#[cfg(loom)]
pub(crate) use loom::sync::{mpsc, Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
pub(crate) use std::sync::{mpsc, Condvar, Mutex, MutexGuard};
