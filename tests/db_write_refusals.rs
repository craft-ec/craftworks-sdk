//! A write the store REFUSED is answered as refused, never as made
//! (craftworks-sdk#180).
//!
//! `Db::write` called `apply_batch`, which returned nothing; `CachedStore`
//! pushed its refusal onto a list and nothing read the list. Measured: 300
//! `create_at` against the copy's cap of 256 pending answered `Ok(Created)`
//! 300 times, while the copy had refused 45 of them — a caller told its data
//! was written when it had been dropped. In the builder that turned a 300-row
//! publish into "46 of 302 records did not reach the node".
//!
//! Driven over the real `Db` on the page's store (`PageStore`) over the page
//! path's node, the schema published, then the node's answers HELD — as a
//! real node's arrive after a round trip — so every write after it stays
//! pending and the cap is reached.

use craftworks_sdk::{CreateAt, DbError, Refused, Schema, SystemEnv};
use serde_json::{json, Map, Value};

mod support;
type Tab = support::page_tab::Tab<SystemEnv>;

/// A slot's `created` in the past, so no write is refused for its clock.
const PAST_MS: u64 = 1_700_000_000_000;

/// A tab with the schema PUBLISHED, then the node silent: what it writes
/// from here stays pending.
fn db() -> Tab {
    let node = testkit::PageNode::new();
    let mut t = Tab::open(&node, SystemEnv, [1, 2, 3, 4]);
    let sc: Schema = serde_json::from_value(json!({ "type": "Row", "fields": [{ "name": "n", "kind": "int" }] })).unwrap();
    t.call(|d| d.define("rows", &sc)).expect("the schema is written");
    t.seconds(3);
    assert_eq!(t.db.store().writes.copy.pending_writes(), 0, "the schema did not publish");
    t.conn.hold_answers();
    t
}

fn fields(n: i64) -> Map<String, Value> {
    match json!({ "n": n }) {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn loc(r: &craftworks_sdk::Record) -> craftworks_sdk::id::Loc {
    craftworks_sdk::id::loc_from_hex(&r.id).expect("a record's id parses")
}

fn slot(i: u64) -> craftworks_sdk::RKey {
    craftworks_sdk::slot_from(PAST_MS, "p", &format!("r{i}"))
}

/// The node answers everything it held: the commit in flight lands, and the
/// writes behind it go.
fn node_answers(t: &mut Tab) {
    t.conn.stop_holding();
    while t.conn.held() > 0 {
        let replies = t.conn.release_one();
        for r in &replies {
            t.db.store_mut().writes.on_inbound(r);
        }
        t.pump();
    }
    t.seconds(3);
}

/// **300 `create_at` past the copy's cap: every `Created` is really there,
/// and the rest are refused `NO_ROOM` by name.** RED before: 300 `Created`,
/// 45 of them absent from the copy and never sent.
#[test]
fn create_at_past_the_cap_is_refused_no_room_and_nothing_created_is_absent() {
    let mut t = db();
    let d = &mut t.db;
    let before = d.store().writes.copy.pending_writes();
    let cap = d.store().writes.copy.max_pending;
    let (mut created, mut no_room, mut absent) = (0usize, 0usize, Vec::new());
    for i in 0..300u64 {
        match d.create_at("rows", slot(i), &fields(i as i64)) {
            Ok(CreateAt::Created(r)) => {
                created += 1;
                // Created means WRITTEN: readable at once, from the copy.
                if d.get("rows", loc(&r)).expect("the range is loaded").is_none() {
                    absent.push(i);
                }
            }
            Ok(CreateAt::Exists(_)) => panic!("row {i}: nothing was there to exist"),
            Err(e) => {
                assert_eq!(e.code(), "NO_ROOM", "row {i}: {e}");
                assert!(e.is_retryable(), "NO_ROOM is retryable after a confirmation");
                assert_eq!(e.cap(), Some(cap));
                no_room += 1;
            }
        }
    }
    println!("  300 create_at, {before} pending before, cap {cap}: created {created}, NO_ROOM {no_room}, created-but-absent {}", absent.len());
    assert!(absent.is_empty(), "answered Created, and not there: rows {absent:?}");
    assert_eq!(created, cap - before, "every write the copy had room for is created");
    assert_eq!(no_room, 300 - (cap - before));
}

/// `put` past the cap: refused, and the store did not take it.
#[test]
fn put_past_the_cap_is_refused_no_room() {
    let mut t = db();
    let d = &mut t.db;
    d.store_mut().writes.copy.max_pending = d.store().writes.copy.pending_writes() + 1;
    d.put("rows", &fields(1)).expect("room for one");
    let pending = d.store().writes.copy.pending_writes();
    let e = d.put("rows", &fields(2)).expect_err("no room for a second");
    assert_eq!(e.code(), "NO_ROOM", "{e}");
    assert_eq!(d.store().writes.copy.pending_writes(), pending, "the refused write entered the copy anyway");
}

/// `update` past the cap: refused, and the record keeps its old value.
#[test]
fn update_past_the_cap_is_refused_and_the_record_is_unchanged() {
    let mut t = db();
    let d = &mut t.db;
    let r = d.put("rows", &fields(1)).expect("room");
    d.store_mut().writes.copy.max_pending = d.store().writes.copy.pending_writes();
    let e = d.update("rows", loc(&r), &fields(2)).expect_err("no room");
    assert_eq!(e.code(), "NO_ROOM", "{e}");
    let now = d.get("rows", loc(&r)).expect("loaded").expect("still there");
    assert_eq!(now.fields.get("n"), Some(&json!(1)), "the refused update shows on screen");
}

/// `delete` past the cap: refused, and the record is still there.
#[test]
fn delete_past_the_cap_is_refused_and_the_record_stays() {
    let mut t = db();
    let d = &mut t.db;
    let r = d.put("rows", &fields(1)).expect("room");
    d.store_mut().writes.copy.max_pending = d.store().writes.copy.pending_writes();
    let e = d.delete("rows", loc(&r)).expect_err("no room");
    assert_eq!(e.code(), "NO_ROOM", "{e}");
    assert!(d.get("rows", loc(&r)).expect("loaded").is_some(), "the refused delete removed it");
}

/// The BYTE cap is its own code.
#[test]
fn past_the_byte_cap_is_refused_no_room_bytes() {
    let mut t = db();
    let d = &mut t.db;
    d.store_mut().writes.copy.max_pending_bytes = 64;
    let e = d.put("rows", &fields(1)).expect_err("a record is more than 64 bytes");
    assert_eq!(e.code(), "NO_ROOM_BYTES", "{e}");
    assert!(e.is_retryable());
    assert_eq!(e.cap(), Some(64));
}

/// A page with NO SESSION makes no writes — and says so, by its own code.
#[test]
fn a_page_with_no_session_is_refused_no_session() {
    let node = testkit::PageNode::new();
    let mut t = Tab::open(&node, SystemEnv, [1, 2, 3, 4]);
    t.db.store_mut().writes.client = craftworks_sdk::Client::from_random(None);
    let d = &mut t.db;
    let sc: Schema = serde_json::from_value(json!({ "type": "Row", "fields": [{ "name": "n", "kind": "int" }] })).unwrap();
    let e = d.define("rows", &sc).expect_err("no session, no write");
    assert_eq!(e.code(), "NO_SESSION", "{e}");
    assert!(!e.is_retryable(), "waiting does not mint a session; a reload does");
    assert!(matches!(e, DbError::WriteRefused(Refused::NoSession)));
}

/// **After a confirmation frees room, the SAME write made again succeeds** —
/// the recovery `NO_ROOM` names.
#[test]
fn a_write_refused_no_room_succeeds_once_a_confirmation_frees_room() {
    let mut t = db();
    let d = &mut t.db;
    let cap = d.store().writes.copy.max_pending;
    let mut i = 0u64;
    while d.store().writes.copy.pending_writes() < cap {
        d.create_at("rows", slot(i), &fields(i as i64)).expect("room");
        i += 1;
    }
    let e = d.create_at("rows", slot(i), &fields(i as i64)).expect_err("full");
    assert_eq!(e.code(), "NO_ROOM");
    node_answers(&mut t);
    let d = &mut t.db;
    match d.create_at("rows", slot(i), &fields(i as i64)) {
        Ok(CreateAt::Created(_)) => {}
        other => panic!("the retry after a confirmation did not create: {other:?}"),
    }
}
