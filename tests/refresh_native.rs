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
use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// One head for every page: these tests do not move the tree, and a
/// constant says so rather than leaving it to be inferred.
const AT: protocol::At = protocol::At {
    seq: 1,
    root: [1u8; 32],
};

#[derive(Clone, Default)]
struct Node(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for Node {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl Node {
    fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
}

/// The head Register, shared by both clients, as the node holds it.
#[derive(Clone, Default)]
struct Head(Rc<RefCell<Option<(u64, Cid)>>>);

/// THE DELEGATE, as a node has it: ONE of them.
///
/// Two tabs on one node invoke the same delegate — same code, same
/// parameters, so the same key — and it is rebuilt from its context on every
/// call (F32). MEASURED: two separate websocket connections to one node
/// share that context. `probe/src/bin/two-connections.rs` counts calls in the
/// context and connection B's first call continued connection A's count
/// (6 after A reached 5), with A's next call seeing 7, while `calls_in_memory`
/// stayed 1 on both as the control requires.
///
/// So two tabs are served by ONE engine state, and `Conn` is shared.
#[derive(Clone)]
struct Conn(Rc<RefCell<ConnState>>);

struct ConnState {
    node: Node,
    head: Head,
    ctx: Vec<u8>,
}

impl Conn {
    fn new(node: &Node, head: &Head) -> Conn {
        Conn(Rc::new(RefCell::new(ConnState {
            node: node.clone(),
            head: head.clone(),
            ctx: Vec::new(),
        })))
    }

    fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        let (ctx, node) = {
            let s = self.0.borrow();
            (s.ctx.clone(), s.node.clone())
        };
        let mut shell: Shell<Node> = Shell::resume_with(
            &ctx,
            engine::Params::default(),
            node,
            StoreFacts::provisioned(),
        );
        let out = shell.handle(inbound);
        self.0.borrow_mut().ctx = shell.to_context().expect("a context after every call");
        let mut next = Vec::new();
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    self.0.borrow().node.put(id, &bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = self.0.borrow().node.get(&id).map(|b| b.to_vec());
                    next.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    let head = self.0.borrow().head.clone();
                    let mut h = head.0.borrow_mut();
                    if h.is_none_or(|(s, _)| seq > s) {
                        *h = Some((seq, root));
                    }
                    let (seq, root) = h.expect("just written");
                    drop(h);
                    next.push(Inbound::GotHead { seq, root });
                }
                engine_delegate::schedule::Op::ReadHead { .. } => {
                    match *self.0.borrow().head.0.borrow() {
                        Some((seq, root)) => next.push(Inbound::GotHead { seq, root }),
                        None => next.push(Inbound::NoHead),
                    }
                }
            }
        }
        let mut replies = out.replies;
        if !next.is_empty() {
            replies.extend(self.step(next));
        }
        replies
    }
}

/// A client: a `CachedStore` whose traffic crosses a real engine.
struct Client {
    store: CachedStore,
    conn: Conn,
    /// Every `ChangesSince` this client sent. The measurement.
    asks: usize,
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

fn key(n: u32) -> Vec<u8> {
    let (lo, _) = range();
    let mut k = lo;
    k.extend_from_slice(format!("{n:012}").as_bytes());
    k
}

impl Client {
    /// A client with its OWN delegate context: a second DEVICE.
    fn new(node: &Node, head: &Head) -> Client {
        Client::on(Conn::new(node, head))
    }

    /// A client sharing a delegate context: a second TAB on one node.
    fn tab(conn: Conn) -> Client {
        Client::on(conn)
    }

    fn on(conn: Conn) -> Client {
        let mut c = Client {
            store: CachedStore::new(Box::new(|| 0)),
            conn,
            asks: 0,
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
                self.store.on_inbound(&reply);
                all.push(reply);
            }
        }
        all
    }

    fn write(&mut self, key: &[u8], value: &[u8]) {
        self.store.put(key, value);
        self.pump();
    }

    /// Load the whole domain, as a binding's first read does.
    fn load(&mut self) {
        let mut loads = Loads::new();
        let (id, _) = loads.want(&range().0, &range().1, 0).expect("a fresh span");
        let mut pending = Some(Loads::range_request(id, &range().0, &range().1, None));
        while let Some(req) = pending.take() {
            self.store.client.send(&req);
            for reply in self.pump() {
                if let Ok(protocol::Reply::Page {
                    req_id,
                    entries,
                    cursor,
                    ..
                }) = protocol::decode_reply(&reply)
                {
                    match loads.on_page(req_id, entries, cursor, AT) {
                        craftworks_sdk::loads::Page::More { lo, hi, after } => {
                            pending = Some(Loads::range_request(req_id, &lo, &hi, Some(after)));
                        }
                        craftworks_sdk::loads::Page::Complete { lo, hi, rows } => {
                            if std::env::var("DIAG").is_ok() {
                                println!("    load complete: {} row(s)", rows.len());
                            }
                            self.store.on_page(&lo, &hi, rows, [0u8; 32]);
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
        if let Some(req) = r.ask("note", &range().0, &range().1) {
            self.store.client.send(&req);
        }
        for reply in self.pump() {
            match protocol::decode_reply(&reply) {
                Ok(protocol::Reply::Delta {
                    req_id,
                    changes,
                    new_root,
                    ..
                }) => {
                    if let Answer::Delta {
                        changes, new_root, ..
                    } = r.on_delta(req_id, changes, new_root)
                    {
                        self.store.on_delta(changes, new_root);
                    }
                }
                Ok(protocol::Reply::FullReloadRequired { req_id, .. }) => {
                    if let Answer::Reload { .. } = r.on_full_reload(req_id) {
                        self.store.copy.forget(&range().0, &range().1);
                        self.load();
                    }
                }
                _ => {}
            }
        }
        r.take_changed()
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
    let (node, head) = (Node::default(), Head::default());
    let mut a = Client::new(&node, &head); // its OWN context
    let mut b = Client::new(&node, &head); // and its own

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
    let (node, head) = (Node::default(), Head::default());
    let mut a = Client::new(&node, &head);
    let mut b = Client::new(&node, &head);

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
    let (node, head) = (Node::default(), Head::default());
    let conn = Conn::new(&node, &head);
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
    let (node, head) = (Node::default(), Head::default());
    let conn = Conn::new(&node, &head);
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
        r.ask("note", &range().0, &range().1).is_some(),
        "the first ask was refused"
    );
    assert!(
        r.ask("note", &range().0, &range().1).is_none(),
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
        r.ask("note", &range().0, &range().1).expect("an ask")
    else {
        unreachable!()
    };
    let a = r.on_delta(req_id, Vec::new(), [7u8; 32]);
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
        r.ask("note", &range().0, &range().1).expect("an ask")
    else {
        unreachable!()
    };
    r.on_delta(
        req_id,
        vec![(b"k".to_vec(), Some(b"v".to_vec()))],
        [9u8; 32],
    );
    assert_eq!(r.seen("note"), Some([9u8; 32]));
    let _ = r.take_changed();

    let protocol::Request::ChangesSince { req_id, from, .. } =
        r.ask("note", &range().0, &range().1).expect("an ask")
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
    let a = r.on_delta(999, vec![(b"k".to_vec(), Some(b"v".to_vec()))], [1u8; 32]);
    assert_eq!(a, Answer::NotOurs);
    assert_eq!(r.unasked, 1);
    assert!(r.take_changed().is_empty());
    assert_eq!(r.seen("note"), None, "it moved a root on a stranger's word");
}
