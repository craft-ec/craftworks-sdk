//! THE REPAIR PASS SAYS WHAT IT COUNTS (the builder's Assets tab: "37 rows" for 24 notes and the app's 13 definition
//! records). `Db::scan_all` counts every row of the tree AND those that are the person's data: a record outside every
//! app's reserved definition domains (`Db::is_data_key`, the SDK's own split: `core_types::name::reserved`). A draft,
//! a published definition and a schema are rows, never data. With an app (a session's stored names) and without one
//! (the in-tab db's).

use craftworks_sdk::definition::DefKey;
use craftworks_sdk::{Db, SystemEnv};
use serde_json::json;

type D = Db<craftworks_sdk::PageStore<testkit::PageConn>, SystemEnv>;

/// Every page of the tree: (rows, data rows).
fn count(db: &mut D) -> (usize, usize) {
    let (mut rows, mut data, mut after) = (0, 0, None);
    loop {
        let (r, d, next) = db.scan_all(after, 4).expect("a page of the tree");
        rows += r;
        data += d;
        match next {
            Some(k) => after = Some(k),
            None => return (rows, data),
        }
    }
}

fn counts(app: Option<&str>) {
    let node = testkit::PageNode::new();
    let (store, _conn, _clock) = testkit::page_store(&node);
    let mut db: D = Db::new(store, SystemEnv, [0u8; 4]);
    let tasks = app.map_or("tasks".to_string(), |a| format!("{a}.tasks"));
    let schema: craftworks_sdk::Schema = serde_json::from_str(r#"{"type":"Task","fields":[{"name":"title","kind":"text"}]}"#).expect("a schema");
    db.define(&tasks, &schema).expect("define");
    for i in 0..5 {
        let mut f = serde_json::Map::new();
        f.insert("title".into(), json!(format!("task {i}")));
        db.put(&tasks, &f).expect("put");
    }
    // The app's own definition: a draft (meta and a domain), then published -- records under the reserved prefix.
    db.draft_put(app, &DefKey::parse("meta").unwrap(), &json!({ "name": "Tasks" })).unwrap();
    db.draft_put(app, &DefKey::parse("d/tasks").unwrap(), &json!({ "type": "Task" })).unwrap();
    db.publish_definition(app).unwrap();
    let tag = app.unwrap_or("<no app>");
    let (rows, data) = count(&mut db);
    assert_eq!(data, 5, "{tag}: the data rows are not the 5 tasks ({rows} rows in all)");
    // THE CONTROL: the tree holds more than the data -- the definition's records and the schema are counted as rows.
    assert!(rows >= data + 4, "{tag}: the definition records were not rows of the tree: {rows} rows, {data} data");
}

#[test]
fn a_pass_counts_the_persons_data_apart_from_an_apps_definition_in_an_apps_session() {
    counts(Some("taskapp"));
}

#[test]
fn a_pass_counts_the_persons_data_apart_from_an_apps_definition_with_no_app() {
    counts(None);
}
