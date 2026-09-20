//! `Db` over the engine, in a browser: reads from the local copy, writes on
//! the pump.
//!
//! The property under test throughout is the one that is easy to get wrong and
//! invisible when you do: **a range nobody has loaded must not answer
//! "empty".** An app told empty draws an empty list and stops; an app told
//! `NotLoaded` reloads.

use craftworks_sdk::{CachedStore, Reads, Store, StoreError};

fn store() -> CachedStore {
    CachedStore::new(Box::new(|| 0))
}

fn root(n: u8) -> [u8; 32] {
    [n; 32]
}

#[test]
fn an_unloaded_range_is_not_an_empty_one() {
    let mut s = store();

    // Nothing loaded: every read says so.
    assert_eq!(Reads::get(&mut s, b"a/1"), Err(StoreError::NotLoaded));
    assert_eq!(
        Reads::scan(&mut s, b"a/", b"b/", false, 10),
        Err(StoreError::NotLoaded)
    );
    assert_eq!(Reads::root(&mut s), Err(StoreError::NotLoaded));

    // Load the range with NOTHING in it. Now "empty" is a fact, and the same
    // calls answer it — which is the whole distinction.
    s.on_page(b"a/", b"b/", vec![], root(1));
    assert_eq!(Reads::get(&mut s, b"a/1"), Ok(None));
    assert_eq!(Reads::scan(&mut s, b"a/", b"b/", false, 10), Ok(vec![]));
    assert_eq!(Reads::root(&mut s), Ok(root(1)));

    // A DIFFERENT range is still unknown. The loaded fact does not spread.
    assert_eq!(Reads::get(&mut s, b"z/1"), Err(StoreError::NotLoaded));
    assert_eq!(
        Reads::scan(&mut s, b"z/", b"z0", false, 10),
        Err(StoreError::NotLoaded)
    );
}

#[test]
fn the_error_says_what_to_do_about_it() {
    let msg = StoreError::NotLoaded.to_string();
    assert!(msg.contains("reload"), "unhelpful: {msg}");
    assert!(
        msg.contains("not known to be empty"),
        "the message does not distinguish itself from empty: {msg}"
    );
}

#[test]
fn a_write_shows_at_once_and_goes_out_on_the_pump() {
    let mut s = store();
    s.on_page(
        b"a/",
        b"b/",
        vec![(b"a/1".to_vec(), b"one".to_vec())],
        root(1),
    );

    Store::put(&mut s, b"a/1", b"mine");
    // Visible immediately — that is what optimistic means.
    assert_eq!(Reads::get(&mut s, b"a/1"), Ok(Some(b"mine".to_vec())));
    // And it says it is not settled.
    assert!(s.copy.get(b"a/1").unwrap().is_pending());

    // It really left, as a Write, exactly once.
    let out = s.take_outbound();
    assert_eq!(out.len(), 1, "expected one request, got {}", out.len());
    match protocol::decode_request(&out[0]) {
        protocol::Incoming::Ok(env) => match env.body {
            protocol::Request::Write { ops, .. } => assert_eq!(ops.len(), 1),
            other => panic!("not a Write: {other:?}"),
        },
        other => panic!("not decodable: {other:?}"),
    }
}

/// A write the copy will not hold is NOT sent.
///
/// Showing nothing while the network took it would put the row and the tree
/// permanently out of step, with nothing on screen to say so. The copy is the
/// thing that decides, and the wire follows it.
#[test]
fn a_write_the_copy_refuses_never_reaches_the_wire() {
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    s.copy.max_pending = 1;

    Store::put(&mut s, b"a/1", b"first");
    assert_eq!(s.take_outbound().len(), 1, "the first write must go");

    Store::put(&mut s, b"a/2", b"second");
    assert_eq!(
        s.take_outbound().len(),
        0,
        "a write the copy refused was sent anyway — the row would show \
         nothing while the tree took it"
    );
    assert_eq!(s.refused.len(), 1, "the refusal was not reported");
    assert_eq!(
        Reads::get(&mut s, b"a/2"),
        Ok(None),
        "the refused write left a value behind"
    );
}

/// A multi-key write is all or nothing in the COPY too, not only on the wire.
#[test]
fn a_refused_multi_key_write_leaves_none_of_its_keys_changed() {
    use craftworks_sdk::Edit;
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    s.copy.max_pending = 2;

    // Three keys, a cap of two: the third is refused part-way through.
    Store::apply_batch(
        &mut s,
        &[
            (b"a/1".to_vec(), Edit::Put(b"x".to_vec())),
            (b"a/2".to_vec(), Edit::Put(b"y".to_vec())),
            (b"a/3".to_vec(), Edit::Put(b"z".to_vec())),
        ],
    );
    assert_eq!(s.refused.len(), 1, "the batch was not refused");
    for k in [&b"a/1"[..], b"a/2", b"a/3"] {
        assert_eq!(
            Reads::get(&mut s, k),
            Ok(None),
            "{} kept part of a refused batch",
            String::from_utf8_lossy(k)
        );
    }
    assert_eq!(s.take_outbound().len(), 0, "a refused batch was sent");
}

#[test]
fn a_delta_updates_what_is_loaded_and_leaves_pending_on_top() {
    let mut s = store();
    s.on_page(
        b"a/",
        b"b/",
        vec![
            (b"a/1".to_vec(), b"one".to_vec()),
            (b"a/2".to_vec(), b"two".to_vec()),
        ],
        root(1),
    );
    Store::put(&mut s, b"a/1", b"mine");

    let told = s.on_delta(
        vec![
            (b"a/1".to_vec(), Some(b"theirs".to_vec())),
            (b"a/2".to_vec(), None),
        ],
        root(2),
    );

    assert_eq!(Reads::get(&mut s, b"a/1"), Ok(Some(b"mine".to_vec())));
    assert_eq!(
        Reads::get(&mut s, b"a/2"),
        Ok(None),
        "a removal was not applied"
    );
    assert_eq!(Reads::root(&mut s), Ok(root(2)));
    assert_eq!(
        told.moved_under_pending,
        vec![b"a/1".to_vec()],
        "the contested key did not raise its signal"
    );
}

/// The store's `changes_since` is the LOCAL fallback and says so.
///
/// The copy holds no history, so it cannot diff two roots. It answers with the
/// defined outcome rather than an error, which is the same recovery a caller
/// does against an engine whose old root has been evicted — one path, not two.
#[test]
fn the_local_copy_cannot_diff_and_asks_for_a_full_reload() {
    use craftworks_sdk::Delta;
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    assert_eq!(
        Reads::changes_since(&mut s, root(0), b"a/", b"b/", 100),
        Ok(Delta::FullReloadRequired { new_root: root(1) })
    );
}
