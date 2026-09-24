//! What an operation SHOULD cost, derived from the tree alone.
//!
//! The GAP machinery (craftworks-instrument#12) compares an observed cost
//! against an expected one. Until now every expectation was HAND-WRITTEN in a
//! test, so the gap only worked where somebody already knew the answer — which
//! is not where defects live. This is the other half.
//!
//! # It runs the library's own descent
//!
//! The expectation is NOT a second implementation of "which child holds this
//! key". It calls [`freenet_prolly::read::get`], [`range`](freenet_prolly::range::range),
//! [`prove`](freenet_prolly::proof::prove) and [`diff`](freenet_prolly::diff::diff)
//! against a wrapper that records what they touch. Two reasons, both of which
//! have bitten before:
//!
//! - **"Path length = height" is wrong.** A lookup stops early when the key is
//!   below a node's minimum, including at the root, so an absent key costs less
//!   than the height and a formula would over-report it.
//! - **A private copy drifts exactly at a boundary** — for a key equal to a
//!   child's minimum. That is the same reason the parity writer calls the same
//!   `group_sizes` the host checks with.
//!
//! # It cannot fetch
//!
//! Not a promise — a type property. Everything here goes through
//! `Blocks::get(&self, &Cid) -> Option<&[u8]>`: synchronous, borrowing, with
//! nowhere to put a network round trip. A block that is not held comes back as
//! `need`, and is BOUNDED rather than acquired. A derivation that fetched would
//! be a measurement wearing a derivation's name.
//!
//! # A partial answer is an UPPER bound
//!
//! Anything not held is priced at the largest it could legally be, so a ratio
//! taken against it is a LOWER bound on waste. This can under-report and it can
//! never cry wolf: a detector that overstates is ignored within a week, and an
//! ignored detector is worse than none.
//!
//! # Three quantities, and bytes leads
//!
//! `bytes` is tight — a node's ACTUAL encoded length wherever it can be read.
//! `blocks` is only boundable in general, because a range's leaf count is not
//! derivable from any aggregate: `child_agg.count` is leaf ENTRIES, and
//! entries→nodes is decided by the content-defined boundaries, a function of
//! the KEYS. `rounds` exists because the missing quantity is not an operation
//! at all: per-operation cost and round trips dominate bytes on this platform
//! (F30), so blocks and bytes alone would call an eight-round operation fine.
//!
//! # What a key read's bytes do NOT include
//!
//! [`Op::Get`] is the cost of LOCATING a value. A value over `MAX_INLINE` lives
//! in its own block behind a reference, and a lookup returns the reference
//! without fetching it: a few KiB against as much as 256 KiB. [`Op::GetValue`]
//! is the one that pays for it.

use freenet_prolly::diff::diff;
use freenet_prolly::node::{Value, HEADER, MAX_NODE};
use freenet_prolly::proof::prove;
use freenet_prolly::range::{range, Range};
use freenet_prolly::read::get;
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Bound;

/// The head contract update a commit publishes, in bytes.
///
/// An UPDATE, not a create: 152 KiB is what CREATING the contract costs, and
/// charging it to every write overstated a commit by three orders of magnitude
/// (F38).
pub const HEAD_UPDATE: u64 = 112;

/// Bytes of one parity id, as the node format stores them.
const REF_LEN: u64 = 32;

/// Nodes one insert rewrites, 99th percentile, at the height it was measured.
///
/// **A point in a parameter space, not a constant**: measured at 20k entries,
/// height 3 (§5, 2000 trials). [`write_estimate`] takes the height so the
/// figure is never silently reused at another shape.
pub const WRITE_TAIL_P99_AT_HEIGHT_3: u64 = 11;
const MEASURED_AT_HEIGHT: u64 = 3;

/// How many blocks a caller fetches per round trip.
///
/// A property of the TRANSPORT, not of the tree, which is why it is a parameter
/// and not a constant derived here. This is only the default.
pub const DEFAULT_BATCH: u64 = 16;

/// How much of an expectation was read, and how much was taken on trust.
///
/// Ordered by strength: [`Basis::Exact`] is the strongest claim and
/// [`Basis::AtMost`] the weakest. Combining two expectations keeps the WEAKER,
/// because an answer is only as good as its softest part.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Basis {
    /// Every block was held and its real encoded length was read.
    Exact,
    /// Part of it rests on a parent's word about a child's aggregate, unchecked
    /// because the child was not loaded.
    ///
    /// Harmless in kind — a lying aggregate makes the EXPECTATION wrong, never
    /// the data wrong — but the report has to say so.
    Claimed,
    /// Part of it was not held at all and was priced at the largest it could
    /// legally be. Any ratio against it is a LOWER bound on waste.
    AtMost,
}

/// What an operation should touch.
///
/// The fields are deliberately close to `instrument::Expected` and this is
/// deliberately NOT that type: the instrument crate has no dependencies at all,
/// because the contract crates must prove it absent from their wasm closure
/// (freenet-contracts#40) — an edge from there to a tree would be an accidental
/// epoch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Expected {
    /// Encoded bytes. The tight one; lead a report with it.
    pub bytes: u64,
    /// Blocks touched. Exact where every block was read, bounded otherwise.
    pub blocks: u64,
    /// Round trips. Path length for a point read, `ceil(blocks / batch)` for a
    /// range.
    pub rounds: u64,
    pub basis: Basis,
}

impl Expected {
    pub const ZERO: Expected = Expected {
        bytes: 0,
        blocks: 0,
        rounds: 0,
        basis: Basis::Exact,
    };

    /// Add two expectations, keeping the weaker basis.
    pub fn and(self, o: Expected) -> Expected {
        Expected {
            bytes: self.bytes + o.bytes,
            blocks: self.blocks + o.blocks,
            rounds: self.rounds + o.rounds,
            basis: self.basis.max(o.basis),
        }
    }

    /// How a ratio against this expectation should be READ OUT.
    ///
    /// "at least 8.0x" when the denominator was rounded up, because the real
    /// ratio can then only be larger.
    pub fn phrase(self, ratio: f64) -> String {
        match self.basis {
            Basis::Exact => format!("{ratio:.1}x"),
            Basis::Claimed => format!("{ratio:.1}x (on the tree's own word)"),
            Basis::AtMost => format!("at least {ratio:.1}x"),
        }
    }
}

/// The operation to price.
#[derive(Clone, Copy, Debug)]
pub enum Op<'a> {
    /// LOCATE one key: the descent, not the value block. See the module docs.
    Get(&'a [u8]),
    /// Locate one key AND fetch its value, including a referenced block.
    GetValue(&'a [u8]),
    /// A range, and the round trips it takes at `batch` blocks a time.
    Scan {
        lo: Bound<&'a [u8]>,
        hi: Bound<&'a [u8]>,
        batch: u64,
    },
    /// A membership proof. Nearly free: a proof IS the root-to-leaf path, the
    /// same computation as [`Op::Get`].
    Proof(&'a [u8]),
    /// What a reader catching up from `from` must transfer.
    ///
    /// The cost model for every live binding. This is the GENERAL case and
    /// needs both trees warm; a writer that has just committed gets the same
    /// answer for free from [`commit`].
    ChangesSince { from: Cid, batch: u64 },
}

/// A store that records what the library asked it for.
///
/// It cannot add anything: `get` takes `&self`, so the only outcomes are a
/// block that was already held and a block that was not.
struct Watch<'b, B: Blocks> {
    inner: &'b B,
    seen: RefCell<HashMap<Cid, usize>>,
}

impl<'b, B: Blocks> Watch<'b, B> {
    fn new(inner: &'b B) -> Self {
        Watch {
            inner,
            seen: RefCell::new(HashMap::new()),
        }
    }

    /// Distinct blocks touched, and their real encoded size.
    fn totals(&self) -> (u64, u64) {
        let s = self.seen.borrow();
        (s.len() as u64, s.values().map(|n| *n as u64).sum())
    }
}

impl<B: Blocks> Blocks for Watch<'_, B> {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        let b = self.inner.get(cid)?;
        self.seen.borrow_mut().insert(*cid, b.len());
        Some(b)
    }
}

/// The largest a node can encode to, given what a PARENT says about it.
///
/// Four corrections, each of which makes a naive `child_agg.bytes` wrong in a
/// direction that matters:
///
/// - The aggregate is the fold of its ENTRIES. **Parity ids sit outside it**
///   (`entries_end = len - pcount * REF_LEN`), so up to 4 KiB per node that a
///   reader really transfers is missing from it. Omitting that understates the
///   expectation, which OVERSTATES the ratio — the one direction this whole
///   design exists to avoid. It is added here as `pcount * REF_LEN`.
/// - It omits the 28-byte header and the 6 bytes of index per entry.
/// - It counts every entry's key in FULL, though prefix compression stores the
///   shared prefix once — so it also overstates, in the other direction.
/// - It counts a `Ref` at its REFERENCED length, which can be 256 KiB where the
///   node stores 32 bytes.
///
/// Because those pull both ways, `child_agg.bytes` is neither the encoded size
/// nor a bound in either direction. This returns a genuine upper bound, capped
/// at [`MAX_NODE`] — a cap the format guarantees
/// (`MAX_LOGICAL + 4096 <= MAX_NODE`).
///
/// An expectation using this is [`Basis::Claimed`] at best: it rests on the
/// parent's word.
pub fn node_bytes_bound(count: u64, logical: u64, pcount: u64) -> u64 {
    let index_and_fixed = count * (6 + 7);
    let parity = pcount * REF_LEN;
    (HEADER as u64 + index_and_fixed + logical + parity).min(MAX_NODE as u64)
}

fn ceil_div(n: u64, d: u64) -> u64 {
    if d == 0 {
        n
    } else {
        n.div_ceil(d)
    }
}

/// What `op` should cost against the tree at `root`, from the data alone.
///
/// Reads only blocks that are already held.
pub fn expected<B: Blocks>(blocks: &B, root: &Cid, op: Op<'_>) -> Expected {
    let w = Watch::new(blocks);
    // `need` blocks are ones the library asked for and could not have.
    let (needed, extra) = match op {
        Op::Get(key) => (locate(&w, root, key), Expected::ZERO),
        Op::GetValue(key) => {
            let need = locate(&w, root, key);
            // The referenced length is recorded in the LEAF, so the value's
            // cost is exact without touching its block.
            let v = get(&w, root, key).ok().flatten();
            let extra = match v {
                Some(Value::Ref { len, .. }) => Expected {
                    bytes: len as u64,
                    blocks: 1,
                    rounds: 1,
                    basis: Basis::Exact,
                },
                _ => Expected::ZERO,
            };
            (need, extra)
        }
        Op::Proof(key) => {
            let need = match prove(&w, root, key) {
                Ok(_) => 0,
                Err(e) => needs(e),
            };
            (need, Expected::ZERO)
        }
        Op::Scan { lo, hi, batch } => {
            let r = Range {
                lo: bound_of(lo),
                hi: bound_of(hi),
                max_entries: usize::MAX,
                max_bytes: usize::MAX,
                ..Default::default()
            };
            let need = match range(&w, root, &r) {
                Ok(page) => page.need.len() as u64,
                Err(_) => 1,
            };
            let (blocks_n, bytes) = w.totals();
            return finish(bytes, blocks_n, ceil_div(blocks_n, batch), need).and(Expected::ZERO);
        }
        Op::ChangesSince { from, batch } => {
            let r = Range {
                max_entries: usize::MAX,
                max_bytes: usize::MAX,
                ..Default::default()
            };
            let need = match diff(&w, &from, root, &r, None) {
                Ok(page) => page.need.len() as u64,
                Err(_) => 1,
            };
            let (blocks_n, bytes) = w.totals();
            return finish(bytes, blocks_n, ceil_div(blocks_n, batch), need);
        }
    };
    let (blocks_n, bytes) = w.totals();
    // A point read's round trips ARE its path: one hop per node, because the
    // next id is only known once the current node is in hand.
    finish(bytes, blocks_n, blocks_n, needed).and(extra)
}

/// Run the library's own lookup and report how many blocks it could not have.
fn locate<B: Blocks>(w: &Watch<'_, B>, root: &Cid, key: &[u8]) -> u64 {
    match get(w, root, key) {
        Ok(_) => 0,
        Err(freenet_prolly::store::ReadError::Need(cids)) => cids.len() as u64,
        Err(_) => 1,
    }
}

fn needs(e: freenet_prolly::store::ReadError) -> u64 {
    match e {
        freenet_prolly::store::ReadError::Need(cids) => cids.len() as u64,
        _ => 1,
    }
}

/// Fold in whatever could not be read, at the largest it could legally be.
fn finish(bytes: u64, blocks: u64, rounds: u64, needed: u64) -> Expected {
    if needed == 0 {
        return Expected {
            bytes,
            blocks,
            rounds,
            basis: Basis::Exact,
        };
    }
    Expected {
        bytes: bytes + needed * MAX_NODE as u64,
        blocks: blocks + needed,
        // Each unheld block is at least one more hop.
        rounds: rounds + needed,
        basis: Basis::AtMost,
    }
}

fn bound_of(b: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match b {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
    }
}

/// What a commit cost, from the writer's own `apply` — free and EXACT.
///
/// The general two-root derivation needs both trees warm. A writer needs
/// nothing: `apply` already hands back the nodes it emitted and the parity it
/// coded, so the writer can derive a reader's catch-up cost from its own commit
/// without a second walk.
///
/// `emitted` is the sink's `(cid, len)` pairs; `parity` is
/// `Applied::parity`'s blocks.
pub fn commit(emitted: &[(Cid, usize)], parity: &[(Cid, Vec<u8>)]) -> Expected {
    let node_bytes: u64 = emitted.iter().map(|(_, n)| *n as u64).sum();
    let parity_bytes: u64 = parity.iter().map(|(_, b)| b.len() as u64).sum();
    Expected {
        bytes: node_bytes + parity_bytes + HEAD_UPDATE,
        blocks: emitted.len() as u64 + parity.len() as u64 + 1,
        // Data nodes, then the head, then parity (§7).
        rounds: 3,
        basis: Basis::Exact,
    }
}

/// What a write that has NOT happened yet should cost.
///
/// Separate from [`expected`] on purpose: a commit that exists is derived
/// exactly, and only a write still to come needs the distribution. The p99 is
/// used rather than the mean, because the mean would make a legitimate p99
/// write report 3x waste — and this must never cry wolf. Always
/// [`Basis::AtMost`]: it is a bound, not a reading.
///
/// The measured figure is a point in a parameter space. Away from the height it
/// was measured at, the tail is scaled by height rather than reused, and the
/// basis keeps saying `AtMost`.
///
/// For a RUN of writes, compare DISTRIBUTIONS rather than points: observed
/// p50/p99 against expected p50/p99 is both more sensitive and less prone to
/// false alarms than any single-write rule.
pub fn write_estimate(height: u64) -> Expected {
    let scaled = WRITE_TAIL_P99_AT_HEIGHT_3 * height.max(1) / MEASURED_AT_HEIGHT;
    let nodes = scaled.max(height);
    Expected {
        bytes: nodes * MAX_NODE as u64 + HEAD_UPDATE,
        blocks: nodes + 1,
        rounds: 3,
        basis: Basis::AtMost,
    }
}
