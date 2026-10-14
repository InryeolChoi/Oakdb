//! Fixed-size pages and the layout of leaf and internal pages.
//!
//! The layout follows SQLite's b-tree pages (ADR 0008): a header, then a
//! cell pointer array (slots) that grows forward in key order, then free
//! space, then cell contents filled from the end of the page backwards.
//! Field details follow `docs/research/0005-btree-page-layout.md`. Every
//! integer is little-endian.
//!
//! Leaf cells hold a key and its value. Internal cells hold a separator key
//! and, as their value, the page number (u64) of the child holding the keys
//! below it and at or above the previous separator. Keys at or above the
//! last separator are in `rightmost_child`.
//!
//! ```text
//! offset  size  field
//! 0..4    u32   checksum         reserved for CRC-32C, 0 for now
//! 4..12   u64   page_lsn         reserved for the WAL, 0 for now
//! 12      u8    kind             1 = leaf, 2 = internal
//! 13      u8    flags            must be 0
//! 14..16  u16   cell_count
//! 16..18  u16   free_start       end of the slot array
//! 18..20  u16   free_end         start of the cell content area
//! 20..22  u16   frag_bytes       bytes freed by replaced or deleted cells
//! 22..24  u16   reserved         0
//! 24..32  u64   right_sibling    leaves only, 0 = none
//! 32..40  u64   rightmost_child  internal pages only
//! 40..    [offset u16, len u16] × cell_count
//! ...     free space
//! ...4096 cells: key_len u16 | value_len u16 | key | value
//! ```

use crate::error::{corrupt, Error, Result, MAX_ENTRY_LEN, MAX_KEY_LEN};

/// Size of every page in the data file.
pub(crate) const PAGE_SIZE: usize = 4096;

const HEADER_SIZE: usize = 40;
const SLOT_SIZE: usize = 4;
const CELL_HEADER_SIZE: usize = 4;

const KIND_LEAF: u8 = 1;
const KIND_INTERNAL: u8 = 2;

const KIND: usize = 12;
const FLAGS: usize = 13;
const CELL_COUNT: usize = 14;
const FREE_START: usize = 16;
const FREE_END: usize = 18;
const FRAG_BYTES: usize = 20;
const RESERVED: usize = 22;
const RIGHT_SIBLING: usize = 24;
const RIGHTMOST_CHILD: usize = 32;

const CHILD_LEN: usize = 8;

/// Space a cell takes in a page, its slot included.
const fn footprint(key_len: usize, value_len: usize) -> usize {
    SLOT_SIZE + CELL_HEADER_SIZE + key_len + value_len
}

// A full leaf plus one more entry always splits into two leaves that fit.
const _: () = assert!(4 * footprint(MAX_ENTRY_LEN, 0) <= PAGE_SIZE - HEADER_SIZE);
// Every separator fits in an internal page.
const _: () = assert!(2 * footprint(MAX_KEY_LEN, CHILD_LEN) <= PAGE_SIZE - HEADER_SIZE);

/// A leaf or internal page holding sorted cells.
#[derive(Clone)]
pub(crate) struct Page {
    buf: Box<[u8]>,
}

impl Page {
    /// A leaf page with no cells.
    pub(crate) fn empty_leaf() -> Self {
        Page::empty(KIND_LEAF)
    }

    /// An internal page whose keys below `separator` are in `left` and the
    /// rest in `right`.
    pub(crate) fn new_internal(left: u64, separator: &[u8], right: u64) -> Self {
        let mut page = Page::empty(KIND_INTERNAL);
        page.write_u64(RIGHTMOST_CHILD, right);
        page.put(separator, &left.to_le_bytes())
            .expect("one separator fits in an empty page");
        page
    }

    fn empty(kind: u8) -> Self {
        let mut page = Page {
            buf: vec![0; PAGE_SIZE].into_boxed_slice(),
        };
        page.buf[KIND] = kind;
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
        let page = Page { buf };
        if page.buf[KIND] != KIND_LEAF && page.buf[KIND] != KIND_INTERNAL {
            return Err(corrupt("page is neither a leaf nor an internal page"));
        }
        if page.buf[FLAGS] != 0 || page.read_u16(RESERVED) != 0 {
            return Err(corrupt("page has unknown flags"));
        }
        if page.is_leaf() && page.read_u64(RIGHTMOST_CHILD) != 0 {
            return Err(corrupt("leaf page has a child"));
        }
        if !page.is_leaf() && page.read_u64(RIGHT_SIBLING) != 0 {
            return Err(corrupt("internal page has a sibling"));
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
            if !page.is_leaf() && value_len != CHILD_LEN {
                return Err(corrupt("internal cell does not hold a page number"));
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

    pub(crate) fn is_leaf(&self) -> bool {
        self.buf[KIND] == KIND_LEAF
    }

    pub(crate) fn cell_count(&self) -> usize {
        self.read_u16(CELL_COUNT)
    }

    pub(crate) fn key_at(&self, index: usize) -> &[u8] {
        let (offset, _) = self.slot(index);
        let start = offset + CELL_HEADER_SIZE;
        &self.buf[start..start + self.read_u16(offset)]
    }

    /// The next leaf to the right, if any. Leaves only.
    pub(crate) fn right_sibling(&self) -> Option<u64> {
        Some(self.read_u64(RIGHT_SIBLING)).filter(|&page_no| page_no != 0)
    }

    pub(crate) fn set_right_sibling(&mut self, page_no: Option<u64>) {
        self.write_u64(RIGHT_SIBLING, page_no.unwrap_or(0));
    }

    /// Which child of this internal page `key` belongs in: an index into
    /// its cells, or [`Page::cell_count`] for the rightmost child.
    pub(crate) fn child_index(&self, key: &[u8]) -> usize {
        match self.search(key) {
            // A key equal to a separator sorts at or above it.
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    /// The page number of child `index`; see [`Page::child_index`].
    pub(crate) fn child_at(&self, index: usize) -> u64 {
        if index == self.cell_count() {
            return self.read_u64(RIGHTMOST_CHILD);
        }
        let value = self.value_at(index);
        u64::from_le_bytes(value.try_into().expect("checked when the page was read"))
    }

    /// Points child `index` at another page, keeping its key range.
    pub(crate) fn set_child_at(&mut self, index: usize, page_no: u64) {
        if index == self.cell_count() {
            self.write_u64(RIGHTMOST_CHILD, page_no);
        } else {
            let (offset, len) = self.slot(index);
            self.write_u64(offset + len - CHILD_LEN, page_no);
        }
    }

    /// Adds a separator to this internal page: keys below `separator`, down
    /// to the previous separator, are in `child`.
    pub(crate) fn insert_child(&mut self, separator: &[u8], child: u64) -> Result<()> {
        self.put(separator, &child.to_le_bytes())
    }

    /// Splits this leaf, with `key` and `value` added, into a left and a
    /// right leaf with about the same number of bytes each. Returns them
    /// with the first key of the right leaf, which separates the two.
    ///
    /// Siblings are left for the caller to set.
    pub(crate) fn split_leaf(&self, key: &[u8], value: &[u8]) -> (Page, Vec<u8>, Page) {
        debug_assert!(key.len() + value.len() <= MAX_ENTRY_LEN);
        let mut entries: Vec<(&[u8], &[u8])> = (0..self.cell_count())
            .map(|i| (self.key_at(i), self.value_at(i)))
            .collect();
        match self.search(key) {
            Ok(i) => entries[i] = (key, value),
            Err(i) => entries.insert(i, (key, value)),
        }
        let total: usize = entries
            .iter()
            .map(|(k, v)| footprint(k.len(), v.len()))
            .sum();
        // The first `mid` entries go left. Pick the split that leaves the
        // fuller leaf as empty as possible, with at least one entry on
        // each side.
        let (mut left, mut mid, mut fuller) = (0, 0, usize::MAX);
        for (i, (k, v)) in entries[..entries.len() - 1].iter().enumerate() {
            left += footprint(k.len(), v.len());
            if left.max(total - left) < fuller {
                (mid, fuller) = (i + 1, left.max(total - left));
            }
        }
        let build = |entries: &[(&[u8], &[u8])]| {
            let mut page = Page::empty_leaf();
            for (k, v) in entries {
                page.put(k, v).expect("half of a split fits in a page");
            }
            page
        };
        let separator = entries[mid].0.to_vec();
        (build(&entries[..mid]), separator, build(&entries[mid..]))
    }

    /// The value stored under `key`, if any.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let i = self.search(key).ok()?;
        Some(self.value_at(i))
    }

    /// Stores `value` under `key`, replacing any value already there.
    ///
    /// Bytes left behind by replaced cells are reclaimed by compacting the
    /// page when the new cell does not fit otherwise. On [`Error::PageFull`]
    /// the page is left unchanged.
    pub(crate) fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let cell_len = CELL_HEADER_SIZE + key.len() + value.len();
        let found = self.search(key);
        // Replacing a cell gives back its slot and its bytes.
        let replaced = match found {
            Ok(i) => SLOT_SIZE + self.slot(i).1,
            Err(_) => 0,
        };
        let reclaimable = self.free_space() + self.read_u16(FRAG_BYTES) + replaced;
        if SLOT_SIZE + cell_len > reclaimable {
            return Err(Error::PageFull);
        }

        let index = match found {
            Ok(i) => {
                self.remove_slot(i);
                i
            }
            Err(i) => i,
        };
        if SLOT_SIZE + cell_len > self.free_space() {
            self.compact();
        }

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

    /// Removes `key` and its value. Returns whether it was there.
    ///
    /// The cell's bytes count as fragmented until a later put compacts the
    /// page.
    pub(crate) fn delete(&mut self, key: &[u8]) -> bool {
        match self.search(key) {
            Ok(i) => {
                self.remove_slot(i);
                true
            }
            Err(_) => false,
        }
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

    /// Moves every live cell to the end of the page, back to back, so the
    /// fragments between them join the free space. Slots keep their order.
    fn compact(&mut self) {
        let old = self.buf.clone();
        let mut end = PAGE_SIZE;
        for i in 0..self.cell_count() {
            let (offset, len) = self.slot(i);
            end -= len;
            self.buf[end..end + len].copy_from_slice(&old[offset..offset + len]);
            self.write_u16(slot_pos(i), end);
        }
        let free_start = self.read_u16(FREE_START);
        self.buf[free_start..end].fill(0);
        self.write_u16(FREE_END, end);
        self.write_u16(FRAG_BYTES, 0);
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

    fn slot(&self, index: usize) -> (usize, usize) {
        let slot = slot_pos(index);
        (self.read_u16(slot), self.read_u16(slot + 2))
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

    fn read_u64(&self, at: usize) -> u64 {
        u64::from_le_bytes(self.buf[at..at + 8].try_into().unwrap())
    }

    fn write_u64(&mut self, at: usize, value: u64) {
        self.buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
}

/// Where slot `index` starts in the page.
fn slot_pos(index: usize) -> usize {
    HEADER_SIZE + index * SLOT_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_leaf_has_all_space_free() {
        let page = Page::empty_leaf();

        assert_eq!(page.buf[KIND], 1);
        assert_eq!(page.read_u16(CELL_COUNT), 0);
        assert_eq!(page.read_u16(FREE_START), 40);
        assert_eq!(page.read_u16(FREE_END), 4096);
    }

    #[test]
    fn slots_grow_forward_in_key_order_and_cells_fill_from_the_end() {
        let mut page = Page::empty_leaf();
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
        let mut page = Page::empty_leaf();
        page.put(b"a", b"old").unwrap();
        page.put(b"a", b"newer").unwrap();

        assert_eq!(page.get(b"a"), Some(&b"newer"[..]));
        assert_eq!(page.read_u16(CELL_COUNT), 1);
        assert_eq!(page.read_u16(FRAG_BYTES), 8);
    }

    #[test]
    fn compacting_reclaims_fragments_and_keeps_every_value() {
        let mut page = Page::empty_leaf();
        for key in 0u8..10 {
            page.put(&[key], &[key; 300]).unwrap();
        }
        for key in (0u8..10).step_by(2) {
            page.put(&[key], &[key]).unwrap();
        }
        let fragments = page.read_u16(FRAG_BYTES);
        let free = page.free_space();

        page.compact();

        assert_eq!(page.read_u16(FRAG_BYTES), 0);
        assert_eq!(page.free_space(), free + fragments);
        for key in 0u8..10 {
            let len = if key % 2 == 0 { 1 } else { 300 };
            assert_eq!(page.get(&[key]), Some(&vec![key; len][..]));
        }
        Page::from_bytes(page.as_bytes().into()).unwrap();
    }

    #[test]
    fn a_replaced_cell_makes_room_for_its_bigger_replacement() {
        let mut page = Page::empty_leaf();
        page.put(b"a", &[1; 3000]).unwrap();
        page.put(b"b", &[2; 900]).unwrap();

        page.put(b"a", &[3; 3100]).unwrap();

        assert_eq!(page.get(b"a"), Some(&[3; 3100][..]));
        assert_eq!(page.get(b"b"), Some(&[2; 900][..]));
    }

    #[test]
    fn random_puts_match_a_btreemap() {
        let mut seed: u64 = 0x0a4b_d0b1;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut page = Page::empty_leaf();
        let mut model = std::collections::BTreeMap::new();
        for step in 0..20_000 {
            let key = [next(40) as u8];
            let value = vec![step as u8; next(400) as usize];
            let before = page.clone();
            match page.put(&key, &value) {
                Ok(()) => {
                    model.insert(key, value);
                }
                Err(Error::PageFull) => assert_eq!(page.as_bytes(), before.as_bytes()),
                Err(e) => panic!("{e}"),
            }
            if step % 500 == 0 {
                page = Page::from_bytes(page.as_bytes().into()).unwrap();
            }
        }
        for key in 0..40u8 {
            assert_eq!(page.get(&[key]), model.get(&[key]).map(Vec::as_slice));
        }
    }

    #[test]
    fn a_deleted_cell_is_gone_and_its_bytes_count_as_fragmented() {
        let mut page = Page::empty_leaf();
        page.put(b"a", b"1").unwrap();
        page.put(b"b", b"22").unwrap();

        assert!(page.delete(b"a"));
        assert!(!page.delete(b"a"));

        assert_eq!(page.get(b"a"), None);
        assert_eq!(page.get(b"b"), Some(&b"22"[..]));
        assert_eq!(page.read_u16(CELL_COUNT), 1);
        assert_eq!(page.read_u16(FRAG_BYTES), 6);
        Page::from_bytes(page.as_bytes().into()).unwrap();
    }

    #[test]
    fn deleting_every_cell_frees_the_whole_page() {
        let mut page = Page::empty_leaf();
        for key in 0u8..10 {
            page.put(&[key], &[key; 350]).unwrap();
        }
        for key in 0u8..10 {
            assert!(page.delete(&[key]));
        }

        page.put(b"a", &[1; 4047]).unwrap();

        assert_eq!(page.get(b"a"), Some(&[1; 4047][..]));
    }

    #[test]
    fn a_full_page_is_left_unchanged() {
        let mut page = Page::empty_leaf();
        page.put(b"a", &[0; 4000]).unwrap();
        let before = page.clone();

        assert!(matches!(page.put(b"b", &[0; 100]), Err(Error::PageFull)));
        assert_eq!(page.as_bytes(), before.as_bytes());
    }

    #[test]
    fn a_value_bigger_than_a_page_does_not_fit() {
        let mut page = Page::empty_leaf();

        assert!(matches!(page.put(b"a", &[0; 70_000]), Err(Error::PageFull)));
    }

    #[test]
    fn a_page_round_trips_through_its_bytes() {
        let mut page = Page::empty_leaf();
        page.put(b"oak", b"tree").unwrap();
        page.put(b"acorn", b"seed").unwrap();

        let read = Page::from_bytes(page.as_bytes().into()).unwrap();

        assert_eq!(read.get(b"oak"), Some(&b"tree"[..]));
        assert_eq!(read.get(b"acorn"), Some(&b"seed"[..]));
        assert_eq!(read.get(b"elm"), None);
    }

    fn assert_refused(mut bytes: Vec<u8>, break_it: impl FnOnce(&mut [u8])) {
        break_it(&mut bytes);
        let result = Page::from_bytes(bytes.into());
        assert!(matches!(result, Err(Error::Corrupt(_))));
    }

    #[test]
    fn inconsistent_pages_are_refused() {
        let mut page = Page::empty_leaf();
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
        let mut page = Page::empty_leaf();
        page.put(b"a", b"old").unwrap();
        page.put(b"a", b"new").unwrap();

        let mut read = Page::from_bytes(page.as_bytes().into()).unwrap();
        read.put(b"a", b"newest").unwrap();

        assert_eq!(read.get(b"a"), Some(&b"newest"[..]));
    }
}
