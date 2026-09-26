//! THE KEEP API'S ANSWERS (sdk#472; KEEPER.md §1, §3, §5): what the Session's `keep_*` calls return to the Assets tab
//! and the health widget, as JSON. Pure functions over the SDK's own facts -- the `keep` record (`craftworks_sdk::keep`,
//! engineer2's) and the audit's `Report` (`page::audit`, engineer2's). Nothing here measures, walks or stores: it only
//! puts those facts into the agreed shapes, and decides the two record writes the SDK owns (a full pass's write-back,
//! and a first publish's record). The Session binds these to wasm names; no fact is derived twice.
//!
//! A target's TEXT (the base58 site id the builder knows its apps by) is passed in as `name`: its one owner is the
//! address parser's inverse beside `page_io::site_id_of_address` (engineer4), not this file.

use crate::keep::{Health as Counts, Keep, Repair};
use page::audit::{Health, Report};
use serde_json::{json, Value};

/// A repair policy as the tab reads it: `"off"`, `"always"`, or `{ "below": N }`.
pub fn repair_json(r: Repair) -> Value {
    match r {
        Repair::Off => json!("off"),
        Repair::Always => json!("always"),
        Repair::Below(n) => json!({ "below": n }),
    }
}

/// A health word as the tab shows it.
pub fn health_word(h: Health) -> &'static str {
    match h {
        Health::Unmeasured => "unmeasured",
        Health::Damaged => "damaged",
        Health::Degraded => "degraded",
        Health::Whole => "whole",
    }
}

/// The word for a RECORD's stored counts: the SAME one derivation as a report's (`page::audit::Health::of`).
fn record_health(c: &Counts) -> Health {
    Health::of(c.damaged as usize, c.degraded as usize)
}

/// One asset as the tab lists it. `audited_at == 0` is "never": no health.
fn asset_json(target: &[u8; 32], kind: &str, k: &Keep, name: &impl Fn(&[u8; 32]) -> String) -> Value {
    let health = (k.audited_at != 0).then(|| {
        let c = k.health;
        json!({ "word": health_word(record_health(&c)), "groups": c.groups, "whole": c.whole, "degraded": c.degraded, "damaged": c.damaged })
    });
    json!({
        "target": name(target),
        "kind": kind,
        "policy": { "repair": repair_json(k.repair), "warn_below": k.warn_below },
        "audited_at": k.audited_at,
        "health": health,
        // A record carries no margins, so its warning is the live report's (see `report_json`); none here.
        "warning": Value::Null,
    })
}

/// `keepAssets()`: the identity's OWN tree first (its record, or `Keep::OWN_DEFAULT` when it has none), then every
/// other `keep` record (KEEPER §2: the list is derived, never configured).
pub fn assets_json(own: &[u8; 32], records: &[([u8; 32], Keep)], name: impl Fn(&[u8; 32]) -> String) -> Value {
    let own_keep = records.iter().find(|(t, _)| t == own).map(|(_, k)| *k).unwrap_or(Keep::OWN_DEFAULT);
    let mut out = vec![asset_json(own, "identity", &own_keep, &name)];
    out.extend(records.iter().filter(|(t, _)| t != own).map(|(t, k)| asset_json(t, "app", k, &name)));
    Value::Array(out)
}

/// A pass as the tab sees it: ONE list of its states (the "Structure before code" rule), each with the words the
/// contract carries -- matched exhaustively, never a string written in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassState {
    /// A pass is running on the asset (`audit_progress()` is `Some`).
    Running,
    /// A pass finished with no signer to ask: nothing measured.
    Unmeasured,
    /// A pass finished and measured.
    Done,
}

impl PassState {
    /// The contract's word for it (builder#164 reads exactly these).
    pub fn word(self) -> &'static str {
        match self {
            PassState::Running => "running",
            PassState::Unmeasured => "unmeasured",
            PassState::Done => "done",
        }
    }
}

/// The warning a pass's margins give under `warn_below` (the count is `keep::warning`'s, the one derivation): the
/// words the tab shows, or null.
pub fn warning_text(margins: &std::collections::BTreeMap<i64, usize>, warn_below: u8) -> Option<String> {
    let n = crate::keep::warning(margins, warn_below);
    let lowest = margins.keys().next().copied()?;
    (n > 0).then(|| format!("{n} group{} {} fewer than {warn_below} blocks to spare (lowest: {lowest})", if n == 1 { "" } else { "s" }, if n == 1 { "has" } else { "have" }))
}

/// `keepReport(target)`: the pass that is running (`progress` = the page's `audit_progress()`), or the one that
/// finished this session (`report` = its `take_audit()`), or null. `policy` gives `warn_below` for the warning.
pub fn report_json(report: Option<&Report>, progress: Option<(usize, usize)>, policy: &Keep) -> Value {
    let hex = |id: &[u8; 32]| core_types::hex::encode(id);
    match (report, progress) {
        // A RUNNING pass is newer than any finished one (through Page it cannot coexist: `audit()` clears the report).
        (_, Some((asked, of))) => json!({ "state": PassState::Running.word(), "pass": "full", "asked": asked, "of": of }),
        (Some(r), _) if !r.measured => json!({ "state": PassState::Unmeasured.word(), "pass": "full", "health": health_word(r.health), "started_at": r.started_at, "finished_at": r.finished_at }),
        (Some(r), _) => json!({
            "state": PassState::Done.word(),
            "pass": "full",
            "health": health_word(r.health),
            "groups": r.groups,
            "whole": r.whole,
            "degraded": r.degraded,
            "damaged": r.damaged.iter().map(hex).collect::<Vec<_>>(),
            "rejected": r.rejected.iter().map(hex).collect::<Vec<_>>(),
            "pending": r.pending,
            "min_margin": r.margins.keys().next(),
            "warning": warning_text(&r.margins, policy.warn_below),
            "started_at": r.started_at,
            "finished_at": r.finished_at,
        }),
        (None, None) => Value::Null,
    }
}

/// A policy edit (`keepSet`'s `policy` JSON: any of `repair`, `warn_below`) applied to `base`. A malformed edit is
/// refused by name, never half-applied.
pub fn apply_policy(base: Keep, policy: &str) -> Result<Keep, String> {
    let v: Value = serde_json::from_str(policy).map_err(|e| format!("the policy is not JSON: {e}"))?;
    let mut k = base;
    if let Some(r) = v.get("repair") {
        k.repair = match r {
            Value::String(s) if s == "off" => Repair::Off,
            Value::String(s) if s == "always" => Repair::Always,
            Value::Object(o) => match o.get("below").and_then(Value::as_u64).and_then(|n| u8::try_from(n).ok()) {
                Some(n) => Repair::Below(n),
                None => return Err(format!("repair {r} is not off, always or {{\"below\": N}}")),
            },
            other => return Err(format!("repair {other} is not off, always or {{\"below\": N}}")),
        };
    }
    if let Some(w) = v.get("warn_below") {
        k.warn_below = w.as_u64().and_then(|n| u8::try_from(n).ok()).ok_or_else(|| format!("warn_below {w} is not a small whole number"))?;
    }
    // The range (0 ..= m for `below N` and `warn_below`, KEEPER §3) is the record's own rule: `Keep::check`.
    k.check()?;
    Ok(k)
}

/// A FULL pass's write-back into the target's record (KEEPER §3/§5): its time and counts, the policy kept. An
/// UNMEASURED pass writes nothing -- it measured nothing (engineer2). The time is the pass's own (`finished_at`, the
/// page's clock in ms), so the record and the report cannot disagree.
pub fn write_back(existing: Option<Keep>, own: bool, r: &Report) -> Option<Keep> {
    if !r.measured {
        return None;
    }
    let now_s = r.finished_at / 1000;
    let base = record_or_default(existing, own);
    let n = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
    Some(Keep { audited_at: now_s, health: Counts { groups: n(r.groups), whole: n(r.whole), degraded: n(r.degraded), damaged: n(r.damaged.len()) }, ..base })
}

/// A FIRST publish's record for the app (KEEPER §2: the one way an app enters the list): `always`, warn below 2 --
/// written only when there is none, so a later publish never overwrites the person's policy.
pub fn first_publish(existing: Option<Keep>) -> Option<Keep> {
    existing.is_none().then_some(Keep::APP_DEFAULT)
}

/// `keepSet`'s answer: `{ ok: true }`, or the address parser's refusal as `{ refused: "BAD_ADDRESS", said }`.
pub fn set_answer(outcome: Result<(), SetRefused>) -> Value {
    match outcome {
        Ok(()) => json!({ "ok": true }),
        Err(SetRefused::BadAddress(said)) => json!({ "refused": "BAD_ADDRESS", "said": said }),
        Err(SetRefused::BadPolicy(said)) => json!({ "refused": "BAD_POLICY", "said": said }),
    }
}

/// Why a `keepSet` was refused, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetRefused {
    BadAddress(String),
    BadPolicy(String),
}

/// An asset's record, or -- when it has none -- its default: the own tree's, or an app's. The ONE place that choice
/// is made (the record writes and the policy the tab reads all start from it).
pub fn record_or_default(existing: Option<Keep>, own: bool) -> Keep {
    existing.unwrap_or(if own { Keep::OWN_DEFAULT } else { Keep::APP_DEFAULT })
}

/// `keepSet`'s decision once the address is parsed (the parser is page-io's, `site_id_of_address`): the target's
/// record, or its default when it has none (the OWN tree's own default, an app's the app default), with the edit
/// applied. A bad edit is `BAD_POLICY` by name, and nothing is written.
pub fn set_policy(own: &[u8; 32], target: &[u8; 32], existing: Option<Keep>, policy: &str) -> Result<Keep, SetRefused> {
    apply_policy(record_or_default(existing, target == own), policy).map_err(SetRefused::BadPolicy)
}

/// The FULL pass's write-back from its report AS THE TAB READ IT (`report_json`'s shape, the golden contract): a
/// report crosses from the asset's page (its `tree()` session) to the own tree's session only in that form, so this
/// reads the one shape rather than a second encoding. Only a `done` pass writes (an `unmeasured` one measured nothing,
/// a running one has not ended); a report that does not read as `done` writes nothing.
pub fn write_back_json(existing: Option<Keep>, own: bool, report: &Value) -> Option<Keep> {
    if report.get("state").and_then(Value::as_str) != Some(PassState::Done.word()) {
        return None;
    }
    let n = |k: &str| report.get(k).and_then(Value::as_u64).and_then(|x| u32::try_from(x).ok());
    let damaged = u32::try_from(report.get("damaged")?.as_array()?.len()).ok()?;
    let finished_at = report.get("finished_at").and_then(Value::as_u64)?;
    let base = record_or_default(existing, own);
    Some(Keep { audited_at: finished_at / 1000, health: Counts { groups: n("groups")?, whole: n("whole")?, degraded: n("degraded")?, damaged }, ..base })
}

/// A lane by the name `notAnswering({ lane })` takes. The names are [`lane_name`]'s, the one list: every lane there
/// is, matched exhaustively, so a new lane has no name until one is written.
pub fn lane_of(name: &str) -> Option<page::Lane> {
    [page::Lane::Interactive, page::Lane::Background].into_iter().find(|l| lane_name(*l) == name)
}

/// A lane's name, as JavaScript says it.
pub fn lane_name(lane: page::Lane) -> &'static str {
    match lane {
        page::Lane::Interactive => "interactive",
        page::Lane::Background => "background",
    }
}
