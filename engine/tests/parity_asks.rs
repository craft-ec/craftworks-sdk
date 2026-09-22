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
        reads: Vec::new(),
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

/// Publish a 64-record write and return the tick of its first parity ask and
/// the blocks asked (left unanswered).
fn first_ask(h: &mut Harness) -> (u64, Vec<Cid>) {
    let first = h.step(write(1, 64, 0));
    let _ = publish(h, first);
    for k in 1..=4 {
        let got = parity(&h.step(Event::Tick(T0 + k)));
        if !got.is_empty() {
            return (k, got);
        }
    }
    panic!("no parity put at all");
}

/// REVIEW A: a FAILED put is paced like silence, never faster. Before, a
/// failure erased the pace and every call re-put: 2100 PUTs on 100 of 100
/// ticks, executed. And it IS asked again -- the opposite mutant, a failed
/// put never re-asked, goes red on the lower bound.
#[test]
fn a_failed_parity_put_is_asked_again_no_faster_than_silence() {
    let mut h = harness(Params::default());
    let first = h.step(write(1, 64, 0));
    let _ = publish(&mut h, first);
    let mut ticks_with_puts = 0;
    for k in 1..=100 {
        let got = parity(&h.step(Event::Tick(T0 + k)));
        if !got.is_empty() {
            ticks_with_puts += 1;
        }
        for id in got {
            let _ = h.step(Event::PutFailed(id));
        }
    }
    assert!(
        (2..=7).contains(&ticks_with_puts),
        "parity put on {ticks_with_puts} of 100 ticks with every put answered FAILED"
    );
    println!("  every parity put failed: asked on {ticks_with_puts} of 100 ticks");
}

/// REVIEW C: nobody answering is re-asked with DECAY. At a fixed pace one
/// 64-record write took 798 parity puts in 600 ticks (executed); doubled per
/// attempt it is a handful per block, and still more than one.
#[test]
fn unanswered_parity_is_re_asked_with_decay() {
    let mut h = harness(Params::default());
    let (at, blocks) = first_ask(&mut h);
    let mut per_block = std::collections::BTreeMap::<Cid, usize>::new();
    for id in &blocks {
        per_block.insert(*id, 1);
    }
    for k in at + 1..=600 {
        for id in parity(&h.step(Event::Tick(T0 + k))) {
            *per_block.entry(id).or_default() += 1;
        }
    }
    let most = per_block.values().max().copied().unwrap_or(0);
    let least = per_block.values().min().copied().unwrap_or(0);
    let total: usize = per_block.values().sum();
    assert!(
        least >= 3,
        "a block was asked only {least} time(s) in 600 ticks: re-asking stopped"
    );
    assert!(
        most <= 7,
        "a block was asked {most} times in 600 ticks: no decay"
    );
    println!(
        "  600 ticks, nobody answering: {total} parity puts for {} blocks ({least}..{most} each)",
        blocks.len()
    );
}

/// REVIEW B: one tick from the FUTURE must not freeze every deadline. The
/// client's clock came back to the present afterwards; before, that read as
/// "10 years early" to every ask, and nothing was re-asked in 600 ticks.
#[test]
fn a_tick_from_the_future_does_not_freeze_re_asking() {
    let mut h = harness(Params::default());
    let (at, _) = first_ask(&mut h);
    let _ = h.step(Event::Tick(T0 + 10 * 365 * 86_400));
    let mut again = 0;
    for k in at + 1..=at + 600 {
        again += parity(&h.step(Event::Tick(T0 + k))).len();
    }
    assert!(
        again > 0,
        "nothing was re-asked in 600 ticks after one tick from the future"
    );
}

/// ...and a tick from the PAST makes nothing due early: it is a reset, and
/// everything is dated from it -- due a full window later, not at once and
/// not never.
#[test]
fn a_tick_from_the_past_makes_nothing_due_early() {
    let p = Params::default();
    let mut h = harness(p);
    let (_, blocks) = first_ask(&mut h);
    let back = T0 - 1000;
    assert!(
        parity(&h.step(Event::Tick(back))).is_empty(),
        "re-asked on the reset itself"
    );
    for k in 1..p.reask_after {
        assert!(
            parity(&h.step(Event::Tick(back + k))).is_empty(),
            "re-asked {k} tick(s) after a reset, under reask_after"
        );
    }
    let again = parity(&h.step(Event::Tick(back + p.reask_after)));
    assert_eq!(
        again.iter().collect::<BTreeSet<_>>(),
        blocks.iter().collect::<BTreeSet<_>>(),
        "not re-asked a full window after the reset"
    );
}

/// The decay's off-by-one, pinned: the FIRST re-ask comes at exactly
/// `reask_after` ticks, the second at 2x that after it -- not 2x, 4x.
#[test]
fn the_first_re_ask_is_at_reask_after_and_the_second_at_twice_that() {
    let p = Params::default();
    let mut h = harness(p);
    let (at, _) = first_ask(&mut h);
    let mut asked_at = Vec::new();
    for k in at + 1..=at + 3 * p.reask_after + 1 {
        if !parity(&h.step(Event::Tick(T0 + k))).is_empty() {
            asked_at.push(k - at);
        }
    }
    assert_eq!(
        asked_at,
        vec![p.reask_after, 3 * p.reask_after],
        "re-asks at these tick offsets from the first ask"
    );
}

/// Parity puts over 600 silent ticks, with a second clock `lag` behind the
/// first ticking in between (two tabs, or two devices on one node).
fn silent_puts(lag: Option<u64>) -> usize {
    let mut h = harness(Params::default());
    let (at, asked) = first_ask(&mut h);
    let mut n = asked.len();
    for k in at + 1..=600 {
        n += parity(&h.step(Event::Tick(T0 + k))).len();
        if let Some(lag) = lag {
            n += parity(&h.step(Event::Tick(T0 + k - lag))).len();
        }
    }
    n
}

/// TWO CLOCKS, ONE ENGINE: engine time is the most it has seen, so a second
/// clock BEHIND the first -- by a second, or by two minutes -- changes
/// nothing. Taken as a reset, a step back re-anchored every deadline on
/// every other tick: 21 parity puts in 600 s, no re-ask ever (executed).
#[test]
fn a_second_clock_behind_the_first_changes_nothing() {
    let one = silent_puts(None);
    for lag in [1, 2, 120] {
        assert_eq!(
            silent_puts(Some(lag)),
            one,
            "with a second clock {lag} s behind, the parity puts differ from one clock's"
        );
    }
    println!("  one clock: {one} parity puts in 600 silent ticks; 1, 2 and 120 s behind: the same");
}

/// ...and where a step back stops being the same clock: 2 behind is
/// ignored, 601 behind (past a context's lifetime) re-anchors.
#[test]
fn a_tick_601_behind_re_anchors_and_2_behind_does_not() {
    let p = Params::default();
    // 2 behind: nothing moves; the re-ask comes on the original schedule.
    let mut h = harness(p);
    let (at, _) = first_ask(&mut h);
    assert!(parity(&h.step(Event::Tick(T0 + at - 2))).is_empty());
    let first = (at + 1..=at + p.reask_after)
        .find(|k| !parity(&h.step(Event::Tick(T0 + k))).is_empty())
        .map(|k| k - at);
    assert_eq!(first, Some(p.reask_after), "2 behind moved the schedule");
    // 601 behind: a different clock; everything is dated from it.
    let mut h = harness(p);
    let (at, _) = first_ask(&mut h);
    let back = T0 + at - 601;
    assert!(parity(&h.step(Event::Tick(back))).is_empty());
    let first = (1..=p.reask_after).find(|k| !parity(&h.step(Event::Tick(back + k))).is_empty());
    assert_eq!(first, Some(p.reask_after), "601 behind did not re-anchor");
}

/// THE FORWARD HALF OF A RESET, at the bad tick itself. One tick far ahead,
/// taken as time passing, makes every age enormous for THAT call: the write
/// in flight is told `Stalled` falsely and every unanswered ask is re-put at
/// once. Both are checked in that call; the control is the stall timer still
/// firing, `max_accept_age` honest ticks after the reset. (Core dev's mutant
/// on #167: `CLOCK_RESET_TICKS = u64::MAX` passed every other test, because
/// the NEXT ordinary tick is "behind" and resets anyway.)
#[test]
fn one_tick_far_ahead_is_a_reset_in_that_very_call() {
    let p = Params::default();
    let mut h = harness(p);
    let (at, asked) = first_ask(&mut h); // unanswered parity asks
    let second = h.step(write(2, 64, 0x55)); // a commit in flight, never confirmed
    assert!(told(&second, 2).contains(&State::Accepted));
    let far = T0 + at + 10 * 365 * 86_400;
    let out = h.step(Event::Tick(far));
    assert!(
        !told(&out, 2).contains(&State::Stalled),
        "a tick far ahead told the write in flight Stalled at once"
    );
    let reput: Vec<Cid> = parity(&out)
        .into_iter()
        .filter(|id| asked.contains(id))
        .collect();
    assert!(
        reput.is_empty(),
        "a tick far ahead re-put {} unanswered ask(s) at once",
        reput.len()
    );
    // CONTROL: time does pass from the reset on.
    let mut stalled_at = None;
    for k in 1..=p.max_accept_age {
        if told(&h.step(Event::Tick(far + k)), 2).contains(&State::Stalled) {
            stalled_at = Some(k);
            break;
        }
    }
    assert_eq!(
        stalled_at,
        Some(p.max_accept_age),
        "the stall timer after the reset"
    );
}
