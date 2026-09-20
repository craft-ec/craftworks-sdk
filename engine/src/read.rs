//! The read path as a pure state machine.
//!
//! Same shape as the write path: `step(event) -> Vec<Effect>`, no sockets, no
//! clock. A cold read of someone else's key is a head and then one block per
//! level, and every one of those is a round-trip a delegate is re-entered for
//! — so the core parks a continuation and resumes it, rather than blocking.
//!
//! **The descent is not reimplemented here.** `freenet_prolly::read::get` and
//! `range::range` already walk the tree and report exactly which blocks they
//! were missing (`ReadError::Need`, `Page::need`). Writing a second walk in
//! the engine would be a second answer to "which child holds this key", and
//! the one way a reader and the library can come to disagree about what a tree
//! contains. So the core's whole job is: try, park on what came back, fetch,
//! try again.

use crate::{ClientId, Params};
use freenet_prolly::node::Value;
use freenet_prolly::range::{range_with, Options as RangeOptions, PageEnd, Range, RangeError};
use freenet_prolly::read::get;
use freenet_prolly::store::{Blocks, ReadError};
use freenet_prolly::{block_id, kind, Cid};
use std::collections::{BTreeMap, BTreeSet};

/// A client's own id for a read, echoed in its reply.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ReqId(pub u64);

/// How a block is being fetched.
///
/// A pack is tried first for a fresh commit: one fetch can carry every block
/// of a level, and the blocks inside are the same blocks with the same ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Direct,
    Pack(Cid),
}

/// What a read came back with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadResult {
    /// The value, or its absence. Absence is an answer.
    Value(Option<Vec<u8>>),
    Page {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        /// Pass back as `after` to continue. `None` means the scan reached the
        /// end of the range, not that the page was full.
        cursor: Option<Vec<u8>>,
        complete: bool,
    },
    /// A block could not be had within the attempt budget. A REPLY, not a
    /// hang: a read that never answers is indistinguishable from a wedged
    /// node, and the caller can do nothing about either.
    Unavailable(Cid),
    /// This read needs to hold more at once than the warm bound allows.
    ///
    /// Distinct from `Unavailable`, which means the network would not answer.
    /// Here the network answered and the engine cannot keep what it was
    /// given — a caller can act on that (raise the bound, narrow the query),
    /// and could do nothing about the other. It is a reply either way: the
    /// alternative is evicting what the read needs and asking for it again
    /// for ever.
    OutOfWarmSpace,
}

/// What a client asked for, kept so the request can be retried as blocks land.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum Want {
    Get(Vec<u8>),
    Scan(Box<ScanSpec>),
}

/// A scan, in the engine's OWN representation.
///
/// `freenet_prolly::range::Range` is the library's type and is not encodable;
/// the context is a wire format that leaves this process, so what goes into it
/// has to be the engine's, converted at the boundary. Mirroring the fields
/// rather than wrapping keeps the conversion total — a field added to `Range`
/// is a compile error here, not a silently dropped bound.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScanSpec {
    pub lo: std::ops::Bound<Vec<u8>>,
    pub hi: std::ops::Bound<Vec<u8>>,
    pub reverse: bool,
    pub after: Option<Vec<u8>>,
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl From<&ScanSpec> for Range {
    fn from(s: &ScanSpec) -> Range {
        Range {
            lo: s.lo.clone(),
            hi: s.hi.clone(),
            reverse: s.reverse,
            after: s.after.clone(),
            max_entries: s.max_entries,
            max_bytes: s.max_bytes,
        }
    }
}

impl From<&Range> for ScanSpec {
    fn from(r: &Range) -> ScanSpec {
        ScanSpec {
            lo: r.lo.clone(),
            hi: r.hi.clone(),
            reverse: r.reverse,
            after: r.after.clone(),
            max_entries: r.max_entries,
            max_bytes: r.max_bytes,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Parked {
    pub client: ClientId,
    pub want: Want,
    pub root: Cid,
    pub levels_done: usize,
    /// Fetch rounds this read has made without finishing.
    ///
    /// The bound has to be PER REQUEST, not per block. A per-block attempt
    /// counter counts FAILURES, and when the node serves a block and then
    /// evicts it before the next call — which it may, because a sync read
    /// does not refresh hosting — every attempt SUCCEEDS and the counter
    /// never trips. The read then re-descends for ever, making progress on
    /// paper and none in fact.
    pub rounds: u32,
    /// Every block this read has been handed, PINNED until it replies.
    ///
    /// A read re-descends from the root on each resume, so it needs the whole
    /// path it has already paid for. Evicting any of it sends the read back
    /// for a block it just had — and because that fetch SUCCEEDS, the attempt
    /// budget never trips and nothing ever ends. A tight warm set turned a
    /// cold read into a livelock exactly that way.
    pub held: BTreeSet<Cid>,
}

/// The read side of the engine's state.
#[derive(Default)]
pub(crate) struct Reads {
    pub parked: BTreeMap<ReqId, Parked>,
    /// Who is waiting on each outstanding block. Two requests needing one
    /// block share its fetch: the second does not pay for the first's round
    /// trip, and a popular branch is fetched once however many readers want it.
    pub waiting: BTreeMap<Cid, BTreeSet<ReqId>>,
    /// Attempts already made per block, for the bounded re-issue.
    pub attempts: BTreeMap<Cid, u32>,
    /// Which pack is known to carry a block, newest first.
    pub in_pack: BTreeMap<Cid, Cid>,
    pub fetches: usize,
}

/// Does this block's content match the id it was fetched under?
///
/// Checked BEFORE it touches the warm tree, against both kinds it could
/// legitimately be. A block that fails this is not cached and not parsed: a
/// content-addressed store whose contents are not their addresses is a store
/// where one bad answer poisons every later read of the same key.
pub fn matches_id(id: &Cid, bytes: &[u8]) -> bool {
    block_id(kind::TREE_NODE, bytes) == *id
        || block_id(kind::RAW, bytes) == *id
        || block_id(crate::pack::PACK_KIND, bytes) == *id
}

impl Reads {
    /// Record who is waiting, and say whether a fetch must be emitted.
    ///
    /// Returns false when someone else is already waiting on this block — the
    /// caller then emits nothing and both requests resume on the one arrival.
    pub fn want(&mut self, id: Cid, req: ReqId, share: bool) -> bool {
        let waiters = self.waiting.entry(id).or_default();
        let first = waiters.is_empty();
        waiters.insert(req);
        first || !share
    }
}

/// Try a request against what is warm, and say what it still needs.
pub(crate) enum Attempt {
    Done(ReadResult),
    Need(Vec<Cid>),
    /// The tree is damaged in a way no fetch can fix.
    Broken(Cid),
}

pub(crate) fn attempt<B: Blocks>(
    blocks: &B,
    params: &Params,
    want: &Want,
    root: &Cid,
    nodes_parsed: &mut usize,
) -> Attempt {
    match want {
        Want::Get(key) => {
            if params.count_descent {
                count_nodes(blocks, root, key, nodes_parsed);
            }
            match get(blocks, root, key) {
                // A value stored by REFERENCE is a block of its own, and the
                // descent that found the leaf does not fetch it. Answering
                // with what is warm would return an EMPTY value for a key
                // that has one — a wrong answer, not a missing one, and
                // indistinguishable downstream from a value that really is
                // empty. So a missing value block is something to fetch, like
                // any other.
                Ok(Some(Value::Ref { cid, .. })) if blocks.get(&cid).is_none() => {
                    Attempt::Need(vec![cid])
                }
                Ok(v) => Attempt::Done(ReadResult::Value(v.map(|v| materialise(blocks, v)))),
                Err(ReadError::Need(ids)) => Attempt::Need(ids),
                Err(ReadError::Corrupt(cid, _)) | Err(ReadError::Mismatch(cid)) => {
                    Attempt::Broken(cid)
                }
            }
        }
        Want::Scan(spec) => {
            let opts = RangeOptions::default();
            let r: Range = spec.as_ref().into();
            match range_with(opts, blocks, root, &r) {
                Ok(page) => {
                    if !page.need.is_empty() && page.end == PageEnd::Blocked {
                        // Only the blocks it actually stopped on, bounded by
                        // the parameter: `need` can name the whole rest of the
                        // range, and asking for all of it is how one scan
                        // turns into an unbounded fan-out.
                        return Attempt::Need(
                            page.need
                                .into_iter()
                                .take(params.max_fetch_per_round)
                                .collect(),
                        );
                    }
                    // Same for a page: an entry whose value block is not warm
                    // would come back empty.
                    let missing: Vec<Cid> = page
                        .entries
                        .iter()
                        .filter_map(|(_, v)| match v {
                            Value::Ref { cid, .. } if blocks.get(cid).is_none() => Some(*cid),
                            _ => None,
                        })
                        .collect();
                    if !missing.is_empty() {
                        return Attempt::Need(
                            missing
                                .into_iter()
                                .take(params.max_fetch_per_round)
                                .collect(),
                        );
                    }
                    let entries = page
                        .entries
                        .into_iter()
                        .map(|(k, v)| (k, materialise(blocks, v)))
                        .collect();
                    Attempt::Done(ReadResult::Page {
                        entries,
                        cursor: page.next,
                        complete: matches!(page.end, PageEnd::EndOfRange | PageEnd::EndOfTree),
                    })
                }
                Err(RangeError::Read(ReadError::Need(ids))) => Attempt::Need(ids),
                Err(RangeError::Read(ReadError::Corrupt(cid, _)))
                | Err(RangeError::Read(ReadError::Mismatch(cid))) => Attempt::Broken(cid),
                // `max_entries == 0` is a caller mistake the SDK screens; here
                // it is an empty, complete page rather than a panic.
                Err(RangeError::NoLimit) => Attempt::Done(ReadResult::Page {
                    entries: Vec::new(),
                    cursor: None,
                    complete: true,
                }),
            }
        }
    }
}

/// The bytes of a value, wherever the format put it.
/// Only called once the value's block is known to be warm: the callers above
/// turn a missing one into a fetch, because defaulting it to empty would
/// answer a question wrongly rather than not answering it.
fn materialise<B: Blocks>(blocks: &B, v: Value<'_>) -> Vec<u8> {
    match v {
        Value::Inline(b) => b.to_vec(),
        Value::Ref { cid, .. } => blocks
            .get(&cid)
            .map(<[u8]>::to_vec)
            .expect("the caller fetched it first"),
    }
}

/// How many nodes a descent for `key` would touch, added to the cost counter.
///
/// A MEASUREMENT, not a second answer: it decides nothing and nothing reads
/// its result but the gate. The descent that answers the read is
/// `freenet_prolly::read::get`, and duplicating that decision here is exactly
/// what this module exists not to do — so this walks the same path only to
/// count it, and if it ever disagreed with the real descent the cost number
/// would be wrong while every answer stayed right.
fn count_nodes<B: Blocks>(blocks: &B, root: &Cid, key: &[u8], out: &mut usize) {
    let mut cur = *root;
    while let Some(bytes) = blocks.get(&cur) {
        let Ok(node) = freenet_prolly::node::Node::parse(bytes) else {
            return;
        };
        *out += 1;
        if node.is_leaf() {
            return;
        }
        let i = match node.search(key) {
            Ok(i) => i,
            Err(0) => return,
            Err(i) => i - 1,
        };
        cur = node.child(i).0;
    }
}
