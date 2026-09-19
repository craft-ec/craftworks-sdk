//! The real prolly tree behind [`Store`], in memory.
//!
//! An app written against the SDK today runs on the data structure it will run
//! on later: the same nodes, the same root hash, the same history-independent
//! shape. What is missing is only the network — every block is held here, so
//! nothing ever has to wait.
//!
//! Because every block is held, [`ReadError::Need`] cannot happen. It is not
//! handled gracefully: it is a bug in this file, and it says so and stops.
//! Papering over it would turn "this store forgot to keep a block it emitted"
//! into a silently wrong answer.

use crate::store::{sorted_edits, Edit, Store};
use freenet_prolly::apply::{apply_into, Edit as TreeEdit};
use freenet_prolly::build::init;
use freenet_prolly::node::{Node, Value};
use freenet_prolly::range::{range, read_value, Range};
use freenet_prolly::read::{get, height};
use freenet_prolly::store::{Blocks, BlocksMut, MemBlocks, ReadError};
use freenet_prolly::Cid;
use std::ops::Bound;

/// What a store holds, for the builder's tree panel and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub blocks: usize,
    pub bytes: usize,
    /// Levels in the tree; 1 is a single leaf.
    pub height: usize,
}

/// Knobs that exist so a test can run a control. The defaults are the real
/// behaviour.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Keep the blocks of values too large to inline. With this off a value
    /// over 1 KiB can be written and never read back — which is what the
    /// negative control proves the suite would catch.
    pub keep_value_blocks: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep_value_blocks: true,
        }
    }
}

/// Where a store's blocks go.
///
/// The library writes emitted blocks through [`BlocksMut`], so the negative
/// control lives here rather than in a step the store can skip: a sink that
/// drops the blocks of values too large to inline. That is the one mistake a
/// hand-written absorb loop makes, and it has to stay expressible or nothing
/// proves the suite would catch it.
#[derive(Default)]
struct Kept {
    inner: MemBlocks,
    drop_value_blocks: bool,
}

impl Blocks for Kept {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.inner.get(cid)
    }
}

impl BlocksMut for Kept {
    fn insert_block(&mut self, cid: Cid, bytes: &[u8]) {
        if self.drop_value_blocks && Node::parse(bytes).is_err() {
            return;
        }
        self.inner.insert(cid, bytes);
    }
}

pub struct TreeStore {
    blocks: Kept,
    root: Cid,
}

impl Default for TreeStore {
    fn default() -> Self {
        Self::with_options(Options::default())
    }
}

impl TreeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(opts: Options) -> Self {
        let mut blocks = Kept {
            drop_value_blocks: !opts.keep_value_blocks,
            ..Kept::default()
        };
        let root = init(&mut blocks);
        TreeStore { blocks, root }
    }

    /// The tree's root hash. This is the whole contents in 32 bytes: two stores
    /// holding the same records report the same root whatever order they were
    /// written in.
    pub fn root(&self) -> Cid {
        self.root
    }

    pub fn stats(&self) -> Stats {
        Stats {
            blocks: self.blocks.inner.0.len(),
            bytes: self.blocks.inner.0.values().map(Vec::len).sum(),
            height: height(&self.blocks, &self.root).unwrap_or_else(|e| Self::impossible(e)),
        }
    }

    /// The bytes of a value, wherever the format put it.
    fn materialise(&self, v: Value<'_>) -> Vec<u8> {
        match read_value(&self.blocks, v) {
            Ok(b) => b.to_vec(),
            Err(e) => Self::impossible(e),
        }
    }

    /// Every block is in memory, so a read that asks for one is this file being
    /// wrong — most likely a block that was emitted and not kept.
    fn impossible(e: ReadError) -> ! {
        match e {
            ReadError::Need(ids) => unreachable!(
                "TreeStore is in memory, so no block can be missing — but it asked for {}. \
                 A block this store emitted was not kept.",
                ids.iter()
                    .map(|c| crate::hex(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            other => unreachable!("the tree refused a block this store wrote: {other:?}"),
        }
    }
}

impl Store for TreeStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match get(&self.blocks, &self.root, key) {
            Ok(v) => v.map(|v| self.materialise(v)),
            Err(e) => Self::impossible(e),
        }
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.apply_batch(&[(key.to_vec(), Edit::Put(value.to_vec()))]);
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        let existed = self.get(key).is_some();
        self.apply_batch(&[(key.to_vec(), Edit::Delete)]);
        existed
    }

    fn scan(&self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        if lo >= hi || limit == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut req = Range {
            lo: Bound::Included(lo.to_vec()),
            hi: Bound::Excluded(hi.to_vec()),
            reverse,
            after: None,
            // One page unless the caller wants more than a page holds.
            max_entries: limit.min(4096),
            max_bytes: usize::MAX,
        };
        loop {
            let page = match range(&self.blocks, &self.root, &req) {
                Ok(p) => p,
                Err(freenet_prolly::range::RangeError::Read(e)) => Self::impossible(e),
                Err(e) => unreachable!("the SDK asked for an impossible range: {e:?}"),
            };
            for (k, v) in &page.entries {
                if out.len() == limit {
                    break;
                }
                out.push((k.clone(), self.materialise(*v)));
            }
            let (next, finished) = (page.next.clone(), page.finished());
            debug_assert!(page.need.is_empty(), "in memory nothing can be missing");
            drop(page);
            if finished || out.len() == limit {
                return out;
            }
            req.after = next;
        }
    }

    /// One `apply`, so a record and everything written with it become one new
    /// root. Writing them one at a time would mint a root for a state the app
    /// never had.
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) {
        if edits.is_empty() {
            return;
        }
        let batch: Vec<(Vec<u8>, TreeEdit)> = sorted_edits(edits.to_vec())
            .into_iter()
            .map(|(k, e)| {
                (
                    k,
                    match e {
                        Edit::Put(v) => TreeEdit::Put(v),
                        Edit::Delete => TreeEdit::Delete,
                    },
                )
            })
            .collect();
        // `apply_into` writes every emitted block back; nothing is ever removed,
        // so superseded nodes stay. `replaced` is a hint for a store that wants
        // to collect them, not permission to drop history.
        let applied = match apply_into(&mut self.blocks, &self.root, &batch) {
            Ok(a) => a,
            Err(freenet_prolly::apply::ApplyError::Read(e)) => Self::impossible(e),
            // A refused batch emits nothing and changes nothing, so leaving the
            // store exactly as it was is not cleanup — it is what already
            // happened.
            //
            // `Store` has no error channel, so this cannot be reported; see the
            // trait's doc. `Db::write` screens every key and value against these
            // same limits before a store is touched and returns an error the app
            // can handle, so reaching here means something bypassed it.
            Err(e) => unreachable!("the SDK built a batch the tree refuses: {e:?}"),
        };
        self.root = applied.root;
    }
}
