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
        let mine = tab.store.client.session();
        assert!(!tab.told.is_empty(), "{name} was told nothing, so this checks nothing");
        assert!(tab.told.iter().all(|(s, _)| *s == Some(mine)),
            "{name} (session {mine}) was told states naming {:?}", tab.told);
    }
    assert_ne!(tab1.store.client.session(), tab2.store.client.session());
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
        session: s.client.session() ^ 0x55,
        write_id: id,
        state: protocol::WriteState::Failed,
    })
    .unwrap();
    s.on_inbound(&foreign);
    assert_eq!(s.copy.pending_ids(), vec![id], "another session's verdict rolled this tab's write back");
    assert_eq!(s.client.foreign_write_states, 1, "and it was not counted");
    // THE CONTROL: the same verdict, for THIS session, is applied.
    let own = protocol::encode_reply(&protocol::Reply::SessionWriteState {
        session: s.client.session(),
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
    let (s1, s2) = (tab1.store.client.session(), tab2.store.client.session());
    println!("  ParityComplete by session: {complete:?} (tab 1 {s1}, tab 2 {s2})");
    assert_eq!(complete.get(&Some(s1)).copied().unwrap_or(0), 1, "tab 1 was not told its parity completed: {complete:?}");
    assert_eq!(complete.get(&Some(s2)).copied().unwrap_or(0), 1, "tab 2 was not told its parity completed: {complete:?}");
}
