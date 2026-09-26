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
use engine::{ClientId, Effect, Engine, Event, Params, PARITY};
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
            assert!(ids.len() >= PARITY * (g + 1), "the node lists no parity for its groups: pcount {}", ids.len());
            return (members, ids[PARITY * g..PARITY * (g + 1)].to_vec());
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
                other => common::no_answer_owed(&other),
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
fn up_to_parity_lost_blocks_of_a_group_are_rebuilt_and_every_read_is_right() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, parity) = a_leaf_group(&mut all, root);
    let k = members.len();
    assert!(k > PARITY, "the group has {k} members: too small to lose PARITY of them");
    // PARITY lost, spread evenly from the group's first member to its last.
    let spread: Vec<Cid> = (0..PARITY).map(|i| members[i * (k - 1) / (PARITY - 1)]).collect();
    let mixed: Vec<Cid> = members[1..PARITY].iter().copied().chain(std::iter::once(parity[0])).collect();
    for (case, lost) in [
        ("1 member".to_string(), vec![members[0]]),
        (format!("{PARITY} members"), spread),
        (format!("{} members and 1 parity", PARITY - 1), mixed),
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
fn parity_plus_one_lost_blocks_of_a_group_wait_and_are_never_answered_unavailable() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, _) = a_leaf_group(&mut all, root);
    let lost: BTreeSet<Cid> = members[..PARITY + 1].iter().copied().collect();
    let keys = keys_in(&all, &members[..1]);
    let (answers, e) = read_cold(root, &all, &lost, &keys, Params::default());
    assert!(answers.is_empty(), "past the group's reach a read ended: {answers:?}");
    assert!(e.readers_of(&members[0]).any(), "the engine stopped waiting on the lost block: nothing would ask for it again");
    assert_eq!(e.repair_counts().1, 0, "a group short of k rebuilt something");
    println!("  {} lost: {} read(s) waiting, block still awaited, repairs {:?}", PARITY + 1, keys.len(), e.repair_counts());
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
    assert!(e.readers_of(&members[0]).any());
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
    assert!(e.readers_of(&members[0]).any(), "the lost block is no longer awaited");
}

/// sdk#405, RULE 11 FOR A WRITE (rule 4: one read path): a write whose path needs a block the network LOST is
/// applied from the block REBUILT from its group, as a read of it would be -- it never waits on the straggler.
/// A cold engine over the tree writes a key in a lost leaf: the leaf is answered NotFound every time it is asked
/// (it never arrives), and the write is Published with the leaf rebuilt from the other members and the parity.
/// THE CONTROL: repair off, the same write stays parked (the page would keep re-asking).
#[test]
fn a_write_whose_path_lost_a_block_is_applied_from_its_group() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, _) = a_leaf_group(&mut all, root);
    let lost: BTreeSet<Cid> = BTreeSet::from([members[0]]);
    let key = keys_in(&all, &members[..1]).into_iter().next().expect("a key in the lost leaf");
    for repair in [true, false] {
        let params = Params { repair_reads: repair, ..Params::default() };
        let (mut e, store) = common::cold_reader(root, params);
        let mut asked: BTreeMap<Cid, usize> = BTreeMap::new();
        let mut published = false;
        let mut arrived_lost = false;
        let mut queue = e.step(Event::forced_write(ClientId(1), engine::WriteId(1), vec![(key.clone(), engine::Op::Put(b"written over a lost leaf".to_vec()))]));
        let mut steps = 0;
        while let Some(f) = queue.pop() {
            steps += 1;
            assert!(steps < 50_000, "repair={repair}: the write did not settle");
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
                    arrived_lost |= lost.contains(&id) && matches!(ev, Event::BlockArrived { .. });
                    queue.extend(e.step(ev));
                }
                Effect::Keep { id, bytes } => store.put(id, &bytes),
                Effect::PutBlock { id, bytes, .. } => {
                    store.put(id, &bytes);
                    queue.extend(e.step(Event::PutConfirmed(id)));
                }
                Effect::PutRepaired { id, bytes } => store.put(id, &bytes),
                // #424's question, answered as the PAGE does (the architect on #413 x #424): held when the node holds it
                // (a confirmed or Held-present block counts toward k), else unknown (its group's deferred parity goes).
                Effect::ConfirmHeld { id } => {
                    let held = store.node_holds(&id) || (all.get(&id).is_some() && !lost.contains(&id));
                    queue.extend(e.step(if held { Event::PutConfirmed(id) } else { Event::HeldUnknown(id) }));
                }
                Effect::UpdateHead { seq, .. } => queue.extend(e.step(Event::HeadConfirmed(seq))),
                Effect::Notify { state: engine::State::Published, .. } => published = true,
                other => common::no_answer_owed(&other),
            }
        }
        let (started, rebuilt, _) = e.repair_counts();
        println!("repair={repair}: published {published}; repairs started {started}, rebuilt {rebuilt}; the lost leaf asked {} time(s)", asked.get(&members[0]).copied().unwrap_or(0));
        assert!(!arrived_lost, "THE SETUP: the lost leaf arrived");
        if repair {
            assert!(published, "a write over a lost leaf waited on the straggler instead of rebuilding it from its group");
            assert!(rebuilt >= 1, "published without a rebuild: the leaf was not needed, the test is vacuous");
        } else {
            assert!(!published, "THE CONTROL: with repair off the write published without the lost leaf");
        }
    }
}

/// THE TABLE CELL (the architect on #413 x #424): a changed group's member THIS page rebuilt and re-put is ABSENT until a
/// Held says present -- its repair PUT's answer confirms nothing (ruling (b)) -- and counts toward k from then. Cold
/// reads of keys in two lost leaves rebuild them from their group (`PutRepaired`); then a write in a SIBLING leaf of the
/// same group changes the group and keeps the rebuilt leaves as UNCHANGED other members, which the commit asks the node
/// about (`ConfirmHeld`), answered as the page does.
/// * (i) the repair PUTs landed: the node holds them -> Held present -> they count -> Published;
/// * (ii) they did not land: Held unknown -> the group's deferred parity is released instead -> still Published (#424's
///   no-deadlock property), and the rebuilt leaves were counted absent.
///
/// In both, ConfirmHeld named the rebuilt leaves (non-vacuity: a write OVER a lost leaf replaces it, and never asks).
#[test]
fn a_member_this_page_rebuilt_counts_toward_k_once_the_node_says_it_holds_it() {
    let records = records();
    let (root, mut all) = tree(&records);
    let (members, _) = a_leaf_group(&mut all, root);
    // TWO lost leaves: with one absent member the write's first wave (its new leaf + one parity) already reaches k, so
    // the released parity would not be needed (measured: the no-release mutant survived); two absent needs it.
    let lost: BTreeSet<Cid> = BTreeSet::from([members[0], members[2]]);
    let rebuilt = [members[0], members[2]];
    let read_keys: Vec<Vec<u8>> = rebuilt.iter().map(|m| keys_in(&all, &[*m]).into_iter().next().expect("a key in a lost leaf")).collect();
    let write_key = keys_in(&all, &members[1..2]).into_iter().next().expect("a key in a sibling leaf");
    for landed in [true, false] {
        let (mut e, store) = common::cold_reader(root, Params { repair_reads: true, ..Params::default() });
        let answer = |id: &Cid, store: &Store| match all.get(id).filter(|_| !lost.contains(id)) {
            Some(b) => {
                store.put(*id, b);
                Event::BlockArrived { id: *id, bytes: b.to_vec() }
            }
            None => Event::BlockMissed(*id),
        };
        // 1. The reads: each lost leaf is rebuilt from its group and re-put (landing on the node only in case (i)).
        let mut repaired = Vec::new();
        for (n, read_key) in read_keys.iter().enumerate() {
        let req = ReqId(n as u64 + 1);
        let mut asked: BTreeMap<Cid, usize> = BTreeMap::new();
        let mut got = None;
        let mut queue = e.step(Event::Get { client: ClientId(1), req_id: req, key: read_key.clone() });
        while let Some(f) = queue.pop() {
            match f {
                Effect::FetchBlock { id, .. } => {
                    let times = asked.entry(id).or_insert(0);
                    *times += 1;
                    if *times <= PACED {
                        queue.extend(e.step(answer(&id, &store)));
                    }
                }
                Effect::Keep { id, bytes } => store.put(id, &bytes),
                // The repair's re-put reaches the NODE only in case (i). (The engine's own store is this `store` too, so
                // the node is modelled apart: `node` below.)
                Effect::PutRepaired { id, .. } => repaired.push(id),
                Effect::Reply { req_id, result, .. } if req_id == req => got = Some(result),
                other => common::no_answer_owed(&other),
            }
        }
        assert!(matches!(got, Some(ReadResult::Value(Some(_)))), "landed={landed}: THE SETUP: the read of a lost leaf was not answered from its group: {got:?}");
        }
        assert!(rebuilt.iter().all(|m| repaired.contains(m)), "landed={landed}: THE SETUP: a lost leaf was not rebuilt and re-put");
        // What the NODE holds: the network less the lost leaf, plus whatever this page put since, plus the rebuilt leaf
        // only if its repair PUT landed.
        let mut put_since: BTreeSet<Cid> = BTreeSet::new();
        let node = |id: &Cid, put_since: &BTreeSet<Cid>| (all.get(id).is_some() && !lost.contains(id)) || put_since.contains(id) || (landed && rebuilt.contains(id));
        // 2. The write in a sibling leaf; the commit asks the node about the group's other members.
        let mut named = Vec::new();
        let mut published = false;
        let mut queue = e.step(Event::forced_write(ClientId(1), engine::WriteId(1), vec![(write_key.clone(), engine::Op::Put(b"written beside a rebuilt leaf".to_vec()))]));
        let mut steps = 0;
        while let Some(f) = queue.pop() {
            steps += 1;
            assert!(steps < 50_000, "landed={landed}: the write did not settle");
            match f {
                Effect::FetchBlock { id, .. } => queue.extend(e.step(answer(&id, &store))),
                Effect::Keep { id, bytes } => store.put(id, &bytes),
                Effect::PutBlock { id, bytes, .. } => {
                    store.put(id, &bytes);
                    put_since.insert(id);
                    queue.extend(e.step(Event::PutConfirmed(id)));
                }
                // As the page answers: held when the node holds it, else unknown.
                Effect::ConfirmHeld { id } => {
                    named.push(id);
                    let held = node(&id, &put_since);
                    queue.extend(e.step(if held { Event::PutConfirmed(id) } else { Event::HeldUnknown(id) }));
                }
                Effect::UpdateHead { seq, .. } => queue.extend(e.step(Event::HeadConfirmed(seq))),
                Effect::Notify { state: engine::State::Published, .. } => published = true,
                other => common::no_answer_owed(&other),
            }
        }
        assert!(rebuilt.iter().all(|m| named.contains(m)), "landed={landed}: VACUOUS: the commit never asked about a rebuilt leaf (asked {} others)", named.len());
        assert!(published, "landed={landed}: a write beside a rebuilt leaf never published");
    }
}
