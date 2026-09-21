//! Writes are paced by a WINDOW of requests outstanding at the node, not sent
//! back-to-back (craftworks-sdk#176).
//!
//! The node's fair queue admits 100 waiting + 1 in service, shared by every
//! delegate request on it; the 102nd is refused with a host error and NO
//! verdict (the architect's L4 probe, read at freenet-core v0.2.135). The
//! outbox sent a 100-row publish's 102 writes at once, and the last one was
//! never answered, every time. `Busy` never paces anything on a real node: it
//! runs one delegate round-trip at a time, so a pipelined write waits in the
//! NODE's queue and meets an idle engine.
//!
//! The fake node here is that queue, in front of a real engine: requests are
//! admitted up to the node's capacity and served one per call; past it they
//! are REFUSED — no verdict, as the node does.

use craftworks_sdk::cached_store::WRITES_IN_FLIGHT;
use craftworks_sdk::{CachedStore, Store as _};
use engine_delegate::shell::Inbound;
use std::collections::VecDeque;

/// The node's fair-queue capacity per key: 100 waiting + 1 in service.
const NODE_ADMITS: usize = 101;

struct QueueingNode {
    conn: testkit::Conn,
    queue: VecDeque<Vec<u8>>,
    /// Requests refused because the queue was full: a host error, no verdict.
    refused: usize,
    /// The deepest the queue ever got.
    deepest: usize,
}

impl QueueingNode {
    fn accept(&mut self, frames: Vec<Vec<u8>>) {
        for f in frames {
            if self.queue.len() >= NODE_ADMITS {
                self.refused += 1;
            } else {
                self.queue.push_back(f);
                self.deepest = self.deepest.max(self.queue.len());
            }
        }
    }
    /// Serve ONE request, as the node runs one delegate round-trip at a time.
    fn serve_one(&mut self, store: &mut CachedStore) -> bool {
        let Some(f) = self.queue.pop_front() else { return false };
        for reply in self.conn.step(vec![Inbound::Client(f)]) {
            store.on_inbound(&reply);
        }
        true
    }
}

/// `n` writes made back-to-back, as the handoff makes them, then the node
/// serves until nothing is left.
fn publish(n: u32) -> (CachedStore, QueueingNode) {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let (mut s, _clock) = testkit::cached_store();
    let mut q = QueueingNode { conn, queue: VecDeque::new(), refused: 0, deepest: 0 };
    s.client.send(&protocol::Request::Identity);
    q.accept(s.take_outbound());
    while q.serve_one(&mut s) {}
    for i in 0..n {
        s.put(format!("d/notes/{i:08}").as_bytes(), &vec![b'x'; 5 * 1024]);
        q.accept(s.take_outbound());
    }
    // The node works through its queue; each answer may open the window.
    for _ in 0..100_000 {
        if !q.serve_one(&mut s) {
            break;
        }
        q.accept(s.take_outbound());
    }
    (s, q)
}

/// **102 writes pipelined: every one published, never more than the window
/// outstanding.** RED before: all 102 went out at once, the node refused one,
/// and it was never answered.
#[test]
fn a_hundred_and_two_writes_all_publish_and_never_overfill_the_node() {
    let (s, q) = publish(102);
    println!(
        "  102 writes: high-water {} (window {WRITES_IN_FLIGHT}), node queue deepest {}, refused {}, still pending {}",
        s.in_flight_high_water,
        q.deepest,
        q.refused,
        s.copy.pending_ids().len()
    );
    assert_eq!(q.refused, 0, "the node refused a request: its queue was overfilled");
    assert!(s.copy.pending_ids().is_empty(), "writes never answered: {:?}", s.copy.pending_ids());
    assert!(s.refused.is_empty(), "{:?}", s.refused);
    assert!(s.in_flight_high_water <= WRITES_IN_FLIGHT, "{} outstanding at once", s.in_flight_high_water);
    assert_eq!(s.held_count(), 0);
}

/// THE CONTROL: the window is actually USED — a burst reaches it, so the
/// assertion above is not met trivially by a store that sends one at a time.
#[test]
fn control_a_burst_fills_the_window() {
    let (s, _q) = publish(40);
    assert_eq!(s.in_flight_high_water, WRITES_IN_FLIGHT, "the window never filled, so it bounded nothing");
}

/// Past the copy's own bound (`max_pending`, 256) a write is refused BY NAME
/// before anything is sent — it is never lost. Every write asserted here is
/// either published or refused as `TooManyPending`, and the count is exact.
fn every_write_published_or_refused_by_the_cap(s: &CachedStore, n: usize) {
    let cap = s.copy.max_pending;
    assert_eq!(s.refused.len(), n.saturating_sub(cap), "refused: {:?}", &s.refused[..s.refused.len().min(3)]);
    for (id, why) in &s.refused {
        assert!(
            matches!(why, craftworks_sdk::copy::Refused::TooManyPending { .. }),
            "write {id} refused for another reason: {why:?}"
        );
    }
}

/// 300 rows, the live probe's other size: the 256 the copy takes all publish.
#[test]
fn three_hundred_writes_every_one_the_copy_takes_publishes() {
    let (s, q) = publish(300);
    assert_eq!(q.refused, 0);
    assert!(s.copy.pending_ids().is_empty());
    every_write_published_or_refused_by_the_cap(&s, 300);
}

/// A write the window HOLDS has not been sent, so it is not timed out: the
/// copy's timeout is for "no verdict ever came", and the node was never
/// asked. Against a node that takes 300 ms a write — the live node's pace —
/// 256 writes (the copy's bound) take 77 s, past its 60 s. RED before: every write not
/// published within 60 s of its `put` was rolled back `Unknown`, including
/// those still held (live: 192–198 of 302 published, the rest rolled back).
#[test]
fn a_slow_node_publishes_every_write_and_none_is_rolled_back_while_held() {
    const SERVE_MS: u64 = 300;
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let (mut s, clock) = testkit::cached_store();
    let mut q = QueueingNode { conn, queue: VecDeque::new(), refused: 0, deepest: 0 };
    s.client.send(&protocol::Request::Identity);
    q.accept(s.take_outbound());
    while q.serve_one(&mut s) {}
    let n = s.copy.max_pending;
    for i in 0..n as u32 {
        s.put(format!("d/notes/{i:08}").as_bytes(), &vec![b'x'; 5 * 1024]);
        q.accept(s.take_outbound());
    }
    assert!(s.refused.is_empty(), "{:?}", s.refused);
    let (mut rolled_back, mut since_tick, mut elapsed) = (Vec::new(), 0u64, 0u64);
    for _ in 0..100_000 {
        if !q.serve_one(&mut s) {
            break;
        }
        clock.advance(SERVE_MS);
        elapsed += SERVE_MS;
        since_tick += SERVE_MS;
        if since_tick >= 1000 {
            since_tick = 0;
            rolled_back.extend(s.tick().rolled_back);
        }
        q.accept(s.take_outbound());
    }
    println!(
        "  {n} writes at {SERVE_MS} ms each: {} s elapsed, rolled back {}, still pending {}, refused by the node {}",
        elapsed / 1000,
        rolled_back.len(),
        s.copy.pending_ids().len(),
        q.refused
    );
    assert!(elapsed > s.copy.pending_timeout_ms, "the run never outlasted the timeout, so it tested nothing");
    assert!(rolled_back.is_empty(), "rolled back {} writes: {:?}", rolled_back.len(), &rolled_back[..rolled_back.len().min(5)]);
    assert!(s.copy.pending_ids().is_empty(), "never answered: {:?}", s.copy.pending_ids());
    assert_eq!(q.refused, 0);
}

/// THE CONTROL: a write that was SENT and never answered still times out —
/// 60 s from when it was sent, not from when it was made. Write 17 is held
/// until write 1 is answered at 50 s; the other fifteen, sent at 0 and never
/// answered, go at 60 s; write 17 goes at 110 s and not before.
#[test]
fn control_a_sent_write_with_no_verdict_times_out_from_its_send() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let (mut s, clock) = testkit::cached_store();
    let mut q = QueueingNode { conn, queue: VecDeque::new(), refused: 0, deepest: 0 };
    s.client.send(&protocol::Request::Identity);
    q.accept(s.take_outbound());
    while q.serve_one(&mut s) {}
    let first = s.next_write_id();
    for i in 0..=WRITES_IN_FLIGHT as u32 {
        s.put(format!("d/notes/{i:08}").as_bytes(), b"v");
        q.accept(s.take_outbound());
    }
    let last = first + WRITES_IN_FLIGHT as u64;
    assert_eq!(s.held_count(), 1, "write {last} should be held");
    // At 50 s the node answers write 1 only; the window sends write 17.
    clock.advance(50_000);
    assert!(q.serve_one(&mut s));
    let _ = s.take_outbound(); // the node takes nothing more: every other request goes unanswered
    assert_eq!(s.held_count(), 0);
    clock.advance(9_999);
    assert!(s.tick().rolled_back.is_empty(), "rolled back before 60 s");
    clock.advance(1);
    let at_60: Vec<u64> = s.tick().rolled_back.iter().map(|(id, _)| *id).collect();
    assert_eq!(at_60.len(), WRITES_IN_FLIGHT - 1, "the fifteen sent at 0 go at 60 s: {at_60:?}");
    assert!(!at_60.contains(&last), "write {last} was rolled back 10 s after it was sent");
    clock.advance(49_999);
    assert!(s.tick().rolled_back.is_empty(), "write {last} went before 60 s from its send");
    clock.advance(1);
    let at_110: Vec<u64> = s.tick().rolled_back.iter().map(|(id, _)| *id).collect();
    assert_eq!(at_110, vec![last], "a sent write with no verdict must still time out");
}
