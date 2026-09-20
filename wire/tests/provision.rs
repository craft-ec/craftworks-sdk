//! Provisioning, against a fake wire.
//!
//! No socket: the whole point of the state machine is that what a browser runs
//! is what a test runs.

use wire::provision::{Did, Provisioner, Step};
use wire::AckKind;

const DELEGATE: &str = "delegate-key";

/// Drive a run to completion, with the delegate answering `Identity` from a
/// store this harness keeps. `writable` is what the store says at the start.
fn run(p: &mut Provisioner, mut writable: bool) -> Vec<Step> {
    let mut order = Vec::new();
    let mut now = 0u64;
    while let Some(step) = p.next_step() {
        order.push(step);
        p.sent(step, DELEGATE, now);
        now += 10;
        match step {
            Step::Delegate => p.on_ack(&AckKind::Registered(DELEGATE.into())),
            Step::Ask => p.on_identity(writable),
            // A real delegate that accepts an Install can sign afterwards.
            Step::Install => writable = true,
        }
        assert!(order.len() < 12, "provisioning did not converge: {order:?}");
    }
    order
}

#[test]
fn a_fresh_node_is_registered_asked_installed_and_asked_again() {
    let mut p = Provisioner::new();
    let order = run(&mut p, false);
    assert_eq!(
        order,
        vec![Step::Delegate, Step::Ask, Step::Install, Step::Ask]
    );
    assert!(p.provisioned());
    assert!(p.result().complete());
    assert_eq!(p.result().did(Step::Install), Some(Did::Installed));
    assert!(p.result().changed_anything());
    assert_eq!(p.unexpected, 0);
}

/// **THE DEFECT THIS STEP SET EXISTS FOR.**
///
/// `Install` writes `SIGNING_KEY` unconditionally, and the Register instance
/// is derived from a keyset — so installing over a delegate that is already
/// provisioned mints a new key, moves the head's contract id, and orphans
/// every head written under the old one. The previous plan ran the install on
/// every page load, so it would have lost the data on every reload while
/// reporting success.
///
/// A reload must therefore ASK and then stop.
#[test]
fn a_provisioned_delegate_is_asked_and_never_installed_over() {
    let mut p = Provisioner::new();
    let order = run(&mut p, true);

    assert!(
        !order.contains(&Step::Install),
        "installed over a provisioned delegate: {order:?}"
    );
    assert_eq!(order, vec![Step::Delegate, Step::Ask]);
    assert!(p.provisioned());
    assert!(p.result().complete());
    assert_eq!(p.result().did(Step::Install), Some(Did::AlreadyThere));
    // The delegate itself WAS registered by this run, so something did
    // change — just not the thing that would have cost the key.
    assert!(p.result().changed_anything());
}

/// THE NEGATIVE CONTROL for the test above: with the store answering `false`,
/// the very same code path DOES install.
///
/// Without this, `a_provisioned_delegate_is_asked_and_never_installed_over`
/// would keep passing if `Install` were removed from the plan entirely.
#[test]
fn control_an_unprovisioned_delegate_is_installed() {
    let mut p = Provisioner::new();
    let order = run(&mut p, false);
    assert!(
        order.contains(&Step::Install),
        "the control did not reach Install, so the test above proves nothing"
    );
}

/// A reload that kept the delegate skips the 721 KB and asks.
#[test]
fn a_reload_skips_the_delegate_and_asks() {
    let mut p = Provisioner::new();
    p.already_registered();
    let order = run(&mut p, true);
    assert_eq!(order, vec![Step::Ask]);
    assert_eq!(p.result().did(Step::Delegate), Some(Did::AlreadyThere));
    assert!(p.provisioned());
    assert!(
        !p.result().changed_anything(),
        "a reload reported a change it did not make"
    );
}

/// Answering `Identity` at all proves the delegate is registered — so a page
/// may ask first and learn it never needed to register.
#[test]
fn an_answer_to_identity_proves_the_delegate_is_registered() {
    let mut p = Provisioner::new();
    p.on_identity(true);
    assert_eq!(p.result().did(Step::Delegate), Some(Did::AlreadyThere));
    assert!(p.provisioned());
    assert_eq!(p.next_step(), None);
}

/// An ack naming another delegate is not ours.
#[test]
fn an_ack_naming_another_delegate_does_not_complete_ours() {
    let mut p = Provisioner::new();
    assert_eq!(p.next_step(), Some(Step::Delegate));
    p.sent(Step::Delegate, DELEGATE, 0);

    p.on_ack(&AckKind::Registered("someone-elses-delegate".into()));
    assert_eq!(p.unexpected, 1);
    assert_eq!(p.result().did(Step::Delegate), None);

    // THE CONTROL: our own name does complete it, so the refusal above is
    // about the name and not about acks being ignored.
    p.on_ack(&AckKind::Registered(DELEGATE.into()));
    assert_eq!(p.result().did(Step::Delegate), Some(Did::Installed));
}

/// An ack of the wrong KIND for the step in flight is not ours either.
#[test]
fn an_ack_of_another_kind_does_not_complete_the_step_in_flight() {
    let mut p = Provisioner::new();
    p.next_step();
    p.sent(Step::Delegate, DELEGATE, 0);
    p.on_ack(&AckKind::Put(DELEGATE.into()));
    assert_eq!(p.unexpected, 1);
    assert_eq!(p.result().did(Step::Delegate), None);
}

#[test]
fn only_one_step_is_ever_in_flight() {
    let mut p = Provisioner::new();
    let first = p.next_step().unwrap();
    p.sent(first, DELEGATE, 0);
    assert_eq!(p.next_step(), None, "a second step went out unanswered");
    assert!(p.in_flight());
}

#[test]
fn an_ack_with_nothing_in_flight_is_counted() {
    let mut p = Provisioner::new();
    p.on_ack(&AckKind::Registered(DELEGATE.into()));
    assert_eq!(p.unexpected, 1);
}

/// A step nobody answers is reported STALLED, and may be sent again.
///
/// Acks take a minute or never arrive at all (F20), so a provisioner with no
/// clock waits for ever on the first one that goes missing — and the page
/// shows nothing while it does.
#[test]
fn a_step_nobody_answers_is_reported_stalled_and_can_be_reissued() {
    let mut p = Provisioner::new();
    let step = p.next_step().unwrap();
    p.sent(step, DELEGATE, 0);

    assert_eq!(p.tick(p.stall_after_ms - 1), None);
    assert_eq!(p.tick(p.stall_after_ms + 1), Some(step));
    // Reported ONCE, not on every tick.
    assert_eq!(p.tick(p.stall_after_ms + 2), None);
    assert_eq!(p.stalled(), Some(step));

    assert_eq!(p.reissue(), Some(step));
    assert!(!p.in_flight());
    assert_eq!(p.next_step(), Some(step));
}

/// THE CONTROL for the stall clock: a step that IS answered never stalls.
#[test]
fn an_answered_step_never_stalls() {
    let mut p = Provisioner::new();
    let step = p.next_step().unwrap();
    p.sent(step, DELEGATE, 0);
    p.on_ack(&AckKind::Registered(DELEGATE.into()));
    assert_eq!(p.tick(p.stall_after_ms * 10), None);
    assert_eq!(p.stalled(), None);
}

/// `Install` is answered by NOTHING, so it must not sit in flight waiting out
/// the stall timeout on every provisioning run.
#[test]
fn install_does_not_wait_for_an_ack_it_will_never_get() {
    let mut p = Provisioner::new();
    p.already_registered();
    p.next_step();
    p.sent(Step::Ask, DELEGATE, 0);
    p.on_identity(false);

    assert_eq!(p.next_step(), Some(Step::Install));
    p.sent(Step::Install, DELEGATE, 10);
    assert!(
        !p.in_flight(),
        "Install is waiting for an ack that never comes"
    );
    assert_eq!(p.tick(p.stall_after_ms * 10), None);
    // It goes straight back to asking, which is what confirms it.
    assert_eq!(p.next_step(), Some(Step::Ask));
}

/// A delegate that accepts installs and stays unwritable must STOP.
///
/// `Install` carries the contract code, so an unbounded Ask→Install loop
/// spends the uplink for ever and silently. The run reports itself exhausted
/// instead — which is a different fact from stalled (nothing answered) and
/// from refused (the node said no): here everything was accepted and the
/// delegate still cannot sign.
#[test]
fn a_delegate_that_never_becomes_writable_stops_instead_of_looping() {
    let mut p = Provisioner::new();
    p.already_registered();
    let mut installs = 0;
    let mut steps = 0;
    while let Some(step) = p.next_step() {
        p.sent(step, DELEGATE, 0);
        if step == Step::Ask {
            p.on_identity(false);
        } else {
            installs += 1;
        }
        steps += 1;
        assert!(steps < 20, "provisioning looped");
    }
    assert_eq!(installs, p.max_installs);
    assert!(p.exhausted());
    assert!(!p.provisioned());
    assert!(!p.result().complete());
}

/// A refusal stops the run and keeps what the node said, for display.
#[test]
fn a_refusal_stops_the_run_and_keeps_the_nodes_words() {
    let mut p = Provisioner::new();
    let step = p.next_step().unwrap();
    p.sent(step, DELEGATE, 0);
    p.on_refused("delegate code rejected");

    assert_eq!(p.refused(), Some((step, "delegate code rejected")));
    assert_eq!(p.next_step(), None, "the run carried on past a refusal");
    assert!(!p.result().complete());
    assert!(!p.provisioned());
}

/// Which steps are CONFIRMED and which rest on an ack is STATED.
///
/// Both steps that change anything are confirmed by asking: `Identity` cannot
/// be answered by an unregistered delegate, and `head_writable` is the
/// delegate's report about its own store rather than an acknowledgement that
/// a message arrived.
#[test]
fn the_steps_say_which_of_them_are_confirmed_rather_than_acknowledged() {
    assert!(Step::Delegate.confirmed_by_asking());
    assert!(Step::Install.confirmed_by_asking());
}
