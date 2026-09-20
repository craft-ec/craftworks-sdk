//! Provisioning, against a fake wire.
//!
//! No socket: the whole point of the state machine is that what a browser runs
//! is what a test runs.

use wire::provision::{Did, Provisioner, Step};
use wire::AckKind;

/// A clean node: every step runs, in order, once.
#[test]
fn a_fresh_node_gets_every_step_in_order() {
    let mut p = Provisioner::new();
    let mut order = Vec::new();
    while let Some(step) = p.next_step() {
        order.push(step);
        p.on_ack(step.expects());
    }
    assert_eq!(order, Step::ALL, "the steps did not run in their order");
    assert!(p.result().complete(), "a step went unaccounted for");
    assert!(p.result().changed_anything());
    for s in Step::ALL {
        assert_eq!(p.result().did(s), Some(Did::Installed));
    }
}

/// A RELOAD does almost nothing, which is the case that matters most.
///
/// A page reloads constantly. Re-registering a 721 KB delegate every time
/// would be the whole cost of opening a tab, and re-creating what exists is
/// the mistake F38 records.
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
    assert!(
        p.result().complete(),
        "the run did not account for every step"
    );
    assert!(
        !p.result().changed_anything(),
        "a reload reported that it changed something"
    );
    for s in Step::ALL {
        assert_eq!(
            p.result().did(s),
            Some(Did::AlreadyThere),
            "{s:?} was not reported as already present"
        );
    }
}

/// A PARTLY provisioned node does only what is missing.
#[test]
fn only_the_missing_steps_are_done() {
    let mut p = Provisioner::new();
    p.already(Step::Delegate);
    p.already(Step::BlockContract);

    let mut did = Vec::new();
    while let Some(step) = p.next_step() {
        did.push(step);
        p.on_ack(step.expects());
    }
    assert_eq!(
        did,
        vec![Step::RegisterContract, Step::Key],
        "it redid something that was already there"
    );
    assert_eq!(p.result().did(Step::Delegate), Some(Did::AlreadyThere));
    assert_eq!(p.result().did(Step::Key), Some(Did::Installed));
    assert!(p.result().complete());
}

/// ONE step in flight at a time.
///
/// The node's acks carry no correlation id, so two steps outstanding cannot be
/// told apart — and matching the wrong ack to the wrong step would report a
/// delegate registered because a contract went in.
#[test]
fn only_one_step_is_ever_in_flight() {
    let mut p = Provisioner::new();
    let first = p.next_step().expect("a first step");
    assert!(p.in_flight());
    assert_eq!(
        p.next_step(),
        None,
        "a second step was handed out while {first:?} was still unanswered"
    );
    p.on_ack(first.expects());
    assert!(!p.in_flight());
    assert!(p.next_step().is_some(), "nothing followed the first step");
}

/// An ack of the WRONG KIND does not advance anything.
///
/// Believing it would record an install that did not happen, and the node
/// chooses what it sends.
#[test]
fn an_ack_of_the_wrong_kind_advances_nothing() {
    let mut p = Provisioner::new();
    let step = p.next_step().expect("a step");
    assert_eq!(step, Step::Delegate);

    p.on_ack(AckKind::Put); // a contract ack, for a delegate step
    assert!(
        p.in_flight(),
        "a wrong-kind ack completed the step in flight"
    );
    assert_eq!(p.result().did(step), None, "it recorded an install anyway");
    assert_eq!(p.unexpected, 1, "the mismatch was not counted");

    // The CONTROL: the right kind does advance it, so the refusal above is
    // about the KIND and not about acks being ignored.
    p.on_ack(step.expects());
    assert!(!p.in_flight());
    assert_eq!(p.result().did(step), Some(Did::Installed));
}

/// An ack nobody was waiting for is counted, not acted on.
#[test]
fn an_ack_with_nothing_in_flight_is_counted() {
    let mut p = Provisioner::new();
    p.on_ack(AckKind::Ok);
    assert_eq!(p.unexpected, 1);
    assert!(p.result().steps.is_empty());
}

/// A refusal STOPS provisioning rather than carrying on.
///
/// The steps are ordered because each depends on the last: installing a
/// contract for a delegate that is not there leaves a node that half works,
/// and the failure surfaces at the first write, a long way from here.
#[test]
fn a_refusal_stops_the_run_rather_than_continuing() {
    let mut p = Provisioner::new();
    let step = p.next_step().expect("a step");
    p.on_refused();
    assert_eq!(
        p.next_step(),
        None,
        "it carried on to a later step after {step:?} was refused"
    );
    assert!(
        !p.result().complete(),
        "a refused run reported itself complete"
    );
}
