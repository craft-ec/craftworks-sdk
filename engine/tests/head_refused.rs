//! A FINAL SIGN REFUSAL ENDS THE COMMIT (PUBLISH-LIFE ⁵, E8; COMMIT-LIFE's Heading x head refused, Committing x head
//! refused). The signer's final refusal is a real answer (rule 8): the page steps `Event::HeadRefused { seq }` and the
//! engine ends that commit at once -- every write of its cut `Failed` -- never leaving it to hang until its own clock
//! calls it `Stalled`. A refusal naming any other seq is stale and changes nothing.
use engine::{ClientId, Effect, Event, Op, Params, State, WriteId};

mod common;
use common::{Harness, Store};

const T0: u64 = 1_790_000_000;

fn write(id: u64, ops: Vec<(Vec<u8>, Op)>) -> Event {
    Event::forced_write(ClientId(1), WriteId(id), ops)
}

fn told(fx: &[Effect], id: u64) -> Vec<State> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notify { write_id, state, .. } if write_id.0 == id => Some(*state),
            _ => None,
        })
        .collect()
}

fn small(tag: u8) -> Vec<(Vec<u8>, Op)> {
    (0..8u8).map(|i| (format!("{tag}/{i}").into_bytes(), Op::Put(vec![i; 700]))).collect()
}

/// A commit for write `id` with every block confirmed and its head SENT (the engine is Heading): the head's seq.
fn heading(h: &mut Harness, id: u64) -> u64 {
    let mut fx = h.step(write(id, small(id as u8)));
    for _ in 0..50 {
        let mut next = Vec::new();
        for f in &fx {
            match f {
                Effect::PutBlock { id, .. } | Effect::PutPack { id, .. } => {
                    h.store.remember(id);
                    next.extend(h.step(Event::PutConfirmed(*id)));
                }
                Effect::UpdateHead { seq, .. } => return *seq,
                other => common::no_answer_owed(other),
            }
        }
        fx = next;
    }
    panic!("THE SETUP: the commit never sent its head");
}

fn harness() -> Harness {
    let mut h = Harness::new(Params::default(), Store::fresh());
    let _ = h.step(Event::Tick(T0));
    h
}

/// **Heading x head refused (final), this seq:** every write of the cut is `Failed` IN THAT STEP -- no tick run, so
/// nothing reaches a stall bound -- and the engine is released: the next write commits. Mutant "the refusal ends
/// nothing" -> red (the write would go `Stalled` on the clock instead).
#[test]
fn a_final_head_refusal_ends_the_commit_failed_at_once() {
    let mut h = harness();
    let seq = heading(&mut h, 1);
    let fx = h.step(Event::HeadRefused { seq });
    assert_eq!(told(&fx, 1), vec![State::Failed], "the refused commit's write was not Failed at once: {:?}", told(&fx, 1));
    let next = h.step(write(2, small(2)));
    assert!(next.iter().any(|f| matches!(f, Effect::PutBlock { .. } | Effect::PutPack { .. })), "the engine was not released: the next write did not commit");
}

/// **A stale seq changes nothing:** a refusal naming a seq that is not the commit in flight's (its head already dead)
/// emits nothing, and the commit in flight still publishes on its own confirmation.
#[test]
fn a_head_refusal_for_another_seq_is_stale_and_changes_nothing() {
    let mut h = harness();
    let seq = heading(&mut h, 1);
    let fx = h.step(Event::HeadRefused { seq: seq + 1 });
    assert!(fx.is_empty(), "a stale refusal did something: {fx:?}");
    let done = h.step(Event::HeadConfirmed(seq));
    assert!(told(&done, 1).contains(&State::Published), "the commit in flight did not publish after a stale refusal: {:?}", told(&done, 1));
}
