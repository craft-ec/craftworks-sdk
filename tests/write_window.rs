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
use std::collections::VecDeque;

/// The node's fair-queue capacity per key: 100 waiting + 1 in service.
const NODE_ADMITS: usize = 101;

/// The window, for a test to loop over — or a FAILURE BY NAME when there is
/// no window worth the name. A test that loops `0..WRITES_IN_FLIGHT` under a
/// window-removed mutant (`usize::MAX`) makes four billion writes and hangs
/// instead of failing (it hung for 8+ minutes in review).
fn window() -> usize {
    // An `if`, not an `assert!`: the value is a constant today and a mutant
    // tomorrow, and this line is for the mutant.
    if WRITES_IN_FLIGHT >= NODE_ADMITS {
        panic!("the window ({WRITES_IN_FLIGHT}) is not narrower than the node's queue ({NODE_ADMITS}): it bounds nothing");
    }
    WRITES_IN_FLIGHT
}

/// Drive `step` until it reports nothing left, or FAIL BY NAME at `cap`
/// steps — never spin.
fn drive(what: &str, cap: usize, mut step: impl FnMut() -> bool) {
    for _ in 0..cap {
        if !step() {
            return;
        }
    }
    panic!("{what}: still going after {cap} steps");
}

struct QueueingNode {
    conn: testkit::PageConn,
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
        for reply in self.conn.frame(&f) {
            store.on_inbound(&reply);
        }
        true
    }
}

/// `n` writes made back-to-back, as the handoff makes them, then the node
/// serves until nothing is left.
fn publish(n: u32) -> (CachedStore, QueueingNode) {
    let node = testkit::PageNode::new();
    let conn = node.connect();
    let (mut s, _clock) = testkit::cached_store();
    let mut q = QueueingNode { conn, queue: VecDeque::new(), refused: 0, deepest: 0 };
    s.client.send(&protocol::Request::Identity);
    q.accept(s.take_outbound());
    drive("the node answering Identity", 100, || q.serve_one(&mut s));
    // Past the copy's cap a write is refused; the refusal is RETURNED, and it
    // is the same count the store reports (sdk#186).
    let mut returned = 0;
    for i in 0..n {
        if s.put(format!("d/notes/{i:08}").as_bytes(), &vec![b'x'; 5 * 1024]).is_err() {
            returned += 1;
        }
        q.accept(s.take_outbound());
    }
    assert_eq!(returned, s.refused.len(), "a refusal was reported but not returned");
    // The node works through its queue; each answer may open the window.
    drive("the node serving the publish", 100_000, || {
        let served = q.serve_one(&mut s);
        q.accept(s.take_outbound());
        served
    });
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
    assert_eq!(s.in_flight_high_water, window(), "the window never filled, so it bounded nothing");
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
    let node = testkit::PageNode::new();
    let conn = node.connect();
    let (mut s, clock) = testkit::cached_store();
    let mut q = QueueingNode { conn, queue: VecDeque::new(), refused: 0, deepest: 0 };
    s.client.send(&protocol::Request::Identity);
    q.accept(s.take_outbound());
    drive("the node answering Identity", 100, || q.serve_one(&mut s));
    let n = s.copy.max_pending;
    for i in 0..n as u32 {
        s.put(format!("d/notes/{i:08}").as_bytes(), &vec![b'x'; 5 * 1024]).expect("the store took the write");
        q.accept(s.take_outbound());
    }
    assert!(s.refused.is_empty(), "{:?}", s.refused);
    let (mut rolled_back, mut since_tick, mut elapsed) = (Vec::new(), 0u64, 0u64);
    drive("the slow node serving", 100_000, || {
        if !q.serve_one(&mut s) {
            return false;
        }
        clock.advance(SERVE_MS);
        elapsed += SERVE_MS;
        since_tick += SERVE_MS;
        if since_tick >= 1000 {
            since_tick = 0;
            rolled_back.extend(s.tick().rolled_back);
        }
        q.accept(s.take_outbound());
        true
    });
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
    let node = testkit::PageNode::new();
    let conn = node.connect();
    let (mut s, clock) = testkit::cached_store();
    let mut q = QueueingNode { conn, queue: VecDeque::new(), refused: 0, deepest: 0 };
    s.client.send(&protocol::Request::Identity);
    q.accept(s.take_outbound());
    drive("the node answering Identity", 100, || q.serve_one(&mut s));
    let first = s.next_write_id();
    for i in 0..=window() as u32 {
        s.put(format!("d/notes/{i:08}").as_bytes(), b"v").expect("the store took the write");
        q.accept(s.take_outbound());
    }
    let last = first + window() as u64;
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

/// A reply naming THIS store's session.
fn verdict(s: &CachedStore, write_id: u64, state: protocol::WriteState) -> Vec<u8> {
    protocol::encode_reply(&protocol::Reply::SessionWriteState {
        session: s.client.session().expect("a session"),
        write_id,
        state,
    })
    .expect("encodes")
}

/// **Sixteen sent writes whose verdicts never come** — the node refused them
/// with a host error naming no request, or never answered — are rolled back
/// at 60 s, and THEIR SLOTS GO WITH THEM. RED before (the architect's probe on
/// 908feb6): a separate set of sent ids was cleared only by a verdict, so
/// "then 5 more writes: 0 reached the wire; held 5; ten minutes later … still
/// held 5" — the outbox dead until reload.
#[test]
fn sixteen_lost_verdicts_free_the_window() {
    let (mut s, clock) = testkit::cached_store();
    for i in 0..window() as u32 {
        s.put(format!("lost/{i:02}").as_bytes(), b"x").expect("the store took the write");
    }
    assert_eq!(s.take_outbound().len(), WRITES_IN_FLIGHT);
    clock.advance(61_000);
    let told = s.tick();
    assert_eq!(told.rolled_back.len(), WRITES_IN_FLIGHT, "the sixteen are rolled back at 60 s");
    for i in 0..5u32 {
        s.put(format!("later/{i:02}").as_bytes(), b"y").expect("the store took the write");
    }
    assert_eq!(s.take_outbound().len(), 5, "the person's next writes never reached the wire");
    assert_eq!(s.held_count(), 0);
}

/// The same, with writes ALREADY HELD when the sixteen time out: the tick that
/// rolls them back sends the held ones. (Nothing else would: no verdict is
/// coming to open the window.)
#[test]
fn a_time_out_releases_what_the_window_holds() {
    let (mut s, clock) = testkit::cached_store();
    for i in 0..window() as u32 + 5 {
        s.put(format!("w/{i:02}").as_bytes(), b"x").expect("the store took the write");
    }
    assert_eq!(s.take_outbound().len(), WRITES_IN_FLIGHT);
    assert_eq!(s.held_count(), 5);
    clock.advance(61_000);
    let told = s.tick();
    assert_eq!(told.rolled_back.len(), WRITES_IN_FLIGHT);
    assert_eq!(s.take_outbound().len(), 5, "the five held writes were not released by the tick");
    assert_eq!(s.held_count(), 0);
}

/// **Every send restarts the timeout — the `Busy` re-send too.** The
/// architect's probe on 908feb6: a write answered `Busy` at 0 s and re-sent
/// at 55 s was rolled back at 61 s — 6 s after the re-send — and the node's
/// `Published` then arrived for a write the copy no longer had.
#[test]
fn a_busy_write_re_sent_is_timed_from_the_re_send() {
    let (mut s, clock) = testkit::cached_store();
    s.put(b"K", b"v").expect("the store took the write");
    let id = s.copy.pending_ids()[0];
    assert_eq!(s.take_outbound().len(), 1);
    let busy = verdict(&s, id, protocol::WriteState::Busy);
    s.on_inbound(&busy);
    clock.advance(55_000);
    let _ = s.tick();
    assert_eq!(s.take_outbound().len(), 1, "the queued write was not re-sent");
    clock.advance(6_000);
    let told = s.tick();
    assert!(told.rolled_back.is_empty(), "rolled back 6 s after its re-send: {:?}", told.rolled_back);
    assert_eq!(s.copy.pending_ids(), vec![id]);
    let published = verdict(&s, id, protocol::WriteState::Published);
    s.on_inbound(&published);
    assert!(s.copy.pending_ids().is_empty(), "the Published verdict did not clear it");
    assert_eq!(s.unknown_verdicts(), 0, "the verdict arrived for a write the copy no longer had");
}

/// **Any verdict frees the slot, not only `Published`.** Sixteen sent and one
/// held; the sixteen are answered `Accepted` only (a cold commit: accepted
/// long before it publishes) — or `Busy` only (the write stays pending,
/// queued to go again). Either way none of them is still WAITING at the node,
/// so the held write goes. A slot freed only when the write left the copy
/// would keep the window shut on sixteen writes the node had already answered.
#[test]
fn any_verdict_frees_the_slot_accepted_or_busy() {
    // Both arms run and both are reported: one failing must not hide the other.
    let mut shut = Vec::new();
    for state in [protocol::WriteState::Accepted, protocol::WriteState::Busy] {
        let (mut s, _clock) = testkit::cached_store();
        for i in 0..=window() as u32 {
            s.put(format!("v/{i:02}").as_bytes(), b"x").expect("the store took the write");
        }
        let sent: Vec<u64> = s.copy.pending_ids().into_iter().take(window()).collect();
        let held_id = *s.copy.pending_ids().last().expect("a held write");
        assert_eq!(s.take_outbound().len(), window());
        assert_eq!(s.held_count(), 1);
        for id in sent {
            let v = verdict(&s, id, state);
            s.on_inbound(&v);
        }
        let out: Vec<u64> = s
            .take_outbound()
            .iter()
            .filter_map(|f| match protocol::decode_request(f) {
                protocol::Incoming::Ok(env) => match env.body {
                    protocol::Request::Write { write_id, .. } | protocol::Request::Commit { write_id, .. } => Some(write_id),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        if !out.contains(&held_id) || s.held_count() != 0 {
            shut.push(format!("after sixteen {state:?} verdicts the held write {held_id} never reached the wire (sent: {out:?})"));
        }
    }
    assert!(shut.is_empty(), "{shut:#?}");
}
