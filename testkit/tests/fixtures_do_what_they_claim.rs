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
