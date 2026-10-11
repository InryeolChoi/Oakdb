//! The meta page: what a data file needs to say about itself.
//!
//! Pages 0 and 1 are the two meta slots that checkpoints will alternate
//! between (ADR 0007). Until then both hold the same content and only slot 0
//! is read.
//!
//! ```text
//! offset  size  field
//! 0..4    u32   checksum        reserved for CRC-32C, 0 for now
//! 4..12   [u8]  magic           "oakdb\0\0\0"
//! 12..16  u32   format_version  1
//! 16..20  u32   page_size       4096
//! 20..28  u64   root_page
//! 28..36  u64   page_count
//! ```

use crate::error::{Error, Result};
use crate::page::PAGE_SIZE;

const MAGIC: &[u8; 8] = b"oakdb\0\0\0";
const FORMAT_VERSION: u32 = 1;

/// Bytes at the start of the meta page that hold its fields.
pub(crate) const META_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) root_page: u64,
    pub(crate) page_count: u64,
}

impl Meta {
    /// Encodes a full meta page.
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode_with_page_size(self, PAGE_SIZE as u32)
    }

    /// Reads the meta fields from the first [`META_LEN`] bytes of the file.
    ///
    /// The page size is checked before anything else that depends on it.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Meta> {
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
        let meta = Meta {
            root_page: u64_at(bytes, 20),
            page_count: u64_at(bytes, 28),
        };
        if meta.root_page < 2 || meta.root_page >= meta.page_count {
            return Err(Error::Corrupt("root page is outside the file".into()));
        }
        Ok(meta)
    }
}

fn encode_with_page_size(meta: &Meta, page_size: u32) -> Vec<u8> {
    let mut page = vec![0; PAGE_SIZE];
    page[4..12].copy_from_slice(MAGIC);
    page[12..16].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    page[16..20].copy_from_slice(&page_size.to_le_bytes());
    page[20..28].copy_from_slice(&meta.root_page.to_le_bytes());
    page[28..36].copy_from_slice(&meta.page_count.to_le_bytes());
    page
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A meta page that claims a different page size, for tests elsewhere.
#[cfg(test)]
pub(crate) fn encode_for_page_size(meta: &Meta, page_size: u32) -> Vec<u8> {
    encode_with_page_size(meta, page_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: Meta = Meta {
        root_page: 2,
        page_count: 3,
    };

    #[test]
    fn meta_round_trips() {
        assert_eq!(Meta::decode(&META.encode()).unwrap(), META);
    }

    #[test]
    fn the_page_size_is_recorded_at_offset_16() {
        assert_eq!(&META.encode()[16..20], &4096u32.to_le_bytes());
    }

    #[test]
    fn a_different_page_size_is_refused() {
        let err = Meta::decode(&encode_with_page_size(&META, 8192)).unwrap_err();

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
        let mut bytes = META.encode();
        bytes[4] = b'x';

        assert!(matches!(Meta::decode(&bytes), Err(Error::Corrupt(_))));
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut bytes = META.encode();
        bytes[12] = 2;

        assert!(matches!(Meta::decode(&bytes), Err(Error::Corrupt(_))));
    }

    #[test]
    fn a_root_outside_the_file_is_refused() {
        let meta = Meta {
            root_page: 3,
            page_count: 3,
        };

        assert!(matches!(
            Meta::decode(&meta.encode()),
            Err(Error::Corrupt(_))
        ));
    }
}
