//! sdk#160: a write that changes NOTHING is published at once, and the
//! engine stays open.
//!
//! As a commit it had no blocks, so no confirmation could arrive to bump the
//! head: `Accepted`, then `Stalled`, and every later write `Busy` for ever.
//! Driven with the engine rebuilt from its context on every step.

use engine::{ClientId, Effect, Event, Op, Params, State, WriteId};

mod common;
use common::{Harness, Mode, Store};

fn write(id: u64, key: &[u8], value: &[u8]) -> Event {
    Event::Write {
        client: ClientId(1),
        write_id: WriteId(id),
        ops: vec![(key.to_vec(), Op::Put(value.to_vec()))],
    }
}

fn states(fx: &[Effect], id: u64) -> Vec<State> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notify {
                write_id, state, ..
            } if write_id.0 == id => Some(*state),
            _ => None,
        })
        .collect()
}

fn to_node(fx: &[Effect]) -> usize {
    fx.iter()
        .filter(|f| {
            matches!(
                f,
                Effect::PutBlock { .. } | Effect::PutPack { .. } | Effect::UpdateHead { .. }
            )
        })
        .count()
}

/// Confirm everything a step put, and the head, until nothing is left.
fn settle(h: &mut Harness, mut fx: Vec<Effect>) -> Vec<Effect> {
    let mut all = fx.clone();
    for _ in 0..50 {
        let mut next = Vec::new();
        for f in &fx {
            match f {
                Effect::PutBlock { id, .. } | Effect::PutPack { id, .. } => {
                    next.extend(h.step(Event::PutConfirmed(*id)))
                }
                Effect::UpdateHead { seq, .. } => next.extend(h.step(Event::HeadConfirmed(*seq))),
                _ => {}
            }
        }
        if next.is_empty() {
            return all;
        }
        all.extend(next.iter().cloned());
        fx = next;
    }
    panic!("never settled");
}

#[test]
fn an_identical_write_is_published_at_once_and_the_next_write_is_accepted() {
    let mut h = Harness::new(Mode::Rehydrate, Params::default(), Store::fresh());
    let first = h.step(write(1, b"schema/tasks", b"v1"));
    let all = settle(&mut h, first);
    assert!(states(&all, 1).contains(&State::Published));
    let root = h.published_root();

    let again = h.step(write(2, b"schema/tasks", b"v1"));
    assert_eq!(
        states(&again, 2),
        vec![State::Accepted, State::Published, State::ParityComplete],
        "a write that changes nothing was not told it is published"
    );
    assert_eq!(
        to_node(&again),
        0,
        "a write that changes nothing went to the node"
    );
    assert_eq!(h.published_root(), root);

    let next = h.step(write(3, b"d/other", b"x"));
    assert_eq!(
        states(&next, 3),
        vec![State::Accepted],
        "the write after the no-op was refused: the engine is still closed"
    );
    let all = settle(&mut h, next);
    assert!(states(&all, 3).contains(&State::Published));
}

/// THE OPPOSITE: a write that DOES change the tree is published only by its
/// head, never by the shortcut -- with its puts unconfirmed it is `Accepted`
/// and nothing more, however many ticks pass under `max_accept_age`.
#[test]
fn a_real_change_is_not_published_without_its_head() {
    let mut h = Harness::new(Mode::Rehydrate, Params::default(), Store::fresh());
    let first = h.step(write(1, b"schema/tasks", b"v1"));
    let _ = settle(&mut h, first);

    let changed = h.step(write(2, b"schema/tasks", b"v2"));
    assert_eq!(states(&changed, 2), vec![State::Accepted]);
    assert!(to_node(&changed) > 0, "a real change put nothing");
    let mut told = Vec::new();
    for t in 1..10 {
        told.extend(states(&h.step(Event::Tick(1_790_000_000 + t)), 2));
    }
    assert!(
        !told.contains(&State::Published),
        "a real change was told Published with no head confirmed: {told:?}"
    );
    let all = settle(&mut h, changed);
    assert!(
        states(&all, 2).contains(&State::Published),
        "and once confirmed it does publish: {:?}",
        states(&all, 2)
    );
}
