//! The live notifier: a client subscription to a tree's HEAD contract.
//!
//! Every test here constructs the system through the probed fixture and
//! nothing else, and every assertion carries the recording — so a failure
//! already says what happened instead of needing a second run with prints
//! added. The vocabulary is `protocol::Step`, the same words the engine puts
//! on the wire, because one event vocabulary serves the tests, the harness's
//! tables, `db.trace` and the builder's viewer.

use craftworks_sdk::{Binding, HeadId, HeadWatch, LiveMode, Store, TreeStore, Trees};
use protocol::Step;

/// The node's client API, as this test sees it: contract subscriptions, and
/// the pushes they produce.
///
/// `accept` is what makes the CONTROL possible — a node that refuses the
/// subscription is the condition under which a live binding has nothing but
/// its tick, and it has to be reachable to be tested.
#[derive(Default)]
struct Watch {
    accept: bool,
    watched: Vec<HeadId>,
    pending: Vec<HeadId>,
    /// Every `watch` call, so a test can see a SECOND subscription being
    /// taken for a tree that already had one.
    calls: usize,
}

impl Watch {
    fn on() -> Watch {
        Watch {
            accept: true,
            ..Watch::default()
        }
    }
    fn refusing() -> Watch {
        Watch::default()
    }
    /// The node says this head moved.
    fn push(&mut self, head: HeadId) {
        if self.watched.contains(&head) {
            self.pending.push(head);
        }
    }
}

impl HeadWatch for Watch {
    fn watch(&mut self, head: HeadId) -> bool {
        self.calls += 1;
        if self.accept && !self.watched.contains(&head) {
            self.watched.push(head);
        }
        self.accept
    }
    fn moved(&mut self) -> Vec<HeadId> {
        std::mem::take(&mut self.pending)
    }
}

fn head(n: u8) -> HeadId {
    [n; 32]
}

fn put(s: &mut impl Store, k: &str, v: &str) {
    s.put(k.as_bytes(), v.as_bytes()).expect("the store took the write");
}

fn rows(t: &Trees, tree: usize, i: usize) -> usize {
    t.binding(tree, i).snapshot().len()
}

/// A push on the head alone updates a watcher. NO tick anywhere in this test.
#[test]
fn a_head_push_updates_a_live_binding_with_no_tick_at_all() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    let mut w = Watch::on();
    let mut trees = Trees::new();

    let i = trees.add(head(1), Binding::new(b"a/", b"b/", true), &mut w);
    // The first read goes through the probed boundary like every other, so
    // the recording covers the whole life of the binding rather than starting
    // once something has already happened.
    trees.tick(&mut store).expect("first read");
    assert_eq!(rows(&trees, 0, i), 1, "{}", trees.probe.dump());
    assert_eq!(
        trees.binding(0, i).mode(),
        LiveMode::HeadSubscribed,
        "the binding is not on the head subscription\n{}",
        trees.probe.dump()
    );

    // A foreign write moves the tree, and the node pushes the head.
    put(&mut store, "a/2", "two");
    w.push(head(1));
    let before = trees.probe.count(Step::Reloaded);

    let changed = trees.on_pushed(&mut w, &mut store).expect("pushed");
    assert_eq!(
        (changed, rows(&trees, 0, i)),
        (1, 2),
        "the push alone did not update the binding — and this test never \
         ticks, so the push is the only thing that could have\n{}",
        trees.probe.dump()
    );
    assert_eq!(
        trees.probe.count(Step::HeadMoved),
        1,
        "{}",
        trees.probe.dump()
    );
    assert_eq!(
        trees.probe.count(Step::Reloaded) - before,
        1,
        "the push produced a number of reloads that is not one\n{}",
        trees.probe.dump()
    );
    println!("{}", trees.probe.dump());
}

/// THE CONTROL: no head subscription and no tick, and it never updates.
///
/// Without this, the test above would equally describe a binding that reloads
/// whenever anything asks it anything. The two differ by one boolean on the
/// node's answer, and nothing else.
#[test]
fn with_the_subscription_refused_and_no_tick_it_never_updates() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    let mut w = Watch::refusing();
    let mut trees = Trees::new();

    let i = trees.add(head(1), Binding::new(b"a/", b"b/", true), &mut w);
    trees.tick(&mut store).expect("first read");
    assert_eq!(rows(&trees, 0, i), 1);
    assert_eq!(
        trees.binding(0, i).mode(),
        LiveMode::Polled,
        "a refused subscription must be reported as a downgrade, not \
         silently treated as one that worked\n{}",
        trees.probe.dump()
    );
    assert_eq!(trees.subscriptions(), 0, "{}", trees.probe.dump());

    put(&mut store, "a/2", "two");
    w.push(head(1)); // The node would not push: nothing is subscribed.

    let changed = trees.on_pushed(&mut w, &mut store).expect("pushed");
    assert_eq!(
        (changed, rows(&trees, 0, i)),
        (0, 1),
        "a binding with NO subscription and NO tick updated anyway, so the \
         test above proves nothing about the push path\n{}",
        trees.probe.dump()
    );

    // And the tick — the thing that was switched off — still catches it.
    let changed = trees.tick(&mut store).expect("tick");
    assert_eq!(
        (changed, rows(&trees, 0, i)),
        (1, 2),
        "the backstop did not catch up either\n{}",
        trees.probe.dump()
    );
}

/// Five live bindings on one tree take ONE subscription.
///
/// The node's per-delegate cap is 256 and it evicts the least-recently-
/// notified without telling anyone, so spending it five times on one contract
/// is not thrift — it is how a feed across many identities evicts the
/// engine's own head.
#[test]
fn five_live_bindings_on_one_tree_take_one_subscription() {
    let mut store = TreeStore::new();
    for i in 0..10u32 {
        put(&mut store, &format!("a/{i}"), "v");
    }
    let mut w = Watch::on();
    let mut trees = Trees::new();

    for i in 0..5u8 {
        let lo = format!("a/{i}");
        let hi = format!("a/{}", i + 1);
        trees.add(
            head(1),
            Binding::new(lo.as_bytes(), hi.as_bytes(), true),
            &mut w,
        );
    }
    assert_eq!(
        (trees.subscriptions(), w.watched.len(), w.calls),
        (1, 1, 1),
        "five bindings on one tree took {} subscription(s) in {} call(s)\n{}",
        w.watched.len(),
        w.calls,
        trees.probe.dump()
    );
    // And every one of them is on it.
    for i in 0..5 {
        assert_eq!(
            trees.binding(0, i).mode(),
            LiveMode::HeadSubscribed,
            "binding {i} did not inherit the tree's subscription\n{}",
            trees.probe.dump()
        );
    }

    // THE CONTROL: a SECOND tree is a second subscription, so "one" above is
    // about sharing a tree rather than about never subscribing twice.
    trees.add(head(2), Binding::new(b"a/", b"b/", true), &mut w);
    assert_eq!(
        (trees.subscriptions(), trees.trees()),
        (2, 2),
        "a second tree did not get its own subscription\n{}",
        trees.probe.dump()
    );
}

/// A non-live binding takes neither kind of subscription.
#[test]
fn a_non_live_binding_takes_no_subscription_of_either_kind() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    let mut w = Watch::on();
    let mut trees = Trees::new();

    let i = trees.add(head(1), Binding::new(b"a/", b"b/", false), &mut w);
    assert_eq!(
        (trees.subscriptions(), w.calls),
        (0, 0),
        "a non-live binding reached for a subscription\n{}",
        trees.probe.dump()
    );
    assert_eq!(trees.binding(0, i).mode(), LiveMode::Manual);

    // THE CONTROL: a live binding on the SAME tree does take one.
    trees.add(head(1), Binding::new(b"a/", b"b/", true), &mut w);
    assert_eq!(
        (trees.subscriptions(), w.calls),
        (1, 1),
        "a live binding on the same tree did not take one, so the zero \
         above says nothing about `live`\n{}",
        trees.probe.dump()
    );
}

/// A push for a tree nobody watches any more is ORDINARY, not an error.
///
/// The platform has no unsubscribe, so the node goes on waking a delegate for
/// contracts it has forgotten (F39). Anything that treated that as a fault
/// would fault during normal operation.
#[test]
fn a_push_for_a_forgotten_tree_is_ignored_rather_than_an_error() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    let mut w = Watch::on();
    let mut trees = Trees::new();
    trees.add(head(1), Binding::new(b"a/", b"b/", true), &mut w);

    // The node pushes a head this client never bound.
    w.watched.push(head(9));
    w.push(head(9));
    let changed = trees
        .on_pushed(&mut w, &mut store)
        .expect("a push for an unknown tree must not be an error");
    assert_eq!(changed, 0, "{}", trees.probe.dump());
    assert_eq!(
        trees.probe.count(Step::HeadMoved),
        0,
        "an unknown tree was recorded as having woken bindings\n{}",
        trees.probe.dump()
    );
}

/// Re-asserting on reconnect re-subscribes every watched tree.
#[test]
fn reconnecting_re_asserts_every_subscription() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    let mut w = Watch::on();
    let mut trees = Trees::new();
    trees.add(head(1), Binding::new(b"a/", b"b/", true), &mut w);
    trees.add(head(2), Binding::new(b"a/", b"b/", true), &mut w);
    trees.add(head(3), Binding::new(b"a/", b"b/", false), &mut w);
    assert_eq!(w.calls, 2, "{}", trees.probe.dump());

    // The connection dropped: the node's copy is gone and nothing told us.
    w.watched.clear();
    trees.reassert(&mut w);
    assert_eq!(
        (w.calls, w.watched.len()),
        (4, 2),
        "a reconnect did not re-assert both live trees (and must not have \
         re-asserted the non-live one)\n{}",
        trees.probe.dump()
    );

    // A refusal on reconnect is a downgrade, reported.
    w.accept = false;
    w.watched.clear();
    trees.reassert(&mut w);
    assert_eq!(
        trees.binding(0, 0).mode(),
        LiveMode::Polled,
        "a subscription refused on reconnect left the binding claiming it \
         was still subscribed\n{}",
        trees.probe.dump()
    );
    assert!(
        trees.probe.count(Step::Downgraded) >= 1,
        "{}",
        trees.probe.dump()
    );
}

/// The probe records ALWAYS, and turning it off is a parameter.
#[test]
fn the_probe_records_without_being_asked_and_can_be_turned_off() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    let mut w = Watch::on();
    let mut trees = Trees::new();
    trees.add(head(1), Binding::new(b"a/", b"b/", true), &mut w);
    trees.tick(&mut store).expect("tick");
    assert!(
        !trees.probe.events().is_empty(),
        "nothing was recorded, so a failure here would say nothing"
    );

    let mut off = craftworks_sdk::Recorder::new(0);
    off.note(Step::Quiet, 0);
    assert!(
        off.events().is_empty(),
        "recording off still recorded; its cost cannot be measured against \
         anything"
    );
}
