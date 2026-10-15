//! The B+Tree: finding the leaf for a key, splitting pages that fill up,
//! and taking out leaves that empty out.
//!
//! The whole tree is kept in memory. Pages are shared with [`Arc`], so a
//! commit can clone the tree cheaply, change its copy, and copy only the
//! pages it touches.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::error::{corrupt, Result};
use crate::meta::Meta;
use crate::page::Page;

/// Page number of the first tree page. Pages 0 and 1 hold the meta slots.
pub(crate) const FIRST_PAGE: u64 = 2;

#[derive(Clone)]
pub(crate) struct Tree {
    root: u64,
    /// Every page of the tree, page number [`FIRST_PAGE`] first.
    pages: Vec<Arc<Page>>,
    /// Pages changed since the last [`Tree::take_dirty`].
    dirty: BTreeSet<u64>,
}

impl Tree {
    /// A tree of one empty leaf.
    pub(crate) fn new() -> Self {
        let mut tree = Tree {
            root: FIRST_PAGE,
            pages: Vec::new(),
            dirty: BTreeSet::new(),
        };
        tree.push(Page::empty_leaf());
        tree
    }

    /// The tree `meta` describes, reading each page with `read`.
    ///
    /// Refuses a tree whose pages do not fit together, so a damaged file
    /// can never send a lookup in circles or to the wrong leaf.
    pub(crate) fn load(meta: Meta, mut read: impl FnMut(u64) -> Result<Page>) -> Result<Self> {
        let pages = (FIRST_PAGE..meta.page_count)
            .map(|page_no| read(page_no).map(Arc::new))
            .collect::<Result<_>>()?;
        let tree = Tree {
            root: meta.root_page,
            pages,
            dirty: BTreeSet::new(),
        };
        tree.check()?;
        Ok(tree)
    }

    pub(crate) fn meta(&self) -> Meta {
        Meta {
            root_page: self.root,
            page_count: FIRST_PAGE + self.pages.len() as u64,
        }
    }

    pub(crate) fn page(&self, page_no: u64) -> &Page {
        &self.pages[(page_no - FIRST_PAGE) as usize]
    }

    /// The pages changed since the last call, to be written out.
    pub(crate) fn take_dirty(&mut self) -> BTreeSet<u64> {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let (_, leaf) = self.descend(key);
        self.page(leaf).get(key)
    }

    /// Stores `value` under `key`, splitting its leaf if it is full, and
    /// its parents too if they fill up.
    pub(crate) fn put(&mut self, key: &[u8], value: &[u8]) {
        let (path, leaf) = self.descend(key);
        if self.page_mut(leaf).put(key, value).is_err() {
            self.split(&path, leaf, key, value);
        }
    }

    /// Removes `key`. Its leaf is written at commit even if `key` was not
    /// there. A leaf left empty leaves the tree, unless it is the root.
    pub(crate) fn delete(&mut self, key: &[u8]) {
        let (path, leaf) = self.descend(key);
        self.page_mut(leaf).delete(key);
        if self.page(leaf).cell_count() == 0 && !path.is_empty() {
            self.remove_leaf(&path, leaf);
        }
    }

    /// The leaf `key` belongs in, and the way down to it: each internal
    /// page passed and which of its children was taken.
    fn descend(&self, key: &[u8]) -> (Vec<(u64, usize)>, u64) {
        let mut path = Vec::new();
        let mut page_no = self.root;
        loop {
            let page = self.page(page_no);
            if page.is_leaf() {
                return (path, page_no);
            }
            let index = page.child_index(key);
            path.push((page_no, index));
            page_no = page.child_at(index);
        }
    }

    /// Splits `leaf` to make room for `key` and `value`. The left half
    /// keeps the leaf's page number, so the leaf before it still points at
    /// it; the right half goes on a new page.
    ///
    /// Each parent on `path` then takes the new right page. A parent with
    /// no room splits the same way and hands its own right half up; a root
    /// with no room gets a new root above it, and the tree grows a level.
    fn split(&mut self, path: &[(u64, usize)], leaf: u64, key: &[u8], value: &[u8]) {
        let (mut left, mut separator, mut right) = self.page(leaf).split_leaf(key, value);
        right.set_right_sibling(self.page(leaf).right_sibling());
        let mut right_no = self.push(right);
        left.set_right_sibling(Some(right_no));
        *self.page_mut(leaf) = left;

        let mut left_no = leaf;
        for &(parent, index) in path.iter().rev() {
            let page = self.page_mut(parent);
            if page.insert_split_child(index, &separator, right_no).is_ok() {
                return;
            }
            let (left, up, right) = page.split_internal(index, &separator, right_no);
            *page = left;
            right_no = self.push(right);
            separator = up;
            left_no = parent;
        }
        self.root = self.push(Page::new_internal(left_no, &separator, right_no));
    }

    /// Takes the empty `leaf` out of the tree. The leaf before it points
    /// past it, and its parent drops it. Parents left with no child at all
    /// go with it; a parent left with one child stays, as a page with no
    /// separators. A root left with one child hands over to it, and the
    /// tree loses a level.
    ///
    /// The pages that leave the tree stay in the file, unused.
    fn remove_leaf(&mut self, path: &[(u64, usize)], leaf: u64) {
        if let Some(before) = self.leaf_before(path) {
            let next = self.page(leaf).right_sibling();
            self.page_mut(before).set_right_sibling(next);
        }
        for &(parent, index) in path.iter().rev() {
            // A parent with no separators had `leaf`'s branch as its only
            // child, so it is empty now too.
            if self.page(parent).cell_count() > 0 {
                self.page_mut(parent).remove_child(index);
                break;
            }
        }
        while !self.page(self.root).is_leaf() && self.page(self.root).cell_count() == 0 {
            self.root = self.page(self.root).child_at(0);
        }
    }

    /// The leaf just before the one at the end of `path`, if any: down the
    /// child before the deepest turn that was not leftmost, then rightmost
    /// all the way.
    fn leaf_before(&self, path: &[(u64, usize)]) -> Option<u64> {
        let &(parent, index) = path.iter().rev().find(|&&(_, index)| index > 0)?;
        let mut page_no = self.page(parent).child_at(index - 1);
        while !self.page(page_no).is_leaf() {
            let page = self.page(page_no);
            page_no = page.child_at(page.cell_count());
        }
        Some(page_no)
    }

    /// Adds `page` at the end of the file and returns its page number.
    fn push(&mut self, page: Page) -> u64 {
        let page_no = FIRST_PAGE + self.pages.len() as u64;
        self.pages.push(Arc::new(page));
        self.dirty.insert(page_no);
        page_no
    }

    /// The page to change, copied first if a committed tree shares it.
    fn page_mut(&mut self, page_no: u64) -> &mut Page {
        self.dirty.insert(page_no);
        Arc::make_mut(&mut self.pages[(page_no - FIRST_PAGE) as usize])
    }

    /// Checks that, from the root down, every child is a page of the tree
    /// reached only once, keys stay within the range their parent gives
    /// them, every leaf is at the same depth, and the leaves point to each
    /// other from left to right.
    fn check(&self) -> Result<()> {
        let mut walk = Walk {
            seen: vec![false; self.pages.len()],
            leaves: Vec::new(),
            leaf_depth: None,
        };
        self.check_page(self.root, None, None, 0, &mut walk)?;
        let mut leaves = walk.leaves.iter().peekable();
        while let Some(&leaf) = leaves.next() {
            if self.page(leaf).right_sibling() != leaves.peek().copied().copied() {
                return Err(corrupt("leaves do not point to the next leaf"));
            }
        }
        Ok(())
    }

    fn check_page(
        &self,
        page_no: u64,
        low: Option<&[u8]>,
        high: Option<&[u8]>,
        depth: usize,
        walk: &mut Walk,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(corrupt("tree is too deep"));
        }
        let seen = page_no
            .checked_sub(FIRST_PAGE)
            .and_then(|i| walk.seen.get_mut(i as usize))
            .ok_or_else(|| corrupt("child page is outside the file"))?;
        if std::mem::replace(seen, true) {
            return Err(corrupt("page is reached twice"));
        }
        let page = self.page(page_no);
        let count = page.cell_count();
        if count > 0 {
            let below_low = low.is_some_and(|low| page.key_at(0) < low);
            let at_or_above_high = high.is_some_and(|high| page.key_at(count - 1) >= high);
            if below_low || at_or_above_high {
                return Err(corrupt("keys are outside the range of their parent"));
            }
        }
        if page.is_leaf() {
            if *walk.leaf_depth.get_or_insert(depth) != depth {
                return Err(corrupt("leaves are at different depths"));
            }
            walk.leaves.push(page_no);
            return Ok(());
        }
        for index in 0..=count {
            let child_low = if index == 0 {
                low
            } else {
                Some(page.key_at(index - 1))
            };
            let child_high = if index == count {
                high
            } else {
                Some(page.key_at(index))
            };
            self.check_page(page.child_at(index), child_low, child_high, depth + 1, walk)?;
        }
        Ok(())
    }
}

/// Deeper than any tree Oakdb writes. The tree only grows a level when its
/// root splits, which takes a full root over pages that split before it,
/// so growing one level more takes at least twice the pages it took to get
/// this far. Pages are never reused, so a tree this deep would need a file
/// of more than 2^64 pages. Checking stops here, so a corrupt file of long page chains cannot
/// overflow the stack.
const MAX_DEPTH: usize = 64;

/// What [`Tree::check`] has seen so far.
struct Walk {
    /// Whether each page has been reached.
    seen: Vec<bool>,
    /// Leaves in key order.
    leaves: Vec<u64>,
    leaf_depth: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, MAX_ENTRY_LEN, MAX_KEY_LEN};

    /// 36 entries of a 4-byte key and a 100-byte value fill a leaf.
    fn entry(i: u32) -> ([u8; 4], Vec<u8>) {
        (i.to_be_bytes(), vec![i as u8; 100])
    }

    fn tree_with(keys: impl IntoIterator<Item = u32>) -> Tree {
        let mut tree = Tree::new();
        for i in keys {
            let (key, value) = entry(i);
            tree.put(&key, &value);
        }
        tree
    }

    /// Reads `tree` back through its bytes, as opening a file would.
    fn reload(tree: &Tree) -> Result<Tree> {
        Tree::load(tree.meta(), |page_no| {
            Page::from_bytes(tree.page(page_no).as_bytes().into())
        })
    }

    #[test]
    fn the_first_split_makes_a_root_over_two_leaves() {
        let tree = tree_with(0..37);

        assert_eq!(tree.meta().page_count, 5);
        assert_eq!(tree.root, 4);
        let root = tree.page(4);
        assert!(!root.is_leaf());
        assert_eq!(root.cell_count(), 1);
        assert_eq!(root.child_at(0), 2);
        assert_eq!(root.child_at(1), 3);
        let separator = root.key_at(0);
        assert_eq!(tree.page(3).key_at(0), separator);
        assert!(tree.page(2).key_at(tree.page(2).cell_count() - 1) < separator);
    }

    #[test]
    fn the_split_halves_have_about_the_same_size() {
        let tree = tree_with(0..37);

        let (left, right) = (tree.page(2).cell_count(), tree.page(3).cell_count());
        assert_eq!(left + right, 37);
        assert!(left.abs_diff(right) <= 1, "{left} and {right}");
    }

    #[test]
    fn the_left_leaf_points_to_the_right_one() {
        let tree = tree_with(0..37);

        assert_eq!(tree.page(2).right_sibling(), Some(3));
        assert_eq!(tree.page(3).right_sibling(), None);
    }

    #[test]
    fn a_leaf_split_in_the_middle_keeps_the_leaves_in_a_chain() {
        let tree = three_leaves();

        assert_eq!(tree.meta().page_count, 6);
        assert_eq!(tree.page(2).right_sibling(), Some(5));
        assert_eq!(tree.page(5).right_sibling(), Some(3));
        assert_eq!(tree.page(3).right_sibling(), None);
        let root = tree.page(tree.root);
        let children: Vec<u64> = (0..=root.cell_count()).map(|i| root.child_at(i)).collect();
        assert_eq!(children, [2, 5, 3]);
        reload(&tree).unwrap();
    }

    /// Leaves 2, 5 and 3 under the root, in key order. The first split
    /// leaves keys 0..18 on the left; 19 more keys that sort among them make
    /// it split too.
    fn three_leaves() -> Tree {
        let mut tree = tree_with(0..37);
        for (i, suffix) in (0..18).map(|i| (i, b"+")).chain([(0, b"-")]) {
            let (key, value) = entry(i);
            tree.put(&[&key[..], suffix].concat(), &value);
        }
        tree
    }

    #[test]
    fn a_split_tree_reads_back_the_same() {
        let tree = tree_with((0..500).rev());

        let read = reload(&tree).unwrap();

        for i in 0..500 {
            let (key, value) = entry(i);
            assert_eq!(read.get(&key), Some(&value[..]), "key {i}");
        }
    }

    #[test]
    fn a_commit_writes_only_the_pages_it_changed() {
        let mut tree = tree_with(0..37);
        tree.take_dirty();

        let (key, value) = entry(0);
        tree.put(&key, &value);

        assert_eq!(tree.take_dirty(), BTreeSet::from([2]));
    }

    /// How many levels the tree has, a lone leaf being one.
    fn height(tree: &Tree) -> usize {
        let mut height = 1;
        let mut page_no = tree.root;
        while !tree.page(page_no).is_leaf() {
            page_no = tree.page(page_no).child_at(0);
            height += 1;
        }
        height
    }

    /// How many internal pages the tree reaches from its root.
    fn internal_pages(tree: &Tree) -> usize {
        let mut count = 0;
        let mut pages = vec![tree.root];
        while let Some(page_no) = pages.pop() {
            let page = tree.page(page_no);
            if !page.is_leaf() {
                count += 1;
                pages.extend((0..=page.cell_count()).map(|i| page.child_at(i)));
            }
        }
        count
    }

    /// Entry `i` of the biggest size: four fill a leaf, and an internal
    /// page holds at most seven of their keys.
    fn big_entry(i: u32) -> (Vec<u8>, Vec<u8>) {
        let mut key = vec![0; MAX_KEY_LEN];
        key[..4].copy_from_slice(&i.to_be_bytes());
        (key, vec![i as u8; MAX_ENTRY_LEN - MAX_KEY_LEN])
    }

    fn big_tree(keys: impl IntoIterator<Item = u32>) -> Tree {
        let mut tree = Tree::new();
        for i in keys {
            let (key, value) = big_entry(i);
            tree.put(&key, &value);
        }
        tree
    }

    #[test]
    fn a_full_root_splits_and_the_tree_grows_a_level() {
        let mut tree = Tree::new();
        let mut n = 0;
        while height(&tree) < 3 {
            let (key, value) = big_entry(n);
            tree.put(&key, &value);
            n += 1;
        }

        // The new root has one separator over the two halves of the old one.
        let root = tree.page(tree.root);
        assert_eq!(root.cell_count(), 1);
        let separator = root.key_at(0).to_vec();
        let (left, right) = (tree.page(root.child_at(0)), tree.page(root.child_at(1)));
        assert!(!left.is_leaf() && !right.is_leaf());
        // The separator moved up: it is in neither half.
        assert!(left.key_at(left.cell_count() - 1) < &separator[..]);
        assert!(right.key_at(0) > &separator[..]);
        assert!(left.cell_count().abs_diff(right.cell_count()) <= 1);
        let read = reload(&tree).unwrap();
        for i in 0..n {
            let (key, value) = big_entry(i);
            assert_eq!(read.get(&key), Some(&value[..]), "key {i}");
        }
    }

    #[test]
    fn an_internal_page_in_the_middle_splits_and_its_parent_takes_the_new_half() {
        let mut tree = big_tree(0..200);
        let (height_before, internal_before) = (height(&tree), internal_pages(&tree));
        // Keys sorting between 0 and 1 all land under the leftmost pages.
        for i in 0..100u32 {
            let (mut key, value) = big_entry(0);
            key[4..8].copy_from_slice(&i.to_be_bytes());
            key[8] = 1;
            tree.put(&key, &value);
        }

        // More internal pages at the same height: one below the root split.
        assert_eq!(height(&tree), height_before);
        assert!(internal_pages(&tree) > internal_before);
        reload(&tree).unwrap();
    }

    #[test]
    fn a_thousand_of_the_biggest_entries_make_a_five_level_tree() {
        // About 300 leaves of up to 4 entries, under internal pages of up
        // to 8 children: 300 → about 60 → about 10 → 2 → the root.
        for order in [
            (0..1000).collect::<Vec<u32>>(),
            (0..1000).map(|i| i * 7919 % 1000).collect(),
        ] {
            let tree = big_tree(order);

            assert_eq!(height(&tree), 5);
            reload(&tree).unwrap();
        }
    }

    #[test]
    fn an_emptied_leaf_leaves_its_parent_and_the_leaf_before_points_past_it() {
        let mut tree = three_leaves();
        let children = |tree: &Tree| {
            let root = tree.page(tree.root);
            (0..=root.cell_count())
                .map(|i| root.child_at(i))
                .collect::<Vec<u64>>()
        };
        assert_eq!(children(&tree), [2, 5, 3]);
        let middle: Vec<Vec<u8>> = (0..tree.page(5).cell_count())
            .map(|i| tree.page(5).key_at(i).to_vec())
            .collect();
        tree.take_dirty();

        for key in &middle {
            tree.delete(key);
        }

        assert_eq!(children(&tree), [2, 3]);
        assert_eq!(tree.page(2).right_sibling(), Some(3));
        assert_eq!(tree.take_dirty(), BTreeSet::from([2, 4, 5]));
        reload(&tree).unwrap();
    }

    #[test]
    fn emptying_the_rightmost_leaf_hands_its_range_to_the_one_before() {
        let mut tree = big_tree(0..10);
        assert_eq!(height(&tree), 2);
        let root = tree.page(tree.root);
        let last = tree.page(root.child_at(root.cell_count()));
        let keys: Vec<Vec<u8>> = (0..last.cell_count())
            .map(|i| last.key_at(i).to_vec())
            .collect();
        let leaves = root.cell_count() + 1;

        for key in &keys {
            tree.delete(key);
        }

        let root = tree.page(tree.root);
        assert_eq!(root.cell_count() + 1, leaves - 1);
        assert_eq!(
            tree.page(root.child_at(root.cell_count())).right_sibling(),
            None
        );
        // A key past the old last leaf goes to the new last one.
        let (key, value) = big_entry(1000);
        tree.put(&key, &value);
        assert_eq!(reload(&tree).unwrap().get(&key), Some(&value[..]));
    }

    #[test]
    fn a_root_left_with_one_child_hands_over_and_the_tree_shrinks_to_a_leaf() {
        let mut tree = big_tree(0..1000);
        assert_eq!(height(&tree), 5);

        // Empty everything but the last few keys, from the front.
        for i in 0..997 {
            tree.delete(&big_entry(i).0);
            reload(&tree).unwrap();
        }

        assert_eq!(height(&tree), 1);
        let read = reload(&tree).unwrap();
        for i in 997..1000 {
            let (key, value) = big_entry(i);
            assert_eq!(read.get(&key), Some(&value[..]));
        }
    }

    #[test]
    fn a_tree_emptied_in_scattered_order_reads_back_at_every_step() {
        let mut tree = big_tree(0..300);

        for i in (0..300).map(|i| i * 7919 % 300) {
            tree.delete(&big_entry(i).0);
            let read = reload(&tree).unwrap();
            assert_eq!(read.get(&big_entry(i).0), None);
        }

        assert_eq!(height(&tree), 1);
        assert_eq!(tree.page(tree.root).cell_count(), 0);
    }

    /// `tree` with its page `page_no` changed by `break_it`.
    fn broken(tree: &Tree, page_no: u64, break_it: impl FnOnce(&mut Page)) -> Result<Tree> {
        let mut tree = tree.clone();
        break_it(tree.page_mut(page_no));
        reload(&tree)
    }

    #[test]
    fn trees_whose_pages_do_not_fit_together_are_refused() {
        let tree = tree_with(0..37);
        let refused = |result: Result<Tree>| matches!(result, Err(Error::Corrupt(_)));

        // A child outside the file, and a child that is the root itself.
        assert!(refused(broken(&tree, 4, |root| root.set_child_at(1, 9))));
        assert!(refused(broken(&tree, 4, |root| root.set_child_at(1, 1))));
        assert!(refused(broken(&tree, 4, |root| root.set_child_at(1, 4))));
        // Both children are the same leaf.
        assert!(refused(broken(&tree, 4, |root| root.set_child_at(1, 2))));
        // The right leaf holds a key below the separator.
        assert!(refused(broken(&tree, 3, |leaf| leaf
            .put(b"\0", b"x")
            .unwrap())));
        // The left leaf holds the separator itself.
        let separator = tree.page(3).key_at(0).to_vec();
        assert!(refused(broken(&tree, 2, |leaf| leaf
            .put(&separator, b"x")
            .unwrap())));
        // The leaves do not point to each other.
        assert!(refused(
            broken(&tree, 2, |leaf| leaf.set_right_sibling(None))
        ));
        assert!(refused(
            broken(&tree, 3, |leaf| leaf.set_right_sibling(Some(2)))
        ));
    }

    #[test]
    fn leaves_at_different_depths_are_refused() {
        let mut tree = tree_with(0..37);
        // The root's right child becomes an internal page over the right
        // leaf and a new empty one, so leaves are at depths 1 and 2.
        let empty = tree.push(Page::empty_leaf());
        tree.page_mut(3).set_right_sibling(Some(empty));
        let inner = tree.push(Page::new_internal(3, &u32::MAX.to_be_bytes(), empty));
        tree.page_mut(4).set_child_at(1, inner);

        assert!(matches!(reload(&tree), Err(Error::Corrupt(_))));
    }

    #[test]
    fn a_tree_deeper_than_any_oakdb_writes_is_refused() {
        // A chain of internal pages with no cells, each over the next, down
        // to the one leaf. Every other check passes.
        let mut tree = Tree::new();
        for _ in 0..=MAX_DEPTH {
            let mut inner = Page::new_internal(0, b"k", tree.root);
            inner.delete(b"k");
            tree.root = tree.push(inner);
        }

        assert!(matches!(reload(&tree), Err(Error::Corrupt(_))));
    }
}
