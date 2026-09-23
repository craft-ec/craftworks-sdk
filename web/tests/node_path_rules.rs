//! THE OWNER'S RULES ON THE NODE PATH, enforced by the gate (AGREED-RULES,
//! "Enforced by the gate"): a PR that breaks one fails here on its own.
//!
//! * **Rule 5 — one path to the node, `Page::send`.** Every frame that reaches
//!   a node is framed in ONE place, page-io's `pump`, from the ops the PAGE
//!   emits (and so from `Page::send`, which owns every deadline and re-send);
//!   the web Session's outbound is filled ONLY from page-io's frames. No
//!   other code calls a `wire::frame_*` function.
//! * **Rule 8 — no time cut-offs.** No constant or option on the send path
//!   is a budget, a lifetime or a give-up: a slow network never ends a write.
//!
//! These read the SOURCE, because what they forbid is a SHAPE a behaviour test
//! cannot see until the day it fires on a slow network. Each has a CONTROL:
//! the same check, run on a copy with the forbidden shape planted, fails.

const PAGE_IO: &str = include_str!("../../page-io/src/lib.rs");
const SESSION: &str = include_str!("../src/session.rs");
const PAGE: &str = include_str!("../../page/src/lib.rs");
const PAGE_STORE: &str = include_str!("../../src/page_store.rs");
const JS_SESSION: &str = include_str!("../../js/session.js");
const JS_CONNECTION: &str = include_str!("../../js/connection.js");

/// Lines that are code, not comments (a rule stated in a comment is not a
/// breach of it).
fn code_lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    src.lines().enumerate().filter(|(_, l)| {
        let t = l.trim_start();
        !(t.starts_with("//") || t.starts_with("*") || t.starts_with("/*"))
    })
}

/// The body of `fn name(` in `src` (to its closing brace at the fn's indent).
fn body_of<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &src[start..];
    let end = rest.find("\n    }\n").map(|e| e + 6).unwrap_or(rest.len());
    &rest[..end]
}

/// Where page-io writes a frame: every `self.out.extend(` / `self.out.push(`.
fn frame_writes(src: &str) -> Vec<(usize, String)> {
    code_lines(src)
        .filter(|(_, l)| l.contains("self.out.extend(") || l.contains("self.out.push("))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

/// Calls of a `wire::frame_*` function, outside `pump`.
fn framings_outside_pump(src: &str) -> Vec<(usize, String)> {
    let pump = body_of(src, "pump");
    let (lo, hi) = {
        let at = src.find(pump).expect("pump in src");
        (src[..at].lines().count(), src[..at + pump.len()].lines().count())
    };
    code_lines(src)
        .filter(|(i, l)| !(lo..=hi).contains(i) && l.contains("wire::") && l.contains("frame_"))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

fn one_path(page_io: &str) -> Result<(), String> {
    let writes = frame_writes(page_io);
    if writes.len() != 1 {
        return Err(format!("page-io writes frames in {} places, not one: {writes:?}", writes.len()));
    }
    let pump = body_of(page_io, "pump");
    if !pump.contains(&writes[0].1) || !pump.contains("self.server.take_ops()") {
        return Err(format!("page-io's one frame write is not pump's framing of the PAGE's ops: {writes:?}"));
    }
    let stray = framings_outside_pump(page_io);
    if !stray.is_empty() {
        return Err(format!("page-io frames a request outside pump: {stray:?}"));
    }
    Ok(())
}

#[test]
fn only_page_send_puts_a_frame_on_the_wire() {
    one_path(PAGE_IO).expect("rule 5");
    // The web Session: its outbound is page-io's frames, and nothing else.
    let fills: Vec<_> = code_lines(SESSION).filter(|(_, l)| l.contains("self.out.extend(") || l.contains("self.out.push(")).collect();
    assert_eq!(fills.len(), 1, "the Session fills its outbound in {} places: {fills:?}", fills.len());
    assert!(fills[0].1.contains("frames"), "the Session's outbound is not page-io's frames: {fills:?}");
    let session_framing: Vec<_> = code_lines(SESSION).filter(|(_, l)| l.contains("wire::frame_")).collect();
    assert!(session_framing.is_empty(), "the Session frames a request itself: {session_framing:?}");
}

#[test]
fn control_a_second_sender_is_caught() {
    // A page-io that frames the signer's registration itself, as it did.
    let planted = PAGE_IO.replacen(
        "    fn next_stream(&mut self) -> u32 {",
        "    fn stray(&mut self) {\n        let f = wire::frame_register_delegate(self.signer_container.clone().unwrap(), 1).unwrap();\n        self.out.extend(f);\n    }\n\n    fn next_stream(&mut self) -> u32 {",
        1,
    );
    assert_ne!(planted, PAGE_IO, "the control did not plant anything");
    assert!(one_path(&planted).is_err(), "a second sender in page-io was not caught");
}

/// Names that ARE time cut-offs: a budget, a lifetime, a give-up.
fn cutoffs(src: &str) -> Vec<(usize, String)> {
    let words = ["BUDGET_MS", "LIFE_MS", "GIVE_UP", "CUTOFF", "budgetMs", "stallMs", "BudgetMs", "deadlineMs"];
    code_lines(src)
        .filter(|(_, l)| words.iter().any(|w| l.contains(w)))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

#[test]
fn no_time_cut_off_on_the_send_path() {
    for (name, src) in [
        ("page/src/lib.rs", PAGE),
        ("page-io/src/lib.rs", PAGE_IO),
        ("web/src/session.rs", SESSION),
        ("src/page_store.rs", PAGE_STORE),
        ("js/session.js", JS_SESSION),
        ("js/connection.js", JS_CONNECTION),
    ] {
        let found = cutoffs(src);
        assert!(found.is_empty(), "rule 8: a time cut-off on the send path in {name}: {found:?}");
    }
}

#[test]
fn control_a_cut_off_constant_is_caught() {
    let planted = format!("{PAGE}\npub const APP_PUT_BUDGET_MS: u64 = 120_000;\n");
    assert!(!cutoffs(&planted).is_empty(), "a planted budget constant was not caught");
    let js = format!("{JS_SESSION}\nexport async function wait({{ provisionBudgetMs = 60_000 }} = {{}}) {{}}\n");
    assert!(!cutoffs(&js).is_empty(), "a planted JS budget was not caught");
}
