//! A binding is an EXTERNAL STORE, and the contract is its snapshot identity.
//!
//! `useSyncExternalStore` re-renders whenever `getSnapshot()` is not the same
//! object as last time. So "the rows did not change" has to mean "the same
//! `Rc`", and a binding that rebuilt its rows on each call would re-render on
//! every render for ever, whatever the data did. That is the property under
//! test here, and it is one that a test asserting only VALUES would not see.

use craftworks_sdk::store::{Delta, Read, Reads};
use craftworks_sdk::{Binding, LiveMode, MemStore, Store, TreeStore};

fn put(s: &mut impl Store, k: &str, v: &str) {
    s.put(k.as_bytes(), v.as_bytes()).expect("the store took the write");
}

fn keys(b: &Binding) -> Vec<String> {
    b.snapshot()
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect()
}

/// The snapshot is the SAME OBJECT until the rows change.
#[test]
fn the_snapshot_is_referentially_stable_until_the_rows_change() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");
    put(&mut store, "a/2", "two");

    let mut b = Binding::new(b"a/", b"b/", false);
    b.reload(&mut store).expect("reload");
    let first = b.snapshot();
    assert_eq!(keys(&b), ["a/1", "a/2"]);

    // Reading it again, and again: the same object every time.
    assert!(
        std::rc::Rc::ptr_eq(&first, &b.snapshot()),
        "two reads of an unchanged binding gave two different objects; \
         useSyncExternalStore would re-render on every render"
    );

    // A reload that finds nothing moved must not replace it either. This is
    // the case the backstop produces on every tick, so it is the one that
    // decides whether a live binding is cheap or a re-render loop.
    b.reload(&mut store).expect("reload");
    assert!(
        std::rc::Rc::ptr_eq(&first, &b.snapshot()),
        "a reload that changed nothing replaced the snapshot"
    );
    assert_eq!(b.reloads.unchanged, 1, "the no-op reload did work");

    // A write OUTSIDE the range moves the root — so the binding does look —
    // but the rows are the same and the object must not be replaced.
    put(&mut store, "z/9", "elsewhere");
    b.reload(&mut store).expect("reload");
    assert!(
        std::rc::Rc::ptr_eq(&first, &b.snapshot()),
        "a change outside the bound range replaced the snapshot, which \
         re-renders every component watching an untouched range"
    );

    // THE CONTROL. A write INSIDE the range must replace it — otherwise
    // "stable" above is just a binding that never updates.
    put(&mut store, "a/3", "three");
    b.reload(&mut store).expect("reload");
    assert!(
        !std::rc::Rc::ptr_eq(&first, &b.snapshot()),
        "a change inside the range did NOT replace the snapshot, so the \
         stability asserted above is a binding that simply does not work"
    );
    assert_eq!(keys(&b), ["a/1", "a/2", "a/3"]);
}

/// A listener fires exactly once per change, and not at all for an untouched
/// range.
#[test]
fn a_listener_fires_once_per_change_and_never_for_an_untouched_range() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");

    let mut b = Binding::new(b"a/", b"b/", false);
    b.reload(&mut store).expect("reload");

    let fired = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let f = fired.clone();
    let id = b.subscribe(Box::new(move || f.set(f.get() + 1)));

    // Three commits that miss the range.
    for i in 0..3 {
        put(&mut store, &format!("z/{i}"), "elsewhere");
        b.reload(&mut store).expect("reload");
    }
    assert_eq!(
        fired.get(),
        0,
        "the listener fired for commits that touched nothing it watches"
    );

    // One that hits it.
    put(&mut store, "a/2", "two");
    b.reload(&mut store).expect("reload");
    assert_eq!(fired.get(), 1, "expected exactly one notification");

    // Unsubscribing really stops it.
    b.unsubscribe(id);
    put(&mut store, "a/3", "three");
    b.reload(&mut store).expect("reload");
    assert_eq!(fired.get(), 1, "a cancelled listener still fired");
}

/// Flipping `live` changes nothing a component can see.
///
/// The surface is identical by construction — same `snapshot`, same
/// `subscribe`, same `reload` — so the test drives BOTH through the same
/// sequence and compares everything a component could observe.
#[test]
fn a_live_binding_and_a_plain_one_look_identical_to_a_component() {
    let mut store = TreeStore::new();
    put(&mut store, "a/1", "one");

    let mut plain = Binding::new(b"a/", b"b/", false);
    let mut live = Binding::new(b"a/", b"b/", true);

    let (p, l) = (
        std::rc::Rc::new(std::cell::Cell::new(0u32)),
        std::rc::Rc::new(std::cell::Cell::new(0u32)),
    );
    let (pc, lc) = (p.clone(), l.clone());
    plain.subscribe(Box::new(move || pc.set(pc.get() + 1)));
    live.subscribe(Box::new(move || lc.set(lc.get() + 1)));

    for step in 0..4 {
        put(&mut store, &format!("a/{step}"), "v");
        put(&mut store, &format!("z/{step}"), "v");
        plain.reload(&mut store).expect("reload");
        live.reload(&mut store).expect("reload");
        assert_eq!(keys(&plain), keys(&live), "step {step}: rows diverged");
    }
    assert_eq!(p.get(), l.get(), "the two fired different numbers of times");

    // The ONE difference, and it is not one a component consumes: a live
    // binding says how it is being kept current.
    assert_eq!(plain.mode(), LiveMode::Manual);
    assert_eq!(
        live.mode(),
        LiveMode::Polled,
        "a live binding that has not been granted a subscription must NOT \
         claim it is being notified"
    );
    assert_eq!(
        live.sub_id(),
        None,
        "a live binding took out an engine subscription nobody granted it"
    );
    assert_eq!(
        plain.sub_id(),
        None,
        "a NON-live binding took out an engine subscription"
    );
}

/// A refused subscription is a DOWNGRADE, reported, never a silent poll.
#[test]
fn a_refused_subscription_is_reported_as_a_downgrade() {
    let mut live = Binding::new(b"a/", b"b/", true);
    assert_eq!(
        live.mode(),
        LiveMode::Polled,
        "it claimed to be notified \
        before anything had accepted a subscription"
    );

    live.note_subscribed(7, true);
    assert_eq!(live.mode(), LiveMode::Notified);
    assert_eq!(live.sub_id(), Some(7));

    live.note_downgraded();
    assert_eq!(
        live.mode(),
        LiveMode::Polled,
        "an engine that stopped notifying left the binding claiming it was \
         still being told"
    );
    assert_eq!(live.sub_id(), None);

    // A refusal at subscribe time is the same outcome by a different door.
    let mut other = Binding::new(b"a/", b"b/", true);
    other.note_subscribed(9, false);
    assert_eq!(other.mode(), LiveMode::Polled);
    assert_eq!(other.sub_id(), None);
}

/// A reload takes the DELTA where the backend can give one, and the full read
/// where it cannot — and both are correct.
///
/// `TreeStore` can diff its own roots; `MemStore` cannot and says so with the
/// defined answer. Running the same sequence on both is what proves the
/// binding's two paths agree, rather than the delta path merely existing.
#[test]
fn a_reload_uses_the_delta_where_it_can_and_the_full_read_where_it_cannot() {
    let mut tree = TreeStore::new();
    let mut mem = MemStore::default();
    let mut a = Binding::new(b"a/", b"b/", false);
    let mut b = Binding::new(b"a/", b"b/", false);
    // 40 rows is well under the threshold, so ask for the delta explicitly:
    // this test is about the two PATHS agreeing, not about which one a
    // 40-row binding picks. That choice has its own test.
    a.delta_above(0);
    b.delta_above(0);

    for i in 0..40u32 {
        put(&mut tree, &format!("a/{i:03}"), "seed");
        put(&mut mem, &format!("a/{i:03}"), "seed");
    }
    a.reload(&mut tree).expect("reload");
    b.reload(&mut mem).expect("reload");
    assert_eq!(keys(&a), keys(&b));

    // A change, a removal, and an addition — every arm of a delta.
    put(&mut tree, "a/007", "moved");
    put(&mut mem, "a/007", "moved");
    tree.delete(b"a/008").expect("the store took the write");
    mem.delete(b"a/008").expect("the store took the write");
    put(&mut tree, "a/999", "added");
    put(&mut mem, "a/999", "added");

    a.reload(&mut tree).expect("reload");
    b.reload(&mut mem).expect("reload");

    assert_eq!(
        keys(&a),
        keys(&b),
        "the delta path and the full-read path disagree about the rows"
    );
    assert_eq!(
        a.snapshot(),
        b.snapshot(),
        "the delta path applied different VALUES from the full read"
    );
    assert!(
        a.snapshot()
            .iter()
            .any(|(k, v)| k == b"a/007" && v == b"moved"),
        "the delta did not apply the change"
    );
    assert!(
        !a.snapshot().iter().any(|(k, _)| k == b"a/008"),
        "the delta did not apply the REMOVAL — a reader told an empty value \
         instead of a removal keeps the key"
    );
    assert!(
        a.snapshot().iter().any(|(k, _)| k == b"a/999"),
        "the delta did not apply the addition"
    );

    // And each backend took the path it should have.
    assert_eq!(
        (a.reloads.by_delta, b.reloads.by_delta),
        (1, 0),
        "tree={:?} mem={:?}: the tree backend should have served this by \
         delta and the in-memory one should have said FullReloadRequired",
        a.reloads,
        b.reloads
    );
}

/// A delta bigger than one page falls back to the full read rather than
/// applying half of it.
///
/// Applying a partial delta and recording the new root would leave the
/// binding claiming to stand at a root it does not stand at, and nothing
/// afterwards would ever notice — which is strictly worse than the full read
/// it was avoiding.
#[test]
fn a_delta_that_does_not_fit_in_one_page_is_not_applied_in_part() {
    /// A store that always answers with a PAGED delta: one change and a
    /// cursor saying there is more.
    struct Paged(TreeStore);
    impl Reads for Paged {
        fn get(&mut self, k: &[u8]) -> Read<Option<Vec<u8>>> {
            self.0.get(k)
        }
        fn scan(
            &mut self,
            lo: &[u8],
            hi: &[u8],
            reverse: bool,
            limit: usize,
        ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
            self.0.scan(lo, hi, reverse, limit)
        }
        fn root(&mut self) -> Read<[u8; 32]> {
            Reads::root(&mut self.0)
        }
        fn changes_since(
            &mut self,
            _from: [u8; 32],
            _lo: &[u8],
            _hi: &[u8],
            _max: u32,
        ) -> Read<Delta> {
            Ok(Delta::Changes {
                // A LIE, deliberately: it names one key that did not change,
                // and says there is more. If the binding applied it, the rows
                // would carry this value and the test would see it.
                changes: vec![(b"a/000".to_vec(), Some(b"POISON".to_vec()))],
                cursor: Some(b"a/000".to_vec()),
                new_root: Reads::root(&mut self.0).expect("a root"),
            })
        }
    }

    let mut s = Paged(TreeStore::new());
    for i in 0..8u32 {
        s.0.put(format!("a/{i:03}").as_bytes(), b"seed").expect("the store took the write");
    }
    let mut b = Binding::new(b"a/", b"b/", false);
    b.delta_above(0);
    b.reload(&mut s).expect("reload");

    s.0.put(b"a/000", b"real").expect("the store took the write");
    b.reload(&mut s).expect("reload");

    assert!(
        b.snapshot()
            .iter()
            .any(|(k, v)| k == b"a/000" && v == b"real"),
        "the binding applied a PARTIAL delta: it holds {:?}",
        b.snapshot()
            .iter()
            .find(|(k, _)| k == b"a/000")
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
    );
    assert_eq!(
        b.reloads.by_delta, 0,
        "a paged delta was counted as served by delta"
    );
    assert!(b.reloads.full >= 1, "it did not fall back to the full read");
}

/// A narrow range re-READS; a wide one diffs. Both sides of the threshold.
///
/// This is the one place a cost measurement turns into a branch, so it is
/// tested at both ends rather than at the one that happens to be true today.
/// The numbers behind it are in `DELTA_WORTH_IT_ABOVE`, and they are the
/// reason the branch leans the way it does: on a range a few blocks wide a
/// diff walks two trees to save reading one.
#[test]
fn a_narrow_range_re_reads_and_a_wide_one_takes_the_delta() {
    let mut store = TreeStore::new();
    for i in 0..200u32 {
        put(&mut store, &format!("a/{i:04}"), "seed");
    }

    // Below the threshold: re-read.
    let mut narrow = Binding::new(b"a/", b"b/", false);
    narrow.delta_above(1000);
    narrow.reload(&mut store).expect("first");
    put(&mut store, "a/0007", "moved");
    narrow.reload(&mut store).expect("second");
    assert_eq!(
        (narrow.reloads.by_delta, narrow.reloads.full),
        (0, 2),
        "a range under the threshold took a delta: {:?}",
        narrow.reloads
    );

    // Above it: diff. Same store, same change, same rows.
    let mut wide = Binding::new(b"a/", b"b/", false);
    wide.delta_above(10);
    wide.reload(&mut store).expect("first");
    put(&mut store, "a/0008", "moved");
    wide.reload(&mut store).expect("second");
    assert_eq!(
        (wide.reloads.by_delta, wide.reloads.full),
        (1, 1),
        "a range over the threshold did not take a delta: {:?}",
        wide.reloads
    );

    // And both arrive at the same rows, which is the part that matters.
    narrow.reload(&mut store).expect("catch up");
    assert_eq!(
        keys(&narrow),
        keys(&wide),
        "the two paths disagree about the rows"
    );
}
