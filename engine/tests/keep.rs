//! THE KEEPER'S PASS (sdk#306, KEEPER.md §3/§5): a real tree with its
//! parity, a "node" that has lost blocks, and one pass. The node here answers
//! every GET (present or absent) and acks every PUT; what it holds at the end
//! is the evidence.
//!
//! * ≤ 3 of a group lost (members, parity, or both): after the pass the node
//!   holds every block again, byte for byte;
//! * 4 lost: the group is named damaged and nothing is put for it;
//! * a forged arrival is refused and repaired like a loss;
//! * a closed page's residual (parity never put) is completed;
//! * a lost tree node is rebuilt AND descended: the pass still reaches
//!   everything under it;
//! * a second pass over a whole tree puts nothing (idempotent).

use engine::keep::{KeepEffect, Pass, Report};
use freenet_prolly::build::TreeBuilder;
use freenet_prolly::node::{Node, Value, MAX_INLINE};
use freenet_prolly::store::MemBlocks;
use freenet_prolly::{block_id, kind, parity, Cid};
use std::collections::{BTreeMap, BTreeSet};

/// A node's store: id → (kind, body).
type Net = BTreeMap<Cid, (u8, Vec<u8>)>;

/// A tree of `rows` rows, values large enough to be stored by reference
/// (so leaves have groups too), with EVERY node's parity computed by
/// `blocks_of` -- the function the writer puts parity with -- and held.
fn tree(rows: u32) -> (Cid, Net) {
    let mut sink = MemBlocks::default();
    let mut b = TreeBuilder::new(|c, bytes: &[u8]| sink.insert(c, bytes));
    for i in 0..rows {
        b.push_bytes(format!("k/{i:06}").as_bytes(), &vec![(i % 251) as u8; MAX_INLINE + 1 + (i as usize % 7)]).expect("push");
    }
    let root = b.finish().expect("finish");
    let mut net = Net::new();
    let mut parity_blocks = Vec::new();
    for (cid, bytes) in sink.0.iter() {
        match Node::parse(bytes) {
            Ok(n) => {
                net.insert(*cid, (kind::TREE_NODE, bytes.clone()));
                let ids: Vec<Cid> = n.parity().collect();
                let blocks = parity::blocks_of(&n, &sink).expect("every member held");
                assert_eq!(blocks.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids, "parity computed is not what the node lists");
                parity_blocks.extend(blocks);
            }
            Err(_) => {
                net.insert(*cid, (kind::RAW, bytes.clone()));
            }
        }
    }
    for (id, p) in parity_blocks {
        net.insert(id, (kind::PARITY, p));
    }
    (root, net)
}

/// Run one pass against `net` (the node), which answers what it holds. A
/// block in `forged` is answered with wrong bytes. Returns the report and
/// every PUT the pass made.
fn pass(root: Cid, net: &mut Net, forged: &BTreeSet<Cid>) -> (Report, Vec<Cid>) {
    let mut p = Pass::new(root);
    let mut puts = Vec::new();
    let mut steps = 0;
    loop {
        let fx = p.take();
        if fx.is_empty() {
            break;
        }
        for f in fx {
            steps += 1;
            assert!(steps < 1_000_000, "a pass did not settle");
            match f {
                KeepEffect::Fetch(id) => match net.get(&id) {
                    Some((_, body)) if forged.contains(&id) => {
                        let mut bad = body.clone();
                        bad.push(0xEE);
                        p.on_got(id, &bad);
                    }
                    Some((_, body)) => {
                        let body = body.clone();
                        p.on_got(id, &body);
                    }
                    None => p.on_missed(id),
                },
                KeepEffect::Put { id, kind, bytes } => {
                    assert_eq!(block_id(kind, &bytes), id, "the pass PUT bytes that are not their id");
                    net.insert(id, (kind, bytes));
                    puts.push(id);
                    p.on_put_ok(id);
                }
            }
        }
    }
    let r = p.report();
    assert!(r.done(), "the pass ended with work out: {r:?}");
    (r, puts)
}

/// The group with the most members under a node at `level`, as
/// `(members, parity ids)`.
fn a_group(net: &Net, root: Cid, level: u8) -> (Vec<Cid>, Vec<Cid>) {
    let mut best: Option<(Vec<Cid>, Vec<Cid>)> = None;
    let mut stack = vec![root];
    while let Some(c) = stack.pop() {
        let Some((kind::TREE_NODE, bytes)) = net.get(&c) else { continue };
        let n = Node::parse(bytes).expect("a node");
        if n.level() == level {
            let ids: Vec<Cid> = n.parity().collect();
            for (g, (_, m)) in parity::group_members(&n).into_iter().enumerate() {
                if best.as_ref().is_none_or(|(b, _)| m.len() > b.len()) {
                    best = Some((m, ids[3 * g..3 * g + 3].to_vec()));
                }
            }
        }
        if !n.is_leaf() {
            for i in 0..n.len() {
                stack.push(n.child(i).0);
            }
        }
    }
    best.expect("a group at that level")
}

fn fixture() -> (Cid, Net) {
    let (root, net) = tree(1500);
    let levels: BTreeSet<u8> = net.values().filter(|(k, _)| *k == kind::TREE_NODE).map(|(_, b)| Node::parse(b).unwrap().level()).collect();
    assert!(levels.len() >= 3, "the fixture is {} level(s) deep; it needs leaves, level 1 and above", levels.len());
    (root, net)
}

#[test]
fn an_intact_tree_is_read_whole_and_nothing_is_put() {
    let (root, mut net) = fixture();
    let (r, puts) = pass(root, &mut net, &BTreeSet::new());
    assert_eq!(r.blocks, net.len(), "the pass did not reach every block the root names");
    assert_eq!(r.held, net.len());
    assert!(puts.is_empty(), "an intact tree needed no puts, got {}", puts.len());
    assert!(r.damaged.is_empty() && r.refused == 0 && !r.root_missing);
    let parity = net.values().filter(|(k, _)| *k == kind::PARITY).count();
    assert!(parity > 0, "the fixture has no parity");
    println!("  intact: {} blocks ({parity} parity), {} B kept", r.blocks, r.bytes_kept);
}

/// Lose `lost` and check the node ends holding exactly what it held before.
fn lose_and_keep(lost: &[Cid]) -> (Report, Vec<Cid>) {
    let (root, mut net) = fixture();
    let whole = net.clone();
    for id in lost {
        assert!(net.remove(id).is_some(), "the fixture does not hold a block it was told to lose");
    }
    let (r, puts) = pass(root, &mut net, &BTreeSet::new());
    assert_eq!(net, whole, "after the pass the node does not hold the tree it held before the loss");
    (r, puts)
}

#[test]
fn three_lost_from_a_group_of_leaves_are_rebuilt_and_put() {
    let (root, net) = fixture();
    let (members, par) = a_group(&net, root, 1);
    let lost = [members[0], members[members.len() - 1], par[1]];
    let (r, puts) = lose_and_keep(&lost);
    assert_eq!((r.repaired, r.parity_reput), (2, 1), "{r:?}");
    assert_eq!(puts.iter().copied().collect::<BTreeSet<_>>(), lost.iter().copied().collect(), "the pass put something other than what was lost");
}

#[test]
fn three_lost_from_a_group_of_values_are_rebuilt_and_put() {
    let (root, net) = fixture();
    let (members, par) = a_group(&net, root, 0);
    assert!(members.iter().all(|m| net[m].0 == kind::RAW), "a leaf's group is over its referenced values");
    let lost = [members[1], par[0], par[2]];
    let (r, _) = lose_and_keep(&lost);
    assert_eq!((r.repaired, r.parity_reput), (1, 2), "{r:?}");
}

#[test]
fn all_three_parity_lost_are_recomputed() {
    let (root, net) = fixture();
    let (_, par) = a_group(&net, root, 1);
    let (r, _) = lose_and_keep(&par);
    assert_eq!((r.repaired, r.parity_reput), (0, 3), "{r:?}");
}

#[test]
fn a_lost_tree_node_is_rebuilt_and_what_is_under_it_is_still_reached() {
    let (root, net) = fixture();
    // A group of level-1 nodes lives on a level-2 node: lose one level-1 node.
    let (members, _) = a_group(&net, root, 2);
    let lost = members[0];
    let under = Node::parse(&net[&lost].1).unwrap().len();
    assert!(under > 0);
    let (r, _) = lose_and_keep(&[lost]);
    assert_eq!(r.repaired, 1);
    assert_eq!(r.blocks, net.len(), "the pass did not descend the rebuilt node: it reached {} of {}", r.blocks, net.len());
}

#[test]
fn four_lost_name_the_group_damaged_and_nothing_is_put_for_it() {
    let (root, mut net) = fixture();
    let (members, par) = a_group(&net, root, 1);
    let lost = [members[0], members[1], members[2], par[0]];
    for id in &lost {
        net.remove(id);
    }
    let (r, puts) = pass(root, &mut net, &BTreeSet::new());
    assert_eq!(r.damaged, vec![par[0]], "the damaged group is named by its first parity id");
    for id in &lost {
        assert!(!puts.contains(id), "a block of a damaged group was put");
        assert!(!net.contains_key(id), "a block of a damaged group reappeared");
    }
}

#[test]
fn a_forged_arrival_is_refused_and_repaired_like_a_loss() {
    let (root, mut net) = fixture();
    let whole = net.clone();
    let (members, _) = a_group(&net, root, 0);
    let forged: BTreeSet<Cid> = [members[2]].into();
    let (r, puts) = pass(root, &mut net, &forged);
    assert_eq!(r.refused, 1);
    assert_eq!(r.repaired, 1);
    assert_eq!(puts, vec![members[2]]);
    assert_eq!(net, whole);
}

#[test]
fn a_closed_pages_residual_parity_is_completed() {
    let (root, net) = fixture();
    // Two groups whose parity was never acked (COMMIT-LIFE §P's residual).
    let (_, a) = a_group(&net, root, 1);
    let (_, b) = a_group(&net, root, 0);
    let lost: Vec<Cid> = a.iter().chain(b.iter()).copied().collect();
    let (r, _) = lose_and_keep(&lost);
    assert_eq!(r.parity_reput, 6, "{r:?}");
}

#[test]
fn a_second_pass_puts_nothing() {
    let (root, mut net) = fixture();
    let (members, par) = a_group(&net, root, 1);
    net.remove(&members[0]);
    net.remove(&par[2]);
    let (first, _) = pass(root, &mut net, &BTreeSet::new());
    assert_eq!(first.repaired + first.parity_reput, 2);
    let (second, puts) = pass(root, &mut net, &BTreeSet::new());
    assert!(puts.is_empty(), "the second pass put {} block(s)", puts.len());
    assert_eq!(second.held, net.len());
}

#[test]
fn a_lost_root_is_named_not_rebuilt() {
    let (root, mut net) = fixture();
    net.remove(&root);
    let (r, puts) = pass(root, &mut net, &BTreeSet::new());
    assert!(r.root_missing);
    assert!(puts.is_empty());
}

#[test]
fn the_fixture_has_referenced_values_on_its_leaves() {
    let (_, net) = fixture();
    let refs = net
        .values()
        .filter(|(k, _)| *k == kind::TREE_NODE)
        .map(|(_, b)| Node::parse(b).unwrap())
        .filter(|n| n.is_leaf())
        .map(|n| (0..n.len()).filter(|&i| matches!(n.value(i), Value::Ref { .. })).count())
        .sum::<usize>();
    assert!(refs > 0, "no leaf references a value: leaf groups are not exercised");
}
