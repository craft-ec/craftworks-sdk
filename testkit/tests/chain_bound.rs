//! sdk#174: ONE client request causes at most one request's worth of GETs.
//!
//! To the node a client request is ONE delegate chain however many fetch
//! rounds it takes, held under exclusion throughout and truncated --
//! invisibly -- past 100 iterations (~400 GETs). So a cold scan answers a
//! SHORT page and a cursor at `max_gets_per_request` (the next page is a new
//! request), and a cold write waits for its client to ask after it
//! (`AskWrite`) before the next round of its path.
use protocol::{Op, Reply, Request, WriteState};
use testkit::full_node::{Conn, FullNode, Served};

const T0: u64 = 1_790_000_000_000;

fn value(i: u64, size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

fn written(n: u64) -> FullNode {
    let a = FullNode::new();
    let mut w = a.connect();
    w.client(&Request::Identity);
    for (k, chunk) in (0..n).collect::<Vec<_>>().chunks(50).enumerate() {
        let ops = chunk
            .iter()
            .map(|i| Op::Put(format!("t/{i:06}").into_bytes(), value(*i, 1024)))
            .collect();
        w.client(&Request::Write {
            write_id: 1000 + k as u64,
            ops,
        });
    }
    a
}

/// A page's keys and its cursor.
type Keys = (Vec<Vec<u8>>, Option<Vec<u8>>);

fn page(r: &[Vec<u8>], req: u64) -> Option<Keys> {
    r.iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .find_map(|x| match x {
            Reply::Page {
                req_id,
                entries,
                cursor,
                ..
            } if req_id == req => Some((entries.into_iter().map(|(k, _)| k).collect(), cursor)),
            _ => None,
        })
}

fn told(r: &[Vec<u8>], id: u64) -> Vec<WriteState> {
    r.iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|x| match x {
            Reply::WriteState { write_id, state } if write_id == id => Some(state),
            _ => None,
        })
        .collect()
}

/// A client request and the GETs its chain caused.
fn chain(c: &mut Conn, r: &Request) -> (Vec<Vec<u8>>, usize) {
    let before = c.served(Served::Get);
    let out = c.client(r);
    (out, c.served(Served::Get) - before)
}

fn read(c: &mut Conn, req_id: u64, after: Option<Vec<u8>>) -> Request {
    let _ = c;
    Request::Range {
        req_id,
        lo: protocol::Bound::Unbounded,
        hi: protocol::Bound::Unbounded,
        reverse: false,
        after,
        max_entries: 256,
    }
}

#[test]
fn a_cold_read_wider_than_one_request_answers_a_short_page_and_its_cursor_reads_the_rest() {
    let cap = engine::Params::default().max_gets_per_request as usize;
    let node = FullNode::cold_over(&written(300));
    let mut c = node.connect();
    c.client(&Request::Identity);
    let mut rows: Vec<Vec<u8>> = Vec::new();
    let (mut after, mut per_request) = (None, Vec::new());
    for req in 1..=20u64 {
        let r = read(&mut c, req, after.clone());
        let (out, gets) = chain(&mut c, &r);
        per_request.push(gets);
        let (keys, cursor) =
            page(&out, req).unwrap_or_else(|| panic!("request {req} was never answered"));
        rows.extend(keys);
        if cursor.is_none() {
            break;
        }
        after = cursor;
    }
    println!(
        "  cold read: GETs per request {per_request:?}; {} rows",
        rows.len()
    );
    assert_eq!(
        per_request[0], cap,
        "the first request's chain was not cut at exactly the cap"
    );
    assert!(
        per_request.iter().all(|g| *g <= cap),
        "a request's chain exceeded the cap: {per_request:?}"
    );
    assert!(
        per_request.len() >= 2,
        "the read fitted one request: the test is empty"
    );
    let distinct: std::collections::BTreeSet<&Vec<u8>> = rows.iter().collect();
    assert_eq!(
        (rows.len(), distinct.len()),
        (300, 300),
        "not every row exactly once"
    );
}

/// CONTROL: warm, the same read is the pages it always was, and costs no GET.
#[test]
fn control_a_warm_read_is_one_page_per_256_rows() {
    let node = written(300);
    let mut c = node.connect();
    c.client(&Request::Identity);
    let r = read(&mut c, 1, None);
    let (out, gets) = chain(&mut c, &r);
    let (keys, cursor) = page(&out, 1).expect("answered");
    assert_eq!((keys.len(), gets), (256, 0));
    assert!(cursor.is_some());
}

/// A cold write whose path needs more than one request's GETs: each client
/// call -- the write, then its client asking after it -- continues it by at
/// most the cap, and it is Accepted after ceil(need / cap) of them.
#[test]
fn a_cold_write_is_continued_by_its_client_asking_after_it_and_no_chain_exceeds_the_cap() {
    let cap = engine::Params::default().max_gets_per_request as usize;
    let node = FullNode::cold_over(&written(300));
    let mut c = node.connect();
    c.client(&Request::Identity);
    let ops: Vec<Op> = (0..20u64)
        .map(|i| {
            Op::Put(
                format!("t/{:06}", i * 15).into_bytes(),
                value(20_000 + i, 1024),
            )
        })
        .collect();
    let (mut out, mut gets) = chain(&mut c, &Request::Write { write_id: 1, ops });
    let mut per_call = vec![gets];
    for _ in 0..8 {
        if told(&out, 1).contains(&WriteState::Accepted) {
            break;
        }
        (out, gets) = chain(&mut c, &Request::AskWrite { write_id: 1 });
        per_call.push(gets);
    }
    let total: usize = per_call.iter().sum();
    println!(
        "  cold write: GETs per client call {per_call:?} (total {total}); told {:?}",
        told(&out, 1)
    );
    assert!(
        told(&out, 1).contains(&WriteState::Published),
        "never published: {:?}",
        told(&out, 1)
    );
    assert!(
        per_call.iter().all(|g| *g <= cap),
        "a chain exceeded the cap: {per_call:?}"
    );
    assert!(
        per_call.len() >= 2,
        "the write fitted one request: the test is empty"
    );
    assert_eq!(
        per_call.len(),
        total.div_ceil(cap),
        "not ceil(need / cap) client calls"
    );
}

/// CONTROL: warm, a write is one call.
#[test]
fn control_a_warm_write_is_one_call() {
    let node = written(300);
    let mut c = node.connect();
    c.client(&Request::Identity);
    let out = c.client(&Request::Write {
        write_id: 1,
        ops: vec![Op::Put(b"t/000015".to_vec(), value(1, 1024))],
    });
    assert!(told(&out, 1).contains(&WriteState::Published));
}

/// A parked write NOBODY asks after -- its client went away -- is released
/// `Busy` after 3 x reask_after ticks RUN (nothing applied; `Busy` is what a
/// client re-sends, where `Failed` is rolled back and shown to the person),
/// and the engine is open again.
#[test]
fn a_parked_write_nobody_asks_after_is_released_and_the_engine_opens() {
    let p = engine::Params::default();
    let node = FullNode::cold_over(&written(300));
    let mut c = node.connect();
    c.client(&Request::Identity);
    let _ = c.tick_at(T0);
    let ops: Vec<Op> = (0..20u64)
        .map(|i| {
            Op::Put(
                format!("t/{:06}", i * 15).into_bytes(),
                value(20_000 + i, 1024),
            )
        })
        .collect();
    let out = c.client(&Request::Write { write_id: 1, ops });
    assert!(
        told(&out, 1).is_empty(),
        "the write was not left parked: {:?}",
        told(&out, 1)
    );
    let mut released_at = None;
    for k in 1..=4 * p.reask_after {
        let t = told(&c.tick_at(T0 + k * 1000), 1);
        assert!(
            !t.iter()
                .any(|s| matches!(s, WriteState::Failed | WriteState::Lost)),
            "a parked write was told {t:?}: rolled back, where nothing was wrong with it"
        );
        if t.contains(&WriteState::Busy) {
            released_at = Some(k);
            break;
        }
    }
    assert_eq!(
        released_at,
        Some(3 * p.reask_after),
        "not released at 3 x reask_after"
    );
    let next = c.client(&Request::Write {
        write_id: 2,
        ops: vec![Op::Put(b"z/after".to_vec(), b"x".to_vec())],
    });
    assert!(
        told(&next, 2).contains(&WriteState::Published),
        "the engine stayed closed: {:?}",
        told(&next, 2)
    );
}

/// THE CLOCK STOOD STILL (the architect's sdk#196 review, executed). While a
/// chain is served the node runs nothing else, so no tick runs and the
/// engine's `now` does not move; each chain here takes 50 s of wall time (64
/// GETs at ~0.75 s -- an ordinary cold write on a real network). The client
/// asks after its write as each chain ends, and the next tick to run carries
/// the real time, +50 s. Measured as `now - stamp`, that jump was the write's
/// "silence" and it was released `Failed` while its client was asking. Counted
/// in ticks run, it is never released and it publishes.
#[test]
fn a_park_the_clock_did_not_see_is_not_the_write_s_silence() {
    let node = FullNode::cold_over(&written(600));
    let mut c = node.connect();
    c.client(&Request::Identity);
    let _ = c.tick_at(T0);
    let ops: Vec<Op> = (0..120u64)
        .map(|i| {
            Op::Put(
                format!("t/{:06}", i * 5).into_bytes(),
                value(20_000 + i, 1024),
            )
        })
        .collect();
    let (mut seen, _) = chain(&mut c, &Request::Write { write_id: 1, ops });
    let mut all = told(&seen, 1);
    let mut clock = T0;
    let mut chains = 1;
    while !all.contains(&WriteState::Published) && chains < 16 {
        clock += 50_000;
        let (o, _) = chain(&mut c, &Request::AskWrite { write_id: 1 });
        seen = o;
        seen.extend(c.tick_at(clock));
        all.extend(told(&seen, 1));
        chains += 1;
    }
    println!("  50 s per chain, no tick between: {chains} chains, told {all:?}");
    assert!(
        !all.iter()
            .any(|s| matches!(s, WriteState::Failed | WriteState::Lost | WriteState::Busy)),
        "a write whose client was asking was released: {all:?}"
    );
    assert!(
        all.contains(&WriteState::Published),
        "never published: {all:?}"
    );
    assert!(
        chains >= 3,
        "the write needed fewer than three chains: the test is empty"
    );
}

/// Asking after a write the engine FINISHED and forgot -- its `Published`
/// dropped or delivered to another tab (F39, F49) -- gets NO verdict. It was
/// told `Lost` by absence, which rolled back a write that is in the tree.
#[test]
fn asking_after_a_finished_write_gets_no_verdict() {
    let node = FullNode::new();
    let mut c = node.connect();
    c.client(&Request::Identity);
    let out = c.client(&Request::Write {
        write_id: 1,
        ops: vec![Op::Put(b"k".to_vec(), b"v".to_vec())],
    });
    assert!(
        told(&out, 1).contains(&WriteState::Published),
        "the control write did not publish"
    );
    let asked = c.client(&Request::AskWrite { write_id: 1 });
    assert_eq!(
        told(&asked, 1),
        Vec::<WriteState>::new(),
        "a published write the engine forgot was given a verdict"
    );
}

/// A write whose client keeps asking is never released, however long its
/// blocks take: every ask is heard, and silence counts only ticks with no
/// ask between. The node here HOLDS its answers for 60 s of ticks while the
/// client asks each second; released, the write publishes.
#[test]
fn a_write_whose_client_keeps_asking_is_never_released() {
    let p = engine::Params::default();
    let node = FullNode::cold_over(&written(300));
    let mut c = node.connect();
    c.client(&Request::Identity);
    let _ = c.tick_at(T0);
    c.hold_answers();
    let ops: Vec<Op> = (0..20u64)
        .map(|i| {
            Op::Put(
                format!("t/{:06}", i * 15).into_bytes(),
                value(20_000 + i, 1024),
            )
        })
        .collect();
    let mut all = told(&c.client(&Request::Write { write_id: 1, ops }), 1);
    assert!(
        c.held() > 0,
        "nothing was held: the write never waited on a block"
    );
    let secs = 4 * p.reask_after;
    for k in 1..=secs {
        all.extend(told(&c.tick_at(T0 + k * 1000), 1));
        all.extend(told(&c.client(&Request::AskWrite { write_id: 1 }), 1));
    }
    assert!(
        !all.iter()
            .any(|s| matches!(s, WriteState::Busy | WriteState::Failed | WriteState::Lost)),
        "released after {secs} s while its client asked every second: {all:?}"
    );
    // The node stops holding; what it held is delivered, and every ask's
    // re-fetch of the same blocks with it (a duplicate answer is ignored).
    c.stop_holding();
    let mut k = secs;
    while c.held() > 0 && k < secs + 200 {
        all.extend(told(&c.release_one(), 1));
        k += 1;
        all.extend(told(&c.tick_at(T0 + k * 1000), 1));
        all.extend(told(&c.client(&Request::AskWrite { write_id: 1 }), 1));
        if all.contains(&WriteState::Published) {
            break;
        }
    }
    println!("  held {secs} s with an ask a second, then released: told {all:?}");
    assert!(
        all.contains(&WriteState::Published),
        "never published: {all:?}"
    );
}

/// A GET ANSWER LOST FROM THE WRITE'S CHAIN (sdk#173's event) is fetched
/// again by its WRITER's next ask, and the write's verdicts are said in the
/// writer's own chain (the architect's sdk#196 v4-bridge review, executed).
/// The ask returned nothing while the round was "still out": the write was
/// neither fetched nor released for as long as its client kept asking, and a
/// block fetched by ANOTHER tab's chain carried its Published to that tab.
#[test]
fn a_lost_get_answer_is_fetched_again_by_the_writer_s_ask() {
    let node = FullNode::cold_over(&written(300));
    let mut c = node.connect();
    c.client(&Request::Identity);
    let _ = c.tick_at(T0);
    c.hold_answers();
    let ops: Vec<Op> = (0..20u64)
        .map(|i| {
            Op::Put(
                format!("t/{:06}", i * 15).into_bytes(),
                value(20_000 + i, 1024),
            )
        })
        .collect();
    let _ = c.client(&Request::Write { write_id: 1, ops });
    let held = c.held();
    assert!(
        held >= 1,
        "the write's round held no answer: nothing to lose"
    );
    assert!(c.lose_one_held(), "nothing lost");
    let mut all = Vec::new();
    while c.held() > 0 {
        all.extend(told(&c.release_one(), 1));
    }
    c.stop_holding();
    assert!(
        all.is_empty(),
        "the write went on with an answer missing: {all:?}"
    );
    let mut in_asks = Vec::new();
    let mut first_ask_gets = None;
    for _ in 0..4 {
        let (o, g) = chain(&mut c, &Request::AskWrite { write_id: 1 });
        first_ask_gets.get_or_insert(g);
        in_asks.extend(told(&o, 1));
        if in_asks.contains(&WriteState::Published) {
            break;
        }
    }
    println!("  one answer lost of {held}: the first ask re-fetched {first_ask_gets:?} GETs; told in the writer's asks {in_asks:?}");
    assert!(
        first_ask_gets.unwrap_or(0) >= 1,
        "the writer's ask fetched nothing: the lost block was never asked again"
    );
    assert!(
        in_asks.contains(&WriteState::Published),
        "the write was not published in its writer's own chain: {in_asks:?}"
    );
}
