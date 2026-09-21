//! `ParityComplete` arrives AFTER `Published` for every coded write — the
//! engine's own order — and by then the client has settled the write. It is
//! not a stranger's verdict and is not counted as one (the write path's model
//! test found `unknown_verdicts` non-zero in 1,000 of 1,000 runs because of it).

fn verdict(s: &craftworks_sdk::CachedStore, write_id: u64, state: protocol::WriteState) -> Vec<u8> {
    protocol::encode_reply(&protocol::Reply::SessionWriteState { session: s.client.session().expect("a session"), write_id, state })
        .expect("encodes")
}

#[test]
fn parity_complete_after_published_is_not_a_strangers_verdict() {
    let (mut s, _clock) = testkit::cached_store();
    craftworks_sdk::Store::put(&mut s, b"k", b"v");
    let id = s.copy.pending_ids()[0];
    for st in [protocol::WriteState::Accepted, protocol::WriteState::Published, protocol::WriteState::ParityComplete] {
        let v = verdict(&s, id, st);
        s.on_inbound(&v);
    }
    assert!(s.copy.pending_ids().is_empty());
    assert_eq!(s.unknown_verdicts(), 0, "the ParityComplete that follows Published was counted as a stranger's verdict");
}

/// THE CONTROL: a verdict for a write this client never made is still counted —
/// a ParityComplete for an id it has not minted, and any other late state.
#[test]
fn control_a_verdict_for_a_write_never_made_is_still_counted() {
    let (mut s, _clock) = testkit::cached_store();
    let never = s.next_write_id() + 5;
    let v = verdict(&s, never, protocol::WriteState::ParityComplete);
    s.on_inbound(&v);
    let v = verdict(&s, never, protocol::WriteState::Published);
    s.on_inbound(&v);
    assert_eq!(s.unknown_verdicts(), 2);
}
