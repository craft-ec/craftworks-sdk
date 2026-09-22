//! THE PAGE PATH in the web Session (ruling B; the switch-over made it the
//! only one). `Session` cannot be built natively, so this reads the source,
//! as the other web wiring gates do: the decisions are tested natively in
//! `page` and `page-io` (`page-io/tests/wire_node.rs` drives a whole write
//! through real frames), and what those cannot see is whether the Session
//! still ROUTES to them.
//!
//! * there is NO delegate path left to route to: no engine-delegate framing,
//!   no install plan, no mode flag;
//! * the store's protocol frames go to page-io; node frames go to page-io;
//!   provisioning goes to the signer through page-io;
//! * the page's own cold reads are off (the in-page engine fetches blocks
//!   itself, through page-io, on the RTO estimator);
//! * the one-shot timer and the tick drive page-io.

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
fn there_is_no_delegate_path_left() {
    let src = session_src();
    for gone in ["frame_engine_request", "frame_register_delegate", "Provisioner", "page_mode", "fn advance(", "wire::unframe"] {
        assert!(!src.contains(gone), "the delegate path is still in the Session: `{gone}`");
    }
}

#[test]
fn every_path_to_the_node_is_page_io() {
    let src = session_src();
    let env = body_of(&src, "envelope_engine_requests");
    assert!(env.contains(".client(&bytes)"), "the store's frames do not go to page-io:\n{env}");
    let inbound = body_of(&src, "on_inbound");
    assert!(inbound.contains("p.inbound(bytes,"), "node frames do not go to page-io:\n{inbound}");
    let prov = body_of(&src, "provision");
    assert!(prov.contains("self.signer_code = signer;") && prov.contains("self.provision_page(block, register);"), "provisioning does not go to the signer:\n{prov}");
    let page = body_of(&src, "provision_page");
    // It ASKS the signer first (`begin`); what it sends is tested on the real
    // Session in tests/js/page-identity.test.mjs.
    assert!(page.contains("io.begin(container)") && page.contains("self.switch_cold(false"), "provision_page does not open through the signer or leaves cold reads on:\n{page}");
}

#[test]
fn the_tick_and_the_timer_drive_page_io() {
    let src = session_src();
    for f in ["tick", "cold_tick"] {
        assert!(body_of(&src, f).contains("p.tick(page::Ms(now))"), "`{f}` does not tick page-io");
    }
    assert!(body_of(&src, "cold_due_ms").contains("p.next_due()"), "the one-shot timer ignores page-io's next due");
}

/// THE CONTROL: the reader finds real bodies.
#[test]
fn control_the_reader_finds_the_bodies() {
    let src = session_src();
    assert!(body_of(&src, "pump_page").contains("take_frames()"));
}
