//! What the executor does with each kind of signer answer, one cell at a time
//! (engineer2's review of sdk#215). The model (`tests/model.rs`) covers the
//! rest; these are the cells it cannot reach reliably: a MISROUTED answer, a
//! fork, a register the signer's node does not hold, and the pace of a
//! wrapper-path `Held` that keeps answering absent.

use engine::{ClientId, Op as WriteOp, Params, WriteId};
use page::{Answer, Op, Page, PutPath, BACKOFF_MS, HELD_ABSENTS, SILENT_MS};
use signer_proto::{Answer as A, Head, Why};

/// A page with one write, driven until it asks the signer; returns it and the
/// time. Every PUT is answered ok; the first head read finds no head.
fn at_sign(path: PutPath) -> (Page, u64) {
    let mut p = Page::new(Params::default(), path);
    p.write(ClientId(1), WriteId(1), vec![(b"k".to_vec(), WriteOp::Put(b"v".to_vec()))]);
    let now = 10;
    for _ in 0..20 {
        for op in p.take_ops() {
            match op {
                Op::ReadHead => p.answer(Answer::Head(None), now),
                Op::Put { id, .. } => p.answer(Answer::PutOk(id), now),
                Op::AskHeld { id } => p.answer(Answer::Held { id, present: true }, now),
                Op::Sign { .. } => return (p, now),
                other => panic!("unexpected before the sign: {other:?}"),
            }
        }
    }
    panic!("the page never asked the signer");
}

fn signs(ops: &[Op]) -> usize {
    ops.iter().filter(|o| matches!(o, Op::Sign { .. })).count()
}

/// A `Held`, `Putting` or other-verb refusal arriving while a sign is out is
/// not the sign's answer: the sign is still re-asked at its deadline.
#[test]
fn a_misrouted_answer_does_not_stop_the_sign_being_asked_again() {
    for misrouted in [
        A::Held { present: vec![true] },
        A::Putting { contracts: vec![[1; 32]] },
        A::Provisioned,
        A::Refused(Why::BlockCount { max: 128, got: 200 }),
    ] {
        let (mut p, now) = at_sign(PutPath::Page);
        p.answer(Answer::Signer(misrouted.clone()), now + 1);
        p.tick(now + SILENT_MS);
        assert_eq!(signs(&p.take_ops()), 1, "after a misrouted {misrouted:?} the sign was never asked again");
        assert!(p.unusable().is_empty(), "a misrouted answer was taken as the sign's: {:?}", p.unusable());
    }
}

/// THE CONTROL: a real sign answer DOES end the wait — a `NotNext` is acted
/// on, so no re-ask of that request follows at the deadline.
#[test]
fn control_a_sign_shaped_answer_ends_the_wait() {
    let (mut p, now) = at_sign(PutPath::Page);
    p.answer(Answer::Signer(A::Refused(Why::NotSuccessor)), now + 1);
    p.tick(now + SILENT_MS);
    assert_eq!(signs(&p.take_ops()), 0, "a permanent refusal was re-asked");
    assert_eq!(p.unusable().len(), 1);
}

/// A fork is NEWS: surfaced as `forked()`, never retried.
#[test]
fn a_fork_is_surfaced_loudly_and_never_retried() {
    let (mut p, now) = at_sign(PutPath::Page);
    let fork = Why::Forked { mine: Head { seq: 1, root: [1; 32] }, read: Head { seq: 1, root: [2; 32] } };
    p.answer(Answer::Signer(A::Refused(fork)), now + 1);
    assert!(p.forked().is_some_and(|m| m.starts_with("FORKED")), "a fork was not surfaced");
    p.tick(now + 10 * SILENT_MS);
    assert_eq!(signs(&p.take_ops()), 0, "a fork was retried");
}

/// `HeadUnknown`: the node does not hold the register, so the page READS it
/// (which makes the node hold it) and asks again after a backoff.
#[test]
fn head_unknown_reads_the_register_then_asks_again_after_a_backoff() {
    let (mut p, now) = at_sign(PutPath::Page);
    p.answer(Answer::Signer(A::Refused(Why::HeadUnknown)), now);
    let ops = p.take_ops();
    assert!(ops.contains(&Op::ReadHead), "the register was not read to make it held: {ops:?}");
    p.tick(now + 1);
    assert_eq!(signs(&p.take_ops()), 0, "asked again with no backoff");
    p.tick(now + 2 * BACKOFF_MS);
    assert_eq!(signs(&p.take_ops()), 1, "never asked again after its backoff");
    assert!(p.unusable().is_empty());
}

/// `RecordNotSaved` and `RootNotHeld` are retried the same way.
#[test]
fn record_not_saved_and_root_not_held_are_asked_again_after_a_backoff() {
    for why in [Why::RecordNotSaved, Why::RootNotHeld] {
        let (mut p, now) = at_sign(PutPath::Page);
        p.answer(Answer::Signer(A::Refused(why.clone())), now);
        p.tick(now + 2 * BACKOFF_MS);
        assert_eq!(signs(&p.take_ops()), 1, "{why:?} was not asked again");
    }
}

/// The wrapper path: `Held` absent is asked AGAIN on a backoff, and the block
/// is PUT again only after `HELD_ABSENTS` in a row — never one PUT per absent.
#[test]
fn a_held_absent_is_asked_again_and_re_put_only_after_several() {
    let mut p = Page::new(Params::default(), PutPath::Wrapper);
    p.write(ClientId(1), WriteId(1), vec![(b"k".to_vec(), WriteOp::Put(b"v".to_vec()))]);
    let mut now = 10;
    let (mut puts, mut asks) = (0usize, 0usize);
    let mut blocks = std::collections::BTreeSet::new();
    for _ in 0..200 {
        for op in p.take_ops() {
            match op {
                Op::ReadHead => p.answer(Answer::Head(None), now),
                Op::Put { id, .. } => {
                    puts += 1;
                    blocks.insert(id);
                    p.answer(Answer::PutOk(id), now);
                }
                Op::AskHeld { id } => {
                    asks += 1;
                    p.answer(Answer::Held { id, present: false }, now);
                }
                _ => {}
            }
        }
        now += 50;
        p.tick(now);
        if asks >= 3 * HELD_ABSENTS as usize {
            break;
        }
    }
    assert!(!blocks.is_empty(), "nothing was put");
    assert!(asks >= 3 * HELD_ABSENTS as usize, "Held was not asked again after an absent ({asks} asks)");
    // Each block once, then once more per HELD_ABSENTS absents — never once
    // per absent.
    let allowed = blocks.len() + asks / HELD_ABSENTS as usize;
    assert!(puts <= allowed, "{puts} PUTs for {} blocks and {asks} Held asks (at most {allowed})", blocks.len());
}
