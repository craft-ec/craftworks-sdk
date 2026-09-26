//! OP-LIFE.md's NAMED-ANSWER ROW, one test per cell: an answer that names its send (the signer's request id, SG02)
//! is that send's whatever its age -- the need's, the held send's, a stale send's -- or, naming nothing the page holds,
//! dropped and counted. And an UNNAMED answer never matches a send that carries an id.
use crate::op_life::{Did, OpEvent};
use crate::*;
use instrument::{Dir, Event as IE, Outcome, Record};

fn page() -> Page {
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.answered(&Waiting::RecoverHead);
    let _ = p.take_ops();
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

/// **The need's send:** its answer.
#[test]
fn a_named_answer_to_the_needs_send_answers_the_need() {
    let mut p = page();
    p.send(key(), sign(5));
    assert!(
        matches!(named(&mut p, 5), Did::Answered { .. }),
        "the need's own named answer did not answer it"
    );
    assert!(!p.ops.contains(&key()));
    assert_eq!(ends(&p), (1, 0));
}

/// **The held send:** released, the slot freed; the need untouched (its successor goes out, unanswered).
#[test]
fn a_named_answer_to_the_held_send_releases_it_and_leaves_the_need() {
    let mut p = page();
    p.test_background.insert(key());
    p.send(key(), sign(5));
    p.send(key(), sign(6));
    assert!(
        p.ops
            .held()
            .is_some_and(|(w, h)| *w == key() && h.send.op == sign(5)),
        "THE SETUP: the superseded background send is not held"
    );
    assert_eq!(
        named(&mut p, 5),
        Did::Nothing,
        "the held send's answer was taken for the need"
    );
    assert!(p.ops.held().is_none(), "the held send was not released");
    assert!(
        p.ops
            .need(&key())
            .and_then(|n| n.send())
            .is_some_and(|s| s.op == sign(6)),
        "the need was touched, or its successor did not take the freed slot"
    );
    assert_eq!(ends(&p), (0, 1));
}

/// **A stale send:** it leaves, Withdrawn; the need untouched.
#[test]
fn a_named_answer_to_a_stale_send_releases_it_and_leaves_the_need() {
    let mut p = page();
    p.send(key(), sign(5));
    p.send(key(), sign(6));
    assert_eq!(
        named(&mut p, 5),
        Did::Nothing,
        "the stale send's answer was taken for the need"
    );
    assert!(
        p.ops
            .need(&key())
            .and_then(|n| n.send())
            .is_some_and(|s| s.op == sign(6)),
        "the need was touched"
    );
    assert_eq!(ends(&p), (0, 1));
    assert!(
        matches!(named(&mut p, 6), Did::Answered { .. }),
        "the need's own answer, after the stale one's, did not answer it"
    );
    assert_eq!(ends(&p), (1, 1));
}

/// **An id nothing holds:** dropped and counted (`Unexpected`), no end, nothing changed.
#[test]
fn a_named_answer_nothing_holds_is_dropped_and_counted() {
    use instrument::{vocab::DropReason, Entry, Key};
    let mut p = page();
    p.send(key(), sign(5));
    assert_eq!(named(&mut p, 99), Did::Nothing);
    assert!(p.ops.on_wire(&key()), "an unknown id changed the need");
    assert_eq!(ends(&p), (0, 0), "an unknown id recorded an end");
    let r = p.recording().expect("attached");
    let counted = r.events().iter().filter(|e| matches!(e, IE::Counter { entry: Entry { key: Key::DroppedMsgs, value }, .. } if *value == DropReason::Unexpected.code())).count();
    assert_eq!(counted, 1, "the unknown id was not counted");
}

/// **An UNNAMED answer never matches a send that carries an id.**
#[test]
fn an_unnamed_answer_never_matches_a_send_carrying_an_id() {
    let mut p = page();
    p.send(key(), sign(5));
    assert_eq!(
        p.answered(&key()),
        None,
        "an unnamed answer took a Sign's send"
    );
    assert!(p.ops.on_wire(&key()));
    assert_eq!(ends(&p), (0, 0));
}
