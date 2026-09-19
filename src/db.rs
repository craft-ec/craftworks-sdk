//! `Db`: collections of typed records over a [`Store`].
//!
//! Keys follow the architecture's keyspace (§5):
//! `0x01 ‖ domain ‖ 0x00 ‖ rkey(16)` for records, `0x00 "schema" 0x00 domain` for
//! a domain's schema — so the schema travels with the data it types.

use crate::id::{self, Env, IdGen, RKey};
use crate::record;
use crate::schema::Schema;
use crate::store::{sorted_edits, Edit, Store};
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
    fn write(&mut self, edits: Vec<(Vec<u8>, Edit)>) {
        self.store.apply_batch(&sorted_edits(edits));
    }

    pub fn env_mut(&mut self) -> &mut E {
        &mut self.env
    }

    /// Declare (or extend) a domain's schema. Extending may only append optional fields.
    pub fn define(&mut self, domain: &str, schema: &Schema) -> Result<()> {
        check_domain(domain)?;
        schema.check()?;
        if let Some(old) = self.schema(domain) {
            old.allows(schema)?;
        }
        let bytes = serde_json::to_vec(schema).map_err(|e| e.to_string())?;
        self.write(vec![(schema_key(domain), Edit::Put(bytes))]);
        Ok(())
    }

    pub fn schema(&self, domain: &str) -> Option<Schema> {
        serde_json::from_slice(&self.store.get(&schema_key(domain))?).ok()
    }

    fn need_schema(&self, domain: &str) -> Result<Schema> {
        check_domain(domain)?;
        self.schema(domain)
            .ok_or_else(|| format!("domain `{domain}` has no schema; define it first"))
    }

    pub fn domains(&self) -> Vec<String> {
        let lo = schema_key("");
        let mut hi = lo.clone();
        *hi.last_mut().unwrap() = 1;
        self.store
            .scan(&lo, &hi, false, usize::MAX)
            .into_iter()
            .filter_map(|(k, _)| String::from_utf8(k[lo.len()..].to_vec()).ok())
            .collect()
    }

    pub fn put(&mut self, domain: &str, fields: &Map<String, Value>) -> Result<Record> {
        let schema = self.need_schema(domain)?;
        let rkey = self.ids.next(&mut self.env);
        let now = id::created_ms(&rkey);
        let bytes = record::encode(&schema, fields, now, now, &self.author)?;
        self.write(vec![(record_key(domain, &rkey), Edit::Put(bytes.clone()))]);
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
        let old = self.store.get(&key).ok_or("no such record")?;
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
        self.write(vec![(key, Edit::Put(bytes.clone()))]);
        self.read(&schema, rkey, &bytes)
    }

    pub fn get(&self, domain: &str, rkey: &RKey) -> Result<Option<Record>> {
        let schema = self.need_schema(domain)?;
        match self.store.get(&record_key(domain, rkey)) {
            Some(b) => self.read(&schema, rkey, &b).map(Some),
            None => Ok(None),
        }
    }

    pub fn delete(&mut self, domain: &str, rkey: &RKey) -> Result<bool> {
        check_domain(domain)?;
        let key = record_key(domain, rkey);
        let existed = self.store.get(&key).is_some();
        self.write(vec![(key, Edit::Delete)]);
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
        self.store
            .scan(&lo, &hi, opts.reverse, limit)
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
        Ok(self.store.scan(&p, &upper(&p), false, usize::MAX).len())
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
