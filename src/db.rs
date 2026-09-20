//! `Db`: collections of typed records over a [`Store`].
//!
//! Keys follow the architecture's keyspace (§5):
//! `0x01 ‖ domain ‖ 0x00 ‖ rkey(16)` for records, `0x00 "schema" 0x00 domain` for
//! a domain's schema — so the schema travels with the data it types.

use crate::id::{self, Env, IdGen, RKey};
use crate::record;
use crate::schema::Schema;
use crate::store::{sorted_edits, Edit, Reads, Store, StoreError};
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
    /// What THIS record's own write is doing.
    ///
    /// Metadata on the row, never a field: it describes the record rather
    /// than being part of it, and a schema that had to declare it would make
    /// every app's data shape depend on how it happened to be stored.
    ///
    /// Over the in-memory store this is always `Clean`, which is the truth —
    /// a write there is applied the moment it is made, so there is never one
    /// in flight to report.
    #[serde(rename = "state", serialize_with = "ser_state")]
    pub state: crate::store::RowState,
}

fn ser_state<S: serde::Serializer>(
    s: &crate::store::RowState,
    ser: S,
) -> std::result::Result<S::Ok, S::Error> {
    // The stable CODE, never the Rust variant name: a row branches on this,
    // and a rename would change behaviour silently.
    ser.serialize_str(s.code())
}

#[derive(Default, Clone, Copy)]
pub struct Scan {
    pub reverse: bool,
    /// 0 = no limit.
    pub limit: usize,
    /// Resume strictly after this record (in scan direction).
    pub after: Option<RKey>,
}

pub struct Db<S: Store + Reads, E: Env> {
    store: S,
    env: E,
    ids: IdGen,
    /// Signing identity. All zero until identities land (phase 3).
    author: [u8; 32],
}

/// Why a `Db` call did not answer.
///
/// **`NotLoaded` is a variant and not a message.** It is the one error an app
/// RECOVERS from rather than reports: the range has not been read yet, it is
/// not known to be empty, and the fix is a reload. Every other refusal means
/// the caller asked for something it may not have — a domain that is not
/// defined, a key over the limit, a record that will not decode — and no
/// reload changes that.
///
/// It was a `String`. A browser app would have had to match on the text of a
/// message to tell "read me again" from "you are wrong", which breaks the
/// first time anybody rewords it, and silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// The store has not loaded the range this read needed. **Not empty.**
    ///
    /// Carries the SPAN the read actually needed, because the caller's whole
    /// recovery is to load it — and an error that says "something was not
    /// loaded" without saying what leaves the caller to guess a range, which
    /// is how a recovery becomes either a spin or a full download.
    ///
    /// The span is the read's OWN span and nothing wider: `[key, key+1)` for
    /// a get, `[lo, hi)` for a scan. Widening it to the whole domain would
    /// make one `get` fetch every record in it, which is the page-freezing
    /// version of being helpful; the loaded-interval bookkeeping means a
    /// later, wider read asks for whatever is still missing.
    NotLoaded { lo: Vec<u8>, hi: Vec<u8> },
    /// The store could not reach a block this read needed. The key may well
    /// exist — this is not an absence either.
    Unavailable,
    /// A key or a record is over the store's limit. The caller asked for
    /// something that will never fit; no reload helps.
    TooLarge(String),
    /// A domain that is not defined, or a name that is not a domain.
    NotDefined(String),
    /// Anything else the caller could have got right, in words.
    Refused(String),
}

impl DbError {
    /// A STABLE code, from a fixed list, for a caller to branch on.
    ///
    /// This is what crosses the JavaScript boundary. An app must be able to
    /// tell "read me again" from "you are wrong" — and the only alternative
    /// to a code is matching on the text of a message, which breaks the first
    /// time anybody rewords it, and breaks silently. Nothing downstream, in
    /// `wrap.js` or in an app, ever branches on the message.
    ///
    /// The list is fixed and exhaustive: adding a variant has to add a code
    /// here, and the compiler says so.
    pub fn code(&self) -> &'static str {
        match self {
            DbError::NotLoaded { .. } => "NOT_LOADED",
            DbError::Unavailable => "UNAVAILABLE",
            DbError::TooLarge(_) => "TOO_LARGE",
            DbError::NotDefined(_) => "NOT_DEFINED",
            DbError::Refused(_) => "REFUSED",
        }
    }

    /// Whether a RELOAD is the recovery. The only two that a reload can fix
    /// are the two that are about reaching data rather than about the ask.
    pub fn is_transient(&self) -> bool {
        matches!(self, DbError::NotLoaded { .. } | DbError::Unavailable)
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::NotLoaded { .. } => f.write_str(
                "this range has not been loaded yet — reload it; it is not known to be empty",
            ),
            DbError::Unavailable => f.write_str(
                "the engine could not reach a block this read needed; the key may well exist",
            ),
            DbError::TooLarge(m) | DbError::NotDefined(m) | DbError::Refused(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DbError {}

/// So every `?` over a `String` error in this module keeps working, and only
/// the two store-read funnels have to say which variant they mean.
impl From<String> for DbError {
    fn from(m: String) -> Self {
        DbError::Refused(m)
    }
}

impl From<&str> for DbError {
    fn from(m: &str) -> Self {
        DbError::Refused(m.to_string())
    }
}

impl DbError {
    /// A store's refusal, told which span the read was for.
    ///
    /// Kept exhaustive so a new `StoreError` has to be classified here rather
    /// than falling into `Refused` and quietly becoming un-recoverable.
    fn from_store(e: StoreError, lo: &[u8], hi: &[u8]) -> Self {
        match e {
            StoreError::NotLoaded => DbError::NotLoaded {
                lo: lo.to_vec(),
                hi: hi.to_vec(),
            },
            StoreError::Unavailable => DbError::Unavailable,
            StoreError::NoAnswer => DbError::Refused(e.to_string()),
        }
    }

    /// The span a `NotLoaded` needs loaded, if that is what this is.
    pub fn needs(&self) -> Option<(&[u8], &[u8])> {
        match self {
            DbError::NotLoaded { lo, hi } => Some((lo, hi)),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, DbError>;

fn check_domain(d: &str) -> Result<()> {
    let ok = !d.is_empty()
        && d.len() <= MAX_DOMAIN
        && d.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(DbError::NotDefined(format!(
            "domain `{d}` must be 1–{MAX_DOMAIN} of a-z 0-9 _ - ."
        )))
    }
}

/// The smallest key strictly greater than `k`.
fn succ(k: &[u8]) -> Vec<u8> {
    let mut out = k.to_vec();
    out.push(0);
    out
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

impl<S: Store + Reads, E: Env> Db<S, E> {
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

    /// The store, mutably. Reads take `&mut self`, so a test or a tool that
    /// wants to look underneath needs this rather than the shared borrow.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
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
                return Err(DbError::TooLarge(format!(
                    "key is {} bytes and the limit is {MAX_KEY}",
                    key.len()
                )));
            }
            if let Edit::Put(v) = edit {
                if v.len() > MAX_VALUE {
                    return Err(DbError::TooLarge(format!(
                        "record is {} bytes and the limit is {MAX_VALUE}; \
                         store content this large as a file or a blob and keep a \
                         reference to it in the record",
                        v.len()
                    )));
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
    pub fn schema(&mut self, domain: &str) -> Result<Option<Schema>> {
        match self.get_key(&schema_key(domain))? {
            Some(b) => Ok(serde_json::from_slice(&b).ok()),
            None => Ok(None),
        }
    }

    fn need_schema(&mut self, domain: &str) -> Result<Schema> {
        check_domain(domain)?;
        self.schema(domain)?.ok_or_else(|| {
            DbError::NotDefined(format!("domain `{domain}` has no schema; define it first"))
        })
    }

    pub fn domains(&mut self) -> Result<Vec<String>> {
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
        let key = record_key(domain, &rkey);
        self.write(vec![(key.clone(), Edit::Put(bytes.clone()))])?;
        self.read(&schema, &rkey, &key, &bytes)
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
        self.write(vec![(key.clone(), Edit::Put(bytes.clone()))])?;
        self.read(&schema, rkey, &key, &bytes)
    }

    pub fn get(&mut self, domain: &str, rkey: &RKey) -> Result<Option<Record>> {
        let schema = self.need_schema(domain)?;
        let key = record_key(domain, rkey);
        match self.get_key(&key)? {
            Some(b) => self.read(&schema, rkey, &key, &b).map(Some),
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

    pub fn scan(&mut self, domain: &str, opts: Scan) -> Result<Vec<Record>> {
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
                self.read(&schema, &rkey, k, v)
            })
            .collect()
    }

    pub fn count(&mut self, domain: &str) -> Result<usize> {
        check_domain(domain)?;
        let p = prefix(domain);
        Ok(self.scan_keys(&p, &upper(&p), false, usize::MAX)?.len())
    }

    /// A store read, with the store's own refusal turned into `Db`'s error.
    ///
    /// Every `&self` read in `Db` goes through these two, so there is one
    /// place where a store that cannot answer synchronously becomes something
    /// an app can catch rather than something that kills the instance.
    fn get_key(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // The span for a single key is that key alone. `succ` is the
        // smallest key strictly after it, so `[key, succ)` contains exactly
        // one key and the loaded-interval bookkeeping stays exact.
        self.store
            .get(key)
            .map_err(|e| DbError::from_store(e, key, &succ(key)))
    }

    fn scan_keys(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.store
            .scan(lo, hi, reverse, limit)
            .map_err(|e| DbError::from_store(e, lo, hi))
    }

    /// Build a record, asking the store what this row's own write is doing.
    ///
    /// The KEY is passed rather than recomputed from the domain, so a caller
    /// cannot ask about a different row than the one it decoded.
    fn read(&self, schema: &Schema, rkey: &RKey, key: &[u8], bytes: &[u8]) -> Result<Record> {
        let d = record::decode(schema, bytes)?;
        Ok(Record {
            id: id::to_hex(rkey),
            created: d.created,
            updated: d.updated,
            fields: d.fields,
            state: self.store.row_state(key),
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
            fn put(&mut self, _: &[u8], _: &[u8]) {
                panic!("the store must not be reached")
            }
            fn delete(&mut self, _: &[u8]) -> bool {
                panic!("the store must not be reached")
            }
            fn apply_batch(&mut self, _: &[(Vec<u8>, Edit)]) {
                panic!("the store must not be reached")
            }
        }
        impl crate::store::Reads for Panics {
            fn get(&mut self, _: &[u8]) -> crate::store::Read<Option<Vec<u8>>> {
                Ok(None)
            }
            fn scan(
                &mut self,
                _: &[u8],
                _: &[u8],
                _: bool,
                _: usize,
            ) -> crate::store::Read<Vec<(Vec<u8>, Vec<u8>)>> {
                Ok(Vec::new())
            }
            fn root(&mut self) -> crate::store::Read<[u8; 32]> {
                Ok([0u8; 32])
            }
            fn changes_since(
                &mut self,
                _: [u8; 32],
                _: &[u8],
                _: &[u8],
                _: u32,
            ) -> crate::store::Read<crate::store::Delta> {
                Ok(crate::store::Delta::FullReloadRequired {
                    new_root: [0u8; 32],
                })
            }
        }

        let mut d = Db::new(Panics, crate::id::SystemEnv, *b"dev1");
        let long = vec![b'k'; MAX_KEY + 1];
        let e = d.write(vec![(long, Edit::Put(b"v".to_vec()))]).unwrap_err();
        assert_eq!(
            e.code(),
            "TOO_LARGE",
            "a key over the limit must carry the code an app branches on, not only a message"
        );
        assert!(e.to_string().contains(&MAX_KEY.to_string()), "{e}");
        // The longest legal key is accepted — so the refusal is the length and
        // not the screen refusing everything.
        let mut d = Db::new(MemStore::default(), crate::id::SystemEnv, *b"dev1");
        d.write(vec![(vec![b'k'; MAX_KEY], Edit::Put(b"v".to_vec()))])
            .unwrap();
    }
}
