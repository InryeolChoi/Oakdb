//! Fixed-size pages and the leaf page layout.
//!
//! The layout follows SQLite's b-tree pages (ADR 0008): a header, then a
//! cell pointer array (slots) that grows forward in key order, then free
//! space, then cell contents filled from the end of the page backwards.
//! Field details follow `docs/research/0005-btree-page-layout.md`. Every
//! integer is little-endian.
//!
//! ```text
//! offset  size  field
//! 0..4    u32   checksum         reserved for CRC-32C, 0 for now
//! 4..12   u64   page_lsn         reserved for the WAL, 0 for now
//! 12      u8    kind             1 = leaf
//! 13      u8    flags            must be 0
//! 14..16  u16   cell_count
//! 16..18  u16   free_start       end of the slot array
//! 18..20  u16   free_end         start of the cell content area
//! 20..22  u16   frag_bytes       bytes freed by replaced cells
//! 22..24  u16   reserved         0
//! 24..32  u64   right_sibling    0 = none
//! 32..40  u64   rightmost_child  internal pages only, 0 for leaves
//! 40..    [offset u16, len u16] × cell_count
//! ...     free space
//! ...4096 cells: key_len u16 | value_len u16 | key | value
//! ```

use crate::error::{Error, Result};

/// Size of every page in the data file.
pub(crate) const PAGE_SIZE: usize = 4096;

const HEADER_SIZE: usize = 40;
const SLOT_SIZE: usize = 4;
const CELL_HEADER_SIZE: usize = 4;

const KIND_LEAF: u8 = 1;

const KIND: usize = 12;
const FLAGS: usize = 13;
const CELL_COUNT: usize = 14;
const FREE_START: usize = 16;
const FREE_END: usize = 18;
const FRAG_BYTES: usize = 20;
const RESERVED: usize = 22;
const RIGHTMOST_CHILD: usize = 32;

/// A leaf page holding sorted key-value cells.
#[derive(Clone)]
pub(crate) struct LeafPage {
    buf: Box<[u8]>,
}

impl LeafPage {
    /// A leaf page with no cells.
    pub(crate) fn empty() -> Self {
        let mut page = LeafPage {
            buf: vec![0; PAGE_SIZE].into_boxed_slice(),
        };
        page.buf[KIND] = KIND_LEAF;
        page.write_u16(FREE_START, HEADER_SIZE);
        page.write_u16(FREE_END, PAGE_SIZE);
        page
    }

    /// Takes a page read from disk, refusing it if its header or slots are
    /// inconsistent, so a broken page can never send a read out of bounds.
    pub(crate) fn from_bytes(buf: Box<[u8]>) -> Result<Self> {
        if buf.len() != PAGE_SIZE {
            return Err(corrupt("page has the wrong size"));
        }
        let page = LeafPage { buf };
        if page.buf[KIND] != KIND_LEAF {
            return Err(corrupt("page is not a leaf page"));
        }
        if page.buf[FLAGS] != 0 {
            return Err(corrupt("page has unknown flags"));
        }
        if page.read_u16(RESERVED) != 0 || page.buf[RIGHTMOST_CHILD..HEADER_SIZE] != [0; 8] {
            return Err(corrupt("leaf page uses fields only internal pages have"));
        }
        let free_start = page.read_u16(FREE_START);
        let free_end = page.read_u16(FREE_END);
        if free_start != HEADER_SIZE + SLOT_SIZE * page.cell_count()
            || free_start > free_end
            || free_end > PAGE_SIZE
        {
            return Err(corrupt("page header is inconsistent"));
        }
        let mut cell_bytes = 0;
        for i in 0..page.cell_count() {
            let (offset, len) = page.slot(i);
            cell_bytes += len;
            if offset < free_end || offset + len > PAGE_SIZE || len < CELL_HEADER_SIZE {
                return Err(corrupt("slot points outside the cell area"));
            }
            let key_len = page.read_u16(offset);
            let value_len = page.read_u16(offset + 2);
            if CELL_HEADER_SIZE + key_len + value_len != len {
                return Err(corrupt("cell length does not match its slot"));
            }
        }
        // Every byte of the cell area is either a live cell or a fragment.
        // This also keeps later fragment counts from overflowing.
        if cell_bytes + page.read_u16(FRAG_BYTES) != PAGE_SIZE - free_end {
            return Err(corrupt("cell area does not add up"));
        }
        for i in 1..page.cell_count() {
            if page.key_at(i - 1) >= page.key_at(i) {
                return Err(corrupt("keys are not in order"));
            }
        }
        Ok(page)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The value stored under `key`, if any.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let i = self.search(key).ok()?;
        Some(self.value_at(i))
    }

    /// Stores `value` under `key`, replacing any value already there.
    ///
    /// On [`Error::PageFull`] the page is left unchanged.
    pub(crate) fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let cell_len = CELL_HEADER_SIZE + key.len() + value.len();
        let found = self.search(key);
        // Replacing a cell gives back its slot. Its old bytes are not reused
        // yet: that needs compaction, which comes with deletes.
        let reused_slot = if found.is_ok() { SLOT_SIZE } else { 0 };
        if SLOT_SIZE + cell_len > self.free_space() + reused_slot {
            return Err(Error::PageFull);
        }

        let index = match found {
            Ok(i) => {
                self.remove_slot(i);
                i
            }
            Err(i) => i,
        };

        let offset = self.read_u16(FREE_END) - cell_len;
        self.write_u16(offset, key.len());
        self.write_u16(offset + 2, value.len());
        let key_start = offset + CELL_HEADER_SIZE;
        self.buf[key_start..key_start + key.len()].copy_from_slice(key);
        self.buf[key_start + key.len()..offset + cell_len].copy_from_slice(value);
        self.write_u16(FREE_END, offset);

        let free_start = self.read_u16(FREE_START);
        let slot = slot_pos(index);
        self.buf.copy_within(slot..free_start, slot + SLOT_SIZE);
        self.write_u16(slot, offset);
        self.write_u16(slot + 2, cell_len);
        self.write_u16(FREE_START, free_start + SLOT_SIZE);
        self.write_u16(CELL_COUNT, self.cell_count() + 1);
        Ok(())
    }

    fn remove_slot(&mut self, index: usize) {
        let (_, len) = self.slot(index);
        let free_start = self.read_u16(FREE_START);
        let slot = slot_pos(index);
        self.buf.copy_within(slot + SLOT_SIZE..free_start, slot);
        self.buf[free_start - SLOT_SIZE..free_start].fill(0);
        self.write_u16(FREE_START, free_start - SLOT_SIZE);
        self.write_u16(CELL_COUNT, self.cell_count() - 1);
        self.write_u16(FRAG_BYTES, self.read_u16(FRAG_BYTES) + len);
    }

    /// Binary search over the slots: `Ok(i)` if slot `i` holds `key`,
    /// otherwise `Err(i)` where `key` would be inserted.
    fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        let (mut low, mut high) = (0, self.cell_count());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.key_at(mid).cmp(key) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }

    /// Bytes between the slot array and the cell content area.
    fn free_space(&self) -> usize {
        self.read_u16(FREE_END) - self.read_u16(FREE_START)
    }

    fn cell_count(&self) -> usize {
        self.read_u16(CELL_COUNT)
    }

    fn slot(&self, index: usize) -> (usize, usize) {
        let slot = slot_pos(index);
        (self.read_u16(slot), self.read_u16(slot + 2))
    }

    fn key_at(&self, index: usize) -> &[u8] {
        let (offset, _) = self.slot(index);
        let start = offset + CELL_HEADER_SIZE;
        &self.buf[start..start + self.read_u16(offset)]
    }

    fn value_at(&self, index: usize) -> &[u8] {
        let (offset, len) = self.slot(index);
        let start = offset + CELL_HEADER_SIZE + self.read_u16(offset);
        &self.buf[start..offset + len]
    }

    fn read_u16(&self, at: usize) -> usize {
        usize::from(u16::from_le_bytes([self.buf[at], self.buf[at + 1]]))
    }

    fn write_u16(&mut self, at: usize, value: usize) {
        let value = u16::try_from(value).expect("page offsets fit in u16");
        self.buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
}

/// Where slot `index` starts in the page.
fn slot_pos(index: usize) -> usize {
    HEADER_SIZE + index * SLOT_SIZE
}

fn corrupt(reason: &str) -> Error {
    Error::Corrupt(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_leaf_has_all_space_free() {
        let page = LeafPage::empty();

        assert_eq!(page.buf[KIND], 1);
        assert_eq!(page.read_u16(CELL_COUNT), 0);
        assert_eq!(page.read_u16(FREE_START), 40);
        assert_eq!(page.read_u16(FREE_END), 4096);
    }

    #[test]
    fn slots_grow_forward_in_key_order_and_cells_fill_from_the_end() {
        let mut page = LeafPage::empty();
        page.put(b"b", b"22").unwrap();
        page.put(b"a", b"1").unwrap();

        // "b" went in first, so its cell sits at the very end of the page.
        assert_eq!(&page.buf[4096 - 7..], b"\x01\x00\x02\x00b22");
        assert_eq!(&page.buf[4096 - 13..4096 - 7], b"\x01\x00\x01\x00a1");
        // Slots are in key order: "a" first, then "b".
        assert_eq!(&page.buf[40..48], &[0xF3, 0x0F, 6, 0, 0xF9, 0x0F, 7, 0]);
        assert_eq!(page.read_u16(CELL_COUNT), 2);
        assert_eq!(page.read_u16(FREE_START), 48);
        assert_eq!(page.read_u16(FREE_END), 4096 - 13);
    }

    #[test]
    fn replacing_a_value_counts_the_old_cell_as_fragmented() {
        let mut page = LeafPage::empty();
        page.put(b"a", b"old").unwrap();
        page.put(b"a", b"newer").unwrap();

        assert_eq!(page.get(b"a"), Some(&b"newer"[..]));
        assert_eq!(page.read_u16(CELL_COUNT), 1);
        assert_eq!(page.read_u16(FRAG_BYTES), 8);
    }

    #[test]
    fn a_full_page_is_left_unchanged() {
        let mut page = LeafPage::empty();
        page.put(b"a", &[0; 4000]).unwrap();
        let before = page.clone();

        assert!(matches!(page.put(b"b", &[0; 100]), Err(Error::PageFull)));
        assert_eq!(page.as_bytes(), before.as_bytes());
    }

    #[test]
    fn a_value_bigger_than_a_page_does_not_fit() {
        let mut page = LeafPage::empty();

        assert!(matches!(page.put(b"a", &[0; 70_000]), Err(Error::PageFull)));
    }

    #[test]
    fn a_page_round_trips_through_its_bytes() {
        let mut page = LeafPage::empty();
        page.put(b"oak", b"tree").unwrap();
        page.put(b"acorn", b"seed").unwrap();

        let read = LeafPage::from_bytes(page.as_bytes().into()).unwrap();

        assert_eq!(read.get(b"oak"), Some(&b"tree"[..]));
        assert_eq!(read.get(b"acorn"), Some(&b"seed"[..]));
        assert_eq!(read.get(b"elm"), None);
    }

    fn assert_refused(mut bytes: Vec<u8>, break_it: impl FnOnce(&mut [u8])) {
        break_it(&mut bytes);
        let result = LeafPage::from_bytes(bytes.into());
        assert!(matches!(result, Err(Error::Corrupt(_))));
    }

    #[test]
    fn inconsistent_pages_are_refused() {
        let mut page = LeafPage::empty();
        page.put(b"a", b"1").unwrap();
        page.put(b"b", b"2").unwrap();
        let bytes = page.as_bytes().to_vec();

        assert_refused(bytes.clone(), |b| b[KIND] = 2);
        assert_refused(bytes.clone(), |b| b[FLAGS] = 1);
        assert_refused(bytes.clone(), |b| b[CELL_COUNT] = 3);
        assert_refused(bytes.clone(), |b| b[FREE_END..FREE_END + 2].fill(0xFF));
        // First slot's offset points into the free space.
        assert_refused(bytes.clone(), |b| {
            b[40..42].copy_from_slice(&100u16.to_le_bytes())
        });
        // "b" went in last, so its cell starts 6 + 6 bytes before the end.
        // Make it claim a longer key than its slot allows.
        assert_refused(bytes.clone(), |b| b[4096 - 12] = 9);
        // Swap the two slots so the keys are out of order.
        assert_refused(bytes.clone(), |b| {
            let first: [u8; 4] = b[40..44].try_into().unwrap();
            b.copy_within(44..48, 40);
            b[44..48].copy_from_slice(&first);
        });
        assert_refused(bytes[..100].to_vec(), |_| {});
        // Cell area bytes no longer add up to live cells + fragments.
        assert_refused(bytes.clone(), |b| b[FRAG_BYTES..FRAG_BYTES + 2].fill(0xFF));
        assert_refused(bytes.clone(), |b| b[22] = 1);
        assert_refused(bytes.clone(), |b| b[39] = 1);
    }

    #[test]
    fn a_page_with_fragments_round_trips() {
        let mut page = LeafPage::empty();
        page.put(b"a", b"old").unwrap();
        page.put(b"a", b"new").unwrap();

        let mut read = LeafPage::from_bytes(page.as_bytes().into()).unwrap();
        read.put(b"a", b"newest").unwrap();

        assert_eq!(read.get(b"a"), Some(&b"newest"[..]));
    }
}
