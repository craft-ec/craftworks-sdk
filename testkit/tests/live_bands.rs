//! LIVE, per watch key (READ-STATE inv. 5; sdk#137's bands): a change is
//! reported for the binding whose RANGE it falls in, and for no other — a
//! write under parent Q does not re-run a binding of parent P's band, and a
//! head move that changed neither reports neither. The diff is the tree's
//! (`LiveBindings::take_changed` over `PageStore::changes_since`), walked in
//! a real tab over the page path's node.

use craftworks_sdk::store::{Edit, Store};
use craftworks_sdk::live_bindings::WatchKey;
use craftworks_sdk::{Db, LiveBindings, MemStore, SystemEnv};

type D = Db<MemStore, SystemEnv>;

const P: [u8; 16] = [0x0a; 16];
const Q: [u8; 16] = [0x0b; 16];

/// A record key inside `parent`'s band of `item`.
fn under(parent: &[u8; 16], n: u8) -> Vec<u8> {
    let (lo, _) = D::parent_range("item", parent);
    let mut k = lo;
    k.extend_from_slice(&[n; 16]);
    k
}

#[test]
fn a_band_is_told_of_its_own_band_and_not_of_a_siblings() {
    let node = testkit::PageNode::new();
    let (mut store, _conn, _clock) = testkit::page_store(&node);
    let key = |p: &[u8; 16]| WatchKey::of(craftworks_sdk::app::StoredName::of_tree(D::watch_key("item", Some(p))));
    let (kp, kq) = (key(&P), key(&Q));
    let mut live = LiveBindings::default();
    let head = store.head();
    live.bind(kp.clone(), head);
    live.bind(kq.clone(), head);
    let mut changed = |store: &mut craftworks_sdk::PageStore<testkit::PageConn>| {
        let head = store.head();
        live.take_changed(store, head, D::watch_range)
    };
    // First head: both bands are at the head they were bound at.
    assert_eq!(changed(&mut store), Vec::<WatchKey>::new(), "nothing changed and yet a band was told");

    store.apply_batch(&[(under(&Q, 1), Edit::Put(b"q1".to_vec()))]).expect("taken");
    assert_eq!(changed(&mut store), vec![kq.clone()], "a write under Q was not told to Q's band alone");

    store.apply_batch(&[(under(&P, 1), Edit::Put(b"p1".to_vec()))]).expect("taken");
    assert_eq!(changed(&mut store), vec![kp.clone()], "a write under P was not told to P's band alone");

    // THE CONTROL: a write outside both bands moves the root and tells neither.
    let (_, hi) = D::domain_range("item");
    store.apply_batch(&[(hi, Edit::Put(b"outside".to_vec()))]).expect("taken");
    assert!(store.head().is_some());
    assert_eq!(changed(&mut store), Vec::<WatchKey>::new(), "a write in neither band re-ran a band");
}
