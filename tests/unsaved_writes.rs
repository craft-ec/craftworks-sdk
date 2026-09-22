//! What a closing tab would lose: every write not yet PUBLISHED, held ones
//! included (craftworks-sdk#163).
//!
//! The page arms its unsaved-changes guard, and says "saving N…", from
//! `CachedStore::unsaved_writes`. With the outbox's window full, most of a
//! large save is HELD — the node has never seen it — so a count of the writes
//! at the node would say 16 of a 100-row save, and a count of none would let
//! the tab close on 84 writes nobody sent.

use craftworks_sdk::cached_store::WRITES_IN_FLIGHT;
use craftworks_sdk::Store as _;

/// **100 writes with the window full: all 100 are unsaved, though only the
/// window's worth is at the node; once every one is published, none is.**
#[test]
fn a_hundred_writes_are_unsaved_until_published_held_ones_included() {
    let node = testkit::PageNode::new();
    let mut conn = node.connect();
    let (mut s, _clock) = testkit::cached_store();
    s.client.send(&protocol::Request::Identity);
    for f in s.take_outbound() {
        for r in conn.frame(&f) {
            s.on_inbound(&r);
        }
    }
    for i in 0..100u32 {
        s.put(format!("d/notes/{i:04}").as_bytes(), b"row").expect("the store took the write");
    }
    let sent = s.take_outbound();
    assert_eq!(sent.len(), WRITES_IN_FLIGHT, "the window is not full, so this proves nothing about held writes");
    assert_eq!(s.held_count(), 100 - WRITES_IN_FLIGHT);
    assert_eq!(
        s.unsaved_writes(),
        100,
        "with {} held and {WRITES_IN_FLIGHT} at the node, the page would guard only what it SENT",
        s.held_count()
    );

    // The node answers, one request at a time, until nothing is left to send.
    let mut queue: std::collections::VecDeque<Vec<u8>> = sent.into();
    for step in 0.. {
        assert!(step < 10_000, "the node was still answering after 10,000 requests");
        let Some(f) = queue.pop_front() else { break };
        for r in conn.frame(&f) {
            s.on_inbound(&r);
        }
        queue.extend(s.take_outbound());
    }
    assert_eq!(s.unsaved_writes(), 0, "writes still unsaved after all were answered: {:?}", s.copy.pending_ids());
}

/// THE CONTROL: an `Accepted` write is still unsaved. The engine computed
/// its blocks; a tab closed now can still lose it (sdk#163's ruling).
#[test]
fn an_accepted_write_is_still_unsaved() {
    let (mut s, _clock) = testkit::cached_store();
    s.put(b"d/notes/0001", b"row").expect("the store took the write");
    let id = s.copy.pending_ids()[0];
    let accepted = protocol::encode_reply(&protocol::Reply::SessionWriteState {
        session: s.client.session().expect("a session"),
        write_id: id,
        state: protocol::WriteState::Accepted,
    })
    .expect("encodes");
    s.on_inbound(&accepted);
    assert_eq!(s.unsaved_writes(), 1, "an Accepted write was counted as saved");
}
