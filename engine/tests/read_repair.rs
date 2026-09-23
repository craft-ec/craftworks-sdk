//! A READ REPAIRS THROUGH PARITY (Phase 4, DURABILITY-STATE gap 1; the
//! owner's test: data survives when the nodes that held it go away).
//!
//! A real tree, written by a real engine with its parity, is read cold by a
//! second engine from a "network" that has LOST blocks of one sibling group:
//! * up to 3 of the group gone (members, or members and parity): every read
//!   answers exactly what the fully-held tree holds -- the lost blocks are
//!   rebuilt from the group, verified by hash, and kept;
//! * 4 gone: the reads that needed them end `Unavailable` naming the block,
//!   and the engine says why (the group could not reach k);
//! * THE CONTROL: repair off, ONE block gone -- `Unavailable`. So the passes
//!   above are the repair's, not a network that still had the block.

use engine::read::{ReadResult, ReqId};
use engine::{ClientId, Effect, Engine, Event, Params};
use freenet_prolly::node::Node;
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

mod common;
use common::{cold_reader, tree, Store};

/// The records: enough small rows for a branch whose children (leaves) form
/// full sibling groups.
fn records() -> BTreeMap<Vec<u8>, Vec<u8>> {
    (0..6000u32).map(|i| (format!("k/{i:06}").into_bytes(), format!("value {i}").into_bytes())).collect()
}

/// A node one level above the leaves, and one of its groups: `(members,
/// parity)`, members being leaves. The node's parity BLOCKS are put on the
/// network too -- computed with `blocks_of`, the function the writer puts them
/// with, and checked to be exactly the ids the node lists (the fixture writer
/// stops at `Published`, before its parity is due).
fn a_leaf_group(all: &mut MemBlocks, root: Cid) -> (Vec<Cid>, Vec<Cid>) {
    let mut at = root;
    loop {
        let bytes = all.get(&at).expect("held").to_vec();
        let n = Node::parse(&bytes).expect("a node");
        assert!(!n.is_leaf(), "the tree is one leaf: no groups to lose");
        if n.level() == 1 {
            let ids: Vec<Cid> = n.parity().collect();
            let blocks = freenet_prolly::parity::blocks_of(&n, &*all).expect("every member held");
            assert_eq!(blocks.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids, "the parity computed is not what the node lists");
            for (id, b) in blocks {
                all.insert(id, &b);
            }
            let groups = freenet_prolly::parity::group_members(&n);
            let (g, (_, members)) = groups.into_iter().enumerate().max_by_key(|(_, (_, m))| m.len()).expect("a group");
            assert!(ids.len() >= 3 * (g + 1), "the node lists no parity for its groups: pcount {}", ids.len());
            return (members, ids[3 * g..3 * g + 3].to_vec());
        }
        at = n.child(0).0;
    }
}

/// Every key held in `leaves`.
fn keys_in(all: &MemBlocks, leaves: &[Cid]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for l in leaves {
        let n = Node::parse(all.get(l).expect("held")).expect("a leaf");
        for i in 0..n.len() {
            out.push(n.key(i));
        }
    }
    out
}

/// What a cold reader answers for `keys`, from a network holding `all` minus
/// `lost`; the page keeps what the engine rebuilt (`Keep`).
///
/// The engine re-asks a missed block with NO count that ends it (rule 7);
/// the PAGE paces those re-asks on a backoff. This harness has no clock, so
/// it plays the pacing as "a block asked more than [`PACED`] times is not
/// asked again yet": its later re-asks are held, as a paced page holds them
/// until due. A read that is still waiting then is a read the page would
/// show as "not answering" -- never one that ended.
const PACED: usize = 3;

fn read_cold(root: Cid, all: &MemBlocks, lost: &BTreeSet<Cid>, keys: &[Vec<u8>], params: Params) -> (BTreeMap<Vec<u8>, ReadResult>, Engine<Store>) {
    let (mut e, store) = cold_reader(root, params);
    let mut answers = BTreeMap::new();
    for (n, key) in keys.iter().enumerate() {
        // Paced per read: each read is its own stretch of time.
        let mut asked: BTreeMap<Cid, usize> = BTreeMap::new();
        let mut queue = e.step(Event::Get { client: ClientId(1), req_id: ReqId(n as u64), key: key.clone() });
        let mut steps = 0;
        while let Some(f) = queue.pop() {
            steps += 1;
            assert!(steps < 20_000, "a read did not settle");
            match f {
                Effect::FetchBlock { id, .. } => {
                    let times = asked.entry(id).or_insert(0);
                    *times += 1;
                    if *times > PACED {
                        continue;
                    }
                    let ev = match all.get(&id).filter(|_| !lost.contains(&id)) {
                        Some(b) => {
                            store.put(id, b);
                            Event::BlockArrived { id, bytes: b.to_vec() }
                        }
                        None => Event::BlockMissed(id),
                    };
                    queue.extend(e.step(ev));
                }
                Effect::Keep { id, bytes } => store.put(id, &bytes),
                Effect::Reply { req_id, result, .. } if req_id == ReqId(n as u64) => {
                    answers.insert(key.clone(), result);
                }
                _ => {}
            }
        }
    }
    (answers, e)
}

fn right(answers: &BTreeMap<Vec<u8>, ReadResult>, records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<String> {
    answers
        .iter()
        .filter_map(|(k, r)| match r {
            ReadResult::Value(Some(v)) if Some(v) == records.get(k) => None,
            other => Some(format!("{}: {other:?}", String::from_utf8_lossy(k))),
        })
        .collect()
}

#[test]
fn up_to_three_lost_blocks_of_a_group_are_rebuilt_and_every_read_is_right() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, parity) = a_leaf_group(&mut all, root);
    let k = members.len();
    assert!(k >= 7, "the group has {k} members: too small to be a real group");
    for (case, lost) in [
        ("1 member", vec![members[0]]),
        ("3 members", vec![members[0], members[k / 2], members[k - 1]]),
        ("2 members and 1 parity", vec![members[1], members[2], parity[0]]),
    ] {
        let lost: BTreeSet<Cid> = lost.into_iter().collect();
        let lost_leaves: Vec<Cid> = members.iter().copied().filter(|m| lost.contains(m)).collect();
        let keys = keys_in(&all, &lost_leaves);
        assert!(!keys.is_empty(), "{case}: nothing to read in the lost leaves");
        let (answers, e) = read_cold(root, &all, &lost, &keys, Params::default());
        let wrong = right(&answers, &records);
        let (started, rebuilt, given_up) = e.repair_counts();
        println!("  {case}: {} reads over {} lost leaves -- repairs started {started}, rebuilt {rebuilt}, given up {given_up}", keys.len(), lost_leaves.len());
        assert!(wrong.is_empty(), "{case}: {} of {} reads wrong after losing {} blocks of a group: {:?}", wrong.len(), keys.len(), lost.len(), &wrong[..wrong.len().min(3)]);
        assert_eq!(answers.len(), keys.len(), "{case}: a read never answered");
        assert_eq!(rebuilt, lost_leaves.len() as u64, "{case}: the lost leaves were not each rebuilt once");
        assert_eq!(given_up, 0, "{case}");
    }
}

/// Past the group's reach the read WAITS (rule 7): it is never answered
/// `Unavailable`, and the engine still awaits the block, so the page keeps
/// asking for it and shows "not answering for N s". The block may come back
/// (a keeper's repair, a late PUT); `Unavailable` would have thrown away the
/// read that could then be answered.
#[test]
fn four_lost_blocks_of_a_group_wait_and_are_never_answered_unavailable() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, _) = a_leaf_group(&mut all, root);
    let lost: BTreeSet<Cid> = members[..4].iter().copied().collect();
    let keys = keys_in(&all, &members[..1]);
    let (answers, e) = read_cold(root, &all, &lost, &keys, Params::default());
    assert!(answers.is_empty(), "past the group's reach a read ended: {answers:?}");
    assert!(e.awaits_block(&members[0]), "the engine stopped waiting on the lost block: nothing would ask for it again");
    assert_eq!(e.repair_counts().1, 0, "a group short of k rebuilt something");
    println!("  4 lost: {} read(s) waiting, block still awaited, repairs {:?}", keys.len(), e.repair_counts());
}

/// THE CONTROL: the same network, repair off -- one lost member is a read
/// that is not answered (it waits on the block). The passes above are the
/// repair's.
#[test]
fn control_with_repair_off_one_lost_block_is_not_read() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, _) = a_leaf_group(&mut all, root);
    let lost: BTreeSet<Cid> = [members[0]].into_iter().collect();
    let keys = keys_in(&all, &members[..1]);
    let (answers, e) = read_cold(root, &all, &lost, &keys, Params { repair_reads: false, ..Params::default() });
    assert!(answers.is_empty(), "with repair off a lost block was answered: {answers:?}");
    assert!(e.awaits_block(&members[0]));
    assert_eq!(e.repair_counts(), (0, 0, 0));
}

/// A NETWORK THAT SENDS RUBBISH for a group block cannot plant a value: a
/// block whose bytes do not hash to its slot counts as missing, and the
/// rebuilt block is kept only if it hashes to the id asked for.
#[test]
fn a_group_block_that_does_not_hash_to_its_slot_is_not_used() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, parity) = a_leaf_group(&mut all, root);
    // Every parity block answers with the wrong bytes, and one member is lost:
    // the member alone is k-1 of the members, so without honest parity the
    // group cannot reach k.
    let mut forged = MemBlocks::default();
    let mut ids = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(c) = stack.pop() {
        if !ids.insert(c) {
            continue;
        }
        if let Some(b) = all.get(&c) {
            forged.insert(c, b);
            if let Ok(n) = Node::parse(b) {
                if !n.is_leaf() {
                    stack.extend((0..n.len()).map(|i| n.child(i).0));
                }
            }
        }
    }
    // Plausible forgeries: the REAL parity with one byte flipped -- the same
    // length, so the solve succeeds and yields a wrong member. Only the hash
    // checks can refuse it (the slot's, then the rebuilt block's).
    for p in &parity {
        let mut b = all.get(p).expect("the group's parity").to_vec();
        let last = b.len() - 1;
        b[last] ^= 0x5A;
        forged.insert(*p, &b);
    }
    let lost: BTreeSet<Cid> = [members[0]].into_iter().collect();
    let keys = keys_in(&all, &members[..1]);
    let (answers, e) = read_cold(root, &forged, &lost, &keys, Params::default());
    assert!(answers.is_empty(), "forged parity answered a read: {answers:?}");
    assert_eq!(e.repair_counts().1, 0, "a block was rebuilt from forged parity");
    assert!(e.awaits_block(&members[0]), "the lost block is no longer awaited");
}
