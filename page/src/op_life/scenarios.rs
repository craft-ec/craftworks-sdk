//! OP-LIFE.md's scenarios that a random sequence reaches too rarely to be its only witness, each driven by
//! construction: N2's bounded memory, and L5 under a background op the node never answers.
use crate::op_life::UNTRACKED_KEYS_MAX;
use crate::*;

fn page() -> Page {
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.answered(&Waiting::RecoverHead);
    let _ = p.take_ops();
    p
}

fn put(i: u32) -> (Waiting, Op) {
    let mut id = [0u8; 32];
    id[..4].copy_from_slice(&i.to_be_bytes());
    id[31] = 0x77;
    (Waiting::Put(id), Op::Put { id, bytes: vec![1] })
}

/// Tick to the page's next due, once.
fn next(p: &mut Page) {
    let t = p.next_due().expect("something is due").0;
    p.tick(Ms(t));
    let _ = p.take_ops();
}

/// **N2's memory is BOUNDED** (the architect): a long session whose node never answers a lost PUT -- one key per block,
/// the audit's repair PUTs -- keeps at most [`UNTRACKED_KEYS_MAX`] keys, the rest dropped and COUNTED.
#[test]
fn the_late_answer_memory_is_capped_and_its_evictions_are_counted() {
    let mut p = page();
    let keys = UNTRACKED_KEYS_MAX as u32 + 44;
    for i in 0..keys {
        let (w, op) = put(i);
        p.send(w.clone(), op);
        let _ = p.take_ops();
        // Its deadline passes unanswered: LOST, remembered (N2); then nobody needs it.
        while p.ops.need(&w).is_some_and(|n| n.attempt() < 2) {
            next(&mut p);
        }
        p.on(&w, op_life::OpEvent::Withdraw);
    }
    let remembered = p.ops.untracked().count();
    println!(
        "{keys} lost PUT keys: {remembered} remembered, {} evicted",
        p.untracked_evicted()
    );
    assert!(
        remembered <= UNTRACKED_KEYS_MAX,
        "N2 remembers {remembered} keys, over its cap {UNTRACKED_KEYS_MAX}"
    );
    assert_eq!(
        p.untracked_evicted(),
        u64::from(keys) - UNTRACKED_KEYS_MAX as u64,
        "the evictions past the cap were not counted"
    );
}

/// A background op the node NEVER answers holds the slot, and one queued behind it.
fn starving(p: &mut Page) -> (Waiting, Waiting) {
    let (hole, hole_op) = put(1_000_000);
    let (other, other_op) = put(1_000_001);
    p.test_background.insert(hole.clone());
    p.test_background.insert(other.clone());
    p.send(hole.clone(), hole_op);
    p.send(other.clone(), other_op);
    let _ = p.take_ops();
    assert!(
        matches!(p.ops.need(&other), Some(op_life::Need::Queued { .. })),
        "THE SETUP: the second background op is not queued"
    );
    (hole, other)
}

/// **L5 by construction, the yield:** a background op that times out YIELDS the slot (round-robin), so the op queued
/// behind a never-answering one reaches the wire within its bound. The starvation mutant "re-sent at once into its
/// own slot" fails here.
#[test]
fn a_never_answered_background_op_does_not_starve_the_queue() {
    let mut p = page();
    let (_, other) = starving(&mut p);
    let start = p.now().0;
    for _ in 0..200 {
        next(&mut p);
        if p.ops.on_wire(&other) {
            let waited = p.now().0 - start;
            println!("the queued op reached the wire after {waited} ms");
            return;
        }
    }
    panic!("the op behind a never-answered background op never reached the wire (starved)");
}

/// **L5 by construction, reconnects:** a reconnect turns the slot's send HELD and queues its successor at the BACK
/// (no priority, the gate's seed 8), so repeated reconnects over a never-answering op do not starve the queue. The
/// mutant "a reconnect's successor goes first" fails here.
#[test]
fn repeated_reconnects_over_a_never_answered_background_op_do_not_starve_the_queue() {
    let mut p = page();
    let (_, other) = starving(&mut p);
    for _ in 0..200 {
        let now = p.now();
        p.reconnected(now);
        let _ = p.take_ops();
        next(&mut p);
        if p.ops.on_wire(&other) {
            return;
        }
    }
    panic!("the op behind a never-answered background op never reached the wire across reconnects (starved)");
}
