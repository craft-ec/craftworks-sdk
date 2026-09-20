//! `Db`: collections of typed records over a [`Store`].
//!
//! Keys follow the architecture's keyspace (§5):
//! `0x01 ‖ domain ‖ 0x00 ‖ rkey(16)` for records, `0x00 "schema" 0x00 domain` for
//! a domain's schema — so the schema travels with the data it types.

use crate::id::{self, Env, IdGen, RKey};
use crate::record;
use crate::schema::Schema;
use crate::store::{sorted_edits, Edit, Store};
use freenet_prolly::node::{MAX_KEY, MAX_VALUE};
use serde::Serialize;
use serde_json::{Map, Value};

pub const MAX_DOMAIN: usize = 32;
const T_SYSTEM: u8 = 0x00;
const T_RECORD: u8 = 0x01;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    pub created: u64,
    pub updated: u64,
    pub fields: Map<String, Value>,
}

#[derive(Default, Clone, Copy)]
pub struct Scan {
    pub reverse: bool,
    /// 0 = no limit.
    pub limit: usize,
    /// Resume strictly after this record (in scan direction).
    pub after: Option<RKey>,
}

pub struct Db<S: Store, E: Env> {
    store: S,
    env: E,
    ids: IdGen,
    /// Signing identity. All zero until identities land (phase 3).
    author: [u8; 32],
}

pub type Result<T> = std::result::Result<T, String>;

fn check_domain(d: &str) -> Result<()> {
    let ok = !d.is_empty()
        && d.len() <= MAX_DOMAIN
        && d.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "domain `{d}` must be 1–{MAX_DOMAIN} of a-z 0-9 _ - ."
        ))
    }
}

fn prefix(domain: &str) -> Vec<u8> {
    let mut k = vec![T_RECORD];
    k.extend_from_slice(domain.as_bytes());
    k.push(0);
    k
}
pub fn record_key(domain: &str, id: &RKey) -> Vec<u8> {
    let mut k = prefix(domain);
    k.extend_from_slice(id);
    k
}
fn schema_key(domain: &str) -> Vec<u8> {
    let mut k = vec![T_SYSTEM];
    k.extend_from_slice(b"schema\0");
    k.extend_from_slice(domain.as_bytes());
    k
}
/// The smallest key greater than every key starting with `p` (p ends in 0x00).
fn upper(p: &[u8]) -> Vec<u8> {
    let mut h = p.to_vec();
    *h.last_mut().unwrap() += 1;
    h
}

impl<S: Store, E: Env> Db<S, E> {
    pub fn new(store: S, env: E, device: [u8; 4]) -> Self {
        Db {
            store,
            env,
            ids: IdGen::new(device),
            author: [0; 32],
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Write several keys as ONE change to the store.
    ///
    /// A record and the index entries that point at it are one fact. Behind
    /// `Store` may be a tree where every write costs a new root, so writing them
    /// separately would publish roots for states the app never had. Today a
    /// record write is one key; the shape is here so that adding index entries
    /// is adding to this list rather than adding a second write.
    ///
    /// Sorting and last-write-wins happen here, in the SDK: a batch is a
    /// sequence the caller wrote in order and the store takes a set, so it is
    /// the caller that knows which of two writes to a key is the later one.
    ///
    /// **This is where the SDK screens what it sends a store.** Every key and
    /// value the app can reach the store through passes here, including keys the
    /// SDK derives rather than takes (index terms are built from a record's
    /// contents, so an app can produce a long one without ever writing it). A
    /// store is allowed to treat a limit breach as a bug and stop — see
    /// [`Store`] — so the app has to be told before, in a way it can handle.
    fn write(&mut self, edits: Vec<(Vec<u8>, Edit)>) -> Result<()> {
        for (key, edit) in &edits {
            if key.len() > MAX_KEY {
                return Err(format!(
                    "key is {} bytes and the limit is {MAX_KEY}",
                    key.len()
                ));
            }
            if let Edit::Put(v) = edit {
                if v.len() > MAX_VALUE {
                    return Err(format!(
                        "record is {} bytes and the limit is {MAX_VALUE}; \
                         store content this large as a file or a blob and keep a \
                         reference to it in the record",
                        v.len()
                    ));
                }
            }
        }
        self.store.apply_batch(&sorted_edits(edits));
        Ok(())
    }

    pub fn env_mut(&mut self) -> &mut E {
        &mut self.env
    }

    /// Declare (or extend) a domain's schema. Extending may only append optional fields.
    pub fn define(&mut self, domain: &str, schema: &Schema) -> Result<()> {
        check_domain(domain)?;
        schema.check()?;
        if let Some(old) = self.schema(domain)? {
            old.allows(schema)?;
        }
        let bytes = serde_json::to_vec(schema).map_err(|e| e.to_string())?;
        self.write(vec![(schema_key(domain), Edit::Put(bytes))])
    }

    /// The domain's schema, or `None` if it has none.
    ///
    /// Fallible because the READ is: a store whose data is remote cannot
    /// answer from `&self`, and saying "no schema" when the truth is "could
    /// not look" would make an app define a domain that already exists.
    pub fn schema(&self, domain: &str) -> Result<Option<Schema>> {
        match self.get_key(&schema_key(domain))? {
            Some(b) => Ok(serde_json::from_slice(&b).ok()),
            None => Ok(None),
        }
    }

    fn need_schema(&self, domain: &str) -> Result<Schema> {
        check_domain(domain)?;
        self.schema(domain)?
            .ok_or_else(|| format!("domain `{domain}` has no schema; define it first"))
    }

    pub fn domains(&self) -> Result<Vec<String>> {
        let lo = schema_key("");
        let mut hi = lo.clone();
        *hi.last_mut().unwrap() = 1;
        Ok(self
            .scan_keys(&lo, &hi, false, usize::MAX)?
            .into_iter()
            .filter_map(|(k, _)| String::from_utf8(k[lo.len()..].to_vec()).ok())
            .collect())
    }

    pub fn put(&mut self, domain: &str, fields: &Map<String, Value>) -> Result<Record> {
        let schema = self.need_schema(domain)?;
        let rkey = self.ids.next(&mut self.env);
        let now = id::created_ms(&rkey);
        let bytes = record::encode(&schema, fields, now, now, &self.author)?;
        self.write(vec![(record_key(domain, &rkey), Edit::Put(bytes.clone()))])?;
        self.read(&schema, &rkey, &bytes)
    }

    /// Merge `patch` over the record; a `null` value removes that field.
    pub fn update(
        &mut self,
        domain: &str,
        rkey: &RKey,
        patch: &Map<String, Value>,
    ) -> Result<Record> {
        let schema = self.need_schema(domain)?;
        let key = record_key(domain, rkey);
        let old = self.get_key(&key)?.ok_or("no such record")?;
        let mut d = record::decode(&schema, &old)?;
        for (k, v) in patch {
            if v.is_null() {
                d.fields.remove(k);
            } else {
                d.fields.insert(k.clone(), v.clone());
            }
        }
        // `updated` never runs backwards, even if the clock does.
        let updated = self.env.now_ms().max(d.updated);
        let bytes = record::encode(&schema, &d.fields, d.created, updated, &d.author)?;
        self.write(vec![(key, Edit::Put(bytes.clone()))])?;
        self.read(&schema, rkey, &bytes)
    }

    pub fn get(&self, domain: &str, rkey: &RKey) -> Result<Option<Record>> {
        let schema = self.need_schema(domain)?;
        match self.get_key(&record_key(domain, rkey))? {
            Some(b) => self.read(&schema, rkey, &b).map(Some),
            None => Ok(None),
        }
    }

    pub fn delete(&mut self, domain: &str, rkey: &RKey) -> Result<bool> {
        check_domain(domain)?;
        let key = record_key(domain, rkey);
        let existed = self.get_key(&key)?.is_some();
        self.write(vec![(key, Edit::Delete)])?;
        Ok(existed)
    }

    pub fn scan(&self, domain: &str, opts: Scan) -> Result<Vec<Record>> {
        let schema = self.need_schema(domain)?;
        let p = prefix(domain);
        let (mut lo, mut hi) = (p.clone(), upper(&p));
        if let Some(a) = opts.after {
            let k = record_key(domain, &a);
            if opts.reverse {
                hi = k;
            } else {
                lo = [k, vec![0]].concat(); // the next possible key
            }
        }
        let limit = if opts.limit == 0 {
            usize::MAX
        } else {
            opts.limit
        };
        self.scan_keys(&lo, &hi, opts.reverse, limit)?
            .iter()
            .map(|(k, v)| {
                let rkey: RKey = k[p.len()..]
                    .try_into()
                    .map_err(|_| "corrupt record key".to_string())?;
                self.read(&schema, &rkey, v)
            })
            .collect()
    }

    pub fn count(&self, domain: &str) -> Result<usize> {
        check_domain(domain)?;
        let p = prefix(domain);
        Ok(self.scan_keys(&p, &upper(&p), false, usize::MAX)?.len())
    }

    /// A store read, with the store's own refusal turned into `Db`'s error.
    ///
    /// Every `&self` read in `Db` goes through these two, so there is one
    /// place where a store that cannot answer synchronously becomes something
    /// an app can catch rather than something that kills the instance.
    fn get_key(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.store.get(key).map_err(|e| e.to_string())
    }

    fn scan_keys(
        &self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.store
            .scan(lo, hi, reverse, limit)
            .map_err(|e| e.to_string())
    }

    fn read(&self, schema: &Schema, rkey: &RKey, bytes: &[u8]) -> Result<Record> {
        let d = record::decode(schema, bytes)?;
        Ok(Record {
            id: id::to_hex(rkey),
            created: d.created,
            updated: d.updated,
            fields: d.fields,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    /// The key screen guards DERIVED keys. Nothing an app can write today
    /// produces one — a record key is a 1-byte tag, a domain of at most 32, a
    /// separator and a 16-byte id — but index terms will be built from a
    /// record's contents (the first 64 B of a term plus a hash of the rest), so
    /// an app will be able to produce a long one without ever writing it. The
    /// screen is tested where it lives, at the funnel every write goes through.
    #[test]
    fn a_key_past_the_trees_limit_is_refused_before_the_store_is_touched() {
        struct Panics;
        impl Store for Panics {
            fn get(&self, _: &[u8]) -> crate::store::Read<Option<Vec<u8>>> {
                Ok(None)
            }
            fn put(&mut self, _: &[u8], _: &[u8]) {
                panic!("the store must not be reached")
            }
            fn delete(&mut self, _: &[u8]) -> bool {
                panic!("the store must not be reached")
            }
            fn scan(
                &self,
                _: &[u8],
                _: &[u8],
                _: bool,
                _: usize,
            ) -> crate::store::Read<Vec<(Vec<u8>, Vec<u8>)>> {
                Ok(Vec::new())
            }
            fn apply_batch(&mut self, _: &[(Vec<u8>, Edit)]) {
                panic!("the store must not be reached")
            }
        }
        let mut d = Db::new(Panics, crate::id::SystemEnv, *b"dev1");
        let long = vec![b'k'; MAX_KEY + 1];
        let e = d.write(vec![(long, Edit::Put(b"v".to_vec()))]).unwrap_err();
        assert!(e.contains(&MAX_KEY.to_string()), "{e}");
        // The longest legal key is accepted — so the refusal is the length and
        // not the screen refusing everything.
        let mut d = Db::new(MemStore::default(), crate::id::SystemEnv, *b"dev1");
        d.write(vec![(vec![b'k'; MAX_KEY], Edit::Put(b"v".to_vec()))])
            .unwrap();
    }
}
