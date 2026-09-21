//! The fixtures are load-bearing, so they get their own tests.
//!
//! A fixture that silently stopped rehydrating, or a clock that silently
//! stopped moving, would not fail anything — it would make a whole class of
//! defect invisible again, which is the failure this crate exists to end.

use instrument::Record;
use testkit::{Clock, DumpOnPanic, Node, Served};

/// The clock MOVES. The thing it replaces — `Box::new(|| 0)`, written by hand
/// in six files — could not, and nothing in the SDK's tests could observe time
/// passing as a result.
#[test]
fn the_clock_can_be_driven() {
    let c = Clock::new(1_000);
    assert_eq!(c.now_ms(), 1_000);
    c.advance(500);
    assert_eq!(c.now_ms(), 1_500);

    // The closure handed to a constructor sees the SAME clock, not a copy
    // frozen at construction — which is the mistake that would reintroduce
    // `|| 0` under a different name.
    let f = c.as_fn();
    assert_eq!(f(), 1_500);
    c.advance(250);
    assert_eq!(
        f(),
        1_750,
        "the closure reads the live clock, not a snapshot"
    );
}

/// The node REHYDRATES on every call.
///
/// This is the property that makes the fixture worth having: four defects in
/// this repo lived in what the context does not carry, and only a driver that
/// rebuilds from the context can see them.
#[test]
fn every_call_rebuilds_the_shell_from_its_context() {
    let mut node = Node::new();
    let _ = node.step(Vec::new());
    let after_first = node.context_len();
    let _ = node.step(Vec::new());

    assert!(
        after_first > 0,
        "a call must leave a context behind: {}",
        node.line()
    );
    assert_eq!(
        node.recording().unfinished().len(),
        0,
        "every call ended: {}",
        node.line()
    );
}

/// The recording explains a failure without a re-run.
#[test]
fn a_failing_test_has_its_story_already_recorded() {
    let mut node = Node::new();
    for _ in 0..3 {
        let _ = node.step(Vec::new());
    }
    let guard = DumpOnPanic {
        node: &node,
        what: "what happened",
    };
    let dump = node.dump("what happened");
    assert!(
        dump.contains("testkit::node"),
        "the dump names the site: {dump}"
    );
    assert!(
        node.line().contains("calls 3"),
        "and the line is a projection of the same stream: {}",
        node.line()
    );
    drop(guard);
}

/// The bare path exists, is named to be noticed, and is not what `new` gives.
#[test]
fn the_unprobed_constructor_is_the_awkward_one() {
    let probed = Node::new();
    let bare = Node::new_unprobed_for_benchmark();
    assert_eq!(probed.puts(), 0);
    assert_eq!(bare.puts(), 0);
}

/// The per-call node-op counts are the SHELL's, recorded on the client side.
///
/// They are counted where the delegate already knows them, which is the only
/// place they can be known honestly: six external instruments failed to infer
/// a PUT's cost from outside (freenet-contracts#39), because an interface
/// carries the whole machine and a rate counter's baseline varies by more than
/// the signal. The counter belongs inside the thing being measured — and the
/// RING belongs to the client, never to the delegate's context or its secret
/// store.
#[test]
fn a_call_reports_the_bytes_it_handed_to_the_node() {
    use instrument::{vocab::Key, Event};

    let mut node = Node::new();
    let _ = node.step(Vec::new());

    // Whatever the call did, the recording's BytesOut must equal the bytes the
    // shell actually put — asserted against the events rather than against a
    // second tally, because a second tally is what disagrees.
    let from_events: u64 = node
        .recording()
        .events()
        .iter()
        .filter_map(|e| match e {
            Event::Counter { entry, .. } if entry.key == Key::BytesOut => Some(entry.value),
            _ => None,
        })
        .sum();
    // Two views of the same value agreeing. Kept — it would catch the
    // accessor and the recording drifting apart — but it CANNOT catch the
    // measurement being wrong: zero equals zero. The core dev zeroed
    // `put_bytes += bytes.len()` and this stayed green.
    assert_eq!(
        node.put_bytes(),
        from_events,
        "the accessor and the recording are the same number: {}",
        node.line()
    );

    // The expectation that makes it falsifiable, computed from what this node
    // ACTUALLY RECEIVED rather than from the shell's own tally.
    assert_eq!(
        node.put_bytes(),
        node.bytes_handed_to_this_node(),
        "the shell's count must equal the bytes this node was handed: {}",
        node.dump("byte count")
    );

    // Stranded must be zero — it is recorded rather than asserted inside the
    // shell so a non-zero one is visible without arithmetic, and this is where
    // it is checked.
    let stranded: u64 = node
        .recording()
        .events()
        .iter()
        .filter_map(|e| match e {
            Event::Counter { entry, .. } if entry.key == Key::Stranded => Some(entry.value),
            _ => None,
        })
        .sum();
    assert_eq!(
        stranded,
        0,
        "effects left queued when a call ended are LOST: {}",
        node.dump("stranded")
    );
}

/// A call that ACTUALLY WRITES, so the byte count has something to be wrong
/// about.
///
/// The test above drives an empty call, where the shell hands the node nothing
/// and every count is zero — so zeroing the measurement left it green. A cost
/// counter needs a case where the number is NOT zero and an expectation
/// computed without the code under test.
#[test]
fn the_byte_count_is_pinned_to_what_the_node_actually_received() {
    let mut node = Node::new();
    let _ = node.client(&protocol::Request::Write {
        write_id: 1,
        ops: vec![protocol::Op::Put(b"k".to_vec(), vec![7u8; 900])],
    });

    let handed = node.bytes_handed_to_this_node();
    assert!(
        handed > 0,
        "the fixture must actually hand the node blocks, or this proves nothing: {}",
        node.line()
    );
    assert_eq!(
        node.put_bytes(),
        handed,
        "the shell's count must equal what this node received: {}",
        node.dump("byte count")
    );
    assert!(
        node.blocks_handed_to_this_node() > 0,
        "and blocks, not just bytes: {}",
        node.line()
    );
}

/// The shell DETECTS which version a client spoke, and does not assume one.
///
/// This is the input to the gate that decides whether a v2-only message may be
/// sent (`entry.rs`: `if out.client_version >= 2`). A reply carries no version
/// of its own — there is no envelope on the reply side — so the envelope the
/// client sent on the way IN is the only thing that knows what the far side
/// can read. Dropping it, which `serve()` used to do, is exactly what would
/// let a v2-only message reach a v1 reader.
///
/// The other half — that an unknown variant is REFUSED rather than misread if
/// one ever does arrive — is proved in `protocol/tests/mixed_versions.rs`.
#[test]
fn the_shell_detects_the_version_a_client_spoke() {
    let write = || protocol::Request::Write {
        write_id: 1,
        ops: vec![protocol::Op::Put(b"k".to_vec(), vec![7u8; 900])],
    };

    let mut v2 = Node::new();
    let _ = v2.step(vec![engine_delegate::shell::Inbound::Client(
        // At CURRENT a frame carries a session (sdk#146).
        protocol::encode_session_request(protocol::CURRENT, protocol::mint_session(5), &write()).expect("encodes"),
    )]);
    assert_eq!(
        v2.detected_client_version(),
        protocol::CURRENT,
        "a client speaking the current version must be detected as such"
    );

    let mut v1 = Node::new();
    let _ = v1.step(vec![engine_delegate::shell::Inbound::Client(
        protocol::encode_request(1, &write()).expect("encodes"),
    )]);
    assert_eq!(
        v1.detected_client_version(),
        1,
        "a v1 client must be detected as v1, not assumed to be current"
    );

    // And silence is not consent to a format: a call with no client message at
    // all detects nothing, so a v2-only message is not sent then either.
    let mut quiet = Node::new();
    let _ = quiet.step(Vec::new());
    assert_eq!(quiet.detected_client_version(), 0);
}

// ---------------------------------------------------------------------------
// The full-node fixture (sdk#94)
// ---------------------------------------------------------------------------

fn a_write(n: u64) -> protocol::Request {
    protocol::Request::Write {
        write_id: n,
        ops: vec![protocol::Op::Put(
            format!("k/{n:06}").into_bytes(),
            vec![(n % 251) as u8; 512],
        )],
    }
}

/// THE test that decides whether this fixture is worth having.
///
/// A full-node fixture that only ever served puts would be the put-only
/// fixture wearing a new name, and no assertion about a reply's CONTENT can
/// tell the difference — a test can pass while three of the four op kinds are
/// silently dropped, which is exactly the state that made three files
/// hand-roll their own node.
///
/// So this counts the kinds. A put-only fixture scores 1 and fails on the
/// first assertion below.
#[test]
fn the_full_node_answers_more_than_puts() {
    let node = testkit::FullNode::new();

    // A write drives Put, Head and ReadHead.
    let mut w = node.connect();
    w.client(&protocol::Request::Identity);
    for i in 1..=4 {
        w.client(&a_write(i));
    }

    assert!(w.served(Served::Put) > 0, "no put was served");
    assert!(
        w.served(Served::Head) > 0,
        "the head was never written: a commit that never publishes looks like \
         a write that worked"
    );
    assert!(
        w.served(Served::ReadHead) > 0,
        "the head was never read back"
    );
    assert!(
        w.kinds_served() >= 3,
        "a writer drove only {} kind(s) — a put-only fixture scores 1, and \
         that is the thing this fixture exists not to be",
        w.kinds_served()
    );

    // `Op::Get` is the one a writer never drives: the delegate reads blocks
    // the node HOLDS straight through, and only ASKS for one it does not have.
    // That is the cold case — a head naming a root nobody holds.
    let cold = testkit::FullNode::new();
    cold.set_head(1, [9u8; 32]);
    let mut r = cold.connect();
    r.client(&protocol::Request::Identity);
    r.client(&protocol::Request::Get {
        req_id: 1,
        key: b"anything".to_vec(),
    });

    assert!(
        r.served(Served::Get) > 0,
        "the block was not held and the node was never asked for it: a cold \
         read that silently answers nothing is the defect, not the test"
    );
    assert!(
        r.replies()
            .iter()
            .any(|x| matches!(x, protocol::Reply::Unavailable { .. })),
        "a missing root must come back as Unavailable, naming what it is \
         blocked on: {:?}",
        r.replies()
    );

    // All four, across the two shapes. The put-only fixture serves one.
    assert_eq!(
        w.served(Served::Get) + r.served(Served::Get),
        r.served(Served::Get),
        "sanity: only the cold reader should drive a Get"
    );
}

/// One node, two clients: what one writes, the other reads.
///
/// The shape `tests/two_clients.rs` needed. Each connection keeps its own
/// delegate context; the blocks and the head are the node's.
#[test]
fn two_connections_share_one_node() {
    let node = testkit::FullNode::new();
    let mut a = node.connect();
    let mut b = node.connect();

    a.client(&protocol::Request::Identity);
    a.client(&a_write(1));
    let head_after_write = node.head();
    assert!(head_after_write.is_some(), "the writer published a head");

    b.client(&protocol::Request::Identity);
    b.client(&protocol::Request::Get {
        req_id: 7,
        key: b"k/000001".to_vec(),
    });

    let found = b.replies().iter().any(
        |x| matches!(x, protocol::Reply::Value { req_id: 7, value: Some(v) } if v.len() == 512),
    );
    assert!(
        found,
        "the second client did not see the first client's write: {:?}",
        b.replies()
    );

    // Separate contexts, one node.
    assert!(a.context_len() > 0 && b.context_len() > 0);
    assert_eq!(b.served(Served::Put), 0, "the reader wrote nothing");
}

/// The full node REHYDRATES too.
///
/// The put-only fixture's headline property, and it would be quietly lost by a
/// second fixture that kept a shell alive across calls.
#[test]
fn the_full_node_rebuilds_the_shell_from_its_context() {
    let node = testkit::FullNode::new();
    let mut c = node.connect();
    c.client(&protocol::Request::Identity);
    let after_first = c.context_len();
    assert!(
        after_first > 0,
        "nothing was carried between calls, so nothing can be lost between \
         them either — which is the defect class this fixture exists to see"
    );

    c.client(&a_write(1));
    assert!(node.blocks() > 0, "the write reached the node's store");
}
