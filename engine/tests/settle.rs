//! sdk#150 E1/E2: a commit that hears nothing is SETTLED FROM FACT -- never
//! left in flight for ever with every later write `Busy`.
//!
//! The engine keeps no bytes of a commit's data puts across calls. Past its
//! pace it looks at what is true: a block the node holds is confirmed; every
//! block in and the head sent -> the head is READ; blocks missing -> re-put
//! from the carried ops (<= 16 KiB), or the commit is `Lost` and the engine
//! released. Every test rebuilds the engine from its context between steps.
use engine::{ClientId, Effect, Event, Op, Params, State, WriteId};
use freenet_prolly::Cid;
use std::collections::BTreeSet;

mod common;
use common::{Harness, Mode, Store};

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

fn puts(fx: &[Effect]) -> Vec<Cid> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::PutBlock { id, .. } | Effect::PutPack { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

/// Small ops: a few keys, a few KiB in all -- carried with the commit.
fn small() -> Vec<(Vec<u8>, Op)> {
    (0..8u8)
        .map(|i| (format!("s/{i}").into_bytes(), Op::Put(vec![i; 700])))
        .collect()
}

/// Ops too large to carry: one 30 KiB value.
fn large() -> Vec<(Vec<u8>, Op)> {
    vec![(b"l/big".to_vec(), Op::Put(vec![9u8; 30 * 1024]))]
}

fn harness() -> Harness {
    let mut h = Harness::new(Mode::Rehydrate, Params::default(), Store::fresh());
    let _ = h.step(Event::Tick(T0));
    h
}

/// Answer everything in `fx` (blocks confirmed, head confirmed), and what
/// that produces, to a standstill.
fn answer(h: &mut Harness, mut fx: Vec<Effect>) -> Vec<Effect> {
    let mut all = fx.clone();
    for _ in 0..50 {
        let mut next = Vec::new();
        for f in &fx {
            match f {
                Effect::PutBlock { id, .. } | Effect::PutPack { id, .. } => {
                    h.store.remember(id);
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

/// Tick until something is told to `id`, up to `n` ticks; everything emitted.
fn tick_until(h: &mut Harness, from: u64, n: u64, id: u64) -> (u64, Vec<Effect>) {
    let mut out = Vec::new();
    for k in 1..=n {
        let fx = h.step(Event::Tick(T0 + from + k));
        let heard = !told(&fx, id).is_empty() || !puts(&fx).is_empty();
        out.extend(fx);
        if heard {
            return (from + k, out);
        }
    }
    (from + n, out)
}

/// CARRIED OPS: every data put DROPPED. Past the pace the engine re-derives
/// the same blocks from the ops it carried and puts them again -- the same
/// ids -- and the write publishes without ever being told `Lost`.
#[test]
fn a_dropped_commit_with_carried_ops_is_re_put_and_publishes() {
    let p = Params::default();
    let mut h = harness();
    let first = h.step(write(1, small()));
    assert_eq!(told(&first, 1), vec![State::Accepted]);
    let sent: BTreeSet<Cid> = puts(&first).into_iter().collect();
    assert!(!sent.is_empty());
    for id in &sent {
        h.store.forget(*id); // the puts never arrived
    }
    let (at, fx) = tick_until(&mut h, 0, 2 * p.reask_after, 1);
    let again: BTreeSet<Cid> = puts(&fx).into_iter().collect();
    assert_eq!(again, sent, "not the same blocks re-put (at tick {at})");
    assert!(at <= p.reask_after + 1, "re-put at tick {at}, past the pace");
    assert!(!told(&fx, 1).contains(&State::Lost), "a carried commit was told Lost");
    let all = answer(&mut h, fx);
    assert!(told(&all, 1).contains(&State::Published), "{:?}", told(&all, 1));
}

/// NOT CARRIED: the same silence, ops too large to carry. The commit is
/// `Lost` within its pace, the engine is RELEASED -- the next write is
/// accepted, not `Busy` -- and the tree is the published one.
#[test]
fn a_dropped_commit_too_large_to_carry_is_lost_and_the_engine_released() {
    let p = Params::default();
    let mut h = harness();
    let root = h.published_root();
    let first = h.step(write(1, large()));
    for id in puts(&first) {
        h.store.forget(id);
    }
    let (at, fx) = tick_until(&mut h, 0, 2 * p.reask_after, 1);
    assert_eq!(told(&fx, 1), vec![State::Lost], "at tick {at}");
    assert!(at <= p.reask_after + 1, "Lost at tick {at}, past the pace");
    assert_eq!(h.published_root(), root);
    let next = h.step(write(2, small()));
    assert_eq!(told(&next, 2), vec![State::Accepted], "the engine was not released");
}


/// E1: the head was sent and never landed -- the read back shows the OLDER
/// seq. That is not a conflict: the same head is issued again, and the
/// commit publishes.
#[test]
fn a_head_that_never_landed_is_issued_again_not_taken_for_a_conflict() {
    let p = Params::default();
    let mut h = harness();
    let first = h.step(write(1, small()));
    // Blocks confirmed as answered; the head effect is NOT answered.
    let mut head = None;
    for f in answer_blocks_only(&mut h, first) {
        if let Effect::UpdateHead { seq, root, .. } = f {
            head = Some((seq, root));
        }
    }
    let (seq, root) = head.expect("the head was sent once the blocks were in");
    // Silence past the pace: the engine asks what is true.
    let (_, fx) = tick_until(&mut h, 0, 2 * p.reask_after, 1);
    assert!(fx.iter().any(|f| matches!(f, Effect::ReadHead { .. })), "the head was never read");
    // The Register still holds the older (empty) head.
    let reissued = h.step(Event::HeadConflict { seq: seq - 1, root: h.published_root() });
    assert!(
        reissued.iter().any(|f| matches!(f, Effect::UpdateHead { seq: s, root: r, .. } if *s == seq && *r == root)),
        "an older head was taken for a conflict: {:?}",
        told(&reissued, 1)
    );
    assert!(!told(&reissued, 1).contains(&State::Lost));
    let done = h.step(Event::HeadConfirmed(seq));
    assert!(told(&done, 1).contains(&State::Published));
}

/// E1's other side (sdk#225): the head this commit was BUILT ON is displaced
/// — the Register shows ITS seq under ANOTHER root (another device of the
/// same identity won the tie-break). That is not "my head never landed": the
/// commit is dead, its writes are told `Lost`, and the winner is adopted. E1
/// alone let the page ask to sign from a displaced prev for ever.
#[test]
fn a_displaced_prev_under_a_pending_commit_is_a_conflict_not_an_unlanded_head() {
    let p = Params::default();
    let mut h = harness();
    let first = h.step(write(1, small()));
    let mut head = None;
    for f in answer_blocks_only(&mut h, first) {
        if let Effect::UpdateHead { seq, .. } = f {
            head = Some(seq);
        }
    }
    let seq = head.expect("the head was sent once the blocks were in");
    let (_, _) = tick_until(&mut h, 0, 2 * p.reask_after, 1);
    let winner: Cid = [7; 32];
    assert_ne!(winner, h.published_root());
    let fx = h.step(Event::HeadConflict { seq: seq - 1, root: winner });
    assert!(told(&fx, 1).contains(&State::Lost), "a displaced prev was taken for an unlanded head: {:?}", told(&fx, 1));
    assert_eq!(h.published_root(), winner, "the winner was not adopted");
}

/// Answer only the block puts in `fx` and what they cause; return every
/// effect seen, head effects unanswered.
fn answer_blocks_only(h: &mut Harness, mut fx: Vec<Effect>) -> Vec<Effect> {
    let mut all = fx.clone();
    for _ in 0..50 {
        let mut next = Vec::new();
        for f in &fx {
            if let Effect::PutBlock { id, .. } | Effect::PutPack { id, .. } = f {
                next.extend(h.step(Event::PutConfirmed(*id)));
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

