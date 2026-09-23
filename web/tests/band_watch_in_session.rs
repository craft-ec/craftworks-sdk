//! The browser session watches BANDS, not only domains (craftworks-sdk#137).
//!
//! `Session` cannot be built natively, so this reads the source: the
//! decisions (`Db::watch_range`, `LiveBindings::take_changed`) are tested in
//! `tests/domain_range.rs` and `testkit/tests/read_state_model.rs`, and what
//! those cannot see is whether the session still diffs OVER THEM — a
//! `domain_range` here would silently turn every band back into its whole
//! domain.

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
fn the_session_diffs_each_watch_keys_own_range() {
    let src = session_src();
    let stale = body_of(&src, "take_stale");
    assert!(stale.contains("watch_range"), "`take_stale` diffs over something other than the watch key's range:\n{stale}");
    assert!(!stale.contains("domain_range"), "`take_stale` widens a band back to its domain");
}

/// THE CONTROL: the reader finds a real body, so the check above can fail.
#[test]
fn control_the_reader_finds_the_body() {
    let src = session_src();
    assert!(body_of(&src, "take_stale").contains("self.bound.take_changed("));
}
