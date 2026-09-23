//! THE PAGE PATH in the web Session (ruling B; the switch-over made it the
//! only one). `Session` cannot be built natively, so this reads the source,
//! as the other web wiring gates do: the decisions are tested natively in
//! `page` and `page-io` (`page-io/tests/wire_node.rs` drives a whole write
//! through real frames), and what those cannot see is whether the Session
//! still ROUTES to them.
//!
//! * there is NO delegate path left to route to: no engine-delegate framing,
//!   no install plan, no mode flag;
//! * page-io is OWNED BY THE STORE (`PageStore`, READ-STATE): the store's
//!   frames reach it there (`PageStore::sync`), node frames go to it,
//!   provisioning goes to the signer through it;
//! * the Session keeps no head, root, row or range (READ-STATE's inventory);
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
    let pump = body_of(&src, "pump_page");
    assert!(pump.contains("self.db.store_mut().sync()"), "the store's frames do not reach page-io:\n{pump}");
    let inbound = body_of(&src, "on_inbound");
    assert!(inbound.contains("p.inbound(bytes,"), "node frames do not go to page-io:\n{inbound}");
    let prov = body_of(&src, "provision");
    assert!(prov.contains("self.signer_code = signer;") && prov.contains("self.provision_page(block, register);"), "provisioning does not go to the signer:\n{prov}");
    let page = body_of(&src, "provision_page");
    // It ASKS the signer first (`begin`); what it sends is tested on the real
    // Session in tests/js/page-identity.test.mjs.
    assert!(page.contains("io.begin(container)") && page.contains("self.db.store_mut().set_host(io)"), "provision_page does not open through the signer, or does not hand page-io to the store:\n{page}");
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

/// ONE OWNER FOR THE HEAD (READ-STATE inv. 1): the Session stores no head,
/// root, row or range — each was a copy of the engine's, and every defect of
/// 2026-09-23 was one of them going stale. LIVE is the tree's diff from each
/// binding's `RenderedAt` (`LiveBindings`, tested natively in
/// `testkit/tests/read_state_model.rs`), and this client's OWN writes changing
/// state are reported by the app-relative domain its bindings are keyed by.
#[test]
fn the_session_keeps_no_copy_of_the_head_and_reports_changes_from_the_tree() {
    let src = session_src();
    for gone in ["head_root", "seen_published", "head_moved", "head_named", "self.loads", "self.refresh", "self.cold", "take_adopted", "mark_stale", ".on_page(", ".on_delta("] {
        assert!(!src.contains(gone), "the Session still keeps a copy the engine owns: `{gone}`");
    }
    let stale = body_of(&src, "take_stale");
    assert!(stale.contains("self.bound.take_changed(") && stale.contains(".head()"), "LIVE is not the diff from each binding's RenderedAt to the engine's root:\n{stale}");
    let own = body_of(&src, "take_state_changed");
    // By the APP-RELATIVE domain (`own_domains_of_keys`, pinned natively in
    // tests/cached_store.rs): `domain_of_key` is the STORED `<app>.<name>`
    // since craftworks-sdk#267 — keyed by no binding, so a plain table said
    // "saving" for good (builder#107, back).
    assert!(
        own.contains(".take_state_changed()") && own.contains("own_domains_of_keys") && own.contains("self.app"),
        "own write states are not reported by the app-relative domain the bindings are keyed by:\n{own}"
    );
}

/// A woken read is RESUMED at its ticket's root, so a chain of hops ends on
/// one tree (READ-STATE inv. 2; the architect's chase). The decision is the
/// store's (`PageStore::resume`, planted as a mutant in the model test; its
/// pin ends in `PageStore::decide`); what only this file can see is that the
/// Session exposes it and decides every result through the store.
#[test]
fn a_woken_read_resumes_at_its_tickets_root_and_every_call_unpins() {
    let src = session_src();
    assert!(body_of(&src, "resume").contains("self.db.store_mut().resume(ticket)"), "the Session does not resume a ticket");
    let at = src.find("fn decide<").expect("fn decide");
    let decide = &src[at..at + src[at..].find("\n    }\n").expect("its end")];
    // `PageStore::decide` ends the pin; it is called for every result.
    assert!(decide.contains("store_mut().decide(r)"), "the Session decides a result itself:\n{decide}");
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
