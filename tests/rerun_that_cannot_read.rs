//! THE RE-RUN'S BOUND (sdk#143/#144, re-planted for READ-STATE R-a): a
//! conflicted update is re-run on the new base, and to re-run it must READ
//! the record again. When that read cannot be answered — its walk stops on a
//! block, and the ticket ends NOT_ANSWERING or UNAVAILABLE, or never ends —
//! the chain does not wait for ever: inside the bound it waits (nothing told,
//! nothing written); at the bound the write fails NAMED, once.
//!
//! The bound is the one the page passes (`Session::tick` →
//! `Db::rerun(now, TICKET_LIFE_MS)`): the ticket's own lifetime. This was
//! `page/tests/server_differential.rs`'s
//! `a_re_run_whose_record_never_loads_fails_at_the_load_budget`, whose
//! premise (a Conflict FORGOT the record in the copy) went with the copy.

use craftworks_sdk::page_store::TICKET_LIFE_MS;
use craftworks_sdk::store::{ConflictChain, Delta, Edit, Read, Reads, Store, StoreError};
use craftworks_sdk::{Db, Env, RerunEvent};
use testkit::MemStore;
use serde_json::{json, Map, Value};

struct FakeEnv(u64);
impl Env for FakeEnv {
    fn now_ms(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
    fn rand32(&mut self) -> u32 {
        7
    }
}

/// A store that numbers its writes, can be told a write CONFLICTED, and can
/// stop answering reads — the walk that stops and whose ticket never loads.
#[derive(Default)]
struct Parking {
    inner: MemStore,
    next: u64,
    pending: Vec<u64>,
    chains: Vec<ConflictChain>,
    refuse_reads: bool,
    writes: u64,
}
impl Store for Parking {
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) -> Result<(), craftworks_sdk::Refused> {
        self.next += 1;
        self.writes += 1;
        self.pending.push(self.next);
        self.inner.apply_batch(edits)
    }
    fn last_write_id(&self) -> Option<u64> {
        (self.next > 0).then_some(self.next)
    }
    fn is_pending_write(&self, id: u64) -> bool {
        self.pending.contains(&id)
    }
    fn take_conflict_chains(&mut self) -> Vec<ConflictChain> {
        std::mem::take(&mut self.chains)
    }
}
impl Reads for Parking {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        if self.refuse_reads {
            return Err(StoreError::NotLoaded);
        }
        self.inner.get(key)
    }
    fn scan(&mut self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        if self.refuse_reads {
            return Err(StoreError::NotLoaded);
        }
        self.inner.scan(lo, hi, reverse, limit)
    }
    fn root(&mut self) -> Read<[u8; 32]> {
        self.inner.root()
    }
    fn changes_since(&mut self, f: [u8; 32], lo: &[u8], hi: &[u8], m: u32) -> Read<Delta> {
        self.inner.changes_since(f, lo, hi, m)
    }
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().expect("an object").clone()
}

#[test]
fn a_re_run_whose_read_never_answers_fails_named_at_the_tickets_lifetime() {
    let mut db = Db::new(Parking::default(), FakeEnv(1_000), [1, 2, 3, 4]);
    let schema = serde_json::from_value(json!({ "type": "T", "fields": [{ "name": "title", "kind": "text", "required": true }, { "name": "body", "kind": "text" }] })).expect("a schema");
    db.define("t", &schema).expect("define");
    let rec = db.put("t", &obj(json!({ "title": "t0", "body": "b0" }))).expect("put");
    let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
    db.update("t", loc, &obj(json!({ "body": "b1" }))).expect("the update is made");
    let w = db.store().last_write_id().expect("numbered");
    // The engine says it CONFLICTED, and from now on the re-run's reads stop.
    db.store_mut().chains.push(ConflictChain { write_ids: vec![w], keys: Vec::new(), tries: 0 });
    db.store_mut().refuse_reads = true;
    let writes = db.store().writes;

    let t0 = 5_000;
    let first = db.rerun(t0, TICKET_LIFE_MS);
    assert!(!first.load.is_empty() && first.events.is_empty(), "the re-run did not wait for its read: {first:?}");
    let inside = db.rerun(t0 + TICKET_LIFE_MS - 1, TICKET_LIFE_MS);
    assert!(!inside.load.is_empty() && inside.events.is_empty(), "gave up inside the bound: {inside:?}");
    let past = db.rerun(t0 + TICKET_LIFE_MS, TICKET_LIFE_MS);
    assert!(
        matches!(past.events.as_slice(), [RerunEvent::Failed { write_id, reason }] if *write_id == w && reason.contains("could not be read")),
        "the re-run did not end NAMED at the bound: {past:?}"
    );
    assert!(db.rerun(t0 + TICKET_LIFE_MS + 1, TICKET_LIFE_MS).events.is_empty(), "told twice");
    assert_eq!(db.store().writes, writes, "a re-run was written without its record");
}

/// THE CONTROL: the same chain, the reads answering — the re-run is MADE
/// (one more write), and nothing fails.
#[test]
fn control_a_re_run_that_can_read_is_made() {
    let mut db = Db::new(Parking::default(), FakeEnv(1_000), [1, 2, 3, 4]);
    let schema = serde_json::from_value(json!({ "type": "T", "fields": [{ "name": "title", "kind": "text", "required": true }, { "name": "body", "kind": "text" }] })).expect("a schema");
    db.define("t", &schema).expect("define");
    let rec = db.put("t", &obj(json!({ "title": "t0", "body": "b0" }))).expect("put");
    let loc = craftworks_sdk::id::loc_from_hex(&rec.id).expect("an id");
    db.update("t", loc, &obj(json!({ "body": "b1" }))).expect("the update is made");
    let w = db.store().last_write_id().expect("numbered");
    // A CONFLICT applied nothing: the tree holds the other side's record —
    // their title, and the body as it was before this write.
    let key = craftworks_sdk::db::record_key("t", loc);
    let now = db.store_mut().inner.get(&key).expect("read").expect("present");
    let mut d = craftworks_sdk::record::decode(&schema, &now).expect("decodes");
    d.fields.insert("title".into(), json!("t1"));
    d.fields.insert("body".into(), json!("b0"));
    let theirs = craftworks_sdk::record::encode(&schema, &d.fields, d.created, d.updated + 1, &d.author).expect("encodes");
    db.store_mut().inner.apply_batch(&[(key.clone(), Edit::Put(theirs))]).expect("theirs");
    db.store_mut().chains.push(ConflictChain { write_ids: vec![w], keys: Vec::new(), tries: 0 });
    let writes = db.store().writes;
    let step = db.rerun(5_000, TICKET_LIFE_MS);
    assert!(step.events.is_empty() && step.load.is_empty(), "{step:?}");
    assert_eq!(db.store().writes, writes + 1, "the re-run was not made");
    let got = craftworks_sdk::record::decode(&schema, &db.store_mut().inner.get(&key).expect("read").expect("present")).expect("decodes").fields;
    assert_eq!((got.get("title"), got.get("body")), (Some(&json!("t1")), Some(&json!("b1"))), "the re-run did not land on theirs");
}
