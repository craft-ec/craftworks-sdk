//! THE KEEP API'S ANSWERS (sdk#472, src/keep_api.rs): the JSON the Assets tab and the health widget read, from the
//! SDK's own facts (a `keep` record, an audit `Report`), and the two record writes the SDK owns. Each shape is the one
//! agreed with engineer2 and pinned by builder#164's view.
use craftworks_sdk::keep::{Health as Counts, Keep, Repair};
use craftworks_sdk::keep_api::*;
use page::audit::{Health, Report};
use serde_json::json;
use std::collections::BTreeMap;

const OWN: [u8; 32] = [1; 32];
const APP: [u8; 32] = [2; 32];
fn name(id: &[u8; 32]) -> String {
    format!("site{}", id[0])
}
fn keep(repair: Repair, audited_at: u64, whole: u32, degraded: u32, damaged: u32) -> Keep {
    Keep { repair, warn_below: 2, audited_at, health: Counts { groups: whole + degraded + damaged, whole, degraded, damaged } }
}
fn report(measured: bool, margins: &[(i64, usize)], whole: usize, degraded: usize, damaged: Vec<[u8; 32]>, health: Health) -> Report {
    Report {
        health,
        margins: margins.iter().copied().collect::<BTreeMap<_, _>>(),
        started_at: 10,
        finished_at: 20,
        root: [0; 32],
        measured,
        groups: whole + degraded + damaged.len(),
        whole,
        degraded,
        damaged,
        pending: 2,
        rejected: vec![[0xab; 32]],
    }
}

/// The list is DERIVED: the own tree first -- its defaults when it has no record -- then every other record; a target
/// is named by the text owner passed in; a never-audited asset has no health.
#[test]
fn keep_assets_lists_the_own_tree_first_with_its_defaults_then_every_record() {
    let v = assets_json(&OWN, &[(APP, keep(Repair::Below(3), 1_790_000_000, 12, 1, 0))], name);
    assert_eq!(
        v,
        json!([
            { "target": "site1", "kind": "identity", "policy": { "repair": "always", "warn_below": 2 }, "audited_at": 0, "health": null, "warning": null },
            { "target": "site2", "kind": "app", "policy": { "repair": { "below": 3 }, "warn_below": 2 }, "audited_at": 1_790_000_000u64,
              "health": { "word": "degraded", "groups": 13, "whole": 12, "degraded": 1, "damaged": 0 }, "warning": null },
        ])
    );
    // The own tree's OWN record wins over its defaults, and is not listed twice.
    let v = assets_json(&OWN, &[(OWN, keep(Repair::Off, 5, 3, 0, 1))], name);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["policy"]["repair"], "off");
    assert_eq!(v[0]["health"]["word"], "damaged");
}

/// `keepReport`: running → progress; unmeasured → said, no counts; done → the report's own word and counts, its damaged
/// and rejected named, the warning from `keep::warning` over its margins; nothing → null.
#[test]
fn keep_report_says_running_unmeasured_or_the_finished_pass() {
    let policy = keep(Repair::Always, 0, 0, 0, 0);
    assert_eq!(report_json(None, Some((212, 318)), &policy), json!({ "state": "running", "pass": "full", "asked": 212, "of": 318 }));
    assert_eq!(report_json(None, None, &policy), json!(null));
    let u = report(false, &[], 0, 0, vec![], Health::Unmeasured);
    assert_eq!(report_json(Some(&u), None, &policy)["state"], "unmeasured");
    assert!(report_json(Some(&u), None, &policy).get("whole").is_none(), "an unmeasured pass carried counts");
    let r = report(true, &[(-1, 1), (1, 1), (8, 12)], 12, 1, vec![[0x8c; 32]], Health::Damaged);
    let v = report_json(Some(&r), None, &policy);
    assert_eq!(v["state"], "done");
    assert_eq!(v["health"], "damaged");
    assert_eq!((v["whole"].clone(), v["degraded"].clone(), v["pending"].clone(), v["min_margin"].clone()), (json!(12), json!(1), json!(2), json!(-1)));
    assert_eq!(v["damaged"], json!([core_types::hex::encode(&[0x8c; 32])]));
    assert_eq!(v["rejected"], json!([core_types::hex::encode(&[0xab; 32])]));
    assert_eq!(v["warning"], "2 groups have fewer than 2 blocks to spare (lowest: -1)");
}

/// The warning is `keep::warning`'s count put in words; none when no group is below `warn_below`.
#[test]
fn the_warning_is_keep_warnings_count_in_words() {
    let m = |v: &[(i64, usize)]| v.iter().copied().collect::<BTreeMap<_, _>>();
    assert_eq!(warning_text(&m(&[(1, 1), (8, 12)]), 2).as_deref(), Some("1 group has fewer than 2 blocks to spare (lowest: 1)"));
    assert_eq!(warning_text(&m(&[(2, 3), (8, 12)]), 2), None);
    assert_eq!(warning_text(&m(&[]), 2), None);
}

/// `keepSet`'s policy edit: each field applied on its own, the rest kept; a malformed edit refused whole, by name.
#[test]
fn a_policy_edit_applies_what_it_names_and_refuses_what_it_cannot_read() {
    let base = keep(Repair::Always, 7, 1, 0, 0);
    assert_eq!(apply_policy(base, r#"{"repair":{"below":3}}"#).unwrap(), Keep { repair: Repair::Below(3), ..base });
    assert_eq!(apply_policy(base, r#"{"warn_below":4}"#).unwrap(), Keep { warn_below: 4, ..base });
    assert_eq!(apply_policy(base, r#"{"repair":"off","warn_below":0}"#).unwrap(), Keep { repair: Repair::Off, warn_below: 0, ..base });
    for bad in [r#"{"repair":"sometimes"}"#, r#"{"repair":{"below":-1}}"#, r#"{"warn_below":300}"#, "not json"] {
        assert!(apply_policy(base, bad).is_err(), "{bad} was applied");
    }
}

/// A FULL pass writes its time and counts back, the policy kept; an UNMEASURED pass writes NOTHING.
#[test]
fn a_full_pass_writes_back_and_an_unmeasured_one_writes_nothing() {
    let existing = keep(Repair::Below(2), 5, 0, 0, 0);
    let r = report(true, &[(8, 9)], 9, 0, vec![[3; 32]], Health::Damaged);
    assert_eq!(write_back(Some(existing), false, &r, 1_790_000_123), Some(Keep { audited_at: 1_790_000_123, health: Counts { groups: 10, whole: 9, degraded: 0, damaged: 1 }, ..existing }));
    assert_eq!(write_back(None, true, &r, 9).map(|k| k.repair), Some(Repair::Always), "the own tree's defaults were not the base");
    assert_eq!(write_back(Some(existing), false, &report(false, &[], 0, 0, vec![], Health::Unmeasured), 9), None, "an unmeasured pass wrote a record");
}

/// A FIRST publish writes the app's record (always, warn below 2); a later publish never overwrites the policy.
#[test]
fn a_first_publish_writes_the_apps_record_once() {
    assert_eq!(first_publish(None), Some(Keep { repair: Repair::Always, warn_below: 2, audited_at: 0, health: Counts::default() }));
    assert_eq!(first_publish(Some(keep(Repair::Off, 0, 0, 0, 0))), None);
}

/// `keepSet`'s answers: ok, or the refusal named.
#[test]
fn keep_set_answers_ok_or_names_its_refusal() {
    assert_eq!(set_answer(Ok(())), json!({ "ok": true }));
    assert_eq!(set_answer(Err(SetRefused::BadAddress("NotASiteLink".into()))), json!({ "refused": "BAD_ADDRESS", "said": "NotASiteLink" }));
    assert_eq!(set_answer(Err(SetRefused::BadPolicy("x".into()))), json!({ "refused": "BAD_POLICY", "said": "x" }));
}

/// THE CONTRACT WITH THE BUILDER'S TAB (the architect): each shape's EXACT field names and value vocabulary, golden. A
/// rename or a new word breaks this test, not builder#164's view (assets.js reads exactly these).
#[test]
fn the_tabs_contract_shapes_are_golden() {
    let keys = |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    let policy = keep(Repair::Always, 0, 0, 0, 0);
    // keepAssets: one element's fields, and its health's.
    let a = assets_json(&OWN, &[(OWN, keep(Repair::Always, 9, 1, 0, 0))], name);
    assert_eq!(keys(&a[0]), ["audited_at", "health", "kind", "policy", "target", "warning"]);
    assert_eq!(keys(&a[0]["policy"]), ["repair", "warn_below"]);
    assert_eq!(keys(&a[0]["health"]), ["damaged", "degraded", "groups", "whole", "word"]);
    // The vocabularies: kind, repair, the health word.
    assert_eq!(a[0]["kind"], "identity");
    assert_eq!(assets_json(&OWN, &[(APP, policy)], name)[1]["kind"], "app");
    assert_eq!([repair_json(Repair::Off), repair_json(Repair::Always), repair_json(Repair::Below(3))], [json!("off"), json!("always"), json!({ "below": 3 })]);
    assert_eq!([Health::Unmeasured, Health::Damaged, Health::Degraded, Health::Whole].map(health_word), ["unmeasured", "damaged", "degraded", "whole"]);
    // keepReport: the three states and each one's exact fields.
    let running = report_json(None, Some((1, 2)), &policy);
    assert_eq!(keys(&running), ["asked", "of", "pass", "state"]);
    let unmeasured = report_json(Some(&report(false, &[], 0, 0, vec![], Health::Unmeasured)), None, &policy);
    assert_eq!(keys(&unmeasured), ["finished_at", "health", "pass", "started_at", "state"]);
    let done = report_json(Some(&report(true, &[(8, 1)], 1, 0, vec![], Health::Whole)), None, &policy);
    assert_eq!(keys(&done), ["damaged", "degraded", "finished_at", "groups", "health", "min_margin", "pass", "pending", "rejected", "started_at", "state", "warning", "whole"]);
    assert_eq!([&running, &unmeasured, &done].map(|v| v["state"].clone()), [json!("running"), json!("unmeasured"), json!("done")]);
    assert_eq!([&running, &unmeasured, &done].map(|v| v["pass"].clone()), [json!("full"), json!("full"), json!("full")]);
    // keepSet's answers.
    assert_eq!(keys(&set_answer(Ok(()))), ["ok"]);
    assert_eq!(keys(&set_answer(Err(SetRefused::BadAddress("x".into())))), ["refused", "said"]);
}
