//! A record at a key THE CALLER derived, created once however often it is
//! asked for (craftworks-sdk#149).
//!
//! `put` mints the rkey, so copying "the same row" twice made two rows — and
//! the only way to stop that was bookkeeping about what had been copied, which
//! can be stale, half-written or raced. `create_at` makes the key a FUNCTION
//! of the source instead: a repeat lands on the same slot and finds its copy.
//!
//! What each test pins:
//!   - a second create is `Exists` and the tree root does not move;
//!   - two sessions on one node: exactly one record, the second told `Exists`;
//!   - NOT LOADED IS NOT ABSENT: a fresh session over a published record
//!     answers `NotLoaded` and writes nothing, and once loaded, `Exists`;
//!   - derived and minted keys scan together in creation-time order;
//!   - `slot_from` is a pinned vector;
//!   - a slot dated in the future is refused; `updated` is now.
use craftworks_sdk::id::{created_ms, loc_from_hex, to_hex};
use craftworks_sdk::*;
use serde_json::{json, Map, Value};

/// A clock the test sets.
struct At(std::rc::Rc<std::cell::Cell<u64>>);
impl Env for At {
    fn now_ms(&mut self) -> u64 {
        self.0.get()
    }
    fn rand32(&mut self) -> u32 {
        7
    }
}

fn clock(ms: u64) -> (At, std::rc::Rc<std::cell::Cell<u64>>) {
    let c = std::rc::Rc::new(std::cell::Cell::new(ms));
    (At(c.clone()), c)
}

fn fields(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn task() -> Schema {
    serde_json::from_value(json!({
        "type": "Task",
        "fields": [{ "name": "title", "kind": "text", "required": true }]
    }))
    .unwrap()
}

const NOW: u64 = 1_750_000_000_000;

fn mem() -> Db<MemStore, At> {
    let mut d = Db::new(MemStore::default(), clock(NOW).0, [1, 2, 3, 4]);
    d.define("tasks", &task()).unwrap();
    d
}

fn slot() -> id::RKey {
    slot_from(NOW - 60_000, "preview", "0190aaaa0000000000000000000000aa")
}

fn record(c: CreateAt) -> Record {
    match c {
        CreateAt::Created(r) | CreateAt::Exists(r) => r,
    }
}

/// The acceptance's first line: assert the ROOT, not the absence of an error.
#[test]
fn a_second_create_at_the_same_slot_is_exists_and_the_root_does_not_move() {
    let mut d = mem();
    let first = d.create_at("tasks", slot(), &fields(json!({ "title": "first" }))).unwrap();
    assert!(matches!(first, CreateAt::Created(_)), "{first:?}");
    let root = d.store_mut().root().unwrap();

    let again = d.create_at("tasks", slot(), &fields(json!({ "title": "other" }))).unwrap();
    let CreateAt::Exists(held) = again else { panic!("a second create wrote over the first: {again:?}") };
    assert_eq!(held.fields["title"], "first", "Exists answers the STORED record");
    assert_eq!(d.store_mut().root().unwrap(), root, "the tree was written");
    assert_eq!(d.count("tasks").unwrap(), 1);
}

/// Two sessions, one node — a second tab, or a second device, opened after
/// the first one's write reached the node. (Two that both read the slot
/// before either write lands are the stated residual until sdk#148 step 1.)
#[test]
fn two_sessions_creating_the_same_slot_store_exactly_one_record() {
    let node = testkit::FullNode::new();
    let mut a = Db::new(EngineStore::new(node.connect()), clock(NOW).0, [0, 0, 0, 1]);
    a.store_mut().identity().unwrap();
    a.define("tasks", &task()).unwrap();
    let from_a = a.create_at("tasks", slot(), &fields(json!({ "title": "from a" }))).unwrap();
    // A's writes reach the node, as they do when a tab ticks or closes.
    a.store_mut().flush();

    let mut b = Db::new(EngineStore::new(node.connect()), clock(NOW).0, [0, 0, 0, 2]);
    b.store_mut().identity().unwrap();
    let from_b = b.create_at("tasks", slot(), &fields(json!({ "title": "from b" }))).unwrap();
    assert!(matches!(from_a, CreateAt::Created(_)), "{from_a:?}");
    let CreateAt::Exists(held) = from_b else { panic!("the second session overwrote: {from_b:?}") };
    assert_eq!(held.fields["title"], "from a");

    let rows = b.scan("tasks", Scan::default()).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].fields["title"], "from a");
}

/// NOT LOADED IS NOT ABSENT (architect's amendment 2).
///
/// A browser session starts holding nothing. `create_at` that read an
/// unloaded slot as absent would write Preview's copy over the published
/// record — including one edited since it was published.
#[test]
fn a_fresh_session_over_a_published_record_does_not_write_until_it_has_looked() {
    // The published backend: one record, edited after it was published.
    let mut published = mem();
    published.create_at("tasks", slot(), &fields(json!({ "title": "as published" }))).unwrap();
    let id = to_hex(&slot());
    published.update("tasks", loc_from_hex(&id).unwrap(), &fields(json!({ "title": "edited since" }))).unwrap();
    let everything = published.store_mut().scan(b"", &[0xFF; 64], false, usize::MAX).unwrap();
    let root = published.store_mut().root().unwrap();

    // A fresh session: everything loaded EXCEPT the domain's records.
    let (mut s, _clock) = testkit::cached_store();
    let (lo, hi) = Db::<CachedStore, At>::domain_range("tasks");
    let outside: Vec<_> = everything.iter().filter(|(k, _)| k[..] < lo[..] || k[..] >= hi[..]).cloned().collect();
    let inside: Vec<_> = everything.iter().filter(|(k, _)| k[..] >= lo[..] && k[..] < hi[..]).cloned().collect();
    s.on_page(b"", &lo, outside.clone(), root);
    s.on_page(&hi, &[0xFF; 64], Vec::new(), root);
    let mut d = Db::new(s, clock(NOW).0, [9, 9, 9, 9]);

    let e = d
        .create_at("tasks", slot(), &fields(json!({ "title": "Preview's copy" })))
        .expect_err("an unloaded slot was taken for absent");
    assert!(matches!(e, DbError::NotLoaded { .. }), "{e:?}");
    assert_eq!(d.store().copy.pending_writes(), 0, "a write went out before the slot was read");

    // Loaded: the record is there, and it is the EDITED one, untouched.
    d.store_mut().on_page(&lo, &hi, inside, root);
    let again = d.create_at("tasks", slot(), &fields(json!({ "title": "Preview's copy" }))).unwrap();
    let CreateAt::Exists(held) = again else { panic!("wrote over the published record: {again:?}") };
    assert_eq!(held.fields["title"], "edited since");
    assert_eq!(d.store().copy.pending_writes(), 0, "stored bytes changed");
}

/// Minted and derived keys, one domain, listed by creation time.
#[test]
fn a_scan_mixing_minted_and_derived_keys_lists_in_creation_time_order() {
    let (env, now) = clock(NOW);
    let mut d = Db::new(MemStore::default(), env, [1, 2, 3, 4]);
    d.define("tasks", &task()).unwrap();
    now.set(NOW + 100);
    d.put("tasks", &fields(json!({ "title": "minted +100" }))).unwrap();
    now.set(NOW + 300);
    d.put("tasks", &fields(json!({ "title": "minted +300" }))).unwrap();
    d.create_at("tasks", slot_from(NOW + 200, "p", "s1"), &fields(json!({ "title": "derived +200" }))).unwrap();
    d.create_at("tasks", slot_from(NOW + 50, "p", "s2"), &fields(json!({ "title": "derived +50" }))).unwrap();

    let order: Vec<String> = d
        .scan("tasks", Scan::default())
        .unwrap()
        .into_iter()
        .map(|r| r.fields["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(order, ["derived +50", "minted +100", "derived +200", "minted +300"]);
}

/// Stable across runs and platforms: the vector is pinned.
#[test]
fn slot_from_is_a_pinned_vector() {
    let s = slot_from(1_750_000_000_000, "craftworks.handoff", "0190aaaa0000000000000000000000aa");
    assert_eq!(to_hex(&s), "000001977420dc00cd196ed3a1c3f054");
    assert_eq!(created_ms(&s), 1_750_000_000_000, "the time prefix is the created time");
    // The namespace is part of the identity, and a split cannot collide.
    assert_ne!(slot_from(1, "ab", "c"), slot_from(1, "a", "bc"));
    assert_ne!(slot_from(1, "one", "x"), slot_from(1, "two", "x"));
}

/// `created` is the slot's time; `updated` is when it was written.
#[test]
fn a_copy_keeps_its_created_time_and_is_updated_now() {
    let mut d = mem();
    let r = record(d.create_at("tasks", slot(), &fields(json!({ "title": "t" }))).unwrap());
    assert_eq!(r.created, NOW - 60_000);
    assert_eq!(r.updated, NOW);
}

/// A future date would head every newest-first list until it passed.
#[test]
fn a_slot_dated_past_the_skew_is_refused_and_one_inside_it_is_not() {
    let mut d = mem();
    let far = slot_from(NOW + SLOT_SKEW_MS + 1, "p", "s");
    let e = d.create_at("tasks", far, &fields(json!({ "title": "t" }))).unwrap_err();
    assert_eq!(e.code(), "REFUSED", "{e}");
    assert_eq!(d.count("tasks").unwrap(), 0);
    // THE CONTROL: at the allowance exactly, it is taken.
    let edge = slot_from(NOW + SLOT_SKEW_MS, "p", "s");
    let r = record(d.create_at("tasks", edge, &fields(json!({ "title": "t" }))).unwrap());
    assert!(r.updated >= r.created, "updated ran behind created: {r:?}");
}

/// In a domain with a parent, the slot is the rkey UNDER it.
#[test]
fn a_parented_create_at_is_addressed_under_its_parent_and_is_exists_twice() {
    let mut d = mem();
    let schema: Schema = serde_json::from_value(json!({
        "type": "Component",
        "fields": [{ "name": "project", "kind": "text", "required": true }, { "name": "title", "kind": "text" }],
        "parent": "project"
    }))
    .unwrap();
    d.define("component", &schema).unwrap();
    let pid = "0000000000000000000000000000000a";
    let f = fields(json!({ "project": pid, "title": "c" }));
    let r = record(d.create_at("component", slot(), &f).unwrap());
    assert_eq!(r.id, format!("{pid}{}", to_hex(&slot())));
    assert!(matches!(d.create_at("component", slot(), &f).unwrap(), CreateAt::Exists(_)));
    assert_eq!(d.get("component", loc_from_hex(&r.id).unwrap()).unwrap().unwrap().fields["title"], "c");
}
