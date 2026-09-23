//! sdk#162, the owed cap: DATA OUTRANKS REDUNDANCY. Past `max_owed_groups`
//! a write's parity is left uncoded -- the write still publishes, nothing
//! already owed is forgotten (owed is not re-derivable today, sdk#181), and
//! the engine never goes Busy for its redundancy backlog. Here the node never
//! answers a parity put, so owed never drains.
use engine::{ClientId, Effect, Event, Op, Params, State, WriteId};

mod common;
use common::{Harness, Mode, Store};

const T0: u64 = 1_790_000_000;

fn told(fx: &[Effect], id: u64) -> Vec<State> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notify { write_id, state, .. } if write_id.0 == id => Some(*state),
            _ => None,
        })
        .collect()
}

/// Answer the data puts and the head, never a parity put.
fn publish(h: &mut Harness, mut fx: Vec<Effect>) -> Vec<Effect> {
    let mut all = fx.clone();
    for _ in 0..100 {
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
fn past_the_owed_cap_writes_still_publish_and_their_parity_is_left_to_the_scrub() {
    let cap = 8;
    let p = Params {
        max_owed_groups: cap,
        ..Params::default()
    };
    let mut h = Harness::new(Mode::Rehydrate, p, Store::fresh());
    let _ = h.step(Event::Tick(T0));
    let mut told_all = Vec::new();
    let mut most_owed = 0;
    for i in 0..200u64 {
        // One key each, a value large enough to be carried by reference, so
        // every write codes parity.
        let fx = h.step(Event::forced_write(ClientId(1), WriteId(i + 1), vec![(format!("k/{i:05}").into_bytes(), Op::Put(vec![(i % 251) as u8; 1400]))]));
        let all = publish(&mut h, fx);
        told_all.push((i + 1, told(&all, i + 1)));
        // Time passes; parity is asked for and never answered.
        let _ = h.step(Event::Tick(T0 + i + 1));
        most_owed = most_owed.max(h.owed_groups());
    }
    let published = told_all.iter().filter(|(_, t)| t.contains(&State::Published)).count();
    let busy = told_all.iter().filter(|(_, t)| t.contains(&State::Busy)).count();
    println!(
        "  cap {cap}: {published} of 200 published, {busy} Busy, most owed {most_owed}, parity left to the scrub for {} group(s)",
        h.shed.uncoded
    );
    assert_eq!(published, 200, "not every write published past the owed cap");
    assert_eq!(busy, 0, "a write was refused for the redundancy backlog");
    assert!(most_owed <= cap, "owed grew to {most_owed}, over its cap of {cap}");
    assert!(h.shed.uncoded > 0, "nothing was left uncoded: the cap was never reached");
    // A write whose parity was left uncoded is never told ParityComplete.
    let late: Vec<&(u64, Vec<State>)> = told_all.iter().skip(100).collect();
    assert!(
        late.iter().all(|(_, t)| !t.contains(&State::ParityComplete)),
        "a write past the cap was told ParityComplete"
    );
    // And the engine is open.
    let fx = h.step(Event::forced_write(ClientId(1), WriteId(999), vec![(b"z/after".to_vec(), Op::Put(b"x".to_vec()))]));
    assert!(told(&publish(&mut h, fx), 999).contains(&State::Published));
}

/// Construction refuses params whose caps cannot fit the context's bound.
#[test]
#[should_panic(expected = "the context's caps do not fit its bound")]
fn caps_that_cannot_fit_the_bound_are_refused_at_construction() {
    let _ = engine::Engine::new(
        Params {
            max_carried_ops_bytes: 400 * 1024,
            ..Params::default()
        },
        Store::fresh(),
    );
}

/// ...and the default params fit, with room for parked reads.
#[test]
fn the_default_caps_fit_the_bound() {
    let _ = engine::Engine::new(Params::default(), Store::fresh());
}
