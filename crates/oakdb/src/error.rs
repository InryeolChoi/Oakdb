//! Errors the engine reports to its users.

use std::fmt;
use std::io;
use std::sync::Arc;

/// Longest key the engine accepts, in bytes (ADR 0008).
pub const MAX_KEY_LEN: usize = 512;

/// Longest key plus value kept inside a leaf page, in bytes.
///
/// Four entries this size fill a page, so a full leaf always splits into
/// two that fit. A longer value goes, whole, to overflow pages, and its
/// leaf keeps only where to find it.
pub const MAX_ENTRY_LEN: usize = 1006;

/// Longest value the engine accepts, in bytes: about 4 GiB, what the
/// length field of an overflow value holds. The whole database is kept in
/// memory for now, so memory is the real limit.
pub const MAX_VALUE_LEN: usize = u32::MAX as usize;

/// Everything that can go wrong in the engine.
///
/// It clones cheaply: an I/O error is shared, not copied, so the error
/// every request reports after the engine stops is the very one the disk
/// gave, with its kind and OS error code ([`io::Error::raw_os_error`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// The file system failed: while opening, or in a checkpoint called
    /// directly ([`crate::Db::checkpoint`], [`crate::Db::close`]). A
    /// checkpoint leaves no commit's outcome in doubt, so it is not
    /// [`Error::OutcomeUnknown`], but the engine stops after it all the
    /// same.
    Io(Arc<io::Error>),
    /// Writing or syncing this commit failed, so whether it is on disk is
    /// unknown: opening the database again shows it either completely or
    /// not at all. The engine stopped (ADR 0007): it does not retry, since
    /// a sync that failed once may say it worked the next time although
    /// the data never reached the disk. Drop the database and open it
    /// again.
    OutcomeUnknown(Arc<io::Error>),
    /// The engine stopped after the disk failed, and this request did
    /// nothing. Holds the error the failing call got, which made it stop.
    /// Drop the database and open it again: opening replays the WAL, and
    /// every acknowledged commit is there.
    Stopped(Arc<Error>),
    /// A key is longer than [`MAX_KEY_LEN`].
    KeyTooLarge {
        /// Length of the rejected key.
        len: usize,
        /// The limit, [`MAX_KEY_LEN`].
        max: usize,
    },
    /// A value is longer than [`MAX_VALUE_LEN`].
    ValueTooLarge {
        /// Length of the rejected value.
        len: usize,
        /// The limit, [`MAX_VALUE_LEN`].
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
    /// Another transaction wrote `key`, which this one writes too, and
    /// committed after this one began (issue #26, ADR 0006: first
    /// committer wins). This commit wrote nothing, and the engine goes on:
    /// begin a new transaction, which reads the winner's write, and try
    /// again.
    Conflict {
        /// A key both wrote. There may be more.
        key: Vec<u8>,
    },
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
            Error::ValueTooLarge { len, max } => {
                write!(f, "value is {len} bytes, longer than the {max} byte limit")
            }
            Error::PageSizeMismatch { expected, found } => write!(
                f,
                "data file uses {found} byte pages, but this build uses {expected}"
            ),
            Error::Corrupt(reason) => write!(f, "data file is corrupt: {reason}"),
            Error::AlreadyOpen => write!(f, "the database is already open"),
            Error::Conflict { key } => write!(
                f,
                "another transaction committed a write to key {} since this one began; \
                 begin again and retry",
                printable(key)
            ),
            Error::OutcomeUnknown(e) => write!(
                f,
                "the disk failed during the commit, so its outcome is unknown, \
                 and the engine stopped: {e}"
            ),
            Error::Stopped(cause) => write!(
                f,
                "the engine stopped after the disk failed; open the database again: {cause}"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) | Error::OutcomeUnknown(e) => Some(&**e),
            Error::Stopped(cause) => Some(&**cause),
            _ => None,
        }
    }
}

/// `key` as text if it is printable ASCII, or else in hex, cut short past
/// 64 bytes so an error message stays short.
fn printable(key: &[u8]) -> String {
    let shown = &key[..key.len().min(64)];
    let more = if key.len() > shown.len() { "…" } else { "" };
    if shown.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("\"{}{more}\"", String::from_utf8_lossy(shown))
    } else {
        let hex: String = shown.iter().map(|b| format!("{b:02x}")).collect();
        format!("0x{hex}{more}")
    }
}

/// An [`Error::Corrupt`] that says what is wrong with the file.
pub(crate) fn corrupt(reason: &str) -> Error {
    Error::Corrupt(reason.to_owned())
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(Arc::new(e))
    }
}
