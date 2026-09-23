//! The Session's OWN cold reads — the F52 mitigation for the engine delegate,
//! whose cold read could stall ≈ 60 s — are GONE (sdk#258, READ-STATE): the
//! in-page engine fetches every block itself through page-io, each node call
//! on its RTO (#227), and a read that needs one waits on the engine's read at
//! its root. This pins that nothing of the old reader is left in the Session
//! and that asking for it is refused by name rather than half-done.
//!
//! `Session` cannot be built natively, so this reads the source.

use std::path::Path;

fn session_src() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session.rs")).expect("web/src/session.rs")
}

/// The body of `fn <name>` in the source, up to its first line that closes at
/// its own indentation.
fn body_of<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &src[start..];
    let end = rest.find("\n    }\n").map(|e| e + 6).unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn the_sessions_own_cold_reads_are_gone_and_turning_them_on_is_refused() {
    let src = session_src();
    for gone in ["ColdReads", "switch_cold", "pump_cold", "cold_contract", "take_cold_log"] {
        assert!(!src.contains(gone), "the Session's own cold reader is still here: `{gone}`");
    }
    let set = body_of(&src, "set_cold_reads");
    assert!(set.contains("self.unusable.push(") && set.contains("sdk#258"), "turning cold reads on is not refused by name:\n{set}");
}

/// The one-shot timer's entry drives page-io's clock and carries out what it
/// decided; its partner reports when that is next due.
#[test]
fn the_cold_timer_entry_ticks_page_io_and_pumps() {
    let src = session_src();
    let tick = body_of(&src, "cold_tick");
    assert!(tick.contains("p.tick(page::Ms(now))") && tick.contains("self.pump_page()"), "`cold_tick` does not tick page-io and pump:\n{tick}");
    assert!(body_of(&src, "cold_due_ms").contains("p.next_due()"), "`cold_due_ms` does not ask page-io");
}

/// THE CONTROL: the reader finds the real bodies, so the checks above can fail.
#[test]
fn control_the_reader_finds_the_bodies() {
    let src = session_src();
    assert!(body_of(&src, "provision_page").contains("io.begin(container)"));
    assert!(body_of(&src, "set_cold_reads").contains("self.unusable.push("));
}
