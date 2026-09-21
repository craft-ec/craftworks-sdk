//! sdk#150 PR 3: owed parity is settled by the node's ANSWERS, and only
//! PACED by what was asked -- across calls, as a delegate runs.
//!
//! `sent = true` at emission, cleared by nothing and re-derived from
//! `in_flight_parity` on every rehydrate, made every lost parity effect a
//! permanent loss: the first commit's parity, held on the empty tree's root
//! and dropped with the call, was never written (the architect's probe: not
//! after 130 ticks, a Flush, or a second write). Every test here rebuilds the
//! engine from its context between steps.

use engine::{ClientId, Effect, Event, Op, Params, State, WriteId};
use freenet_prolly::Cid;
use std::collections::BTreeSet;

mod common;
use common::{Harness, Mode, Store};

const T0: u64 = 1_790_000_000;

/// Values by reference, so leaves carry parity over them.
fn write(id: u64, n: u32, salt: u8) -> Event {
    Event::Write {
        client: ClientId(1),
        write_id: WriteId(id),
        ops: (0..n)
            .map(|i| {
                (
                    format!("k/{i:05}").into_bytes(),
                    Op::Put(vec![(i % 251) as u8 ^ salt; 1400]),
                )
            })
            .collect(),
    }
}

fn parity(fx: &[Effect]) -> Vec<Cid> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::PutParity { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn told(fx: &[Effect], id: u64) -> Vec<State> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notify {
                write_id, state, ..
            } if write_id.0 == id => Some(*state),
            _ => None,
        })
        .collect()
}

/// Confirm the commit's data and head as they come -- NOT its parity, which
/// is returned for the test to answer or withhold.
fn publish(h: &mut Harness, first: Vec<Effect>) -> Vec<Effect> {
    let mut all = first.clone();
    let mut fx = first;
    for _ in 0..200 {
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
    panic!("the commit never settled");
}

fn harness(p: Params) -> Harness {
    let mut h = Harness::new(Mode::Rehydrate, p, Store::fresh());
    let _ = h.step(Event::Tick(T0));
    h
}

/// THE FIRST COMMIT: no parity goes out while it is unpublished -- a put
/// gated `after` the empty tree's root is one the scheduler holds and drops
/// -- and once it publishes, its parity goes out and settles.
#[test]
fn the_first_commits_parity_waits_for_its_publish_and_then_completes() {
    let mut h = harness(Params::default());
    let first = h.step(write(1, 64, 0));
    // Ticks WHILE the commit is in flight, before any confirmation.
    let mut early = parity(&first);
    for k in 1..=5 {
        early.extend(parity(&h.step(Event::Tick(T0 + k))));
    }
    assert!(
        early.is_empty(),
        "{} parity put(s) before the first publish",
        early.len()
    );
    let all = publish(&mut h, first);
    assert!(told(&all, 1).contains(&State::Published));
    assert!(parity(&all).is_empty(), "parity before the publish's tick");

    let mut asked: Vec<Cid> = Vec::new();
    for k in 6..=8 {
        asked.extend(parity(&h.step(Event::Tick(T0 + k))));
    }
    assert!(!asked.is_empty(), "the first commit's parity was never put");
    let mut out = Vec::new();
    for id in &asked {
        out.extend(h.step(Event::PutConfirmed(*id)));
    }
    assert!(
        told(&out, 1).contains(&State::ParityComplete),
        "write 1 was never told ParityComplete: {:?}",
        told(&out, 1)
    );
    println!(
        "  first commit: 0 parity puts in flight, {} after publish, ParityComplete",
        asked.len()
    );
}

/// A PARITY PUT THAT IS NEVER ANSWERED is asked again, exactly `reask_after`
/// ticks later and not before; answered, it stops.
#[test]
fn an_unanswered_parity_put_is_asked_again_after_reask_after_ticks() {
    let p = Params::default();
    let mut h = harness(p);
    let first = h.step(write(1, 64, 0));
    let _ = publish(&mut h, first);
    let (mut at, mut first_ask) = (0u64, Vec::new());
    for k in 1..=4 {
        let got = parity(&h.step(Event::Tick(T0 + k)));
        if !got.is_empty() {
            (at, first_ask) = (k, got);
            break;
        }
    }
    assert!(!first_ask.is_empty(), "no parity put at all");
    // Withheld: no answer. Nothing again until reask_after ticks have passed.
    let mut again = Vec::new();
    for k in at + 1..at + p.reask_after {
        let got = parity(&h.step(Event::Tick(T0 + k)));
        assert!(
            got.is_empty(),
            "re-asked after {} tick(s), under reask_after ({})",
            k - at,
            p.reask_after
        );
    }
    again.extend(parity(&h.step(Event::Tick(T0 + at + p.reask_after))));
    let a: BTreeSet<Cid> = first_ask.iter().copied().collect();
    let b: BTreeSet<Cid> = again.iter().copied().collect();
    assert_eq!(a, b, "not the same blocks asked again at reask_after");
    // Answered: it stops.
    let mut out = Vec::new();
    for id in &again {
        out.extend(h.step(Event::PutConfirmed(*id)));
    }
    assert!(told(&out, 1).contains(&State::ParityComplete));
    for k in 1..=2 * p.reask_after {
        let got = parity(&h.step(Event::Tick(T0 + at + p.reask_after + k)));
        assert!(
            got.is_empty(),
            "asked again after every block was confirmed"
        );
    }
}

/// CONFIRMATIONS ARE FACTS, carried: a group two-thirds confirmed in earlier
/// calls re-asks for the ONE block not confirmed.
#[test]
fn a_partly_confirmed_group_asks_again_only_for_what_is_missing() {
    let p = Params::default();
    let mut h = harness(p);
    let first = h.step(write(1, 64, 0));
    let _ = publish(&mut h, first);
    let mut asked = Vec::new();
    let mut at = 0;
    for k in 1..=4 {
        asked = parity(&h.step(Event::Tick(T0 + k)));
        if !asked.is_empty() {
            at = k;
            break;
        }
    }
    assert!(asked.len() >= 3);
    let (answered, withheld) = asked.split_at(asked.len() - 1);
    for id in answered {
        let _ = h.step(Event::PutConfirmed(*id));
    }
    let again = parity(&h.step(Event::Tick(T0 + at + p.reask_after)));
    assert_eq!(
        again,
        withheld.to_vec(),
        "asked again for more than was missing"
    );
}

/// THE AGES ARE CARRIED: a group coded at tick T is still "changed this
/// tick" in a later call at the same T, so it is not put until T+1. Rebuilt
/// as 0 they read "settled long ago" and the parity went on the first tick
/// after any call boundary.
#[test]
fn an_owed_groups_age_survives_the_call() {
    let mut h = harness(Params::default());
    let first = h.step(write(1, 64, 0));
    let _ = publish(&mut h, first);
    assert!(
        parity(&h.step(Event::Tick(T0))).is_empty(),
        "a group coded this tick was put this tick: its last_changed was not carried"
    );
    assert!(!parity(&h.step(Event::Tick(T0 + 1))).is_empty());
}

/// THE TABLE HAS A CAP, in asks: at most `max_asks` are out at once, the
/// rest stay owed and go as answers free places.
#[test]
fn the_asks_table_holds_at_most_max_asks_and_the_rest_wait() {
    let p = Params {
        max_asks: 4,
        ..Params::default()
    };
    let mut h = harness(p);
    let first = h.step(write(1, 64, 0));
    let _ = publish(&mut h, first);
    let got = parity(&h.step(Event::Tick(T0 + 1)));
    assert_eq!(got.len(), 4, "{} asks out under a cap of 4", got.len());
    let mut all: BTreeSet<Cid> = got.iter().copied().collect();
    let mut k = 2;
    let mut next = got;
    while !next.is_empty() && k < 100 {
        for id in &next {
            let _ = h.step(Event::PutConfirmed(*id));
        }
        next = parity(&h.step(Event::Tick(T0 + k)));
        assert!(next.len() <= 4);
        all.extend(next.iter().copied());
        k += 1;
    }
    assert!(
        all.len() > 4,
        "nothing went out after the first four were answered"
    );
    println!(
        "  cap 4: {} parity blocks put, never more than 4 out",
        all.len()
    );
}

/// The table's worst case is what `asks::ASK_BYTES` says, measured.
#[test]
fn an_ask_costs_at_most_ask_bytes_in_the_context() {
    let p = Params::default();
    let mut h = harness(p);
    let first = h.step(write(1, 64, 0));
    let _ = publish(&mut h, first);
    let before = h.context_len();
    let asked = parity(&h.step(Event::Tick(T0 + 1)));
    assert!(!asked.is_empty());
    let grew = h.context_len() - before;
    assert!(
        grew <= asked.len() * engine::asks::ASK_BYTES,
        "{} asks grew the context by {grew} B, over {} B each",
        asked.len(),
        engine::asks::ASK_BYTES
    );
    println!(
        "  {} asks: +{grew} B ({} B each)",
        asked.len(),
        grew / asked.len()
    );
}
