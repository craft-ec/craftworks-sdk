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
/// Failed after 3 x reask_after ticks, and the engine is open again.
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
        if told(&c.tick_at(T0 + k * 1000), 1).contains(&WriteState::Failed) {
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
