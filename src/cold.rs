//! COLD READS IN THE PAGE, with a short timeout and a re-fetch.
//!
//! A range this node does not hold is read by the PAGE itself, block by block,
//! through client-API GETs — not by the engine in the delegate, whose cold
//! read can sit in F52's stall for ≈ 60 s, parking the delegate so every other
//! request queues behind it.
//!
//! NOTHING IS FIXED (the owner): both the timeout and the concurrency move
//! with what the network does — TCP's answers, which are prior art for
//! exactly this:
//! * the TIMEOUT is RFC 6298's RTO over completed fetches (`SRTT`, `RTTVAR`,
//!   `RTO = SRTT + 4·RTTVAR`, a [`COLD_RTO_FLOOR_MS`] floor, [`COLD_RTO_INITIAL_MS`]
//!   before any sample), with KARN'S RULE — a fetch that was re-sent is never
//!   sampled, which copy answered is unknowable — and §5.5's BACK-OFF: a
//!   timeout doubles the RTO until a fetch answers on its first send;
//! * the CONCURRENCY is a congestion window — [`COLD_WINDOW_INITIAL`] to start,
//!   +1 per answer below the slow-start threshold (doubling per round), +1 per
//!   window of answers above it, halved (≥ 1) on a timeout with the threshold
//!   set there — so queueing inside the node shows as rising samples, a wider
//!   RTO and a window that stops growing: the loop finds the limit.
//!
//! EVERYTHING IS PER FETCH (the owner): a GET that times out
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

/// The RTO before any fetch has answered (RFC 6298 §2.1 says 1 s).
pub const COLD_RTO_INITIAL_MS: f64 = 1_000.0;

/// The RTO never falls below this: a loopback node answers in a few ms, and a
/// timeout that close to the sample re-sends on scheduling jitter alone.
pub const COLD_RTO_FLOOR_MS: f64 = 100.0;

/// The congestion window a page starts with, in fetches.
pub const COLD_WINDOW_INITIAL: f64 = 4.0;

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

/// The rows of a range, key and value.
pub type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// A load the page finished: its ticket, every row of the range, the root read at.
pub type Finished = (u64, Rows, Cid);

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
    /// In flight: at most the congestion window.
    fetching: BTreeMap<Cid, Fetch>,
    /// Wanted, waiting for a place in flight, in the order wanted.
    queued: std::collections::VecDeque<Cid>,
    loads: BTreeMap<u64, Load>,
    gets: Vec<Get>,
    done: Vec<Finished>,
    returned: Vec<(u64, Vec<u8>, Vec<u8>)>,
    not_answering: Vec<u64>,
    pub log: Vec<ColdEvent>,
    /// RFC 6298's smoothed round-trip time and its variation, over completed
    /// fetches that were never re-sent (Karn). `None` before the first sample.
    srtt: Option<f64>,
    rttvar: f64,
    /// The congestion window, and the slow-start threshold (`None`: none yet).
    window: f64,
    ssthresh: Option<f64>,
    /// RFC 6298 §5.5's back-off: doublings of the RTO since the last clean
    /// sample. Without it, Karn's rule leaves the RTO at a floor learned from
    /// fast fetches while every slower one is re-sent — and so never sampled —
    /// for ever (the first live run: RTO 100 ms, 142 timeouts, 72 blocks
    /// answered only on a re-send, of fetches that take ≈ 1.5 s).
    backoff: u32,
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
    /// A cold reader with the flag ON.
    pub fn switched_on() -> ColdReads {
        ColdReads { on: true, window: COLD_WINDOW_INITIAL, ..ColdReads::default() }
    }

    /// The retransmission timeout now, in ms (RFC 6298).
    pub fn rto_ms(&self) -> f64 {
        let base = match self.srtt {
            None => COLD_RTO_INITIAL_MS,
            Some(srtt) => (srtt + 4.0 * self.rttvar).max(COLD_RTO_FLOOR_MS),
        };
        base * f64::from(1u32 << self.backoff)
    }

    /// The smoothed round trip (RFC 6298's SRTT), `None` before any clean
    /// sample. Karn's rule is what keeps it honest: a re-sent fetch never
    /// feeds it, whatever the back-off does to the RTO meanwhile.
    pub fn srtt_ms(&self) -> Option<f64> {
        self.srtt
    }

    /// The congestion window now: how many fetches may be in flight.
    pub fn window(&self) -> f64 {
        self.window.max(1.0)
    }

    /// A fetch answered on its FIRST send: its round trip is a sample (RFC
    /// 6298 §2.2–2.3), and the window opens.
    fn sample(&mut self, r: f64) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
            }
            Some(srtt) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (srtt - r).abs();
                self.srtt = Some(0.875 * srtt + 0.125 * r);
            }
        }
    }

    /// An answer opens the window: +1 below the slow-start threshold, +1 per
    /// window of answers above it.
    fn opened(&mut self) {
        let w = self.window();
        self.window = if self.ssthresh.is_some_and(|t| w >= t) { w + 1.0 / w } else { w + 1.0 };
    }

    /// A timeout halves the window (≥ 1) and sets the threshold there. Once per
    /// tick however many fetches timed out in it — TCP halves once per loss
    /// EVENT, not per lost segment.
    fn halved(&mut self) {
        let half = (self.window() / 2.0).max(1.0);
        self.window = half;
        self.ssthresh = Some(half);
    }

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
            // Dropped, and asked again when its OWN clock says — not now. A
            // node that answers wrong bytes fast would otherwise be asked again
            // at the speed of its answers, each re-send re-arming the clock,
            // so the block would never time out and never reach its deadline.
            self.log.push(ColdEvent::Rejected { block });
            self.fetching.insert(block, f);
            return;
        }
        let after_ms = now_ms.saturating_sub(f.first_at);
        // KARN'S RULE: a re-sent fetch is never a sample — the answer may be
        // the first copy's, late, or the second's; nothing says which.
        if f.attempt == 1 {
            self.sample(now_ms.saturating_sub(f.sent_at) as f64);
            self.backoff = 0;
        }
        self.opened();
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
    /// the root is LOCAL and its loads go back to the engine. Any other block
    /// stays in flight on its own clock: [`ColdReads::tick`] asks it again at
    /// its RTO, as a timeout, and its deadline still runs.
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
        // Not re-sent NOW: that would re-arm its clock on every refusal, and a
        // node that refuses fast would be asked as fast, for ever.
        self.fetching.insert(block, f);
    }

    /// The page's clock, PER FETCH: each GET past its own timeout is asked
    /// AGAIN as a fresh GET — only that one; a block whose fetch ran out its
    /// own [`COLD_FETCH_DEADLINE_MS`] ends the reads needing it as "the node is
    /// not answering"; blocks no open read still needs stop being asked.
    pub fn tick(&mut self, now_ms: u64) {
        let late: Vec<Cid> = self
            .fetching
            .iter()
            .filter(|(_, f)| now_ms.saturating_sub(f.sent_at) as f64 >= self.rto_ms())
            .map(|(c, _)| *c)
            .collect();
        if !late.is_empty() {
            self.halved();
            // RFC 6298 §5.5: back the timer off — once per tick, however many
            // timed out in it — and keep it backed off until a clean sample.
            self.backoff = (self.backoff + 1).min(6);
        }
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
        while (self.fetching.len() as f64) < self.window().floor() {
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
    pub fn take_done(&mut self) -> Vec<Finished> {
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

    /// Is load `id` this reader's (taken, not yet finished, returned or ended)?
    pub fn holds(&self, id: u64) -> bool {
        self.loads.contains_key(&id)
    }

    /// Loads under way.
    pub fn open(&self) -> usize {
        self.loads.len()
    }
}
