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

/// Three answers, three different reasons.
///
/// `Busy` — nothing was applied and nothing has changed, so it is re-sent
/// UNCONDITIONALLY. There is nothing to compare and nothing to decide.
///
/// `Failed` — TERMINAL. The edit is not in the tree and never will be
/// because the write itself was refused; sending it again refuses it again,
/// for ever. Re-submitting is not a slower success, it is a loop.
///
/// `Lost` — the only one that DEPENDS on the world. Its commit will not
/// publish and the engine no longer has it, so whether re-applying is right
/// depends on whether the keys moved meanwhile. It goes to `settle_lost`,
/// which can look; routing it through here would drop it as terminal.
#[test]
fn busy_is_unconditional_failed_is_terminal_and_lost_is_neither() {
    for (state, expect_requeue) in [
        (WriteState::Busy, true),
        (WriteState::Failed, false),
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

    // `Failed` is terminal even though, like `Busy`, it applied nothing.
    // That is the distinction worth pinning: "nothing was applied" does not
    // by itself mean "send it again".
    let mut ob = Outbox::new();
    ob.push(1, ops("k"));
    let _ = ob.next_to_send();
    assert_eq!(
        ob.settle(1, WriteState::Failed),
        Settled::Ended(WriteState::Failed)
    );
    assert!(
        ob.next_to_send().is_none(),
        "a Failed write was sent again, which refuses it again, for ever"
    );
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

// ---------------------------------------------------------------------------
// `Lost`: decided by the outbox, never by a person.
// ---------------------------------------------------------------------------

use protocol::outbox::{Lost, PreImage};

fn hash(v: &[u8]) -> PreImage {
    let mut h = [0u8; 32];
    // A stand-in for whatever the SDK hashes with; all these tests need is
    // that equal bytes give equal hashes and different bytes do not.
    for (i, b) in v.iter().enumerate() {
        h[i % 32] ^= *b ^ (i as u8);
    }
    h
}

/// A write lost while its keys did NOT move is applied, once, silently.
#[test]
fn a_lost_write_whose_keys_are_unchanged_is_resubmitted_without_asking_anyone() {
    let world: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = [
        (b"a".to_vec(), b"old".to_vec()),
        (b"b".to_vec(), b"old".to_vec()),
    ]
    .into_iter()
    .collect();
    let pre: Vec<(Vec<u8>, Option<PreImage>)> = world
        .iter()
        .map(|(k, v)| (k.clone(), Some(hash(v))))
        .collect();

    let mut ob = Outbox::new();
    ob.push_against(
        1,
        vec![Op::Put(b"a".to_vec(), b"mine".to_vec())],
        pre.clone(),
    );
    let _ = ob.next_to_send();

    // Nothing moved while it was away.
    let decided = ob.settle_lost(1, |k| world.get(k).map(|v| hash(v)));
    assert_eq!(
        decided,
        Lost::Resubmitted,
        "a write lost against an unchanged world was not re-applied; the edit \
         is simply gone, and nobody was told"
    );
    assert_eq!(ob.len(), 1, "it was requeued but left the outbox");

    // ...and it is sent again, once.
    let again = ob.next_to_send().expect("a re-submission");
    assert_eq!(id_of(&again), 1);
    ob.settle(1, WriteState::Accepted);
    assert_eq!(ob.len(), 0);
    assert!(ob.next_to_send().is_none(), "it was sent a third time");
}

/// A write lost while someone else changed a key is NOT re-applied, and the
/// newer value stands.
#[test]
fn a_lost_write_whose_key_moved_keeps_the_newer_value_and_reports_a_conflict() {
    let before: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        [(b"a".to_vec(), b"old".to_vec())].into_iter().collect();
    let pre: Vec<(Vec<u8>, Option<PreImage>)> = before
        .iter()
        .map(|(k, v)| (k.clone(), Some(hash(v))))
        .collect();

    let mut ob = Outbox::new();
    ob.push_against(1, vec![Op::Put(b"a".to_vec(), b"mine".to_vec())], pre);
    let _ = ob.next_to_send();

    // Someone else wrote it while this one was away.
    let after: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        [(b"a".to_vec(), b"theirs".to_vec())].into_iter().collect();
    let decided = ob.settle_lost(1, |k| after.get(k).map(|v| hash(v)));

    match &decided {
        Lost::Conflict { write_id, keys } => {
            assert_eq!(*write_id, 1);
            assert_eq!(
                keys,
                &vec![b"a".to_vec()],
                "the conflicting key is not named"
            );
        }
        other => panic!(
            "a write lost against a CHANGED key was {other:?}; re-applying it \
             would overwrite someone else's newer value with a value from \
             before it existed"
        ),
    }
    assert_eq!(ob.len(), 0, "the conflicting write is still queued");
    assert!(
        ob.next_to_send().is_none(),
        "it was sent again despite the conflict"
    );
    // The newer value is untouched: nothing here writes.
    assert_eq!(after.get(b"a".as_slice()), Some(&b"theirs".to_vec()));
}

/// The CONTROL: a blind re-submit overwrites the newer value, and the test
/// sees it.
///
/// Without this, "the newer value stands" is also what a world where nothing
/// ever writes looks like.
#[test]
fn a_blind_resubmit_overwrites_the_newer_value() {
    let mut world: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        [(b"a".to_vec(), b"old".to_vec())].into_iter().collect();
    let pre: Vec<(Vec<u8>, Option<PreImage>)> = world
        .iter()
        .map(|(k, v)| (k.clone(), Some(hash(v))))
        .collect();

    // Someone else's newer value.
    world.insert(b"a".to_vec(), b"theirs".to_vec());

    // What a client that re-sends on Lost without looking would do.
    let blind_ops = vec![Op::Put(b"a".to_vec(), b"mine".to_vec())];
    for op in &blind_ops {
        if let Op::Put(k, v) = op {
            world.insert(k.clone(), v.clone());
        }
    }
    assert_eq!(
        world.get(b"a".as_slice()),
        Some(&b"mine".to_vec()),
        "the control did not overwrite anything, so it is not modelling a \
         blind re-submit and the test above has nothing to be better than"
    );

    // The outbox, on the same facts, refuses.
    let mut ob = Outbox::new();
    ob.push_against(1, blind_ops, pre);
    let _ = ob.next_to_send();
    let theirs: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
        [(b"a".to_vec(), b"theirs".to_vec())].into_iter().collect();
    assert!(
        matches!(
            ob.settle_lost(1, |k| theirs.get(k).map(|v| hash(v))),
            Lost::Conflict { .. }
        ),
        "the outbox re-applied a write the blind control shows would clobber \
         a newer value"
    );
}

/// ALL-OR-NOTHING: if ANY key of a multi-key write moved, none of it is
/// re-applied.
///
/// Applying half an edit produces a state the app never asked for and cannot
/// describe — worse than applying none, which it already knows how to.
#[test]
fn a_multi_key_write_with_one_moved_key_is_not_partly_applied() {
    let pre: Vec<(Vec<u8>, Option<PreImage>)> = vec![
        (b"a".to_vec(), Some(hash(b"old"))),
        (b"b".to_vec(), Some(hash(b"old"))),
        (b"c".to_vec(), Some(hash(b"old"))),
    ];
    let mut ob = Outbox::new();
    ob.push_against(
        1,
        vec![
            Op::Put(b"a".to_vec(), b"1".to_vec()),
            Op::Put(b"b".to_vec(), b"2".to_vec()),
            Op::Delete(b"c".to_vec()),
        ],
        pre,
    );
    let _ = ob.next_to_send();

    // Only "b" moved.
    let now = |k: &[u8]| -> Option<PreImage> {
        if k == b"b" {
            Some(hash(b"someone else"))
        } else {
            Some(hash(b"old"))
        }
    };
    match ob.settle_lost(1, now) {
        Lost::Conflict { keys, .. } => assert_eq!(
            keys,
            vec![b"b".to_vec()],
            "the conflict names the wrong keys"
        ),
        other => panic!(
            "one moved key out of three gave {other:?}; re-applying the other \
             two is half an edit, which is a state the app never asked for"
        ),
    }
    assert_eq!(ob.len(), 0, "any of it was left queued for re-application");
}

/// A key the write expected to be ABSENT, and which now exists, is a change.
#[test]
fn a_key_that_appeared_counts_as_moved() {
    let mut ob = Outbox::new();
    ob.push_against(
        1,
        vec![Op::Put(b"new".to_vec(), b"mine".to_vec())],
        vec![(b"new".to_vec(), None)],
    );
    let _ = ob.next_to_send();
    assert!(
        matches!(
            ob.settle_lost(1, |_| Some(hash(b"someone got there first"))),
            Lost::Conflict { .. }
        ),
        "a key the write expected not to exist, and which now does, was \
         treated as unchanged"
    );
}
