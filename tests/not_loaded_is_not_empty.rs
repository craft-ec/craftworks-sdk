//! "I have not looked" and "I looked and it is not there" are different
//! answers, and an app must be able to tell them apart.
//!
//! Told EMPTY it draws an empty list and stops. Told NOT_LOADED it reloads.
//! The two facts are one byte apart in a reply and a world apart in what an
//! app does with them, and the whole cached-read layer exists to keep them
//! distinct — so the distinction has to survive the last hop, out through
//! `Db` to the caller.
//!
//! It did not. `Db`'s error was a `String`, so a browser app would have had
//! to match on the TEXT of a message to tell recovery from refusal.

use craftworks_sdk::{CachedStore, Db, DbError, Scan, SystemEnv};

fn store() -> CachedStore {
    CachedStore::new(Box::new(|| 0))
}

fn db(store: CachedStore) -> Db<CachedStore, SystemEnv> {
    Db::new(store, SystemEnv, [0u8; 4])
}

/// Everything, as a range: the whole key space, loaded and genuinely empty.
fn load_everything_empty(s: &mut CachedStore) {
    s.on_page(b"", &[0xFFu8; 64], Vec::new(), [0u8; 32]);
}

/// A read the store has not loaded is `NOT_LOADED` — never an empty result.
#[test]
fn an_unloaded_read_says_not_loaded() {
    let mut d = db(store());
    let e = d
        .scan("note", Scan::default())
        .expect_err("a store that has loaded nothing answered a scan");
    assert_eq!(e, DbError::NotLoaded);
    assert_eq!(e.code(), "NOT_LOADED");
    assert!(
        e.is_transient(),
        "NOT_LOADED must be recoverable, or an app will not reload"
    );
}

/// **THE CONTROL.** The SAME call, on a store that HAS loaded the range and
/// found it empty, does not say `NOT_LOADED`.
///
/// Without this the test above passes against a `Db` that answers
/// `NOT_LOADED` to everything for ever — which would be a database that
/// never works, tested green.
#[test]
fn control_a_loaded_and_genuinely_empty_read_does_not_say_not_loaded() {
    let mut s = store();
    load_everything_empty(&mut s);
    let mut d = db(s);

    let e = d
        .scan("note", Scan::default())
        .expect_err("a domain that was never defined answered a scan");
    assert_ne!(
        e,
        DbError::NotLoaded,
        "a range that WAS loaded and is empty was reported as not loaded; \
         the app would reload for ever"
    );
    assert_eq!(
        e.code(),
        "NOT_DEFINED",
        "the store looked, and the domain genuinely is not there"
    );
    assert!(
        !e.is_transient(),
        "a domain that does not exist is not fixed by reloading"
    );
}

/// The codes are a FIXED list, and every variant has one.
///
/// This is what crosses the JavaScript boundary. Nothing downstream may
/// branch on a message, because a reworded message would change behaviour
/// silently.
#[test]
fn every_error_carries_a_code_from_the_fixed_list() {
    const KNOWN: &[&str] = &[
        "NOT_LOADED",
        "UNAVAILABLE",
        "TOO_LARGE",
        "NOT_DEFINED",
        "REFUSED",
    ];
    let all = [
        DbError::NotLoaded,
        DbError::Unavailable,
        DbError::TooLarge("x".into()),
        DbError::NotDefined("x".into()),
        DbError::Refused("x".into()),
    ];
    for e in &all {
        assert!(
            KNOWN.contains(&e.code()),
            "{e:?} has code {} which is not in the published list",
            e.code()
        );
        assert!(
            !e.to_string().is_empty(),
            "{e:?} has no message for a person"
        );
    }
    // Distinct, or two different situations would be indistinguishable to
    // the code that branches on them.
    let mut codes: Vec<&str> = all.iter().map(|e| e.code()).collect();
    codes.sort_unstable();
    let n = codes.len();
    codes.dedup();
    assert_eq!(codes.len(), n, "two variants share a code");
}

/// A key over the limit is `TOO_LARGE`, not `REFUSED` — the caller cannot fix
/// it by reloading and should not be told to.
#[test]
fn an_oversized_record_is_too_large_and_is_not_transient() {
    let mut s = store();
    load_everything_empty(&mut s);
    let mut d = db(s);

    let schema: craftworks_sdk::Schema = serde_json::from_value(serde_json::json!({
        "type": "Note",
        "fields": [{"name": "body", "kind": "text"}],
    }))
    .expect("a schema");
    d.define("note", &schema).expect("define");

    let huge = "x".repeat(2_000_000);
    let mut fields = serde_json::Map::new();
    fields.insert("body".into(), serde_json::Value::String(huge));
    let e = d
        .put("note", &fields)
        .expect_err("an oversized record was put");
    assert_eq!(e.code(), "TOO_LARGE", "got {e:?}");
    assert!(!e.is_transient());
}
