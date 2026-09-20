//! Provisioning, against a fake wire.
//!
//! No socket: the whole point of the state machine is that what a browser runs
//! is what a test runs.

use wire::provision::{Did, Provisioner, Step};
use wire::AckKind;

/// The name each step puts on the wire, as a test stands in for it.
fn named(step: Step) -> &'static str {
    match step {
        Step::Delegate => "delegate-key",
        Step::BlockContract => "block-contract-id",
        Step::RegisterContract => "register-contract-id",
        Step::Key => "",
    }
}

fn ack_for(step: Step) -> AckKind {
    match step {
        Step::Delegate => AckKind::Registered(named(step).into()),
        Step::BlockContract | Step::RegisterContract => AckKind::Put(named(step).into()),
        Step::Key => AckKind::Ok,
    }
}

/// Run a clean provisioning to completion.
fn run(p: &mut Provisioner) -> Vec<Step> {
    let mut order = Vec::new();
    let mut now = 0u64;
    while let Some(step) = p.next_step() {
        order.push(step);
        p.sent(step, named(step), now);
        now += 10;
        p.on_ack(&ack_for(step));
    }
    order
}

#[test]
fn a_fresh_node_gets_every_step_in_order() {
    let mut p = Provisioner::new();
    assert_eq!(
        run(&mut p),
        Step::ALL,
        "the steps did not run in their order"
    );
    assert!(p.result().complete(), "a step went unaccounted for");
    assert!(p.result().changed_anything());
}

/// A RELOAD does almost nothing, which is the case that matters most.
#[test]
fn a_reload_redoes_nothing_and_says_so() {
    let mut p = Provisioner::new();
    for s in Step::ALL {
        p.already(s);
    }
    assert_eq!(
        p.next_step(),
        None,
        "a fully provisioned node had work to do"
    );
    assert!(p.result().complete());
    assert!(
        !p.result().changed_anything(),
        "a reload reported that it changed something"
    );
}

#[test]
fn only_the_missing_steps_are_done() {
    let mut p = Provisioner::new();
    p.already(Step::Delegate);
    p.already(Step::BlockContract);
    assert_eq!(run(&mut p), vec![Step::RegisterContract, Step::Key]);
    assert_eq!(p.result().did(Step::Delegate), Some(Did::AlreadyThere));
    assert!(p.result().complete());
}

/// **THE DEFECT.** A duplicate ack for an EARLIER step must not complete a
/// later one.
///
/// Both contract steps send a `Put` and get a `Put` back. Matching on the KIND
/// alone meant a second acknowledgement of the Block contract — which a node
/// may send, and a hostile one certainly may — completed the Register step,
/// and the run reported a contract installed that had never been sent.
///
/// "One step in flight" does not help: the ack that arrives need not be this
/// step's. The node names what it answers; the fix is to read the name.
#[test]
fn a_duplicate_ack_for_an_earlier_step_cannot_complete_a_later_one() {
    let mut p = Provisioner::new();
    p.already(Step::Delegate);

    // Block goes out and is acknowledged.
    let step = p.next_step().expect("block");
    assert_eq!(step, Step::BlockContract);
    p.sent(step, named(step), 0);
    p.on_ack(&AckKind::Put(named(Step::BlockContract).into()));
    assert_eq!(p.result().did(Step::BlockContract), Some(Did::Installed));

    // Register goes out. The node then answers BLOCK's put a second time.
    let step = p.next_step().expect("register");
    assert_eq!(step, Step::RegisterContract);
    p.sent(step, named(step), 10);
    p.on_ack(&AckKind::Put(named(Step::BlockContract).into()));

    assert_eq!(
        p.result().did(Step::RegisterContract),
        None,
        "a duplicate ack for the BLOCK contract completed the REGISTER step — \
         the run would report a contract installed that was never sent"
    );
    assert!(p.in_flight(), "the register step stopped being in flight");
    assert_eq!(p.unexpected, 1, "the stray ack was not counted");

    // THE CONTROL: its OWN ack does complete it, so the refusal above is
    // about the name and not about acks being ignored.
    p.on_ack(&AckKind::Put(named(Step::RegisterContract).into()));
    assert_eq!(p.result().did(Step::RegisterContract), Some(Did::Installed));
}

/// The same, for the delegate: another delegate's registration is not ours.
#[test]
fn an_ack_naming_another_delegate_does_not_complete_ours() {
    let mut p = Provisioner::new();
    let step = p.next_step().expect("delegate");
    p.sent(step, "our-delegate", 0);
    p.on_ack(&AckKind::Registered("somebody-elses-delegate".into()));
    assert_eq!(p.result().did(Step::Delegate), None);
    assert_eq!(p.unexpected, 1);
    p.on_ack(&AckKind::Registered("our-delegate".into()));
    assert_eq!(p.result().did(Step::Delegate), Some(Did::Installed));
}

#[test]
fn only_one_step_is_ever_in_flight() {
    let mut p = Provisioner::new();
    let first = p.next_step().expect("a first step");
    p.sent(first, named(first), 0);
    assert_eq!(
        p.next_step(),
        None,
        "a second step was handed out while {first:?} was unanswered"
    );
    p.on_ack(&ack_for(first));
    assert!(p.next_step().is_some());
}

#[test]
fn an_ack_with_nothing_in_flight_is_counted() {
    let mut p = Provisioner::new();
    p.on_ack(&AckKind::Ok);
    assert_eq!(p.unexpected, 1);
    assert!(p.result().steps.is_empty());
}

/// A step nobody answers is reported STALLED, and may be sent again.
///
/// Acks take a minute or never arrive at all (F20), so a provisioner with no
/// clock waits for ever on the first one that goes missing — and the page
/// shows nothing while it does.
#[test]
fn a_step_nobody_answers_is_reported_stalled_and_can_be_reissued() {
    let mut p = Provisioner::new();
    p.stall_after_ms = 1_000;
    let step = p.next_step().expect("a step");
    p.sent(step, named(step), 0);

    assert_eq!(p.tick(999), None, "reported stalled before its time");
    assert_eq!(p.tick(1_000), Some(step), "never reported the stall");
    assert_eq!(
        p.tick(5_000),
        None,
        "reported the same stall twice; a page would show it repeatedly"
    );
    assert_eq!(p.stalled(), Some(step));

    // RE-ISSUED, which is safe only because every step is idempotent — the
    // same property that made matching acks by kind dangerous.
    assert_eq!(p.reissue(), Some(step));
    assert!(!p.in_flight());
    assert_eq!(p.next_step(), Some(step), "the step was not re-offered");
}

/// THE CONTROL for the stall clock: a step that IS answered never stalls.
#[test]
fn an_answered_step_never_stalls() {
    let mut p = Provisioner::new();
    p.stall_after_ms = 1_000;
    let step = p.next_step().expect("a step");
    p.sent(step, named(step), 0);
    p.on_ack(&ack_for(step));
    assert_eq!(
        p.tick(1_000_000),
        None,
        "a step that was answered was reported stalled a long time later"
    );
    assert_eq!(p.stalled(), None);
}

/// A refusal stops the run and keeps what the node said, for display.
#[test]
fn a_refusal_stops_the_run_and_keeps_the_nodes_words() {
    let mut p = Provisioner::new();
    let step = p.next_step().expect("a step");
    p.sent(step, named(step), 0);
    p.on_refused("the delegate code was rejected");

    assert_eq!(p.next_step(), None, "it carried on after a refusal");
    assert!(!p.result().complete());
    let (at, said) = p.refused().expect("the refusal was not kept");
    assert_eq!(at, step);
    assert_eq!(said, "the delegate code was rejected");
}

/// Which steps are CONFIRMED and which rest on an ack is STATED.
///
/// They are different promises. `Identity` proves the delegate is registered
/// and the engine has a key — an unregistered delegate cannot answer it. The
/// contract puts rest on the node's own word about its own store, which is
/// why provisioning is loopback-only.
#[test]
fn the_steps_say_which_of_them_are_confirmed_rather_than_acknowledged() {
    assert!(Step::Delegate.confirmed_by_asking());
    assert!(Step::Key.confirmed_by_asking());
    assert!(!Step::BlockContract.confirmed_by_asking());
    assert!(!Step::RegisterContract.confirmed_by_asking());
}
