//! THE NODES WALK (the assets dashboard's audit, KEEPER §4): `Walk::Nodes` / `Event::Nodes` pages through EVERY node of
//! a tree, level by level, with each node's groups (members and their PARITY parity ids) -- through the one read path:
//! a cold node parks the read and is fetched like any other. Its fetches are BACKGROUND work when only
//! `ClientId::BACKGROUND` waits on them (KEEPER §7).

use engine::read::{NodeGroups, NodesAt, NodesSpec, ReadResult, ReqId, Walk, Walked};
use engine::{ClientId, Effect, Engine, Event, Params};
use freenet_prolly::node::Node;
use freenet_prolly::parity::{group_members, PARITY};
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

mod common;
use common::{new_store_params, tree, Store};

/// 600 records of 2,000-byte values (stored by reference: the leaves carry value groups) -- a tree of several
/// levels.
fn fixture() -> (Cid, MemBlocks) {
    let records: BTreeMap<Vec<u8>, Vec<u8>> = (0..600u32).map(|i| (format!("k/{i:05}").into_bytes(), vec![(i % 251) as u8; 2_000])).collect();
    tree(&records)
}

/// Every node the root reaches, found independently (a plain descent over the fixture's own store): id -> its groups.
/// A node's groups: (members, parity ids).
type Groups = Vec<(Vec<Cid>, Vec<Cid>)>;

fn all_nodes(blocks: &MemBlocks, root: Cid) -> BTreeMap<Cid, Groups> {
    let mut out = BTreeMap::new();
    let mut todo = vec![root];
    while let Some(id) = todo.pop() {
        let node = Node::parse(blocks.get(&id).expect("the fixture holds every node")).expect("a node");
        let parity: Vec<Cid> = node.parity().collect();
        let groups = group_members(&node).into_iter().enumerate().map(|(g, (_, m))| (m, parity.iter().skip(g * PARITY).take(PARITY).copied().collect())).collect();
        if !node.is_leaf() {
            todo.extend((0..node.len()).map(|i| node.child(i).0));
        }
        out.insert(id, groups);
    }
    out
}

fn started(root: Cid) -> Engine<Store> {
    let mut e = new_store_params(Params::default());
    let _ = e.step(Event::Start { key: engine::KeySource::SecretStore, epochs: vec![engine::Epoch(1)] });
    let _ = e.step(Event::HeadRead { epoch: engine::Epoch(1), seq: 1, root });
    e
}

/// **A WARM tree, paged: every node once, each with its groups.** Pages of 5 nodes from the root down; the union is
/// exactly the tree's node set (checked against an independent descent), each node's groups its own.
#[test]
fn the_nodes_walk_pages_through_every_node_once_with_its_groups() {
    let (root, all) = fixture();
    let want = all_nodes(&all, root);
    let e = started(root);
    for id in want.keys() {
        e.blocks().put(*id, all.get(id).expect("held"));
    }
    let mut got: Vec<NodeGroups> = Vec::new();
    let mut at: Option<NodesAt> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 1_000, "the walk never ended");
        match e.walk(&root, &Walk::Nodes(NodesSpec { at: at.clone(), max_nodes: 5 })) {
            Walked::Done(ReadResult::Nodes { nodes, next }) => {
                got.extend(nodes);
                match next {
                    Some(n) => at = Some(n),
                    None => break,
                }
            }
            other => panic!("a warm walk did not answer: {other:?}"),
        }
    }
    let levels: BTreeSet<u8> = got.iter().map(|n| n.level).collect();
    println!("{} nodes over {} levels in {pages} pages of 5 (the tree: {} nodes)", got.len(), levels.len(), want.len());
    assert!(levels.len() >= 2, "THE SETUP: a one-level tree");
    let ids: Vec<Cid> = got.iter().map(|n| n.id).collect();
    assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>().len(), ids.len(), "a node was walked twice");
    assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>(), want.keys().copied().collect(), "the walk's nodes are not the tree's");
    for n in &got {
        assert_eq!(&n.groups, &want[&n.id], "a node's groups are not its own");
    }
    assert!(got.iter().any(|n| n.groups.iter().any(|(m, p)| m.len() >= 2 && p.len() == PARITY)), "THE SETUP: no grouped node");
}

/// **A COLD tree, through the parked-read path, as BACKGROUND work.** `Event::Nodes` from `ClientId::BACKGROUND` on an
/// empty store parks, fetches each node it needs (every fetch background), and replies with the whole tree once they
/// land. The control: an app's read of the same cold tree is interactive.
#[test]
fn a_cold_nodes_walk_fetches_through_the_read_path_as_background_work() {
    let (root, all) = fixture();
    let want = all_nodes(&all, root);
    let mut e = started(root);
    let mut at: Option<NodesAt> = None;
    let mut got = 0usize;
    let mut fetched = 0usize;
    for req in 1..1_000u64 {
        let mut fx = e.step(Event::Nodes { client: ClientId::BACKGROUND, req_id: ReqId(req), spec: NodesSpec { at: at.clone(), max_nodes: 16 } });
        let mut done = None;
        for _ in 0..10_000 {
            let mut next = Vec::new();
            for f in &fx {
                match f {
                    Effect::FetchBlock { id, .. } => {
                        assert!(e.fetch_is_background(id), "a fetch only the background walk waits on is not background");
                        fetched += 1;
                        let bytes = all.get(id).expect("the fixture holds it").to_vec();
                        e.blocks().put(*id, &bytes);
                        next.push(Event::BlockArrived { id: *id, bytes });
                    }
                    Effect::Reply { client, result: ReadResult::Nodes { nodes, next }, .. } if *client == ClientId::BACKGROUND => done = Some((nodes.len(), next.clone())),
                    Effect::Progress { .. } => {}
                    other => panic!("a nodes walk emitted {other:?}"),
                }
            }
            if done.is_some() || next.is_empty() {
                break;
            }
            fx = next.into_iter().flat_map(|ev| e.step(ev)).collect();
        }
        let (n, next) = done.expect("the walk never replied");
        got += n;
        match next {
            Some(a) => at = Some(a),
            None => break,
        }
    }
    println!("cold walk: {got} nodes, {fetched} background fetches (the tree: {} nodes)", want.len());
    assert_eq!(got, want.len(), "the cold walk did not reach every node");
    assert!(fetched > 0, "THE SETUP: nothing was cold");
    // THE CONTROL: an app's read of a cold block is interactive.
    let (root2, all2) = fixture();
    let _ = all2;
    let mut app = started(root2);
    let fx = app.step(Event::Get { client: ClientId(7), req_id: ReqId(1), key: b"k/00300".to_vec() });
    let first = fx.iter().find_map(|f| if let Effect::FetchBlock { id, .. } = f { Some(*id) } else { None }).expect("a cold read fetches");
    assert!(!app.fetch_is_background(&first), "an app read's fetch was taken as background work");
}
