//! THE RESERVED `craftworks.` PREFIX, BY TYPE (ARCHITECTURE §19, "An app is DATA in its owner's tree"; app-as-data P2):
//! an app's definition lives in reserved domains of the app (`craftworks.draft`, `craftworks.app`, and until P5
//! `craftworks.published`), and ONLY the SDK's definition doors write them -- through the one commit path. An ordinary
//! write naming one is refused, whatever door it came in by.

use craftworks_sdk::{app, Db, SystemEnv};
use serde_json::{json, Map};
use testkit::MemStore;

fn fresh() -> Db<MemStore, SystemEnv> {
    Db::new(MemStore::default(), SystemEnv, [0u8; 4])
}

/// AN ORDINARY WRITE OF THE RESERVED PREFIX IS REFUSED: by the app namespace (`app::write`, every Session write's
/// name) and by the Db itself (a stored name that reached it some other way). Every reserved form: the three names,
/// the bare prefix, and a name under it.
#[test]
#[should_panic(expected = "an ordinary write of a reserved name was not refused")] // PINNED: flipped by P2
fn an_ordinary_write_of_the_reserved_prefix_is_refused_by_the_namespace_and_the_db() {
    let schema: craftworks_sdk::Schema = serde_json::from_value(json!({ "type": "T", "fields": [{ "name": "x", "kind": "text" }] })).unwrap();
    for name in ["craftworks.draft", "craftworks.app", "craftworks.published", "craftworks", "craftworks.anything"] {
        let through_namespace = app::write(Some("notes"), name);
        let stored = format!("notes.{name}");
        let mut db = fresh();
        let defined = db.define(&stored, &schema);
        let mut f = Map::new();
        f.insert("x".into(), json!("v"));
        let put = db.put(&stored, &f);
        assert!(
            through_namespace.is_err() && defined.is_err() && put.is_err(),
            "an ordinary write of a reserved name was not refused: `{name}` (namespace {:?}, define {:?}, put {:?})",
            through_namespace.map(|n| n.to_string()),
            defined,
            put.map(|r| r.id)
        );
    }
}

/// THE CONTROL: an ordinary name that merely CONTAINS the word is the app's own.
#[test]
fn a_name_that_is_not_under_the_prefix_is_the_apps_own() {
    for name in ["notes", "craftworks_notes", "my.craftworks.draft", "craftworksx.y"] {
        assert!(app::write(Some("notes"), name).is_ok(), "`{name}` is not reserved, yet refused");
    }
}
