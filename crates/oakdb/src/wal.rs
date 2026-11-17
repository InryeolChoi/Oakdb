//! The write-ahead log (ADR 0007, issue #22).
//!
//! A commit appends a full image of every page it changed, then a commit
//! record holding the new meta fields, and syncs once before it answers.
//! The data file is only written by checkpoints (#23); until then the WAL
//! holds the whole database, and opening replays it.
//!
//! ```text
//! page record    kind 1 (u8) · page_no (u64) · page (4096 bytes) · crc (u32)
//! commit record  kind 2 (u8) · root · page_count · free_head · free_count
//!                (u64 each, free_head 0 = none) · crc (u32)
//! ```
//!
//! Integers are little-endian. Each record's crc is the CRC-32C of its
//! bytes before the crc, carried on from the crc of the record before it
//! ([`START`] for the first). A record only checks out in the place it was
//! written, after the records it was written after, so a stale record left
//! behind a shorter one never joins the chain. Replay stops at the first
//! record that does not check out, and page records count only once a
//! commit record follows them.

use std::collections::BTreeMap;

use crate::meta::Meta;
use crate::page::PAGE_SIZE;

/// Name of the WAL file inside the database directory.
pub const WAL_FILE: &str = "oakdb.wal";

/// The crc the chain starts from. Not 0, so a WAL of zeros never checks
/// out.
pub(crate) const START: u32 = 0x0AC_DB_10;

/// What replaying a WAL found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// The latest committed image of each page the WAL holds.
    pub(crate) pages: BTreeMap<u64, Box<[u8]>>,
    /// The meta fields of the last commit, if there is one.
    pub(crate) meta: Option<Meta>,
    /// Where the last commit ends: anything after it is to be cut off.
    pub(crate) end: u64,
    /// The crc of the last commit record, which the next record carries on.
    pub(crate) crc: u32,
}

/// The records of one commit: `pages` (number and image), then a commit
/// record for `meta`, chained on from `prev`. Returns the bytes and the
/// crc the next commit carries on from.
pub(crate) fn encode_commit<'p>(
    prev: u32,
    pages: impl IntoIterator<Item = (u64, &'p [u8; PAGE_SIZE])>,
    meta: Meta,
) -> (Vec<u8>, u32) {
    let _ = (prev, pages.into_iter().count(), meta);
    todo!()
}

/// Replays the WAL `bytes`: every commit up to the first record that does
/// not check out.
pub(crate) fn replay(bytes: &[u8]) -> Replayed {
    let _ = bytes;
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(byte: u8) -> [u8; PAGE_SIZE] {
        [byte; PAGE_SIZE]
    }

    fn meta(root: u64) -> Meta {
        Meta {
            root_page: root,
            page_count: 5,
            free_head: Some(4),
            free_count: 1,
        }
    }

    /// Two commits: pages 2 and 3, then page 3 again.
    fn two_commits() -> (Vec<u8>, usize, u32) {
        let (mut wal, crc) = encode_commit(START, [(2, &page(1)), (3, &page(2))], meta(2));
        let first = wal.len();
        let (second, crc) = encode_commit(crc, [(3, &page(3))], meta(3));
        wal.extend(second);
        (wal, first, crc)
    }

    #[test]
    fn an_empty_wal_holds_nothing() {
        assert_eq!(
            replay(&[]),
            Replayed {
                pages: BTreeMap::new(),
                meta: None,
                end: 0,
                crc: START,
            }
        );
    }

    #[test]
    fn replay_gives_the_latest_image_of_each_page_and_the_last_meta() {
        let (wal, _, crc) = two_commits();

        let replayed = replay(&wal);

        assert_eq!(replayed.meta, Some(meta(3)));
        assert_eq!(replayed.pages.len(), 2);
        assert_eq!(*replayed.pages[&2], page(1));
        assert_eq!(*replayed.pages[&3], page(3));
        assert_eq!(replayed.end, wal.len() as u64);
        assert_eq!(replayed.crc, crc);
    }

    #[test]
    fn a_commit_is_one_record_per_page_and_a_commit_record() {
        let (one, _) = encode_commit(START, [(2, &page(1))], meta(2));
        let (two, _) = encode_commit(START, [(2, &page(1)), (3, &page(1))], meta(2));
        let page_record = 1 + 8 + PAGE_SIZE + 4;
        assert_eq!(two.len() - one.len(), page_record);
        assert_eq!(one.len(), page_record + 1 + 4 * 8 + 4);
    }

    #[test]
    fn replay_stops_at_the_first_record_that_does_not_check_out() {
        let (mut wal, first, _) = two_commits();
        // One bit of the second commit's page image.
        wal[first + 100] ^= 1;

        let replayed = replay(&wal);

        assert_eq!(replayed.meta, Some(meta(2)));
        assert_eq!(*replayed.pages[&3], page(2));
        assert_eq!(replayed.end, first as u64);
    }

    #[test]
    fn pages_without_a_commit_record_are_not_applied() {
        let (wal, first, _) = two_commits();
        // The second commit's page record is whole; its commit record is
        // cut short.
        for cut in [first + 1, wal.len() - 1 - 4 * 8, wal.len() - 1] {
            let replayed = replay(&wal[..cut]);
            assert_eq!(replayed.meta, Some(meta(2)), "cut at {cut}");
            assert_eq!(*replayed.pages[&3], page(2), "cut at {cut}");
            assert_eq!(replayed.end, first as u64, "cut at {cut}");
        }
    }

    #[test]
    fn a_commit_out_of_its_place_in_the_chain_is_refused() {
        let (wal, first, _) = two_commits();
        // The second commit alone, as if the first had never been written.
        assert_eq!(replay(&wal[first..]).meta, None);
        // The first commit written again after itself.
        let mut twice = wal[..first].to_vec();
        twice.extend_from_slice(&wal[..first]);
        assert_eq!(replay(&twice).end, first as u64);
    }

    #[test]
    fn an_unknown_record_kind_ends_the_replay() {
        let (mut wal, first, _) = two_commits();
        wal[first] = 9;
        assert_eq!(replay(&wal).end, first as u64);
    }

    #[test]
    fn a_commit_without_a_free_list_keeps_none() {
        let meta = Meta {
            free_head: None,
            free_count: 0,
            ..meta(2)
        };
        let (wal, _) = encode_commit(START, [(2, &page(1))], meta);
        assert_eq!(replay(&wal).meta, Some(meta));
    }
}
