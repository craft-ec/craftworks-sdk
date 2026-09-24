//! EVERY WRITE AND EVERY READ GOES THROUGH THE DECISION.
//!
//! `Session` is `#[wasm_bindgen]` in a `cdylib`: nothing native can build
//! one, so no test can CALL these methods. That is not a reason to leave the
//! wiring unchecked — it is the reason this file reads the source, the way
//! `surfaces_agree.rs` does.
//!
//! The gap this closes was found by review, not by guesswork. The sdk#89 fix
//! moved the recovery into `craftworks_sdk::parking` and tested it against a
//! real `Shell` in `tests/cold_write_native.rs` — and the defect could STILL
//! be put back here:
//!
//! ```ignore
//! self.db.define(domain, &s).map_err(|e| db_err(&e))   // ticketless again
//! ```
//!
//! The build succeeded, every JavaScript test passed, all four native cold-
//! write tests passed. They exercise the decision; nothing checked that this
//! file still ASKS for it. A helper nothing calls is the shape that has cost
//! this repo more than anything else.
//!
//! So: each method that returns a `Db` result must hand it to `answer` (reads)
//! or `applied` (writes). Both go through `Session::decide`, which is the one
//! place a `NotLoaded` becomes a ticket and a range request.

use std::collections::BTreeSet;

const SRC: &str = include_str!("../src/session.rs");

/// The methods whose result comes from `Db` and must therefore be decided.
///
/// Writes read before they apply — `define` reads the existing schema, and
/// `put`, `update` and `delete` go through `need_schema` — so a `NotLoaded`
/// from any of them means "I could not look", never "this is invalid", and is
/// retryable. That is the whole of sdk#89.
const MUST_DECIDE: &[&str] = &[
    "define", "put", "update", "delete", // writes that read first
    "schema", "domains", "get", "count", // reads
];

/// The body of `pub fn <name>`, to its closing brace at method indentation.
fn body_of(name: &str) -> String {
    // `(` for a plain method, `<` for a generic one. Matching only `name(`
    // silently found nothing for `decide<T>` — and a reader that finds
    // nothing reports everything compliant, which is the failure this whole
    // file exists to prevent, one level up.
    let at = SRC
        .find(&format!("fn {name}("))
        .or_else(|| SRC.find(&format!("fn {name}<")))
        .unwrap_or_else(|| panic!("no `fn {name}` in session.rs — this gate is reading nothing"));
    let rest = &SRC[at..];
    let end = rest
        .find("\n    }\n")
        .unwrap_or_else(|| panic!("could not find the end of `{name}`"));
    // COMMENTS STRIPPED. This gate asks what the CODE does, and the comments
    // around it name the very things it looks for — a warning not to reach
    // past the store to `copy.time_out` contains `copy.time_out`. Checking the
    // raw text made a method fail its own gate for explaining itself, and
    // would equally let a method pass by mentioning the right call in prose.
    rest[..end]
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_db_result_is_decided() {
    let mut bare = Vec::new();
    for name in MUST_DECIDE {
        let body = body_of(name);
        if !body.contains("self.decided(") && !body.contains("self.answer(") {
            bare.push(*name);
        }
    }
    assert!(
        bare.is_empty(),
        "these hand a `Db` result straight to the caller without deciding it: {bare:?}.\n\
         A `NotLoaded` then leaves with NO TICKET, so nothing can retry it — which is \
         sdk#89: a published app with a form on screen, a button saying Published, and \
         every write refused with `has no schema; define it first`. 120 of 120 refused \
         against a real node.\n\
         Route it through `self.decided(..)` for a write or `self.answer(..)` for a read."
    );
    println!("  {} Db-backed methods, all decided", MUST_DECIDE.len());
}

/// THE CONTROL: the reader really finds these methods and their bodies.
///
/// Without it, a `body_of` that returned an empty string — a rename, a
/// reformat, a changed signature — would report every method compliant for
/// ever. A gate that cannot read what it checks passes everything.
#[test]
fn control_the_reader_finds_real_bodies() {
    for name in MUST_DECIDE {
        let body = body_of(name);
        assert!(
            body.len() > 20,
            "the body read for `{name}` is {} bytes; the reader is broken, not the code",
            body.len()
        );
        assert!(
            body.contains("self.db."),
            "the body read for `{name}` never touches `self.db`, so it is not the method \
             this gate thinks it is"
        );
    }
    // And the list is not secretly empty or duplicated.
    let unique: BTreeSet<&&str> = MUST_DECIDE.iter().collect();
    assert_eq!(unique.len(), MUST_DECIDE.len(), "the list repeats itself");
    assert!(
        MUST_DECIDE.len() >= 8,
        "the list has shrunk; a method dropped from it is a method nothing checks"
    );
}

/// **THE PAGE'S TICK TIMES NOTHING OUT: no write, and no read.**
///
/// R-b (sdk#291): the outbox is gone with its timeout. A write waiting its
/// turn behind the engine's own commits is not "unanswered", and the tick must
/// never roll one back. And (rule 8, sdk#302) the store's own tick went too:
/// its only work was ending a read's ticket at `TICKET_LIFE_MS`, a time
/// cut-off. A ticket now ends only on its answer, so the store has NO tick and
/// the Session must not grow one back. The history below is why the call site
/// is checked at all.
///
/// `Session::tick` called `self.db.store_mut().copy.time_out(now)` — straight
/// to the COPY, which rolls back writes that waited too long and does nothing
/// else. Everything else `CachedStore::tick` does therefore never ran on a
/// page, and what it does besides rolling back is drain the OUTBOX: the
/// writes the engine refused with `Busy` while a commit was in flight, which
/// nothing else re-sends (sdk#106).
///
/// So that backstop was dead here the day it was written, and no JavaScript
/// test could see it: they drive a FAKE session, and a fake has whatever
/// methods the test gives it. Same gap as the parking in sdk#90 — the
/// decision moved somewhere testable and nothing checked the call site still
/// asked for it — so this is the same answer.
///
/// Reaching past a type's own entry point to one of its fields is the shape
/// to watch: the copy IS a field of the store, and touching it directly
/// skipped every decision the store makes around it.
#[test]
fn the_page_tick_times_nothing_out() {
    let body = body_of("tick");
    // The reader found the real tick: it tells the page's writes the time.
    assert!(body.contains("writes.send_tick(now)"), "the body read is not Session::tick");
    assert!(
        !body.contains("store_mut().tick("),
        "`Session::tick` calls a store tick again: the store's only tick ended reads by a clock (rule 8)."
    );
    assert!(
        !include_str!("../../src/page_store.rs").contains("pub fn tick("),
        "the page store has a tick again: its only work was a ticket LIFETIME, a time cut-off (rule 8)."
    );
    assert!(
        !body.contains("writes.tick()"),
        "`Session::tick` calls a write-half tick: nothing may time a queued write out (sdk#291)."
    );
    assert!(
        !body.contains("copy.time_out"),
        "`Session::tick` reaches past the store to `copy.time_out`. That rolls \
         back and nothing else: it was how the outbox backstop came to be dead \
         on every page while its own test passed."
    );
}

/// THE CONTROL: the reader can tell the two apart.
///
/// Without it, a `body_of` that returned an empty string would satisfy both
/// assertions above — the first by not finding the absence it checks for, and
/// the second by finding nothing at all. An empty body passes a `!contains`.
#[test]
fn control_the_tick_body_is_really_read() {
    let body = body_of("tick");
    assert!(
        body.len() > 80,
        "the body read for `tick` is {} bytes; the reader is broken, not the code",
        body.len()
    );
    assert!(
        body.contains("send_tick"),
        "the body read for `tick` does not send the delegate the time, so it is \
         not the method this gate thinks it is"
    );
}

/// The decision itself lives in the SDK, where something native can run it.
///
/// If it moves back into this file it becomes unreachable again — and that is
/// how it was possible to reintroduce sdk#89 with the suite green.
#[test]
fn the_decision_is_delegated_to_the_sdk() {
    let body = body_of("decide");
    assert!(
        // `PageStore::decide` (READ-STATE): the ticket is the walk's fetch.
        body.contains("self.db.store_mut().decide(r)"),
        "`Session::decide` no longer calls the SDK's. Inline here, nothing native can \
         reach it: `Session` is #[wasm_bindgen] in a cdylib, so the only test that could \
         run it is a fake session in JavaScript — which compiles this file and runs none \
         of it."
    );
}

/// sdk#174: THE PAGE'S TICK ASKS AFTER A WRITE STILL APPLYING. A cold write
/// whose path needs more blocks than one chain fetches continues only when
/// its client asks after it; nothing else asks. A `tick` that stopped asking
/// leaves such a write `Applying` until the engine fails it -- with every
/// native test green, because they drive the store directly.
#[test]
fn the_tick_asks_after_quiet_writes() {
    let body = body_of("tick");
    assert!(
        calls(&body, ".ask_after_applying();"),
        "`Session::tick` no longer asks after the write the engine is still applying (sdk#174)"
    );
}

/// Does this code CALL `statement` (e.g. `.ask_after_applying();`)? Line
/// comments, block comments and string literals are removed first: `body_of`
/// strips only `//`, so a `/* ask_after_applying() */` or a string naming it would
/// otherwise pass a gate about what the code DOES (sdk#196 merge review).
fn calls(code: &str, statement: &str) -> bool {
    let mut out = String::new();
    let b = code.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            match code[i + 2..].find("*/") {
                Some(e) => i += e + 4,
                None => break,
            }
        } else if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    let squeezed: String = out.split_whitespace().collect();
    let want: String = statement.split_whitespace().collect();
    squeezed.contains(&want)
}

/// THE CONTROL for the reader above: it finds a real call, and refuses the
/// same words in a line comment, a block comment, a string, or mentioned
/// without being called -- so `the_tick_asks_after_quiet_writes` can fail.
#[test]
fn control_only_a_real_call_counts_as_asking_after_applying() {
    assert!(calls("self.db.store_mut().ask_after_applying();", ".ask_after_applying();"));
    assert!(calls("self.db\n    .store_mut()\n    .ask_after_applying();", ".ask_after_applying();"));
    for fake in [
        "// self.db.store_mut().ask_after_applying();",
        "/* self.db.store_mut().ask_after_applying(); */",
        "let s = \"self.db.store_mut().ask_after_applying();\";",
        "let f = Store::ask_after_applying;",
    ] {
        assert!(!calls(fake, ".ask_after_applying();"), "the reader counted a non-call: {fake}");
    }
}
