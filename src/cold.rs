//! COLD READS IN THE PAGE, with a short timeout and a re-fetch.
//!
//! A range this node does not hold is read by the PAGE itself, block by block,
//! through client-API GETs — not by the engine in the delegate, whose cold
//! read can sit in F52's stall for ≈ 60 s, parking the delegate so every other
//! request queues behind it. At most [`COLD_IN_FLIGHT`] GETs are in flight
//! (the rest wait in the page), so each GET's clock
//! ([`COLD_GET_TIMEOUT_MS`], from when it is SENT) measures that GET and not
//! the node's queue. EVERYTHING IS PER FETCH (the owner): a GET that times out
//! is asked again as a FRESH GET — ONLY that GET; the others in flight are
//! untouched — client GETs are not deduplicated, each is its own transaction
//! (F55, read; a re-GET beating a stall is what the live test measures). There
//! is no try count: each BLOCK has its own deadline ([`COLD_FETCH_DEADLINE_MS`],
//! from its FIRST send). A read is "still loading" while any of its blocks is
//! being retried, and "the node is not answering" only when one of its blocks
//! ran out its own deadline — never "missing". Every timeout, and whether its
//! re-GET returned, is LOGGED ([`ColdEvent`]).
//!
//! Parameters settled with the owner (it proposed 1 s × 3 tries; adjusted by
//! what was measured).
//!
//! Sans-IO: this says which blocks to GET and when; the page frames and sends
//! them (`wire::frame_get`) and hands back what arrives. Every block is
//! VERIFIED — its id is the hash of its kind and bytes — before it is kept.
//!
//! USED ONLY FOR DATA NOT ON THIS NODE (F55): a page GET of a block this
//! node's own DELEGATE wrote is answered NotFound on a node with a peer, so a
//! root the node answers "not found" for is marked LOCAL and its loads go
//! back to the engine, which reads its own store synchronously and never
//! stalls on it.

use freenet_prolly::range::{range, read_value, Range, RangeError};
use freenet_prolly::store::{Blocks, ReadError};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// GETs in flight at once; the rest wait in the page.
///
/// So a GET's clock measures THAT GET: 70 pipelined GETs finished only after
/// ≈ 8 s under load (measured), so a short per-GET clock on an uncapped batch
/// cuts healthy fetches that were merely queued at the node, and each
/// re-fetch adds load. At 8, a GET waits behind at most 7 others.
pub const COLD_IN_FLIGHT: usize = 8;

/// How long one cold GET is given, from when it is SENT, before it is asked
/// AGAIN as a fresh GET.
///
/// 2 s: an unstalled cold GET, one at a time, answers well under 1 s; F52's
/// stall heals only ≈ 60 s after the lost response was sent. With at most
/// [`COLD_IN_FLIGHT`] in flight, 2 s is past every healthy answer.
pub const COLD_GET_TIMEOUT_MS: u64 = 2_000;

/// How long one BLOCK's fetch — its first GET and every re-fetch — may take,
/// from its FIRST send, before the reads needing it say "the node is not
/// answering". Until then they are "still loading"; blocks that arrived are
/// kept, verified, for every read after.
///
/// 30 s: half of F52's 60 s — a fetch outlasting it is waiting on a node, not
/// on an answer a re-fetch can beat.
pub const COLD_FETCH_DEADLINE_MS: u64 = 30_000;

/// Rows one cold walk gathers per pass of the tree before it resumes after the
/// last key — the range is read WHOLE before the load completes.
const WALK_PAGE: usize = 4096;

/// An instruction to the page: GET this block, as a fresh transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Get {
    pub block: Cid,
    /// 1 for the first GET, 2.. for re-fetches.
    pub attempt: u32,
}

/// What happened to a cold GET — the log main asked for: every timeout, and
/// whether the re-GET returned.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ColdEvent {
    /// A GET went unanswered for its timeout; `attempt` is the one that timed out.
    TimedOut { block: Cid, attempt: u32, after_ms: u64 },
    /// A block arrived on a RE-fetch: `attempt` ≥ 2, `after_ms` since the FIRST GET.
    ReGetAnswered { block: Cid, attempt: u32, after_ms: u64 },
    /// The block arrived on its first GET.
    Answered { block: Cid, after_ms: u64 },
    /// A block's fetch ran out its own deadline: the reads needing it say
    /// "the node is not answering".
    NotAnswering { block: Cid, after_ms: u64 },
    /// An answer whose bytes do not hash to the block asked for: dropped, and
    /// the block asked for again.
    Rejected { block: Cid },
    /// The node would not serve the ROOT to the page: this node's own data
    /// (F55) — its loads went back to the engine.
    Local { root: Cid },
}

#[derive(Debug)]
struct Fetch {
    attempt: u32,
    sent_at: u64,
    first_at: u64,
}

#[derive(Debug)]
struct Load {
    lo: Vec<u8>,
    hi: Vec<u8>,
    root: Cid,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    after: Option<Vec<u8>>,
    /// What it is waiting on right now.
    needs: Vec<Cid>,
}

/// One page's cold reads.
#[derive(Debug, Default)]
pub struct ColdReads {
    /// The builder's flag: off, every load goes to the engine as before.
    pub on: bool,
    /// The root this page reads at (the head it knows).
    root: Option<Cid>,
    /// Roots this node's own delegate wrote (F55): the engine reads them.
    local: BTreeSet<Cid>,
    /// Blocks held, each verified against its id.
    blocks: BTreeMap<Cid, Vec<u8>>,
    /// In flight: at most [`COLD_IN_FLIGHT`].
    fetching: BTreeMap<Cid, Fetch>,
    /// Wanted, waiting for a place in flight, in the order wanted.
    queued: std::collections::VecDeque<Cid>,
    loads: BTreeMap<u64, Load>,
    gets: Vec<Get>,
    done: Vec<(u64, Vec<(Vec<u8>, Vec<u8>)>, Cid)>,
    returned: Vec<(u64, Vec<u8>, Vec<u8>)>,
    not_answering: Vec<u64>,
    pub log: Vec<ColdEvent>,
}

/// The held blocks, as the tree reads them.
struct Held<'a>(&'a BTreeMap<Cid, Vec<u8>>);

impl Blocks for Held<'_> {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.get(cid).map(Vec::as_slice)
    }
}

/// What one pass of the walk found.
enum Pass {
    /// These rows, and resume after this key (or the range is finished).
    Rows(Vec<(Vec<u8>, Vec<u8>)>, Option<Vec<u8>>),
    /// These blocks are needed first.
    Need(Vec<Cid>),
    /// The tree is not what it says (a corrupt block, a child that does not
    /// match its parent): not something a fetch fixes.
    Broken,
}

impl ColdReads {
    /// The root this page reads at — from the head it learned (Identity, a page
    /// answered at a root). Loads already under way keep the root they began at.
    pub fn set_root(&mut self, root: Cid) {
        self.root = Some(root);
    }

    /// Take a range load, or leave it to the engine (`false`): taken only when
    /// the flag is on, the root is known, and the node has not said the root
    /// is its own.
    pub fn take(&mut self, id: u64, lo: &[u8], hi: &[u8], now_ms: u64) -> bool {
        let Some(root) = self.root.filter(|r| self.on && !self.local.contains(r)) else {
            return false;
        };
        self.loads.insert(id, Load { lo: lo.to_vec(), hi: hi.to_vec(), root, rows: Vec::new(), after: None, needs: Vec::new() });
        self.drive(id);
        self.fill(now_ms);
        true
    }

    /// A GET answered for `block` (the page matched the contract the node named
    /// to the block it asked for). Kept only if the bytes hash to `block`;
    /// otherwise dropped and asked for again.
    pub fn arrived(&mut self, block: Cid, state: &[u8], now_ms: u64) {
        let Some(f) = self.fetching.remove(&block) else {
            return; // not asked for, or already here: nothing to do
        };
        let verified = state.split_first().is_some_and(|(&k, body)| freenet_prolly::block_id(k, body) == block);
        if !verified {
            self.log.push(ColdEvent::Rejected { block });
            self.send(block, f.attempt + 1, f.first_at, now_ms);
            return;
        }
        let after_ms = now_ms.saturating_sub(f.first_at);
        self.log.push(if f.attempt > 1 {
            ColdEvent::ReGetAnswered { block, attempt: f.attempt, after_ms }
        } else {
            ColdEvent::Answered { block, after_ms }
        });
        self.blocks.insert(block, state[1..].to_vec());
        let open: Vec<u64> = self.loads.keys().copied().collect();
        for id in open {
            self.drive(id);
        }
        self.fill(now_ms);
    }

    /// The node REFUSED a GET for `block`. For the ROOT — a block this node's
    /// own delegate wrote is refused to a page on a node with a peer (F55) —
    /// the root is LOCAL and its loads go back to the engine. Any other block:
    /// treated as a timeout, asked again.
    ///
    /// ASSUMPTION (the live run tells): a refusal for the root means "local",
    /// not "missing everywhere" — a root that exists nowhere is not one a head
    /// names.
    pub fn refused(&mut self, block: Cid, now_ms: u64) {
        let Some(f) = self.fetching.remove(&block) else { return };
        let is_root = self.loads.values().any(|l| l.root == block);
        if is_root {
            self.local.insert(block);
            self.log.push(ColdEvent::Local { root: block });
            let back: Vec<u64> = self.loads.iter().filter(|(_, l)| l.root == block).map(|(id, _)| *id).collect();
            for id in back {
                let l = self.loads.remove(&id).expect("listed");
                self.returned.push((id, l.lo, l.hi));
            }
            self.forget_unneeded();
            self.fill(now_ms);
            return;
        }
        self.send(block, f.attempt + 1, f.first_at, now_ms);
    }

    /// The page's clock, PER FETCH: each GET past its own timeout is asked
    /// AGAIN as a fresh GET — only that one; a block whose fetch ran out its
    /// own [`COLD_FETCH_DEADLINE_MS`] ends the reads needing it as "the node is
    /// not answering"; blocks no open read still needs stop being asked.
    pub fn tick(&mut self, now_ms: u64) {
        let late: Vec<Cid> = self
            .fetching
            .iter()
            .filter(|(_, f)| now_ms.saturating_sub(f.sent_at) >= COLD_GET_TIMEOUT_MS)
            .map(|(c, _)| *c)
            .collect();
        for block in late {
            let f = self.fetching.remove(&block).expect("listed");
            self.log.push(ColdEvent::TimedOut { block, attempt: f.attempt, after_ms: now_ms.saturating_sub(f.sent_at) });
            if now_ms.saturating_sub(f.first_at) >= COLD_FETCH_DEADLINE_MS {
                self.log.push(ColdEvent::NotAnswering { block, after_ms: now_ms.saturating_sub(f.first_at) });
                let ended: Vec<u64> = self.loads.iter().filter(|(_, l)| l.needs.contains(&block)).map(|(id, _)| *id).collect();
                for id in ended {
                    self.loads.remove(&id);
                    self.not_answering.push(id);
                }
                continue;
            }
            self.send(block, f.attempt + 1, f.first_at, now_ms);
        }
        self.forget_unneeded();
        self.fill(now_ms);
    }

    /// A GET goes out NOW (in flight), as attempt `attempt`.
    fn send(&mut self, block: Cid, attempt: u32, first_at: u64, now_ms: u64) {
        self.fetching.insert(block, Fetch { attempt, sent_at: now_ms, first_at });
        self.gets.push(Get { block, attempt });
    }

    /// Queued blocks take the places in flight that are free.
    fn fill(&mut self, now_ms: u64) {
        while self.fetching.len() < COLD_IN_FLIGHT {
            let Some(block) = self.queued.pop_front() else { break };
            if !self.blocks.contains_key(&block) && !self.fetching.contains_key(&block) {
                self.send(block, 1, now_ms, now_ms);
            }
        }
    }

    /// A block no open read is waiting on stops being asked for.
    fn forget_unneeded(&mut self) {
        let needed: BTreeSet<Cid> = self.loads.values().flat_map(|l| l.needs.iter().copied()).collect();
        self.fetching.retain(|b, _| needed.contains(b));
        self.queued.retain(|b| needed.contains(b));
    }

    fn want(&mut self, block: Cid) {
        if self.blocks.contains_key(&block) || self.fetching.contains_key(&block) || self.queued.contains(&block) {
            return;
        }
        self.queued.push_back(block);
    }

    /// Walk load `id` as far as the held blocks allow: finish it, or ask for
    /// what it needs next.
    fn drive(&mut self, id: u64) {
        loop {
            let Some(l) = self.loads.get(&id) else { return };
            let pass = Self::pass(&self.blocks, l);
            match pass {
                Pass::Need(blocks) => {
                    for b in &blocks {
                        self.want(*b);
                    }
                    self.loads.get_mut(&id).expect("present").needs = blocks;
                    return;
                }
                Pass::Broken => {
                    // Not a fetch's to fix: the engine reads it as before.
                    let l = self.loads.remove(&id).expect("present");
                    self.returned.push((id, l.lo, l.hi));
                    return;
                }
                Pass::Rows(rows, next) => {
                    let l = self.loads.get_mut(&id).expect("present");
                    l.rows.extend(rows);
                    match next {
                        Some(after) => l.after = Some(after),
                        None => {
                            let l = self.loads.remove(&id).expect("present");
                            self.done.push((id, l.rows, l.root));
                            return;
                        }
                    }
                }
            }
        }
    }

    /// One pass of prolly's own `range` over the held blocks.
    fn pass(blocks: &BTreeMap<Cid, Vec<u8>>, l: &Load) -> Pass {
        let held = Held(blocks);
        let r = Range {
            lo: Bound::Included(l.lo.clone()),
            hi: Bound::Excluded(l.hi.clone()),
            reverse: false,
            after: l.after.clone(),
            max_entries: WALK_PAGE,
            max_bytes: usize::MAX,
        };
        let page = match range(&held, &l.root, &r) {
            Ok(p) => p,
            Err(RangeError::Read(ReadError::Need(need))) => return Pass::Need(need),
            Err(_) => return Pass::Broken,
        };
        if !page.need.is_empty() {
            return Pass::Need(page.need.clone());
        }
        let mut rows = Vec::with_capacity(page.entries.len());
        let mut need = Vec::new();
        for (k, v) in &page.entries {
            match read_value(&held, *v) {
                Ok(bytes) => rows.push((k.clone(), bytes.to_vec())),
                Err(ReadError::Need(n)) => need.extend(n),
                Err(_) => return Pass::Broken,
            }
        }
        if !need.is_empty() {
            return Pass::Need(need);
        }
        let next = if page.finished() { None } else { page.next.clone() };
        Pass::Rows(rows, next)
    }

    /// GETs to send, in order.
    pub fn take_gets(&mut self) -> Vec<Get> {
        std::mem::take(&mut self.gets)
    }

    /// Loads that finished: (ticket, every row of the range, the root read at).
    pub fn take_done(&mut self) -> Vec<(u64, Vec<(Vec<u8>, Vec<u8>)>, Cid)> {
        std::mem::take(&mut self.done)
    }

    /// Loads a block of which ran out its deadline: "the node is not answering".
    pub fn take_not_answering(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.not_answering)
    }

    /// Loads handed back to the engine: (ticket, lo, hi) — to be sent as today.
    pub fn take_returned(&mut self) -> Vec<(u64, Vec<u8>, Vec<u8>)> {
        std::mem::take(&mut self.returned)
    }

    /// Blocks being fetched right now — the page matches an answer's contract
    /// to one of these.
    pub fn fetching(&self) -> impl Iterator<Item = &Cid> {
        self.fetching.keys()
    }

    /// Loads under way.
    pub fn open(&self) -> usize {
        self.loads.len()
    }
}
