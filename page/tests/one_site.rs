//! ONE SITE (the architect's check 2 on sdk#407), held by the source: a deadline leaves only through `Page::end`,
//! arrives only through `send` and `park`, and an op reaches the wire only from `send` and `reconnected` -- each of
//! which records. A new path that bypassed them would be an op the page's recording never saw.
//!
//! The cut between production and tests is the ONE shared `production_of` (sdk#419): this scan once cut at the first
//! `#[cfg(test)]` anywhere, and #401's test-only field cut it early -- every count below read 0 over nothing.

mod common;
use common::{items_after_the_cut, production_of};

const LIB: &str = include_str!("../src/lib.rs");

/// The counts a scan of production code makes, one owner for the three patterns.
fn counts(production: &str) -> [usize; 3] {
    [
        production.matches(concat!("deadlines", ".remove(")).count(),
        production.matches(concat!("deadlines", ".insert(")).count(),
        production.matches(concat!("self.out", ".push(")).count(),
    ]
}

#[test]
fn a_deadline_ends_only_through_end_and_an_op_goes_out_only_from_send_or_reconnected() {
    let lib = production_of(LIB);
    assert!(lib.contains("fn end(") && lib.contains("fn send(") && lib.len() * 2 > LIB.len(), "THE CONTROL: the production cut of lib.rs lost the code it scans ({} of {} bytes)", lib.len(), LIB.len());
    let stray = items_after_the_cut(LIB);
    assert!(stray.is_empty(), "page/src/lib.rs: production items after the production cut, where this scan does not look: {stray:?}");
    let [removes, inserts, pushes] = counts(lib);
    assert_eq!(removes, 1, "a deadline is removed outside `end`: its end is not recorded");
    assert_eq!(inserts, 2, "a deadline is made outside `send`/`park`");
    assert_eq!(pushes, 2, "an op is put on the wire outside `send`/`reconnected`: it is not recorded");
}

/// THE CONTROLS of the one cut (sdk#419's acceptance): a `#[cfg(test)]` FIELD before the cut -- indented, or a test-only
/// top-level fn -- leaves the production part and its counts unchanged; the cut is the first top-level test MODULE;
/// and a non-test item AFTER the cut is refused by name.
#[test]
fn the_cut_is_the_first_test_module_and_nothing_else_hides_after_it() {
    let plain = "struct Page {\n    x: u8,\n}\nfn end() { deadlines.remove(1); }\n#[cfg(test)]\nmod tests {\n    fn t() { deadlines.remove(2); }\n}\n";
    let field = "struct Page {\n    #[cfg(test)]\n    confirmed_steps: u8,\n    x: u8,\n}\nfn end() { deadlines.remove(1); }\n#[cfg(test)]\nmod tests {\n    fn t() { deadlines.remove(2); }\n}\n";
    let test_fn = "#[cfg(test)]\nfn helper() {}\nfn end() { deadlines.remove(1); }\n#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n    fn t() { deadlines.remove(2); }\n}\n";
    // The architect on #423: the cut looks past DOC COMMENTS to the item, and a `pub` / `pub(crate)` module is a module.
    let doc = "fn end() { deadlines.remove(1); }\n#[cfg(test)]\n/// The tests.\nmod tests {\n    fn t() { deadlines.remove(2); }\n}\n";
    let public = "fn end() { deadlines.remove(1); }\n#[cfg(test)]\npub(crate) mod tests {\n    fn t() { deadlines.remove(2); }\n}\n";
    for (name, src) in [("plain", plain), ("a test-only field", field), ("a test-only fn", test_fn), ("a doc comment before the mod", doc), ("a pub(crate) mod", public)] {
        assert_eq!(counts(production_of(src))[0], 1, "{name}: the cut moved (the test module's removal counted, or production's lost)");
        assert!(items_after_the_cut(src).is_empty(), "{name}: a test module after the cut was taken for production");
    }
    let hidden = "fn end() {}\n#[cfg(test)]\nmod tests {}\nfn escaped() { deadlines.remove(9); }\n";
    assert_eq!(items_after_the_cut(hidden), vec![(4, "fn escaped() { deadlines.remove(9); }".to_string())], "a production item after the cut was not refused by name");
}
