//! sdk#150: MODE x WORKLOAD -- what no native test exercised across delegate
//! calls.
//!
//! Running the existing suite with answers delivered one per call turned ONE
//! test red, against a defect that stopped every real write over one block:
//! the suite had almost no test in which a multi-block commit must publish
//! through the shell. So the gate is a matrix. Each WORKLOAD runs under each
//! MODE it can meet on a real node, and every cell asserts the same three
//! things: nothing is STRANDED (lost at the end of a call), every request is
//! eventually ANSWERED, and the engine is OPEN AFTERWARDS -- a trailing
//! one-key write on the same connection reaches `Published`. The third is
//! what a wedged engine fails while passing the first two: a commit that can
//! never end strands nothing and leaves nothing unanswered that anyone asked
//! twice (sdk#160).
//!
//! A cell is green, red (with its reason), or NOT REACHED -- the workload did
//! not create the condition it exists for (a parity flush that fit in one
//! return tests nothing), which is reported, never counted as green.
//!
//! # Which cells exist, and which are ABSENT on purpose
//!
//! Modes: batch (answers together -- the old fixture, a control), one-per-call
//! (the real node's shape), cold, evicting, real ticks, and two live
//! sessions. A cell that is not run is a decision, listed here:
//!
//! | workload | run under | absent, and why |
//! |---|---|---|
//! | W1 multi-block commit | batch, one-per-call, +real-ticks, +cold | evicting: a WRITE reads its path once per apply, so eviction is W2/W3's case; two sessions: sdk#146's mode, not yet in this fixture |
//! | W2 cold wide read | batch+cold, one-per-call+cold, +evicting | warm: nothing is fetched, so it cannot overflow; real ticks: a read is not ticked |
//! | W3 parked then refused | one-per-call+cold | warm: nothing parks (reported NOT REACHED if tried); batch: its answers arrive together, hiding the node-driven verdict |
//! | W4 across a tick, FIRST commit / not first | one-per-call+real-ticks | batch: nothing is in flight across a call; cold/evicting: W1 covers the cold commit |
//! | W5 parity past one return | one-per-call | cold/evicting: parity is put, not read; real ticks: W4 covers ticks during a commit |
//! | W7 a write that emits zero blocks | batch (control), one-per-call, +real-ticks | cold/evicting: the write still reads its path cold, which is W8's case (M1 DoD note 11) |
//! | (all) | -- | two live sessions: needs a second connection whose calls interleave -- sdk#146, added there |
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
    /// TWO LIVE SESSIONS (sdk#146): the workload's writer is one v4 session,
    /// and after each of its calls a second session — speaking v2, a tab
    /// that has not reloaded since an upgrade — sends a tick of its own
    /// through the same delegate.
    two_sessions: bool,
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
        if self.two_sessions {
            p.push("two-sessions")
        }
        p.join("+")
    }
}

const BATCH: Mode = Mode {
    one_per_call: false,
    cold: false,
    evicting: false,
    real_ticks: false,
    two_sessions: false,
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
            // In a two-session cell only the WRITER has writes, so its states
            // are these; the bystander must never be told one (see `Tabs`).
            Reply::SessionWriteState { write_id, state, .. } if write_id == id => Some(state),
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
    } else {
        c.answers_together();
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

/// The two sessions of a `two_sessions` cell (sdk#146).
const WRITER: u64 = 0x0000_1111_2222_3333;
const BYSTANDER: u64 = 0x0000_4444_5555_6666;

/// The workload's request — and in a two-session cell, the WRITER's, followed
/// by a tick from a v2 BYSTANDER through the same delegate.
fn send(c: &mut Conn, mode: Mode, r: &Request) -> Vec<Vec<u8>> {
    if !mode.two_sessions {
        return c.client(r);
    }
    let mut out = c.client_as(WRITER, protocol::CURRENT, r);
    out.extend(c.client_as(
        BYSTANDER,
        2,
        &Request::Tick { now: protocol::tick_of(1_790_000_000_000) },
    ));
    out
}

/// In a two-session cell, every write state must be the WRITER's and say so:
/// one unnamed, or naming the bystander, is a state a tab could apply to its
/// own write of the same id.
fn misaddressed(mode: Mode, replies: &[Vec<u8>]) -> Option<String> {
    if !mode.two_sessions {
        return None;
    }
    let bad: Vec<String> = replies
        .iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|r| match r {
            Reply::WriteState { write_id, state } => Some(format!("unnamed w{write_id}:{state:?}")),
            Reply::SessionWriteState { session, write_id, state } if session != WRITER => {
                Some(format!("session {session:#x} w{write_id}:{state:?}"))
            }
            _ => None,
        })
        .collect();
    (!bad.is_empty()).then(|| format!("write states not addressed to the writer: {bad:?}"))
}

/// The assertions every cell makes: on every call it ran, and then that the
/// engine is still open.
fn every_call(c: &mut Conn) -> Option<String> {
    // OPEN AFTERWARDS. Its calls are subject to the two checks below as well.
    let mut after = c.client(&Request::Write {
        write_id: OPEN_AFTERWARDS,
        ops: vec![Op::Put(b"k/open-afterwards".to_vec(), b"x".to_vec())],
    });
    while c.held() > 0 {
        after.extend(c.release_one());
    }
    let open = states_of(&after, OPEN_AFTERWARDS);
    if c.max_stranded() > 0 {
        // WHICH effects, from the per-call report -- a count cannot say.
        let which = c.strand_details();
        return Some(format!(
            "a call stranded {} effect(s) -- lost at the end of the call [{}]",
            c.max_stranded(),
            which.join(" / ")
        ));
    }
    let u = c.unanswered();
    if !u.is_empty() {
        return Some(format!("never answered: {u:?}"));
    }
    (!open.contains(&WriteState::Published))
        .then(|| format!("the engine is CLOSED afterwards: a trailing write was told {open:?}"))
}

/// The trailing write's id, clear of every workload's.
const OPEN_AFTERWARDS: u64 = 900_001;

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
    let mut replies = send(&mut c, mode, &w);
    // Real ticks: time keeps passing while the commit is in flight.
    if mode.real_ticks {
        let base = 1_790_000_000_000u64;
        for k in 1..=5 {
            replies.extend(c.tick_at(base + k * 1000));
        }
    }
    let st = states_of(&replies, 1);
    if let Some(why) = every_call(&mut c) {
        return Cell::Red(format!("{why}; told {st:?}"));
    }
    if let Some(why) = misaddressed(mode, &replies) {
        return Cell::Red(why);
    }
    if !st.contains(&WriteState::Published) {
        return Cell::Red(format!("never published: told {st:?}"));
    }
    Cell::Green
}

/// W7: a write that changes NOTHING -- the tree it asks for is the published
/// one, so it emits zero blocks -- is published at once, with no PUT and no
/// head (sdk#160: it was a commit with no blocks, which no confirmation could
/// ever end, and the engine was `Busy` to everyone after it).
fn w7_zero_block_write(mode: Mode) -> Cell {
    use testkit::full_node::Served;
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.client(&Request::Identity);
    let same = || Request::Write {
        write_id: 0,
        ops: vec![Op::Put(b"k/schema".to_vec(), b"the same bytes".to_vec())],
    };
    let base = 1_790_000_000_000u64;
    let mut replies = Vec::new();
    let mut to_node = (0, 0);
    for id in [1u64, 2] {
        let (puts, heads) = (c.served(Served::Put), c.served(Served::Head));
        let Request::Write { ops, .. } = same() else {
            unreachable!()
        };
        replies.extend(c.client(&Request::Write { write_id: id, ops }));
        if mode.real_ticks {
            for k in 1..=3 {
                replies.extend(c.tick_at(base + (id * 10 + k) * 1000));
            }
        }
        to_node = (c.served(Served::Put) - puts, c.served(Served::Head) - heads);
    }
    let two = states_of(&replies, 2);
    if !states_of(&replies, 1).contains(&WriteState::Published) {
        return Cell::NotReached(format!(
            "the first write never published: {:?}",
            states_of(&replies, 1)
        ));
    }
    if let Some(why) = every_call(&mut c) {
        return Cell::Red(format!("{why}; the no-op was told {two:?}"));
    }
    if two
        != [
            WriteState::Accepted,
            WriteState::Published,
            WriteState::ParityComplete,
        ]
    {
        return Cell::Red(format!("the no-op write was told {two:?}"));
    }
    if to_node != (0, 0) {
        return Cell::Red(format!(
            "the no-op write sent {} PUT(s) and {} head(s)",
            to_node.0, to_node.1
        ));
    }
    Cell::Green
}

/// W2: a COLD read needing more blocks than one return may fetch.
fn w2_cold_wide_read(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.client(&Request::Identity);
    // 300 is over one page (`MAX_PAGE_ENTRIES` 256), so the reader follows
    // the cursor, as a client does: the claim is 300 of 300, not one page.
    let mut after: Option<Vec<u8>> = None;
    let mut rows: Vec<Vec<u8>> = Vec::new();
    let mut pages: Vec<usize> = Vec::new();
    for req_id in 1..=4u64 {
        let r = c.client(&Request::Range {
            req_id,
            lo: Bound::Unbounded,
            hi: Bound::Unbounded,
            reverse: false,
            after: after.clone(),
            max_entries: 300 - rows.len() as u32,
        });
        let page = r
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .find_map(|x| match x {
                Reply::Page {
                    req_id: id,
                    entries,
                    cursor,
                    ..
                } if id == req_id => Some((entries, cursor)),
                _ => None,
            });
        let Some((entries, cursor)) = page else { break };
        pages.push(entries.len());
        rows.extend(entries.into_iter().map(|(k, _)| k));
        if cursor.is_none() || rows.len() >= 300 {
            break;
        }
        after = cursor;
    }
    if let Some(why) = every_call(&mut c) {
        return Cell::Red(format!("{why}; pages {pages:?}"));
    }
    let distinct: std::collections::BTreeSet<&Vec<u8>> = rows.iter().collect();
    if rows.len() == 300 && distinct.len() == 300 {
        Cell::Green
    } else {
        Cell::Red(format!(
            "the cold read returned {} rows ({} distinct) of 300 in pages {pages:?}",
            rows.len(),
            distinct.len()
        ))
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
    let mut all = Vec::new();
    for _ in 0..6 {
        let replies = send(&mut c, mode, &Request::Write { write_id: 1, ops: ops.clone() });
        let got = states_of(&replies, 1);
        all.extend(replies);
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
    if let Some(why) = every_call(&mut c) {
        return Cell::Red(format!("{why}; told {st:?}"));
    }
    if let Some(why) = misaddressed(mode, &all) {
        return Cell::Red(why);
    }
    match st.last() {
        Some(WriteState::TooLarge { .. }) => Cell::Green,
        other => Cell::Red(format!(
            "a v{} client was told {other:?} after the park, not TooLarge (all: {st:?})",
            protocol::CURRENT
        )),
    }
}

/// W4: a commit IN FLIGHT across a real tick -- one second old must not be
/// `Stalled` (the bound is 64 ticks), it must publish, and its PARITY must be
/// written: `ParityComplete`, with the node's PUT count moving after the
/// publish. "Published" alone is not redundancy (M1 DoD A0).
///
/// Two arms. FIRST commit ever: its parity used to go out `after` the empty
/// tree's root, which nobody confirms, so it was held, dropped with the call
/// and -- recorded as `sent` -- never asked for again (sdk#150 PR 3). NOT
/// first: a commit already published before it.
fn w4_commit_across_a_tick(mode: Mode, first: bool) -> Cell {
    use testkit::full_node::Served;
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    let base = 1_790_000_000_000u64;
    let mut k = 1;
    let mut replies = Vec::new();
    c.client(&Request::Identity);
    if !first {
        let r = c.client(&Request::Write {
            write_id: 9,
            ops: vec![Op::Put(b"k/before".to_vec(), value(3, 30 * 1024))],
        });
        if !states_of(&r, 9).contains(&WriteState::Published) {
            return Cell::NotReached(format!(
                "the earlier commit never published: {:?}",
                states_of(&r, 9)
            ));
        }
        replies.extend(r);
    }
    c.hold_answers();
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
    while c.held() > 0 && k < 40 {
        replies.extend(c.tick_at(base + k * 1000));
        replies.extend(c.release_one());
        k += 1;
    }
    // THE CRITERION, explicit: a commit younger than `max_accept_age` ticks
    // is not `Stalled`.
    let bound = engine::Params::default().max_accept_age;
    assert!(
        k - 1 < bound,
        "the workload outgrew its own premise: {} ticks >= {bound}",
        k - 1
    );
    let published_at = c.served(Served::Put);
    // Time after the publish. TWO ticks before each tick's answers are
    // delivered, so an engine that re-asked what is merely unanswered -- a
    // re-put on every call -- shows here as more PUTs (review of #167).
    for j in 0..10 {
        replies.extend(c.tick_at(base + (k + 2 * j) * 1000));
        replies.extend(c.tick_at(base + (k + 2 * j + 1) * 1000));
        while c.held() > 0 {
            replies.extend(c.release_one());
        }
    }
    let parity_puts = c.served(Served::Put) - published_at;
    // THE UPPER BOUND: this commit's two keys code ONE parity group, three
    // blocks, and nothing is unanswered for `reask_after` ticks here.
    if parity_puts > 3 {
        return Cell::Red(format!(
            "{parity_puts} PUTs after the publish for ONE parity group (3 blocks): re-asked what was only unanswered"
        ));
    }
    let st = states_of(&replies, 1);
    if let Some(why) = every_call(&mut c) {
        return Cell::Red(format!("{why}; told {st:?}"));
    }
    if st.contains(&WriteState::Stalled) {
        return Cell::Red(format!(
            "told Stalled within {} s, under max_accept_age ({bound} ticks): {st:?}",
            k - 1
        ));
    }
    if !st.contains(&WriteState::Published) {
        return Cell::Red(format!("never published: {st:?}"));
    }
    if !st.contains(&WriteState::ParityComplete) || parity_puts == 0 {
        return Cell::Red(format!(
            "published without redundancy: {parity_puts} PUT(s) after the publish; told {st:?}"
        ));
    }
    Cell::Green
}

fn w4_first(mode: Mode) -> Cell {
    w4_commit_across_a_tick(mode, true)
}

fn w4_not_first(mode: Mode) -> Cell {
    w4_commit_across_a_tick(mode, false)
}

/// How many writes W5 makes before its flush: enough that the flush needs
/// more parity puts than one return carries (measured: 12 writes gave 45).
const W5_WRITES: u64 = 40;

/// W5: parity for more groups than one return may put.
fn w5_parity_overflow(mode: Mode) -> Cell {
    let node = node_for(mode);
    let mut c = connect(&node, mode);
    c.client(&Request::Identity);
    for w in 0..W5_WRITES {
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
    }
    let puts_before = c.served(testkit::full_node::Served::Put);
    let mut r = c.client(&Request::Flush);
    let flush_puts = c.served(testkit::full_node::Served::Put) - puts_before;
    // TIME after the flush, past `reask_after`: what a return could not
    // carry is asked again (PR 3), so "re-asked, not lost" is SHOWN here --
    // every write's ParityComplete, and the PUTs it took -- not stated.
    let base = 1_790_000_000_000u64;
    let ticks = 3 * engine::Params::default().reask_after;
    for k in 1..=ticks {
        r.extend(c.tick_at(base + k * 1000));
        while c.held() > 0 {
            r.extend(c.release_one());
        }
    }
    let parity_puts = c.served(testkit::full_node::Served::Put) - puts_before;
    let complete = (100..100 + W5_WRITES)
        .filter(|w| states_of(&r, *w).contains(&WriteState::ParityComplete))
        .count();
    let eventually = format!(
        "after {ticks} ticks: {parity_puts} parity PUTs, ParityComplete for {complete} of {W5_WRITES} writes"
    );
    // Strands FIRST. What was SERVED can never exceed the per-return cap the
    // cell exists to test -- a flush that emitted 200 serves exactly 128 --
    // so "fit in one return" is only believed when nothing was stranded.
    if let Some(why) = every_call(&mut c) {
        return Cell::Red(format!("{why}; flush put {flush_puts}; {eventually}"));
    }
    if flush_puts < 128 {
        return Cell::NotReached(format!(
            "the flush put {flush_puts} parity blocks, under one return (128)"
        ));
    }
    if (complete as u64) < W5_WRITES {
        return Cell::Red(format!(
            "not every write reached ParityComplete: {eventually}"
        ));
    }
    Cell::Green
}

/// A workload: its name, the function that runs one cell, and the modes it
/// runs under.
type Workload = (&'static str, fn(Mode) -> Cell, Vec<Mode>);

/// The cells red on the code as it stands, each with the defect it shows.
const KNOWN_RED: &[(&str, &str, &str)] = &[
    // THE HEAD BUMP (F2), THE CLOCK and FIRST-COMMIT PARITY are fixed: W1
    // and both W4 arms are green, W4 asserting ParityComplete and parity PUTs.
    // THE READ OVERFLOW is fixed: the engine never asks for more fetches in
    // a round than one return carries (`max_fetch_per_round` <= `max_gets`,
    // refused at the shell's construction). W2's two cold cells are green.
    // THE EVICTION PING-PONG: `arrived` lasts one call and the node keeps
    // nothing it served, so a read needing two such blocks never finishes.
    (
        "W2 cold read wider than one return",
        "one-per-call+cold+evicting",
        "eviction ping-pong (cycle)",
    ),
    // The parked write's fetches strand: `park_write` asks for its whole
    // path, which `max_fetch_per_round` does not bound (the limits PR). The verdict's
    // VERSION is no longer a reason: sdk#146 carries it with the write, and
    // engine-delegate's `a_write_refused_after_a_park_is_told_in_its_clients_
    // version_whoever_else_speaks` proves it with the GET limit lifted.
    (
        "W3 parked write refused, told why",
        "one-per-call+cold",
        "parked write's fetches stranded: park_write asks for its whole path, which max_fetch_per_round does not bound -- sdk#150's LIMITS PR owns it",
    ),
    (
        "W3 parked write refused, told why",
        "one-per-call+cold+two-sessions",
        "parked write's fetches stranded: park_write asks for its whole path, which max_fetch_per_round does not bound -- sdk#150's LIMITS PR owns it",
    ),
    // The flush asks for more parity than one return carries. Since PR 3 an
    // ask that strands is asked again after `reask_after` ticks, so this is
    // no longer a LOSS -- but a strand is still a strand, and the engine
    // learning the shell's limits is PR 5.
    (
        "W5 parity wider than one return",
        "one-per-call",
        "A0.1 fails: the flush strands 4 parity puts past max_puts 128 (the engine's max_asks 132 is not told the shell's limit -- PR 5). What holds, printed in the cell: re-asked after reask_after, every write ParityComplete",
    ),
];

#[test]
fn every_workload_under_every_mode() {
    let workloads: [Workload; 7] = [
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
                Mode {
                    two_sessions: true,
                    ..APC
                },
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
            vec![
                Mode { cold: true, ..APC },
                Mode {
                    cold: true,
                    two_sessions: true,
                    ..APC
                },
            ],
        ),
        (
            "W4 across a tick, FIRST commit",
            w4_first,
            vec![Mode {
                real_ticks: true,
                ..APC
            }],
        ),
        (
            "W4 across a tick, not first",
            w4_not_first,
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
        (
            "W7 a write that emits zero blocks",
            w7_zero_block_write,
            vec![
                BATCH,
                APC,
                Mode {
                    real_ticks: true,
                    ..APC
                },
            ],
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
