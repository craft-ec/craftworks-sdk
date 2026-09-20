//! The fixtures are load-bearing, so they get their own tests.
//!
//! A fixture that silently stopped rehydrating, or a clock that silently
//! stopped moving, would not fail anything — it would make a whole class of
//! defect invisible again, which is the failure this crate exists to end.

use instrument::Record;
use testkit::{Clock, DumpOnPanic, Node};

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
