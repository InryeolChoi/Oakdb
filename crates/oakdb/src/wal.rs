//! The write-ahead log (ADR 0007, issue #22).
//!
//! A commit appends a full image of every page it changed, then a commit
//! record holding the new meta fields, and syncs once before it answers.
//! The data file is only written by checkpoints (#23, design 0011), which
//! move the pages the WAL holds there and start the WAL again; opening
//! replays the WAL over the data file.
//!
//! ```text
//! header         magic "oakwal\0\0" · checkpoint (u64) · crc (u32)
//! page record    kind 1 (u8) · page_no (u64) · page (4096 bytes) · crc (u32)
//! commit record  kind 2 (u8) · root · page_count · free_head · free_count
//!                (u64 each, free_head 0 = none) · crc (u32)
//! ```
//!
//! Integers are little-endian. The header names the checkpoint the WAL
//! follows (0 for a new database), and its crc is the CRC-32C of the bytes
//! before it, carried on from [`START`]. Each record's crc is the CRC-32C
//! of its bytes before the crc, carried on from the crc of the record
//! before it, or of the header for the first. A record only checks out in
//! the place it was written, after the records it was written after and
//! under the header of the same checkpoint, so a stale record left behind
//! a shorter one or from an older WAL never joins the chain. Replay stops
//! at the first record that does not check out, and page records count
//! only once a commit record follows them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::checksum;
use crate::error::{Error, Result};
use crate::fs::{Fs, FsFile};
use crate::meta::Meta;
use crate::page::PAGE_SIZE;

/// Name of the WAL file inside the database directory.
pub const WAL_FILE: &str = "oakdb.wal";

/// Where opening keeps what it cuts off the WAL, added to the end, so a
/// commit lost to a damaged record can still be looked at by hand.
pub(crate) const CUT_FILE: &str = "oakdb.wal.cut";

/// Where opening writes the WAL it read before renaming it over
/// [`WAL_FILE`] (design 0014).
pub(crate) const NEW_FILE: &str = "oakdb.wal.new";

/// The crc the chain starts from. Not 0, so a WAL of zeros never checks
/// out.
pub(crate) const START: u32 = 0x0AC0_DB10;

const MAGIC: &[u8; 8] = b"oakwal\0\0";
/// Bytes of the header: magic, checkpoint number, crc.
const HEADER: usize = 20;
/// [`HEADER`] as a file length.
pub(crate) const HEADER_LEN: u64 = HEADER as u64;
const CHECKPOINT_AT: usize = 8;
const HEADER_BODY: usize = CHECKPOINT_AT + 8;

const PAGE: u8 = 1;
const COMMIT: u8 = 2;
const CRC_LEN: usize = 4;
/// Where a page record keeps its page number and its page image.
const PAGE_NO_AT: usize = 1;
const IMAGE_AT: usize = PAGE_NO_AT + 8;
/// A page record's bytes before its crc: kind, page number, page.
const PAGE_BODY: usize = IMAGE_AT + PAGE_SIZE;
/// Where a commit record keeps its meta fields, and how many there are.
const FIELDS_AT: usize = 1;
const FIELDS: usize = 4;
/// A commit record's bytes before its crc: kind and the meta fields.
const COMMIT_BODY: usize = FIELDS_AT + FIELDS * 8;

/// The bytes of a commit of `pages` pages: one page record each and a
/// commit record.
pub(crate) const fn commit_len(pages: usize) -> u64 {
    (pages * (PAGE_BODY + CRC_LEN) + COMMIT_BODY + CRC_LEN) as u64
}

/// Where the last whole commit ends, and the crc the next record carries
/// on from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) end: u64,
    pub(crate) crc: u32,
}

impl Tail {
    /// The tail of a WAL that follows `checkpoint` and holds no commit yet.
    pub(crate) fn after_header(checkpoint: u64) -> Tail {
        Tail {
            end: HEADER_LEN,
            crc: encode_header(checkpoint).1,
        }
    }
}

/// The header of a WAL that follows checkpoint `checkpoint`, and the crc
/// its first record carries on from.
pub(crate) fn encode_header(checkpoint: u64) -> ([u8; HEADER], u32) {
    let mut header = [0; HEADER];
    header[..CHECKPOINT_AT].copy_from_slice(MAGIC);
    header[CHECKPOINT_AT..HEADER_BODY].copy_from_slice(&checkpoint.to_le_bytes());
    let crc = crc32c::crc32c_append(START, &header[..HEADER_BODY]);
    header[HEADER_BODY..].copy_from_slice(&crc.to_le_bytes());
    (header, crc)
}

/// What replaying a WAL found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// The checkpoint the header names, or `None` if the WAL holds no
    /// header that checks out and nothing after it: new, or cut short
    /// while it was started.
    pub(crate) checkpoint: Option<u64>,
    /// The latest committed image of each page the WAL holds, with its
    /// page checksum filled in.
    pub(crate) pages: BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    /// The meta fields of the last commit, if there is one.
    pub(crate) meta: Option<Meta>,
    /// Where the last commit ends, or the header if there is none:
    /// anything after it is to be cut off. Meaningless without a header.
    pub(crate) tail: Tail,
}

/// The WAL: where commits are appended, and what a checkpoint is to move.
pub(crate) struct Wal<F: FsFile> {
    file: F,
    tail: Tail,
    /// The checkpoint the WAL follows.
    checkpoint: u64,
    /// Every page a commit in the WAL holds an image of: the pages the
    /// next checkpoint moves to the data file.
    pages: BTreeSet<u64>,
}

impl<F: FsFile> Wal<F> {
    /// The WAL in `file`, which follows `checkpoint` and whose last commit
    /// ends at `tail`, with images of `pages`.
    pub(crate) fn new(file: F, tail: Tail, checkpoint: u64, pages: BTreeSet<u64>) -> Self {
        Wal {
            file,
            tail,
            checkpoint,
            pages,
        }
    }

    /// Empties the WAL and starts it again after `checkpoint`. The length
    /// change is synced before the header is written, so a header torn by
    /// a power cut is never followed by older records, and a WAL no longer
    /// than a header never holds one ([`replay`] relies on it). Only a
    /// checkpoint calls it, on a WAL with commits in it, so reads seeing it
    /// empty while the disk does not cannot happen here: the engine stops
    /// at a failed sync, and opening writes a new WAL file (design 0014).
    pub(crate) fn start(&mut self, checkpoint: u64) -> Result<()> {
        if self.file.size()? > 0 {
            self.file.set_len(0)?;
            self.file.sync_data()?;
        }
        self.file.write_all_at(&encode_header(checkpoint).0, 0)?;
        self.file.sync_data()?;
        self.tail = Tail::after_header(checkpoint);
        self.checkpoint = checkpoint;
        self.pages.clear();
        Ok(())
    }

    /// Appends a commit of `pages` and `meta` and syncs it. If that fails,
    /// the tail stays where it was, and the caller stops the engine: it
    /// writes nothing to the WAL again, and opening the database again
    /// finds the whole commit or cuts off what part of it reached the file.
    pub(crate) fn append(
        &mut self,
        pages: &[(u64, &[u8; PAGE_SIZE])],
        meta: Meta,
    ) -> std::io::Result<()> {
        let (bytes, crc) = encode_commit(self.tail.crc, pages, meta);
        self.file.write_all_at(&bytes, self.tail.end)?;
        self.file.sync_data()?;
        self.tail = Tail {
            end: self.tail.end + bytes.len() as u64,
            crc,
        };
        self.pages.extend(pages.iter().map(|&(page_no, _)| page_no));
        Ok(())
    }

    /// The length of the WAL up to the end of its last commit.
    pub(crate) fn len(&self) -> u64 {
        self.tail.end
    }

    /// Whether the WAL holds any commit.
    pub(crate) fn has_commits(&self) -> bool {
        self.tail.end > HEADER_LEN
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.checkpoint
    }

    pub(crate) fn pages(&self) -> &BTreeSet<u64> {
        &self.pages
    }
}

/// The records of one commit: a page record for each of `pages` (number
/// and image, whose page checksum is filled in on the way), then a commit
/// record for `meta`, chained on from `prev`. Returns the bytes and the
/// crc the next commit carries on from.
pub(crate) fn encode_commit(
    prev: u32,
    pages: &[(u64, &[u8; PAGE_SIZE])],
    meta: Meta,
) -> (Vec<u8>, u32) {
    let mut out = Vec::with_capacity(commit_len(pages.len()) as usize);
    let mut crc = prev;
    for &(page_no, page) in pages {
        let start = out.len();
        out.push(PAGE);
        out.extend_from_slice(&page_no.to_le_bytes());
        out.extend_from_slice(page);
        checksum::seal(page_no, &mut out[start + IMAGE_AT..]);
        crc = seal(&mut out, start, crc);
    }
    let start = out.len();
    out.push(COMMIT);
    for field in meta_fields(meta) {
        out.extend_from_slice(&field.to_le_bytes());
    }
    crc = seal(&mut out, start, crc);
    (out, crc)
}

/// The meta fields in the order a commit record holds them.
fn meta_fields(meta: Meta) -> [u64; FIELDS] {
    [
        meta.root_page,
        meta.page_count,
        meta.free_head.unwrap_or(0),
        meta.free_count,
    ]
}

/// The meta of a commit record's fields, the inverse of [`meta_fields`].
fn meta_from([root_page, page_count, free_head, free_count]: [u64; FIELDS]) -> Meta {
    Meta {
        root_page,
        page_count,
        free_head: Some(free_head).filter(|&page_no| page_no != 0),
        free_count,
    }
}

/// Appends the crc of `out[start..]`, carried on from `prev`
/// (`crc32c_append(prev, record)`), and returns it.
fn seal(out: &mut Vec<u8>, start: usize, prev: u32) -> u32 {
    let crc = crc32c::crc32c_append(prev, &out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
    crc
}

/// Makes `bytes` the WAL of the database in `dir`, durably, and returns
/// the file that holds it. They go to a new file, synced, which is then
/// renamed over the WAL and the directory synced: the old WAL is never
/// written over, so a power cut leaves it or the new one whole, whatever
/// the disk does to a sector it was writing (design 0014). And as the
/// bytes are written again, the next sync saves them even if they were
/// read from a page cache that holds what a failed sync never got to the
/// disk (issue #59).
pub(crate) fn write_new<F: Fs>(fs: &F, dir: &Path, bytes: &[u8]) -> Result<F::File> {
    let new = dir.join(NEW_FILE);
    // A file left by an opening cut short holds nothing anyone needs, but
    // it may be longer, and end in a commit chained to these bytes that
    // this opening did not see: cut it to their length, or a crash later
    // could bring that commit back (PR #60, mutant K7).
    let file = fs.open(&new)?;
    file.write_all_at(bytes, 0)?;
    let len = u64::try_from(bytes.len()).expect("a WAL read into memory fits in a u64");
    file.set_len(len)?;
    file.sync_data()?;
    fs.rename(&new, &dir.join(WAL_FILE))?;
    fs.sync_dir(dir)?;
    Ok(file)
}

/// Replays the WAL `bytes`: every commit up to the first record that does
/// not check out.
///
/// A WAL no longer than a header holds no record: it is emptied and synced
/// before a header is written. So if its header does not check out, which
/// a cut while writing the header can leave (the new length lands, the
/// bytes do not), it counts as having none. A longer WAL whose header does
/// not check out is refused: the records after it may be acknowledged
/// commits.
pub(crate) fn replay(bytes: &[u8]) -> Result<Replayed> {
    let mut done = Replayed {
        checkpoint: None,
        pages: BTreeMap::new(),
        meta: None,
        tail: Tail { end: 0, crc: START },
    };
    let Some(header) = bytes.get(..HEADER) else {
        return Ok(done);
    };
    let checkpoint = u64::from_le_bytes(
        header[CHECKPOINT_AT..HEADER_BODY]
            .try_into()
            .expect("the field is 8 bytes"),
    );
    if header != encode_header(checkpoint).0 {
        if bytes.len() as u64 == HEADER_LEN {
            return Ok(done);
        }
        return Err(Error::Corrupt("WAL header is damaged".into()));
    }
    done.checkpoint = Some(checkpoint);
    done.tail = Tail::after_header(checkpoint);
    // Pages of the commit being read, applied once its commit record is.
    let mut pending = Vec::new();
    let (mut at, mut crc) = (HEADER, done.tail.crc);
    loop {
        let body = match bytes.get(at) {
            Some(&PAGE) => PAGE_BODY,
            Some(&COMMIT) => COMMIT_BODY,
            _ => return Ok(done),
        };
        let Some(record) = bytes.get(at..at + body + CRC_LEN) else {
            return Ok(done);
        };
        let (body, stored) = record.split_at(body);
        crc = crc32c::crc32c_append(crc, body);
        if stored != crc.to_le_bytes() {
            return Ok(done);
        }
        at += record.len();
        let u64_at = |i: usize| u64::from_le_bytes(body[i..i + 8].try_into().unwrap());
        if body[0] == PAGE {
            let image: [u8; PAGE_SIZE] = body[IMAGE_AT..].try_into().unwrap();
            pending.push((u64_at(PAGE_NO_AT), Box::new(image)));
            continue;
        }
        done.pages.extend(pending.drain(..));
        done.meta = Some(meta_from(std::array::from_fn(|i| {
            u64_at(FIELDS_AT + 8 * i)
        })));
        done.tail = Tail {
            end: at as u64,
            crc,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(byte: u8) -> [u8; PAGE_SIZE] {
        [byte; PAGE_SIZE]
    }

    /// The image the WAL keeps of `page(byte)` at `page_no`: with its page
    /// checksum.
    fn image(page_no: u64, byte: u8) -> [u8; PAGE_SIZE] {
        checksum::sealed(page_no, &page(byte))
    }

    fn meta(root: u64) -> Meta {
        Meta {
            root_page: root,
            page_count: 5,
            free_head: Some(4),
            free_count: 1,
        }
    }

    /// A WAL after checkpoint 3 with two commits: pages 2 and 3, then page
    /// 3 again. Returns it, where the first commit ends, and the last crc.
    fn two_commits() -> (Vec<u8>, usize, u32) {
        let (header, crc) = encode_header(3);
        let mut wal = header.to_vec();
        let (first, crc) = encode_commit(crc, &[(2, &page(1)), (3, &page(2))], meta(2));
        wal.extend(first);
        let first = wal.len();
        let (second, crc) = encode_commit(crc, &[(3, &page(3))], meta(3));
        wal.extend(second);
        (wal, first, crc)
    }

    #[test]
    fn a_wal_shorter_than_a_header_holds_nothing() {
        for len in [0, 1, HEADER - 1] {
            let replayed = replay(&encode_header(3).0[..len]).unwrap();
            assert_eq!(replayed.checkpoint, None, "len {len}");
            assert_eq!(replayed.meta, None);
            assert!(replayed.pages.is_empty());
        }
    }

    #[test]
    fn a_wrong_header_with_nothing_after_it_counts_as_none() {
        // Emptying is synced before the header is written, so a WAL no
        // longer than a header holds no record, whatever its bytes.
        let mut header = encode_header(3).0;
        header[17] ^= 1;
        for wal in [header, [0; HEADER]] {
            assert_eq!(replay(&wal).unwrap().checkpoint, None);
        }
    }

    #[test]
    fn a_header_alone_names_its_checkpoint_and_holds_nothing() {
        let replayed = replay(&encode_header(3).0).unwrap();
        assert_eq!(replayed.checkpoint, Some(3));
        assert_eq!(replayed.meta, None);
        assert_eq!(replayed.tail, Tail::after_header(3));
    }

    #[test]
    fn a_damaged_header_is_refused() {
        let (wal, ..) = two_commits();
        for at in [0, 9, HEADER - 1] {
            let mut damaged = wal.clone();
            damaged[at] ^= 1;
            assert!(
                matches!(replay(&damaged), Err(Error::Corrupt(_))),
                "byte {at}"
            );
        }
        // A header of zeros too: a WAL emptied by a length change that
        // landed as zeros is never longer than a header.
        assert!(replay(&[0; 100]).is_err());
    }

    #[test]
    fn records_written_after_one_checkpoint_do_not_check_out_after_another() {
        let (mut wal, ..) = two_commits();
        wal[..HEADER].copy_from_slice(&encode_header(4).0);

        let replayed = replay(&wal).unwrap();

        assert_eq!(replayed.checkpoint, Some(4));
        assert_eq!(replayed.meta, None);
    }

    #[test]
    fn replay_gives_the_latest_image_of_each_page_and_the_last_meta() {
        let (wal, _, crc) = two_commits();

        let replayed = replay(&wal).unwrap();

        assert_eq!(replayed.checkpoint, Some(3));
        assert_eq!(replayed.meta, Some(meta(3)));
        assert_eq!(replayed.pages.len(), 2);
        assert_eq!(*replayed.pages[&2], image(2, 1));
        assert_eq!(*replayed.pages[&3], image(3, 3));
        assert_eq!(replayed.tail.end, wal.len() as u64);
        assert_eq!(replayed.tail.crc, crc);
    }

    #[test]
    fn a_commit_is_one_record_per_page_and_a_commit_record() {
        let (one, _) = encode_commit(START, &[(2, &page(1))], meta(2));
        let (two, _) = encode_commit(START, &[(2, &page(1)), (3, &page(1))], meta(2));
        let page_record = 1 + 8 + PAGE_SIZE + 4;
        assert_eq!(two.len() - one.len(), page_record);
        assert_eq!(one.len(), page_record + 1 + 4 * 8 + 4);
        assert_eq!(commit_len(2), two.len() as u64);
    }

    #[test]
    fn replay_stops_at_the_first_record_that_does_not_check_out() {
        let (mut wal, first, _) = two_commits();
        // One bit of the second commit's page image.
        wal[first + 100] ^= 1;

        let replayed = replay(&wal).unwrap();

        assert_eq!(replayed.meta, Some(meta(2)));
        assert_eq!(*replayed.pages[&3], image(3, 2));
        assert_eq!(replayed.tail.end, first as u64);
    }

    #[test]
    fn pages_without_a_commit_record_are_not_applied() {
        let (wal, first, _) = two_commits();
        // The second commit's page record is whole; its commit record is
        // cut short.
        for cut in [first + 1, wal.len() - 1 - 4 * 8, wal.len() - 1] {
            let replayed = replay(&wal[..cut]).unwrap();
            assert_eq!(replayed.meta, Some(meta(2)), "cut at {cut}");
            assert_eq!(*replayed.pages[&3], image(3, 2), "cut at {cut}");
            assert_eq!(replayed.tail.end, first as u64, "cut at {cut}");
        }
    }

    #[test]
    fn a_commit_out_of_its_place_in_the_chain_is_refused() {
        let (wal, first, _) = two_commits();
        let header = &wal[..HEADER];
        // The second commit alone, as if the first had never been written.
        let mut alone = header.to_vec();
        alone.extend_from_slice(&wal[first..]);
        assert_eq!(replay(&alone).unwrap().meta, None);
        // The first commit written again after itself.
        let mut twice = wal[..first].to_vec();
        twice.extend_from_slice(&wal[HEADER..first]);
        assert_eq!(replay(&twice).unwrap().tail.end, first as u64);
    }

    #[test]
    fn an_unknown_record_kind_ends_the_replay() {
        let (mut wal, first, _) = two_commits();
        wal[first] = 9;
        assert_eq!(replay(&wal).unwrap().tail.end, first as u64);
    }

    #[test]
    fn a_commit_without_a_free_list_keeps_none() {
        let meta = Meta {
            free_head: None,
            free_count: 0,
            ..meta(2)
        };
        let (header, crc) = encode_header(0);
        let mut wal = header.to_vec();
        wal.extend(encode_commit(crc, &[(2, &page(1))], meta).0);
        assert_eq!(replay(&wal).unwrap().meta, Some(meta));
    }
}
