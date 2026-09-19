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
use freenet_prolly::apply::{apply, Edit as TreeEdit};
use freenet_prolly::node::{Node, Value, MAX_INLINE};
use freenet_prolly::range::{range, value_bytes, Range};
use freenet_prolly::store::{Blocks, MemBlocks, ReadError};
use freenet_prolly::{block_id, build::TreeBuilder, kind, read::get, Cid};
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

pub struct TreeStore {
    blocks: MemBlocks,
    root: Cid,
    opts: Options,
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
        // The empty tree is one empty leaf, and it has an id like any other.
        let mut blocks = MemBlocks::default();
        let root = TreeBuilder::new(|c, b: &[u8]| blocks.insert(c, b))
            .finish()
            .expect("an empty tree always builds");
        TreeStore { blocks, root, opts }
    }

    /// The tree's root hash. This is the whole contents in 32 bytes: two stores
    /// holding the same records report the same root whatever order they were
    /// written in.
    pub fn root(&self) -> Cid {
        self.root
    }

    pub fn stats(&self) -> Stats {
        let height = Node::parse(self.blocks.get(&self.root).expect("the root is held"))
            .map(|n| n.level() as usize + 1)
            .unwrap_or(0);
        Stats {
            blocks: self.blocks.0.len(),
            bytes: self.blocks.0.values().map(Vec::len).sum(),
            height,
        }
    }

    /// Nothing is ever removed: superseded nodes stay. `apply` reports which
    /// ones it replaced, but that is a hint for a store that wants to collect
    /// them, not permission to drop history.
    fn absorb(&mut self, emitted: Vec<(Cid, Vec<u8>)>) {
        for (c, b) in emitted {
            let is_node = Node::parse(&b).is_ok();
            if is_node || self.opts.keep_value_blocks {
                self.blocks.insert(c, &b);
            }
        }
    }

    fn value_of(v: &[u8]) -> TreeEdit {
        TreeEdit::Put(v.to_vec())
    }

    /// The bytes of a value, wherever the tree decided to put it.
    fn materialise(&self, v: Value<'_>) -> Vec<u8> {
        match v {
            Value::Inline(b) => b.to_vec(),
            Value::Ref { cid, len } => match value_bytes(&self.blocks, &cid, len) {
                Ok(b) => b.to_vec(),
                Err(e) => Self::impossible(e),
            },
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
                        Edit::Put(v) => Self::value_of(&v),
                        Edit::Delete => TreeEdit::Delete,
                    },
                )
            })
            .collect();
        let mut emitted = Vec::new();
        let applied = match apply(&self.blocks, &self.root, &batch, |c, b| {
            emitted.push((c, b.to_vec()))
        }) {
            Ok(a) => a,
            Err(freenet_prolly::apply::ApplyError::Read(e)) => Self::impossible(e),
            // A refused batch emits nothing and changes nothing, so leaving the
            // store exactly as it was is not cleanup — it is what already
            // happened. The SDK screens its own keys and values, so reaching
            // here means the SDK let through something it should not have.
            Err(e) => unreachable!("the SDK built a batch the tree refuses: {e:?}"),
        };
        self.absorb(emitted);
        self.root = applied.root;
    }
}

/// The id a value of these bytes would have if the tree stored it in its own
/// block. Values at or below [`MAX_INLINE`] live inside their leaf and have no
/// block of their own.
pub fn value_block_id(bytes: &[u8]) -> Option<Cid> {
    (bytes.len() > MAX_INLINE).then(|| block_id(kind::RAW, bytes))
}
