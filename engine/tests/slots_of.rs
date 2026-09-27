//! THE CONTROL (the architect on sdk#542): an id among a group's slots is looked up in ONE place,
//! `engine::repair::slots_of`, which returns EVERY slot holding it. Identical values are one block, so one id can fill
//! several slots; a FIRST-MATCH lookup marks one of them and was the defect twice (sdk#527's mark, sdk#542's repair).
//! A first match by the element's own equality -- `.position(|s| *s == id)` and its spellings -- or any
//! `.iter().position(` over `slots` / `members`, in the engine, the page or page-io, fails this test, so a third copy
//! cannot merge.

use std::path::{Path, PathBuf};

/// A line looks an element up by FIRST match on its own equality: `.position(|p| <p> == ..)` or `.. == <p>)`, where
/// `<p>` is the closure's parameter itself (`p`, `*p`, `&p`) -- not a field of it (`p.client == ..` is a keyed lookup of
/// a record, not an id among slots). Or any `.iter().position(` over `slots` or `members`.
fn first_match(l: &str) -> bool {
    if ["slots.iter().position(", "members.iter().position("].iter().any(|p| l.contains(p)) {
        return true;
    }
    let mut rest = l;
    while let Some(at) = rest.find(".position(|") {
        let after = &rest[at + ".position(|".len()..];
        let Some(end) = after.find('|') else { break };
        let param = after[..end].trim().trim_start_matches('&');
        let body = &after[end + 1..];
        let body = &body[..body.find(')').unwrap_or(body.len())];
        let simple = !param.is_empty() && param.chars().all(|c| c.is_alphanumeric() || c == '_');
        if simple {
            let me = |side: &str| side.trim().trim_start_matches(['*', '&']) == param;
            if let Some((lhs, rhs)) = body.split_once("==") {
                if me(lhs) || me(rhs) {
                    return true;
                }
            }
        }
        rest = after;
    }
    false
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn an_id_among_slots_is_looked_up_only_through_slots_of() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("the workspace");
    let mut files = Vec::new();
    for dir in ["engine/src", "page/src", "page-io/src"] {
        let before = files.len();
        rust_files(&root.join(dir), &mut files);
        // Not vacuous: the walk found each crate's sources (engine::repair among them).
        assert!(files.len() > before, "the scan found no .rs file under {dir}: it is not looking at it");
    }
    assert!(files.iter().any(|f| f.ends_with("engine/src/repair.rs")), "the scan did not reach engine/src/repair.rs");
    let found: Vec<String> = files
        .iter()
        .flat_map(|f| {
            let text = std::fs::read_to_string(f).unwrap_or_default();
            let hits: Vec<String> = text.lines().filter(|l| !l.trim_start().starts_with("//")).filter(|l| first_match(l)).map(|l| l.trim().to_string()).collect();
            hits.into_iter().map(move |l| format!("{}: {l}", f.strip_prefix(root).unwrap_or(f).display()))
        })
        .collect();
    assert!(found.is_empty(), "a first-match lookup of an id outside engine::repair::slots_of (use slots_of: EVERY slot):\n{}", found.join("\n"));
}

/// The detector's control: each shape of the defect IS caught (three forms, and the three sites sdk#542 converted: the repair's, find_group's, and app_publish's mark), and the
/// lookups it must leave alone are not.
#[test]
fn the_detector_catches_each_first_match_shape() {
    for caught in [
        "            let Some(i) = r.group.slots.iter().position(|s| *s == slot) else { continue };",
        "            let Some(missing_ix) = members.iter().position(|m| *m == missing) else { continue };",
        "    let i = s.keys.iter().position(|k| k == key)?;",
        "    let at = group.slots.iter().position(|s| s == &id);",
        "    let at = cids.iter().position(|&c| c == id).unwrap();",
        "    let at = cids.iter().position(|c| id == *c)?;",
        "    let at = slots.iter().position(|s| s.eq(&id));",
    ] {
        assert!(first_match(caught), "not caught: {caught}");
    }
    for fine in [
        "            let Some(missing_ix) = slots_of(&members, &missing).next() else { continue };",
        "        if let Some(i) = self.queue.iter().position(|q| (q.client, q.write_id) == (client, write_id)) {",
        "            if let Some(i) = self.queue.iter().position(|q| q.warm_after.is_none()) {",
        "        let Some(at) = c.deferred.iter().position(|(d, _)| g.new.contains(d)) else { return Vec::new() };",
    ] {
        assert!(!first_match(fine), "a lookup that is not the defect was caught: {fine}");
    }
}
