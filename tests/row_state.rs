//! What a record's OWN write is doing.
//!
//! "Saving", "saved here but not yet on the network" and "on the network" are
//! three different facts, and somebody deciding whether it is safe to close
//! the tab needs the second told apart from the third. A row that showed one
//! spinner for all three would be the thing craftworks-builder#20 part 2
//! exists to prevent.

use craftworks_sdk::store::{Reads, RowState};
use craftworks_sdk::{CachedStore, Db, SystemEnv, TreeStore};

fn schema() -> craftworks_sdk::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "Note",
        "fields": [{"name": "body", "kind": "text"}],
    }))
    .expect("a schema")
}

fn fields(v: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("body".into(), serde_json::Value::String(v.into()));
    m
}

fn engine_db() -> Db<CachedStore, SystemEnv> {
    let mut s = CachedStore::new(Box::new(|| 0));
    // Everything loaded and empty, so reads are answered rather than refused.
    s.on_page(b"", &[0xFFu8; 64], Vec::new(), [0u8; 32]);
    Db::new(s, SystemEnv, [0u8; 4])
}

/// A record just written is PENDING, and the same record once its write is
/// published is CLEAN.
///
/// **Both arms.** A single arm passes against a constant, and a constant is
/// exactly what this replaced.
#[test]
fn a_record_reports_its_own_write_both_before_and_after_it_lands() {
    let mut d = engine_db();
    d.define("note", &schema()).expect("define");

    let write_id = d.store().next_write_id();
    let r = d.put("note", &fields("hello")).expect("put");
    // PENDING, not clean: it has been sent and nothing has confirmed it.
    // Closing the tab now loses it, and the row has to be able to say so.
    assert_eq!(
        r.state,
        RowState::Pending,
        "a record just written claims nothing is in flight"
    );
    assert!(
        !r.state.is_settled(),
        "an unacknowledged row claimed to be saved"
    );

    let id = craftworks_sdk::id::from_hex(&r.id).expect("an id");

    // QUEUED is the engine saying it is busy with another write. A distinct
    // fact from pending — "yours has not gone yet" rather than "yours has
    // gone and nothing has answered" — and the row says which.
    d.store_mut().copy.queued(write_id);
    assert_eq!(
        d.get("note", id).expect("get").expect("there").state,
        RowState::Queued,
        "a write the engine has not taken yet is reported as if it had"
    );
    d.store_mut().copy.submitted(write_id);
    assert_eq!(
        d.get("note", id).expect("get").expect("there").state,
        RowState::Pending
    );

    d.store_mut().copy.published(write_id);
    let after = d.get("note", id).expect("get").expect("there");
    assert_eq!(after.state, RowState::Clean);
    assert!(
        after.state.is_settled(),
        "a published row still says it is in flight"
    );
}

/// A key NOT LOADED is `Unknown`, never `Clean`.
///
/// "I have not looked" is not "there is nothing in flight". A row saying
/// "saved" about a write it cannot see is the exact failure `NOT_LOADED`
/// exists to prevent, one layer up.
#[test]
fn a_key_that_is_not_loaded_is_unknown_and_not_clean() {
    let s = CachedStore::new(Box::new(|| 0));
    assert_eq!(s.row_state(b"anything"), RowState::Unknown);
    assert!(
        !s.row_state(b"anything").is_settled(),
        "a row about a key this client has never read was allowed to say 'saved'"
    );

    // THE CONTROL: once the range IS loaded, the same key reports Clean.
    // Without it, a `row_state` answering Unknown to everything for ever
    // would pass the assertion above.
    let mut s = CachedStore::new(Box::new(|| 0));
    s.on_page(b"", &[0xFFu8; 64], Vec::new(), [0u8; 32]);
    assert_eq!(s.row_state(b"anything"), RowState::Clean);
}

/// A write that was ROLLED BACK does not go back to reporting `Clean`.
///
/// The moment a write is rolled back it stops being pending, so a row asking
/// afterwards would be told "saved" about a write that never landed. That is
/// the worst answer available.
#[test]
fn a_rolled_back_write_is_not_reported_as_saved() {
    // A clock the test drives, so the timeout is reached by DECIDING it has
    // been rather than by waiting.
    let now = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let c = now.clone();
    let mut s = CachedStore::new(Box::new(move || c.get()));
    s.on_page(b"", &[0xFFu8; 64], Vec::new(), [0u8; 32]);
    s.copy.pending_timeout_ms = 10;

    let key = b"d\0note\0x".to_vec();
    craftworks_sdk::store::Store::put(&mut s, &key, b"v");
    assert_eq!(s.row_state(&key), RowState::Pending);

    // Nothing ever answers it, and the client gives up.
    now.set(1_000);
    let told = s.tick();
    assert!(
        !told.rolled_back.is_empty(),
        "the write was never rolled back"
    );
    assert_eq!(
        told.rolled_back_keys,
        vec![key.clone()],
        "the rollback did not say which ROW it was about, so no row could show it"
    );

    assert_eq!(
        s.row_state(&key),
        RowState::RolledBack,
        "a write that was rolled back went back to reporting 'saved'"
    );
    assert!(!s.row_state(&key).is_settled());

    // Writing again replaces the failure with something in flight, which is
    // a truer answer than the old failure.
    craftworks_sdk::store::Store::put(&mut s, &key, b"v2");
    assert_eq!(s.row_state(&key), RowState::Pending);
}

/// THE CONTROL for the rollback: a write that IS answered never reports
/// `RolledBack`.
///
/// Without it, a `row_state` that reported every key rolled back would pass
/// the test above.
#[test]
fn control_an_answered_write_is_never_reported_rolled_back() {
    let now = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let c = now.clone();
    let mut s = CachedStore::new(Box::new(move || c.get()));
    s.on_page(b"", &[0xFFu8; 64], Vec::new(), [0u8; 32]);
    s.copy.pending_timeout_ms = 10;

    let key = b"d\0note\0x".to_vec();
    let write_id = s.next_write_id();
    craftworks_sdk::store::Store::put(&mut s, &key, b"v");
    s.copy.published(write_id);

    now.set(1_000);
    let told = s.tick();
    assert!(
        told.rolled_back.is_empty(),
        "a published write was rolled back"
    );
    assert_eq!(s.row_state(&key), RowState::Clean);
}

/// The in-memory store answers `Clean`, always, and that is the TRUTH.
///
/// A write there is applied the moment it is made, so there is never a write
/// in flight to report. Stated rather than left implicit, because a surface
/// that answered `Unknown` would make every row of an unpublished project
/// claim this client cannot see its own data.
#[test]
fn the_in_memory_store_answers_clean_because_it_has_nothing_in_flight() {
    let mut d: Db<TreeStore, SystemEnv> = Db::new(TreeStore::new(), SystemEnv, [0u8; 4]);
    d.define("note", &schema()).expect("define");
    let r = d.put("note", &fields("hello")).expect("put");
    assert_eq!(r.state, RowState::Clean);
    assert!(r.state.is_settled());
}

/// The codes are a fixed list, distinct, and every variant has one.
#[test]
fn every_row_state_has_a_distinct_stable_code() {
    const KNOWN: &[&str] = &["CLEAN", "QUEUED", "PENDING", "ROLLED_BACK", "UNKNOWN"];
    let all = [
        RowState::Clean,
        RowState::Queued,
        RowState::Pending,
        RowState::RolledBack,
        RowState::Unknown,
    ];
    let mut codes: Vec<&str> = all.iter().map(|s| s.code()).collect();
    for c in &codes {
        assert!(KNOWN.contains(c), "{c} is not in the published list");
    }
    codes.sort_unstable();
    let n = codes.len();
    codes.dedup();
    assert_eq!(codes.len(), n, "two states share a code");

    // Only CLEAN may claim to be saved.
    for s in all {
        assert_eq!(
            s.is_settled(),
            s == RowState::Clean,
            "{s:?} settled wrongly"
        );
    }
}
