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

/// The page path tells the UI what the delegate path used to (builder#107):
/// the in-page engine's PUBLISHED head moving sets `head_moved` (the delegate
/// path's HeadChanged, which re-asks LIVE bindings), and this client's OWN
/// writes changing state are reported by domain for every binding.
#[test]
fn the_page_reports_head_moves_and_own_write_states() {
    let src = session_src();
    let pump = body_of(&src, "pump_page");
    assert!(pump.contains("p.server.page.published()") && pump.contains("self.head_moved = true"), "a published head move does not mark the head moved:\n{pump}");
    let own = body_of(&src, "take_state_changed");
    // By the APP-RELATIVE domain (`own_domains_of_keys`, pinned natively in
    // tests/cached_store.rs): this used to require `domain_of_key` here, which
    // is the STORED `<app>.<name>` since craftworks-sdk#267 — keyed by no
    // binding, so a plain table said "saving" for good (builder#107, back).
    assert!(
        own.contains(".take_state_changed()") && own.contains("own_domains_of_keys") && own.contains("self.app"),
        "own write states are not reported by the app-relative domain the bindings are keyed by:\n{own}"
    );
}

/// sdk#266: the Session ROUTES adoption and the re-ask. The decisions are
/// tested natively (`craftworks-sdk/tests/stale_on_adopt.rs` for the copy and
/// the delta, `testkit/tests/read_when_needed.rs` for a whole tab reading at
/// a head it adopted); what only this file can see is whether the Session
/// still calls them.
#[test]
fn an_adopted_head_makes_the_ranges_stale_and_a_read_re_asks() {
    let src = session_src();
    let pump = body_of(&src, "pump_page");
    assert!(
        pump.contains("take_adopted()") && pump.contains("mark_stale()"),
        "a head this page ADOPTED does not make its loaded ranges stale, so a read answers at a head the page has left:\n{pump}"
    );
    let delta = body_of(&src, "on_delta");
    assert!(
        delta.contains("parking::delta_for_read"),
        "a delta answering a READ's own ticket is not routed to it, so the read waits out its budget:\n{delta}"
    );
    let full = body_of(&src, "on_full_reload");
    assert!(
        full.contains("parking::full_reload_for_read"),
        "a re-ask the engine could not diff does not fall back to a full load under the read's ticket:\n{full}"
    );
}

/// sdk#259: `live_mode` reports the PAGE PATH's subscription — page-io's head
/// read with subscribe — and not the Session's `watching`, which belongs to
/// the delegate path the switch-over deleted. Reported from `watching`, a
/// page that held the subscription the whole time said "Polled", and the
/// two-tab probe's first item read "not subscribed, so its number is the
/// tick's number wearing a different name".
#[test]
fn live_mode_reports_the_pages_own_head_subscription() {
    let src = session_src();
    let body = body_of(&src, "live_mode");
    assert!(
        body.contains("head_subscription()"),
        "live_mode does not read page-io's head subscription, so it describes a path that no longer exists:\n{body}"
    );
    assert!(
        !src.contains("self.watching") && !src.contains("self.subscribed"),
        "the delegate path's watch flags are still in the Session: they are never set now, so anything reading them reports a path that does not exist"
    );
    assert!(body.contains("headChanges"), "what the subscription has DELIVERED is not reported, so subscribed cannot be told from subscribed-and-being-told:\n{body}");
    // The MAPPING is page-io's (`HeadSubscription::live_mode`, pinned branch
    // by branch natively). The Session only serializes it: it calls it, and
    // it names no mode of its own — a literal here is a second mapping no
    // native test can reach (a mutant making "subscribed" unreachable in the
    // Session survived every suite).
    assert!(body.contains(".live_mode()"), "live_mode does not use page-io's mapping:\n{body}");
    for literal in ["\"HeadSubscribed\"", "\"Polled\""] {
        assert!(!body.contains(literal), "live_mode decides a mode itself ({literal}), outside the mapping the native test pins:\n{body}");
    }
}

/// A loaded page is recorded at the root it was READ at (`at.root`), never at
/// `head_root` — which is set from `Identity` only and stays the first head
/// on the page path. Recorded at the stale root, every load after a delta
/// looked like another tree and wiped the whole copy (measured in sdk#282's
/// acceptance run: a read of another app's notes answered NOT_LOADED for ever).
#[test]
fn a_loaded_page_is_recorded_at_the_root_it_was_read_at() {
    let src = session_src();
    let at = src.find("craftworks_sdk::loads::Page::Complete { lo, hi, rows, at } =>").expect("the page-complete arm");
    let arm = &src[at..at + src[at..].find("craftworks_sdk::loads::Page::Restart").expect("the next arm")];
    assert!(arm.contains("on_page(&lo, &hi, rows, at.root)"), "a completed page is not recorded at the root it was read at:\n{arm}");
    assert!(!arm.contains("self.head_root"), "a completed page is recorded at head_root, the stale first head:\n{arm}");
}
