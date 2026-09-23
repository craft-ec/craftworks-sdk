//! This client's OWN writes, until the engine has them (READ-STATE, design B).
//!
//! There are no rows here. Reads walk the tree from the engine's root over its
//! blocks (`crate::page_store`); what this keeps is the write path's own
//! bookkeeping: each write made and not yet settled, per key, in the order it
//! was made — so a verdict is a small edit to one term, a write falls WHOLE
//! with every write behind it, and the outbox re-sends in order.
//!
//! **Not an authority.** Nothing here is "saved" until the engine says
//! published.
//!
//! INTERIM (removed by R-b, READ-STATE § queue): a write the engine has NOT
//! taken — held by the window, or queued after `Busy` — is in no root yet, so
//! a read shows it from here ([`Copy::unaccepted`]). R-b moves the queue into
//! the engine, and this file with it.

use std::collections::BTreeMap;
use std::ops::Bound as B;

/// A write the client has made and the engine has not yet settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    pub write_id: u64,
    /// `None` is a DELETE. Not an empty value — a reader told an empty value
    /// keeps a key the writer removed.
    pub value: Option<Vec<u8>>,
    /// Queued but not yet submitted (the engine answered `Busy`).
    pub queued: bool,
    /// When this write last LEFT for the node, on the client's clock
    /// (restamped by [`Copy::sent`] on every send). What the timeout measures
    /// against.
    pub at_ms: u64,
    /// Made, and held by the outbox's window: the node has never seen it
    /// (craftworks-sdk#176). Never timed out — the timeout is for "no verdict
    /// ever came", and nothing was asked yet.
    pub held: bool,
    /// Sent, and no verdict yet: this write holds one of the outbox's window
    /// slots. THE record of the slot — the window is counted from these bits
    /// ([`Copy::at_node_count`]), so a write that is rolled back, published or
    /// answered takes its slot with it. A separate set of ids, cleared only by
    /// a verdict, kept the slots of timed-out writes for ever (sdk#179).
    pub at_node: bool,
    /// **Reserved, unused in phase 3.** The base version this write was
    /// computed against, for the phase-8 fix that lets a verdict invalidate
    /// only the writes that actually depended on the failed one. Present now
    /// so the shape does not have to change when it arrives.
    pub declared_base: Option<[u8; 32]>,
}

/// A write waiting to be sent again: its id, and all of its edits.
///
/// `None` for a value is a DELETE, as it is in [`PendingWrite`].
pub type QueuedWrite = (u64, Vec<(Vec<u8>, Option<Vec<u8>>)>);

/// What a key looks like to a component right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visible {
    /// No pending write: this is what the engine last said.
    Clean(Option<Vec<u8>>),
    /// A pending write that has not been submitted yet (the engine was busy).
    Queued(Option<Vec<u8>>, u64),
    /// A pending write the engine has accepted but not published.
    Pending(Option<Vec<u8>>, u64),
}

impl Visible {
    pub fn value(&self) -> Option<&[u8]> {
        match self {
            Visible::Clean(v) | Visible::Queued(v, _) | Visible::Pending(v, _) => v.as_deref(),
        }
    }
    /// Is anything here still unacknowledged? A UI invariant: a row showing an
    /// unacknowledged value must SAY so.
    pub fn is_pending(&self) -> bool {
        !matches!(self, Visible::Clean(_))
    }
}

/// Why a write was refused before it was applied anywhere.
///
/// Refused, not queued and not dropped. `Busy` already means "nothing was
/// applied, it is still yours, try again" and this is the same fact for a
/// different reason — so it reaches the app as an answer it can act on rather
/// than as a write that quietly never happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// More pending writes than this client will hold.
    TooManyPending { cap: usize },
    /// More pending BYTES than this client will hold. A count is not a byte
    /// budget: ten writes of a megabyte are not ten small ones.
    TooManyPendingBytes { cap: usize },
    /// The write, encoded, would be `bytes` against a wire limit of `limit`:
    /// it cannot be SENT. Refused before anything is held, so it never sits
    /// waiting for an answer that cannot come (craftworks-sdk#136).
    TooLargeToSend { bytes: u64, limit: usize },
    /// The ENGINE refused it as never acceptable: over `limit` of `bound` by
    /// `got`. Rolled back; sending it again is refused again, so splitting it
    /// is the caller's call (craftworks-sdk#136).
    TooLarge {
        bound: protocol::WriteBound,
        limit: u32,
        got: u32,
    },
    /// This page has NO SESSION: the browser gave it no randomness to mint one
    /// (craftworks-sdk#146). Without one its writes could not be told apart
    /// from another tab's, so none is made — loudly, never as a shared
    /// fallback number that is the old collision under a new name.
    NoSession,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::TooManyPending { cap } => {
                write!(f, "{cap} writes are already waiting for an answer")
            }
            Refused::TooManyPendingBytes { cap } => {
                write!(f, "{cap} bytes of writes are already waiting for an answer")
            }
            Refused::NoSession => write!(
                f,
                "this page could not start a session (the browser gave it no randomness), so it cannot save; reload the page"
            ),
            Refused::TooLargeToSend { bytes, limit } => write!(
                f,
                "this write is {bytes} bytes as sent and the limit is {limit}; split it into smaller writes"
            ),
            Refused::TooLarge { bound, limit, got } => match bound {
                protocol::WriteBound::CommitBlocks => write!(
                    f,
                    "this write would change {got} blocks at once and the limit is {limit}; split it into smaller writes"
                ),
                protocol::WriteBound::WriteBytes => write!(
                    f,
                    "this write is {got} bytes and the limit is {limit}; split it into smaller writes"
                ),
            },
        }
    }
}

/// Why a pending write was rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolledBack {
    /// The engine refused it.
    Failed,
    /// It was invalidated by an EARLIER write on the same key failing.
    ///
    /// Phase 3 writes are blind — the copy cannot know whether this write was
    /// computed from the failed one — so every later write on that key goes.
    /// Over-rolling-back is annoying and honest; under-rolling-back shows a
    /// value that was never anywhere.
    AfterFailed,
    /// No verdict within the timeout.
    ///
    /// An accepted write can be lost with the engine's context (F32), and
    /// without this one tab would show a phantom value for ever while another
    /// never saw it — the one permanent divergence.
    Unknown,
}

/// What a caller must be told after a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Told {
    /// Writes that are no longer pending and did not land.
    pub rolled_back: Vec<(u64, RolledBack)>,
    /// The KEYS those writes touched.
    ///
    /// Carried separately because a row asks "what is MY write doing" and a
    /// write id cannot answer that — a row would have to have remembered
    /// which id carried it, which is bookkeeping every caller would have to
    /// repeat and get right.
    pub rolled_back_keys: Vec<Vec<u8>>,
}

impl Told {
    pub fn is_empty(&self) -> bool {
        self.rolled_back.is_empty()
    }
}

#[derive(Debug, Default, Clone)]
struct Entry {
    /// The write of THIS client that last LANDED here (Published), if one
    /// did while a write was still pending on the key (sdk#265): a `Lost`
    /// write older than it can no longer land in the order the person made
    /// it.
    landed: Option<u64>,
    /// In the order they were made. The last one is what shows.
    pending: Vec<PendingWrite>,
}

/// This client's unsettled writes, per key.
pub struct Copy {
    keys: BTreeMap<Vec<u8>, Entry>,
    /// No verdict within this many milliseconds ⇒ `Unknown`, roll back.
    pub pending_timeout_ms: u64,
    /// Pending writes this client will hold at once.
    ///
    /// A pending write is not a cache entry, it is something the app is
    /// waiting on — so it is bounded here, or an app in a loop, or one whose
    /// engine has stopped answering, would accumulate them with nothing to
    /// stop it.
    pub max_pending: usize,
    /// And the same bound in BYTES, because a count is not a byte budget.
    pub max_pending_bytes: usize,
    /// Bytes currently held by pending writes.
    pending_bytes: usize,
    pending_count: usize,
}

impl Default for Copy {
    fn default() -> Self {
        Copy::new()
    }
}

impl Copy {
    pub fn new() -> Copy {
        Copy {
            keys: BTreeMap::new(),
            pending_timeout_ms: 60_000,
            max_pending: 256,
            max_pending_bytes: 4 * 1024 * 1024,
            pending_bytes: 0,
            pending_count: 0,
        }
    }

    /// What this client's own LAST write on `key` is doing, or `None` when
    /// no write of this client's is pending there.
    pub fn get(&self, key: &[u8]) -> Option<Visible> {
        let w = self.keys.get(key)?.pending.last()?;
        Some(if w.queued { Visible::Queued(w.value.clone(), w.write_id) } else { Visible::Pending(w.value.clone(), w.write_id) })
    }

    /// INTERIM: removed by R-b (READ-STATE § queue). The writes the ENGINE HAS
    /// NOT TAKEN — held by the window, queued after `Busy`, or SENT AND NOT
    /// YET ANSWERED — in `[lo, hi)`, each key's LAST such value (`None`:
    /// deleted). These are in no root, so a read shows them on top of its
    /// walk. The third kind is a write the engine PARKED to fetch the blocks
    /// it lands on: in a browser the node answers later, so a write to a cold
    /// tree waits there with no verdict, and a read in between missed it
    /// (measured: the notes acceptance's `put` after its own `define` read "no
    /// schema"). A write the engine accepted is in the warm root and is NOT
    /// here: two copies of one value is the defect this design removes. An
    /// entry dies on the engine's verdict: `Accepted` (it is in the warm
    /// root), or any refusal (it falls).
    pub fn unaccepted(&self, lo: &[u8], hi: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        if lo >= hi {
            return Vec::new();
        }
        self.keys
            .range::<[u8], _>((B::Included(lo), B::Excluded(hi)))
            .filter_map(|(k, e)| {
                // Reading only the LAST write is right because no write the
                // engine TOOK follows one it has not on the same key: the
                // outbox sends in order, and go-back-N pulls a later write
                // back behind a `Lost` one (sdk#265). Asserted, not assumed.
                debug_assert!(
                    {
                        let taken = |w: &PendingWrite| !(w.queued || w.held || w.at_node);
                        let first_untaken = e.pending.iter().position(|w| !taken(w));
                        first_untaken.is_none_or(|i| e.pending[i..].iter().all(|w| !taken(w)))
                    },
                    "a write the engine took follows one it has not, on one key: the overlay would show the older value"
                );
                let w = e.pending.last()?;
                (w.queued || w.held || w.at_node).then(|| (k.clone(), w.value.clone()))
            })
            .collect()
    }

    /// INTERIM (R-b): is any write not yet taken by the engine (held, queued,
    /// or sent and unanswered)?
    pub fn any_unaccepted(&self) -> bool {
        self.keys.values().flat_map(|e| e.pending.iter()).any(|w| w.queued || w.held || w.at_node)
    }

    /// A local write, applied optimistically — or REFUSED.
    ///
    /// The refusal leaves no trace. A write that is half-applied and then
    /// reported refused is worse than either outcome on its own: the app is
    /// told nothing happened while something did.
    pub fn write(
        &mut self,
        key: &[u8],
        value: Option<Vec<u8>>,
        write_id: u64,
        at_ms: u64,
    ) -> Result<(), Refused> {
        let size = value.as_ref().map_or(0, |v| v.len()) + key.len();
        // Checked BEFORE anything is touched, and both bounds separately —
        // a cap that is only ever reached through the other one is a cap
        // nobody has tested.
        if self.pending_count >= self.max_pending {
            return Err(Refused::TooManyPending {
                cap: self.max_pending,
            });
        }
        if self.pending_bytes + size > self.max_pending_bytes {
            return Err(Refused::TooManyPendingBytes {
                cap: self.max_pending_bytes,
            });
        }
        self.keys
            .entry(key.to_vec())
            .or_default()
            .pending
            .push(PendingWrite {
                write_id,
                value,
                queued: false,
                at_ms,
                held: false,
                at_node: false,
                declared_base: None,
            });
        self.pending_count += 1;
        self.pending_bytes += size;
        Ok(())
    }

    /// Pending ENTRIES held — one per (key, write), so a 2-key write counts
    /// 2 — and the bytes they hold.
    ///
    /// Not a count of WRITES: this comment said "writes" while the code
    /// counted entries, and the outbox compared it with [`queued_count`],
    /// which does count writes — so a queued multi-key write stopped the
    /// outbox for ever (craftworks-sdk#139). For writes, see
    /// [`pending_writes`](Self::pending_writes).
    ///
    /// [`queued_count`]: Self::queued_count
    pub fn pending(&self) -> (usize, usize) {
        (self.pending_count, self.pending_bytes)
    }

    /// Recount what pending holds. Called wherever entries leave.
    fn recount_pending(&mut self) {
        self.pending_count = self.keys.values().map(|e| e.pending.len()).sum();
        self.pending_bytes = self
            .keys
            .iter()
            .map(|(k, e)| {
                e.pending
                    .iter()
                    .map(|w| w.value.as_ref().map_or(0, |v| v.len()) + k.len())
                    .sum::<usize>()
            })
            .sum();
    }

    /// The engine answered `Busy`: the write is ours still, and not submitted.
    pub fn queued(&mut self, write_id: u64) {
        for e in self.keys.values_mut() {
            for w in &mut e.pending {
                if w.write_id == write_id {
                    w.queued = true;
                }
            }
        }
    }

    fn each_of(&mut self, write_id: u64, mut f: impl FnMut(&mut PendingWrite)) {
        for e in self.keys.values_mut() {
            for w in &mut e.pending {
                if w.write_id == write_id {
                    f(w);
                }
            }
        }
    }

    /// The outbox's window is full: this write is held, not sent.
    pub fn hold(&mut self, write_id: u64) {
        self.each_of(write_id, |w| {
            w.held = true;
            w.queued = false;
            w.at_node = false;
        });
    }

    /// THIS WRITE LEFT FOR THE NODE NOW — the one function every send goes
    /// through: the first send, a release from the window, a `Busy` re-send.
    /// It takes a window slot, and its timeout starts HERE. Measured from the
    /// `put`, a 300-row publish rolled back every row the window was still
    /// holding at 60 s as `Unknown` (craftworks-sdk#176); measured from the
    /// FIRST send, a `Busy` write re-sent at 55 s was rolled back 6 s later
    /// and its `Published` then arrived for a write the copy no longer had
    /// (sdk#179).
    pub fn sent(&mut self, write_id: u64, now_ms: u64) {
        self.each_of(write_id, |w| {
            w.held = false;
            w.queued = false;
            w.at_node = true;
            w.at_ms = now_ms;
        });
    }

    /// The node answered this write — any verdict: its window slot is free.
    pub fn answered(&mut self, write_id: u64) {
        self.each_of(write_id, |w| w.at_node = false);
    }

    /// Writes sent and not yet answered: the outbox's window, counted from the
    /// writes themselves. Distinct WRITES — a multi-key write is one entry per
    /// key and one slot.
    pub fn at_node_count(&self) -> usize {
        let ids: std::collections::BTreeSet<u64> = self
            .keys
            .values()
            .flat_map(|e| e.pending.iter())
            .filter(|w| w.at_node)
            .map(|w| w.write_id)
            .collect();
        ids.len()
    }

    pub fn submitted(&mut self, write_id: u64) {
        for e in self.keys.values_mut() {
            for w in &mut e.pending {
                if w.write_id == write_id {
                    w.queued = false;
                }
            }
        }
    }

    /// The engine published this write: it leaves the list, and the key
    /// remembers it LANDED (sdk#265's overtaken rule).
    /// The keys this pending write touches.
    pub fn keys_of(&self, write_id: u64) -> Vec<Vec<u8>> {
        self.keys.iter().filter(|(_, e)| e.pending.iter().any(|w| w.write_id == write_id)).map(|(k, _)| k.clone()).collect()
    }

    pub fn published(&mut self, write_id: u64) {
        for e in self.keys.values_mut() {
            let Some(i) = e.pending.iter().position(|w| w.write_id == write_id) else {
                continue;
            };
            e.pending.remove(i);
            e.landed = Some(write_id);
        }
        self.drop_empty();
    }

    /// The engine refused this write — and with it every LATER write on the
    /// same key.
    ///
    /// Blind writes: the copy cannot know whether a later write was computed
    /// from this one. Over-rolling-back is honest; under-rolling-back leaves a
    /// value on screen that was never anywhere.
    pub fn failed(&mut self, write_id: u64) -> Told {
        self.fall(std::collections::BTreeMap::from([(
            write_id,
            RolledBack::Failed,
        )]))
    }

    /// `seeds` and every write BEHIND one of them on any key -- a closure: a
    /// write behind a seed can take down writes behind it on its other keys.
    fn closure(&self, mut set: std::collections::BTreeSet<u64>) -> std::collections::BTreeSet<u64> {
        loop {
            let mut grew = false;
            for e in self.keys.values() {
                if let Some(i) = e.pending.iter().position(|w| set.contains(&w.write_id)) {
                    for w in &e.pending[i..] {
                        grew |= set.insert(w.write_id);
                    }
                }
            }
            if !grew {
                return set;
            }
        }
    }

    /// The writes made AFTER `write_id` on its keys (and behind those on
    /// theirs), oldest first: what must land after it (sdk#265).
    pub fn behind(&self, write_id: u64) -> Vec<u64> {
        let mut set = self.closure(std::collections::BTreeSet::from([write_id]));
        set.remove(&write_id);
        set.into_iter().collect()
    }

    /// Has this write LEFT the outbox -- sent, or taken by the engine --
    /// rather than held by the window or queued to go again?
    pub fn left_outbox(&self, write_id: u64) -> bool {
        self.keys.values().flat_map(|e| e.pending.iter()).any(|w| w.write_id == write_id && !w.queued && !w.held)
    }

    /// Roll back `seeds` -- and every write behind one of them on any key --
    /// WHOLE, on every key each touches.
    ///
    /// A write made after another on the same key was made on top of it, so it
    /// goes too (`AfterFailed`). This used to truncate KEY BY KEY: a later
    /// multi-key write lost only its entry on the shared key, stayed queued
    /// with the rest, and was re-sent WITHOUT it -- half of one atomic write
    /// published (craftworks-sdk#136, measured: sent `[other, shared]`, then
    /// `[other]`, which published). A write is all or nothing in the copy, as
    /// on the wire; and a write that falls can take down writes behind it on
    /// ITS other keys, so this is a closure, not one pass.
    fn fall(&mut self, seeds: std::collections::BTreeMap<u64, RolledBack>) -> Told {
        let falls = self.closure(seeds.keys().copied().collect());
        let mut told = Told::default();
        for (key, e) in self.keys.iter_mut() {
            let mut touched = false;
            e.pending.retain(|w| {
                if !falls.contains(&w.write_id) {
                    return true;
                }
                touched = true;
                told.rolled_back.push((
                    w.write_id,
                    seeds
                        .get(&w.write_id)
                        .copied()
                        .unwrap_or(RolledBack::AfterFailed),
                ));
                false
            });
            if touched {
                told.rolled_back_keys.push(key.clone());
            }
        }
        self.drop_empty();
        told
    }

    /// Roll back anything that has waited too long without a verdict.
    pub fn time_out(&mut self, now_ms: u64) -> Told {
        let timeout = self.pending_timeout_ms;
        // As `failed`: everything behind a timed-out write goes too, for the
        // same reason, and whole.
        let seeds = self
            .keys
            .values()
            .flat_map(|e| e.pending.iter())
            .filter(|w| !w.held && now_ms.saturating_sub(w.at_ms) >= timeout)
            .map(|w| (w.write_id, RolledBack::Unknown))
            .collect();
        self.fall(seeds)
    }

    fn drop_empty(&mut self) {
        self.keys.retain(|_, e| !e.pending.is_empty());
        self.recount_pending();
    }

    /// Write ids still waiting on a verdict.
    /// THE OLDEST QUEUED WRITE, with everything needed to send it again.
    ///
    /// A write the engine answered `Busy` is one it refused and did not
    /// apply — "the client still holds the write and may re-submit it". This
    /// is the client holding it. Nothing re-sent them, so they sat here until
    /// `time_out` rolled them back and their rows vanished from the screen
    /// (sdk#106).
    ///
    /// ONE, and the oldest, because the engine takes one commit at a time and
    /// because writes must land in the order they were made: a later edit to
    /// the same key arriving first would leave the network holding the older
    /// value. Ordered by `write_id`, which is issued in that order.
    ///
    /// All the edits of that write together — a multi-key write is all or
    /// nothing on the wire as it is in the copy.
    /// The writes made BEFORE `write_id` on any of its keys and still
    /// pending: what should land ahead of it.
    pub fn ahead_of(&self, write_id: u64) -> Vec<u64> {
        let mut ids = std::collections::BTreeSet::new();
        for e in self.keys.values() {
            if let Some(i) = e.pending.iter().position(|w| w.write_id == write_id) {
                ids.extend(e.pending[..i].iter().map(|w| w.write_id));
            }
        }
        ids.into_iter().collect()
    }

    /// Has a LATER write of this client's already landed on one of this
    /// write's keys (sdk#265)? Then this one cannot go again: it would put
    /// the older value back where the person's last edit is.
    pub fn overtaken(&self, write_id: u64) -> bool {
        self.keys
            .values()
            .filter(|e| e.pending.iter().any(|w| w.write_id == write_id))
            .any(|e| e.landed.is_some_and(|b| b > write_id))
    }

    /// Is this write queued to go again?
    pub fn is_queued(&self, write_id: u64) -> bool {
        self.keys.values().flat_map(|e| e.pending.iter()).any(|w| w.write_id == write_id && w.queued)
    }

    /// These writes can no longer land (sdk#265): they fall, WHOLE, with
    /// every write behind them, as [`Copy::failed`].
    pub fn failed_all(&mut self, write_ids: &[u64]) -> Told {
        self.fall(write_ids.iter().map(|id| (*id, RolledBack::Failed)).collect())
    }

    pub fn oldest_queued(&self) -> Option<QueuedWrite> {
        let mut best: Option<u64> = None;
        for e in self.keys.values() {
            for w in &e.pending {
                if w.queued && best.is_none_or(|b| w.write_id < b) {
                    best = Some(w.write_id);
                }
            }
        }
        let id = best?;
        let mut edits: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for (k, e) in &self.keys {
            for w in &e.pending {
                if w.write_id == id {
                    edits.push((k.clone(), w.value.clone()));
                }
            }
        }
        edits.sort_by(|a, b| a.0.cmp(&b.0));
        Some((id, edits))
    }

    /// How many distinct pending WRITES there are, whatever keys each holds.
    /// The same unit as [`queued_count`](Self::queued_count).
    pub fn pending_writes(&self) -> usize {
        self.pending_ids().len()
    }

    /// How many distinct WRITES are waiting to be sent again.
    pub fn queued_count(&self) -> usize {
        let mut ids = std::collections::BTreeSet::new();
        for e in self.keys.values() {
            for w in &e.pending {
                if w.queued {
                    ids.insert(w.write_id);
                }
            }
        }
        ids.len()
    }

    pub fn pending_ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .keys
            .values()
            .flat_map(|e| e.pending.iter().map(|w| w.write_id))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}
