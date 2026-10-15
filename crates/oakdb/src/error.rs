//! Errors the engine reports to its users.

use std::fmt;
use std::io;

/// Longest key the engine accepts, in bytes (ADR 0008).
pub const MAX_KEY_LEN: usize = 512;

/// Longest key plus value the engine accepts, in bytes.
///
/// Four entries this size fill a page, so a full leaf always splits into
/// two that fit. Values longer than this will go to overflow pages (#18).
pub const MAX_ENTRY_LEN: usize = 1006;

/// Everything that can go wrong in the engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The file system failed.
    Io(io::Error),
    /// A key is longer than [`MAX_KEY_LEN`].
    KeyTooLarge {
        /// Length of the rejected key.
        len: usize,
        /// The limit, [`MAX_KEY_LEN`].
        max: usize,
    },
    /// A key plus its value is longer than [`MAX_ENTRY_LEN`].
    ValueTooLarge {
        /// Length of the rejected value.
        len: usize,
        /// The longest value this key can have: [`MAX_ENTRY_LEN`] minus the
        /// key's length.
        max: usize,
    },
    /// The data file was made with a different page size.
    PageSizeMismatch {
        /// Page size of this build.
        expected: u32,
        /// Page size recorded in the file.
        found: u32,
    },
    /// The data file is damaged or is not an Oakdb file.
    Corrupt(String),
    /// Another [`crate::Db`] already has this database open, in this process
    /// or another one. Two of them writing the same file would silently
    /// overwrite each other's commits.
    AlreadyOpen,
}

/// Result type of the engine.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::KeyTooLarge { len, max } => {
                write!(f, "key is {len} bytes, longer than the {max} byte limit")
            }
            Error::ValueTooLarge { len, max } => write!(
                f,
                "value is {len} bytes, longer than the {max} bytes left for it"
            ),
            Error::PageSizeMismatch { expected, found } => write!(
                f,
                "data file uses {found} byte pages, but this build uses {expected}"
            ),
            Error::Corrupt(reason) => write!(f, "data file is corrupt: {reason}"),
            Error::AlreadyOpen => write!(f, "the database is already open"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// An [`Error::Corrupt`] that says what is wrong with the file.
pub(crate) fn corrupt(reason: &str) -> Error {
    Error::Corrupt(reason.to_owned())
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
