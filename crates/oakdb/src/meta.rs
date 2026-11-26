//! The meta page: what a data file needs to say about itself.
//!
//! Pages 0 and 1 are the two meta slots that checkpoints alternate between
//! (ADR 0007, design 0011). Each holds the meta fields as of a checkpoint
//! and that checkpoint's number; opening uses the slot with the higher
//! number of the two that check out.
//!
//! ```text
//! offset  size  field
//! 0..4    u32   checksum        CRC-32C, filled in when written (checksum.rs)
//! 4..12   [u8]  magic           "oakdb\0\0\0"
//! 12..16  u32   format_version  4
//! 16..20  u32   page_size       4096
//! 20..28  u64   root_page
//! 28..36  u64   page_count
//! 36..44  u64   free_head       first page of the free list, 0 = empty
//! 44..52  u64   free_count      pages on the free list
//! 52..60  u64   checkpoint      the checkpoint this slot was written by
//! ```
//!
//! Version 1 files had no free list and could hold pages in no use at all;
//! version 2 files had no page checksums; version 3 files had one meta slot
//! in use and no checkpoint number. All are refused, by version, before any
//! checksum is checked; no such file holds anyone's data.

use crate::error::{Error, Result};
use crate::page::PAGE_SIZE;

const MAGIC: &[u8; 8] = b"oakdb\0\0\0";
const FORMAT_VERSION: u32 = 4;

/// Bytes at the start of the meta page that hold its fields.
pub(crate) const META_LEN: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) root_page: u64,
    pub(crate) page_count: u64,
    pub(crate) free_head: Option<u64>,
    pub(crate) free_count: u64,
}

/// What a meta slot holds: the meta fields as of a checkpoint, and its
/// number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) meta: Meta,
    pub(crate) checkpoint: u64,
}

impl Slot {
    /// Encodes a full meta page, its checksum left for the caller.
    pub(crate) fn encode(&self) -> [u8; PAGE_SIZE] {
        encode_with_page_size(self, PAGE_SIZE as u32)
            .try_into()
            .expect("the meta page is PAGE_SIZE bytes")
    }

    /// Reads the slot from a meta page whose header [`Meta::check_header`]
    /// has already accepted.
    pub(crate) fn read(bytes: &[u8]) -> Result<Slot> {
        Ok(Slot {
            meta: Meta::read_fields(bytes)?,
            checkpoint: u64_at(bytes, 52),
        })
    }

    /// Checks the header, then reads the slot.
    #[cfg(test)]
    pub(crate) fn decode(bytes: &[u8]) -> Result<Slot> {
        Meta::check_header(bytes)?;
        Slot::read(bytes)
    }
}

impl Meta {
    /// Checks that the first [`META_LEN`] bytes of a file start an Oakdb
    /// data file of this page size and format version.
    ///
    /// This comes before the meta page's checksum: the page size says how
    /// much of the file is the page, and an older version has no checksum
    /// to check, so it is refused by its version.
    pub(crate) fn check_header(bytes: &[u8]) -> Result<()> {
        if bytes.len() < META_LEN || &bytes[4..12] != MAGIC {
            return Err(Error::Corrupt("not an Oakdb data file".into()));
        }
        let page_size = u32_at(bytes, 16);
        if page_size != PAGE_SIZE as u32 {
            return Err(Error::PageSizeMismatch {
                expected: PAGE_SIZE as u32,
                found: page_size,
            });
        }
        let version = u32_at(bytes, 12);
        if version != FORMAT_VERSION {
            return Err(Error::Corrupt(format!("unknown format version {version}")));
        }
        Ok(())
    }

    /// Reads the meta fields from the first [`META_LEN`] bytes of the meta
    /// page, whose header [`Meta::check_header`] has already accepted.
    pub(crate) fn read_fields(bytes: &[u8]) -> Result<Meta> {
        let meta = Meta {
            root_page: u64_at(bytes, 20),
            page_count: u64_at(bytes, 28),
            free_head: Some(u64_at(bytes, 36)).filter(|&page_no| page_no != 0),
            free_count: u64_at(bytes, 44),
        };
        let inside = |page_no: u64| page_no >= 2 && page_no < meta.page_count;
        if !inside(meta.root_page) {
            return Err(Error::Corrupt("root page is outside the file".into()));
        }
        if meta.free_head.is_some_and(|head| !inside(head))
            || (meta.free_head.is_none() != (meta.free_count == 0))
            || meta.free_count >= meta.page_count
        {
            return Err(Error::Corrupt("free list does not fit the file".into()));
        }
        Ok(meta)
    }
}

fn encode_with_page_size(slot: &Slot, page_size: u32) -> Vec<u8> {
    let meta = &slot.meta;
    let mut page = vec![0; PAGE_SIZE];
    page[4..12].copy_from_slice(MAGIC);
    page[12..16].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    page[16..20].copy_from_slice(&page_size.to_le_bytes());
    page[20..28].copy_from_slice(&meta.root_page.to_le_bytes());
    page[28..36].copy_from_slice(&meta.page_count.to_le_bytes());
    page[36..44].copy_from_slice(&meta.free_head.unwrap_or(0).to_le_bytes());
    page[44..52].copy_from_slice(&meta.free_count.to_le_bytes());
    page[52..60].copy_from_slice(&slot.checkpoint.to_le_bytes());
    page
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A meta page that claims a different page size, for `db`'s tests.
#[cfg(all(test, not(loom)))]
pub(crate) fn encode_for_page_size(slot: &Slot, page_size: u32) -> Vec<u8> {
    encode_with_page_size(slot, page_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: Meta = Meta {
        root_page: 2,
        page_count: 3,
        free_head: None,
        free_count: 0,
    };

    fn slot(meta: Meta) -> Slot {
        Slot {
            meta,
            checkpoint: 7,
        }
    }

    fn decode(bytes: &[u8]) -> Result<Meta> {
        Slot::decode(bytes).map(|slot| slot.meta)
    }

    #[test]
    fn a_slot_round_trips_with_its_checkpoint() {
        assert_eq!(Slot::decode(&slot(META).encode()).unwrap(), slot(META));
    }

    #[test]
    fn the_checkpoint_is_recorded_at_offset_52() {
        assert_eq!(&slot(META).encode()[52..60], &7u64.to_le_bytes());
    }

    #[test]
    fn the_page_size_is_recorded_at_offset_16() {
        assert_eq!(&slot(META).encode()[16..20], &4096u32.to_le_bytes());
    }

    #[test]
    fn a_different_page_size_is_refused() {
        let err = decode(&encode_with_page_size(&slot(META), 8192)).unwrap_err();

        assert!(matches!(
            err,
            Error::PageSizeMismatch {
                expected: 4096,
                found: 8192
            }
        ));
    }

    #[test]
    fn a_missing_magic_is_refused() {
        let mut bytes = slot(META).encode();
        bytes[4] = b'x';

        assert!(matches!(decode(&bytes), Err(Error::Corrupt(_))));
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut bytes = slot(META).encode();
        bytes[12] = 5;

        assert!(matches!(decode(&bytes), Err(Error::Corrupt(_))));
    }

    #[test]
    fn version_1_2_and_3_files_are_refused() {
        let mut bytes = slot(META).encode();
        bytes[12] = 3;
        assert!(matches!(decode(&bytes), Err(Error::Corrupt(_))));
        bytes[12] = 2;
        assert!(matches!(decode(&bytes), Err(Error::Corrupt(_))));
        bytes[12] = 1;

        assert!(matches!(decode(&bytes), Err(Error::Corrupt(_))));
    }

    #[test]
    fn a_root_outside_the_file_is_refused() {
        let meta = Meta {
            root_page: 3,
            ..META
        };

        assert!(matches!(
            decode(&slot(meta).encode()),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_free_list_round_trips() {
        let meta = Meta {
            root_page: 2,
            page_count: 9,
            free_head: Some(7),
            free_count: 3,
        };

        assert_eq!(decode(&slot(meta).encode()).unwrap(), meta);
    }

    #[test]
    fn a_free_list_that_does_not_fit_the_file_is_refused() {
        let base = Meta {
            root_page: 2,
            page_count: 9,
            free_head: Some(7),
            free_count: 3,
        };
        for meta in [
            Meta {
                free_head: Some(9),
                ..base
            },
            Meta {
                free_head: Some(1),
                ..base
            },
            Meta {
                free_count: 0,
                ..base
            },
            Meta {
                free_head: None,
                ..base
            },
            Meta {
                free_count: 9,
                ..base
            },
        ] {
            assert!(
                matches!(decode(&slot(meta).encode()), Err(Error::Corrupt(_))),
                "{meta:?}"
            );
        }
    }
}
