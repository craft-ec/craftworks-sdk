//! Every tab, and every page load, is its OWN session (craftworks-sdk#146).
//!
//! The engine keys writes by `(ClientId, WriteId)`. `CachedStore::new` starts
//! write ids at 1 and the shell named every client `ClientId(1)`, so the first
//! write of EVERY tab and EVERY reload was `(1, 1)` — a collision by default,
//! not a race. Everything the engine keys on the pair (parity waiting, stalls,
//! pending writes, "have I seen this write") took one session's write for
//! another's.

use craftworks_sdk::{CachedStore, Store as _};
use engine_delegate::shell::Inbound;

/// One tab: a `CachedStore` over a connection to the node.
struct Tab {
    store: CachedStore,
    conn: testkit::Conn,
    /// `(session, write_id)` of every write this tab sent, read off the wire.
    sent: Vec<(u64, u64)>,
    /// Every write state this tab was told, with the session it names
    /// (`None`: an unnamed, pre-v4 `WriteState`).
    told: Vec<(Option<u64>, protocol::WriteState)>,
}

/// The session a request envelope names — what the shell keys the client by
/// (with the version, see `engine-delegate`'s `as_client`). Before v4 there
/// was none, and every client was `ClientId(1)`.
fn session_of(env: &protocol::Envelope) -> u64 {
    env.session
}

impl Tab {
    fn on(conn: testkit::Conn) -> Tab {
        let mut t = Tab { store: testkit::cached_store().0, conn, sent: Vec::new(), told: Vec::new() };
        t.store.client.send(&protocol::Request::Identity);
        t.pump();
        t
    }

    fn pump(&mut self) {
        for frame in self.store.take_outbound() {
            if let protocol::Incoming::Ok(env) = protocol::decode_request(&frame) {
                if let protocol::Request::Write { write_id, .. } = env.body {
                    self.sent.push((session_of(&env), write_id));
                }
            }
            for reply in self.conn.step(vec![Inbound::Client(frame)]) {
                match protocol::decode_reply(&reply) {
                    Ok(protocol::Reply::SessionWriteState { session, state, .. }) => self.told.push((Some(session), state)),
                    Ok(protocol::Reply::WriteState { state, .. }) => self.told.push((None, state)),
                    _ => {}
                }
                self.store.on_inbound(&reply);
            }
        }
    }

    fn write(&mut self, key: &[u8], value: &[u8]) {
        self.store.put(key, value);
        self.pump();
    }
}

/// What the node holds at `key`, read by a fresh tab.
fn stored(conn: &testkit::Conn, key: &[u8]) -> Option<Vec<u8>> {
    use craftworks_sdk::store::Reads;
    let mut reader = Tab::on(conn.clone());
    let mut hi = key.to_vec();
    hi.push(0);
    reader.store.request_range(1, key, &hi, 8);
    let frames = reader.store.take_outbound();
    let mut rows = Vec::new();
    for frame in frames {
        for reply in reader.conn.step(vec![Inbound::Client(frame)]) {
            if let Ok(protocol::Reply::Page { entries, at, .. }) = protocol::decode_reply(&reply) {
                rows = entries.clone();
                reader.store.on_page(key, &hi, entries, at.root);
            }
        }
    }
    let _ = rows;
    reader.store.get(key).ok().flatten()
}

/// **Two tabs and a reload: three sessions, three distinct write keys, three
/// writes stored.**
#[test]
fn two_tabs_and_a_reload_are_three_sessions_and_all_three_writes_land() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut tab1 = Tab::on(conn.clone());
    let mut tab2 = Tab::on(conn.clone());
    tab1.write(b"k/tab1", b"from tab 1");
    tab2.write(b"k/tab2", b"from tab 2");
    let mut reloaded = Tab::on(conn.clone());   // tab 1, after a page reload
    reloaded.write(b"k/reload", b"from the reload");

    let pairs: Vec<(u64, u64)> = [&tab1, &tab2, &reloaded].iter().flat_map(|t| t.sent.clone()).collect();
    println!("  (session, write_id) reaching the engine: {pairs:?}");
    assert_eq!(pairs.len(), 3, "each tab sent one write");
    let distinct: std::collections::BTreeSet<_> = pairs.iter().collect();
    assert_eq!(distinct.len(), 3, "three sessions' first writes collided on one key: {pairs:?}");

    // The STORED values, not the verdicts.
    assert_eq!(stored(&conn, b"k/tab1").as_deref(), Some(&b"from tab 1"[..]));
    assert_eq!(stored(&conn, b"k/tab2").as_deref(), Some(&b"from tab 2"[..]));
    assert_eq!(stored(&conn, b"k/reload").as_deref(), Some(&b"from the reload"[..]));
}

/// **The delegate ADDRESSES each write's state to its own session.** The
/// wire's session is not enough: the shell has to key the engine's client by
/// it, or every state goes out unnamed and a tab cannot tell its own from
/// another's.
#[test]
fn each_tabs_write_states_name_that_tabs_session() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut tab1 = Tab::on(conn.clone());
    let mut tab2 = Tab::on(conn.clone());
    tab1.write(b"k/tab1", b"from tab 1");
    tab2.write(b"k/tab2", b"from tab 2");
    for (name, tab) in [("tab 1", &tab1), ("tab 2", &tab2)] {
        let mine = tab.store.client.session().expect("a session");
        assert!(!tab.told.is_empty(), "{name} was told nothing, so this checks nothing");
        assert!(tab.told.iter().all(|(s, _)| *s == Some(mine)),
            "{name} (session {mine}) was told states naming {:?}", tab.told);
    }
    assert_ne!(tab1.store.client.session().expect("a session"), tab2.store.client.session().expect("a session"));
}

/// **A state naming ANOTHER session is dropped, never applied**, even for a
/// write id this tab has pending — which is the ordinary case, since every
/// tab's first write is id 1.
#[test]
fn a_write_state_for_another_session_is_ignored() {
    let (mut s, _clock) = testkit::cached_store();
    s.put(b"k", b"v");                       // write id 1, pending
    let id = s.copy.pending_ids()[0];
    let foreign = protocol::encode_reply(&protocol::Reply::SessionWriteState {
        session: s.client.session().expect("a session") ^ 0x55,
        write_id: id,
        state: protocol::WriteState::Failed,
    })
    .unwrap();
    s.on_inbound(&foreign);
    assert_eq!(s.copy.pending_ids(), vec![id], "another session's verdict rolled this tab's write back");
    assert_eq!(s.client.foreign_write_states, 1, "and it was not counted");
    // THE CONTROL: the same verdict, for THIS session, is applied.
    let own = protocol::encode_reply(&protocol::Reply::SessionWriteState {
        session: s.client.session().expect("a session"),
        write_id: id,
        state: protocol::WriteState::Failed,
    })
    .unwrap();
    s.on_inbound(&own);
    assert!(s.copy.pending_ids().is_empty(), "its own verdict was not applied either, so the drop above proves nothing");
}

/// **Two tabs' first writes waiting at ONCE each hear `ParityComplete`** — the
/// architect's B on sdk#146: `parity_waiting` was keyed on `(client, write)`,
/// so tab 2's `(1, 1)` entry overwrote tab 1's; both got parity, and one tab
/// was never told. The node's answers are HELD so both writes wait together.
#[test]
fn two_sessions_waiting_on_parity_together_are_each_told_it_completed() {
    let node = testkit::FullNode::new();
    let mut conn = node.connect();
    let mut tab1 = Tab::on(conn.clone());
    let mut tab2 = Tab::on(conn.clone());
    conn.hold_answers();
    tab1.write(b"k/tab1", b"from tab 1");
    tab2.write(b"k/tab2", b"from tab 2");
    assert!(conn.held() > 0, "nothing was held, so the two writes never waited together");
    // Release the node's answers one per call, and let each tab tick between
    // rounds as a page does (a write refused Busy is re-sent from its queue).
    let mut released: Vec<(Option<u64>, protocol::WriteState)> = Vec::new();
    for _ in 0..200 {
        while conn.held() > 0 {
            for reply in conn.release_one() {
                match protocol::decode_reply(&reply) {
                    Ok(protocol::Reply::SessionWriteState { session, state, .. }) => released.push((Some(session), state)),
                    Ok(protocol::Reply::WriteState { state, .. }) => released.push((None, state)),
                    _ => {}
                }
                tab1.store.on_inbound(&reply);
                tab2.store.on_inbound(&reply);
            }
        }
        for tab in [&mut tab1, &mut tab2] {
            tab.store.tick();
            tab.pump();
        }
        if conn.held() == 0 && tab1.store.copy.pending_ids().is_empty() && tab2.store.copy.pending_ids().is_empty() {
            break;
        }
    }
    let mut complete: std::collections::BTreeMap<Option<u64>, usize> = Default::default();
    for (who, st) in tab1.told.iter().chain(tab2.told.iter()).chain(released.iter()) {
        if *st == protocol::WriteState::ParityComplete {
            *complete.entry(*who).or_default() += 1;
        }
    }
    let (s1, s2) = (tab1.store.client.session().expect("a session"), tab2.store.client.session().expect("a session"));
    println!("  ParityComplete by session: {complete:?} (tab 1 {s1}, tab 2 {s2})");
    assert_eq!(complete.get(&Some(s1)).copied().unwrap_or(0), 1, "tab 1 was not told its parity completed: {complete:?}");
    assert_eq!(complete.get(&Some(s2)).copied().unwrap_or(0), 1, "tab 2 was not told its parity completed: {complete:?}");
}

// ---- the architect's review of #165 -----------------------------------------

/// **A2: an UNNAMED verdict is somebody else's.** A v3 tab's `w1: Failed`,
/// delivered to this v4 tab's connection because they share the delegate, must
/// not roll back this tab's own `w1`.
#[test]
fn a_plain_verdict_is_another_tabs_and_is_never_applied() {
    let (mut s, _clock) = testkit::cached_store();
    s.put(b"k", b"v");
    let id = s.copy.pending_ids()[0];
    let plain = protocol::encode_reply(&protocol::Reply::WriteState { write_id: id, state: protocol::WriteState::Failed }).unwrap();
    s.on_inbound(&plain);
    assert_eq!(s.copy.pending_ids(), vec![id], "a v3 tab's verdict rolled back this tab's write");
    assert_eq!(s.client.foreign_write_states, 1, "and it was not counted as foreign");
}

/// **A1, and the RNG: a page that could not mint a session makes NO write** —
/// refused by name, nothing sent — rather than sharing one fallback number
/// with every other such page.
#[test]
fn a_page_with_no_session_refuses_its_writes_by_name_and_sends_nothing() {
    let (mut s, _clock) = testkit::cached_store();
    s.client = craftworks_sdk::engine_client::Client::from_random(None);
    s.put(b"k", b"v");
    assert_eq!(s.refused.len(), 1);
    assert!(matches!(s.refused[0].1, craftworks_sdk::copy::Refused::NoSession), "{:?}", s.refused);
    assert!(s.copy.pending_ids().is_empty(), "the write is held as if it could be sent");
    assert!(s.take_outbound().is_empty(), "it was sent under no session");
    // THE CONTROL: the same page with randomness writes.
    let (mut ok, _c) = testkit::cached_store();
    ok.put(b"k", b"v");
    assert!(ok.refused.is_empty());
    assert_eq!(ok.take_outbound().len(), 1);
}

/// **A3: a reload is a new session, and its bindings are not starved by the
/// pages before it.** Six page loads of eight bindings each, through one
/// delegate that is never told a page went away. Before, the fifth and sixth
/// loads were refused 8 of 8.
#[test]
fn six_page_loads_of_eight_bindings_each_are_all_accepted() {
    let node = testkit::FullNode::new();
    let mut c = node.connect();
    let sub = |i: u64| protocol::Request::SubscribeRange {
        sub_id: i,
        lo: protocol::Bound::Included(format!("d/{i:03}/").into_bytes()),
        hi: protocol::Bound::Excluded(format!("d/{i:03}0").into_bytes()),
    };
    let accepted = |r: &[Vec<u8>]| -> Vec<bool> {
        r.iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .filter_map(|x| match x {
                protocol::Reply::Subscribed { accepted, .. } => Some(matches!(accepted, protocol::Accepted::Yes)),
                _ => None,
            })
            .collect()
    };
    let mut log = Vec::new();
    for load in 0..6u64 {
        let session = protocol::mint_session(0x5000 + load);
        let mut yes = 0;
        for i in 1..=8u64 {
            let a = accepted(&c.client_as(session, protocol::CURRENT, &sub(i)));
            assert_eq!(a.len(), 1, "load {} binding {i} was not answered", load + 1);
            yes += a[0] as usize;
        }
        log.push(yes);
    }
    println!("  accepted per page load: {log:?}");
    assert_eq!(log, vec![8; 6], "a later page load was refused its bindings");
}

/// A3's other bound: ONE page cannot take every other page's room.
#[test]
fn one_page_is_refused_past_its_own_share() {
    let node = testkit::FullNode::new();
    let mut c = node.connect();
    let per = engine::Params::default().max_subscriptions_per_client as u64;
    let session = protocol::mint_session(0x77);
    let mut answers = Vec::new();
    for i in 1..=per + 1 {
        for r in c.client_as(session, protocol::CURRENT, &protocol::Request::SubscribeRange {
            sub_id: i,
            lo: protocol::Bound::Included(format!("e/{i:03}/").into_bytes()),
            hi: protocol::Bound::Excluded(format!("e/{i:03}0").into_bytes()),
        }) {
            if let Ok(protocol::Reply::Subscribed { accepted, .. }) = protocol::decode_reply(&r) {
                answers.push(matches!(accepted, protocol::Accepted::Yes));
            }
        }
    }
    assert_eq!(answers.len() as u64, per + 1);
    assert!(answers[..per as usize].iter().all(|a| *a), "a page was refused inside its share");
    assert!(!answers[per as usize], "a page was given more than its share of the delegate's subscriptions");
}

/// A1 at the delegate: a frame with a bad session is ANSWERED `BadSession`, and
/// nothing it asked for is done.
#[test]
fn the_delegate_answers_a_bad_session_by_name_and_applies_nothing() {
    let node = testkit::FullNode::new();
    let mut c = node.connect();
    let r = c.client_as(0xABCD_0000_0000_0123, protocol::CURRENT, &protocol::Request::Write {
        write_id: 1,
        ops: vec![protocol::Op::Put(b"k/wide".to_vec(), b"x".to_vec())],
    });
    let replies: Vec<protocol::Reply> = r.iter().filter_map(|b| protocol::decode_reply(b).ok()).collect();
    assert!(
        replies.iter().any(|x| matches!(x, protocol::Reply::Dropped { reason: protocol::Dropped::BadSession })),
        "{replies:?}"
    );
    assert!(!replies.iter().any(|x| matches!(x, protocol::Reply::WriteState { .. } | protocol::Reply::SessionWriteState { .. })),
        "a write under a bad session was acted on: {replies:?}");
}
