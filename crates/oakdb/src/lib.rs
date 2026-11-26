//! Oakdb storage engine.
//!
//! The engine is a library so tests can drive it directly and inject faults.
//! All file access goes through [`fs::Fs`], which is the only place that knows
//! about the operating system.

mod checkpoint;
mod checksum;
mod commit_queue;
mod conflict;
mod db;
mod error;
pub mod fs;
mod meta;
mod page;
mod planted;
mod sync;
mod tree;
mod wal;

pub use db::{
    check_key, check_value_len, Db, Scan, Stats, Txn, BATCH_SIZE_BUCKETS, CHECKPOINT_AFTER,
    COMMIT_THREAD, DATA_FILE, MAX_BATCH,
};
pub use error::{Error, Result, MAX_ENTRY_LEN, MAX_KEY_LEN, MAX_VALUE_LEN};
#[cfg(feature = "planted-bugs")]
pub use planted::PlantedBug;
pub use wal::WAL_FILE;
