//! Oakdb storage engine.
//!
//! The engine is a library so tests can drive it directly and inject faults.
//! All file access goes through [`fs::Fs`], which is the only place that knows
//! about the operating system.

mod db;
mod error;
pub mod fs;
mod meta;
mod page;

pub use db::{Db, Txn, DATA_FILE};
pub use error::{Error, Result, MAX_KEY_LEN};
