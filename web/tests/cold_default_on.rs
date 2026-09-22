//! The Session's OWN cold reads — the F52 mitigation for the engine delegate,
//! whose cold read could stall ≈ 60 s — are OFF on the page path, which the
//! switch-over made the only one: the in-page engine fetches every block
//! itself through page-io, each node call on its RTO (#227). Their removal is
//! sdk#258; until then this pins that they stay off and that asking for them
//! is refused by name rather than half-done.
//!
//! `Session` cannot be built natively, so this reads the source. The reader
//! itself (`craftworks_sdk::cold`) is tested in the sdk's `tests/cold_read.rs`.

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
fn provisioning_turns_the_sessions_cold_reads_off_and_turning_them_on_is_refused() {
    let src = session_src();
    let page = body_of(&src, "provision_page");
    assert!(page.contains("self.switch_cold(false"), "provisioning leaves the Session's cold reads on:\n{page}");
    let set = body_of(&src, "set_cold_reads");
    assert!(set.contains("self.unusable.push(") && set.contains("sdk#258"), "turning cold reads on is not refused by name:\n{set}");
    assert!(!src.contains("switch_cold(true"), "something still switches the Session's own cold reads ON");
}

/// THE CONTROL: the reader finds the real bodies, so the check above can fail.
#[test]
fn control_the_reader_finds_the_bodies() {
    let src = session_src();
    assert!(body_of(&src, "provision_page").contains("io.begin(container)"));
    assert!(body_of(&src, "switch_cold").contains("contract_deriver("));
}

/// The session's clock does not end a load the cold reader holds by its age.
/// Those loads end by their blocks' deadlines (`Loads::time_out_except`, tested
/// in the sdk's `tests/loads.rs`). A plain `time_out` here would end a cold
/// read of many blocks as UNAVAILABLE at 30 s while each block was still
/// inside its own deadline.
#[test]
fn the_tick_spares_the_loads_the_cold_reader_holds() {
    let src = session_src();
    let tick = body_of(&src, "tick");
    assert!(tick.contains("time_out_except(now, |id| cold.holds(id))"), "`tick` ends cold loads by their age:\n{tick}");
    assert!(!tick.contains("self.loads.time_out(now)"), "`tick` still ends every load by its age");
    assert!(tick.contains("self.cold.tick(now)"), "`tick` never drives the cold reader's clock");
}

/// The one-shot timer's entry runs the cold clock AND carries out what it
/// decided (the re-fetches it queued, the give-ups it reported); its partner
/// reports when that is next due.
#[test]
fn the_cold_timer_entry_ticks_and_pumps() {
    let src = session_src();
    let tick = body_of(&src, "cold_tick");
    assert!(tick.contains("self.cold.tick(") && tick.contains("self.pump_cold()"), "`cold_tick` does not tick and pump:\n{tick}");
    assert!(body_of(&src, "cold_due_ms").contains("next_due_ms("), "`cold_due_ms` does not ask the cold reader");
}
