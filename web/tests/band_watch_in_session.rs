//! The browser session watches BANDS, not only domains (craftworks-sdk#137).
//!
//! `Session` cannot be built natively, so this reads the source: the
//! decisions (`Db::watch_range`, `Db::watch_key_of_range`) are tested in
//! `tests/refresh_native.rs` and `tests/domain_range.rs`, and what those
//! cannot see is whether the session still ASKS for them — a `domain_range`
//! here would silently turn every band back into its whole domain.

use std::path::Path;

fn session_src() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session.rs")).expect("web/src/session.rs")
}

/// The body of `fn <name>` in the source, to its first line that closes at
/// its own indentation.
fn body_of<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &src[start..];
    let end = rest.find("\n    }\n").map(|e| e + 6).unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn the_session_asks_what_changed_in_a_watch_keys_range_and_seeds_bands() {
    let src = session_src();
    let refresh = body_of(&src, "refresh");
    assert!(refresh.contains("watch_range("), "`refresh` asks over something other than the watch key's range:\n{refresh}");
    assert!(!refresh.contains("domain_range("), "`refresh` widens a band back to its domain");
    let note_local = body_of(&src, "note_local");
    assert!(note_local.contains("watch_range("), "`note_local` does not attribute to bands");
    assert!(src.contains("watch_key_of_range(&lo, &hi)"), "a completed load does not seed the band it loaded");
}

/// THE CONTROL: the reader finds a real body, so the check above can fail.
#[test]
fn control_the_reader_finds_the_body() {
    let src = session_src();
    assert!(body_of(&src, "refresh").contains("self.refresh.ask("));
}
