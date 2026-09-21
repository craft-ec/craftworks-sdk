//! sdk#174, the client half: AT MOST ONE UNANSWERED TICK PER SESSION, and the
//! continuation of a parked write is its client asking after it.
//!
//! Driven against the node's QUEUE (`NodeQueue`): one delegate chain at a
//! time, 8 requests waiting, the 9th refused with no reply. A page ticks every
//! second; a delegate parked on the network for 30 s used to collect a tick a
//! second from every tab and the queue filled (sdk#173).
use craftworks_sdk::CachedStore;
use protocol::{Op, Request};
use testkit::full_node::{FullNode, NodeQueue, NODE_QUEUE};
use testkit::{cached_store_at, Clock};

const T0: u64 = 1_790_000_000_000;

struct Tab {
    store: CachedStore,
    clock: Clock,
}

fn tabs(n: usize) -> Vec<Tab> {
    (0..n)
        .map(|_| {
            let (store, clock) = cached_store_at(T0);
            Tab { store, clock }
        })
        .collect()
}

/// Hand every tab's outbound frames to the node, in tab order.
fn offer(q: &mut NodeQueue, tabs: &mut [Tab]) {
    for (i, t) in tabs.iter_mut().enumerate() {
        for f in t.store.take_outbound() {
            q.offer(i, f);
        }
    }
}

/// Serve what is queued and deliver each answer to the tab that asked.
fn serve(q: &mut NodeQueue, tabs: &mut [Tab]) {
    for (i, replies) in q.run() {
        for r in replies {
            tabs[i].store.on_inbound(&r);
        }
    }
}

fn ticks_of(reqs: &[(usize, Request)], tab: usize) -> Vec<u64> {
    reqs.iter()
        .filter_map(|(t, r)| match r {
            Request::Tick { now } if *t == tab => Some(*now),
            _ => None,
        })
        .collect()
}

/// Two tabs, a delegate parked for 30 s -- and for 90 s, the node's own
/// PARK_TTL (F50): each tab holds exactly ONE tick in the node's queue,
/// nothing is refused, and the first tick after carries the time as it is
/// then.
#[test]
fn two_tabs_on_a_delegate_parked_30_s_hold_one_tick_each() {
    parked_for(30);
}

#[test]
fn two_tabs_on_a_delegate_parked_90_s_hold_one_tick_each() {
    parked_for(90);
}

fn parked_for(secs: u64) {
    let node = FullNode::new();
    let mut q = NodeQueue::new(node.connect());
    let mut tabs = tabs(2);
    q.park(true);
    for s in 0..=secs {
        for t in tabs.iter_mut() {
            t.clock.advance(if s == 0 { 0 } else { 1000 });
            let now = t.clock.now_ms();
            t.store.send_tick(now);
        }
        offer(&mut q, &mut tabs);
    }
    let queued = q.queued();
    println!(
        "  parked {secs} s: queued {:?}, refused {}, tab refusals {:?}",
        queued.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        q.rejected,
        tabs.iter()
            .map(|t| t.store.client.ticks.refused)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        (ticks_of(&queued, 0).len(), ticks_of(&queued, 1).len()),
        (1, 1),
        "not exactly one queued tick per tab"
    );
    assert_eq!(q.rejected, 0, "the node refused a frame");

    // The delegate is free: the queued ticks run (with the time they were
    // sent -- stale, and harmless: engine time is the max seen).
    q.park(false);
    serve(&mut q, &mut tabs);
    let before = q.ran.len();
    for t in tabs.iter_mut() {
        t.clock.advance(1000);
        let now = t.clock.now_ms();
        assert!(
            t.store.send_tick(now),
            "answered, and the next tick was refused"
        );
    }
    offer(&mut q, &mut tabs);
    serve(&mut q, &mut tabs);
    let after: Vec<(usize, Request)> = q.ran[before..].to_vec();
    for tab in 0..2 {
        assert_eq!(
            ticks_of(&after, tab),
            vec![protocol::tick_of(T0 + (secs + 1) * 1000)],
            "tab {tab}'s first tick after the wait did not carry the real time"
        );
    }
}

/// CONTROL: the same 30 s with every tick sent, as a page did before the
/// gate. The queue fills and the node refuses frames -- so the test above can
/// fail, and what holds it green is the gate.
#[test]
fn control_ungated_ticks_fill_the_node_s_queue() {
    let node = FullNode::new();
    let mut q = NodeQueue::new(node.connect());
    let mut tabs = tabs(2);
    q.park(true);
    for s in 0..=30u64 {
        for t in tabs.iter_mut() {
            t.clock.advance(if s == 0 { 0 } else { 1000 });
            t.store.client.send(&Request::Tick {
                now: protocol::tick_of(t.clock.now_ms()),
            });
        }
        offer(&mut q, &mut tabs);
    }
    assert_eq!(q.max_depth, NODE_QUEUE);
    assert_eq!(
        q.rejected,
        2 * 31 - NODE_QUEUE,
        "the queue model did not refuse the 9th"
    );
}

/// ONE LOST TICK REPLY MUST NOT STOP THE CLOCK. The first tick's frame never
/// reaches the delegate (refused by a full queue: no call, no report); after
/// that the node answers everything at once. Ticking every second for 160 s:
/// refused until the lost one is forgotten at FORGET_MS (100 s), then one a
/// second, each answered before the next -- 61 ticks run, pinned because both
/// numbers are the mechanism's (FORGET_MS; a 1 s cadence answered within it).
/// Without the forgetting: 0. Without the write-off, or without counting
/// answers: one per FORGET_MS, 1.
#[test]
fn a_lost_tick_reply_is_forgotten_and_the_clock_keeps_going() {
    use craftworks_sdk::tick_gate::FORGET_MS;
    let node = FullNode::new();
    let mut q = NodeQueue::new(node.connect());
    let mut tabs = tabs(1);
    assert!(tabs[0].store.send_tick(T0));
    let lost = tabs[0].store.take_outbound();
    assert_eq!(lost.len(), 1, "the tick was not sent");
    for _ in 1..=160u64 {
        tabs[0].clock.advance(1000);
        let now = tabs[0].clock.now_ms();
        tabs[0].store.send_tick(now);
        offer(&mut q, &mut tabs);
        serve(&mut q, &mut tabs);
    }
    let ran = ticks_of(&q.ran, 0);
    println!(
        "  ticks run {} (forgotten {}, refused {})",
        ran.len(),
        tabs[0].store.client.ticks.forgotten,
        tabs[0].store.client.ticks.refused
    );
    assert_eq!(
        ran.first(),
        Some(&protocol::tick_of(T0 + FORGET_MS)),
        "not resumed at FORGET_MS"
    );
    assert_eq!(ran.len(), 61, "after resuming, not one tick a second");
    assert_eq!(tabs[0].store.client.ticks.forgotten, 1);
}

fn value(i: u64, size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

/// A node that holds 300 rows, and a COLD node over it: a write there needs
/// more blocks than one request may fetch, so the engine parks it.
fn cold_node() -> FullNode {
    let a = FullNode::new();
    let mut w = a.connect();
    w.client(&Request::Identity);
    for (k, chunk) in (0..300u64).collect::<Vec<_>>().chunks(50).enumerate() {
        let ops = chunk
            .iter()
            .map(|i| Op::Put(format!("t/{i:06}").into_bytes(), value(*i, 1024)))
            .collect();
        w.client(&Request::Write {
            write_id: 1000 + k as u64,
            ops,
        });
    }
    FullNode::cold_over(&a)
}

/// Serve, and collect the write states this tab was told.
fn serve_told(q: &mut NodeQueue, tabs: &mut [Tab], told: &mut Vec<protocol::WriteState>) {
    for (i, replies) in q.run() {
        for r in replies {
            if let Ok(reply) = protocol::decode_reply(&r) {
                if let Some((_, st)) = tabs[i].store.client.own_write_state(&reply) {
                    told.push(st);
                }
            }
            tabs[i].store.on_inbound(&r);
        }
    }
}

/// Drive one tab against a cold node for `secs` seconds -- ticking, and
/// asking after unheard writes when `ask` -- and return the second the write
/// was PUBLISHED, how many AskWrites went, and every state it was told.
fn cold_write(ask: bool, secs: u64) -> (Option<u64>, usize, Vec<protocol::WriteState>) {
    let mut q = NodeQueue::new(cold_node().connect());
    let mut tabs = tabs(1);
    tabs[0].store.client.send(&Request::Identity);
    tabs[0].store.send_tick(T0);
    offer(&mut q, &mut tabs);
    serve(&mut q, &mut tabs);
    let edits: Vec<(Vec<u8>, craftworks_sdk::store::Edit)> = (0..20u64)
        .map(|i| {
            (
                format!("t/{:06}", i * 15).into_bytes(),
                craftworks_sdk::store::Edit::Put(value(20_000 + i, 1024)),
            )
        })
        .collect();
    use craftworks_sdk::store::Store;
    tabs[0]
        .store
        .apply_batch(&edits)
        .expect("the copy took the write");
    let mut told = Vec::new();
    offer(&mut q, &mut tabs);
    serve_told(&mut q, &mut tabs, &mut told);
    let mut published = None;
    for s in 1..=secs {
        tabs[0].clock.advance(1000);
        let now = tabs[0].clock.now_ms();
        tabs[0].store.send_tick(now);
        if ask {
            tabs[0].store.ask_unheard(now);
        }
        offer(&mut q, &mut tabs);
        serve_told(&mut q, &mut tabs, &mut told);
        if told.contains(&protocol::WriteState::Published) {
            published = Some(s);
            break;
        }
    }
    let asks = q
        .ran
        .iter()
        .filter(|(_, r)| matches!(r, Request::AskWrite { .. }))
        .count();
    (published, asks, told)
}

/// A cold write the engine parks is continued by the page asking after it,
/// with no one else doing anything, and is published.
#[test]
fn a_parked_write_is_continued_by_the_page_asking_after_it() {
    let (published, asks, told) = cold_write(true, 30);
    println!("  with asks: published at {published:?} s after {asks} AskWrite(s); told {told:?}");
    assert!(
        published.is_some(),
        "the write was never published: {told:?}"
    );
    assert!(
        asks >= 1,
        "cleared without an ask: the write never parked, the test is empty"
    );
    assert!(
        asks <= 3,
        "{asks} asks for one write: more than one per silence"
    );
}

/// CONTROL: the same write with no asking stays parked and is released
/// Failed by the engine -- so what published it above was the asking.
#[test]
fn control_a_parked_write_nobody_asks_after_is_not_published() {
    let (published, asks, told) = cold_write(false, 30);
    println!("  without asks: published at {published:?}, {asks} asks; told {told:?}");
    assert_eq!(asks, 0);
    assert_eq!(
        published, None,
        "published with nobody asking: the asks proved nothing"
    );
}

/// A write the engine refused `Busy` -- another tab's commit is in flight --
/// is back in THIS page's outbox, not at the engine, and must never be asked
/// after: the engine does not know it and would answer `Lost`, rolling back a
/// write that was only waiting its turn.
#[test]
fn a_busy_write_is_not_asked_after() {
    let node = FullNode::new();
    let conn = node.connect();
    conn.hold_answers();
    let mut q = NodeQueue::new(conn);
    let mut tabs = tabs(2);
    let mut told = Vec::new();
    use craftworks_sdk::store::{Edit, Store};
    // Tab 1's commit goes in flight and STAYS (the node's answers are held).
    tabs[1]
        .store
        .apply_batch(&[(b"b/1".to_vec(), Edit::Put(b"x".to_vec()))])
        .expect("taken");
    offer(&mut q, &mut tabs);
    serve(&mut q, &mut tabs);
    // Tab 0's write meets it: Busy.
    tabs[0]
        .store
        .apply_batch(&[(b"a/1".to_vec(), Edit::Put(b"y".to_vec()))])
        .expect("taken");
    offer(&mut q, &mut tabs);
    serve_told(&mut q, &mut tabs, &mut told);
    assert!(
        told.contains(&protocol::WriteState::Busy),
        "tab 0's write was not refused Busy: the test is empty ({told:?})"
    );
    // A page's tick, for 5 s: the time, the asking, the store's own tick.
    for _ in 0..5 {
        for t in tabs.iter_mut() {
            t.clock.advance(1000);
            let now = t.clock.now_ms();
            t.store.send_tick(now);
            t.store.ask_unheard(now);
            t.store.tick();
        }
        offer(&mut q, &mut tabs);
        serve_told(&mut q, &mut tabs, &mut told);
    }
    println!(
        "  ran {:?}\n  told {told:?}",
        q.ran
            .iter()
            .map(|(t, r)| format!(
                "{t}:{}",
                match r {
                    Request::Write { write_id, .. } => format!("W{write_id}"),
                    Request::AskWrite { write_id } => format!("A{write_id}"),
                    Request::Tick { .. } => "T".into(),
                    _ => "?".into(),
                }
            ))
            .collect::<Vec<_>>()
    );
    let asked_by_0 = q
        .ran
        .iter()
        .filter(|(t, r)| *t == 0 && matches!(r, Request::AskWrite { .. }))
        .count();
    assert_eq!(asked_by_0, 0, "a Busy write was asked after");
    assert!(
        !told.contains(&protocol::WriteState::Lost),
        "tab 0's waiting write was answered Lost: {told:?}"
    );
}
