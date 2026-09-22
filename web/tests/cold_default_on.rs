//! Cold reads in the page are ON by default, and the builder can turn them off.
//!
//! `Session` cannot be built natively, so this reads the source. The reader
//! itself (`craftworks_sdk::cold`) is tested in the sdk's `tests/cold_read.rs`.
//! What those tests cannot see is whether a page that never calls
//! `set_cold_reads` gets it switched on. Without that, the default would
//! silently be OFF and every cold range would go back to the engine's F52 stall.

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
fn provisioning_switches_cold_reads_on_unless_the_builder_chose() {
    let src = session_src();
    let provision = body_of(&src, "provision");
    assert!(
        provision.contains("if !self.cold_chosen") && provision.contains("self.switch_cold(true, block.clone())"),
        "`provision` does not switch cold reads on by default:\n{provision}"
    );
    let set = body_of(&src, "set_cold_reads");
    assert!(set.contains("self.cold_chosen = true"), "the builder's choice is not remembered:\n{set}");
    assert!(set.contains("self.switch_cold(on, block_code)"), "the builder's choice is not applied:\n{set}");
}

/// THE CONTROL: the reader finds the real bodies, so the check above can fail.
#[test]
fn control_the_reader_finds_the_bodies() {
    let src = session_src();
    assert!(body_of(&src, "provision").contains("self.artefacts = Some("));
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

/// The cold clock runs on every GET answer, not only on the page's 1 s tick.
/// The live runs ticked it at that cadence (after every message, at least
/// every 100 ms); a page that ticked it once a second would notice a late
/// fetch up to a second after its RTO.
#[test]
fn every_get_answer_also_runs_the_cold_clock() {
    let src = session_src();
    for arm in ["Incoming::Got { id, state } =>", "Incoming::GetFailed { id } =>"] {
        let at = src.find(arm).unwrap_or_else(|| panic!("no arm {arm}"));
        let body = &src[at..at + src[at..].find("None =>").expect("the arm's None")];
        assert!(body.contains("self.cold.tick(now)"), "{arm} does not run the cold clock:\n{body}");
    }
}
