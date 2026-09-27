//! THE RESERVED `craftworks.` PREFIX, BY TYPE (ARCHITECTURE §19, "An app is DATA in its owner's tree"; app-as-data P2):
//! an app's definition lives in reserved domains of the app (`craftworks.draft`, `craftworks.app`, and until P5
//! `craftworks.published`), and ONLY the SDK's definition doors write them -- through the one commit path. An ordinary
//! write naming one is refused, whatever door it came in by.

use craftworks_sdk::definition::DefKey;
use craftworks_sdk::store::{Delta, Edit, Read, Reads, Store};
use craftworks_sdk::{app, CreateAt, Db, SystemEnv};
use core_types::name::SystemDomain;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use testkit::MemStore;

fn fresh() -> Db<MemStore, SystemEnv> {
    Db::new(MemStore::default(), SystemEnv, [0u8; 4])
}

/// AN ORDINARY WRITE OF THE RESERVED PREFIX IS REFUSED: by the app namespace (`app::write`, every Session write's
/// name) and by the Db itself (a stored name that reached it some other way). Every reserved form: the three names,
/// the bare prefix, and a name under it.
#[test]
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
    for name in ["notes", "craftworks_notes", "my.notes.craftworks", "craftworksx.y"] {
        assert!(app::write(Some("notes"), name).is_ok(), "`{name}` is not reserved, yet refused");
    }
}

fn key(s: &str) -> DefKey {
    DefKey::parse(s).expect(s)
}

/// THE DOORS WRITE WHAT THE ORDINARY WRITES MAY NOT: a draft edited (put, replaced, deleted), published into
/// `craftworks.app`, read back by the one `definition` read -- behind the app's id (the tree's form) and with none
/// (the in-tab db's) alike. The live markers at the builder's own slot.
#[test]
fn the_definition_doors_edit_the_draft_and_publish_it() {
    for app in [Some("notes"), None] {
        let mut db = fresh();
        db.draft_put(app, &key("meta"), &json!({ "name": "Notes" })).unwrap();
        db.draft_put(app, &key("c/header"), &json!({ "kind": "Text" })).unwrap();
        db.draft_put(app, &key("d/rows"), &json!({ "type": "Row" })).unwrap();
        db.draft_put(app, &key("c/header"), &json!({ "kind": "Heading" })).unwrap();
        assert!(db.draft_delete(app, &key("d/rows")).unwrap(), "a draft record was not deleted");
        assert!(!db.draft_delete(app, &key("d/rows")).unwrap(), "an absent record was deleted");
        let want = vec![(key("c/header"), json!({ "kind": "Heading" })), (key("meta"), json!({ "name": "Notes" }))];
        let sorted = |mut v: Vec<(DefKey, Value)>| {
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        assert_eq!(sorted(db.definition(app, SystemDomain::Draft).unwrap()), sorted(want.clone()));
        assert!(db.definition(app, SystemDomain::App).unwrap().is_empty(), "the app holds a definition before a publish");
        assert_eq!(db.publish_definition(app).unwrap(), 2);
        assert_eq!(sorted(db.definition(app, SystemDomain::App).unwrap()), sorted(want));
        assert_eq!(db.publish_definition(app).unwrap(), 0, "an unchanged draft published something");
        // The live marker: created, then found (create_at's answer), at the builder's slot.
        assert!(!db.is_published(app, "rows").unwrap());
        assert!(matches!(db.mark_published(app, "rows").unwrap(), CreateAt::Created(_)));
        assert!(matches!(db.mark_published(app, "rows").unwrap(), CreateAt::Exists(_)));
        assert!(db.is_published(app, "rows").unwrap());
        assert_eq!(craftworks_sdk::slot_from(0, "domain", "rows").len(), 16);
    }
}

/// A store that SAMPLES the app's published definition after every commit it takes: what a reader between commits
/// would see.
struct Sampling {
    inner: MemStore,
    prefix: Vec<u8>,
    seen: Vec<BTreeMap<Vec<u8>, Vec<u8>>>,
}
impl Store for Sampling {
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) -> Result<(), craftworks_sdk::Refused> {
        self.inner.apply_batch(edits)?;
        let mut hi = self.prefix.clone();
        hi.push(0xff);
        self.seen.push(self.inner.scan(&self.prefix, &hi, false, usize::MAX).expect("in memory").into_iter().collect());
        Ok(())
    }
}
impl Reads for Sampling {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        self.inner.get(key)
    }
    fn scan(&mut self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.scan(lo, hi, reverse, limit)
    }
    fn root(&mut self) -> Read<[u8; 32]> {
        self.inner.root()
    }
    fn changes_since(&mut self, from: [u8; 32], lo: &[u8], hi: &[u8], max_entries: u32) -> Read<Delta> {
        self.inner.changes_since(from, lo, hi, max_entries)
    }
}

/// NO READER EVER SEES A MIXED DEFINITION (§19 "Publish is ONE write"; engineer1's E1): the app's published records
/// sampled after EVERY commit -- draft edits and publishes alike, over three publishes that write AND delete (a
/// record changed, one removed, one added; then the rest removed and new ones added) -- are each exactly the empty
/// definition or one whole published version. Mutant "publish one record per write" -> red.
#[test]
fn a_reader_between_commits_never_sees_a_mixed_definition() {
    let prefix = craftworks_sdk::db::record_prefix(&format!("notes.{}", SystemDomain::App.code()));
    let mut db = Db::new(Sampling { inner: MemStore::default(), prefix, seen: Vec::new() }, SystemEnv, [0u8; 4]);
    let app = Some("notes");
    let versions: Vec<Vec<(&str, Value)>> = vec![
        vec![("meta", json!({ "v": 1 })), ("c/a", json!(1)), ("c/b", json!(1)), ("d/x", json!(1))],
        vec![("meta", json!({ "v": 2 })), ("c/a", json!(2)), ("c/c", json!(2)), ("d/x", json!(1))],
        vec![("meta", json!({ "v": 3 })), ("c/d", json!(3)), ("d/y", json!(3))],
    ];
    let mut published: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = vec![BTreeMap::new()];
    let mut had: Vec<&str> = Vec::new();
    let mut commits_per_publish = Vec::new();
    for v in &versions {
        for gone in had.iter().filter(|k| !v.iter().any(|(n, _)| n == *k)) {
            db.draft_delete(app, &key(gone)).unwrap();
        }
        for (k, body) in v {
            db.draft_put(app, &key(k), body).unwrap();
        }
        let before = db.store().seen.len();
        assert!(db.publish_definition(app).unwrap() > 0, "THE SETUP: a publish changed nothing");
        commits_per_publish.push(db.store().seen.len() - before);
        published.push(db.store().seen.last().unwrap().clone());
        had = v.iter().map(|(k, _)| *k).collect();
    }
    let seen = &db.store().seen;
    assert!(seen.len() > versions.len() * 3, "THE SETUP: too few commits sampled ({})", seen.len());
    for (i, s) in seen.iter().enumerate() {
        assert!(published.contains(s), "after commit {i} a reader saw a definition that is no published version: {} records", s.len());
    }
    // And the mechanism: each publish was ONE commit.
    assert_eq!(commits_per_publish, vec![1; versions.len()], "a publish was not ONE commit");
    // Non-vacuity: every published version was different and actually seen.
    for (a, b) in published.iter().zip(published.iter().skip(1)) {
        assert_ne!(a, b, "THE SETUP: two consecutive versions were equal");
    }
}
