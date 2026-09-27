//! THE SDK AS DATA (ARCHITECTURE §19; craftworks-docs 288f935): an SDK version is ONE record `f/sdk/<rev>` of the
//! platform app's PUBLISHED definition -- its bytes the version's piece set, its body `{ rev, name?, format_tag? }` --
//! read by `Db::platform_sdk`; an app's `meta.sdk` is `{ tree, rev }` and nothing else.

use craftworks_sdk::definition::DefKey;
use craftworks_sdk::platform::{check_meta_sdk, parse_piece_set, PLATFORM_APP};
use craftworks_sdk::{Db, SystemEnv};
use serde_json::{json, Value};
use testkit::MemStore;

fn fresh() -> Db<MemStore, SystemEnv> {
    Db::new(MemStore::default(), SystemEnv, [0u8; 4])
}

const REV: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

/// A piece set as a build's pieces.json names one: `k + m` pieces, each an address and a sha256.
fn set(k: usize, m: usize) -> String {
    let pieces: Vec<Value> = (0..k + m).map(|i| json!({ "address": format!("addr{i}"), "sha256": format!("{:064x}", i + 1) })).collect();
    json!({ "name": "app", "k": k, "m": m, "pieces": pieces }).to_string()
}

/// The platform app publishes `rev` (its record's body `body`, its bytes `bytes`).
fn publish(db: &mut Db<MemStore, SystemEnv>, rev: &str, body: Value, bytes: &str) {
    db.draft_file(Some(PLATFORM_APP), &format!("sdk/{rev}"), bytes.as_bytes(), &body).unwrap();
    db.publish_definition(Some(PLATFORM_APP)).unwrap();
}

#[test]
fn a_published_version_is_read_whole_by_its_rev() {
    let mut db = fresh();
    publish(&mut db, REV, json!({ "rev": REV, "name": "0.4", "format_tag": "pt2" }), &set(3, 2));
    let v = db.platform_sdk(REV).unwrap();
    assert_eq!((v.rev.as_str(), v.name.as_deref(), v.format_tag.as_deref()), (REV, Some("0.4"), Some("pt2")));
    assert_eq!((v.set.k, v.set.m, v.set.pieces.len()), (3, 2, 5));
    assert_eq!(v.set.pieces[0].address, "addr0");
    assert_eq!(v.set.pieces[4].sha256[31], 5);
}

#[test]
fn only_what_is_published_is_a_version_and_every_bad_record_is_refused_by_name() {
    let mut db = fresh();
    // A DRAFT is not a version: nothing published yet.
    db.draft_file(Some(PLATFORM_APP), &format!("sdk/{REV}"), set(2, 1).as_bytes(), &json!({ "rev": REV })).unwrap();
    let e = format!("{:?}", db.platform_sdk(REV).unwrap_err());
    assert!(e.contains("publishes no SDK"), "a draft was read as a published version: {e}");
    db.publish_definition(Some(PLATFORM_APP)).unwrap();
    assert!(db.platform_sdk(REV).is_ok(), "THE CONTROL: once published it is a version");

    // Not a rev at all.
    for bad in ["", "0123", "0123456789ABCDEF0123456789ABCDEF01234567", &format!("{REV}0")] {
        assert!(format!("{:?}", db.platform_sdk(bad).unwrap_err()).contains("is not an SDK rev"), "`{bad}` taken as a rev");
    }
    // A record whose body names another rev.
    publish(&mut db, OTHER, json!({ "rev": REV }), &set(2, 1));
    assert!(format!("{:?}", db.platform_sdk(OTHER).unwrap_err()).contains("names rev"), "a record naming another rev was taken");
    // Bytes that are not a piece set: k + m does not match the pieces, k is 0, not JSON.
    for (bytes, words) in [(json!({ "name": "app", "k": 3, "m": 2, "pieces": [] }).to_string(), "pieces for k"), (set(0, 1), "pieces for k"), ("not json".to_string(), "not JSON")] {
        publish(&mut db, OTHER, json!({ "rev": OTHER }), &bytes);
        let e = format!("{:?}", db.platform_sdk(OTHER).unwrap_err());
        assert!(e.contains(words), "`{bytes}` was read as a piece set: {e}");
    }
}

#[test]
fn a_version_is_read_from_the_platform_app_whatever_app_the_db_writes() {
    let mut db = fresh();
    publish(&mut db, REV, json!({ "rev": REV }), &set(1, 1));
    // Another app's draft named like a version is not the platform's.
    db.draft_file(Some("someone-else"), &format!("sdk/{OTHER}"), set(1, 1).as_bytes(), &json!({ "rev": OTHER })).unwrap();
    db.publish_definition(Some("someone-else")).unwrap();
    assert!(db.platform_sdk(REV).is_ok());
    assert!(db.platform_sdk(OTHER).is_err(), "another app's record was read as a platform version");
}

#[test]
fn meta_sdk_is_the_one_shape_refused_at_the_write() {
    let tree = "ab".repeat(32);
    let ok = json!({ "name": "A", "sdk": { "tree": tree, "rev": REV } });
    assert!(check_meta_sdk(&ok).is_ok());
    assert!(check_meta_sdk(&json!({ "name": "A" })).is_ok(), "a meta with no sdk (the bootstrap) was refused");
    for (bad, words) in [
        (json!({ "sdk": "latest" }), "not an object"),
        (json!({ "sdk": { "tree": tree, "rev": REV, "pin": true } }), "other than tree and rev"),
        (json!({ "sdk": { "tree": "abc", "rev": REV } }), "64-hex Register"),
        (json!({ "sdk": { "tree": tree, "rev": "v1" } }), "40-hex SDK rev"),
        (json!({ "sdk": { "rev": REV } }), "64-hex Register"),
    ] {
        let e = format!("{:?}", check_meta_sdk(&bad).unwrap_err());
        assert!(e.contains(words), "{bad} refused for the wrong reason: {e}");
    }
    // THE WRITE refuses it: the draft door, before anything is written.
    let mut db = fresh();
    let e = format!("{:?}", db.draft_put(Some("proj"), &DefKey::Meta, &json!({ "sdk": "latest" })).unwrap_err());
    assert!(e.contains("meta.sdk"), "the draft door wrote a bad meta.sdk: {e}");
    assert!(db.definition(Some("proj"), core_types::name::SystemDomain::Draft).unwrap().is_empty(), "the refused meta was written");
    db.draft_put(Some("proj"), &DefKey::Meta, &ok).unwrap();
}

#[test]
fn one_reader_of_a_piece_set_names_its_caller() {
    let e = parse_piece_set("publish_app", "{}").unwrap_err();
    assert!(e.starts_with("publish_app: "), "{e}");
    let e = parse_piece_set("SDK x", &json!({ "name": "app", "k": 1, "m": 0, "pieces": [{ "address": "a", "sha256": "zz" }] }).to_string()).unwrap_err();
    assert!(e.contains("no 32-byte sha256"), "{e}");
}
