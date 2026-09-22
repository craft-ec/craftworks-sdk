//! COLD READS IN THE PAGE against a scripted client API (craftworks_sdk::cold).
//!
//! NOTHING IS FIXED (the owner): the timeout is RFC 6298's RTO over completed
//! fetches (with Karn's rule), the concurrency a congestion window — TCP's
//! answers. EVERYTHING IS PER FETCH: each GET has its own clock, and a timeout
//! re-sends only that GET.
//!
//! * a fast node: the window opens past 32 and the RTO falls below 200 ms;
//! * a node answering IN TURN at 100 ms a GET: no false timeouts in a
//!   128-fetch read — its queue shows as rising samples and a wider RTO;
//! * one GET stalled the way F52 stalls (first GET answered after 60 s):
//!   exactly ONE re-fetch, within about one RTO, every other block sent once;
//! * KARN: the late answer of a re-sent fetch is never a sample;
//! * a node that never answers: "not answering" when a block's OWN deadline
//!   runs out, never "missing".

use craftworks_sdk::cold::{ColdEvent, ColdReads, COLD_FETCH_DEADLINE_MS, COLD_RTO_INITIAL_MS};
use craftworks_sdk::{Store, TreeStore};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

/// An unstalled cold GET, one at a time: well under 1 s.
const FETCH_MS: u64 = 300;
/// A fast node: loopback.
const FAST_MS: u64 = 20;
/// F52: a lost streamed response is healed ≈ 60 s after it was sent.
const STALL_MS: u64 = 60_000;
/// A node answering GETs in turn.
const IN_TURN_MS: u64 = 100;
/// A block fetched from a peer, cold: the first live run's ≈ 1.5 s.
const SLOW_MS: u64 = 1_500;
const STEP_MS: u64 = 5;
const ROWS: u32 = 700;
const VALUE_BYTES: usize = 4_000;

/// Two ranges of `ROWS` rows each, values big enough to be their own blocks —
/// so one range is a batch of 70+ GETs (the owner's test).
fn tree() -> (Cid, BTreeMap<Cid, Vec<u8>>) {
    let mut t = TreeStore::new();
    for p in ["a", "b"] {
        for i in 0..ROWS {
            let mut v = format!("{p}/{i:04}:").into_bytes();
            v.resize(VALUE_BYTES, p.as_bytes()[0]);
            t.put(format!("{p}/{i:04}").as_bytes(), &v);
        }
    }
    let root = t.root();
    // The network holds each block as its contract state: `kind ‖ body`.
    let net = t
        .blocks()
        .map(|(cid, body)| {
            let kind = [freenet_prolly::kind::TREE_NODE, freenet_prolly::kind::RAW, freenet_prolly::kind::PARITY]
                .into_iter()
                .find(|k| freenet_prolly::block_id(*k, body) == *cid)
                .expect("every block hashes under one kind");
            let mut state = vec![kind];
            state.extend_from_slice(body);
            (*cid, state)
        })
        .collect();
    (root, net)
}

/// How the scripted node answers.
#[derive(Clone, Copy)]
enum Node {
    /// Every GET after `ms`, except the FIRST GET of `stalled`, after `STALL_MS`.
    Stalls { ms: u64, stalled: Option<Cid> },
    /// One GET at a time, `IN_TURN_MS` each, in the order sent.
    InTurn,
    /// Every GET after `FAST_MS`, except `late`: its FIRST GET answered after
    /// `late_ms`, and every re-send of it never.
    Late { late: Cid, late_ms: u64 },
    /// The first `fast` GETs after `FAST_MS` (blocks the node holds nearby),
    /// every later one after `SLOW_MS` (fetched from a peer) — the first live
    /// run's shape.
    TwoSpeed { fast: usize },
    /// Never.
    Silent,
    /// Every GET after `FAST_MS`, but `refused` is REFUSED after `FAST_MS`,
    /// every time — a block the node will not serve (not the root).
    Refuses { refused: Cid },
}

struct Run {
    finished: BTreeMap<u64, u64>,
    cold: ColdReads,
    asked: Vec<Cid>,
    most_window: f64,
    timed_out: usize,
    /// (window just before, just after) every tick in which a fetch timed out.
    halvings: Vec<(f64, f64)>,
}

fn run(root: Cid, net: &BTreeMap<Cid, Vec<u8>>, loads: &[(u64, &str)], node: Node, until_ms: u64) -> Run {
    let mut c = ColdReads::switched_on();
    c.set_root(root);
    let mut due: Vec<(u64, Cid)> = Vec::new();
    let mut refusals: Vec<(u64, Cid)> = Vec::new();
    let mut node_free = 0u64;
    let mut asked = Vec::new();
    let mut finished = BTreeMap::new();
    let (mut most_window, mut now) = (0f64, 0);
    let mut halvings = Vec::new();
    for (id, prefix) in loads {
        let (lo, hi) = (format!("{prefix}/").into_bytes(), format!("{prefix}0").into_bytes());
        assert!(c.take(*id, &lo, &hi, now), "the cold reader refused a load");
    }
    while now <= until_ms && finished.len() < loads.len() {
        for g in c.take_gets() {
            asked.push(g.block);
            match node {
                Node::Stalls { ms, stalled } => {
                    let wait = if Some(g.block) == stalled && g.attempt == 1 { STALL_MS } else { ms };
                    due.push((now + wait, g.block));
                }
                Node::InTurn => {
                    node_free = node_free.max(now) + IN_TURN_MS;
                    due.push((node_free, g.block));
                }
                Node::Late { late, late_ms } => {
                    if g.block != late {
                        due.push((now + FAST_MS, g.block));
                    } else if g.attempt == 1 {
                        due.push((now + late_ms, g.block));
                    }
                }
                Node::TwoSpeed { fast } => {
                    let wait = if asked.len() <= fast { FAST_MS } else { SLOW_MS };
                    due.push((now + wait, g.block));
                }
                Node::Silent => {}
                Node::Refuses { refused } if g.block == refused => refusals.push((now + FAST_MS, g.block)),
                Node::Refuses { .. } => due.push((now + FAST_MS, g.block)),
            }
        }
        most_window = most_window.max(c.window());
        now += STEP_MS;
        let (ready, later): (Vec<_>, Vec<_>) = due.into_iter().partition(|(t, _)| *t <= now);
        due = later;
        for (_, b) in ready {
            c.arrived(b, &net[&b], now);
        }
        let (ready, later): (Vec<_>, Vec<_>) = refusals.into_iter().partition(|(t, _)| *t <= now);
        refusals = later;
        for (_, b) in ready {
            c.refused(b, now);
        }
        let (before, timeouts) = (c.window(), c.log.iter().filter(|e| matches!(e, ColdEvent::TimedOut { .. })).count());
        c.tick(now);
        if c.log.iter().filter(|e| matches!(e, ColdEvent::TimedOut { .. })).count() > timeouts {
            halvings.push((before, c.window()));
        }
        for (id, rows, _) in c.take_done() {
            assert_eq!(rows.len(), ROWS as usize, "load {id} finished with {} rows", rows.len());
            finished.insert(id, now);
        }
        if !c.take_not_answering().is_empty() {
            break;
        }
    }
    let timed_out = c.log.iter().filter(|e| matches!(e, ColdEvent::TimedOut { .. })).count();
    Run { finished, cold: c, asked, most_window, timed_out, halvings }
}

/// The blocks one range's walk GETs, unstalled.
fn blocks_of(root: Cid, net: &BTreeMap<Cid, Vec<u8>>, prefix: &str) -> BTreeSet<Cid> {
    run(root, net, &[(1, prefix)], Node::Stalls { ms: FETCH_MS, stalled: None }, 600_000).asked.into_iter().collect()
}

#[test]
fn on_a_fast_node_the_window_opens_and_the_rto_falls() {
    let (root, net) = tree();
    let r = run(root, &net, &[(1, "a")], Node::Stalls { ms: FAST_MS, stalled: None }, 600_000);
    assert!(r.finished.contains_key(&1));
    assert!(r.most_window > 32.0, "the window only reached {}", r.most_window);
    assert!(r.cold.rto_ms() < 200.0, "the RTO is {} ms on a {FAST_MS} ms node", r.cold.rto_ms());
    assert_eq!(r.timed_out, 0);
}

/// A node answering in turn: every answer waits behind those sent before it,
/// so a growing window shows as rising samples — a wider RTO — and no healthy
/// fetch, merely queued, is timed out.
#[test]
fn a_node_answering_in_turn_has_no_false_timeouts() {
    let (root, net) = tree();
    let r = run(root, &net, &[(1, "a")], Node::InTurn, 600_000);
    assert!(r.finished.contains_key(&1), "the read did not finish");
    assert!(r.asked.len() >= 128, "{} fetches: not the 128 the owner's test names", r.asked.len());
    assert_eq!(r.timed_out, 0, "{} healthy fetches, merely queued at the node, were timed out", r.timed_out);
}

#[test]
fn a_stalled_get_costs_exactly_one_re_fetch_within_about_one_rto() {
    let (root, net) = tree();
    let only_a: Vec<Cid> = blocks_of(root, &net, "a").difference(&blocks_of(root, &net, "b")).copied().collect();
    let stalled = *only_a.last().expect("range a needs a block range b does not");
    let base = run(root, &net, &[(1, "a")], Node::Stalls { ms: FETCH_MS, stalled: None }, 600_000).finished[&1];
    let r = run(root, &net, &[(1, "a"), (2, "b")], Node::Stalls { ms: FETCH_MS, stalled: Some(stalled) }, 600_000);
    let a = r.finished[&1];
    assert!(a < STALL_MS, "the read took the whole stall ({a} ms)");
    // The re-fetch went out on the stalled GET's OWN timeout — about one RTO.
    let after = r.cold.log.iter().find_map(|e| match e {
        ColdEvent::TimedOut { block, attempt: 1, after_ms } if *block == stalled => Some(*after_ms),
        _ => None,
    });
    assert!(after.is_some_and(|ms| ms as f64 <= COLD_RTO_INITIAL_MS + STEP_MS as f64), "the stalled GET timed out after {after:?} ms");
    assert!(r.cold.log.iter().any(|e| matches!(e, ColdEvent::ReGetAnswered { block, attempt: 2, .. } if *block == stalled)));
    // PER FETCH: exactly ONE re-fetch was sent — the stalled block's.
    let mut sends: BTreeMap<Cid, usize> = BTreeMap::new();
    for b in &r.asked {
        *sends.entry(*b).or_default() += 1;
    }
    assert!(r.asked.len() >= 70, "{} GETs: not the batch the owner's test names", r.asked.len());
    assert_eq!(sends[&stalled], 2, "the stalled block was sent {} times", sends[&stalled]);
    let others: Vec<_> = sends.iter().filter(|(b, n)| **b != stalled && **n != 1).collect();
    assert!(others.is_empty(), "blocks other than the stalled one were re-sent: {others:?}");
    println!("unstalled {base} ms; with one GET stalled 60 s: read a {a} ms, read b {} ms", r.finished[&2]);
}

/// A TIMEOUT HALVES THE WINDOW (≥ 1): a node that stops answering is not
/// sent more.
#[test]
fn a_timeout_halves_the_window() {
    let (root, net) = tree();
    let blocks: Vec<Cid> = blocks_of(root, &net, "a").into_iter().collect();
    let stalled = blocks[blocks.len() - 1];
    let r = run(root, &net, &[(1, "a")], Node::Stalls { ms: FAST_MS, stalled: Some(stalled) }, 600_000);
    assert!(!r.halvings.is_empty(), "no fetch timed out");
    for (before, after) in &r.halvings {
        assert!((*after - (before / 2.0).max(1.0)).abs() < 1e-9, "a timeout took the window from {before} to {after}");
    }
}

/// THE FIRST LIVE RUN'S SHAPE: a few fast answers teach the RTO its 100 ms
/// floor, then every fetch takes 1.5 s. Without §5.5's back-off every slow
/// fetch is re-sent — so, by Karn, never sampled — and the RTO never learns
/// (live: 142 timeouts, 72 blocks answered only on a re-send). With it, the RTO
/// climbs past the round trip, a first GET answers, and SRTT learns 1.5 s.
#[test]
fn after_a_few_fast_answers_the_rto_learns_a_slow_round_trip() {
    let (root, net) = tree();
    let r = run(root, &net, &[(1, "a")], Node::TwoSpeed { fast: 3 }, 600_000);
    assert!(r.finished.contains_key(&1), "the read did not finish");
    let first = r.cold.log.iter().filter(|e| matches!(e, ColdEvent::Answered { .. })).count();
    let resent = r.cold.log.iter().filter(|e| matches!(e, ColdEvent::ReGetAnswered { .. })).count();
    let srtt = r.cold.srtt_ms().expect("sampled");
    assert!(srtt >= 1_000.0, "SRTT {srtt} ms: the RTO never learned the 1.5 s round trip");
    assert!(first > 4 * resent, "{first} blocks answered on the first GET, {resent} only on a re-send");
}

/// KARN'S RULE: a fetch that was re-sent is never a sample. Here the FIRST
/// GET of one block is answered only after 5 s and its re-sends never are:
/// the answer comes back while the fetch is on a re-send, and nothing says
/// which copy answered. Sampled, 5 s of "round trip" would blow the RTO far
/// past what a 20 ms node deserves.
#[test]
fn a_late_answer_to_a_re_sent_fetch_is_not_a_sample() {
    let (root, net) = tree();
    let blocks: Vec<Cid> = blocks_of(root, &net, "a").into_iter().collect();
    let late = blocks[blocks.len() / 2];
    let r = run(root, &net, &[(1, "a")], Node::Late { late, late_ms: 5_000 }, 600_000);
    assert!(r.finished.contains_key(&1), "the read did not finish");
    assert!(r.cold.log.iter().any(|e| matches!(e, ColdEvent::ReGetAnswered { block, .. } if *block == late)), "the late block was not re-sent before its answer came");
    // The SMOOTHED round trip — not the RTO, which is rightly backed off by
    // the re-sends that never answered.
    let srtt = r.cold.srtt_ms().expect("fast fetches were sampled");
    assert!(srtt < 100.0, "SRTT {srtt} ms on a {FAST_MS} ms node: a re-sent fetch's answer was sampled");
}

#[test]
fn a_block_the_node_never_answers_is_not_answering_at_its_own_deadline_not_missing() {
    let (root, net) = tree();
    let r = run(root, &net, &[(1, "a")], Node::Silent, 4 * COLD_FETCH_DEADLINE_MS);
    assert!(r.finished.is_empty());
    let when = r.cold.log.iter().find_map(|e| match e {
        ColdEvent::NotAnswering { block, after_ms } if *block == root => Some(*after_ms),
        _ => None,
    });
    // The deadline counts from the block's FIRST send and is checked on each
    // of its own (backed-off) timeouts: it ends on the first timeout past 30 s.
    assert!(when.is_some_and(|ms| (COLD_FETCH_DEADLINE_MS..2 * COLD_FETCH_DEADLINE_MS).contains(&ms)), "not answering at {when:?}");
    assert!(r.timed_out >= 3, "re-fetched only {} times before giving up", r.timed_out);
    assert_eq!(r.cold.open(), 0);
    assert_eq!(r.cold.fetching().count(), 0, "a read that ended left GETs in flight");
}

#[test]
fn a_block_whose_bytes_do_not_hash_to_it_is_dropped_and_asked_again() {
    let (root, net) = tree();
    let mut c = ColdReads::switched_on();
    c.set_root(root);
    assert!(c.take(1, b"a/", b"a0", 0));
    let first = c.take_gets();
    assert_eq!(first.len(), 1, "the root first");
    c.arrived(root, b"\x00not the root", 100);
    assert!(c.log.contains(&ColdEvent::Rejected { block: root }));
    // Asked again on its OWN clock, at its RTO — not at the speed of the bad answer.
    assert!(c.take_gets().is_empty(), "a rejected block was asked again at once, re-arming its clock");
    let rto = COLD_RTO_INITIAL_MS as u64;
    c.tick(rto - 1);
    assert!(c.take_gets().is_empty(), "asked again before its RTO");
    c.tick(rto);
    let again = c.take_gets();
    assert_eq!((again[0].block, again[0].attempt), (root, 2), "a rejected block was not asked again");
    c.arrived(root, &net[&root], rto + 100);
    assert!(!c.take_gets().is_empty(), "the verified root did not lead on to its children");
}

#[test]
fn a_root_the_node_refuses_is_its_own_and_goes_back_to_the_engine() {
    let (root, _) = tree();
    let mut c = ColdReads::switched_on();
    c.set_root(root);
    assert!(c.take(7, b"a/", b"a0", 0));
    let _ = c.take_gets();
    c.refused(root, 50);
    assert_eq!(c.take_returned(), vec![(7, b"a/".to_vec(), b"a0".to_vec())]);
    assert!(c.log.contains(&ColdEvent::Local { root }));
    assert!(!c.take(8, b"b/", b"b0", 60), "a local root's next load was taken from the engine");
}

#[test]
fn off_or_rootless_it_takes_nothing() {
    let mut c = ColdReads::default();
    assert!(!c.take(1, b"a/", b"a0", 0), "the flag is off");
    c.on = true;
    assert!(!c.take(1, b"a/", b"a0", 0), "no root known");
}

/// A block the node REFUSES every time, fast, is not re-asked at the speed of
/// the refusal: it waits its RTO like any unanswered fetch, and its own
/// deadline still runs out. Re-asking it at once would re-arm its clock on
/// every refusal, so it would never time out, never reach its deadline, and
/// the page would send GETs as fast as the node refuses them.
#[test]
fn a_block_the_node_refuses_every_time_waits_its_rto_and_runs_out_its_deadline() {
    let (root, net) = tree();
    let inner = *blocks_of(root, &net, "a").iter().find(|b| **b != root).expect("an inner block");
    let r = run(root, &net, &[(1, "a")], Node::Refuses { refused: inner }, 4 * COLD_FETCH_DEADLINE_MS);
    let asks = r.asked.iter().filter(|b| **b == inner).count();
    let not_answering = r.cold.log.iter().any(|e| matches!(e, ColdEvent::NotAnswering { block, .. } if *block == inner));
    assert!(not_answering, "the refused block never ran out its deadline ({asks} GETs of it)");
    // Backed-off RTOs from the floor: ≈ 100·(1+2+4+…) ms, capped at ×64 —
    // a dozen or so asks in 30 s, not one per refusal (30 s / 20 ms = 1500).
    assert!(asks <= 30, "the refused block was asked {asks} times in {} ms", COLD_FETCH_DEADLINE_MS);
}

/// The page's one-shot timer: `next_due_ms` names when the earliest fetch in
/// flight reaches its RTO, and a tick AT that moment times it out — not one
/// millisecond before.
#[test]
fn next_due_names_the_moment_the_earliest_fetch_times_out() {
    let (root, _net) = tree();
    let mut c = ColdReads::switched_on();
    assert_eq!(c.next_due_ms(0), None, "nothing in flight, nothing due");
    c.set_root(root);
    assert!(c.take(1, b"a/", b"a0", 0));
    let _ = c.take_gets();
    let rto = COLD_RTO_INITIAL_MS as u64;
    assert_eq!(c.next_due_ms(0), Some(rto));
    assert_eq!(c.next_due_ms(400), Some(rto - 400));
    c.tick(rto - 1);
    assert!(c.take_gets().is_empty(), "timed out before it was due");
    assert_eq!(c.next_due_ms(rto - 1), Some(1));
    c.tick(rto);
    assert_eq!(c.take_gets().len(), 1, "not re-sent at the moment it was due");
}
