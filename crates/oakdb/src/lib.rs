//! Oakdb storage engine.
//!
//! The engine is a library so tests can drive it directly and inject faults.
//! All file access goes through [`fs::Fs`], which is the only place that knows
//! about the operating system.

pub mod fs;
