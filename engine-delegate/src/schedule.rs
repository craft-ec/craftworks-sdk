//! When the engine's effects are allowed to leave, and how many at a time.
//!
//! The core emits effects carrying DEPENDENCIES (`after`), never batch sizes:
//! "this pack may be put once these blocks are on the node" is a fact about
//! the format, while "at most four GETs per return" is a fact about the
//! platform, and only one of them changes when Freenet does. So the core
//! states the first and this states the second.
//!
//! Three rules, each with a test:
//!
//! 1. An effect whose `after` set is not fully confirmed is HELD, and is
//!    released exactly when its last dependency confirms.
//! 2. A return carries at most `max_gets` fetches and `max_puts` puts. What
//!    does not fit waits for the next entry — it is not dropped.
//! 3. Nothing is issued twice, however often it is offered.

use engine::read::Via;
use engine::{Effect, Epoch};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

/// What one `process()` return is allowed to carry.
///
/// Defaults are the live limits as measured (F15, F21), and are parameters
/// because they are the node's numbers and not the engine's: a node that
/// raises them should not need a change here, only a different value.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_gets: usize,
    pub max_puts: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_gets: 4,
            max_puts: 8,
        }
    }
}

/// An effect the shell is ready to turn into a node operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Get { id: Cid, via: Via, attempt: u32 },
    Put { id: Cid, bytes: Vec<u8> },
    Head { seq: u64, root: Cid },
    ReadHead { epoch: Epoch },
}

impl Op {
    /// The id this op is ABOUT, which is what a confirmation names.
    fn id(&self) -> Option<Cid> {
        match self {
            Op::Get { id, .. } | Op::Put { id, .. } => Some(*id),
            // A head bump is named by its seq, not by a block id, and a
            // ReadHead is not about a block at all.
            Op::Head { .. } | Op::ReadHead { .. } => None,
        }
    }

    fn is_get(&self) -> bool {
        matches!(self, Op::Get { .. })
    }
}

/// Effects waiting for their dependencies, and for room in a return.
#[derive(Default)]
pub struct Scheduler {
    /// Ready to go, in the order the core emitted them.
    ready: Vec<Op>,
    /// Waiting on blocks that are not on the node yet.
    held: Vec<(BTreeSet<Cid>, Op)>,
    /// Blocks the node has confirmed it holds.
    confirmed: BTreeSet<Cid>,
    /// Ops already handed out. A re-offer of one of these is dropped.
    ///
    /// Keyed by id, with the ATTEMPT for a get: the core re-issues a fetch
    /// deliberately when one did not answer, and that is a different op, not
    /// the same one twice.
    issued_puts: BTreeSet<Cid>,
    issued_gets: BTreeMap<Cid, u32>,
    /// Head bumps already handed out, by seq.
    issued_heads: BTreeSet<u64>,
    /// Counted, never panicked on: what the core emitted that is not a node
    /// operation at all (a reply to a client, a progress note).
    pub not_an_op: usize,
}

impl Scheduler {
    /// Take what the core emitted this entry.
    pub fn offer(&mut self, effects: Vec<Effect>) {
        for f in effects {
            let (deps, op) = match f {
                Effect::FetchBlock { id, via, attempt } => {
                    (BTreeSet::new(), Op::Get { id, via, attempt })
                }
                Effect::PutPack {
                    id, bytes, after, ..
                }
                | Effect::PutBlock {
                    id, bytes, after, ..
                }
                | Effect::PutParity {
                    id, bytes, after, ..
                } => (after.into_iter().collect(), Op::Put { id, bytes }),
                Effect::UpdateHead { seq, root, after } => {
                    (after.into_iter().collect(), Op::Head { seq, root })
                }
                Effect::ReadHead { epoch } => (BTreeSet::new(), Op::ReadHead { epoch }),
                // Notify, Reply, Progress: for the client, not the node.
                _ => {
                    self.not_an_op += 1;
                    continue;
                }
            };
            self.admit(deps, op);
        }
    }

    fn admit(&mut self, deps: BTreeSet<Cid>, op: Op) {
        if self.already_issued(&op) {
            return;
        }
        let outstanding: BTreeSet<Cid> = deps.difference(&self.confirmed).copied().collect();
        if outstanding.is_empty() {
            self.ready.push(op);
        } else {
            self.held.push((outstanding, op));
        }
    }

    fn already_issued(&self, op: &Op) -> bool {
        match op {
            Op::Put { id, .. } => self.issued_puts.contains(id),
            // A LATER attempt is a new op; the same attempt is not.
            Op::Get { id, attempt, .. } => self.issued_gets.get(id).is_some_and(|a| a >= attempt),
            Op::Head { seq, .. } => self.issued_heads.contains(seq),
            Op::ReadHead { .. } => false,
        }
    }

    /// The node holds this block now.
    ///
    /// Releases anything that was waiting only on it. A dependency confirmed
    /// BEFORE the effect that names it arrives is remembered, so the order of
    /// the two does not matter — which it otherwise would, on a node that
    /// answers a put while the next effect is still being built.
    pub fn confirm(&mut self, id: Cid) {
        self.confirmed.insert(id);
        let mut still = Vec::new();
        for (mut deps, op) in std::mem::take(&mut self.held) {
            deps.remove(&id);
            if deps.is_empty() {
                self.ready.push(op);
            } else {
                still.push((deps, op));
            }
        }
        self.held = still;
    }

    /// What this return may carry, marking it issued.
    ///
    /// Order is preserved: the core emitted these in an order that is
    /// meaningful for reads (the first fetch of a descent is the one that
    /// unblocks it), and a scheduler that reordered them would turn a
    /// three-round descent into a three-entry one.
    pub fn take(&mut self, limits: Limits) -> Vec<Op> {
        let (mut gets, mut puts) = (0usize, 0usize);
        let mut out = Vec::new();
        let mut keep = Vec::new();
        for op in std::mem::take(&mut self.ready) {
            let fits = if op.is_get() {
                gets < limits.max_gets
            } else if matches!(op, Op::Put { .. }) {
                puts < limits.max_puts
            } else {
                true
            };
            if !fits {
                // Waits for the next entry. NOT dropped: the core will not
                // emit it again, so losing it here loses the write.
                keep.push(op);
                continue;
            }
            match &op {
                Op::Get { id, attempt, .. } => {
                    gets += 1;
                    self.issued_gets.insert(*id, *attempt);
                }
                Op::Put { id, .. } => {
                    puts += 1;
                    self.issued_puts.insert(*id);
                }
                Op::Head { seq, .. } => {
                    self.issued_heads.insert(*seq);
                }
                Op::ReadHead { .. } => {}
            }
            let _ = op.id();
            out.push(op);
        }
        self.ready = keep;
        out
    }

    /// Ops waiting for room in a return.
    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// Ops waiting for a dependency.
    pub fn held_len(&self) -> usize {
        self.held.len()
    }
}
