//! The browser's local copy: what it shows, and what it refuses to claim.
//!
//! The copy is a CACHE. It resolves nothing, it is correct when empty, and the
//! interesting cases are all about what it says when it does not know — which
//! is why most of these tests are about absence rather than about values.

use craftworks_sdk::copy::{Copy, RolledBack, Visible};

fn v(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}
fn root(n: u8) -> [u8; 32] {
    [n; 32]
}

/// A copy with `a/1..a/3` loaded at root 1.
fn loaded() -> Copy {
    let mut c = Copy::new();
    c.loaded_range(
        b"a/",
        b"b/",
        vec![(v("a/1"), v("one")), (v("a/2"), v("two"))],
        root(1),
    );
    c
}

// ---------------------------------------------------------------------------
// "Not loaded" is not "empty" — the distinction the whole structure is for.
// ---------------------------------------------------------------------------

#[test]
fn a_range_that_was_never_bound_says_it_does_not_know() {
    let c = loaded();

    // Inside the loaded range, absence is a FACT.
    assert_eq!(
        c.get(b"a/9"),
        Some(Visible::Clean(None)),
        "a key inside a loaded range must be reported absent, not unknown"
    );
    assert_eq!(c.range(b"a/", b"b/").map(|r| r.len()), Some(2));

    // Outside it, absence is IGNORANCE, and saying "empty" would make a
    // component show nothing for data that exists.
    assert_eq!(
        c.get(b"z/1"),
        None,
        "a key in a range nobody loaded was reported as absent"
    );
    assert_eq!(
        c.range(b"z/", b"z0"),
        None,
        "an unloaded range answered as an EMPTY range"
    );

    // A range that only PARTLY overlaps what is loaded is not loaded either —
    // answering with the part that is held would silently truncate.
    assert_eq!(
        c.range(b"a/", b"c/"),
        None,
        "a range half-covered by loaded intervals answered anyway"
    );
}

#[test]
fn two_abutting_loads_cover_the_span_between_them() {
    let mut c = Copy::new();
    c.loaded_range(b"a/", b"b/", vec![(v("a/1"), v("x"))], root(1));
    assert_eq!(c.range(b"a/", b"c/"), None, "not loaded yet");
    c.loaded_range(b"b/", b"c/", vec![(v("b/1"), v("y"))], root(1));
    let rows = c
        .range(b"a/", b"c/")
        .expect("two abutting loads must cover the whole span");
    assert_eq!(rows.len(), 2, "{rows:?}");
}

// ---------------------------------------------------------------------------
// visible = base ⊕ pending-in-order
// ---------------------------------------------------------------------------

#[test]
fn a_pending_write_shows_on_top_of_base_and_says_it_is_pending() {
    let mut c = loaded();
    c.write(b"a/1", Some(v("mine")), 1, 0);

    let seen = c.get(b"a/1").expect("loaded");
    assert_eq!(seen.value(), Some(&b"mine"[..]));
    assert!(
        seen.is_pending(),
        "an unacknowledged value did not say it was unacknowledged — a row \
         showing it would claim the data is saved"
    );

    // Published: it becomes base, and stops being pending.
    c.published(1);
    let seen = c.get(b"a/1").expect("loaded");
    assert_eq!(seen, Visible::Clean(Some(v("mine"))));
}

#[test]
fn the_last_pending_write_is_what_shows_and_the_ones_under_it_survive() {
    let mut c = loaded();
    c.write(b"a/1", Some(v("first")), 1, 0);
    c.write(b"a/1", Some(v("second")), 2, 0);
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
    let mut c = loaded();
    c.write(b"a/1", Some(v("w1")), 1, 0);
    c.write(b"a/1", Some(v("w2")), 2, 0);
    c.write(b"a/1", Some(v("w3")), 3, 0);
    c.queued(3);
    // A write on ANOTHER key, which must survive.
    c.write(b"a/2", Some(v("elsewhere")), 4, 0);

    let told = c.failed(1);
    assert_eq!(
        told.rolled_back,
        vec![
            (1, RolledBack::Failed),
            (2, RolledBack::AfterFailed),
            (3, RolledBack::AfterFailed),
        ],
        "each write id must be named, and WHY — the app redoes them"
    );

    // The key is back to what the engine last said. Not to w1, and not to
    // nothing.
    assert_eq!(
        c.get(b"a/1"),
        Some(Visible::Clean(Some(v("one")))),
        "the key did not roll back to base"
    );
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
    let mut c = loaded();
    c.write(b"a/1", Some(v("w1")), 1, 0);
    c.write(b"a/1", Some(v("w2")), 2, 0);

    // Simulate the narrow rollback by publishing nothing and removing only
    // w1's effect the way a one-entry design would: drop w1, keep w2.
    // Done through the real API by failing w1 and re-applying w2, which is
    // exactly the state the rejected design would have left.
    c.failed(1);
    c.write(b"a/1", Some(v("w2")), 2, 0);

    assert_eq!(
        c.get(b"a/1").unwrap().value(),
        Some(&b"w2"[..]),
        "the control did not reproduce the fabricated value, so the test \
         above is not showing that the cascade prevents one"
    );
    // And that is the point: w2 may have been computed FROM w1, which the
    // engine refused. The copy cannot tell, so phase 3 does not keep it.
}

// ---------------------------------------------------------------------------
// A delta under a pending write.
// ---------------------------------------------------------------------------

#[test]
fn a_delta_moves_base_leaves_pending_on_top_and_signals_once() {
    let mut c = loaded();
    c.write(b"a/1", Some(v("mine")), 1, 0);

    let told = c.apply_delta(
        vec![
            (v("a/1"), Some(v("theirs"))),
            (v("a/2"), Some(v("also theirs"))),
        ],
        root(2),
    );

    // Base moved for both...
    assert_eq!(c.root(), Some(root(2)));
    assert_eq!(c.get(b"a/2"), Some(Visible::Clean(Some(v("also theirs")))));
    // ...and the pending write is still on top of the one it covers.
    let seen = c.get(b"a/1").unwrap();
    assert_eq!(seen.value(), Some(&b"mine"[..]), "{seen:?}");
    assert!(seen.is_pending());

    // The signal fires for the key with a pending write, EXACTLY ONCE, and
    // not for the other.
    assert_eq!(
        told.moved_under_pending,
        vec![v("a/1")],
        "the signal did not fire exactly once for the contested key"
    );

    // Publishing the pending write now reveals what the copy was covering.
    c.published(1);
    assert_eq!(c.get(b"a/1"), Some(Visible::Clean(Some(v("mine")))));
}

#[test]
fn a_delta_with_no_pending_writes_signals_nothing() {
    let mut c = loaded();
    let told = c.apply_delta(vec![(v("a/1"), Some(v("theirs")))], root(2));
    assert!(
        told.is_empty(),
        "a delta over clean keys raised a signal: {told:?}"
    );
    assert_eq!(c.get(b"a/1"), Some(Visible::Clean(Some(v("theirs")))));
}

// ---------------------------------------------------------------------------
// The timeout: the one permanent divergence.
// ---------------------------------------------------------------------------

/// A write with no verdict is rolled back, so no tab shows a phantom for ever.
#[test]
fn a_pending_write_with_no_verdict_times_out_and_rolls_back() {
    let mut c = loaded();
    c.pending_timeout_ms = 1_000;
    c.write(b"a/1", Some(v("phantom")), 1, 0);

    // Before the timeout: still showing, still pending.
    let told = c.time_out(999);
    assert!(told.is_empty(), "rolled back early: {told:?}");
    assert_eq!(c.get(b"a/1").unwrap().value(), Some(&b"phantom"[..]));

    // After: gone, and the app is told which write and why.
    let told = c.time_out(1_000);
    assert_eq!(told.rolled_back, vec![(1, RolledBack::Unknown)]);
    assert_eq!(
        c.get(b"a/1"),
        Some(Visible::Clean(Some(v("one")))),
        "the key did not return to what the engine last said"
    );
}

/// THE CONTROL: with the timeout off, the phantom survives for ever.
///
/// An accepted write can be lost with the engine's context (F32). Without the
/// timeout one tab shows a value no other tab will ever see, and nothing in
/// the system ever corrects it — the one permanent divergence.
#[test]
fn with_the_timeout_off_the_phantom_survives_for_ever() {
    let mut c = loaded();
    c.pending_timeout_ms = u64::MAX;
    c.write(b"a/1", Some(v("phantom")), 1, 0);

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
// Cache, not authority. Bounded. Per tree.
// ---------------------------------------------------------------------------

#[test]
fn an_empty_copy_says_it_knows_nothing_rather_than_that_there_is_nothing() {
    let c = Copy::new();
    assert_eq!(c.root(), None);
    assert_eq!(c.get(b"a/1"), None, "an empty copy claimed a key is absent");
    assert_eq!(
        c.range(b"a/", b"b/"),
        None,
        "an empty copy claimed a range is empty"
    );
}

#[test]
fn rows_read_at_a_different_root_replace_base_rather_than_merging() {
    let mut c = loaded();
    assert_eq!(c.range(b"a/", b"b/").unwrap().len(), 2);

    // The same range, read at a DIFFERENT root, with one row gone. Merging
    // would leave the vanished row visible for ever — and merging two roots'
    // rows is the resolution this must never do.
    c.loaded_range(b"a/", b"b/", vec![(v("a/1"), v("one"))], root(2));
    let rows = c.range(b"a/", b"b/").unwrap();
    assert_eq!(rows.len(), 1, "a row from the OLD root survived: {rows:?}");
    assert_eq!(c.root(), Some(root(2)));
}

#[test]
fn eviction_keeps_it_under_the_cap_and_never_drops_a_pending_write() {
    let mut c = Copy::new();
    c.max_bytes = 100;
    for i in 0..4u32 {
        let lo = format!("r{i}/");
        let hi = format!("r{i}0");
        c.loaded_range(
            lo.as_bytes(),
            hi.as_bytes(),
            (0..5)
                .map(|j| (format!("r{i}/{j}").into_bytes(), vec![b'x'; 20]))
                .collect(),
            root(1),
        );
    }
    assert!(c.bytes() <= 100, "over the cap: {} B", c.bytes());

    // A pending write is not a cache entry — it is something the app is
    // waiting on, and evicting it would show a row reverting for a reason
    // nobody can see.
    c.write(b"r0/0", Some(v("mine")), 1, 0);
    for i in 4..8u32 {
        let lo = format!("r{i}/");
        let hi = format!("r{i}0");
        c.loaded_range(
            lo.as_bytes(),
            hi.as_bytes(),
            (0..5)
                .map(|j| (format!("r{i}/{j}").into_bytes(), vec![b'x'; 20]))
                .collect(),
            root(1),
        );
    }
    assert_eq!(
        c.pending_ids(),
        vec![1],
        "eviction dropped a write the app is still waiting on"
    );
}
