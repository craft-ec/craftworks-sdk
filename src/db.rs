//! `Db`: collections of typed records over a [`Store`].
//!
//! Keys follow the architecture's keyspace (§5):
//! `0x01 ‖ domain ‖ 0x00 ‖ rkey(16)` for records, `0x00 "schema" 0x00 domain` for
//! a domain's schema — so the schema travels with the data it types.

use crate::id::{self, Env, IdGen, Loc, RKey};
use crate::record;
use crate::schema::Schema;
use crate::store::{sorted_edits, Edit, IdWidth, Reads, Store, StoreError};
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

/// What [`Db::create_at`] did: made the record, or found one already there.
///
/// An outcome and not an error, because both answers are ordinary — a second
/// handoff of the same row is EXPECTED to find its copy — and both carry the
/// record. What `Exists` never means is "overwritten": the held record comes
/// back exactly as it is stored, and deciding whether to update it is the
/// caller's, which is the only one that knows what the copy was copied from.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "outcome", content = "record", rename_all = "lowercase")]
pub enum CreateAt {
    Created(Record),
    Exists(Record),
}

/// How far ahead of this device's clock a slot's time may be.
///
/// A slot's first 8 bytes ARE the record's `created`, and every newest-first
/// list sorts by them — so a slot dated in the future would sit at the top of
/// every such list until that time passed, for ever if it is far enough out.
/// The allowance is for two devices' clocks disagreeing, not for a date the
/// caller chose: a policy value, five minutes, not a measured one.
pub const SLOT_SKEW_MS: u64 = 5 * 60 * 1000;

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
/// `T_RECORD | domain | 0 | [parent] | rkey`.
///
/// The parent, when there is one, sits BEFORE the rkey, which is the whole
/// point: it makes every child of one parent a contiguous band, so reading
/// them is a bounded prefix scan instead of a scan of the domain filtered by
/// a field (craftworks-sdk#122).
pub fn record_key(domain: &str, at: impl Into<Loc>) -> Vec<u8> {
    let loc = at.into();
    let mut k = prefix(domain);
    if let Some(parent) = &loc.parent {
        k.extend_from_slice(parent);
    }
    k.extend_from_slice(&loc.rkey);
    k
}

/// Every key under `parent` in `domain`, as a half-open range.
fn parent_span(domain: &str, parent: &RKey) -> (Vec<u8>, Vec<u8>) {
    let mut lo = prefix(domain);
    lo.extend_from_slice(parent);
    // The band is exactly the 16-byte rkeys following this prefix, so the end
    // is the prefix with a 16-byte all-ones tail exceeded -- simplest correct
    // form is to bump the prefix itself.
    let mut hi = lo.clone();
    for b in hi.iter_mut().rev() {
        if *b == 0xff { *b = 0; } else { *b += 1; break; }
    }
    (lo, hi)
}
fn schema_key(domain: &str) -> Vec<u8> {
    let mut k = vec![T_SYSTEM];
    k.extend_from_slice(b"schema\0");
    k.extend_from_slice(domain.as_bytes());
    k
}
/// Split a stored key back into the parent (if the domain has one) and rkey.
fn loc_of_key(schema: &Schema, p: &[u8], k: &[u8]) -> Result<Loc> {
    let tail = &k[p.len()..];
    let want = if schema.parent.is_some() { 32 } else { 16 };
    if tail.len() != want {
        return Err(DbError::from("corrupt record key".to_string()));
    }
    Ok(if schema.parent.is_some() {
        Loc::under(tail[..16].try_into().unwrap(), tail[16..].try_into().unwrap())
    } else {
        Loc::bare(tail.try_into().unwrap())
    })
}

/// The parent a record declares, read from the field the schema names.
fn parent_of(schema: &Schema, fields: &Map<String, Value>) -> Result<Option<RKey>> {
    let Some(pf) = &schema.parent else { return Ok(None) };
    let v = fields.get(pf).and_then(|v| v.as_str()).ok_or_else(|| {
        DbError::Refused(format!("`{pf}` names this record's parent and is required"))
    })?;
    id::from_hex(v).map(Some).ok_or_else(|| {
        DbError::Refused(format!(
            "`{pf}` must be a record id (32 hex characters); got `{v}`"
        ))
    })
}

/// The smallest key greater than every key starting with `p` (p ends in 0x00).
fn upper(p: &[u8]) -> Vec<u8> {
    let mut h = p.to_vec();
    *h.last_mut().unwrap() += 1;
    h
}

impl<S: Store + Reads, E: Env> Db<S, E> {
    /// Every key a domain's records live under, as `[lo, hi)`.
    ///
    /// The ONE place that turns a name into a range. A caller that built this
    /// itself would be encoding the key layout — which is the thing this
    /// boundary exists to hide, and which could then never change without
    /// breaking every caller that had hard-coded it.
    ///
    /// It takes no `&self` because it is a property of the layout and not of
    /// any particular database, and it does not check that the domain is
    /// DEFINED: that is a question about this database's contents, and the
    /// caller that needs it asks separately.
    /// Every key one PARENT's children live under, in a domain that declares
    /// a parent, as `[lo, hi)` — the band `Db::children` reads.
    pub fn parent_range(domain: &str, parent: &RKey) -> (Vec<u8>, Vec<u8>) {
        parent_span(domain, parent)
    }

    /// What a session WATCHES for one binding: the whole domain, or one
    /// parent's band of it (craftworks-sdk#137). A string, so it can cross to
    /// JavaScript and back as an opaque name; its layout is known only here
    /// and in [`Db::watch_range`]. `#` cannot occur in a domain name.
    pub fn watch_key(domain: &str, parent: Option<&RKey>) -> String {
        match parent {
            Some(p) => format!("{domain}#{}", id::to_hex(p)),
            None => domain.to_string(),
        }
    }

    /// The watch key whose range is EXACTLY `[lo, hi)`: a whole domain, or
    /// one parent's band — so a completed load of either establishes the root
    /// a live binding of it asks deltas from (sdk#142 for domains, sdk#137 for
    /// bands). A span that is neither names nothing.
    pub fn watch_key_of_range(lo: &[u8], hi: &[u8]) -> Option<String> {
        if let Some(d) = Self::domain_of_range(lo, hi) {
            return Some(d);
        }
        let rest = lo.strip_prefix(&[T_RECORD])?;
        let z = rest.iter().position(|b| *b == 0)?;
        let domain = std::str::from_utf8(&rest[..z]).ok()?;
        check_domain(domain).ok()?;
        let parent: RKey = rest[z + 1..].try_into().ok()?;
        (Self::parent_range(domain, &parent) == (lo.to_vec(), hi.to_vec()))
            .then(|| Self::watch_key(domain, Some(&parent)))
    }

    /// The range a [`Db::watch_key`] names, or `None` for a string that is
    /// not one.
    pub fn watch_range(key: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        match key.split_once('#') {
            Some((domain, parent)) => {
                check_domain(domain).ok()?;
                Some(Self::parent_range(domain, &id::from_hex(parent)?))
            }
            None => {
                check_domain(key).ok()?;
                Some(Self::domain_range(key))
            }
        }
    }

    pub fn domain_range(domain: &str) -> (Vec<u8>, Vec<u8>) {
        let lo = prefix(domain);
        let hi = upper(&lo);
        (lo, hi)
    }

    /// The domain whose WHOLE range is exactly `[lo, hi)`, if any — the
    /// inverse of [`Db::domain_range`], kept beside it so the layout is
    /// still known in one place. A narrower or wider span names no domain.
    pub fn domain_of_range(lo: &[u8], hi: &[u8]) -> Option<String> {
        let name = lo.strip_prefix(&[T_RECORD])?.strip_suffix(&[0])?;
        let domain = std::str::from_utf8(name).ok()?;
        check_domain(domain).ok()?;
        (Self::domain_range(domain) == (lo.to_vec(), hi.to_vec())).then(|| domain.to_string())
    }

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
        // The rkey stays SDK-minted. The caller chooses only the PARENT, which
        // is what keeps an app from encoding the key layout -- it names an
        // intent, and the SDK still owns what a key is.
        let loc = Loc { parent: parent_of(&schema, fields)?, rkey };
        let now = id::created_ms(&rkey);
        let bytes = record::encode(&schema, fields, now, now, &self.author)?;
        let key = record_key(domain, loc);
        self.write(vec![(key.clone(), Edit::Put(bytes.clone()))])?;
        self.read(&schema, &loc, &key, &bytes)
    }

    /// Create a record at a key THE CALLER chose, or find the one already there.
    ///
    /// `put` mints the rkey, so writing "the same record" twice writes two.
    /// A copy whose key is a FUNCTION of its source — [`id::slot_from`] —
    /// lands on the same key however many times, from however many tabs, it
    /// is made, and nothing has to remember that it was (craftworks-sdk#149).
    ///
    /// **A create, never an overwrite.** If a record is already stored at the
    /// slot, it is returned as [`CreateAt::Exists`] and the tree is not
    /// touched.
    ///
    /// **Not loaded is not absent.** The slot is READ before anything is
    /// decided, and a read that could not be answered is an error, exactly as
    /// it is for `get`. A fresh session holds nothing; answering "absent"
    /// there would write Preview's copy over a published record — including
    /// one edited since it was published. The recovery is the usual one: load
    /// and ask again, which repeats the attempt and not the effect, because
    /// nothing is written before the read answers.
    ///
    /// The slot's first 8 bytes are the record's `created`, as a minted id's
    /// are — so a copy keeps its source's creation time and sorts among the
    /// minted records by it. That makes `created` the caller's to choose, so a
    /// time more than [`SLOT_SKEW_MS`] ahead of this clock is refused. The
    /// record's `updated` is NOW: it was written now, whatever it copies.
    ///
    /// The parent, in a domain that has one, comes from the fields as it does
    /// for `put`; the slot is the rkey under it.
    ///
    /// Stated residual, until the write can carry `expected: Absent`
    /// (craftworks-sdk#148 step 1): two sessions that both read the slot
    /// absent both write it, and the second write wins. There is still at
    /// most ONE record at the slot; which content it holds is the later's.
    pub fn create_at(
        &mut self,
        domain: &str,
        slot: RKey,
        fields: &Map<String, Value>,
    ) -> Result<CreateAt> {
        let schema = self.need_schema(domain)?;
        let loc = Loc { parent: parent_of(&schema, fields)?, rkey: slot };
        let key = record_key(domain, loc);
        if let Some(held) = self.get_key(&key)? {
            return self.read(&schema, &loc, &key, &held).map(CreateAt::Exists);
        }
        let now = self.env.now_ms();
        let created = id::created_ms(&slot);
        if created > now.saturating_add(SLOT_SKEW_MS) {
            return Err(DbError::Refused(format!(
                "this slot is dated {}ms ahead of this device's clock; a record's \
                 creation time may not be more than {SLOT_SKEW_MS}ms in the future",
                created - now
            )));
        }
        let bytes = record::encode(&schema, fields, created, now.max(created), &self.author)?;
        self.write(vec![(key.clone(), Edit::Put(bytes.clone()))])?;
        self.read(&schema, &loc, &key, &bytes).map(CreateAt::Created)
    }

    /// Merge `patch` over the record; a `null` value removes that field.
    pub fn update(
        &mut self,
        domain: &str,
        at: impl Into<Loc>,
        patch: &Map<String, Value>,
    ) -> Result<Record> {
        let schema = self.need_schema(domain)?;
        let loc = self.locate(&schema, domain, at)?;
        let key = record_key(domain, loc);
        let old = self.get_key(&key)?.ok_or("no such record")?;
        let mut d = record::decode(&schema, &old)?;
        // The parent decides the KEY, so a patch that moved it would leave the
        // record filed under its old parent while claiming the new one. A
        // re-parent is a delete and a write, and it is not this call.
        if let Some(pf) = &schema.parent {
            if let Some(v) = patch.get(pf) {
                if v != d.fields.get(pf).unwrap_or(&Value::Null) {
                    return Err(DbError::Refused(format!(
                        "`{pf}` is this domain's parent and decides the record's \
                         key; changing it here would leave the record filed under \
                         the old one"
                    )));
                }
            }
        }
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
        self.read(&schema, &loc, &key, &bytes)
    }

    /// A record, or `None`.
    ///
    /// **An id this domain cannot address answers `None` rather than
    /// refusing.** The ids reaching a read come from OUTSIDE the program — a
    /// value in browser storage written by an older build, a pasted link, a
    /// record deleted on another device — and for every one of those the
    /// honest answer is *absent*, which this already has a representation for.
    /// Making "malformed" a different CONTROL-FLOW outcome from "missing"
    /// forces every caller that touches an outside id into a `try`, and the
    /// one that forgets is the one that crashes (craftworks-sdk#118).
    ///
    /// This matters more since #122, not less: a component id stored before
    /// that change is 32 hex where the domain now wants 64, so the stale-id
    /// case is no longer hypothetical.
    ///
    /// A WRITE still refuses, and that asymmetry is deliberate: answering
    /// "nothing to do" to a delete or an update aimed at an id nobody can
    /// resolve would hide the caller's mistake instead of reporting it.
    pub fn get(&mut self, domain: &str, at: impl Into<Loc>) -> Result<Option<Record>> {
        let schema = self.need_schema(domain)?;
        let at: Loc = at.into();
        let Ok(loc) = self.locate(&schema, domain, at) else {
            // Well-formed (it parsed) and the wrong WIDTH for this domain: the
            // one case the answer "not found" would otherwise leave silent.
            self.store.wrong_width(IdWidth::of(at.parent.is_some()), IdWidth::of(schema.parent.is_some()));
            return Ok(None);
        };
        let key = record_key(domain, loc);
        match self.get_key(&key)? {
            Some(b) => self.read(&schema, &loc, &key, &b).map(Some),
            None => Ok(None),
        }
    }

    pub fn delete(&mut self, domain: &str, at: impl Into<Loc>) -> Result<bool> {
        check_domain(domain)?;
        let schema = self.need_schema(domain)?;
        let loc = self.locate(&schema, domain, at)?;
        let key = record_key(domain, loc);
        let existed = self.get_key(&key)?.is_some();
        self.write(vec![(key, Edit::Delete)])?;
        Ok(existed)
    }

    pub fn scan(&mut self, domain: &str, opts: Scan) -> Result<Vec<Record>> {
        let schema = self.need_schema(domain)?;
        let p = prefix(domain);
        let (mut lo, mut hi) = (p.clone(), upper(&p));
        if let Some(a) = opts.after {
            let k = record_key(domain, Loc::bare(a));
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
                let loc = loc_of_key(&schema, &p, k)?;
                self.read(&schema, &loc, k, v)
            })
            .collect()
    }

    /// THE CHILDREN OF ONE PARENT, as a bounded read.
    ///
    /// The app names the parent; it never builds a range. What the range IS
    /// stays the SDK's business, which is the same rule `Session::preload`
    /// keeps — a caller that built one would be encoding the key layout.
    ///
    /// The cost is the size of THIS parent's band. It does not grow with the
    /// number of records under other parents, which is the whole difference
    /// from the scan-and-filter this replaces (craftworks-sdk#122).
    pub fn children(&mut self, domain: &str, parent: &RKey, opts: Scan) -> Result<Vec<Record>> {
        let schema = self.need_schema(domain)?;
        let pf = schema.parent.clone().ok_or_else(|| {
            DbError::Refused(format!(
                "domain `{domain}` does not declare a parent, so it has no \
                 children to read: records in it are keyed by their own id alone"
            ))
        })?;
        let _ = pf;
        let p = prefix(domain);
        let (mut lo, mut hi) = parent_span(domain, parent);
        if let Some(a) = opts.after {
            let k = record_key(domain, Loc::under(*parent, a));
            if opts.reverse { hi = k; } else { lo = [k, vec![0]].concat(); }
        }
        let limit = if opts.limit == 0 { usize::MAX } else { opts.limit };
        self.scan_keys(&lo, &hi, opts.reverse, limit)?
            .iter()
            .map(|(k, v)| {
                let loc = loc_of_key(&schema, &p, k)?;
                self.read(&schema, &loc, k, v)
            })
            .collect()
    }

    /// Resolve what a caller handed us into a full location, or refuse.
    ///
    /// A bare rkey in a parent-keyed domain cannot address anything: the key
    /// needs the parent. A WRITE refuses here. A read does not reach this
    /// refusal — `get` turns it into `None`, because the bare ids that reach a
    /// read come from outside (a value stored before #122, a pasted link) and
    /// for those "absent" is the honest answer (craftworks-sdk#118).
    fn locate(&self, schema: &Schema, domain: &str, at: impl Into<Loc>) -> Result<Loc> {
        let loc = at.into();
        match (&schema.parent, &loc.parent) {
            (Some(pf), None) => Err(DbError::Refused(format!(
                "domain `{domain}` keys its records under `{pf}`, so it wants a \
                 64-hex id (parent then record); this one is 32 hex"
            ))),
            (None, Some(_)) => Err(DbError::Refused(format!(
                "domain `{domain}` does not key its records under a parent, so it \
                 wants a 32-hex id; this one is 64 hex"
            ))),
            _ => Ok(loc),
        }
    }

    /// How many records the domain holds — from the store's `count`, which a
    /// store with the tree answers in O(height) node reads (craftworks-sdk#123).
    /// It used to enumerate every key and value in the domain to return a
    /// number every node already carries.
    pub fn count(&mut self, domain: &str) -> Result<usize> {
        check_domain(domain)?;
        let p = prefix(domain);
        let hi = upper(&p);
        let n = self
            .store
            .count(&p, &hi)
            .map_err(|e| DbError::from_store(e, &p, &hi))?;
        Ok(n as usize)
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
    fn read(&self, schema: &Schema, loc: &Loc, key: &[u8], bytes: &[u8]) -> Result<Record> {
        let d = record::decode(schema, bytes)?;
        Ok(Record {
            id: id::loc_to_hex(loc),
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
