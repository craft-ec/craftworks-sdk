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

/// A sorted byte map.
///
/// **An implementation may treat an input it cannot represent as a bug and
/// stop.** A store has no error channel — `put` returns nothing — so a key or
/// value past what the underlying structure allows cannot be reported, and
/// failing quietly would be worse than failing loudly. Screening is
/// [`Db`](crate::Db)'s job: it checks every key and value against the tree's
/// limits before a store is touched, and returns an error the app can handle.
/// That division is deliberate, and it is why [`TreeStore`](crate::TreeStore)
/// may panic where a limit is breached.
pub trait Store {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn put(&mut self, key: &[u8], value: &[u8]);
    /// Returns whether the key existed.
    fn delete(&mut self, key: &[u8]) -> bool;
    /// Entries with `lo <= key < hi`, ascending, or descending if `reverse`;
    /// at most `limit`.
    fn scan(&self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Vec<(Vec<u8>, Vec<u8>)>;

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
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key).cloned()
    }
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.0.insert(key.to_vec(), value.to_vec());
    }
    fn delete(&mut self, key: &[u8]) -> bool {
        self.0.remove(key).is_some()
    }
    fn scan(&self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        if lo >= hi {
            return Vec::new();
        }
        let r = self
            .0
            .range(lo.to_vec()..hi.to_vec())
            .map(|(k, v)| (k.clone(), v.clone()));
        if reverse {
            r.rev().take(limit).collect()
        } else {
            r.take(limit).collect()
        }
    }
}
