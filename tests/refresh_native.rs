//! Asking what changed, against a REAL engine.
//!
//! The browser test for this drives a FAKE session written in JavaScript. It
//! is worth having — it is the path an app takes — but it compiles the Rust
//! and runs none of it: no file under `tests/` mentioned `ChangesSince` at
//! all, so ~175 lines that decide when to ask, which answer belongs to which
//! domain, and what to do when the engine cannot compute a delta were never
//! executed by anything.
//!
//! So this drives `Refresh` against a real `Shell` over a real block store,
//! two clients on one node — which is what a second tab IS: its own delegate
//! context, the same node underneath.

use craftworks_sdk::{Answer, CachedStore, Loads, Refresh, Store as _};
use engine_delegate::shell::Inbound;

/// A client: a `CachedStore` whose traffic crosses a real engine.
struct Client {
    store: CachedStore,
    conn: testkit::Conn,
    /// Every `ChangesSince` this client sent. The measurement.
    asks: usize,
    /// Full reloads `Refresh` decided on.
    reloads: usize,
    /// What `ChangesSince` was ANSWERED with — the reply type, which is the
    /// assertion every earlier test of this path lacked (sdk#142).
    deltas: usize,
    refused: usize,
    /// Entries handed to this client: rows in pages plus changes in deltas.
    /// What a notification COSTS it, whatever the engine had cached.
    received: usize,
}

/// The domain's range, from the ONE function that knows the layout.
///
/// Hand-written, these were wrong: the record tag is not `b'd'`, so the range
/// contained none of the keys and every scan was empty. A test that encodes
/// the key layout is a second copy of it, and a wrong second copy fails in a
/// way that looks like the code under test.
fn range() -> (Vec<u8>, Vec<u8>) {
    craftworks_sdk::Db::<CachedStore, craftworks_sdk::SystemEnv>::domain_range("note")
}

/// A request id for a refresh. The session takes it from the ONE counter its
/// loads use (`Loads::take_id`, sdk#166); this harness makes a fresh `Loads`
/// per load, so its refreshes number from far above any of them instead.
fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn key(n: u32) -> Vec<u8> {
    let (lo, _) = range();
    let mut k = lo;
    k.extend_from_slice(format!("{n:012}").as_bytes());
    k
}

impl Client {
    /// A client with its OWN delegate context: a second DEVICE.
    fn new(node: &testkit::FullNode) -> Client {
        Client::on(node.connect())
    }

    /// A client sharing a delegate context: a second TAB on one node.
    fn tab(conn: testkit::Conn) -> Client {
        Client::on(conn)
    }

    fn on(conn: testkit::Conn) -> Client {
        let mut c = Client {
            store: testkit::cached_store().0,
            conn,
            asks: 0,
            reloads: 0,
            deltas: 0,
            refused: 0,
            received: 0,
        };
        // START THE ENGINE. `Identity` is also how a session begins: answering
        // "where is your head" requires reading it, so the head read goes out
        // with this call. Without it this client's engine stands on nothing
        // and every scan is empty — which is not the code under test failing,
        // it is the harness never having connected.
        c.store.client.send(&protocol::Request::Identity);
        c.pump();
        c
    }

    /// Send whatever is queued and feed every reply back. Returns the replies.
    fn pump(&mut self) -> Vec<Vec<u8>> {
        let mut all = Vec::new();
        for frame in self.store.take_outbound() {
            if let protocol::Incoming::Ok(env) = protocol::decode_request(&frame) {
                if matches!(env.body, protocol::Request::ChangesSince { .. }) {
                    self.asks += 1;
                }
            }
            for reply in self.conn.step(vec![Inbound::Client(frame)]) {
                match protocol::decode_reply(&reply) {
                    Ok(protocol::Reply::Page { entries, .. }) => self.received += entries.len(),
                    Ok(protocol::Reply::Delta { changes, .. }) => self.received += changes.len(),
                    _ => {}
                }
                self.store.on_inbound(&reply);
                all.push(reply);
            }
        }
        all
    }

    /// Many keys as ONE write, so a large domain is set up in one commit.
    fn write_many(&mut self, keys: impl Iterator<Item = Vec<u8>>, value: &[u8]) {
        // In batches of 100: measured here, one write of a thousand keys left
        // the domain EMPTY, with nothing said. So each batch asserts the copy
        // refused nothing, and the caller asserts the rows arrived.
        let keys: Vec<Vec<u8>> = keys.collect();
        for chunk in keys.chunks(100) {
            let edits: Vec<_> = chunk.iter().map(|k| (k.clone(), craftworks_sdk::Edit::Put(value.to_vec()))).collect();
            self.store.apply_batch(&edits).expect("the store took the write");
            assert!(self.store.refused.is_empty(), "the copy refused a set-up write: {:?}", self.store.refused);
            self.pump();
        }
    }

    fn write(&mut self, key: &[u8], value: &[u8]) {
        self.store.put(key, value);
        self.pump();
    }

    /// Load the whole domain, as a binding's first read does, and — as the
    /// session does — tell `Refresh` the root it was read at (sdk#142).
    fn load_into(&mut self, r: &mut Refresh) {
        let (lo, hi) = range();
        self.load_span_into(r, &lo, &hi);
    }

    /// Load any span — a domain, or one parent's band — and seed `Refresh`
    /// with the watch key it names, as the session does (sdk#137).
    fn load_span_into(&mut self, r: &mut Refresh, lo: &[u8], hi: &[u8]) {
        if let Some((lo, hi, root)) = self.load_span(lo, hi) {
            if let Some(d) = craftworks_sdk::Db::<CachedStore, craftworks_sdk::SystemEnv>::watch_key_of_range(&lo, &hi) {
                r.on_loaded(&d, root);
            }
        }
    }

    /// Load the whole domain, as a binding's first read does.
    fn load(&mut self) {
        let _ = self.load_at();
    }

    /// Load the whole domain; the span and root it completed at, if it did.
    fn load_at(&mut self) -> Option<(Vec<u8>, Vec<u8>, [u8; 32])> {
        let (lo, hi) = range();
        self.load_span(&lo, &hi)
    }

    /// Load `[lo, hi)`; the span and root it completed at, if it did.
    fn load_span(&mut self, lo: &[u8], hi: &[u8]) -> Option<(Vec<u8>, Vec<u8>, [u8; 32])> {
        let mut done = None;
        let mut loads = Loads::new();
        let (id, _) = loads.want(lo, hi, 0).expect("a fresh span");
        let mut pending = Some(Loads::range_request(id, lo, hi, None));
        while let Some(req) = pending.take() {
            self.store.client.send(&req);
            for reply in self.pump() {
                if let Ok(protocol::Reply::Page {
                    req_id,
                    entries,
                    cursor,
                    at,
                    ..
                }) = protocol::decode_reply(&reply)
                {
                    // The page's OWN head, as the session passes it: that is
                    // the root a completed load records (sdk#142).
                    match loads.on_page(req_id, entries, cursor, at) {
                        craftworks_sdk::loads::Page::More { lo, hi, after } => {
                            pending = Some(Loads::range_request(req_id, &lo, &hi, Some(after)));
                        }
                        craftworks_sdk::loads::Page::Complete { lo, hi, rows, at } => {
                            if std::env::var("DIAG").is_ok() {
                                println!("    load complete: {} row(s)", rows.len());
                            }
                            self.store.on_page(&lo, &hi, rows, [0u8; 32]);
                            done = Some((lo, hi, at.root));
                        }
                        // The tree moved under this load: ask again from
                        // the top of the range.
                        craftworks_sdk::loads::Page::Restart { lo, hi } => {
                            pending = Some(Loads::range_request(req_id, &lo, &hi, None));
                        }
                        craftworks_sdk::loads::Page::Nothing => {}
                    }
                }
            }
        }
        done
    }

    /// How many rows this client can SEE in the domain.
    ///
    /// **`NotLoaded` is not zero.** The first version of this said
    /// `.unwrap_or(0)`, which collapsed "I have not looked" into "there is
    /// nothing there" — the exact distinction this whole layer exists to
    /// keep, erased in the harness that was meant to test it. A harness that
    /// hides the failure it is looking for reports the code as broken when
    /// the code is fine, or the reverse.
    fn rows(&mut self) -> usize {
        use craftworks_sdk::store::Reads;
        // `usize::MAX`, not 0. At the STORE level the limit is literal and 0
        // means zero rows; "0 = no limit" is a `Scan` convention that `Db`
        // translates. I suspected a bug here and checked: `Db::scan` does the
        // translation and both stores agree. The test was wrong.
        match self.store.scan(&range().0, &range().1, false, usize::MAX) {
            Ok(r) => r.len(),
            Err(e) => panic!("this client cannot answer at all: {e}"),
        }
    }

    /// Ask what changed, feed the answer back through `Refresh`, and say
    /// which domains moved.
    /// Give the engine a moment of TIME.
    ///
    /// A delegate has no clock: it re-reads its head, retries and flushes
    /// only when something outside sends it time. Without this B's engine
    /// stands for ever on the head it read when it started.
    fn tick(&mut self) {
        self.store.client.send(&protocol::Request::Tick { now: 1 });
        self.pump();
    }

    fn refresh(&mut self, r: &mut Refresh) -> Vec<String> {
        if let Some(req) = r.ask(next_id(), "note", &range().0, &range().1) {
            self.store.client.send(&req);
        }
        for reply in self.pump() {
            match protocol::decode_reply(&reply) {
                Ok(protocol::Reply::Delta {
                    req_id,
                    changes,
                    cursor,
                    new_root,
                    ..
                }) => {
                    self.deltas += 1;
                    let a = r.on_delta(req_id, changes, cursor, new_root);
                    self.answer(r, a)
                }
                Ok(protocol::Reply::FullReloadRequired { req_id, .. }) => {
                    self.refused += 1;
                    let a = r.on_full_reload(req_id);
                    self.answer(r, a)
                }
                _ => {}
            }
        }
        r.take_changed()
    }

    /// Do what `Refresh` decided, as the session does: apply a delta, or
    /// forget the range and load it again — INSTEAD, never after.
    fn answer(&mut self, r: &mut Refresh, a: Answer) {
        match a {
            Answer::Delta { changes, new_root, .. } => {
                self.store.on_delta(changes, new_root);
            }
            Answer::Reload { .. } => {
                self.reloads += 1;
                self.store.copy.forget(&range().0, &range().1);
                self.load_into(r);
            }
            Answer::NotOurs => {}
        }
    }
}

/// **TWO DEVICES: the second does NOT see the first's write.** Ignored, and
/// it is not a phase-3 defect.
///
/// Each client here has its OWN delegate context, which is what two DEVICES
/// are: two nodes, two delegates, two engine states. It is not what two tabs
/// are — that was MEASURED (`probe/src/bin/two-connections.rs`) and two
/// connections to one node share one context, which is why
/// `two_tabs_on_one_node_see_each_others_writes` passes.
///
/// What this measures, with every reply printed:
///
/// * B asks `ChangesSince` from the root it last saw — all zeroes, having
///   never been told one.
/// * The engine answers **`FullReloadRequired`** with a real, non-zero
///   `new_root`. Correct: it cannot diff from a root it does not know.
/// * The re-load returns **0 rows**, though the engine has just named the
///   root at which A's row exists.
///
/// So an engine answers a range read from the root it STANDS on, and nothing
/// makes it adopt a newer head IT DID NOT WRITE. `Request::Tick` was tried
/// and changed nothing.
///
/// That is a real design item and it is multi-device (sdk#78, phase 6), not
/// this phase. The hard half is the case where the adopting engine holds
/// unpublished writes of its own.
///
/// Kept rather than deleted, because it is the measurement.
/// **TWO DEVICES: the second does NOT see the first's write.**
///
/// Ignored, and it is NOT a phase-3 defect.
///
/// Each client here has its OWN delegate context, which is what two DEVICES
/// are: two nodes, two delegates, two engine states. It is not what two tabs
/// are — that was MEASURED (`probe/src/bin/two-connections.rs`): two
/// connections to one node share one context, which is why
/// `two_tabs_on_one_node_see_each_others_writes` passes.
///
/// What this measures, with every reply printed:
///
/// * B asks `ChangesSince` from the root it last saw — all zeroes, having
///   never been told one.
/// * The engine answers **`FullReloadRequired`** with a real, non-zero
///   `new_root`. Correct: it cannot diff from a root it does not know.
/// * The re-load returns **0 rows**, though the engine has just named the
///   root at which A's row exists.
///
/// So an engine answers a range read from the root it STANDS on, and nothing
/// makes it adopt a newer head IT DID NOT WRITE. `Request::Tick` was tried
/// and changed nothing. That is a real design item, and it is multi-device
/// (sdk#78, phase 6). The hard half is the case where the adopting engine
/// holds unpublished writes of its own.
///
/// Kept rather than deleted, because it IS the measurement.
#[test]
#[ignore = "TWO DEVICES, not two tabs: an engine does not adopt a head it did not write (sdk#78, phase 6)"]
fn a_second_device_does_not_see_the_first_ones_write() {
    let node = testkit::FullNode::new();
    let mut a = Client::new(&node); // its OWN context
    let mut b = Client::new(&node); // and its own

    b.load();
    assert_eq!(b.rows(), 0);
    a.write(&key(1), b"first");

    let mut r = Refresh::new();
    b.tick();
    let _ = b.refresh(&mut r);

    assert_eq!(
        b.rows(),
        1,
        "the second device never adopted the head the first wrote"
    );
}

/// THE CONTROL: a client that does NOT ask sees nothing new.
///
/// Without this the test above passes against a copy that somehow refreshed
/// itself — and the whole point is that reading alone never can.
#[test]
fn control_a_client_that_never_asks_sees_nothing_new() {
    let node = testkit::FullNode::new();
    let mut a = Client::new(&node);
    let mut b = Client::new(&node);

    b.load();
    assert_eq!(b.rows(), 0);

    a.write(&key(1), b"first");
    // B pumps its connection — it simply never ASKS.
    b.pump();

    assert_eq!(
        b.rows(),
        0,
        "B saw a row without asking for it, so the test above proves nothing \
         about asking"
    );
}

/// **TWO TABS: B, WHICH WROTE NOTHING, SEES A'S ROW BY ASKING.**
///
/// The shape a node actually gives them: one delegate, ONE context, two
/// connections. Measured, not assumed — see `Conn`.
#[test]
fn two_tabs_on_one_node_see_each_others_writes() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn.clone());

    b.load();
    assert_eq!(b.rows(), 0, "the domain should start empty");
    let asks_before = b.asks;

    a.write(&key(1), b"first");

    // B is told the head moved and asks what changed. Nothing else happens
    // to B: it made no write and issued no read of its own.
    let mut r = Refresh::new();
    let changed = b.refresh(&mut r);

    assert!(b.asks > asks_before, "B did not send ChangesSince");
    assert_eq!(
        b.rows(),
        1,
        "B asked, was answered, and still cannot see the row A wrote"
    );
    assert_eq!(
        changed,
        vec!["note".to_string()],
        "the answer did not report the domain as moved, so no binding would re-run"
    );
}

/// THE CONTROL for the tab case: without asking, B sees nothing.
#[test]
fn control_a_tab_that_never_asks_sees_nothing_new() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn.clone());

    b.load();
    a.write(&key(1), b"first");
    b.pump(); // B turns its connection over; it never ASKS

    assert_eq!(
        b.rows(),
        0,
        "B saw a row without asking, so the test above proves nothing about asking"
    );
}

/// A second notification while a question is outstanding sends nothing.
#[test]
fn one_question_per_domain_at_a_time() {
    let mut r = Refresh::new();
    assert!(
        r.ask(next_id(), "note", &range().0, &range().1).is_some(),
        "the first ask was refused"
    );
    assert!(
        r.ask(next_id(), "note", &range().0, &range().1).is_none(),
        "a second question went out while one was outstanding; a tree somebody \
         is writing to would multiply its traffic by how often it is written"
    );
    assert_eq!(r.in_flight(), 1);
}

/// An EMPTY delta reports no domain changed.
///
/// A binding re-run on one would re-render every screen on every
/// notification about anything.
#[test]
fn an_empty_delta_moves_nothing() {
    let mut r = Refresh::new();
    let protocol::Request::ChangesSince { req_id, .. } =
        r.ask(next_id(), "note", &range().0, &range().1).expect("an ask")
    else {
        unreachable!()
    };
    let a = r.on_delta(req_id, Vec::new(), None, [7u8; 32]);
    assert!(matches!(a, Answer::Delta { moved: false, .. }));
    assert!(
        r.take_changed().is_empty(),
        "an empty delta reported a change"
    );
    // The root still moved: the client now stands where the engine says.
    assert_eq!(r.seen("note"), Some([7u8; 32]));
}

/// `FullReloadRequired` forgets the root, so the next ask starts from zero
/// rather than from one the engine has said it cannot reconcile.
#[test]
fn a_full_reload_forgets_the_root_it_could_not_reconcile() {
    let mut r = Refresh::new();
    let protocol::Request::ChangesSince { req_id, .. } =
        r.ask(next_id(), "note", &range().0, &range().1).expect("an ask")
    else {
        unreachable!()
    };
    r.on_delta(
        req_id,
        vec![(b"k".to_vec(), Some(b"v".to_vec()))],
        None,
        [9u8; 32],
    );
    assert_eq!(r.seen("note"), Some([9u8; 32]));
    let _ = r.take_changed();

    let protocol::Request::ChangesSince { req_id, from, .. } =
        r.ask(next_id(), "note", &range().0, &range().1).expect("an ask")
    else {
        unreachable!()
    };
    assert_eq!(from, [9u8; 32], "it did not ask from the root it last saw");

    assert!(matches!(r.on_full_reload(req_id), Answer::Reload { .. }));
    assert_eq!(
        r.seen("note"),
        None,
        "it kept a root the engine said it cannot reconcile from, so the next \
         ask would fail the same way"
    );
    assert_eq!(r.take_changed(), vec!["note".to_string()]);
}

/// An answer to a question this client never asked changes nothing.
#[test]
fn an_answer_nobody_asked_for_is_counted_and_not_applied() {
    let mut r = Refresh::new();
    let a = r.on_delta(999, vec![(b"k".to_vec(), Some(b"v".to_vec()))], None, [1u8; 32]);
    assert_eq!(a, Answer::NotOurs);
    assert_eq!(r.unasked, 1);
    assert!(r.take_changed().is_empty());
    assert_eq!(r.seen("note"), None, "it moved a root on a stranger's word");
}

// ---- a CURSORED delta is not a complete one (craftworks-sdk#140) -------------

/// What `ChangesSince` answered.
struct Asked {
    req_id: u64,
    changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    cursor: Option<Vec<u8>>,
    new_root: [u8; 32],
}

/// B loaded `note` — which records the root it was loaded at (sdk#142) — A
/// then wrote `n` rows, and B asks what changed.
///
/// Before sdk#142 this had to be SEEDED at the wire: `Refresh` recorded a
/// root only on a delta and got a delta only from a recorded root, so it
/// never received one at all, which is how sdk#140 hid. Now the completed
/// load seeds it, as the session does.
fn changed_since_load(n: u32) -> (Client, Refresh, Asked) {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn);
    a.write(&key(0), b"before");
    let mut r = Refresh::new();
    b.load_into(&mut r);
    assert!(r.seen("note").is_some(), "the completed load recorded no root");
    for i in 1..=n {
        a.write(&key(i), b"row");
    }
    let req = r.ask(next_id(), "note", &range().0, &range().1).expect("Refresh asked nothing");
    b.store.client.send(&req);
    let asked = b
        .pump()
        .iter()
        .find_map(|x| match protocol::decode_reply(x) {
            Ok(protocol::Reply::Delta { req_id, changes, cursor, new_root, .. }) => {
                Some(Asked { req_id, changes, cursor, new_root })
            }
            _ => None,
        })
        .expect("VACUITY GUARD: no Reply::Delta came back, so the path under test never ran");
    (b, r, asked)
}

/// **A delta of 300 changes is paged at 256, and page 1 is not the answer.**
///
/// RED before the fix, measured: page 1 applied, `seen` advanced to its
/// `new_root`, and B saw 257 of 301 rows — 44 stale under a root claiming to
/// be current, and nothing would ever have asked for them.
#[test]
fn a_cursored_delta_reloads_the_domain_instead_of_standing_on_page_one() {
    let (mut b, mut r, d) = changed_since_load(300);
    assert!(d.cursor.is_some(), "VACUITY GUARD: 300 changes came back in one page, so no cursor was ever handled");
    assert_eq!(d.changes.len(), 256);
    let answer = r.on_delta(d.req_id, d.changes, d.cursor, d.new_root);
    assert!(matches!(answer, Answer::Reload { .. }), "a first page was taken for the whole answer: {answer:?}");
    // Before the reload completes, NEITHER place a root is kept claims the
    // new one: not `Refresh`, and not the copy.
    assert_eq!(r.seen("note"), None, "seen advanced on a partial page");
    assert_ne!(b.store.copy.root(), Some(d.new_root), "the copy recorded a root it is not at");
    b.answer(&mut r, answer);
    assert_eq!(b.rows(), 301, "after the reload the copy equals the tree");
    assert_eq!(r.take_changed(), vec!["note".to_string()], "and the domain is reported as moved");
}

/// THE CONTROL: fewer changes than a page is ONE delta, applied — no reload.
///
/// Or the fix has quietly turned every delta into a reload.
#[test]
fn control_a_delta_under_a_page_is_applied_and_reloads_nothing() {
    let (mut b, mut r, d) = changed_since_load(200);
    assert!(d.cursor.is_none());
    assert_eq!(d.changes.len(), 200);
    let new_root = d.new_root;
    let answer = r.on_delta(d.req_id, d.changes, d.cursor, d.new_root);
    assert!(matches!(answer, Answer::Delta { .. }), "{answer:?}");
    b.answer(&mut r, answer);
    assert_eq!(b.reloads, 0, "a delta that fit in a page was answered with a reload");
    assert_eq!(r.seen("note"), Some(new_root), "and seen advanced exactly to it");
    assert_eq!(b.rows(), 201);
}

/// THE BOUNDARY: exactly one page of changes. What the engine says here is
/// asserted, not assumed — "256 means more" would be a guess.
#[test]
fn a_delta_of_exactly_one_page_is_complete_when_the_engine_says_so() {
    let (mut b, mut r, d) = changed_since_load(256);
    assert_eq!(d.changes.len(), 256);
    let complete = d.cursor.is_none();
    let answer = r.on_delta(d.req_id, d.changes, d.cursor, d.new_root);
    b.answer(&mut r, answer);
    println!("  256 changes: cursor {}, reloads {}", if complete { "None" } else { "Some" }, b.reloads);
    assert_eq!(b.reloads, if complete { 0 } else { 1 }, "the answer follows the cursor, and only the cursor");
    assert_eq!(b.rows(), 257);
    // Measured: this engine sends a cursor at exactly a full page, since it
    // cannot know there is no 257th without looking. So the other half of the
    // boundary — a full page with NO cursor is complete, not "probably more" —
    // is pinned on `Refresh` directly.
    let mut r = Refresh::new();
    let Some(protocol::Request::ChangesSince { req_id, .. }) = r.ask(next_id(), "note", &range().0, &range().1) else { unreachable!() };
    let page: Vec<_> = (0..256).map(|i| (key(i), Some(b"v".to_vec()))).collect();
    assert!(matches!(r.on_delta(req_id, page, None, [3u8; 32]), Answer::Delta { .. }), "256 is not taken to mean more");
    assert_eq!(r.seen("note"), Some([3u8; 32]));
}

// ---- the delta path RUNS (craftworks-sdk#142) ---------------------------------

const ZERO: [u8; 32] = [0u8; 32];

/// Two tabs; `rows` rows written by A; B loaded the domain (seeding `seen`
/// only if `seed`); A then changes one row; B is asked to refresh.
fn one_change(rows: u32, seed: bool) -> (Client, Refresh, testkit::Conn, usize, usize) {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn.clone());
    a.write_many((0..rows).map(key), b"v1");
    let mut r = Refresh::new();
    if seed { b.load_into(&mut r) } else { b.load() }
    assert_eq!(b.rows(), rows as usize, "the domain was set up");
    a.write(&key(0), b"v2");
    let gets = conn.served(testkit::full_node::Served::Get);
    let zero = conn.fetches_of(&ZERO);
    b.received = 0;
    let _ = b.refresh(&mut r);
    (b, r, conn, gets, zero)
}

/// **After one completed load, the next refresh is answered by a DELTA.**
///
/// RED before this change: `seen` was never set, the question went out from
/// the zero root, and the answer was `FullReloadRequired` — every time.
#[test]
fn after_a_completed_load_the_next_refresh_is_a_delta() {
    let (mut b, r, conn, _, zero_before) = one_change(3, true);
    assert_eq!((b.deltas, b.refused), (1, 0), "the reply TYPE: a Delta, and no refusal");
    assert_eq!(b.reloads, 0, "and nothing was reloaded");
    assert_eq!(conn.fetches_of(&ZERO) - zero_before, 0, "no fetch of the block that cannot exist");
    assert_eq!(b.rows(), 3);
    assert!(r.seen("note").is_some());
}

/// THE CONTROL, and the old behaviour: a load that records no root makes
/// the question go out from zero, refused after fetches of block `00…0`.
#[test]
fn control_with_no_recorded_root_the_engine_refuses_and_the_domain_reloads() {
    let (b, _, conn, _, zero_before) = one_change(3, false);
    assert_eq!((b.deltas, b.refused, b.reloads), (0, 1, 1));
    println!("  fetches of block 00…0 for one refresh from zero: {}", conn.fetches_of(&ZERO) - zero_before);
    assert!(conn.fetches_of(&ZERO) > zero_before, "the doomed fetches this change removes");
}

/// A root the engine genuinely cannot reach still falls back to a reload —
/// the fix did not turn the fallback into a hang.
#[test]
fn an_unobtainable_old_root_still_falls_back_to_a_full_reload() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn);
    a.write(&key(0), b"v1");
    let mut r = Refresh::new();
    b.load_into(&mut r);
    r.on_loaded("note", [9u8; 32]);   // a root no block exists for
    a.write(&key(1), b"v1");
    let _ = b.refresh(&mut r);
    assert_eq!((b.refused, b.reloads), (1, 1));
    assert_eq!(b.rows(), 2);
    assert_ne!(r.seen("note"), Some([9u8; 32]), "the unreachable root is not kept as a starting point");
    assert!(r.seen("note").is_some(), "and the reload recorded the root it was read at");
}

/// **The cost, as a number**: what ONE change notification over a
/// 1,000-record domain costs the tab, from zero (before) and from the loaded
/// root (after).
#[test]
fn a_change_notification_over_a_thousand_rows_costs_a_delta_not_a_reload() {
    let (old, _, before_conn, before_gets, _) = one_change(1000, false);
    let before = before_conn.served(testkit::full_node::Served::Get) - before_gets;
    let (new, _, after_conn, after_gets, _) = one_change(1000, true);
    let after = after_conn.served(testkit::full_node::Served::Get) - after_gets;
    println!("  per change notification, 1,000 rows: blocks fetched {before} -> {after}; entries handed to the tab {} -> {}", old.received, new.received);
    assert_eq!(new.deltas, 1);
    assert_eq!(old.received, 1000, "before: the whole domain, again");
    assert_eq!(new.received, 1, "after: the one change");
    assert!(after < before, "and no doomed fetch: {after} vs {before}");
}

/// A copy brought up to date BY DELTA and one brought up by a FULL RELOAD
/// hold equal rows — byte for byte, which is what the JS `sameRows` compares.
#[test]
fn a_delta_applied_copy_and_a_full_reload_hold_equal_rows() {
    use craftworks_sdk::store::Reads;
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn.clone());
    a.write_many((0..10).map(key), b"v1");
    let mut r = Refresh::new();
    b.load_into(&mut r);
    a.write(&key(3), b"v2");
    a.store.delete(&key(4));
    a.pump();
    a.write(&key(20), b"new");
    let _ = b.refresh(&mut r);
    assert_eq!(b.deltas, 1, "B came up to date by delta");
    let mut c = Client::tab(conn);
    c.load();
    let by_delta = b.store.scan(&range().0, &range().1, false, usize::MAX).unwrap();
    let by_reload = c.store.scan(&range().0, &range().1, false, usize::MAX).unwrap();
    assert_eq!(by_delta, by_reload);
    assert_eq!(by_delta.len(), 10);
}

// ---- a LIVE BAND is told of its band, not of a sibling's (sdk#137) ----------

type Dbx = craftworks_sdk::Db<CachedStore, craftworks_sdk::SystemEnv>;

fn band_key(parent: u8, n: u32) -> Vec<u8> {
    let (lo, _) = Dbx::parent_range("note", &[parent; 16]);
    let mut k = lo;
    k.extend_from_slice(&[0u8; 12]);
    k.extend_from_slice(&n.to_be_bytes());
    k
}

/// **B watches parent P's band. A write under sibling Q is NOT a change to it;
/// a write under P is.** The second half is the control: without it a band
/// told of everything would pass.
#[test]
fn a_band_watch_is_told_of_its_band_and_not_of_a_siblings() {
    let node = testkit::FullNode::new();
    let conn = node.connect();
    let mut a = Client::tab(conn.clone());
    let mut b = Client::tab(conn);
    let p = Dbx::watch_key("note", Some(&[1u8; 16]));
    let (lo, hi) = Dbx::watch_range(&p).expect("a band key names a range");
    a.write(&band_key(1, 0), b"p0");
    a.write(&band_key(2, 0), b"q0");
    // B loads its band and records where it stands (as a session does).
    let mut r = Refresh::new();
    let ask = |r: &mut Refresh, b: &mut Client| -> Vec<String> {
        if let Some(req) = r.ask(next_id(), &p, &lo, &hi) {
            b.store.client.send(&req);
        }
        for reply in b.pump() {
            match protocol::decode_reply(&reply) {
                Ok(protocol::Reply::Delta { req_id, changes, cursor, new_root, .. }) => {
                    if let Answer::Delta { changes, new_root, .. } = r.on_delta(req_id, changes, cursor, new_root) {
                        b.store.on_delta(changes, new_root);
                    }
                }
                Ok(protocol::Reply::FullReloadRequired { req_id, .. }) => {
                    let _ = r.on_full_reload(req_id);
                }
                _ => {}
            }
        }
        r.take_changed()
    };
    // B loads its BAND — as a parented binding's `children` read does — and
    // the completed load names the band, so its root is the one deltas start
    // from (the session does the same, via `watch_key_of_range`).
    b.load_span_into(&mut r, &lo, &hi);
    assert!(r.seen(&p).is_some(), "the band's load recorded no root, so every question would be a reload");

    a.write(&band_key(2, 1), b"q1"); // under the SIBLING
    assert_eq!(ask(&mut r, &mut b), Vec::<String>::new(), "a write under a sibling parent was reported as a change to this band");

    a.write(&band_key(1, 1), b"p1"); // under P
    assert_eq!(ask(&mut r, &mut b), vec![p.clone()], "a write in the band was not reported");
}

/// The key layout round-trips, and a string that is not a key names nothing.
#[test]
fn a_watch_key_names_its_range_and_nothing_else_does() {
    assert_eq!(Dbx::watch_range("note"), Some(Dbx::domain_range("note")));
    let k = Dbx::watch_key("note", Some(&[7u8; 16]));
    assert_eq!(Dbx::watch_range(&k), Some(Dbx::parent_range("note", &[7u8; 16])));
    for bad in ["", "No", "note#", "note#xyz", "note#0123", "a#b#c"] {
        assert_eq!(Dbx::watch_range(bad), None, "{bad:?}");
    }
}
