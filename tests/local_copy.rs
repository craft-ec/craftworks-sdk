//! This client's OWN unsettled writes (`copy::Copy`): what each key's last
//! write is doing, what falls with a failure, what times out, and the caps.
//!
//! There are no rows here (READ-STATE, design B): reads walk the tree. What
//! the copy says about a key is only "a write of mine is still pending here",
//! and `None` when none is — which is why a published or rolled-back write
//! leaves nothing behind.

use craftworks_sdk::copy::{Copy, Refused, RolledBack};

fn v(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// The last pending write is what shows.
// ---------------------------------------------------------------------------

#[test]
fn a_pending_write_shows_and_says_it_is_pending_until_it_is_published() {
    let mut c = Copy::new();
    c.write(b"a/1", Some(v("mine")), 1, 0).expect("under the cap");

    let seen = c.get(b"a/1").expect("a write is pending");
    assert_eq!(seen.value(), Some(&b"mine"[..]));
    assert!(
        seen.is_pending(),
        "an unacknowledged value did not say it was unacknowledged — a row \
         showing it would claim the data is saved"
    );

    // Published: the tree has it now, and the copy keeps nothing.
    c.published(1);
    assert_eq!(c.get(b"a/1"), None, "a published write was still held as pending");
}

#[test]
fn the_last_pending_write_is_what_shows_and_the_ones_under_it_survive() {
    let mut c = Copy::new();
    c.write(b"a/1", Some(v("first")), 1, 0).expect("under the cap");
    c.write(b"a/1", Some(v("second")), 2, 0).expect("under the cap");
    assert_eq!(c.get(b"a/1").unwrap().value(), Some(&b"second"[..]));

    // The FIRST publishes. The second is still pending and still on top —
    // which is why this is a list and not one value.
    c.published(1);
    let seen = c.get(b"a/1").unwrap();
    assert_eq!(seen.value(), Some(&b"second"[..]), "{seen:?}");
    assert!(seen.is_pending(), "{seen:?}");
    assert_eq!(c.pending_ids(), vec![2]);
}

// ---------------------------------------------------------------------------
// A failure takes everything behind it. Blind writes, phase 3.
// ---------------------------------------------------------------------------

/// w1 fails with w2 and w3 queued on the same key: all three go, other keys
/// are untouched, and each write id is named.
#[test]
fn a_failed_write_invalidates_every_later_write_on_that_key() {
    let mut c = Copy::new();
    c.write(b"a/1", Some(v("w1")), 1, 0).expect("under the cap");
    c.write(b"a/1", Some(v("w2")), 2, 0).expect("under the cap");
    c.write(b"a/1", Some(v("w3")), 3, 0).expect("under the cap");
    c.queued(3);
    // A write on ANOTHER key, which must survive.
    c.write(b"a/2", Some(v("elsewhere")), 4, 0).expect("under the cap");

    let told = c.failed(1);
    assert_eq!(
        told.rolled_back,
        vec![(1, RolledBack::Failed), (2, RolledBack::AfterFailed), (3, RolledBack::AfterFailed),],
        "each write id must be named, and WHY — the app redoes them"
    );

    // Nothing of mine is pending on the key any more: a read of it is the
    // tree's. Not w1, and not w2 or w3.
    assert_eq!(c.get(b"a/1"), None, "a fallen write is still shown");
    // The other key is untouched.
    let other = c.get(b"a/2").unwrap();
    assert_eq!(other.value(), Some(&b"elsewhere"[..]));
    assert!(other.is_pending());
    assert_eq!(c.pending_ids(), vec![4]);
}

/// THE CONTROL: without the cascade, w2's value survives its predecessor's
/// failure — a value that was never anywhere, on screen, with nothing to
/// reveal it.
#[test]
fn rolling_back_only_the_failed_write_leaves_a_fabricated_value() {
    let mut c = Copy::new();
    c.write(b"a/1", Some(v("w1")), 1, 0).expect("under the cap");
    c.write(b"a/1", Some(v("w2")), 2, 0).expect("under the cap");

    // The state a one-entry design would leave: w1 dropped, w2 kept. Made
    // through the real API by failing w1 and re-applying w2.
    c.failed(1);
    c.write(b"a/1", Some(v("w2")), 2, 0).expect("under the cap");

    assert_eq!(
        c.get(b"a/1").unwrap().value(),
        Some(&b"w2"[..]),
        "the control did not reproduce the fabricated value, so the test \
         above is not showing that the cascade prevents one"
    );
}

// ---------------------------------------------------------------------------
// The timeout: the one permanent divergence.
// ---------------------------------------------------------------------------

/// A write with no verdict is rolled back, so no tab shows a phantom for ever.
#[test]
fn a_pending_write_with_no_verdict_times_out_and_rolls_back() {
    let mut c = Copy::new();
    c.pending_timeout_ms = 1_000;
    c.write(b"a/1", Some(v("phantom")), 1, 0).expect("under the cap");

    // Before the timeout: still showing, still pending.
    let told = c.time_out(999);
    assert!(told.is_empty(), "rolled back early: {told:?}");
    assert_eq!(c.get(b"a/1").unwrap().value(), Some(&b"phantom"[..]));

    // After: gone, and the app is told which write and why.
    let told = c.time_out(1_000);
    assert_eq!(told.rolled_back, vec![(1, RolledBack::Unknown)]);
    assert_eq!(c.get(b"a/1"), None, "the phantom is still pending");
}

/// THE CONTROL: with the timeout off, the phantom survives for ever.
///
/// An accepted write can be lost with the engine's context (F32). Without the
/// timeout one tab shows a value no other tab will ever see, and nothing in
/// the system ever corrects it — the one permanent divergence.
#[test]
fn with_the_timeout_off_the_phantom_survives_for_ever() {
    let mut c = Copy::new();
    c.pending_timeout_ms = u64::MAX;
    c.write(b"a/1", Some(v("phantom")), 1, 0).expect("under the cap");

    // A year later.
    let told = c.time_out(365 * 24 * 60 * 60 * 1000);
    assert!(
        told.is_empty(),
        "the control rolled something back, so it is not showing what the \
         timeout prevents: {told:?}"
    );
    assert_eq!(
        c.get(b"a/1").unwrap().value(),
        Some(&b"phantom"[..]),
        "the control did not keep the phantom, so the test above does not \
         demonstrate that the timeout is what removes it"
    );
    assert_eq!(c.pending_ids(), vec![1]);
}

// ---------------------------------------------------------------------------
// INTERIM (removed by R-b, READ-STATE § queue): the overlay a read lays over
// its walk is ONLY what the engine has not taken.
// ---------------------------------------------------------------------------

/// A write the engine accepted is in the warm root; a second copy of it here
/// would be two copies of one value. Only a write HELD by the window, or
/// QUEUED after `Busy`, is in no root — and only those are overlaid. Each
/// leaves the overlay on the door's verdict.
#[test]
fn interim_the_overlay_is_only_the_writes_the_engine_has_not_taken() {
    let mut c = Copy::new();
    c.write(b"a/1", Some(v("taken")), 1, 0).expect("under the cap");
    c.sent(1, 0);
    c.submitted(1);
    c.write(b"a/2", Some(v("held")), 2, 0).expect("under the cap");
    c.hold(2);
    c.write(b"a/3", None, 3, 0).expect("under the cap");
    c.sent(3, 0);
    c.queued(3);

    assert_eq!(
        c.unaccepted(b"a/", b"b/"),
        vec![(v("a/2"), Some(v("held"))), (v("a/3"), None)],
        "the overlay must be exactly the held and the Busy-queued writes, a delete as None"
    );
    assert!(c.any_unaccepted());

    // The held write goes out and is taken: it leaves the overlay.
    c.sent(2, 0);
    c.submitted(2);
    // The queued one is refused for good: it falls, and leaves too.
    c.failed(3);
    assert_eq!(c.unaccepted(b"a/", b"b/"), Vec::new());
    assert!(!c.any_unaccepted(), "nothing is held or queued, and yet something is overlaid");
}

// ---------------------------------------------------------------------------
// Pending is bounded: its own caps.
// ---------------------------------------------------------------------------

/// At the write cap a write is REFUSED, by name, and leaves no trace.
#[test]
fn past_the_pending_cap_a_write_is_refused_and_changes_nothing() {
    let mut c = Copy::new();
    c.max_pending = 3;
    for i in 0..3u64 {
        c.write(format!("a/{i}").as_bytes(), Some(v("x")), i, 0).expect("under the cap");
    }
    assert_eq!(c.pending().0, 3);

    let refused = c.write(b"a/9", Some(v("over")), 9, 0).unwrap_err();
    assert_eq!(refused, Refused::TooManyPending { cap: 3 }, "the refusal must name WHICH cap it hit");
    assert!(refused.to_string().contains("waiting for an answer"), "unhelpful refusal: {refused}");

    // NO TRACE. A write half-applied and then reported refused is worse than
    // either outcome: the app is told nothing happened while something did.
    assert_eq!(c.get(b"a/9"), None, "the refused write left something behind");
    assert_eq!(c.pending().0, 3, "the refused write was counted");
    assert!(!c.pending_ids().contains(&9));
}

/// THE CONTROL: one under the cap goes through, so the refusal above is the
/// cap firing rather than writes failing generally.
#[test]
fn below_the_pending_cap_nothing_changes() {
    let mut c = Copy::new();
    c.max_pending = 3;
    for i in 0..2u64 {
        c.write(format!("a/{i}").as_bytes(), Some(v("x")), i, 0).expect("under the cap");
    }
    c.write(b"a/2", Some(v("third")), 2, 0).expect("the third write is AT the cap, not over it");
    assert_eq!(c.pending().0, 3);
    assert_eq!(c.get(b"a/2").unwrap().value(), Some(&b"third"[..]));
}

/// The BYTE cap fires on its own, where the write cap would not have.
///
/// A count is not a byte budget — ten writes of a megabyte are not ten small
/// ones — so the two bounds are tested where each is the tighter one. A cap
/// only ever reached through the other is a cap nobody has tested.
#[test]
fn the_byte_cap_refuses_where_the_write_cap_would_not_have() {
    let mut c = Copy::new();
    c.max_pending = 1000;
    c.max_pending_bytes = 100;

    c.write(b"a/1", Some(vec![b'x'; 60]), 1, 0).expect("under both caps");
    let refused = c.write(b"a/2", Some(vec![b'x'; 60]), 2, 0).unwrap_err();
    assert_eq!(
        refused,
        Refused::TooManyPendingBytes { cap: 100 },
        "two writes is nowhere near the WRITE cap of 1000, so this must be \
         the byte cap or the byte cap is unreachable"
    );
    assert_eq!(c.pending().0, 1, "the refused write was counted anyway");
}

/// Settling a write gives its room back.
#[test]
fn a_settled_write_frees_its_place_at_the_cap() {
    let mut c = Copy::new();
    c.max_pending = 2;
    c.write(b"a/1", Some(v("a")), 1, 0).expect("under the cap");
    c.write(b"a/2", Some(v("b")), 2, 0).expect("under the cap");
    assert!(c.write(b"a/3", Some(v("c")), 3, 0).is_err());

    c.published(1);
    c.write(b"a/3", Some(v("c")), 3, 0).expect("a published write must give its place back");

    c.failed(2);
    c.write(b"a/4", Some(v("d")), 4, 0).expect("a failed write must give its place back too");
}
