//! What takes a write OUT of the pending list.
//!
//! For a while, nothing did. A write entered the local copy as pending and
//! stayed there: at `max_pending` the copy began refusing writes and the app
//! could make no more, every row reported `PENDING` for ever so none could
//! say "saved", and `tick` eventually rolled them all back as `Unknown` —
//! every write a person made turning into "not applied".
//!
//! It was found by writing 300 records and asserting that 300 arrived. 256
//! did. The round-trip test it belonged to had been PASSING, correct about a
//! range that was not the one it named.
//!
//! The rule it cost: for every state a thing can enter, name the code that
//! takes it out.

use craftworks_sdk::store::{Reads, RowState, Store as _};
use craftworks_sdk::CachedStore;
use protocol::WriteState;

fn store() -> CachedStore {
    let (mut s, _clock) = testkit::cached_store();
    // Everything loaded and empty, so reads are answered rather than refused.
    s.on_page(b"", &[0xFFu8; 64], Vec::new(), [0u8; 32]);
    s
}

/// The engine's verdict on one of THIS store's writes: named with its session,
/// as the delegate names a v4 writer's (sdk#146) — an unnamed one is another
/// tab's, and is never applied.
fn verdict(s: &CachedStore, write_id: u64, state: WriteState) -> Vec<u8> {
    let session = s.client.session().expect("a session");
    protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id, state }).expect("encodes")
}

/// **300 sequential writes all land, with zero refusals.**
///
/// The case that failed: 256 landed and 44 were refused, because nothing
/// cleared a pending write.
#[test]
fn three_hundred_writes_all_land_when_each_is_published() {
    let mut s = store();
    for i in 0..300u64 {
        let id = s.next_write_id();
        s.put(format!("d\0note\0{i:06}").as_bytes(), b"v");
        s.on_inbound(&verdict(&s, id, WriteState::Published));
    }
    assert!(
        s.refused.is_empty(),
        "{} of 300 writes were refused; the copy filled up because nothing \
         took published writes out of it",
        s.refused.len()
    );
    assert_eq!(
        s.copy.pending().0,
        0,
        "writes stayed pending after publishing"
    );
}

/// THE CONTROL for that: without a verdict, the copy fills and refuses.
///
/// This is the defect, reproduced. Without it, the test above would pass
/// against a copy with no cap at all, and would say nothing about what
/// clears a write.
#[test]
fn control_without_a_verdict_the_copy_fills_up_and_refuses() {
    let mut s = store();
    for i in 0..300u64 {
        s.put(format!("d\0note\0{i:06}").as_bytes(), b"v");
    }
    assert!(
        !s.refused.is_empty(),
        "300 unacknowledged writes were all accepted, so the cap that makes \
         the test above meaningful is not there"
    );
    assert_eq!(s.copy.pending().0, 256);
}

/// A row goes PENDING → CLEAN through the REAL reply path.
///
/// Not by a test poking the copy: that is how the previous tests for this
/// passed while nothing in the product ever made the transition.
#[test]
fn a_row_goes_pending_then_clean_through_the_reply_path() {
    let mut s = store();
    let key = b"d\x00note\x00000001".to_vec();
    let id = s.next_write_id();
    s.put(&key, b"v");
    assert_eq!(s.row_state(&key), RowState::Pending);

    s.on_inbound(&verdict(&s, id, WriteState::Published));
    assert_eq!(
        s.row_state(&key),
        RowState::Clean,
        "a published write still reports itself in flight, so the row can \
         never say 'saved'"
    );
    assert!(s.row_state(&key).is_settled());
}

/// `Busy` is the engine saying it has not taken this write yet.
#[test]
fn busy_puts_the_row_back_in_the_queue() {
    let mut s = store();
    let key = b"d\x00note\x00000001".to_vec();
    let id = s.next_write_id();
    s.put(&key, b"v");
    s.on_inbound(&verdict(&s, id, WriteState::Busy));
    assert_eq!(s.row_state(&key), RowState::Queued);
}

/// A `Failed` verdict makes the row ROLLED_BACK, not clean.
#[test]
fn a_failed_verdict_gives_the_row_rolled_back() {
    let mut s = store();
    let key = b"d\x00note\x00000001".to_vec();
    let id = s.next_write_id();
    s.put(&key, b"v");
    s.on_inbound(&verdict(&s, id, WriteState::Failed));
    assert_eq!(
        s.row_state(&key),
        RowState::RolledBack,
        "a write the engine refused went back to reporting 'saved'"
    );
}

/// A verdict for a write this client never issued changes NOTHING.
///
/// The node chooses the id. Believing one would let a single message clear
/// or fail somebody else's write.
#[test]
fn a_verdict_for_an_unknown_write_changes_nothing_and_is_counted() {
    let mut s = store();
    let key = b"d\x00note\x00000001".to_vec();
    let id = s.next_write_id();
    s.put(&key, b"v");
    assert_eq!(s.row_state(&key), RowState::Pending);

    s.on_inbound(&verdict(&s, id + 999, WriteState::Published));
    assert_eq!(
        s.row_state(&key),
        RowState::Pending,
        "a verdict about a different write cleared this one"
    );
    assert_eq!(
        s.unknown_verdicts(),
        1,
        "it was applied silently rather than counted"
    );

    // THE CONTROL: its OWN id does clear it, so the refusal above is about
    // the id and not about verdicts being ignored.
    s.on_inbound(&verdict(&s, id, WriteState::Published));
    assert_eq!(s.row_state(&key), RowState::Clean);
}

/// A DUPLICATE `Published` is harmless.
///
/// A node may repeat itself, and the second one arrives when the write is
/// already gone from the pending list — which is exactly the shape that
/// makes it look like a verdict for an unknown write.
#[test]
fn a_duplicate_published_is_harmless() {
    let mut s = store();
    let key = b"d\x00note\x00000001".to_vec();
    let id = s.next_write_id();
    s.put(&key, b"v");
    s.on_inbound(&verdict(&s, id, WriteState::Published));
    assert_eq!(s.row_state(&key), RowState::Clean);

    s.on_inbound(&verdict(&s, id, WriteState::Published));
    assert_eq!(
        s.row_state(&key),
        RowState::Clean,
        "a repeated verdict disturbed a write that had already landed"
    );
    assert!(s.refused.is_empty());
    // It IS counted as unknown, which is correct and worth pinning: by then
    // the write is genuinely not in the pending list, and the alternative —
    // remembering every id ever issued — is unbounded.
    assert_eq!(s.unknown_verdicts(), 1);
}
