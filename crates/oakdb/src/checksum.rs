//! Page checksums (ADR 0008).
//!
//! Every page in the data file starts with a CRC-32C of the rest of the
//! page followed by its page number as 8 little-endian bytes. The page
//! number makes a whole, valid page written in the wrong place fail too.
//! Pages in memory keep 0 in the field: the checksum is filled in on the
//! way to the file and checked on the way back, and a page whose checksum
//! does not match is always an error.

use crate::error::{Error, Result};
use crate::page::PAGE_SIZE;

/// Bytes at the start of every page that hold its checksum.
const CHECKSUM_LEN: usize = 4;

/// The checksum page `page_no` should carry, given its bytes.
fn checksum(page_no: u64, page: &[u8]) -> u32 {
    let crc = crc32c::crc32c(&page[CHECKSUM_LEN..]);
    crc32c::crc32c_append(crc, &page_no.to_le_bytes())
}

/// A copy of `page` with its checksum for `page_no` filled in, ready to be
/// written there.
pub(crate) fn sealed(page_no: u64, page: &[u8]) -> [u8; PAGE_SIZE] {
    let mut out = [0; PAGE_SIZE];
    out.copy_from_slice(page);
    let sum = checksum(page_no, &out);
    out[..CHECKSUM_LEN].copy_from_slice(&sum.to_le_bytes());
    out
}

/// Refuses `page`, read from `page_no`, if its checksum does not match.
pub(crate) fn verify(page_no: u64, page: &[u8]) -> Result<()> {
    let stored = u32::from_le_bytes(page[..CHECKSUM_LEN].try_into().unwrap());
    if stored != checksum(page_no, page) {
        return Err(Error::Corrupt(format!(
            "page {page_no} has a wrong checksum"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_of(byte: u8) -> Vec<u8> {
        vec![byte; PAGE_SIZE]
    }

    #[test]
    fn a_sealed_page_verifies_where_it_was_sealed_for() {
        let page = sealed(7, &page_of(3));

        verify(7, &page).unwrap();
        assert_eq!(&page[CHECKSUM_LEN..], &page_of(3)[CHECKSUM_LEN..]);
    }

    #[test]
    fn the_same_bytes_at_another_page_number_fail() {
        let page = sealed(7, &page_of(3));

        assert!(matches!(verify(8, &page), Err(Error::Corrupt(_))));
    }

    #[test]
    fn every_single_bit_flip_fails() {
        let page = sealed(2, &page_of(0x5A));
        for bit in 0..PAGE_SIZE * 8 {
            let mut broken = page;
            broken[bit / 8] ^= 1 << (bit % 8);
            assert!(verify(2, &broken).is_err(), "bit {bit}");
        }
    }

    #[test]
    fn the_checksum_is_crc32c_of_the_rest_then_the_page_number() {
        // CRC-32C of the 9 bytes "123456789" is 0xE3069283 (RFC 3720).
        assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
        let page = page_of(1);
        let mut rest = page[CHECKSUM_LEN..].to_vec();
        rest.extend_from_slice(&5u64.to_le_bytes());

        let stored = u32::from_le_bytes(sealed(5, &page)[..4].try_into().unwrap());

        assert_eq!(stored, crc32c::crc32c(&rest));
    }
}
