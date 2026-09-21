//! A domain's key range: the one place a name becomes bytes.
//!
//! `preload` used to take `[[lo, hi], …]` raw key strings, so a caller had to
//! know that a domain's records live under a particular prefix and where that
//! range ends. That is the one thing this boundary exists to hide — and a
//! layout hard-coded into apps could never change afterwards without breaking
//! every one of them.

use craftworks_sdk::{CachedStore, Db, SystemEnv};

type D = Db<CachedStore, SystemEnv>;

/// A domain's range contains its own records and stops before the next
/// domain's.
#[test]
fn a_domains_range_holds_its_records_and_nothing_else() {
    let (lo, hi) = D::domain_range("tasks");
    assert!(lo < hi, "an empty range asks for nothing");

    let id = [7u8; 16];
    let key = craftworks_sdk::db::record_key("tasks", id);
    assert!(
        key >= lo && key < hi,
        "a record of the domain falls outside the range that is supposed to load it"
    );

    // THE CONTROL: another domain's record does NOT fall in it. Without this,
    // a `domain_range` returning the whole key space would pass the assertion
    // above — and would make every preload fetch the entire database.
    let other = craftworks_sdk::db::record_key("notes", id);
    assert!(
        other < lo || other >= hi,
        "another domain's records fall inside this domain's range"
    );
}

/// Two domains have two ranges. A constant would make preload load the same
/// thing however many domains it was given.
#[test]
fn different_domains_have_different_ranges() {
    assert_ne!(D::domain_range("tasks"), D::domain_range("notes"));
}

/// A domain whose name is a PREFIX of another's does not swallow it.
///
/// `task` and `tasks` are different domains. A range built by prefix without
/// a separator would make preloading `task` fetch every record of `tasks`
/// too — and, worse, make a scan of `task` report them as its own.
#[test]
fn a_domain_is_not_swallowed_by_one_whose_name_extends_it() {
    let (lo, hi) = D::domain_range("task");
    let inner = craftworks_sdk::db::record_key("tasks", [1u8; 16]);
    assert!(
        inner < lo || inner >= hi,
        "records of `tasks` fall inside the range of `task`"
    );
}

/// `domain_of_range` is the inverse, and ONLY of a whole domain's range: a
/// completed load of anything narrower or wider must not be taken for the
/// domain, or `Refresh` would ask for deltas from a root the copy only partly
/// holds (sdk#142).
#[test]
fn domain_of_range_names_a_domain_only_for_its_whole_range() {
    let (lo, hi) = D::domain_range("tasks");
    assert_eq!(D::domain_of_range(&lo, &hi).as_deref(), Some("tasks"));
    let key = craftworks_sdk::db::record_key("tasks", [1u8; 16]);
    let mut after = key.clone();
    after.push(0);
    assert_eq!(D::domain_of_range(&key, &after), None, "one record's span is not the domain");
    assert_eq!(D::domain_of_range(&lo, &after), None, "nor is a prefix of it");
    assert_eq!(D::domain_of_range(b"", &[0xFF; 8]), None, "nor everything");
    let (task_lo, _) = D::domain_range("task");
    assert_eq!(D::domain_of_range(&task_lo, &hi), None, "nor a span straddling two domains");
}

/// `watch_key_of_range` names a whole domain OR one parent's band — the
/// spans a binding reads — and nothing else (sdk#137).
#[test]
fn watch_key_of_range_names_a_domain_or_a_band_and_nothing_else() {
    let (lo, hi) = D::domain_range("tasks");
    assert_eq!(D::watch_key_of_range(&lo, &hi).as_deref(), Some("tasks"));
    let p = [3u8; 16];
    let (blo, bhi) = D::parent_range("tasks", &p);
    let key = D::watch_key_of_range(&blo, &bhi).expect("a band names its key");
    assert_eq!(key, D::watch_key("tasks", Some(&p)));
    assert_eq!(D::watch_range(&key), Some((blo.clone(), bhi.clone())), "and the key names the band back");
    let mut past = blo.clone();
    past.push(0);
    assert_eq!(D::watch_key_of_range(&blo, &past), None, "a narrower span is not the band");
    assert_eq!(D::watch_key_of_range(&blo, &hi), None, "nor one reaching the domain's end");
}
