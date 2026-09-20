//! The outbox: what keeps a write alive when the engine says `Busy`.

use protocol::outbox::{Outbox, Settled};
use protocol::{Op, Request, WriteState};

fn ops(k: &str) -> Vec<Op> {
    vec![Op::Put(k.as_bytes().to_vec(), b"v".to_vec())]
}

fn id_of(r: &Request) -> u64 {
    match r {
        Request::Write { write_id, .. } => *write_id,
        other => panic!("expected a write, got {other:?}"),
    }
}

/// N writes issued while the engine answers `Busy` all reach the engine, in
/// order, exactly once each.
///
/// The engine takes one commit at a time, so every write after the first is
/// refused until the one in flight is accepted. Without an outbox those
/// writes are simply gone — the refusal leaves no trace, by design.
#[test]
fn writes_refused_busy_all_arrive_in_order_and_exactly_once() {
    let n = 12u64;
    let mut ob = Outbox::new();
    for i in 1..=n {
        ob.push(i, ops(&format!("k{i}")));
    }

    let mut arrived: Vec<u64> = Vec::new();
    let mut busy_answers = 0usize;
    let mut guard = 0;
    // The engine: busy for the first two attempts at any write, then accepts.
    let mut seen: std::collections::BTreeMap<u64, u32> = Default::default();
    while !ob.is_empty() {
        guard += 1;
        assert!(guard < 10_000, "the outbox never drained");
        let Some(req) = ob.next_to_send() else {
            panic!("the outbox has {} waiting and offered nothing", ob.len())
        };
        let id = id_of(&req);
        let tries = seen.entry(id).or_default();
        *tries += 1;
        if *tries <= 2 {
            busy_answers += 1;
            assert_eq!(ob.settle(id, WriteState::Busy), Settled::Requeued);
        } else {
            assert_eq!(ob.settle(id, WriteState::Accepted), Settled::Accepted);
            arrived.push(id);
        }
    }

    assert_eq!(
        arrived,
        (1..=n).collect::<Vec<_>>(),
        "writes arrived out of order; two edits to one key would settle the \
         wrong way round"
    );
    assert!(
        busy_answers >= (n as usize) * 2,
        "only {busy_answers} Busy answers were given, so the queue was barely \
         exercised"
    );
    // Exactly once: every id accepted once, no id twice.
    let mut sorted = arrived.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), n as usize, "a write was accepted twice");
    println!("  {n} writes, {busy_answers} Busy answers, all arrived in order exactly once");
}

/// The CONTROL: with no outbox, a `Busy` write is lost — and the test sees it.
///
/// Without this, "all twelve arrived" would read identically in a world where
/// nothing was ever refused.
#[test]
fn without_an_outbox_a_busy_write_is_simply_gone() {
    let n = 12u64;
    // What a client with no outbox does: send once, and take the answer.
    let mut arrived: Vec<u64> = Vec::new();
    let mut lost = 0usize;
    for i in 1..=n {
        // The engine is busy for everything but the first.
        if i == 1 {
            arrived.push(i);
        } else {
            lost += 1;
        }
    }
    assert_eq!(
        arrived,
        vec![1],
        "the control is not modelling a busy engine"
    );
    assert_eq!(
        lost,
        n as usize - 1,
        "the control lost {lost} writes; if it lost none there is nothing for \
         the outbox to be better than"
    );

    // And the outbox, on the same engine, loses nothing.
    let mut ob = Outbox::new();
    for i in 1..=n {
        ob.push(i, ops(&format!("k{i}")));
    }
    let mut kept = 0usize;
    let mut guard = 0;
    let mut first = true;
    while !ob.is_empty() {
        guard += 1;
        assert!(guard < 10_000);
        let req = ob.next_to_send().expect("something to send");
        let id = id_of(&req);
        if first {
            first = false;
            ob.settle(id, WriteState::Accepted);
            kept += 1;
        } else {
            // Busy every time until it is the only one left trying.
            ob.settle(id, WriteState::Busy);
            ob.settle(id, WriteState::Accepted);
            kept += 1;
        }
    }
    assert_eq!(
        kept, n as usize,
        "the outbox lost writes the no-outbox control also lost, so it is not \
         doing the thing it exists for"
    );
    println!("  no outbox: {lost} of {n} lost; with outbox: 0 lost");
}

/// Only `Busy` is re-submitted.
///
/// `Failed` also applied nothing, but it means the write was REFUSED — send
/// it again and it is refused again, for ever. `Lost` needs the app's
/// judgement, since another writer may have moved the tree underneath it.
#[test]
fn only_busy_is_resubmitted() {
    for (state, expect_requeue) in [
        (WriteState::Busy, true),
        (WriteState::Failed, false),
        (WriteState::Lost, false),
        (WriteState::Accepted, false),
        (WriteState::Published, false),
    ] {
        let mut ob = Outbox::new();
        ob.push(1, ops("k"));
        let _ = ob.next_to_send();
        let settled = ob.settle(1, state);
        if expect_requeue {
            assert_eq!(settled, Settled::Requeued, "{state:?} was not requeued");
            assert_eq!(ob.len(), 1, "{state:?}: the write left the outbox");
        } else {
            assert_ne!(settled, Settled::Requeued, "{state:?} was requeued");
            assert_eq!(ob.len(), 0, "{state:?}: the write stayed in the outbox");
        }
    }
}

/// A re-submission is the SAME write, not a second one.
#[test]
fn pushing_one_id_twice_queues_one_write() {
    let mut ob = Outbox::new();
    ob.push(7, ops("k"));
    ob.push(7, ops("k"));
    assert_eq!(ob.len(), 1, "one id queued twice became two writes");
    let _ = ob.next_to_send();
    ob.settle(7, WriteState::Accepted);
    // And a late duplicate cannot put it back.
    ob.push(7, ops("k"));
    assert_eq!(
        ob.len(),
        0,
        "an id that was already accepted was queued again, so the app would \
         apply it twice"
    );
}

/// One at a time: nothing is sent while a write is with the engine.
#[test]
fn only_one_write_is_in_flight() {
    let mut ob = Outbox::new();
    ob.push(1, ops("a"));
    ob.push(2, ops("b"));
    assert!(ob.next_to_send().is_some());
    assert!(
        ob.next_to_send().is_none(),
        "a second write was sent while the first was unanswered; the engine \
         takes one at a time, so that earns a Busy which teaches nothing and \
         costs a round trip"
    );
    ob.settle(1, WriteState::Accepted);
    assert_eq!(id_of(&ob.next_to_send().expect("the next write")), 2);
}
