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
    testkit::cached_store().0
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
    assert!(matches!(e, DbError::NotLoaded { .. }), "got {e:?}");
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
    assert!(
        !matches!(e, DbError::NotLoaded { .. }),
        "a range that WAS loaded and is empty was reported as not loaded; \
         the app would reload for ever ({e:?})"
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
        DbError::NotLoaded {
            lo: b"a".to_vec(),
            hi: b"b".to_vec(),
        },
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

/// A `NotLoaded` names the span the read actually needed.
///
/// **The granularity is the read's OWN span**, and nothing wider: the exact
/// key for a get, the exact range for a scan. Widen it to the domain and one
/// `get` fetches every record in that domain, which is the page-freezing
/// version of being helpful; the copy's loaded-interval bookkeeping means a
/// later, wider read asks for whatever is still missing, so asking for
/// exactly what was wanted costs nothing.
///
/// A cold read therefore reports a CHAIN of spans, not one. `scan` and `get`
/// both read their domain's schema first, so on an empty store the first span
/// either of them names is the schema's key — the read genuinely cannot
/// proceed without it. The next ask names the next missing thing. That is why
/// the recovery is bounded by PROGRESS rather than by a count of two.
///
/// Without the span the recovery cannot be performed at all, which is exactly
/// what the first version of it did: it requested nothing.
#[test]
fn a_not_loaded_names_the_span_the_read_needed() {
    let mut d = db(store());
    let scan = d
        .scan("note", Scan::default())
        .expect_err("answered a scan");
    let (lo, hi) = scan
        .needs()
        .expect("a NotLoaded that names no span cannot be recovered from");
    assert!(lo < hi, "an empty span asks for nothing: {lo:?}..{hi:?}");
    let span_is_narrow = hi.len() <= lo.len() + 1;
    assert!(
        span_is_narrow,
        "the first span a cold scan needs is its schema's single key, not a \
         range: {lo:?}..{hi:?}"
    );

    // THE CONTROL: the span is derived from the READ and is not one constant.
    // A scan of a different domain needs a different span.
    let other = d
        .scan("task", Scan::default())
        .expect_err("answered a scan");
    let (olo, _) = other.needs().expect("names no span");
    assert_ne!(
        lo, olo,
        "two different domains report the same span, so it is not the read's \
         own and the recovery would load the wrong thing"
    );
}
