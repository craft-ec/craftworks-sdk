//! The storage interface the SDK is written against: a sorted byte map.
//!
//! Today an in-memory map sits behind it. The prolly tree, and later the engine
//! delegate, plug in behind the same four operations; nothing above changes.

use std::collections::BTreeMap;

/// One change to one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Put(Vec<u8>),
    Delete,
}

/// Why a read could not be answered.
///
/// Reads have an error channel and writes do not, and the asymmetry is
/// deliberate: a write is screened by [`Db`](crate::Db) before it reaches a
/// store, but whether a read can be answered at all is a property of the
/// store, and only the store knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The engine could not answer: a block it needed was not to be had.
    ///
    /// Not "the key does not exist" — that is `Ok(None)`, and the two are
    /// different facts an app acts on differently.
    Unavailable,
    /// The connection to the engine produced nothing usable.
    NoAnswer,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unavailable => f.write_str(
                "the engine could not reach a block this read needed; \
                 the key may well exist",
            ),
            StoreError::NoAnswer => f.write_str("the engine did not answer"),
        }
    }
}

impl std::error::Error for StoreError {}

pub type Read<T> = std::result::Result<T, StoreError>;

/// What a delta answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    /// The changes, and where they bring the reader to. `None` as a value is
    /// a REMOVAL, not an empty value.
    Changes {
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        cursor: Option<Vec<u8>>,
        new_root: [u8; 32],
    },
    /// The delta could not be computed; read the range in full.
    ///
    /// A defined outcome rather than an error: the root the reader was
    /// standing on may simply be gone, which is ordinary for a reader that
    /// was away a while.
    FullReloadRequired { new_root: [u8; 32] },
}

/// Reading, which for some stores is a ROUND TRIP.
///
/// Separate from [`Store`], and taking `&mut self`, and both are the same
/// decision. An engine-backed store's data is on the other side of a
/// connection: a read is an operation with a duration, not an accessor. A
/// `&self` read would have to hide interior mutability — making a network
/// round trip look free — or answer `None`, which says the key does not
/// exist, or panic, which in a browser with `panic = abort` is a dead wasm
/// instance rather than something an app can catch.
///
/// `&mut self` says so in the type, and it says so for every backend, which
/// is what lets one surface sit over both: the in-memory store implements
/// this trivially and an app does not change when it moves onto a node.
pub trait Reads {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>>;

    /// Entries with `lo <= key < hi`, ascending, or descending if `reverse`;
    /// at most `limit`.
    fn scan(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>>;

    /// The root this store is currently standing on.
    ///
    /// What a reader passes back to [`Reads::changes_since`]. An in-memory
    /// store has one too — its tree has a root like any other — so a binding
    /// written against this works unchanged on both backends.
    fn root(&mut self) -> Read<[u8; 32]>;

    /// What changed in `[lo, hi)` since `from`.
    ///
    /// **Per TREE.** A view assembled over several device trees is NOT the
    /// union of their per-tree deltas: a tombstone in one tree can REVEAL an
    /// older value in another, which no per-tree diff mentions. Phase 3 has
    /// one tree per identity; this is stated so nothing later assumes the
    /// union is valid.
    ///
    /// No subscription is involved. A delta is a capability of the tree,
    /// available to any reader holding a root — a plain reload uses it.
    fn changes_since(
        &mut self,
        from: [u8; 32],
        lo: &[u8],
        hi: &[u8],
        max_entries: u32,
    ) -> Read<Delta>;
}

/// A sorted byte map: the WRITE half.
///
/// Reading lives in [`Reads`], because for some stores a read is a round trip
/// and that has to be visible in the type rather than hidden behind `&self`.
///
/// **An implementation may treat a write it cannot represent as a bug and
/// stop.** A write has no error channel — `put` returns nothing — so a key or
/// value past what the underlying structure allows cannot be reported, and
/// failing quietly would be worse than failing loudly. Screening is
/// [`Db`](crate::Db)'s job: it checks every key and value against the tree's
/// limits before a store is touched, and returns an error the app can handle.
/// That division is deliberate, and it is why [`TreeStore`](crate::TreeStore)
/// may panic where a limit is breached.
pub trait Store {
    fn put(&mut self, key: &[u8], value: &[u8]);
    /// Returns whether the key existed.
    fn delete(&mut self, key: &[u8]) -> bool;

    /// Apply several edits as ONE change.
    ///
    /// A record and the index entries that point at it are one fact, and the
    /// store behind this trait may be a structure where every write costs a new
    /// root — writing them one at a time would make several versions of a state
    /// that was never half-written. The default loops, which is right for a
    /// store where a write is just a write.
    ///
    /// **The caller sorts.** Edits arrive sorted by key with no duplicates; the
    /// SDK does that in [`sorted_edits`] before calling, because it is the
    /// caller that knows which of two writes to one key is the later one.
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) {
        for (k, e) in edits {
            match e {
                Edit::Put(v) => self.put(k, v),
                Edit::Delete => {
                    self.delete(k);
                }
            }
        }
    }
}

/// Sort by key and drop all but the LAST edit for each key.
///
/// Last-write-wins is the SDK's rule, not the tree's: a batch is a sequence the
/// caller wrote in order, and the tree takes a set. Doing it here means every
/// store sees the same batch, so the differential tests compare like with like.
pub fn sorted_edits(edits: Vec<(Vec<u8>, Edit)>) -> Vec<(Vec<u8>, Edit)> {
    let mut out: BTreeMap<Vec<u8>, Edit> = BTreeMap::new();
    for (k, e) in edits {
        out.insert(k, e);
    }
    out.into_iter().collect()
}

/// In-memory store.
#[derive(Default)]
pub struct MemStore(BTreeMap<Vec<u8>, Vec<u8>>);

impl Store for MemStore {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.0.insert(key.to_vec(), value.to_vec());
    }
    fn delete(&mut self, key: &[u8]) -> bool {
        self.0.remove(key).is_some()
    }
}

impl Reads for MemStore {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        Ok(self.0.get(key).cloned())
    }
    fn scan(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        if lo >= hi {
            return Ok(Vec::new());
        }
        let r = self
            .0
            .range(lo.to_vec()..hi.to_vec())
            .map(|(k, v)| (k.clone(), v.clone()));
        Ok(if reverse {
            r.rev().take(limit).collect()
        } else {
            r.take(limit).collect()
        })
    }
    /// A hash of the whole map.
    ///
    /// Not a prolly root, and it does not have to be: what a root is FOR here
    /// is answering "is this the same content I last saw", and a content hash
    /// answers that exactly. It means a binding over the in-memory store
    /// behaves like one over a tree — including reloading when it should —
    /// rather than working only on the backend it was tested against.
    fn root(&mut self) -> Read<[u8; 32]> {
        let mut h = blake3::Hasher::new();
        for (k, v) in &self.0 {
            h.update(&(k.len() as u64).to_le_bytes());
            h.update(k);
            h.update(&(v.len() as u64).to_le_bytes());
            h.update(v);
        }
        Ok(*h.finalize().as_bytes())
    }
    /// The in-memory store keeps no history, so it cannot diff two roots.
    ///
    /// It says so with the DEFINED answer rather than an error, which is the
    /// point of that answer existing: a binding does the same thing here as
    /// it does against an engine whose old root has been evicted, so the
    /// reload path is exercised on both backends instead of only the one
    /// that is harder to test.
    fn changes_since(
        &mut self,
        _from: [u8; 32],
        _lo: &[u8],
        _hi: &[u8],
        _max_entries: u32,
    ) -> Read<Delta> {
        Ok(Delta::FullReloadRequired {
            new_root: self.root()?,
        })
    }
}
