//! Two clients, one node: a write on A updates B's binding without B asking.
//!
//! The two clients are separate ENGINES over one block store, which is what a
//! second tab is: its own delegate context, its own subscriptions, and the
//! same node underneath. B never writes anything. It learns by re-reading the
//! head onto a root A published — the path a real second tab is actually on,
//! and the one an implementation that notified only from its own commit path
//! would fail while passing every single-engine test.
//!
//! What this file does NOT claim: that a notification always arrives. It does
//! not, by construction — the node's delivery is lossy, and a subscription can
//! be evicted at the cap without anyone being told. The backstop is what makes
//! a binding correct, and it has its own test here.

use craftworks_sdk::{Binding, EngineStore, LiveMode, Store as _, Transport};
use engine_delegate::shell::{Inbound, Shell};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// The node's block store, shared by both clients.
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

/// The head Register, as the node holds it: one per key, shared.
#[derive(Clone, Default)]
struct Head(Rc<RefCell<Option<(u64, Cid)>>>);

/// One client's connection: its own delegate context over the shared node.
struct Conn {
    node: Node,
    head: Head,
    ctx: Vec<u8>,
}

impl Conn {
    fn new(node: Node, head: Head) -> Conn {
        Conn {
            node,
            head,
            ctx: Vec::new(),
        }
    }

    fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        let mut shell: Shell<Node> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.node.clone(),
            true,
        );
        let out = shell.handle(inbound);
        self.ctx = shell.to_context().expect("a context after every call");
        assert_eq!(out.stranded, 0, "the shell stranded effects");
        let mut next = Vec::new();
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    self.node.put(id, &bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = self.node.get(&id).map(|b| b.to_vec());
                    next.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    // The node's Register takes the higher seq, as it does.
                    let mut h = self.head.0.borrow_mut();
                    if h.is_none_or(|(s, _)| seq > s) {
                        *h = Some((seq, root));
                    }
                    let (seq, root) = h.expect("just written");
                    drop(h);
                    next.push(Inbound::GotHead { seq, root });
                }
                engine_delegate::schedule::Op::ReadHead { .. } => match *self.head.0.borrow() {
                    Some((seq, root)) => next.push(Inbound::GotHead { seq, root }),
                    None => next.push(Inbound::NoHead),
                },
            }
        }
        let mut replies = out.replies;
        if !next.is_empty() {
            replies.extend(self.step(next));
        }
        replies
    }
}

impl Transport for Conn {
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
        self.step(vec![Inbound::Client(request.to_vec())])
    }
}

fn client(node: &Node, head: &Head) -> EngineStore<Conn> {
    let mut s = EngineStore::new(Conn::new(node.clone(), head.clone()));
    s.identity().expect("the engine answers who it is");
    s
}

/// Drive B's binding from whatever the engine has been told.
///
/// This is the whole client-side rule, and it is deliberately three lines: a
/// `Changed` is a TRIGGER, and what it triggers is the same reload the
/// backstop does. One code path means the rarely-exercised one is the one
/// that runs all the time.
fn pump(store: &mut EngineStore<Conn>, b: &mut Binding) -> usize {
    let mut woken = 0;
    for e in store.take_events() {
        if let craftworks_sdk::EngineEvent::Changed { sub_id, .. } = e {
            if Some(sub_id) == b.sub_id() {
                woken += 1;
            }
        }
    }
    if woken > 0 {
        b.reload(store).expect("reload");
    }
    woken
}

fn keys(b: &Binding) -> Vec<String> {
    b.snapshot()
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect()
}

/// A writes; B's list changes, and B never asked.
#[test]
fn a_write_on_one_client_updates_the_others_list_without_polling() {
    let (node, head) = (Node::default(), Head::default());
    let mut a = client(&node, &head);
    let mut b = client(&node, &head);

    // A seeds the list.
    a.put(b"list/01", b"first");

    // B binds the range, LIVE, and asks to be told.
    let mut view = Binding::new(b"list/", b"list0", true);
    let accepted = b.subscribe_range(7, b"list/", b"list0").expect("answered");
    view.note_subscribed(7, accepted);
    assert_eq!(
        view.mode(),
        LiveMode::Notified,
        "the engine did not accept the subscription, so the rest of this \
         test would be measuring the backstop instead"
    );
    view.reload(&mut b).expect("first read");
    assert_eq!(keys(&view), ["list/01"]);
    let before = view.snapshot();

    // A writes again. B does nothing but let its connection turn over.
    a.put(b"list/02", b"second");
    let woken = pump(&mut b, &mut view);

    assert!(
        woken > 0,
        "B was never told: it holds a subscription the engine accepted, and \
         a commit landed in its range"
    );
    assert_eq!(
        keys(&view),
        ["list/01", "list/02"],
        "B was told and its rows did not follow"
    );
    assert!(
        !std::rc::Rc::ptr_eq(&before, &view.snapshot()),
        "the rows changed and the snapshot did not, so nothing re-renders"
    );
    println!(
        "  B woken {woken}x, {} row(s), reloads {:?}",
        view.snapshot().len(),
        view.reloads
    );
}

/// THE CONTROL: a binding that is NOT live takes out no subscription, hears
/// nothing, and is still correct once reloaded.
///
/// Without this, the test above would equally describe an SDK that subscribes
/// to everything — which is the thing the design forbids, and which nothing
/// in a passing notification test would reveal.
#[test]
fn a_non_live_binding_takes_no_subscription_and_is_still_correct() {
    let (node, head) = (Node::default(), Head::default());
    let mut a = client(&node, &head);
    let mut b = client(&node, &head);

    a.put(b"list/01", b"first");

    let mut view = Binding::new(b"list/", b"list0", false);
    view.reload(&mut b).expect("first read");
    assert_eq!(keys(&view), ["list/01"]);
    assert_eq!(view.sub_id(), None, "a non-live binding subscribed");
    assert_eq!(view.mode(), LiveMode::Manual);

    a.put(b"list/02", b"second");

    // It is told nothing...
    let woken = pump(&mut b, &mut view);
    assert_eq!(woken, 0, "a non-live binding was notified");
    assert_eq!(
        keys(&view),
        ["list/01"],
        "a non-live binding updated without being reloaded"
    );

    // ...and a reload is all it takes.
    view.reload(&mut b).expect("reload");
    assert_eq!(
        keys(&view),
        ["list/01", "list/02"],
        "a non-live binding did not pick up the change when asked"
    );
}

/// The BACKSTOP: a live binding that is never told still catches up.
///
/// The notifications are thrown away here, which is exactly what the node
/// does when its channel is full or the subscription has been evicted. What
/// remains is a tick, a root comparison and a reload — and that has to be
/// sufficient on its own, because being told is an accelerator and nothing
/// more.
#[test]
fn a_live_binding_that_is_never_told_still_catches_up_on_its_tick() {
    let (node, head) = (Node::default(), Head::default());
    let mut a = client(&node, &head);
    let mut b = client(&node, &head);

    a.put(b"list/01", b"first");
    let mut view = Binding::new(b"list/", b"list0", true);
    let accepted = b.subscribe_range(7, b"list/", b"list0").expect("answered");
    view.note_subscribed(7, accepted);
    view.reload(&mut b).expect("first read");

    for i in 2..6u32 {
        a.put(format!("list/{i:02}").as_bytes(), b"more");
        // Every notification DROPPED, as the node's full channel drops them.
        let dropped = b.take_events().len();
        let _ = dropped;
    }

    // The tick. No notification has survived; this is all the binding has.
    view.reload(&mut b).expect("the backstop");
    assert_eq!(
        keys(&view),
        ["list/01", "list/02", "list/03", "list/04", "list/05"],
        "a live binding whose notifications were all dropped did not catch \
         up on its own tick — being told is an accelerator, so the backstop \
         is the thing that has to be correct"
    );
    println!(
        "  backstop caught up 4 missed commits, reloads {:?}",
        view.reloads
    );
}

/// A reload that finds nothing moved costs one root comparison and no reading.
///
/// The backstop runs on every tick of every live binding, so this is the
/// number that decides whether it is affordable at all.
#[test]
fn the_backstops_quiet_tick_reads_nothing() {
    let (node, head) = (Node::default(), Head::default());
    let mut a = client(&node, &head);
    let mut b = client(&node, &head);
    for i in 0..8u32 {
        a.put(format!("list/{i:02}").as_bytes(), b"v");
    }
    let mut view = Binding::new(b"list/", b"list0", true);

    // SETTLE first. The root moves once more after the first read — a
    // commit's head lands on a later exchange — so a binding that read
    // during the commit has one real delta still to take. Measuring the
    // quiet tick from there rather than from the first read is the
    // difference between measuring quiescence and measuring the tail of a
    // write.
    let mut settling = 0;
    loop {
        let before = view.reloads.unchanged;
        view.reload(&mut b).expect("settle");
        if view.reloads.unchanged > before {
            break;
        }
        settling += 1;
        assert!(settling < 8, "the tree never settled: {:?}", view.reloads);
    }
    let settled = view.reloads;
    println!("  settled after {settling} reload(s): {settled:?}");

    for _ in 0..10 {
        view.reload(&mut b).expect("tick");
    }
    assert_eq!(
        view.reloads.unchanged - settled.unchanged,
        10,
        "ten quiet ticks did not all short-circuit: {:?}",
        view.reloads
    );
    assert_eq!(
        (view.reloads.full, view.reloads.by_delta),
        (settled.full, settled.by_delta),
        "a quiet tick read the range"
    );
}
