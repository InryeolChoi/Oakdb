//! The executor: runs a parsed statement as one engine transaction
//! (design 0017, section 5).

use oakdb::fs::Fs;
use oakdb::Db;

use crate::parser::Statement;
use crate::{Output, SqlError};

/// Runs `statement` in a transaction of its own, committing it if the
/// statement succeeds.
pub(crate) fn run<F: Fs>(db: &Db<F>, statement: Statement) -> Result<Output, SqlError> {
    let _ = (db, statement);
    unimplemented!("executor")
}
