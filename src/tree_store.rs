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

use crate::store::{sorted_edits, Delta, Edit, Read, Reads, Store};
use freenet_prolly::apply::{apply_into, Edit as TreeEdit};
use freenet_prolly::build::init;
use freenet_prolly::node::{Node, Value};
use freenet_prolly::range::{range, read_value, Range};
use freenet_prolly::read::{get, height};
use freenet_prolly::rs::PARITY;
use freenet_prolly::store::{Blocks, BlocksMut, MemBlocks, ReadError};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// One group's owed redundancy: the three parity blocks and the members they
/// protect.
///
/// The unit an engine works in. Three parity blocks are one group's
/// redundancy and are only worth anything together, so "how much is owed" is
/// answered in groups as well as in blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwedGroup {
    /// The three parity block ids, in the order the node lists them.
    pub parity: [Cid; PARITY],
    /// The blocks this group's parity covers.
    pub members: Vec<Cid>,
}

/// Parity blocks a write coded that have not been published: a debt, not a
/// value.
///
/// `#[must_use]` because the whole point is that they cannot go missing
/// quietly. A node lists its parity ids the moment it is written; if the bytes
/// are never put, the tree still looks complete — same root, same ids, every
/// reader satisfied — and the redundancy simply does not exist. Nothing
/// downstream can detect it. So the type refuses to be dropped without a word.
///
/// [`OwedParity::forget`] is the way to say "yes, really, discard these", and
/// it is spelled as a deliberate act rather than a `let _ =`.
#[must_use = "these parity blocks have not been put: dropping them loses the \
              redundancy the tree already promises. Put them, or call \
              `forget()` to say that is intended"]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwedParity {
    blocks: Vec<(Cid, Vec<u8>)>,
}

impl OwedParity {
    /// How many blocks are owed.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The blocks, to be PUT after the head.
    pub fn into_blocks(self) -> Vec<(Cid, Vec<u8>)> {
        self.blocks
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Cid, &[u8])> {
        self.blocks.iter().map(|(c, b)| (c, b.as_slice()))
    }

    /// Discard the debt on purpose.
    ///
    /// For a caller that is throwing the tree away too — a test, a scratch
    /// build, an oracle. It exists so that discarding is something someone
    /// wrote down, rather than the default that happens when nobody thinks
    /// about it.
    pub fn forget(self) {}
}

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
    /// Record the parity blocks each write codes. With this off the store
    /// writes nodes listing parity ids and keeps none of the bytes — the tree
    /// looks identical and has no redundancy behind it, which is the state
    /// this SDK was in before the library reported them at all. The control
    /// for [`TreeStore::owed_parity`] turns it off, so that assertion is known
    /// to be capable of failing.
    pub track_owed_parity: bool,
    /// Drop the debt for groups the current tree no longer lists. With this
    /// off the owed map only GROWS: a re-coded group's superseded trio stays
    /// owed for ever, and a writer draining the map puts parity for groups
    /// that are not in the tree. That is the negative control for
    /// [`TreeStore::owed_groups`], and it is a real state the code was in.
    pub prune_superseded_parity: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep_value_blocks: true,
            track_owed_parity: true,
            prune_superseded_parity: true,
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
    /// Parity blocks this store has been handed and has NOT published.
    ///
    /// Kept apart from `blocks` on purpose. A node lists its parity ids as soon
    /// as it is written, but the bytes behind them are separate blocks that a
    /// commit puts AFTER the head (ARCHITECTURE §7) — so until they are out,
    /// that group has no redundancy, and folding them in here would record a
    /// debt as though it were paid. In memory the distinction costs nothing and
    /// buys the one number a caller needs: how much redundancy it still owes.
    ///
    /// A later edit to a group whose parity was never put falls back to a full
    /// recode, because a correction needs the old parity to correct.
    owed: BTreeMap<Cid, Vec<u8>>,
    track_owed: bool,
    prune_superseded: bool,
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
        TreeStore {
            blocks,
            root,
            owed: BTreeMap::new(),
            track_owed: opts.track_owed_parity,
            prune_superseded: opts.prune_superseded_parity,
        }
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

    /// Every block this store holds, as (id, bytes).
    ///
    /// A store nobody can walk is a store nobody can check. The parity rule
    /// (freenet-prolly#19) is a property of each NODE, and the only way to know
    /// that the nodes this SDK writes satisfy it is to look at all of them —
    /// including the superseded ones, which stay, and which a reader can still
    /// be handed.
    pub fn blocks(&self) -> impl Iterator<Item = (&Cid, &[u8])> {
        self.blocks.inner.0.iter().map(|(c, b)| (c, b.as_slice()))
    }

    /// How many parity blocks this store holds but has not published.
    ///
    /// Zero means every group of the current tree has its redundancy out —
    /// which, for this in-memory store, is true only of a store nothing has
    /// been written to, since nothing here ever publishes. The number is what a
    /// networked store would work down, and what a panel shows as the tree's
    /// unpaid durability.
    pub fn owed_parity(&self) -> usize {
        self.owed.len()
    }

    /// The owed parity blocks themselves, as `(id, bytes)`.
    ///
    /// What a commit puts after the head. Ordered by id so two stores holding
    /// the same debt hand it over in the same order. This BORROWS: the store
    /// still owes them afterwards, which is why it is safe to ignore.
    pub fn owed(&self) -> impl Iterator<Item = (&Cid, &[u8])> {
        self.owed.iter().map(|(c, b)| (c, b.as_slice()))
    }

    /// The nodes of the CURRENT tree.
    ///
    /// Superseded nodes are never deleted -- another tree may still use them,
    /// and a reader can still be handed one -- so "every node this store
    /// holds" is not the same question as "what this tree promises". The debt
    /// is about the current tree: parity for a group no longer reachable from
    /// the root protects bytes no reader will ask for, and putting it is a PUT
    /// bought for nothing.
    fn live_nodes(&self) -> Vec<Vec<u8>> {
        let mut seen: BTreeSet<Cid> = BTreeSet::new();
        let mut out = Vec::new();
        let mut stack = vec![self.root];
        while let Some(cid) = stack.pop() {
            if !seen.insert(cid) {
                continue;
            }
            let Some(bytes) = self.blocks.get(&cid) else {
                continue;
            };
            let Ok(node) = Node::parse(bytes) else {
                continue;
            };
            if !node.is_leaf() {
                for i in 0..node.len() {
                    stack.push(node.child(i).0);
                }
            }
            out.push(bytes.to_vec());
        }
        out
    }

    /// Every parity id the current tree lists.
    fn live_parity(&self) -> BTreeSet<Cid> {
        let mut out = BTreeSet::new();
        for bytes in self.live_nodes() {
            if let Ok(n) = Node::parse(&bytes) {
                out.extend(n.parity());
            }
        }
        out
    }

    /// Drop the debt for groups the current tree no longer lists.
    ///
    /// A group re-coded by a later write is SUPERSEDED: its members moved on,
    /// and its parity is redundancy for a version of the tree nobody will read.
    /// Without this the map only grows, and a writer draining it puts parity
    /// for groups that are not in the tree -- which is the same failure as
    /// reporting a block no node lists, arriving one write later.
    fn prune_owed(&mut self) {
        let live = self.live_parity();
        self.owed.retain(|cid, _| live.contains(cid));
    }

    /// The owed parity as GROUPS: what each set of three blocks protects.
    ///
    /// Three parity blocks are not three independent things — they are one
    /// group's redundancy, and they are only worth anything together. An
    /// engine putting them needs to know which members each set covers, so it
    /// can say what is still unprotected when a PUT fails partway.
    ///
    /// Derived from the nodes rather than recorded alongside, because the node
    /// is where the grouping is DEFINED: reading it back the way a checker
    /// reads it means the store cannot drift from the format. A group whose
    /// three ids are not all owed is not reported — partial redundancy is not
    /// a group.
    pub fn owed_groups(&self) -> Vec<OwedGroup> {
        let mut out = Vec::new();
        for bytes in self.live_nodes() {
            let Ok(node) = Node::parse(&bytes) else {
                continue;
            };
            let ids: Vec<Cid> = node.parity().collect();
            let groups = freenet_prolly::parity::group_members(&node);
            if ids.len() != groups.len() * PARITY {
                // A node whose parity count disagrees with its grouping should
                // never have been stored; it is not this function's job to
                // guess what it meant.
                continue;
            }
            for (i, (_, members)) in groups.into_iter().enumerate() {
                let trio = [ids[i * PARITY], ids[i * PARITY + 1], ids[i * PARITY + 2]];
                if !trio.iter().all(|c| self.owed.contains_key(c)) {
                    continue;
                }
                out.push(OwedGroup {
                    parity: trio,
                    members,
                });
            }
        }
        out.sort_by_key(|g| g.parity[0]);
        out.dedup_by_key(|g| g.parity[0]);
        out
    }

    /// Take the debt, leaving the store owing nothing.
    ///
    /// The store stops tracking these the moment they leave, so dropping the
    /// returned value drops the only record that they were owed — the tree
    /// keeps listing the ids and nothing anywhere knows the bytes were never
    /// put. That is the failure this whole change exists to prevent, so
    /// [`OwedParity`] is `#[must_use]`: ignoring it is a compile warning, not
    /// a silent loss of redundancy.
    pub fn take_owed(&mut self) -> OwedParity {
        OwedParity {
            blocks: std::mem::take(&mut self.owed).into_iter().collect(),
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

impl TreeStore {
    /// The read, which for this store is local and cannot need a round trip.
    ///
    /// The trait's `get` wraps this. `delete` calls it directly, so that a
    /// `Result` no caller of this store can ever see `Err` on does not have to
    /// be unwrapped on a write path.
    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        match get(&self.blocks, &self.root, key) {
            Ok(v) => v.map(|v| self.materialise(v)),
            Err(e) => Self::impossible(e),
        }
    }
}

impl Store for TreeStore {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.apply_batch(&[(key.to_vec(), Edit::Put(value.to_vec()))]);
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        let existed = self.lookup(key).is_some();
        self.apply_batch(&[(key.to_vec(), Edit::Delete)]);
        existed
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
        // The parity this write CODED, kept as a debt rather than published.
        // `apply` reports only groups it coded: one whose parity was already
        // out is not re-reported, so this never grows by a block that is
        // already on the network.
        if self.track_owed {
            self.owed.extend(applied.parity);
            // Newest wins: this write may have re-coded a group whose parity
            // was still owed, and the superseded trio must never be put.
            if self.prune_superseded {
                self.prune_owed();
            }
        }
    }
}

impl Reads for TreeStore {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        Ok(self.lookup(key))
    }

    fn scan(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        if lo >= hi || limit == 0 {
            return Ok(Vec::new());
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
                return Ok(out);
            }
            req.after = next;
        }
    }

    fn root(&mut self) -> Read<[u8; 32]> {
        Ok(TreeStore::root(self))
    }

    /// The in-memory tree keeps every block it has ever written, so it CAN
    /// diff two of its own roots — and does, through the same library call
    /// the engine uses. A binding's delta path is therefore exercised on this
    /// backend too, rather than only against a node.
    fn changes_since(
        &mut self,
        from: [u8; 32],
        lo: &[u8],
        hi: &[u8],
        max_entries: u32,
    ) -> Read<Delta> {
        let now = TreeStore::root(self);
        let r = Range {
            lo: Bound::Included(lo.to_vec()),
            hi: Bound::Excluded(hi.to_vec()),
            reverse: false,
            after: None,
            max_entries: max_entries.max(1) as usize,
            max_bytes: usize::MAX,
        };
        match freenet_prolly::diff::diff(&self.blocks, &from, &now, &r, None) {
            Ok(page) if page.need.is_empty() => {
                let cursor = page.next.as_ref().map(|n| n.after.clone());
                let changes = page
                    .changes
                    .into_iter()
                    .map(|c| match c {
                        freenet_prolly::diff::Change::Added { key, new }
                        | freenet_prolly::diff::Change::Changed { key, new, .. } => {
                            (key, Some(self.materialise(new)))
                        }
                        freenet_prolly::diff::Change::Removed { key, .. } => (key, None),
                    })
                    .collect();
                Ok(Delta::Changes {
                    changes,
                    cursor,
                    new_root: now,
                })
            }
            // A root this store never held, or one whose blocks it cannot
            // reach. The defined answer, not an error.
            _ => Ok(Delta::FullReloadRequired { new_root: now }),
        }
    }
}
