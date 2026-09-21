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
    let mut effects = changed;
    for t in 1..10 {
        // Ticks may settle the commit from FACT (sdk#150): its blocks are
        // held, so they are confirmed and the head is sent -- in a tick's
        // effects, which are kept to be answered below.
        let out = h.step(Event::Tick(1_790_000_000 + t));
        told.extend(states(&out, 2));
        effects.extend(out);
    }
    assert!(
        !told.contains(&State::Published),
        "a real change was told Published with no head confirmed: {told:?}"
    );
    let all = settle(&mut h, effects);
    assert!(
        states(&all, 2).contains(&State::Published),
        "and once confirmed it does publish: {:?}",
        states(&all, 2)
    );
}

/// ParityComplete only when TRUE. A no-op made while the published tree's
/// parity is still owed -- a re-send after a lost `Published`, the shortcut's
/// main customer -- waits on those groups and hears it with the write that
/// coded them. Executed by the architect on #164: told ParityComplete at once
/// while three parity puts were still to come.
#[test]
fn a_no_op_while_parity_is_owed_is_parity_complete_only_when_it_is() {
    let mut h = Harness::new(Mode::Rehydrate, Params::default(), Store::fresh());
    let big = vec![7u8; 30 * 1024];
    // No tick yet: published, and its parity still owed.
    let first = h.step(write(1, b"k/big", &big));
    let all = settle(&mut h, first);
    assert_eq!(states(&all, 1), vec![State::Accepted, State::Published]);

    let again = h.step(write(2, b"k/big", &big));
    assert_eq!(
        states(&again, 2),
        vec![State::Accepted, State::Published],
        "a no-op was told ParityComplete while the tree's parity is still owed"
    );
    assert_eq!(to_node(&again), 0);

    // Time passes and the node answers the parity puts.
    let mut later = Vec::new();
    for t in 1..=40 {
        let out = h.step(Event::Tick(1_790_000_000 + t));
        for f in &out {
            if let Effect::PutParity { id, .. } = f {
                later.extend(h.step(Event::PutConfirmed(*id)));
            }
        }
        later.extend(out);
    }
    assert!(
        states(&later, 1).contains(&State::ParityComplete),
        "{:?}",
        states(&later, 1)
    );
    assert_eq!(
        states(&later, 2),
        vec![State::ParityComplete],
        "the no-op never heard ParityComplete once the parity landed"
    );
}

/// A CONTROL, named because it is a door: a write BACK to an earlier value
/// (A, B, A) is not a no-op against the PUBLISHED tree, so it commits -- its
/// blocks are emitted again, because the apply does not de-duplicate against
/// what the node holds. Were that de-duplication ever added, this write would
/// emit no blocks with a root that is NOT the published one, and the wedge
/// would return through here.
#[test]
fn a_write_back_to_an_earlier_value_commits_and_publishes() {
    let mut h = Harness::new(Mode::Rehydrate, Params::default(), Store::fresh());
    for (id, v) in [(1u64, b"A"), (2, b"B")] {
        let fx = h.step(write(id, b"k/v", v));
        assert!(states(&settle(&mut h, fx), id).contains(&State::Published));
    }
    let back = h.step(write(3, b"k/v", b"A"));
    assert_eq!(
        states(&back, 3),
        vec![State::Accepted],
        "A->B->A was short-cut"
    );
    assert!(
        to_node(&back) > 0,
        "A->B->A emitted nothing: the apply now de-duplicates"
    );
    let all = settle(&mut h, back);
    assert!(
        states(&all, 3).contains(&State::Published),
        "{:?}",
        states(&all, 3)
    );
}
