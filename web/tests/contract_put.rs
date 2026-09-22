//! A PUT of a contract the APP names (builder#104), wired through `Session`.
//!
//! `Session` cannot be built natively, so this reads the source, as the other
//! web wiring gates do. The decisions are tested natively: the key and the
//! matching in `wire/tests/puts.rs`, the page-mode path end to end in
//! `page-io/tests/wire_node.rs`. What those cannot see is whether the Session
//! still ROUTES the node's answers to them:
//!
//! * an ack is offered to the app's PUTs BEFORE the provisioning plan, which
//!   would otherwise take a `Put` ack for one of its own steps;
//! * a PUT refusal naming its contract goes to the app's PUTs, and only one
//!   that is not theirs to the plan (as every refusal went before);
//! * in page mode the PUT goes out through page-io — the only path to the
//!   node there — and page-io's handed-back answers reach the app's PUTs;
//! * a dropped socket leaves what was waiting `Unanswered`.

const SRC: &str = include_str!("../src/session.rs");

fn body_of(name: &str) -> &'static str {
    let start = SRC.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &SRC[start..];
    let end = rest.find("\n    }\n").map(|e| e + 6).unwrap_or(rest.len());
    &rest[..end]
}

fn before(body: &str, first: &str, then: &str) -> bool {
    matches!((body.find(first), body.find(then)), (Some(a), Some(b)) if a < b)
}

#[test]
fn an_ack_is_offered_to_the_apps_puts_before_the_plan() {
    let b = body_of("on_ack");
    assert!(before(b, "self.puts.acked(key)", "self.plan.on_ack(&kind)"), "the plan sees a Put ack first:\n{b}");
}

#[test]
fn a_named_put_refusal_goes_to_the_apps_puts_and_only_the_rest_to_the_plan() {
    let b = body_of("on_inbound");
    let arm = &b[b.find("Incoming::PutFailed { key, said } =>").expect("no PutFailed arm")..];
    let arm = &arm[..arm.find("Incoming::Refused(why)").expect("the Refused arm follows")];
    assert!(
        arm.contains("if !self.puts.refused(&key, &said) {") && arm.contains("self.plan.on_refused(&said);"),
        "a PUT refusal is not attributed by key, or an unowned one no longer reaches the plan:\n{arm}"
    );
}

#[test]
fn in_page_mode_the_put_goes_through_page_io_and_its_answer_comes_back() {
    let put = body_of("put_contract");
    let page = &put[put.find("if self.page_mode {").expect("a page-mode branch")..put.find("} else {").expect("an else")];
    assert!(page.contains("p.put_contract(contract, state)") && !page.contains("self.out"), "page mode PUTs around page-io:\n{page}");
    let pump = body_of("pump_page");
    assert!(pump.contains("p.take_others()"), "page-io's handed-back answers are never read:\n{pump}");
    for m in ["self.puts.acked(key)", "self.puts.refused(key, said)"] {
        assert!(pump.contains(m), "`{m}` is not in pump_page:\n{pump}");
    }
}

#[test]
fn a_dropped_socket_leaves_waiting_puts_unanswered() {
    assert!(body_of("reconnected").contains("self.puts.connection_lost();"));
}

/// THE CONTROL: the reader finds real bodies, and `before` can say no.
#[test]
fn control_the_reader_finds_the_bodies() {
    assert!(body_of("put_contract").contains("wire::puts::contract(&code, &params, &state)"));
    assert!(!before(body_of("on_ack"), "self.plan.on_ack(&kind)", "self.puts.acked(key)"));
}
