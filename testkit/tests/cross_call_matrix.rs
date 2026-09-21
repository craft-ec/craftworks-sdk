//! sdk#150: MODE x WORKLOAD -- what no native test exercised across delegate
//! calls.
//!
//! Running the existing suite with answers delivered one per call turned ONE
//! test red, against a defect that stopped every real write over one block:
//! the suite had almost no test in which a multi-block commit must publish
//! through the shell. So the gate is a matrix. Each WORKLOAD runs under each
//! MODE it can meet on a real node, and every cell asserts the same two things
//! on every call: nothing is STRANDED (lost at the end of the call), and every
//! request is eventually ANSWERED.
//!
//! A cell is green, red (with its reason), or NOT REACHED -- the workload did
//! not create the condition it exists for (a parity flush that fit in one
//! return tests nothing), which is reported, never counted as green.
//!
//! `KNOWN_RED` is the list of cells red on the code as it stands, each naming
//! the defect. A red cell not in the list fails the test; so does a listed
//! cell that turns green -- a fix must move its cells, so the list can never
//! quietly describe a different tree.

use protocol::{Bound, Op, Reply, Request, WriteState};
use testkit::full_node::{Conn, FullNode};

#[derive(Clone, Copy, Debug)]
struct Mode {
    one_per_call: bool,
    cold: bool,
    evicting: bool,
    real_ticks: bool,
}

impl Mode {
    fn name(&self) -> String {
        let mut p = vec![];
        if self.one_per_call {
            p.push("one-per-call")
        } else {
            p.push("batch")
        }
        if self.cold {
            p.push("cold")
        }
        if self.evicting {
            p.push("evicting")
        }
        if self.real_ticks {
            p.push("real-ticks")
        }
        p.join("+")
    }
}

const BATCH: Mode = Mode {
    one_per_call: false,
    cold: false,
    evicting: false,
    real_ticks: false,
};
const APC: Mode = Mode {
    one_per_call: true,
    ..BATCH
};

#[derive(Debug, PartialEq)]
enum Cell {
    Green,
    Red(String),
    NotReached(String),
}

fn states_of(replies: &[Vec<u8>], id: u64) -> Vec<WriteState> {
    replies
        .iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|r| match r {
            Reply::WriteState { write_id, state } if write_id == id => Some(state),
            _ => None,
        })
        .collect()
}

fn value(i: u64, size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

/// A warm node holding a published tree of `n` records of `size` bytes,
/// written in batches that each fit one commit.
fn tree(n: u64, size: usize) -> FullNode {
    let node = FullNode::new();
    let mut c = node.connect();
    c.client(&Request::Identity);
    for (w, chunk) in (0..n).collect::<Vec<_>>().chunks(50).enumerate() {
        let ops = chunk
            .iter()
            .map(|i| Op::Put(format!("t/{i:06}").into_bytes(), value(*i, size)))
            .collect();
        let r = c.client(&Request::Write {
            write_id: 1000 + w as u64,
            ops,
        });
        assert!(
            states_of(&r, 1000 + w as u64).contains(&WriteState::Published),
            "the fixture tree did not publish"
        );
    }
    node
}

fn connect(node: &FullNode, mode: Mode) -> Conn {
    let c = node.connect();
    if mode.one_per_call {
        c.one_answer_per_call();
    }
    c
}

/// The node for a mode: a fresh warm one, or a cold one over a published tree.
fn node_for(mode: Mode) -> FullNode {
    if mode.cold {
        let n = FullNode::cold_over(&tree(300, 1024));
        if mode.evicting {
            n.evicting()
        } else {
            n
        }
    } else {
        FullNode::new()
    }
}

/// The two assertions every cell makes, on every call it ran.
fn every_call(c: &Conn) -> Option<String> {
    if c.max_stranded() > 0 {
        return Some(format!(
            "a call stranded {} effect(s) -- lost at the end of the call",
            c.max_stranded()
        ));
    }
    let u = c.unanswered();
    (!u.is_empty()).then(|| format!("never answered: {u:?}"))
}

/// W1: a commit of TWO OR MORE data blocks must PUBLISH.
fn w1_multi_block_commit(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.client(&Request::Identity);
    let w = Request::Write {
        write_id: 1,
        ops: vec![
            Op::Put(b"k/big".to_vec(), value(7, 30 * 1024)),
            Op::Put(b"k/small".to_vec(), b"small".to_vec()),
        ],
    };
    let mut replies = c.client(&w);
    // Real ticks: time keeps passing while the commit is in flight.
    if mode.real_ticks {
        let base = 1_790_000_000_000u64;
        for k in 1..=5 {
            replies.extend(c.tick_at(base + k * 1000));
        }
    }
    let st = states_of(&replies, 1);
    if let Some(why) = every_call(&c) {
        return Cell::Red(format!("{why}; told {st:?}"));
    }
    if !st.contains(&WriteState::Published) {
        return Cell::Red(format!("never published: told {st:?}"));
    }
    Cell::Green
}

/// W2: a COLD read needing more blocks than one return may fetch.
fn w2_cold_wide_read(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.client(&Request::Identity);
    let r = c.client(&Request::Range {
        req_id: 1,
        lo: Bound::Unbounded,
        hi: Bound::Unbounded,
        reverse: false,
        after: None,
        max_entries: 300,
    });
    let pages: Vec<usize> = r
        .iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|x| match x {
            Reply::Page {
                req_id: 1, entries, ..
            } => Some(entries.len()),
            _ => None,
        })
        .collect();
    if let Some(why) = every_call(&c) {
        return Cell::Red(format!("{why}; pages {pages:?}"));
    }
    match pages.last() {
        Some(300) => Cell::Green,
        other => Cell::Red(format!("the cold read returned {other:?} of 300 entries")),
    }
}

/// W3: a write that PARKS on a cold path and is then refused must still be
/// told WHY (`TooLarge`), not the older client's `Failed`.
fn w3_parked_then_refused(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    // A budget small enough that a write SMALL ENOUGH TO PARK is still over
    // it once its blocks arrive: the verdict is then decided in a call the
    // NODE made, not the client.
    c.set_params(engine::Params {
        max_commit_blocks: 4,
        ..engine::Params::default()
    });
    c.client(&Request::Identity);
    let ops: Vec<Op> = (0..20u64)
        .map(|i| {
            Op::Put(
                format!("t/{:06}", i * 15).into_bytes(),
                value(20_000 + i, 1024),
            )
        })
        .collect();
    // Re-sent on Busy, the way the outbox does, a bounded number of times.
    let mut st = Vec::new();
    for _ in 0..6 {
        let got = states_of(
            &c.client(&Request::Write {
                write_id: 1,
                ops: ops.clone(),
            }),
            1,
        );
        let busy = got.last() == Some(&WriteState::Busy);
        st.extend(got);
        if !busy {
            break;
        }
    }
    if !mode.cold {
        return Cell::NotReached("a warm node never parks".into());
    }
    if c.served(testkit::full_node::Served::Get) == 0 {
        return Cell::NotReached(format!("the write never parked (no GETs); told {st:?}"));
    }
    if let Some(why) = every_call(&c) {
        return Cell::Red(format!("{why}; told {st:?}"));
    }
    match st.last() {
        Some(WriteState::TooLarge { .. }) => Cell::Green,
        other => Cell::Red(format!(
            "a v3 client was told {other:?} after the park, not TooLarge (all: {st:?})"
        )),
    }
}

/// W4: a commit IN FLIGHT across a real tick -- one second old must not be
/// `Stalled` (the bound is 64 ticks), and it must still publish.
fn w4_commit_across_a_tick(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.hold_answers();
    c.client(&Request::Identity);
    let mut replies = Vec::new();
    while c.held() > 0 {
        replies.extend(c.release_one());
    }
    replies.extend(c.client(&Request::Write {
        write_id: 1,
        ops: vec![
            Op::Put(b"k/big".to_vec(), value(7, 30 * 1024)),
            Op::Put(b"k/small".to_vec(), b"small".to_vec()),
        ],
    }));
    if c.held() == 0 {
        return Cell::NotReached("the commit finished inside its first call".into());
    }
    let base = 1_790_000_000_000u64;
    let mut k = 1;
    while c.held() > 0 && k < 40 {
        replies.extend(c.tick_at(base + k * 1000));
        replies.extend(c.release_one());
        k += 1;
    }
    let st = states_of(&replies, 1);
    if let Some(why) = every_call(&c) {
        return Cell::Red(format!("{why}; told {st:?}"));
    }
    if st.contains(&WriteState::Stalled) {
        return Cell::Red(format!("told Stalled after at most {} s: {st:?}", k - 1));
    }
    if !st.contains(&WriteState::Published) {
        return Cell::Red(format!("never published: {st:?}"));
    }
    Cell::Green
}

/// W5: parity for more groups than one return may put.
fn w5_parity_overflow(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.client(&Request::Identity);
    let mut last = 0;
    for w in 0..12u64 {
        let ops = (0..60u64)
            .map(|i| {
                Op::Put(
                    format!("p/{:06}", w * 1000 + i * 17).into_bytes(),
                    value(w * 100 + i, 700),
                )
            })
            .collect();
        let st = states_of(
            &c.client(&Request::Write {
                write_id: 100 + w,
                ops,
            }),
            100 + w,
        );
        if !st.contains(&WriteState::Published) {
            return Cell::Red(format!("write {w} never published: {st:?}"));
        }
        last = 100 + w;
    }
    let puts_before = c.served(testkit::full_node::Served::Put);
    let r = c.client(&Request::Flush);
    let flush_puts = c.served(testkit::full_node::Served::Put) - puts_before;
    let done = states_of(&r, last);
    if flush_puts <= 128 {
        return Cell::NotReached(format!(
            "the flush put {flush_puts} parity blocks, which fit one return (128)"
        ));
    }
    if let Some(why) = every_call(&c) {
        return Cell::Red(format!("{why}; flush put {flush_puts}"));
    }
    if !done.contains(&WriteState::ParityComplete) {
        return Cell::Red(format!(
            "the last write never reached ParityComplete after the flush ({flush_puts} puts)"
        ));
    }
    Cell::Green
}

/// The cells red on the code as it stands, each with the defect it shows.
const KNOWN_RED: &[(&str, &str, &str)] = &[
    // THE HEAD BUMP: held on blocks an EARLIER call confirmed, stranded, never
    // re-emitted (head_sent is already true). Fix: F2, seed the scheduler from
    // the engine's carried `pending.confirmed`.
    (
        "W1 multi-block commit publishes",
        "one-per-call",
        "head bump stranded (F2)",
    ),
    (
        "W1 multi-block commit publishes",
        "one-per-call+cold",
        "head bump stranded (F2)",
    ),
    // ...and with real ticks, also `Stalled` after ~1 s: the engine's `now` is
    // not in the context, so `in_flight_since` is 0 against an epoch tick.
    (
        "W1 multi-block commit publishes",
        "one-per-call+real-ticks",
        "head bump stranded (F2); `now` not carried",
    ),
    (
        "W4 commit in flight across a real tick",
        "one-per-call+real-ticks",
        "head bump stranded (F2); Stalled after 1 s (`now` not carried)",
    ),
    // THE READ OVERFLOW: the engine asks for more fetches than one return
    // carries (max_gets 4); the rest wait "for the next entry", which the
    // per-call scheduler never reaches, and the read is never answered.
    (
        "W2 cold read wider than one return",
        "batch+cold",
        "read fetches over max_gets stranded",
    ),
    (
        "W2 cold read wider than one return",
        "one-per-call+cold",
        "read fetches over max_gets stranded",
    ),
    // THE EVICTION PING-PONG: `arrived` lasts one call and the node keeps
    // nothing it served, so a read needing two such blocks never finishes.
    (
        "W2 cold read wider than one return",
        "one-per-call+cold+evicting",
        "eviction ping-pong (cycle)",
    ),
    // The parked write's fetches strand FIRST (the read overflow), so the
    // `client_version`-after-a-park defect behind it cannot show yet.
    (
        "W3 parked write refused, told why",
        "one-per-call+cold",
        "parked write's fetches stranded; client_version behind it",
    ),
    // Every write here is multi-block, so the head strand stops the workload
    // before the parity flush it exists for.
    (
        "W5 parity wider than one return",
        "one-per-call",
        "head bump stranded (F2) before parity is reached",
    ),
];

#[test]
fn every_workload_under_every_mode() {
    let workloads: [(&str, fn(Mode) -> Cell, Vec<Mode>); 5] = [
        (
            "W1 multi-block commit publishes",
            w1_multi_block_commit,
            vec![
                BATCH,
                APC,
                Mode {
                    real_ticks: true,
                    ..APC
                },
                Mode { cold: true, ..APC },
            ],
        ),
        (
            "W2 cold read wider than one return",
            w2_cold_wide_read,
            vec![
                Mode {
                    cold: true,
                    ..BATCH
                },
                Mode { cold: true, ..APC },
                Mode {
                    cold: true,
                    evicting: true,
                    ..APC
                },
            ],
        ),
        (
            "W3 parked write refused, told why",
            w3_parked_then_refused,
            vec![Mode { cold: true, ..APC }],
        ),
        (
            "W4 commit in flight across a real tick",
            w4_commit_across_a_tick,
            vec![Mode {
                real_ticks: true,
                ..APC
            }],
        ),
        (
            "W5 parity wider than one return",
            w5_parity_overflow,
            vec![APC],
        ),
    ];
    let mut surprises = Vec::new();
    println!();
    for (w, run, modes) in workloads.iter() {
        for m in modes {
            // A panic is a RESULT for its cell (the fixture's cycle guard, a
            // decoder refusing), not the end of the matrix.
            let cell = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(*m)))
                .unwrap_or_else(|p| {
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    Cell::Red(format!(
                        "PANICKED: {}",
                        msg.chars().take(110).collect::<String>()
                    ))
                });
            let mode = m.name();
            let known = KNOWN_RED
                .iter()
                .find(|(kw, km, _)| *kw == *w && *km == mode);
            let shown = match &cell {
                Cell::Green => "GREEN".to_string(),
                Cell::Red(why) => format!("RED   {why}"),
                Cell::NotReached(why) => format!("NOT REACHED  {why}"),
            };
            println!("  {w:<42} {mode:<34} {shown}");
            match (&cell, known) {
                (Cell::Red(_), None) => {
                    surprises.push(format!("{w} / {mode}: red and not in KNOWN_RED"))
                }
                (Cell::Green, Some((_, _, defect))) => surprises.push(format!(
                    "{w} / {mode}: GREEN but listed red ({defect}) -- move it"
                )),
                (Cell::NotReached(why), _) => {
                    surprises.push(format!("{w} / {mode}: not reached -- {why}"))
                }
                _ => {}
            }
        }
    }
    assert!(surprises.is_empty(), "\n{}", surprises.join("\n"));
}
