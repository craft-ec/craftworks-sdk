//! Acceptance 1: the scheduler alone — no wasm, no node.
//!
//! Every test here states its control, because each rule has a plausible
//! implementation that passes a careless version of its test: a scheduler
//! that releases everything immediately passes a dependency test whose
//! effects had no dependencies; one that drops what does not fit passes a
//! batch test that never looks for the remainder.

use engine::read::Via;
use engine::{ClientId, Effect, Epoch, State, WriteId};
use engine_delegate::schedule::{Limits, Op, Scheduler};
use freenet_prolly::Cid;

fn cid(n: u8) -> Cid {
    [n; 32]
}

fn put(id: u8, after: &[u8]) -> Effect {
    Effect::PutPack {
        id: cid(id),
        bytes: vec![id; 8],
        after: after.iter().map(|a| cid(*a)).collect(),
    }
}

fn get(id: u8) -> Effect {
    Effect::FetchBlock {
        id: cid(id),
        via: Via::Direct,
        attempt: 1,
    }
}

fn ids(ops: &[Op]) -> Vec<u8> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Get { id, .. } | Op::Put { id, .. } => Some(id[0]),
            _ => None,
        })
        .collect()
}

/// A held effect is released exactly when its LAST dependency confirms.
#[test]
fn a_held_effect_waits_for_all_its_dependencies_and_then_goes() {
    let mut s = Scheduler::default();
    s.offer(vec![put(9, &[1, 2, 3])]);
    assert_eq!(s.take(Limits::default()), vec![], "it went out unblocked");
    assert_eq!(s.held_len(), 1);

    s.confirm(cid(1));
    assert!(
        s.take(Limits::default()).is_empty(),
        "released after 1 of 3"
    );
    s.confirm(cid(2));
    assert!(
        s.take(Limits::default()).is_empty(),
        "released after 2 of 3"
    );

    s.confirm(cid(3));
    let out = s.take(Limits::default());
    assert_eq!(ids(&out), vec![9], "the last dependency did not release it");
    assert_eq!(s.held_len(), 0);

    // The control: the SAME effect with no dependencies goes immediately. So
    // the waiting above is the dependency set doing it, not a scheduler that
    // holds everything for a while.
    let mut c = Scheduler::default();
    c.offer(vec![put(9, &[])]);
    assert_eq!(
        ids(&c.take(Limits::default())),
        vec![9],
        "an effect with no dependencies was held anyway, so the test above \
         does not show that dependencies are what hold one"
    );
}

/// A dependency confirmed BEFORE the effect that names it still counts.
///
/// The node can answer a put while the next effect is still being built, so
/// the two orders both happen. A scheduler that only ever subtracts on
/// `confirm` holds the second one for ever, and nothing re-confirms it.
#[test]
fn a_dependency_confirmed_before_the_effect_arrives_still_releases_it() {
    let mut s = Scheduler::default();
    s.confirm(cid(1));
    s.offer(vec![put(9, &[1])]);
    assert_eq!(
        ids(&s.take(Limits::default())),
        vec![9],
        "a dependency confirmed before the effect arrived was forgotten, so \
         the effect waits for a confirmation that already happened"
    );

    // The control: an UNconfirmed dependency in the same position holds it.
    let mut c = Scheduler::default();
    c.confirm(cid(7));
    c.offer(vec![put(9, &[1])]);
    assert!(
        c.take(Limits::default()).is_empty(),
        "confirming an unrelated block released the effect, so the check is \
         not about the dependency at all"
    );
}

/// A return carries at most the limits, and the remainder is KEPT.
#[test]
fn a_batch_is_capped_and_what_does_not_fit_waits_rather_than_vanishing() {
    let limits = Limits {
        max_gets: 4,
        max_puts: 8,
    };
    let mut s = Scheduler::default();
    let offered: Vec<Effect> = (1..=10u8).map(get).collect();
    s.offer(offered);

    let first = s.take(limits);
    assert_eq!(first.len(), 4, "the get limit was not respected");
    assert_eq!(ids(&first), vec![1, 2, 3, 4], "the order was not preserved");
    let second = s.take(limits);
    assert_eq!(ids(&second), vec![5, 6, 7, 8]);
    let third = s.take(limits);
    assert_eq!(ids(&third), vec![9, 10]);
    assert_eq!(s.ready_len(), 0, "something was left behind");
    assert!(
        s.take(limits).is_empty(),
        "a fourth return produced ops from nowhere"
    );

    // Every one of the ten came out, exactly once and in order. A scheduler
    // that DROPPED the overflow would pass a test that only checked the first
    // return's size — the core never re-emits them, so a dropped op is a lost
    // write.
    let all: Vec<u8> = [first, second, third]
        .concat()
        .iter()
        .filter_map(|o| match o {
            Op::Get { id, .. } => Some(id[0]),
            _ => None,
        })
        .collect();
    assert_eq!(all, (1..=10u8).collect::<Vec<_>>());

    // The control: a limit large enough takes them all in one return, so the
    // splitting above is the LIMIT doing it.
    let mut c = Scheduler::default();
    c.offer((1..=10u8).map(get).collect());
    assert_eq!(
        c.take(Limits {
            max_gets: 100,
            max_puts: 100
        })
        .len(),
        10,
        "a limit of 100 still split the batch, so the cap is not what splits it"
    );
}

/// Gets and puts are capped SEPARATELY, and neither eats the other's room.
#[test]
fn the_two_limits_are_independent() {
    let limits = Limits {
        max_gets: 2,
        max_puts: 3,
    };
    let mut s = Scheduler::default();
    let mut offered: Vec<Effect> = (1..=5u8).map(get).collect();
    offered.extend((10..=15u8).map(|i| put(i, &[])));
    s.offer(offered);

    let out = s.take(limits);
    let gets = out.iter().filter(|o| matches!(o, Op::Get { .. })).count();
    let puts = out.iter().filter(|o| matches!(o, Op::Put { .. })).count();
    assert_eq!(gets, 2, "took {gets} gets against a limit of 2");
    assert_eq!(puts, 3, "took {puts} puts against a limit of 3");
    // Five ops in one return, from two caps of 2 and 3: a single shared cap
    // would have taken at most 3.
    assert_eq!(out.len(), 5);
}

/// Nothing is issued twice, however often it is offered.
#[test]
fn an_op_is_never_issued_twice_but_a_later_attempt_is_not_a_repeat() {
    let mut s = Scheduler::default();
    s.offer(vec![put(9, &[])]);
    assert_eq!(ids(&s.take(Limits::default())), vec![9]);
    // The core does not re-emit a put, but a shell that re-offered its whole
    // queue would; the node must not be asked to store it twice.
    s.offer(vec![put(9, &[])]);
    assert!(
        s.take(Limits::default()).is_empty(),
        "the same put was issued twice"
    );

    // A fetch is different: the core RE-ISSUES one deliberately when an
    // attempt did not answer, and that is a new op. Refusing it would leave
    // the read waiting for ever on a block nobody is asking for any more.
    s.offer(vec![Effect::FetchBlock {
        id: cid(5),
        via: Via::Direct,
        attempt: 1,
    }]);
    assert_eq!(ids(&s.take(Limits::default())), vec![5]);
    s.offer(vec![Effect::FetchBlock {
        id: cid(5),
        via: Via::Direct,
        attempt: 1,
    }]);
    assert!(
        s.take(Limits::default()).is_empty(),
        "the same attempt went out twice"
    );
    s.offer(vec![Effect::FetchBlock {
        id: cid(5),
        via: Via::Direct,
        attempt: 2,
    }]);
    assert_eq!(
        ids(&s.take(Limits::default())),
        vec![5],
        "a LATER attempt was refused as a repeat, so a read whose first try \
         missed can never be retried"
    );
}

/// What the core emits for the CLIENT is not a node operation, and is counted.
#[test]
fn client_effects_are_not_node_operations_and_are_counted() {
    let mut s = Scheduler::default();
    s.offer(vec![
        Effect::Notify {
            client: ClientId(1),
            write_id: WriteId(1),
            state: State::Accepted,
        },
        Effect::Progress {
            client: ClientId(1),
            req_id: engine::read::ReqId(1),
            levels_done: 1,
            levels_total: 2,
        },
        put(9, &[]),
    ]);
    let out = s.take(Limits::default());
    assert_eq!(
        ids(&out),
        vec![9],
        "a client effect became a node operation"
    );
    assert_eq!(
        s.not_an_op, 2,
        "client effects were dropped without being counted; anything the \
         shell does not turn into an op must be visible, or a whole class of \
         them can go missing in silence"
    );
}

/// A head bump waits for its packs, which is the ordering the format needs.
#[test]
fn a_head_waits_for_the_blocks_it_names() {
    let mut s = Scheduler::default();
    s.offer(vec![
        put(1, &[]),
        put(2, &[]),
        Effect::UpdateHead {
            seq: 7,
            root: cid(9),
            after: vec![cid(1), cid(2)],
        },
    ]);
    let first = s.take(Limits::default());
    assert_eq!(ids(&first), vec![1, 2], "the head went out with its packs");
    assert!(
        !first.iter().any(|o| matches!(o, Op::Head { .. })),
        "a head written before its packs names blocks nobody has"
    );
    s.confirm(cid(1));
    assert!(s.take(Limits::default()).is_empty());
    s.confirm(cid(2));
    assert!(
        matches!(
            s.take(Limits::default()).as_slice(),
            [Op::Head { seq: 7, .. }]
        ),
        "the head did not go out once both its packs were confirmed"
    );
}

/// A ReadHead is not about a block and never waits.
#[test]
fn a_read_head_goes_out_immediately() {
    let mut s = Scheduler::default();
    s.offer(vec![Effect::ReadHead { epoch: Epoch(2) }]);
    assert!(
        matches!(
            s.take(Limits::default()).as_slice(),
            [Op::ReadHead { epoch: Epoch(2) }]
        ),
        "the engine's first act on a fresh start was held"
    );
}
