//! Does a tree this SDK writes satisfy the rule a HOST enforces?
//!
//! The Block contract runs `freenet_prolly::boundary::check_node` on every
//! `TREE_NODE` it is offered (freenet-contracts, `well_formed`). So "the tests
//! pass" says nothing about whether a node this SDK emits would be KEPT — the
//! suite never asked the question the network asks. It bit us once already:
//! the SDK pinned the tree library from before the parity rule and would have
//! shipped trees every host refuses, and nothing in this repo noticed. The
//! panel's `prollyRev` did.
//!
//! This file asks it directly, walking every block a store holds — including
//! the superseded nodes, which stay and which a reader can still be handed.

use craftworks_sdk::store::{Edit, Store};
use craftworks_sdk::{Db, TreeStore};
use freenet_prolly::boundary::check_node;
use freenet_prolly::node::{Node, MAX_INLINE};
use serde_json::{json, Map, Value};

/// Every node in the store, checked the way a host checks it.
fn every_node_passes(store: &TreeStore) -> (usize, usize) {
    let (mut nodes, mut others) = (0, 0);
    for (cid, bytes) in store.blocks() {
        match Node::parse(bytes) {
            Ok(node) => {
                nodes += 1;
                check_node(&node).unwrap_or_else(|e| {
                    panic!(
                        "a node this SDK wrote would be REFUSED by a host: {e:?} (block {})",
                        craftworks_sdk::hex(cid)
                    )
                });
            }
            // Value blocks: bytes too large to inline. Not nodes, not this
            // rule's business — but counted, so "every node passed" cannot be
            // true because there were no nodes.
            Err(_) => others += 1,
        }
    }
    (nodes, others)
}

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// Enough entries to force several levels, so BRANCHES are checked and not
/// only a single leaf — the split rule is about interiors. Chosen as the
/// smallest that clears the node floor below with room to spare: the suite ran
/// for minutes in a debug build, and a fixture that is bigger than the property
/// it demonstrates is paid for on every run by everyone.
const ENTRIES: u64 = 2000;

#[test]
fn every_node_a_tall_tree_holds_would_be_accepted_by_a_host() {
    let mut store = TreeStore::new();
    let mut r = rng(7);
    // Written as ONE batch, not entry by entry. Entry by entry the store also
    // accumulates a superseded node per write, which multiplies the work by the
    // number of entries without adding a single new SHAPE — and it took this
    // suite to minutes in a debug build. The history case has its own test
    // below, where the superseded nodes are the point.
    let batch: Vec<(Vec<u8>, Edit)> = (0..ENTRIES)
        .map(|i| {
            let v = vec![(r() % 251) as u8; 24 + (r() % 400) as usize];
            (format!("k/{i:08}").into_bytes(), Edit::Put(v))
        })
        .collect();
    store.apply_batch(&batch);

    // The property the fixture exists for is DEPTH: the split rule is about
    // interiors, so a run that only ever saw leaves would pass while saying
    // nothing. Asserted on the height rather than on the entry count, because
    // the entry count is a proxy that the tree library is free to change.
    let h = store.stats().height;
    assert!(
        h >= 3,
        "the fixture must have branches above branches, got height {h}"
    );
    let (nodes, others) = every_node_passes(&store);
    assert!(
        nodes > 20,
        "a tall tree must hold many nodes, found {nodes}"
    );
    println!("height {h}: checked {nodes} nodes ({others} value blocks) — all accepted");
}

#[test]
fn nodes_holding_referenced_values_are_accepted_too() {
    // Values over MAX_INLINE are stored as references. Closing a leaf that
    // holds one needs the value's BYTES, so this is the case where a store that
    // did not keep them would either fail or, worse, build a node without the
    // parity the host demands.
    let mut store = TreeStore::new();
    let mut r = rng(11);
    for i in 0..60u64 {
        let big = vec![(r() % 251) as u8; MAX_INLINE + 1 + (r() % 2048) as usize];
        store.put(format!("big/{i:06}").as_bytes(), &big);
    }
    let (nodes, others) = every_node_passes(&store);
    assert!(nodes >= 1);
    assert!(
        others > 0,
        "this fixture is pointless if nothing was referenced"
    );
    // And the values really read back, which is the other half: a node that
    // passes the host's check while its value block is gone is a tree that
    // validates and cannot be read.
    let mut r = rng(11);
    for i in 0..60u64 {
        let want = vec![(r() % 251) as u8; MAX_INLINE + 1 + (r() % 2048) as usize];
        assert_eq!(store.get(format!("big/{i:06}").as_bytes()), Some(want));
    }
    println!("checked {nodes} nodes over {others} referenced value blocks");
}

#[test]
fn deletes_and_rewrites_leave_no_refusable_node_behind() {
    // Superseded nodes stay in the store, and a reader can be handed one. If an
    // edit path could emit a node that fails the rule, checking only the
    // current root would never see it.
    let mut store = TreeStore::new();
    let mut r = rng(13);
    for i in 0..500u64 {
        store.put(format!("k/{i:08}").as_bytes(), &[(r() % 251) as u8; 40]);
    }
    let mut batch: Vec<(Vec<u8>, Edit)> = Vec::new();
    for i in (0..500u64).step_by(3) {
        batch.push((format!("k/{i:08}").into_bytes(), Edit::Delete));
    }
    for i in (1..500u64).step_by(7) {
        batch.push((format!("k/{i:08}").into_bytes(), Edit::Put(vec![9u8; 900])));
    }
    store.apply_batch(&batch);
    let (nodes, _) = every_node_passes(&store);
    println!("checked {nodes} nodes across the whole history of edits");
}

#[test]
fn a_database_built_through_the_public_api_writes_acceptable_nodes() {
    // The route an app actually takes: a schema, records, the Db surface. The
    // store tests above reach the tree directly, which is not what ships.
    struct FixedEnv(u64, u32);
    impl craftworks_sdk::Env for FixedEnv {
        fn now_ms(&mut self) -> u64 {
            self.0 += 1;
            self.0
        }
        fn rand32(&mut self) -> u32 {
            self.1 = self.1.wrapping_mul(1664525).wrapping_add(1013904223);
            self.1
        }
    }
    let mut db = Db::new(
        TreeStore::new(),
        FixedEnv(1_700_000_000_000, 7),
        [1, 2, 3, 4],
    );
    db.define(
        "tasks",
        &serde_json::from_value(json!({
            "type": "Task",
            "fields": [
                { "name": "title", "kind": "text", "required": true },
                { "name": "body", "kind": "text" }
            ]
        }))
        .unwrap(),
    )
    .expect("the schema is valid");
    for i in 0..400usize {
        let fields: Map<String, Value> = serde_json::from_value(json!({
            "title": format!("t{i}"),
            "body": "x".repeat(200 + i % 900),
        }))
        .unwrap();
        db.put("tasks", &fields).expect("a valid record");
    }
    let (nodes, others) = every_node_passes(db.store());
    assert!(nodes > 5, "found only {nodes} nodes");
    println!("Db route: checked {nodes} nodes ({others} value blocks)");
}

/// The redundancy this store OWES is exactly the redundancy its nodes promise.
///
/// A node lists parity ids the moment it is written; the bytes behind them are
/// separate blocks a commit puts after the head. Between those two moments the
/// group has no redundancy, and the store's job is to know precisely which
/// blocks those are — no more and no fewer.
///
/// Both directions fail differently, so both are asserted. A block held that no
/// node lists is a PUT paid for redundancy over a node that is not in the tree.
/// A group listed whose parity the store does not hold is a node promising
/// redundancy that exists nowhere and that nothing downstream can detect: the
/// ids are there and look exactly like ids whose blocks were put.
#[test]
fn the_owed_parity_is_exactly_what_the_nodes_list() {
    let mut store = TreeStore::new();
    assert_eq!(
        store.owed_parity(),
        0,
        "an empty store owes nothing: the empty leaf has no members, so no group"
    );

    // Values over MAX_INLINE are stored by REFERENCE, so the leaves carry
    // parity members and the leaf grouping is exercised, not only the branches.
    let big = vec![b'v'; MAX_INLINE + 1];
    let mut writes = 0;
    for round in 0..3u32 {
        let edits: Vec<(Vec<u8>, Edit)> = (0..80u32)
            .map(|i| {
                (
                    format!("k/{round}/{i:04}").into_bytes(),
                    Edit::Put(big.clone()),
                )
            })
            .collect();
        writes += edits.len();
        store.apply_batch(&edits);
    }
    assert!(writes > 0, "the fixture must write something");
    assert!(
        store.owed_parity() > 0,
        "three batches of referenced values coded no parity at all — either \
         nothing was owed or nothing was reported, and this test would pass \
         for the wrong reason"
    );

    owed_is_exactly_what_is_listed(&store, "TreeStore::apply_batch");
    println!("  after {writes} writes");
}

/// The invariant itself, so the two public doors are held to the SAME test.
///
/// A number that is right on one door and wrong on the other is worse than one
/// that is wrong on both, because the disagreement is what nobody looks for.
fn owed_is_exactly_what_is_listed(store: &TreeStore, door: &str) {
    use std::collections::HashSet;

    // Every parity id any node in the store lists. Superseded nodes included:
    // they stay, and a reader can still be handed one.
    let mut listed: HashSet<[u8; 32]> = HashSet::new();
    let mut nodes = 0;
    for (_, bytes) in store.blocks() {
        if let Ok(n) = Node::parse(bytes) {
            nodes += 1;
            listed.extend(n.parity());
        }
    }
    assert!(nodes > 0, "{door}: no nodes to check");
    assert!(
        !listed.is_empty(),
        "{door}: no node lists any parity, so there is no promise to check"
    );

    // A held block must BE its id: the id is what a PUT files it under.
    let mut held: HashSet<[u8; 32]> = HashSet::new();
    for (id, bytes) in store.owed() {
        assert_eq!(
            *id,
            freenet_prolly::block_id(freenet_prolly::kind::PARITY, bytes),
            "{door}: an owed parity block does not hash to the id it is filed under"
        );
        held.insert(*id);
    }

    // No more.
    for id in &held {
        assert!(
            listed.contains(id),
            "{door}: the store owes a parity block no node lists — a PUT for \
             redundancy over a node that is not in the tree"
        );
    }
    // No fewer. Nothing here ever publishes, so every id any node lists must
    // still be owed; the day this store gains a publish step, that step is what
    // this assertion will have to account for.
    let gaps: Vec<&[u8; 32]> = listed.iter().filter(|id| !held.contains(*id)).collect();
    assert!(
        gaps.is_empty(),
        "{door}: {} group(s) list parity this store never received — that \
         redundancy exists nowhere and nobody is tracking the debt",
        gaps.len()
    );

    // The same debt, expressed as the unit an engine works in. Three parity
    // blocks are ONE group's redundancy; a count of blocks alone cannot say
    // what is still unprotected when a PUT fails partway.
    let groups = store.owed_groups();
    assert_eq!(
        groups.len() * 3,
        store.owed_parity(),
        "{door}: {} group(s) account for {} of {} owed blocks — some debt \
         belongs to no group, or a group was counted twice",
        groups.len(),
        groups.len() * 3,
        store.owed_parity()
    );
    for g in &groups {
        assert!(
            !g.members.is_empty(),
            "{door}: a group protects nothing, so its parity protects nothing"
        );
        for id in &g.parity {
            assert!(
                held.contains(id),
                "{door}: a group names a parity block the store does not hold"
            );
        }
        // The members must be blocks this store actually has: parity over a
        // member nobody holds is parity over nothing.
        for m in &g.members {
            assert!(
                store.blocks().any(|(c, _)| c == m),
                "{door}: a group covers a member this store does not hold"
            );
        }
    }

    println!(
        "  {door}: {} owed block(s) in {} group(s), {} listed id(s), {nodes} node(s)",
        store.owed_parity(),
        groups.len(),
        listed.len()
    );
}

/// The control for the test above, and it RUNS.
///
/// "The store owes exactly what its nodes list" is only worth asserting if the
/// store could fail to. With `track_owed_parity` off it writes the same nodes,
/// listing the same parity ids, and keeps none of the bytes — the tree is
/// byte-identical and its redundancy exists nowhere. That is not a hypothetical
/// state: it is the state this SDK was in until the library began reporting the
/// blocks at all, and nothing in the suite could see it.
///
/// So this asserts the gap is real AND that the tree is unchanged, because a
/// control that also changed the tree would be caught by something else and
/// would prove nothing about this assertion.
#[test]
fn a_store_that_drops_its_parity_writes_the_same_tree_and_owes_nothing() {
    use craftworks_sdk::tree_store::Options;

    let big = vec![b'v'; MAX_INLINE + 1];
    let edits: Vec<(Vec<u8>, Edit)> = (0..80u32)
        .map(|i| (format!("k/{i:04}").into_bytes(), Edit::Put(big.clone())))
        .collect();

    let mut tracked = TreeStore::new();
    tracked.apply_batch(&edits);

    let mut dropped = TreeStore::with_options(Options {
        track_owed_parity: false,
        ..Options::default()
    });
    dropped.apply_batch(&edits);

    // The same tree. The parity IDS are in the nodes either way; only the bytes
    // behind them differ, and a root hash cannot tell you that.
    assert_eq!(
        tracked.root(),
        dropped.root(),
        "the control changed the tree, so it is not a control for this"
    );

    assert!(
        tracked.owed_parity() > 0,
        "nothing was owed even with tracking on: the fixture codes no parity"
    );
    assert_eq!(
        dropped.owed_parity(),
        0,
        "the control kept parity anyway — `track_owed_parity` does not reach \
         the code the assertion is about"
    );

    // And the tree the control wrote still LISTS the parity it does not hold:
    // that is the whole failure, invisible to the root and to every reader.
    let mut listed = 0;
    for (_, bytes) in dropped.blocks() {
        if let Ok(n) = Node::parse(bytes) {
            listed += n.parity().count();
        }
    }
    assert!(
        listed > 0,
        "the control's nodes list no parity, so there was no promise to break"
    );
    println!("  control: {listed} parity id(s) listed, 0 blocks held, same root");
}

/// Taking the debt empties it, and every public door reports the same debt.
///
/// `TreeStore::apply_batch` is one way in; `Db::write` is the one an app
/// actually uses, and it reaches the tree through the same `Store`. A number
/// that is right on one door and wrong on the other is worse than one that is
/// wrong on both, because the disagreement is what nobody looks for.
#[test]
fn taking_the_owed_parity_empties_it_and_every_door_agrees() {
    use craftworks_sdk::store::Store as _;

    let big = vec![b'v'; MAX_INLINE + 1];
    let edits: Vec<(Vec<u8>, Edit)> = (0..80u32)
        .map(|i| (format!("k/{i:04}").into_bytes(), Edit::Put(big.clone())))
        .collect();

    let mut direct = TreeStore::new();
    direct.apply_batch(&edits);
    let owed_direct = direct.owed_parity();
    assert!(owed_direct > 0, "the fixture codes no parity");

    // Taking it hands over exactly what was owed and leaves nothing behind.
    let taken = direct.take_owed();
    assert_eq!(
        taken.len(),
        owed_direct,
        "take_owed lost or invented blocks"
    );
    assert_eq!(
        direct.owed_parity(),
        0,
        "the store still owes what it just handed over"
    );
    // Taking twice owes nothing, rather than repeating the debt.
    let again = direct.take_owed();
    assert!(again.is_empty(), "the debt came back");
    again.forget();

    // Every block handed over is its id, checked on the way out as well as in.
    for (id, bytes) in taken.iter() {
        assert_eq!(
            *id,
            freenet_prolly::block_id(freenet_prolly::kind::PARITY, bytes),
            "a taken block does not hash to the id it is filed under"
        );
    }
    let n = taken.into_blocks().len();
    assert_eq!(n, owed_direct);

    // The other door, held to the SAME invariant. An app never calls
    // `apply_batch`; it calls `Db::put`, which reaches the tree through the
    // same `Store` and must owe its redundancy just as precisely.
    let mut db = Db::new(TreeStore::new(), craftworks_sdk::SystemEnv, *b"dev1");
    db.define(
        "notes",
        &serde_json::from_value(json!({
            "type": "Note",
            "fields": [{ "name": "body", "kind": "text", "required": true }]
        }))
        .unwrap(),
    )
    .expect("the schema is valid");
    for i in 0..120usize {
        let fields: Map<String, Value> = serde_json::from_value(json!({
            "body": "x".repeat(MAX_INLINE + 1 + i),
        }))
        .unwrap();
        db.put("notes", &fields).expect("a valid record");
    }
    assert!(
        db.store().owed_parity() > 0,
        "the Db door coded no parity, so it was not tested"
    );
    owed_is_exactly_what_is_listed(db.store(), "Db::put");
}
