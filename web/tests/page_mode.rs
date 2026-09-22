//! PAGE MODE in the web Session (ruling B, part 2). `Session` cannot be built
//! natively, so this reads the source, as the other web wiring gates do: the
//! decisions are tested natively in `page` and `page-io`
//! (`page-io/tests/wire_node.rs` drives a whole write through real frames),
//! and what those cannot see is whether the Session still ROUTES to them.
//!
//! * the flag is OFF by default;
//! * with it on, the store's protocol frames go to page-io, not to a delegate;
//!   node frames go to page-io; provisioning goes to the signer through
//!   page-io; the engine delegate's plan does not run — no other path;
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
fn page_mode_is_off_by_default() {
    let src = session_src();
    assert!(src.contains("            page_mode: false,"), "page mode is not OFF in `new`");
}

#[test]
fn in_page_mode_every_path_to_the_node_is_page_io() {
    let src = session_src();
    let env = body_of(&src, "envelope_engine_requests");
    assert!(env.contains("if self.page_mode {") && env.contains(".client(&bytes)"), "the store's frames do not go to page-io:\n{env}");
    let inbound = body_of(&src, "on_inbound");
    let head = &inbound[..inbound.find("match wire::unframe").expect("the delegate path")];
    assert!(head.contains("if self.page_mode {") && head.contains("p.inbound(bytes,") && head.contains("return owned;"), "node frames reach the delegate path in page mode:\n{head}");
    let prov = body_of(&src, "provision");
    assert!(prov.contains("if self.page_mode {") && prov.contains("self.provision_page(block, register);"), "provisioning does not go to the signer:\n{prov}");
    let page = body_of(&src, "provision_page");
    // It ASKS the signer first (`begin`); what it sends is tested on the real
    // Session in tests/js/page-identity.test.mjs.
    assert!(page.contains("io.begin(container)") && page.contains("self.switch_cold(false"), "provision_page does not open through the signer or leaves cold reads on:\n{page}");
    let adv = body_of(&src, "advance");
    assert!(adv.contains("if self.page_mode {\n            return;"), "the engine delegate's plan runs in page mode:\n{adv}");
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
