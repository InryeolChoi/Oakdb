//! Oakdb storage engine.
//!
//! The engine is a library so tests can drive it directly and inject faults.
//! All file access goes through [`fs::Fs`], which is the only place that knows
//! about the operating system.

mod checkpoint;
mod checksum;
mod conflict;
mod db;
mod error;
pub mod fs;
mod meta;
mod page;
mod tree;
mod wal;

pub use db::{Db, Scan, Stats, Txn, CHECKPOINT_AFTER, COMMIT_THREAD, DATA_FILE, MAX_BATCH};
pub use error::{Error, Result, MAX_ENTRY_LEN, MAX_KEY_LEN, MAX_VALUE_LEN};
pub use wal::WAL_FILE;
