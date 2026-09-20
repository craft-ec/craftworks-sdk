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
    rest[..end].to_string()
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

/// The decision itself lives in the SDK, where something native can run it.
///
/// If it moves back into this file it becomes unreachable again — and that is
/// how it was possible to reintroduce sdk#89 with the suite green.
#[test]
fn the_decision_is_delegated_to_the_sdk() {
    let body = body_of("decide");
    assert!(
        body.contains("craftworks_sdk::decide("),
        "`Session::decide` no longer calls the SDK's. Inline here, nothing native can \
         reach it: `Session` is #[wasm_bindgen] in a cdylib, so the only test that could \
         run it is a fake session in JavaScript — which compiles this file and runs none \
         of it."
    );
}
