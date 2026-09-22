//! COLD READS IN THE PAGE against a scripted client API (craftworks_sdk::cold).
//!
//! * One GET stalled the way F52 stalls — its first GET answered only after
//!   60 s: the read completes in about one timeout plus one fetch, and a read
//!   that does not need that block does not wait for it at all.
//! * A batch the node answers IN TURN (≈ 115 ms a GET: 70 pipelined GETs took
//!   ≈ 8 s under load, measured): no false timeouts with the in-flight cap —
//!   and without the cap there are (the cap's mutant, run by hand).
//! * A node that never answers: "not answering" when a block's OWN deadline
//!   runs out, never "missing".
//!
//! EVERYTHING IS PER FETCH (the owner): each GET has its own clock, and a
//! timeout re-sends only that GET.

use craftworks_sdk::cold::{ColdEvent, ColdReads, COLD_FETCH_DEADLINE_MS, COLD_GET_TIMEOUT_MS, COLD_IN_FLIGHT};
use craftworks_sdk::{Store, TreeStore};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

/// An unstalled cold GET, one at a time: well under 1 s.
const FETCH_MS: u64 = 300;
/// F52: a lost streamed response is healed ≈ 60 s after it was sent.
const STALL_MS: u64 = 60_000;
/// A node answering GETs in turn: 70 pipelined in ≈ 8 s.
const IN_TURN_MS: u64 = 115;
const STEP_MS: u64 = 10;

/// Two ranges of 150 rows each, values big enough to be their own blocks.
fn tree() -> (Cid, BTreeMap<Cid, Vec<u8>>) {
    let mut t = TreeStore::new();
    for p in ["a", "b"] {
        for i in 0..150u32 {
            let mut v = format!("{p}/{i:04}:").into_bytes();
            v.resize(1_000, p.as_bytes()[0]);
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
    /// Every GET after `FETCH_MS`, except the FIRST GET of this block, after `STALL_MS`.
    Stalls(Option<Cid>),
    /// One GET at a time, `IN_TURN_MS` each, in the order sent.
    InTurn,
    /// Never.
    Silent,
}

struct Run {
    finished: BTreeMap<u64, u64>,
    cold: ColdReads,
    asked: Vec<Cid>,
    most_in_flight: usize,
    timed_out: usize,
}

fn run(root: Cid, net: &BTreeMap<Cid, Vec<u8>>, loads: &[(u64, &str)], node: Node, until_ms: u64) -> Run {
    let mut c = ColdReads { on: true, ..ColdReads::default() };
    c.set_root(root);
    let mut due: Vec<(u64, Cid)> = Vec::new();
    let mut node_free = 0u64;
    let mut asked = Vec::new();
    let mut finished = BTreeMap::new();
    let (mut most_in_flight, mut now) = (0, 0);
    for (id, prefix) in loads {
        let (lo, hi) = (format!("{prefix}/").into_bytes(), format!("{prefix}0").into_bytes());
        assert!(c.take(*id, &lo, &hi, now), "the cold reader refused a load");
    }
    while now <= until_ms && finished.len() < loads.len() {
        for g in c.take_gets() {
            asked.push(g.block);
            match node {
                Node::Stalls(s) => {
                    let wait = if Some(g.block) == s && g.attempt == 1 { STALL_MS } else { FETCH_MS };
                    due.push((now + wait, g.block));
                }
                Node::InTurn => {
                    node_free = node_free.max(now) + IN_TURN_MS;
                    due.push((node_free, g.block));
                }
                Node::Silent => {}
            }
        }
        most_in_flight = most_in_flight.max(c.fetching().count());
        now += STEP_MS;
        let (ready, later): (Vec<_>, Vec<_>) = due.into_iter().partition(|(t, _)| *t <= now);
        due = later;
        for (_, b) in ready {
            c.arrived(b, &net[&b], now);
        }
        c.tick(now);
        for (id, rows, _) in c.take_done() {
            assert_eq!(rows.len(), 150, "load {id} finished with {} rows", rows.len());
            finished.insert(id, now);
        }
        if !c.take_not_answering().is_empty() {
            break;
        }
    }
    let timed_out = c.log.iter().filter(|e| matches!(e, ColdEvent::TimedOut { .. })).count();
    Run { finished, cold: c, asked, most_in_flight, timed_out }
}

/// The blocks one range's walk GETs, unstalled.
fn blocks_of(root: Cid, net: &BTreeMap<Cid, Vec<u8>>, prefix: &str) -> BTreeSet<Cid> {
    run(root, net, &[(1, prefix)], Node::Stalls(None), 60_000).asked.into_iter().collect()
}

#[test]
fn a_stalled_get_costs_one_timeout_and_one_fetch_not_the_stall() {
    let (root, net) = tree();
    let only_a: Vec<Cid> = blocks_of(root, &net, "a").difference(&blocks_of(root, &net, "b")).copied().collect();
    let stalled = *only_a.last().expect("range a needs a block range b does not");
    let base = run(root, &net, &[(1, "a")], Node::Stalls(None), 120_000).finished[&1];
    let r = run(root, &net, &[(1, "a"), (2, "b")], Node::Stalls(Some(stalled)), 120_000);
    let (a, b) = (r.finished[&1], r.finished[&2]);
    assert!(a <= base + COLD_GET_TIMEOUT_MS + FETCH_MS + 2 * STEP_MS, "a stalled GET cost {a} ms against {base} unstalled: it waited on the stall");
    assert!(a < STALL_MS, "the read took the whole stall ({a} ms)");
    // …because it was ASKED AGAIN, and the re-GET is what answered.
    assert!(r.cold.log.iter().any(|e| matches!(e, ColdEvent::TimedOut { block, attempt: 1, .. } if *block == stalled)));
    assert!(r.cold.log.iter().any(|e| matches!(e, ColdEvent::ReGetAnswered { block, attempt: 2, .. } if *block == stalled)));
    // PER FETCH: exactly ONE re-fetch was sent — the stalled block's — and
    // every other block was sent once.
    let mut sends: BTreeMap<Cid, usize> = BTreeMap::new();
    for b in &r.asked {
        *sends.entry(*b).or_default() += 1;
    }
    assert!(r.asked.len() >= 70, "{} GETs: not the batch the owner's test names", r.asked.len());
    assert_eq!(sends[&stalled], 2, "the stalled block was sent {} times", sends[&stalled]);
    let others: Vec<_> = sends.iter().filter(|(b, n)| **b != stalled && **n != 1).collect();
    assert!(others.is_empty(), "blocks other than the stalled one were re-sent: {others:?}");
    // The other read never waited on it.
    assert!(b <= base + 2 * STEP_MS, "a read that does not need the stalled block took {b} ms against {base}");
}

/// THE CAP: a node answering in turn delays the LAST of a big batch by the
/// whole batch; with at most `COLD_IN_FLIGHT` in flight no GET waits long
/// enough to be timed out while it is merely queued.
#[test]
fn a_batch_the_node_answers_in_turn_has_no_false_timeouts() {
    let (root, net) = tree();
    let r = run(root, &net, &[(1, "a")], Node::InTurn, 120_000);
    assert!(r.finished.contains_key(&1), "the batch did not finish");
    assert!(r.asked.len() >= 70, "the batch is {} GETs, not a batch", r.asked.len());
    assert!(r.most_in_flight <= COLD_IN_FLIGHT, "{} GETs were in flight", r.most_in_flight);
    assert_eq!(r.timed_out, 0, "{} healthy GETs, merely queued at the node, were timed out", r.timed_out);
}

#[test]
fn a_block_the_node_never_answers_is_not_answering_at_its_own_deadline_not_missing() {
    let (root, net) = tree();
    let r = run(root, &net, &[(1, "a")], Node::Silent, 2 * COLD_FETCH_DEADLINE_MS);
    assert!(r.finished.is_empty());
    let when = r.cold.log.iter().find_map(|e| match e {
        ColdEvent::NotAnswering { block, after_ms } if *block == root => Some(*after_ms),
        _ => None,
    });
    // The block's deadline counts from its FIRST send, checked on each of its
    // own 2 s timeouts: it ends within one timeout past 30 s.
    assert!(when.is_some_and(|ms| (COLD_FETCH_DEADLINE_MS..COLD_FETCH_DEADLINE_MS + COLD_GET_TIMEOUT_MS + STEP_MS).contains(&ms)), "not answering at {when:?}");
    // Re-fetched without a try count until then, and nothing is asked after.
    assert!(r.timed_out >= (COLD_FETCH_DEADLINE_MS / COLD_GET_TIMEOUT_MS) as usize);
    assert_eq!(r.cold.open(), 0);
    assert_eq!(r.cold.fetching().count(), 0, "a read that ended left GETs in flight");
}

#[test]
fn a_block_whose_bytes_do_not_hash_to_it_is_dropped_and_asked_again() {
    let (root, net) = tree();
    let mut c = ColdReads { on: true, ..ColdReads::default() };
    c.set_root(root);
    assert!(c.take(1, b"a/", b"a0", 0));
    let first = c.take_gets();
    assert_eq!(first.len(), 1, "the root first");
    c.arrived(root, b"\x00not the root", 100);
    assert!(c.log.contains(&ColdEvent::Rejected { block: root }));
    let again = c.take_gets();
    assert_eq!((again[0].block, again[0].attempt), (root, 2), "a rejected block was not asked again");
    c.arrived(root, &net[&root], 200);
    assert!(!c.take_gets().is_empty(), "the verified root did not lead on to its children");
}

#[test]
fn a_root_the_node_refuses_is_its_own_and_goes_back_to_the_engine() {
    let (root, _) = tree();
    let mut c = ColdReads { on: true, ..ColdReads::default() };
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
