//! Checkpoints (ADR 0007, issue #23, design 0011): move the pages the WAL
//! holds to the data file, so the WAL can start again and opening has less
//! to replay.
//!
//! ```text
//! 1. every page the WAL holds an image of, from the tree, to its place
//!    in the data file → sync
//! 2. a meta page for checkpoint n + 1 to slot (n + 1) % 2, or to both
//!    slots for the first checkpoint → sync
//! 3. the WAL emptied → sync, then a header naming n + 1 → sync
//! ```
//!
//! A power cut at any point leaves something opening can use. Before 2
//! lands, the slot of checkpoint n and its WAL are whole, and every page 1
//! wrote over has an image in that WAL. After 2, the data file holds
//! everything that WAL did, so opening drops it.

use crate::checksum;
use crate::db::page_offset;
use crate::error::{Error, Result};
use crate::fs::FsFile;
use crate::meta::Slot;
use crate::page::PAGE_SIZE;
use crate::tree::Tree;
use crate::wal::Wal;

/// Checkpoints `tree`, the last commit in `wal`, to the data file `file`.
/// Does nothing if the WAL holds no commit. If any step fails, stops the
/// WAL: whether the data file now holds the newer checkpoint is unknown,
/// and a commit appended to a WAL it holds would be dropped on opening.
pub(crate) fn run(file: &impl FsFile, wal: &mut Wal<impl FsFile>, tree: &Tree) -> Result<()> {
    wal.check_running()?;
    if !wal.has_commits() {
        return Ok(());
    }
    let result = write(file, wal, tree);
    if result.is_err() {
        wal.stop();
    }
    result
}

fn write(file: &impl FsFile, wal: &mut Wal<impl FsFile>, tree: &Tree) -> Result<()> {
    // Before anything is written. A number that cannot grow must not wrap
    // around to 0: a slot of 0 loses to the other slot, and opening would
    // drop this WAL as older, with every commit in it.
    let next = wal
        .checkpoint()
        .checked_add(1)
        .ok_or_else(|| Error::Corrupt("checkpoint number cannot grow".into()))?;
    let mut page = [0; PAGE_SIZE];
    // Every page the WAL holds is in the tree: a commit writes only pages
    // of its tree, and a tree never gives pages back (it frees them).
    for &page_no in wal.pages() {
        debug_assert!(page_no < tree.meta().page_count, "page {page_no}");
        page.copy_from_slice(tree.page(page_no).as_bytes());
        checksum::seal(page_no, &mut page);
        file.write_all_at(&page, page_offset(page_no))?;
    }
    file.sync_data()?;
    let slot = Slot {
        meta: tree.meta(),
        checkpoint: next,
    };
    // The first checkpoint fills both slots, so every page of the data file
    // has its checksum from the start; later ones write the older slot.
    let slots = if next == 1 {
        0..2
    } else {
        next % 2..next % 2 + 1
    };
    for slot_no in slots {
        let mut page = slot.encode();
        checksum::seal(slot_no, &mut page);
        file.write_all_at(&page, page_offset(slot_no))?;
    }
    file.sync_data()?;
    wal.start(next)
}
