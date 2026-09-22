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

    Store::put(&mut s, b"a/1", b"mine").expect("the store took the write");
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

    Store::put(&mut s, b"a/1", b"first").expect("the store took the write");
    assert_eq!(s.take_outbound().len(), 1, "the first write must go");

    // The refusal is the RETURN VALUE (sdk#186), and the same one reported.
    let refused = Store::put(&mut s, b"a/2", b"second").expect_err("a write past the copy's cap was taken");
    assert_eq!(s.refused.last().map(|(_, r)| r), Some(&refused), "the returned refusal is not the one reported");
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

    // Three keys, a cap of two: the third is refused part-way through — and
    // the refusal is what the call RETURNS (sdk#180).
    let refused = Store::apply_batch(
        &mut s,
        &[
            (b"a/1".to_vec(), Edit::Put(b"x".to_vec())),
            (b"a/2".to_vec(), Edit::Put(b"y".to_vec())),
            (b"a/3".to_vec(), Edit::Put(b"z".to_vec())),
        ],
    );
    assert_eq!(refused, Err(craftworks_sdk::Refused::TooManyPending { cap: 2 }), "the batch was not refused");
    assert_eq!(s.refused.len(), 1, "the refusal was not counted");
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
    Store::put(&mut s, b"a/1", b"mine").expect("the store took the write");

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

/// With nothing loaded there is no root to name, and it says so.
///
/// The first version answered with thirty-two zero bytes — a root-shaped value
/// that is not a root. A caller would have recorded it as where it stands and
/// asked for deltas against it for ever, and every one of those would come
/// back `FullReloadRequired` with the same fake root: a loop that looks like
/// work. `NotLoaded` is what `root()` already says, and it is true.
#[test]
fn a_delta_with_nothing_loaded_says_not_loaded_rather_than_naming_a_zero_root() {
    use craftworks_sdk::Delta;
    let mut s = store();
    assert_eq!(Reads::root(&mut s), Err(StoreError::NotLoaded));
    let answer = Reads::changes_since(&mut s, root(0), b"a/", b"b/", 100);
    assert_eq!(
        answer,
        Err(StoreError::NotLoaded),
        "a delta over an unloaded copy answered {answer:?} — a root of zeroes \
         is a value a caller would keep"
    );
    assert_ne!(
        answer,
        Ok(Delta::FullReloadRequired {
            new_root: [0u8; 32]
        })
    );
}

/// **THE TIME A PAGE SENDS IS THE WALL CLOCK, QUANTISED — not a count of
/// timer firings.**
///
/// The unit is a decision with two consequences, and both are measured here:
///
/// * `Params::parity_age` and `Params::max_accept_age` are counts of it, so
///   an engine bound of 32 means 32 of whatever this sends;
/// * a delegate's context is shared by EVERY connection (F47), so two tabs
///   must send the same number for the same instant. They do here, because
///   they are reading one clock rather than counting their own events.
#[test]
fn a_tick_carries_the_wall_clock_in_the_protocol_s_unit() {
    let mut s = store();
    s.send_tick(1_700_000_123_456);
    let out = s.take_outbound();
    assert_eq!(out.len(), 1, "one tick, one frame");

    let want = protocol::tick_of(1_700_000_123_456);
    match protocol::decode_request(&out[0]) {
        protocol::Incoming::Ok(protocol::Envelope {
            body: protocol::Request::Tick { now },
            ..
        }) => assert_eq!(
            now, want,
            "the page quantised the clock its own way. `tick_of` is the one \
             place that decides, so a page and a test cannot pick two units"
        ),
        other => panic!("send_tick queued {other:?}, so the delegate is told no time at all"),
    }

    // TWO TABS, ONE INSTANT, ONE NUMBER. The delegate cannot tell them apart
    // and must not need to.
    let mut a = store();
    let mut b = store();
    a.send_tick(1_700_000_123_456);
    b.send_tick(1_700_000_123_999);
    // The BODIES, not the frames: since sdk#146 each tab's frame names its own
    // session, which is the point of it; the time is the body's.
    let bodies = |fs: Vec<Vec<u8>>| -> Vec<protocol::Request> {
        fs.iter()
            .filter_map(|f| match protocol::decode_request(f) {
                protocol::Incoming::Ok(e) => Some(e.body),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        bodies(a.take_outbound()),
        bodies(b.take_outbound()),
        "two tabs reading the same second sent different times. A delegate's \
         context is shared by every connection (F47), so the lower one would \
         make everything look young again and the deadlines would never fire"
    );
}

/// A CONTROL for the quantum: two instants a tick apart are NOT the same.
///
/// Without it, `tick_of` returning a constant would satisfy the agreement
/// test above perfectly — every tab would agree, on a clock that never moves,
/// and no deadline would ever be reached.
#[test]
fn control_a_later_tick_is_a_later_number() {
    let mut early = store();
    let mut late = store();
    early.send_tick(1_700_000_000_000);
    late.send_tick(1_700_000_000_000 + protocol::TICK_MS);
    assert_ne!(
        early.take_outbound(),
        late.take_outbound(),
        "a whole tick apart and the same number: the clock does not advance"
    );
}

/// **A FLUSH IS WHAT A CLOSING PAGE SAYS**, and it must be a frame.
#[test]
fn a_flush_is_queued_as_a_frame() {
    let mut s = store();
    s.send_flush();
    let out = s.take_outbound();
    assert_eq!(out.len(), 1, "one flush, one frame");
    assert!(
        matches!(
            protocol::decode_request(&out[0]),
            protocol::Incoming::Ok(protocol::Envelope {
                body: protocol::Request::Flush,
                ..
            })
        ),
        "send_flush queued something else, so a tab going away ships nothing \
         and the engine waits for a tick that will never come"
    );
}

/// M2 (sdk#148), main's surviving mutant on #238: a write that CONFLICTS on
/// one key (a/1) and also wrote another key nobody touched (a/2). After the
/// Conflict the copy must not serve the fallen value at EITHER key — a/2 is
/// not the conflicted key, so only the fall's own forgetting clears it: it is
/// NotLoaded, and the next read fetches the tree's value. Control: the same
/// write Published shows the new value at a/2.
#[test]
fn a_conflicted_write_leaves_none_of_its_own_keys_in_the_copy() {
    use craftworks_sdk::read_token::read_token;
    for verdict in [protocol::WriteState::Published, protocol::WriteState::Conflict] {
        let mut s = store();
        s.on_page(b"a/", b"b/", vec![(b"a/1".to_vec(), b"old1".to_vec()), (b"a/2".to_vec(), b"old2".to_vec())], root(1));
        Store::apply_commit(
            &mut s,
            &[
                (b"a/1".to_vec(), protocol::Expect::Value(read_token(b"old1"))),
                (b"a/2".to_vec(), protocol::Expect::Value(read_token(b"old2"))),
            ],
            &[
                (b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"mine1".to_vec())),
                (b"a/2".to_vec(), craftworks_sdk::store::Edit::Put(b"mine2".to_vec())),
            ],
        )
        .expect("the store took the write");
        let frames = s.take_outbound();
        let write_id = frames
            .iter()
            .find_map(|f| match protocol::decode_request(f) {
                protocol::Incoming::Ok(env) => match env.body {
                    protocol::Request::Commit { write_id, .. } => Some(write_id),
                    _ => None,
                },
                _ => None,
            })
            .expect("the write went as a Commit");
        let session = s.client.session().expect("a session");
        let say = |s: &mut CachedStore, r: protocol::Reply| s.on_inbound(&protocol::encode_reply(&r).expect("encodes"));
        say(&mut s, protocol::Reply::SessionWriteState { session, write_id, state: verdict });
        if verdict == protocol::WriteState::Conflict {
            // What page::Server sends beside it, naming a/1 only.
            say(&mut s, protocol::Reply::Conflicted { session, write_id, key: b"a/1".to_vec(), current: Some(read_token(b"theirs")) });
            assert_eq!(Reads::get(&mut s, b"a/1"), Err(StoreError::NotLoaded), "the conflicted key still shows a value");
            assert_eq!(Reads::get(&mut s, b"a/2"), Err(StoreError::NotLoaded), "the fallen write's OTHER key still shows its unsaved value (W6)");
        } else {
            assert_eq!(Reads::get(&mut s, b"a/2"), Ok(Some(b"mine2".to_vec())), "control: the published write's value is not shown");
        }
    }
}

/// builder#107: a client's OWN write changing state names its KEYS, so the
/// rows showing that write can be re-read — the "saving" chip on a plain
/// binding moves to "saved" (Published), to "saved + backed up"
/// (ParityComplete, after the copy has let the write go), and a lost write's
/// rows show it is gone. Each verdict names exactly the write's keys, once.
/// The control: a verdict that changes nothing (Accepted) names none.
#[test]
fn an_own_writes_state_change_names_its_keys() {
    let verdict_of = |states: &[protocol::WriteState]| -> Vec<Vec<Vec<u8>>> {
        let mut s = store();
        s.on_page(b"a/", b"b/", vec![], root(1));
        Store::apply_batch(
            &mut s,
            &[
                (b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"v1".to_vec())),
                (b"a/2".to_vec(), craftworks_sdk::store::Edit::Put(b"v2".to_vec())),
            ],
        )
        .expect("the store took the write");
        let write_id = s
            .take_outbound()
            .iter()
            .find_map(|f| match protocol::decode_request(f) {
                protocol::Incoming::Ok(env) => match env.body {
                    protocol::Request::Commit { write_id, .. } | protocol::Request::Write { write_id, .. } => Some(write_id),
                    _ => None,
                },
                _ => None,
            })
            .expect("the write went out");
        let session = s.client.session().expect("a session");
        let _ = s.take_state_changed();
        states
            .iter()
            .map(|st| {
                s.on_inbound(&protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id, state: *st }).expect("encodes"));
                s.take_state_changed()
            })
            .collect()
    };
    let both = vec![b"a/1".to_vec(), b"a/2".to_vec()];
    use protocol::WriteState as W;
    assert_eq!(verdict_of(&[W::Accepted, W::Published, W::ParityComplete]), vec![vec![], both.clone(), both.clone()], "Published and then ParityComplete must each name the write's keys; Accepted changes no row's settled state");
    // sdk#265: a `Lost` write GOES AGAIN, so nothing about its rows has
    // changed — they still say saving, which is true. Its keys are named when
    // it runs out of tries and FALLS, which is when the rows are wrong.
    let tries = usize::from(craftworks_sdk::cached_store::WRITE_TRIES);
    let losts = vec![W::Lost; tries + 1];
    let mut named: Vec<Vec<Vec<u8>>> = vec![vec![]; tries];
    named.push(both.clone());
    assert_eq!(verdict_of(&losts), named, "a lost write named its rows before it fell, or did not name them when it did");
    assert_eq!(verdict_of(&[W::Published, W::Published]), vec![both, vec![]], "a repeated verdict named the keys again");
}

/// sdk#264 / builder#107: a SUPERSEDED write (another device's head won and
/// holds ITS values at these keys) names its keys, so a plain binding on them
/// re-reads and shows the winner's value instead of this tab's forgotten one
/// -- the case where the screen is most wrong. The control: a `Superseded` for
/// another session names none.
#[test]
fn a_superseded_write_names_its_keys() {
    let named = |ours: bool| -> Vec<Vec<u8>> {
        let mut s = store();
        s.on_page(b"a/", b"b/", vec![], root(1));
        Store::apply_batch(&mut s, &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"mine".to_vec()))]).expect("taken");
        let _ = s.take_outbound();
        let session = s.client.session().expect("a session");
        let _ = s.take_state_changed();
        let to = if ours { session } else { session + 1 };
        let keys = vec![b"a/1".to_vec()];
        s.on_inbound(&protocol::encode_reply(&protocol::Reply::Superseded { session: to, write_id: 1, seq: 2, root: root(2), keys }).expect("encodes"));
        s.take_state_changed()
    };
    assert_eq!(named(true), vec![b"a/1".to_vec()], "a superseded write did not name its keys: its plain bindings keep showing this tab's forgotten value");
    assert_eq!(named(false), Vec::<Vec<u8>>::new(), "another session's Superseded named keys here");
}

/// The keys `take_state_changed` names become DOMAINS by `Db::domain_of_key`:
/// a record key names its domain; a schema key (or any non-record key) names
/// none, so it re-runs nothing.
#[test]
fn a_record_key_names_its_domain_and_a_schema_key_none() {
    use craftworks_sdk::db::record_key;
    type D = craftworks_sdk::Db<craftworks_sdk::MemStore, craftworks_sdk::SystemEnv>;
    let loc = craftworks_sdk::id::loc_from_hex(&"ab".repeat(16)).expect("an id");
    assert_eq!(D::domain_of_key(&record_key("notes", loc)), Some("notes".to_string()));
    let mut schema = vec![0u8];
    schema.extend_from_slice(b"schema\0notes");
    assert_eq!(D::domain_of_key(&schema), None, "a schema key named a domain: a define would re-run every binding");
    assert_eq!(D::domain_of_key(b""), None);
}

/// sdk#265: a write told `Lost` is RE-SENT (with its reads), not rolled back:
/// its commit applied nothing and this client is the only thing that holds
/// it. BOUNDED: the first WRITE_TRIES Losts re-send it; the next one
/// falls it, and it is named for the app (`take_lost_gave_up`).
#[test]
fn a_lost_write_is_re_sent_and_falls_named_only_at_the_bound() {
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    Store::apply_batch(&mut s, &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"v".to_vec()))]).expect("taken");
    let sent = |s: &mut CachedStore| -> Vec<u64> {
        s.take_outbound()
            .iter()
            .filter_map(|f| match protocol::decode_request(f) {
                protocol::Incoming::Ok(env) => match env.body {
                    protocol::Request::Commit { write_id, .. } | protocol::Request::Write { write_id, .. } => Some(write_id),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    let first = sent(&mut s);
    let write_id = *first.first().expect("the write went out");
    let session = s.client.session().expect("a session");
    let lost = |s: &mut CachedStore| s.on_inbound(&protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id, state: protocol::WriteState::Lost }).expect("encodes"));
    let bound = craftworks_sdk::cached_store::WRITE_TRIES as usize + 1;
    for n in 1..bound {
        lost(&mut s);
        assert_eq!(sent(&mut s), vec![write_id], "Lost #{n} did not re-send the write");
        assert_eq!(Reads::get(&mut s, b"a/1"), Ok(Some(b"v".to_vec())), "Lost #{n} rolled the row back");
        assert!(s.take_lost_gave_up().is_empty(), "named before the bound");
    }
    lost(&mut s);
    assert!(sent(&mut s).is_empty(), "re-sent past the bound: a write that is Lost for ever would spin");
    assert_eq!(s.take_lost_gave_up(), vec![write_id], "the write that fell at the bound was not named");
    assert_eq!(s.copy.pending().0, 0, "the fallen write is still held");
}

/// Every write frame this store has sent since the last call, by write id.
fn sent_ids(s: &mut CachedStore) -> Vec<u64> {
    s.take_outbound()
        .iter()
        .filter_map(|f| match protocol::decode_request(f) {
            protocol::Incoming::Ok(env) => match env.body {
                protocol::Request::Commit { write_id, .. } | protocol::Request::Write { write_id, .. } => Some(write_id),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// sdk#265, the architect's item 1 (ORDERING). A write made AFTER a `Lost`
/// one, on its keys, that has already LEFT the outbox is PULLED BACK: its
/// `Conflict` — its reads declared the Lost write's value, which the tree
/// that won does not hold — queues it again BEHIND that write, rather than
/// falling it and asking the app. Both land, in the order the person made
/// them, and the key ends with the LATER value.
#[test]
fn a_write_behind_a_lost_one_is_pulled_back_and_lands_after_it() {
    use craftworks_sdk::read_token::read_token;
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![(b"a/1".to_vec(), b"old".to_vec())], root(1));
    Store::apply_commit(&mut s, &[(b"a/1".to_vec(), protocol::Expect::Value(read_token(b"old")))], &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"first".to_vec()))]).expect("taken");
    let w1 = *sent_ids(&mut s).first().expect("w1 went out");
    // Made and SENT before the Lost arrives: it is at the node, not in the
    // outbox, so it cannot simply be re-ordered — it is pulled back.
    Store::apply_commit(&mut s, &[(b"a/1".to_vec(), protocol::Expect::Value(read_token(b"first")))], &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"second".to_vec()))]).expect("taken");
    let w2 = *sent_ids(&mut s).first().expect("w2 went out");
    let session = s.client.session().expect("a session");
    let say = |s: &mut CachedStore, id: u64, st: protocol::WriteState| s.on_inbound(&protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id: id, state: st }).expect("encodes"));
    say(&mut s, w1, protocol::WriteState::Lost);
    // Nothing goes yet: w2 is still at the node, and the engine takes one
    // commit at a time (`drain_queued`).
    assert!(sent_ids(&mut s).is_empty(), "a write went while another was still awaiting its verdict");
    // w2 was judged at the head that won, where a/1 does not hold "first".
    say(&mut s, w2, protocol::WriteState::Conflict);
    s.on_inbound(&protocol::encode_reply(&protocol::Reply::Conflicted { session, write_id: w2, key: b"a/1".to_vec(), current: Some(read_token(b"theirs")) }).expect("encodes"));
    assert!(s.take_conflict_chains().is_empty(), "the pulled-back write was sent to the app as a conflict instead of going again behind the Lost one");
    assert_eq!(sent_ids(&mut s), vec![w1], "the Lost write did not go again, oldest first");
    say(&mut s, w1, protocol::WriteState::Published);
    assert_eq!(sent_ids(&mut s), vec![w2], "the pulled-back write did not follow the Lost one");
    say(&mut s, w2, protocol::WriteState::Published);
    assert_eq!(Reads::get(&mut s, b"a/1"), Ok(Some(b"second".to_vec())), "the key does not hold the LAST write the person made");
    assert_eq!(s.copy.pending().0, 0, "a write is still unsaved");
}

/// The other half of item 1: when a later write of this client's LANDS first
/// on the Lost write's key, the Lost one can no longer go — in the order made
/// it is underneath, and a write goes whole or not at all (W1). It falls, and
/// it is NAMED; the key keeps the LAST value the person wrote.
#[test]
fn a_lost_write_a_later_one_landed_over_falls_named_rather_than_re_sent() {
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    Store::apply_batch(&mut s, &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"first".to_vec()))]).expect("taken");
    let w1 = *sent_ids(&mut s).first().expect("w1 went out");
    Store::apply_batch(&mut s, &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"second".to_vec()))]).expect("taken");
    let w2 = *sent_ids(&mut s).first().expect("w2 went out");
    let session = s.client.session().expect("a session");
    let say = |s: &mut CachedStore, id: u64, st: protocol::WriteState| s.on_inbound(&protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id: id, state: st }).expect("encodes"));
    // The later write is taken and published while w1's Lost is still on its
    // way — the node's order, not the client's choice.
    say(&mut s, w2, protocol::WriteState::Published);
    say(&mut s, w1, protocol::WriteState::Lost);
    assert!(sent_ids(&mut s).is_empty(), "the Lost write went again over a later write that had landed: the older value would win");
    assert_eq!(s.take_lost_gave_up(), vec![w1], "the write that could no longer go was not named");
    assert_eq!(Reads::get(&mut s, b"a/1"), Ok(Some(b"second".to_vec())), "the key does not hold the LAST write the person made");
}

/// sdk#265, the architect's item 2 (ONE BUDGET). The tries a write spends on
/// `Lost` re-sends travel with its conflict CHAIN into `Db`'s re-run, so the
/// two bounds are one budget of [`WRITE_TRIES`] — not 3 × 3 = 9 rounds — and
/// a re-run write starts from the tries already spent.
#[test]
fn the_lost_re_sends_and_the_conflict_re_runs_share_one_budget() {
    use craftworks_sdk::store::Store as _;
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    Store::apply_batch(&mut s, &[(b"a/1".to_vec(), craftworks_sdk::store::Edit::Put(b"v".to_vec()))]).expect("taken");
    let w1 = *sent_ids(&mut s).first().expect("w1 went out");
    let session = s.client.session().expect("a session");
    let say = |s: &mut CachedStore, id: u64, st: protocol::WriteState| s.on_inbound(&protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id: id, state: st }).expect("encodes"));
    say(&mut s, w1, protocol::WriteState::Lost);
    let _ = sent_ids(&mut s);
    say(&mut s, w1, protocol::WriteState::Lost);
    let _ = sent_ids(&mut s);
    say(&mut s, w1, protocol::WriteState::Conflict);
    let chains = s.take_conflict_chains();
    assert_eq!(chains.len(), 1, "the conflict did not reach the app as one chain");
    assert_eq!(chains[0].tries, 2, "the chain did not carry the tries the Lost re-sends spent: the two bounds would multiply");

    // And the other direction: a write `Db` made as re-run round N spends its
    // Lost tries from N, not from zero.
    let mut s = store();
    s.on_page(b"a/", b"b/", vec![], root(1));
    Store::apply_batch(&mut s, &[(b"a/2".to_vec(), craftworks_sdk::store::Edit::Put(b"v".to_vec()))]).expect("taken");
    let w = *sent_ids(&mut s).first().expect("it went out");
    s.carry_tries(w, craftworks_sdk::cached_store::WRITE_TRIES);
    let session = s.client.session().expect("a session");
    s.on_inbound(&protocol::encode_reply(&protocol::Reply::SessionWriteState { session, write_id: w, state: protocol::WriteState::Lost }).expect("encodes"));
    assert!(sent_ids(&mut s).is_empty(), "a re-run write went again on a fresh Lost budget: the bounds multiply");
    assert_eq!(s.take_lost_gave_up(), vec![w], "it fell without being named");
}

/// builder#107, after craftworks-sdk#267: a tab's OWN write changing state is
/// reported by the domain the APP names — the name its bindings are keyed by —
/// never the stored `<app>.<name>`. Reported as stored, no binding matched,
/// nothing re-ran, and a plain table said "saving" for good (measured in the
/// builder's two-tab control, SDK 53e9f50: A's chip showed "saving" and never
/// "saved" in 20 s).
#[test]
fn own_state_changes_name_the_app_relative_domain_its_bindings_are_keyed_by() {
    use craftworks_sdk::db::record_key;
    type D = craftworks_sdk::Db<craftworks_sdk::MemStore, craftworks_sdk::SystemEnv>;
    let loc = craftworks_sdk::id::loc_from_hex(&"ab".repeat(16)).expect("an id");
    let app = "rmud6o02cnfqk0001"; // a builder project id, the case that was measured
    let mine = record_key(&format!("{app}.notes"), loc);
    let also_mine = record_key(&format!("{app}.craftworks.published"), loc);
    let theirs = record_key("otherapp.notes", loc);
    let mut schema = vec![0u8];
    schema.extend_from_slice(format!("schema\0{app}.notes").as_bytes());

    assert_eq!(
        D::own_domains_of_keys(Some(app), &[mine.clone(), also_mine.clone(), mine.clone(), theirs.clone(), schema]),
        vec!["craftworks.published".to_string(), "notes".to_string()],
        "a tab's own state change did not name the domains its bindings are keyed by (once each, app-relative), or named another app's"
    );
    // THE CONTROL: the stored name is NOT what a binding is keyed by.
    assert!(!D::own_domains_of_keys(Some(app), std::slice::from_ref(&mine)).contains(&format!("{app}.notes")), "the stored name leaked through");
    // No app (data from before apps): the name as stored, as `app::own` says.
    assert_eq!(D::own_domains_of_keys(None, &[record_key("notes", loc)]), vec!["notes".to_string()]);
}
