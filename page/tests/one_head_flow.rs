//! ONE HEAD FLOW FOR BOTH TREES (sdk#399 step 4, the architect): the page's data tree and its observation tree are
//! published by the SAME read, sign, write, read-back and land -- one path, keyed by the tree -- and every per-head
//! fact lives in the tree's one `TreeState`. Half-generalised state is where a second tree corrupts the first, so it
//! is held by source, over page/src/lib.rs's production code, read at test time:
//!
//! 1. `Label::Head` is named only where the label and the tree are MAPPED (`Label::tree`, `Tree::label`) and in the
//!    words shown for a wait (`waiting_name`): no head-flow fn names the data head's label.
//! 2. No head-flow fn reaches the data tree's state as `self.data.` -- it takes its tree's through `tree(t)` /
//!    `tree_mut(t)` -- except the data tree's own, named call sites below, each with its reason.
//! 3. `Tree::Data` inside a head-flow fn only at those same named data-only sites.

mod common;
use common::production_of;

/// The head flow: every fn that reads, signs, writes, reads back or lands a tree's head, or keeps its timers.
const HEAD_FLOW: &[&str] = &[
    "answer",
    "tick",
    "tick_sends",
    "tick_register",
    "on_signer",
    "on_verify",
    "on_hint",
    "reconnected",
    "head_hint_for",
    "head_pushed_for",
    "read_back_owed",
    "note_record",
    "head_reads",
    "reading_head",
    "confirms",
    "on_read_back",
    "ask_land",
    "ask_sign",
    "ask_sign_for",
    "pub_mut",
    "pub_ref",
    "ledger_of",
    "adopt",
    "step",
    "drop_dead_head",
    "carry_out",
    "release",
    "engine_published",
    "end_unneeded_gets",
    "confirm",
    "next_due",
    "waiting",
];

/// The DATA TREE's own sites inside the head flow, `(fn, text)`, each for a stated reason.
const DATA_ONLY: &[(&str, &str)] = &[
    // The Server merges data groups, never observations: only the data engine's cut is held on a displacement.
    ("step", "t == Tree::Data && self.hold_on_displace"),
    ("step", "self.data.engine.hold_cut()"),
    // The ride-along's only trigger is the DATA head landing (OBSERVABILITY §3), where the one confirmation is sent.
    ("on_read_back", "if t == Tree::Data && self.tree(t).engine.published_seq() > from"),
    // The published-head floor is an app version's: the data head's alone.
    ("answer", "t == Tree::Data && self.head_floor"),
    // A client's effects are the data tree's clients'; the observation tree has none.
    ("carry_out", "client if t == Tree::Data"),
];

/// Where `Label::Head` may be named: the two halves of the one mapping, and the words shown for a wait.
const LABEL_HEAD_AT: &[&str] = &["tree", "label", "waiting_name"];

/// Each production fn of `src` (comments dropped): `(name, body)`. A fn's body runs from its first `{` to the brace
/// that closes it.
fn fns(src: &str) -> Vec<(String, String)> {
    let code: String = production_of(src).lines().map(|l| format!("{}\n", l.split("//").next().unwrap_or(""))).collect();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = code[at..].find("fn ") {
        let start = at + i;
        at = start + 3;
        if start > 0 && code[..start].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let name: String = code[start + 3..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        // The signature ends at the first `{` (a body) or `;` (a declaration) OUTSIDE brackets: `[T; 5]` is a type.
        let mut depth = 0i32;
        let mut ends = None;
        for (k, c) in code[start..].char_indices() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '{' | ';' if depth == 0 => {
                    ends = Some((start + k, c));
                    break;
                }
                _ => {}
            }
        }
        let Some((open, c)) = ends else { break };
        if c == ';' {
            continue; // a declaration (a trait's), no body
        }
        let mut depth = 0;
        let mut end = open;
        for (k, c) in code[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + k + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push((name, code[open..end].to_string()));
    }
    out
}

/// The rule over `src`: what breaks it, empty when it holds.
fn breaches(src: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for (name, body) in fns(src) {
        if body.contains("Label::Head") && !LABEL_HEAD_AT.contains(&name.as_str()) {
            bad.push(format!("fn {name} names Label::Head: the head flow takes its tree's label (`t.label()`)"));
        }
        if !HEAD_FLOW.contains(&name.as_str()) {
            continue;
        }
        let mut rest = body.clone();
        for (f, allowed) in DATA_ONLY {
            if *f == name {
                rest = rest.replace(allowed, "");
            }
        }
        if rest.contains("self.data.") {
            bad.push(format!("head-flow fn {name} reaches `self.data.`: a tree's state comes through `tree(t)`"));
        }
        if rest.contains("Tree::Data") {
            bad.push(format!("head-flow fn {name} names `Tree::Data` outside its named data-only sites"));
        }
    }
    bad
}

#[test]
fn the_head_flow_is_one_path_keyed_by_the_tree() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).expect("page/src/lib.rs");
    let found = fns(&src);
    let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
    // THE CONTROL: the scan found the functions it names (a renamed one is a list to update, never a silent pass).
    for f in HEAD_FLOW.iter().chain(LABEL_HEAD_AT) {
        assert!(names.contains(f), "THE CONTROL: no fn {f} in page/src/lib.rs -- the list is out of date");
    }
    for (f, allowed) in DATA_ONLY {
        let body = &found.iter().find(|(n, _)| n == f).expect("listed").1;
        assert!(body.contains(allowed), "THE CONTROL: fn {f} no longer holds `{allowed}` -- drop it from DATA_ONLY");
    }
    let bad = breaches(&src);
    assert!(bad.is_empty(), "the head flow is not one path keyed by the tree:\n{}", bad.join("\n"));
}

/// THE CONTROLS: the rule flags each half-generalised form, and passes the allowed ones.
#[test]
fn the_scan_flags_each_half_generalised_form() {
    let ok = "impl Label { fn tree(&self) -> Option<Tree> { match self { Label::Head => Some(Tree::Data), _ => None } } }\nimpl P {\n    fn on_verify(&mut self, t: Tree) { let v = self.tree(t).verify.clone(); self.send(Waiting::Verify(t), Op::ReadHead { label: t.label() }); }\n    fn step(&mut self, t: Tree) { if t == Tree::Data && self.hold_on_displace { self.data.engine.hold_cut(); } }\n    fn published(&self) -> u64 { self.data.engine.published_seq() }\n}\n";
    assert!(breaches(ok).is_empty(), "the allowed forms were flagged: {:?}", breaches(ok));
    for (name, src) in [
        ("the data label in the head flow", "impl P { fn on_verify(&mut self) { self.send(Waiting::Verify, Op::ReadHead { label: Label::Head }); } }\n"),
        ("the data tree's state reached directly", "impl P { fn on_hint(&mut self, t: Tree) { if self.data.verify.is_some() { return; } } }\n"),
        ("the data tree named in the head flow", "impl P { fn release(&mut self, t: Tree) { let x = self.tree(Tree::Data).held.len(); } }\n"),
        ("a comment does not hide a use on its line's code", "impl P { fn confirm(&mut self, t: Tree) { self.data.confirmed.insert(id); // fine?\n } }\n"),
    ] {
        assert!(!breaches(src).is_empty(), "{name} was not flagged");
    }
    // A return type with a `;` in brackets is a signature, not a declaration: its body is still scanned.
    assert!(!breaches("impl P { fn head_reads(t: Tree) -> [Waiting; 5] { let x = self.data.verify; } }\n").is_empty(), "a fn returning `[T; N]` escaped the scan");
    // A comment naming the label calls nothing.
    assert!(breaches("impl P { fn ask_sign(&mut self, t: Tree) { // was Label::Head\n } }\n").is_empty(), "a comment was taken for code");
}
