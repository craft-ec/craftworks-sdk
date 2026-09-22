//! The browser's local copy: a cache that resolves NOTHING.
//!
//! `Db` and every component need synchronous reads, and in a browser a read
//! cannot be a round trip. So the client keeps a copy of the ranges the app
//! has bound: reads come from here, writes go to the engine AND here, and the
//! engine's deltas are applied in.
//!
//! # What it is not
//!
//! **Not an authority.** The SDK is correct when this is EMPTY — nothing here
//! is "saved" until the engine says published, and every surface above keeps
//! saying so. Recovery lives on the network.
//!
//! **Not a tree.** It needs ordered reads, delta application, pending writes,
//! byte-capped eviction, and one thing more interesting than all of them; it
//! needs no proofs and no canonical encoding, and it never COMPUTES a root —
//! it RECORDS the one it reflects. A tree builder in the client would
//! contradict §19's thin browser SDK for no capability.
//!
//! **Not a resolver.** Base is whatever the engine last told us; pending is
//! our own unacknowledged writes. There is no last-writer-wins here and there
//! must never be: in phase 8 resolution belongs to the contract or the CRDT,
//! and a shortcut taken in the client would be a second, invisible merge rule
//! that nothing on the network agrees with.
//!
//! # The one thing that is interesting
//!
//! **"Not loaded" is not "empty".** A map alone cannot tell them apart, and
//! the difference is the whole reason a read can be trusted: a range nobody
//! has fetched must not answer "there is nothing here". So the copy tracks the
//! set of key INTERVALS it has loaded, and a read outside them says so.
//!
//! # Per key: `visible = base ⊕ pending-in-order`
//!
//! Not one pending value but an ordered LIST, so every verdict is a small edit
//! to one term: published moves base forward, failed removes an entry. Rollback
//! is then defined by construction rather than reconstructed from a history
//! nobody kept.

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
    /// Keys where a delta moved BASE while a write was pending on them.
    ///
    /// Not preventable here — the pending write may have been computed from
    /// the value the delta just changed — so it is made VISIBLE instead. Fires
    /// once per key per delta.
    pub moved_under_pending: Vec<Vec<u8>>,
}

impl Told {
    pub fn is_empty(&self) -> bool {
        self.rolled_back.is_empty() && self.moved_under_pending.is_empty()
    }
}

#[derive(Debug, Default, Clone)]
struct Entry {
    base: Option<Vec<u8>>,
    /// In the order they were made. The last one is what shows.
    pending: Vec<PendingWrite>,
}

/// One tree's copy, keyed by the root it reflects.
pub struct Copy {
    /// What the engine last told us, per key.
    keys: BTreeMap<Vec<u8>, Entry>,
    /// The key intervals that have been LOADED, as `[lo, hi)`, disjoint and
    /// sorted. What makes "not loaded" different from "empty".
    loaded: Vec<(Vec<u8>, Vec<u8>)>,
    /// The root this copy reflects. RECORDED, never computed.
    root: Option<[u8; 32]>,
    bytes: usize,
    /// Over this, unbound ranges are evicted. A cache with no cap is a leak
    /// with a nicer name.
    pub max_bytes: usize,
    /// No verdict within this many milliseconds ⇒ `Unknown`, roll back.
    pub pending_timeout_ms: u64,
    /// Pending writes this client will hold at once.
    ///
    /// Eviction deliberately never drops a pending write — it is not a cache
    /// entry, it is something the app is waiting on — which left pending as
    /// the only unbounded thing here. An app in a loop, or one whose engine
    /// has stopped answering, would accumulate them with nothing to stop it.
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
            loaded: Vec::new(),
            root: None,
            bytes: 0,
            max_bytes: 8 * 1024 * 1024,
            pending_timeout_ms: 60_000,
            max_pending: 256,
            max_pending_bytes: 4 * 1024 * 1024,
            pending_bytes: 0,
            pending_count: 0,
        }
    }

    /// The root this copy reflects, if it reflects one.
    pub fn root(&self) -> Option<[u8; 32]> {
        self.root
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Is this key inside a range that has been loaded?
    pub fn is_loaded(&self, key: &[u8]) -> bool {
        self.loaded
            .iter()
            .any(|(lo, hi)| key >= lo.as_slice() && key < hi.as_slice())
    }

    /// What a component sees, or `None` if this range was never loaded.
    ///
    /// The outer `None` is "I do not know", NOT "there is nothing here". A
    /// caller told the wrong one of those shows an empty list for data that
    /// exists.
    pub fn get(&self, key: &[u8]) -> Option<Visible> {
        if !self.is_loaded(key) {
            return None;
        }
        Some(self.visible(key))
    }

    fn visible(&self, key: &[u8]) -> Visible {
        let Some(e) = self.keys.get(key) else {
            return Visible::Clean(None);
        };
        match e.pending.last() {
            None => Visible::Clean(e.base.clone()),
            Some(w) if w.queued => Visible::Queued(w.value.clone(), w.write_id),
            Some(w) => Visible::Pending(w.value.clone(), w.write_id),
        }
    }

    /// Stop answering for `[lo, hi)`: it is no longer known.
    ///
    /// For the case where the engine says it CANNOT compute a delta. The
    /// range must then stop being answered from cache — a copy that kept
    /// serving it would answer confidently from a version the engine has
    /// just said it cannot reconcile. Told nothing, the next read says
    /// `NotLoaded` and the range is fetched in full, which is the honest
    /// outcome.
    ///
    /// PENDING WRITES SURVIVE. They are this client's own, not the engine's
    /// account of anything, and dropping them would silently discard writes a
    /// person made and can still see.
    pub fn forget(&mut self, lo: &[u8], hi: &[u8]) {
        let doomed: Vec<Vec<u8>> = self
            .keys
            .range(lo.to_vec()..hi.to_vec())
            .filter(|(_, e)| e.pending.is_empty())
            .map(|(k, _)| k.clone())
            .collect();
        for k in doomed {
            if let Some(e) = self.keys.remove(&k) {
                self.bytes = self
                    .bytes
                    .saturating_sub(e.base.as_ref().map_or(0, |v| v.len()));
            }
        }
        // And the interval itself: "loaded" is the claim that must go.
        self.loaded
            .retain(|(l, h)| !(l.as_slice() >= lo && h.as_slice() <= hi));
    }

    /// Forget ONE key, however wide the interval that loaded it (M2): its
    /// entry goes (unless a write is pending on it) and a hole is punched in
    /// every loaded interval that covers it, so the next read of it is
    /// `NotLoaded` and fetches what the TREE holds. [`Copy::forget`] drops
    /// only intervals lying wholly inside its range, which a single key never
    /// covers.
    pub fn forget_key(&mut self, key: &[u8]) {
        if self.keys.get(key).is_some_and(|e| e.pending.is_empty()) {
            if let Some(e) = self.keys.remove(key) {
                self.bytes = self.bytes.saturating_sub(e.base.as_ref().map_or(0, |v| v.len()));
            }
        }
        let mut after = key.to_vec();
        after.push(0);
        let mut out = Vec::with_capacity(self.loaded.len() + 1);
        for (l, h) in std::mem::take(&mut self.loaded) {
            if l.as_slice() <= key && key < h.as_slice() {
                if l.as_slice() < key {
                    out.push((l, key.to_vec()));
                }
                if after < h {
                    out.push((after.clone(), h));
                }
            } else {
                out.push((l, h));
            }
        }
        self.loaded = out;
    }

    /// Rows in `[lo, hi)`, or `None` if any of that range was never loaded.
    pub fn range(&self, lo: &[u8], hi: &[u8]) -> Option<Vec<(Vec<u8>, Visible)>> {
        if !self.range_loaded(lo, hi) {
            return None;
        }
        Some(
            self.keys
                .range::<[u8], _>((B::Included(lo), B::Excluded(hi)))
                .map(|(k, _)| (k.clone(), self.visible(k)))
                .filter(|(_, v)| v.value().is_some())
                .collect(),
        )
    }

    /// Is every part of `[lo, hi)` covered by loaded intervals?
    pub fn range_loaded(&self, lo: &[u8], hi: &[u8]) -> bool {
        if lo >= hi {
            return true;
        }
        let mut at = lo.to_vec();
        loop {
            let Some((_, end)) = self
                .loaded
                .iter()
                .find(|(l, h)| at >= *l && at < *h)
                .cloned()
            else {
                return false;
            };
            if end.as_slice() >= hi {
                return true;
            }
            at = end;
        }
    }

    /// Record that `[lo, hi)` has been read, with these rows.
    ///
    /// Everything in the interval and not in `rows` is known ABSENT, which is
    /// a different fact from unknown and is why this takes the interval rather
    /// than only the rows.
    pub fn loaded_range(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        rows: Vec<(Vec<u8>, Vec<u8>)>,
        at_root: [u8; 32],
    ) {
        // A copy is per ROOT. Rows read at a different root than the one held
        // describe a different tree state, so the base is replaced rather than
        // merged — merging two roots' rows is the resolution this must never
        // do.
        if self.root != Some(at_root) {
            self.forget_base();
            self.root = Some(at_root);
        }
        let held: Vec<Vec<u8>> = self
            .keys
            .range::<[u8], _>((B::Included(lo), B::Excluded(hi)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in held {
            if let Some(e) = self.keys.get_mut(&k) {
                self.bytes -= e.base.as_ref().map_or(0, |v| v.len());
                e.base = None;
                if e.pending.is_empty() {
                    self.keys.remove(&k);
                }
            }
        }
        for (k, v) in rows {
            self.bytes += v.len();
            self.keys.entry(k).or_default().base = Some(v);
        }
        self.add_interval(lo.to_vec(), hi.to_vec());
        self.evict();
    }

    fn add_interval(&mut self, lo: Vec<u8>, hi: Vec<u8>) {
        self.loaded.push((lo, hi));
        self.loaded.sort();
        let mut merged: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (lo, hi) in std::mem::take(&mut self.loaded) {
            match merged.last_mut() {
                Some((_, end)) if lo <= *end => {
                    if hi > *end {
                        *end = hi;
                    }
                }
                _ => merged.push((lo, hi)),
            }
        }
        self.loaded = merged;
    }

    fn forget_base(&mut self) {
        self.keys.retain(|_, e| {
            e.base = None;
            !e.pending.is_empty()
        });
        self.loaded.clear();
        self.bytes = 0;
    }

    /// Drop loaded ranges until under the cap.
    ///
    /// Base only: a pending write is not a cache entry, it is something the
    /// app is waiting on, and evicting it would show the row reverting for a
    /// reason nobody can see.
    fn evict(&mut self) {
        while self.bytes > self.max_bytes {
            let Some((lo, hi)) = self.loaded.pop() else {
                return;
            };
            let victims: Vec<Vec<u8>> = self
                .keys
                .range::<[u8], _>((B::Included(lo.as_slice()), B::Excluded(hi.as_slice())))
                .map(|(k, _)| k.clone())
                .collect();
            for k in victims {
                if let Some(e) = self.keys.get_mut(&k) {
                    self.bytes -= e.base.as_ref().map_or(0, |v| v.len());
                    e.base = None;
                    if e.pending.is_empty() {
                        self.keys.remove(&k);
                    }
                }
            }
        }
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

    /// The engine published this write: its value becomes base.
    pub fn published(&mut self, write_id: u64) {
        let mut empty: Vec<Vec<u8>> = Vec::new();
        for (k, e) in self.keys.iter_mut() {
            let Some(i) = e.pending.iter().position(|w| w.write_id == write_id) else {
                continue;
            };
            let w = e.pending.remove(i);
            // Base moves forward. Anything pending BEHIND it still shows on
            // top, which is the point of keeping a list.
            e.base = w.value;
            if e.base.is_none() && e.pending.is_empty() {
                empty.push(k.clone());
            }
        }
        for k in empty {
            self.keys.remove(&k);
        }
        self.recount();
        self.recount_pending();
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
        let mut falls: std::collections::BTreeSet<u64> = seeds.keys().copied().collect();
        loop {
            let mut grew = false;
            for e in self.keys.values() {
                if let Some(i) = e.pending.iter().position(|w| falls.contains(&w.write_id)) {
                    for w in &e.pending[i..] {
                        grew |= falls.insert(w.write_id);
                    }
                }
            }
            if !grew {
                break;
            }
        }
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

    /// The engine moved to a new root: apply the changes to BASE.
    ///
    /// Pending writes stay on top. The copy can then show a combination that
    /// exists nowhere yet — which is what optimistic UI IS — and the hazard,
    /// a pending write derived from a value this delta just changed, is made
    /// VISIBLE rather than pretended away.
    pub fn apply_delta(
        &mut self,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        new_root: [u8; 32],
    ) -> Told {
        let mut told = Told::default();
        for (k, v) in changes {
            // Outside what is loaded, there is nothing to update: the copy
            // does not accumulate rows it was never asked for.
            if !self.is_loaded(&k) {
                continue;
            }
            let e = self.keys.entry(k.clone()).or_default();
            e.base = v;
            if !e.pending.is_empty() {
                told.moved_under_pending.push(k.clone());
            }
            if e.base.is_none() && e.pending.is_empty() {
                self.keys.remove(&k);
            }
        }
        self.root = Some(new_root);
        self.recount();
        told
    }

    fn drop_empty(&mut self) {
        self.keys
            .retain(|_, e| e.base.is_some() || !e.pending.is_empty());
        self.recount();
        self.recount_pending();
    }

    fn recount(&mut self) {
        self.bytes = self
            .keys
            .values()
            .map(|e| e.base.as_ref().map_or(0, |v| v.len()))
            .sum();
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
