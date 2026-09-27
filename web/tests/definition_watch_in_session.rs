//! The browser session WATCHES a definition by type (§19 P3, `bind_definition`), never through the name rule.
//!
//! `Session` cannot be built natively, so this reads the source, as `band_watch_in_session.rs` does. The decision
//! (which head moves a definition's watch key is told of) is tested natively in `tests/definition_watch.rs`; what
//! that cannot see is whether the SESSION builds the key from `Db::definition_domain` -- a door that went through
//! `read_name` would watch the wrong key in an app's session (the name prefixed, or refused) and do so SILENTLY.

use std::path::Path;

fn session_src() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session.rs")).expect("web/src/session.rs")
}

fn body_of<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &src[start..];
    let end = rest.find("\n    }\n").map(|e| e + 6).unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn the_definition_doors_build_their_watch_key_by_type_never_through_the_name_rule() {
    let src = session_src();
    let key = body_of(&src, "definition_watch");
    assert!(key.contains("definition_domain("), "the definition's watch key is not built from `Db::definition_domain`:\n{key}");
    for door in ["bind_definition", "rendered_definition", "unbind_definition", "definition_watch"] {
        let body = body_of(&src, door);
        assert!(!body.contains("read_name("), "`{door}` goes through the name rule, which refuses or prefixes the reserved name:\n{body}");
    }
    for door in ["bind_definition", "rendered_definition", "unbind_definition"] {
        assert!(body_of(&src, door).contains("self.definition_watch(which)"), "`{door}` builds its key some other way");
    }
    assert!(body_of(&src, "rendered_definition").contains("answered_at()"), "rendered_definition does not record the root its read walked");
}

/// THE CONTROL: the reader finds real bodies, and `bind` (the domain door) DOES go through the name rule -- so the
/// check above can fail.
#[test]
fn control_the_reader_finds_the_bodies_and_the_domain_door_uses_the_name_rule() {
    let src = session_src();
    assert!(body_of(&src, "bind").contains("self.read_name(domain)"));
    assert!(body_of(&src, "bind_definition").contains("self.bound.bind(key)"));
}
