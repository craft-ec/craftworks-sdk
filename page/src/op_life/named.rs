//! OP-LIFE.md draft 3.1: a NAMED answer (the signer's request id, SG02) is CHECKED against its key's ONE send in
//! flight (L0) -- never routed among sends. Under that id it answers the key; under any other it is dropped and counted.
//! A different payload for a key with a send in flight WAITS for its answer (F1) and goes after it.
use crate::op_life::{Did, OpEvent};
use crate::*;
use instrument::{Dir, Event as IE, Outcome, Record};

fn page() -> Page {
    let mut p = super::scenarios::page();
    p.record_into(1024);
    p
}

fn key() -> Waiting {
    Waiting::Sign(Label::Site("t".into()))
}

fn sign(id: u32) -> Op {
    Op::Sign {
        id,
        prev_seq: 0,
        prev_root: [0; 32],
        seq: 1,
        root: [0; 32],
        ledger: Vec::new(),
        label: Label::Site("t".into()),
    }
}

fn named(p: &mut Page, id: u32) -> Did {
    p.on(&key(), OpEvent::AnswerNamed { request: id })
}

/// The ends recorded so far: (Responses, Withdrawn exits).
fn ends(p: &Page) -> (usize, usize) {
    let r = p.recording().expect("attached");
    let ev = r.events();
    (
        ev.iter()
            .filter(|e| {
                matches!(
                    e,
                    IE::Edge {
                        dir: Dir::Response,
                        ..
                    }
                )
            })
            .count(),
        ev.iter()
            .filter(|e| {
                matches!(
                    e,
                    IE::Exit {
                        outcome: Outcome::Withdrawn,
                        ..
                    }
                )
            })
            .count(),
    )
}

/// **The send in flight:** its own id answers it.
#[test]
fn a_named_answer_to_the_send_in_flight_answers_it() {
    let mut p = page();
    p.send(key(), sign(5));
    assert!(matches!(named(&mut p, 5), Did::Answered { .. }), "the send's own named answer did not answer it");
    assert!(!p.ops.contains(&key()));
    assert_eq!(ends(&p), (1, 0));
}

/// **A different payload WAITS (F1), and goes after the answer:** a second sign while one is in flight is not sent;
/// the first's answer releases it.
#[test]
fn a_second_sign_waits_for_the_first_answer_and_then_goes() {
    let mut p = page();
    p.send(key(), sign(5));
    let _ = p.take_ops();
    p.send(key(), sign(6));
    assert!(p.take_ops().is_empty(), "a second payload went out beside the send in flight (two sends of one key, L0)");
    assert_eq!(p.ops.named(5), Some(&key()), "the send in flight is not the first");
    // The first send's payload is superseded: its answer serves nobody (Withdrawn), and releases the waiting one.
    assert_eq!(named(&mut p, 5), Did::Nothing, "the superseded send's answer was taken for the new payload");
    assert_eq!(p.take_ops(), vec![sign(6)], "the waiting payload did not go after the answer");
    assert!(matches!(named(&mut p, 6), Did::Answered { .. }), "the second send's answer did not answer it");
    assert_eq!(ends(&p), (1, 1));
}

/// **An id no send in flight carries:** dropped and counted, no end, nothing changed (the check).
#[test]
fn a_named_answer_under_another_id_is_dropped_and_counted() {
    use instrument::{vocab::DropReason, Entry, Key};
    let mut p = page();
    p.send(key(), sign(5));
    assert_eq!(named(&mut p, 99), Did::Nothing);
    assert_eq!(p.ops.named(5), Some(&key()), "an unknown id changed the send in flight");
    assert_eq!(ends(&p), (0, 0), "an unknown id recorded an end");
    assert_eq!(p.ops.late(crate::op_life::Resend::OnlyOne, Lane::Interactive, false), 1, "the unknown id was not counted");
    let r = p.recording().expect("attached");
    let counted = r.events().iter().filter(|e| matches!(e, IE::Counter { entry: Entry { key: Key::DroppedMsgs, value }, .. } if *value == DropReason::AnsweredAfterReask.code())).count();
    assert_eq!(counted, 1, "the unknown id was not recorded");
}

/// **Through the page's real `Answer::Signer` door** (`Ops::named`: PUBLISH-LIFE stores no sign id): the key is the one
/// whose send in flight carries the id; an OLD id after a re-sign at the bound is dropped and counted, and the send in
/// flight still answers under its own.
#[test]
fn the_signers_answer_is_checked_against_the_one_send_in_flight() {
    let mut p = page();
    p.send(key(), sign(5));
    let now = p.now;
    p.answer(Answer::Signer { id: 4, answer: signer_proto::Answer::Signed(vec![1]) }, Ms(now));
    assert!(p.ops.contains(&key()), "an old id's answer took the key");
    assert_eq!(p.ops.late(crate::op_life::Resend::OnlyOne, Lane::Interactive, false), 1, "the old id was not counted");
    p.answer(Answer::Signer { id: 5, answer: signer_proto::Answer::Signed(vec![1]) }, Ms(now));
    assert!(!p.ops.contains(&key()), "the send's own id did not answer it");
    assert_eq!(p.ops.named(5), None, "an answered send still names its key");
}
