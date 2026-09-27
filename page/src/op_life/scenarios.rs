//! OP-LIFE.md's scenarios that a random sequence reaches too rarely to be its only witness, each driven by
//! construction: N2's bounded memory, and L5 under a background op the node never answers.
use crate::*;

/// A fresh page past its head recovery, nothing on the wire: the OP-LIFE tests' one fixture.
pub(super) fn page() -> Page {
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

/// Is a send of `w`'s key in flight, asked by it?
fn on_wire(p: &Page, w: &Waiting) -> bool {
    p.ops.at_node().any(|(by, ..)| by == w)
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
        matches!(p.ops.need(&other), Some(op_life::Entry::Queued { .. })),
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
        if on_wire(&p, &other) {
            let waited = p.now().0 - start;
            println!("the queued op reached the wire after {waited} ms");
            return;
        }
    }
    panic!("the op behind a never-answered background op never reached the wire (starved)");
}

/// **L5 by construction, reconnects:** a reconnect frees the key and leaves the slot OWED to the orphaned send until its
/// bound (one Option, the architect's Q4), re-queueing the key at the BACK (round-robin), so repeated reconnects over a
/// never-answering op do not starve the queue.
#[test]
fn repeated_reconnects_over_a_never_answered_background_op_do_not_starve_the_queue() {
    let mut p = page();
    let (_, other) = starving(&mut p);
    for _ in 0..200 {
        let now = p.now();
        p.reconnected(now);
        let _ = p.take_ops();
        next(&mut p);
        if on_wire(&p, &other) {
            return;
        }
    }
    panic!("the op behind a never-answered background op never reached the wire across reconnects (starved)");
}

/// **An overlap's answer is COUNTED (F1, one counter):** a background PUT silent to its bound is sent again, the same
/// payload, marked overlap; its answer may be either send's, so it lands in `Ops::late` under (Duplicates,
/// Background, served) -- exactly once. The mutant "an overlap's answer is not counted" fails here.
#[test]
fn a_background_overlaps_answer_is_counted_once_as_served_late() {
    let mut p = page();
    let (w, op) = put(7);
    p.test_background.insert(w.clone());
    p.send(w.clone(), op.clone());
    assert_eq!(p.take_ops(), vec![op.clone()], "THE SETUP: the background PUT did not go out");
    let mut resent = false;
    for _ in 0..50 {
        let t = p.next_due().expect("the silent PUT is due").0;
        p.tick(Ms(t));
        if p.take_ops().contains(&op) {
            resent = true;
            break;
        }
    }
    assert!(resent, "THE SETUP: the silent background PUT was never sent again at its bound");
    assert!(p.ops.at_node().any(|(by, s, _)| *by == w && s.overlap), "THE SETUP: the re-send is not marked overlap");
    let before = p.ops.late(op_life::Resend::Duplicates, Lane::Background, true);
    let Op::Put { id, .. } = op else { unreachable!() };
    let now = p.now();
    p.answer(Answer::PutOk(id), now);
    assert_eq!(p.ops.late(op_life::Resend::Duplicates, Lane::Background, true) - before, 1, "the overlap's answer was not counted as served late");
    assert!(!p.ops.contains(&w), "THE CONTROL: the overlap's answer did not end the PUT");
}

/// **F1, next waits for an ANSWER (main's standing requirement for #512):** a kind whose answer carries no request id
/// (a site's UPDATE) never sends a different payload at its bound -- the bound proves nothing is over, and the old
/// send's answer could not be told from the new one's. At the bound the SAME payload goes again (overlap); the waiting
/// payload goes on the answer. The mutant "next sent on the bound, any kind" fails here.
#[test]
fn a_site_updates_next_payload_waits_for_an_answer_not_the_bound() {
    let mut p = page();
    let site = Label::Site("app".into());
    let w = Waiting::Update(site.clone());
    let (a, b) = (Op::Update { label: site.clone(), state: vec![1] }, Op::Update { label: site.clone(), state: vec![2] });
    p.send(w.clone(), a.clone());
    assert_eq!(p.take_ops(), vec![a.clone()], "THE SETUP: the first payload did not go out");
    p.send(w.clone(), b.clone());
    assert!(p.take_ops().is_empty(), "THE SETUP: a second payload went out beside the first (L0)");
    let mut at_bound = Vec::new();
    for _ in 0..50 {
        let t = p.next_due().expect("the silent UPDATE is due").0;
        p.tick(Ms(t));
        at_bound = p.take_ops();
        if !at_bound.is_empty() {
            break;
        }
    }
    assert_eq!(at_bound, vec![a.clone()], "at its bound the site UPDATE did not re-send the SAME payload (F1: next waits for an answer)");
    assert!(p.ops.at_node().any(|(by, s, _)| *by == w && s.overlap && s.op == a), "THE SETUP: the re-send is not the overlap");
    p.answered(&w);
    assert_eq!(p.take_ops(), vec![b], "THE CONTROL: the waiting payload did not go on the answer");
}
