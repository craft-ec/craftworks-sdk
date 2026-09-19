//! The storage interface the SDK is written against: a sorted byte map.
//!
//! Today an in-memory map sits behind it. The prolly tree, and later the engine
//! delegate, plug in behind the same four operations; nothing above changes.

use std::collections::BTreeMap;

pub trait Store {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn put(&mut self, key: &[u8], value: &[u8]);
    /// Returns whether the key existed.
    fn delete(&mut self, key: &[u8]) -> bool;
    /// Entries with `lo <= key < hi`, ascending, or descending if `reverse`;
    /// at most `limit`.
    fn scan(&self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Vec<(Vec<u8>, Vec<u8>)>;
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
