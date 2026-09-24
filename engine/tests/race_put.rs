//! RACE PUT's accounting (COMMIT-LIFE §P), driven through the engine's own
//! events with the acks under the test's control:
//!
//! * SUPERSEDED stragglers: a later commit that re-codes a group an earlier
//!   commit is still putting WITHDRAWS the earlier commit's blocks there, and
//!   the earlier write is BACKED_UP with the newer version of the group --
//!   not before it is full, and not never.
//! * VALUE blocks are never withdrawn: the same value under two keys is one
//!   block, so a removed leaf's value may still be needed.
//! * The head's race set: an earlier commit's straggler is NEVER counted
//!   present toward a group's k, and `after` names only this commit's acked
//!   blocks.

use engine::{ClientId, Effect, Engine, Event, Op, Params, State, WriteId, PARITY};
use freenet_prolly::node::Node;
use freenet_prolly::{parity, Cid};
use std::collections::{BTreeMap, BTreeSet};

mod common;
use common::Store;

fn put(k: &str, v: &[u8]) -> (Vec<u8>, Op) {
    (k.as_bytes().to_vec(), Op::Put(v.to_vec()))
}

fn puts(fx: &[Effect]) -> BTreeMap<Cid, Vec<u8>> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::PutBlock { id, bytes, .. } => Some((*id, bytes.clone())),
            _ => None,
        })
        .collect()
}

fn head(fx: &[Effect]) -> Option<(u64, Vec<Cid>)> {
    fx.iter().find_map(|f| match f {
        Effect::UpdateHead { seq, after, .. } => Some((*seq, after.clone())),
        _ => None,
    })
}

fn states(fx: &[Effect], id: u64) -> Vec<State> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notify { write_id, state, .. } if *write_id == WriteId(id) => Some(*state),
            _ => None,
        })
        .collect()
}

fn withdrawn(fx: &[Effect]) -> BTreeSet<Cid> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Withdraw { id } => Some(*id),
            _ => None,
        })
        .collect()
}

/// An engine over a node that holds what it is sent, and every block it was
/// asked to put, by id (the test's view of the tree).
struct Rig {
    e: Engine<Store>,
    known: BTreeMap<Cid, Vec<u8>>,
}

impl Rig {
    /// A published base tree of 600 small rows, every block acked.
    fn base() -> Rig {
        let mut r = Rig { e: common::new_store_params(Params::default()), known: BTreeMap::new() };
        let ops: Vec<(Vec<u8>, Op)> = (0..600u32).map(|i| put(&format!("k/{i:06}"), &[(i % 251) as u8; 20])).collect();
        let all = r.commit(1, ops, |_, _| true);
        assert!(states(&all, 1).contains(&State::Published), "the base write did not publish");
        r
    }

    fn step(&mut self, ev: Event) -> Vec<Effect> {
        let fx = self.e.step(ev);
        self.e.blocks().absorb(&fx);
        self.known.extend(puts(&fx));
        fx
    }

    /// Commit `ops` as write `id`, acking every block `ack` allows; land the
    /// head once it is asked for. Every effect, in order.
    fn commit(&mut self, id: u64, ops: Vec<(Vec<u8>, Op)>, ack: impl Fn(&Cid, &[u8]) -> bool) -> Vec<Effect> {
        let fx = self.step(Event::forced_write(ClientId(1), WriteId(id), ops));
        let mut all = fx.clone();
        for (bid, bytes) in puts(&fx) {
            if ack(&bid, &bytes) {
                all.extend(self.step(Event::PutConfirmed(bid)));
            }
        }
        if let Some((seq, _)) = head(&all) {
            all.extend(self.step(Event::HeadConfirmed(seq)));
        }
        all
    }
}

/// The new LEAF a commit put, holding `key`.
fn leaf_with(fx: &[Effect], key: &[u8]) -> Cid {
    puts(fx)
        .into_iter()
        .find(|(_, b)| Node::parse(b).is_ok_and(|n| n.is_leaf() && (0..n.len()).any(|i| n.key(i) == key)))
        .map(|(id, _)| id)
        .expect("the commit put a leaf holding the key")
}

/// The leaf write 2 (`k/000100` := "two") puts, learned on a probe engine:
/// the tree is deterministic, so a fresh engine puts the same block.
fn l2() -> Cid {
    let mut p = Rig::base();
    let fx = p.step(Event::forced_write(ClientId(1), WriteId(2), vec![put("k/000100", b"two")]));
    leaf_with(&fx, b"k/000100")
}

/// Write 2 published with ITS LEAF as the only straggler (race put: the head
/// signs at k of k+m).
fn with_straggler() -> (Rig, Cid) {
    let l2 = l2();
    let mut r = Rig::base();
    let all = r.commit(2, vec![put("k/000100", b"two")], |id, _| *id != l2);
    assert!(states(&all, 2).contains(&State::Published), "write 2 did not publish with one straggler");
    assert!(!states(&all, 2).contains(&State::ParityComplete), "write 2 BACKED_UP with its leaf un-acked");
    (r, l2)
}

#[test]
fn a_later_commit_re_coding_the_group_withdraws_the_straggler_and_backs_up_the_earlier_write() {
    let (mut r, l2) = with_straggler();
    // Write 3 re-codes the SAME leaf and its group: l2 is garbage now.
    let all = r.commit(3, vec![put("k/000100", b"three")], |_, _| true);
    let w = withdrawn(&all);
    assert!(w.contains(&l2), "the superseded straggler was not withdrawn: {w:?}");
    assert!(states(&all, 3).contains(&State::ParityComplete), "write 3 is fully acked and not BACKED_UP");
    assert!(states(&all, 2).contains(&State::ParityComplete), "write 2's data lives in write 3's full group and it is not BACKED_UP: the straggler is waited on for ever");
    assert_eq!(r.e.backing(), 0, "a Backing still waits on a withdrawn block");
}

#[test]
fn an_earlier_write_is_not_backed_up_while_its_current_group_has_a_hole() {
    let (mut probe, _) = with_straggler();
    let l3 = leaf_with(&probe.step(Event::forced_write(ClientId(1), WriteId(3), vec![put("k/000100", b"three")])), b"k/000100");
    let (mut r, l2) = with_straggler();
    // Write 3 re-codes the group, but ITS new leaf is the straggler now.
    let all = r.commit(3, vec![put("k/000100", b"three")], |id, _| *id != l3);
    assert!(withdrawn(&all).contains(&l2), "the superseded straggler was not withdrawn");
    assert!(states(&all, 3).contains(&State::Published));
    assert!(!states(&all, 2).contains(&State::ParityComplete), "write 2 is BACKED_UP while the current version of its group misses a block");
    assert!(!states(&all, 3).contains(&State::ParityComplete));
    // The hole fills: both are BACKED_UP.
    let fx = r.step(Event::PutConfirmed(l3));
    assert!(states(&fx, 2).contains(&State::ParityComplete), "write 2 never BACKED_UP after its group filled");
    assert!(states(&fx, 3).contains(&State::ParityComplete));
}

#[test]
fn a_value_block_shared_by_an_unchanged_leaf_is_never_withdrawn() {
    // A value over MAX_INLINE is a block of its own; the SAME bytes under two
    // keys far apart (two leaves) are ONE block.
    let big = vec![7u8; freenet_prolly::node::MAX_INLINE + 100];
    let v = freenet_prolly::block_id(freenet_prolly::kind::RAW, &big);
    let mut r = Rig::base();
    let all = r.commit(2, vec![put("k/000050", &big), put("k/000550", &big)], |id, _| *id != v);
    assert!(puts(&all).contains_key(&v), "the shared value was not put as its own block");
    assert!(states(&all, 2).contains(&State::Published));
    assert!(!states(&all, 2).contains(&State::ParityComplete));
    // Write 3 rewrites ONE of the keys: its leaf is removed; the other leaf
    // still references the value.
    let all = r.commit(3, vec![put("k/000050", b"small")], |_, _| true);
    assert!(!withdrawn(&all).contains(&v), "a value block still referenced by an unchanged leaf was WITHDRAWN");
    assert!(!states(&all, 2).contains(&State::ParityComplete), "write 2 BACKED_UP with its value block never acked");
    let fx = r.step(Event::PutConfirmed(v));
    assert!(states(&fx, 2).contains(&State::ParityComplete), "write 2 never BACKED_UP after its value landed");
}

#[test]
fn an_earlier_straggler_is_never_counted_present_and_after_names_only_this_commits_acked_blocks() {
    let (mut r, l2) = with_straggler();
    // l2's group, as its parent lists it.
    let parent = r
        .known
        .values()
        .filter_map(|b| Node::parse(b).ok())
        .find(|n| !n.is_leaf() && parity::group_members(n).iter().any(|(_, m)| m.contains(&l2)))
        .expect("l2's parent was put");
    let (_, members) = parity::group_members(&parent).into_iter().find(|(_, m)| m.contains(&l2)).unwrap();
    // A key in a SIBLING leaf of the same group: write 3 changes that leaf,
    // so l2 is an unchanged member of a group write 3 changes -- un-acked.
    let sibling = *members.iter().find(|m| **m != l2).unwrap();
    let key = Node::parse(&r.known[&sibling]).unwrap().key(0);
    let fx3 = r.step(Event::forced_write(ClientId(1), WriteId(3), vec![(key, Op::Put(b"three".to_vec()))]));
    let new3 = puts(&fx3);
    let pn = new3
        .values()
        .filter_map(|b| Node::parse(b).ok())
        .find(|n| !n.is_leaf() && parity::group_members(n).iter().any(|(_, m)| m.contains(&l2)))
        .expect("write 3 re-codes l2's parent");
    let g = parity::group_members(&pn).into_iter().position(|(_, m)| m.contains(&l2)).unwrap();
    let pids: Vec<Cid> = pn.parity().collect();
    let (_, gm) = parity::group_members(&pn).into_iter().nth(g).unwrap();
    let group_new: BTreeSet<Cid> = gm.iter().chain(&pids[PARITY * g..PARITY * (g + 1)]).copied().filter(|c| new3.contains_key(c)).collect();
    // Write 3's own blocks in that group: its new leaf and PARITY new parity.
    assert!(group_new.len() >= 2, "write 3 put {} block(s) of the group", group_new.len());
    // Ack everything of write 3 EXCEPT all but one of its blocks in that
    // group. Counting l2 would make it (k-2 present) + 1 (l2) + 1 = k: the
    // head would sign. Not counting it: k-1, no head.
    let keep_one = *group_new.iter().next().unwrap();
    let mut all = fx3.clone();
    for id in new3.keys() {
        if !group_new.contains(id) || *id == keep_one {
            all.extend(r.step(Event::PutConfirmed(*id)));
        }
    }
    assert!(head(&all).is_none(), "the head was signed with an earlier commit's UN-ACKED straggler counted present");
    // The straggler lands: the group has k now, and the head signs.
    let more = r.step(Event::PutConfirmed(l2));
    let (_, after) = head(&more).expect("with the earlier straggler acked, the group reaches k and the head signs");
    // `after` names only blocks write 3 put AND that were acked.
    for id in &after {
        assert!(new3.contains_key(id), "after names a block write 3 did not put: {id:?}");
        assert!(!group_new.contains(id) || *id == keep_one, "after names a block of write 3 that was never acked");
    }
    assert!(!after.is_empty(), "after is empty: the check above is vacuous");
}

/// SUPERSESSION ON A FOREIGN MOVE (the architect: "every published-root
/// move"): another device of the same identity publishes a head whose tree
/// re-codes the group this engine's straggler is in. Adopting it withdraws
/// the straggler, and the write is CARRIED -- never BACKED_UP on the foreign
/// tree's say-so, whose stragglers nothing here tracks -- until this engine's
/// next own commit backs it up.
#[test]
fn a_foreign_head_re_coding_the_group_withdraws_the_straggler_and_the_write_waits_for_the_next_own_commit() {
    // The foreign tree: the same base, then k/000100 := "foreign" (seq 2),
    // built by another engine and held by the same node.
    let foreign = {
        let mut p = Rig::base();
        let all = p.commit(2, vec![put("k/000100", b"foreign")], |_, _| true);
        assert!(states(&all, 2).contains(&State::Published));
        p.e.published_root()
    };
    let (mut r, l2) = with_straggler();
    // A newer head (seq 3) arrives with no commit in flight: adopted.
    let fx = r.step(Event::HeadRead { epoch: engine::Epoch(1), seq: 3, root: foreign });
    assert!(withdrawn(&fx).contains(&l2), "a foreign move re-coding the group did not withdraw the straggler: {:?}", withdrawn(&fx));
    assert!(!states(&fx, 2).contains(&State::ParityComplete), "write 2 BACKED_UP on a FOREIGN tree's say-so");
    assert_eq!(r.e.backing(), 0, "the withdrawn straggler's Backing is still waiting");
    // The next own commit, fully acked, takes the carried write with it.
    let all = r.commit(4, vec![put("k/000200", b"four")], |_, _| true);
    assert!(states(&all, 4).contains(&State::ParityComplete));
    assert!(states(&all, 2).contains(&State::ParityComplete), "the carried write was never BACKED_UP by the next own commit");
}

/// THE SAME VALUE UNDER MANY KEYS OF ONE LEAF is ONE block filling many
/// slots of the leaf's value group. Counted as a set it filled one slot, the
/// group never reached k, and the commit never published (measured: 50 rows
/// of one 1,400 B value -- 14 puts acked, no head, the write `Stalled`).
#[test]
fn repeated_values_in_one_group_publish() {
    let mut r = Rig::base();
    let v = vec![7u8; freenet_prolly::node::MAX_INLINE + 400];
    let ops: Vec<(Vec<u8>, Op)> = (0..50u32).map(|i| put(&format!("r/{i:06}"), &v)).collect();
    let all = r.commit(2, ops, |_, _| true);
    assert!(head(&all).is_some(), "no head: a group of one value in many slots never reached k");
    assert!(states(&all, 2).contains(&State::Published), "{:?}", states(&all, 2));
    assert!(states(&all, 2).contains(&State::ParityComplete), "{:?}", states(&all, 2));
}

// ---------------------------------------------------------------------------
// COMMIT-LIFE §P's gate tests.
// ---------------------------------------------------------------------------

fn is_parity(id: &Cid, bytes: &[u8]) -> bool {
    freenet_prolly::block_id(freenet_prolly::kind::PARITY, bytes) == *id
}

impl Rig {
    fn base_with(params: Params) -> Rig {
        let mut r = Rig { e: common::new_store_params(params), known: BTreeMap::new() };
        let ops: Vec<(Vec<u8>, Op)> = (0..600u32).map(|i| put(&format!("k/{i:06}"), &[(i % 251) as u8; 20])).collect();
        let _ = r.commit(1, ops, |_, _| true);
        r
    }
}

/// §P 1: a commit whose slowest PUTs never answer -- every PARITY block of the
/// commit (3 per changed group, so each group keeps exactly its k members) --
/// still PUBLISHES, and is SAVED, not BACKED_UP. The CONTROL, run here: with
/// race put off (wait for every PUT) the same commit does NOT sign.
#[test]
fn a_commit_whose_slowest_puts_never_answer_still_publishes_saved_not_backed_up() {
    for race in [true, false] {
        let mut r = Rig::base_with(Params { race_put: race, ..Params::default() });
        let all = r.commit(2, vec![put("k/000100", b"two")], |id, b| !is_parity(id, b));
        let held = puts(&all).iter().filter(|(id, b)| is_parity(id, b)).count();
        assert!(held >= 3, "race={race}: the commit coded {held} parity block(s): nothing to hold back");
        if race {
            assert!(states(&all, 2).contains(&State::Published), "race put: {held} slow parity PUTs held the head back");
            assert!(!states(&all, 2).contains(&State::ParityComplete), "BACKED_UP with {held} PUTs never answered");
        } else {
            assert!(head(&all).is_none(), "the CONTROL (wait for all) signed with {held} PUTs un-acked: this test cannot tell race put from it");
        }
    }
}

/// §P 3: NO parity PUT is sequenced after a data block's ack. Every block the
/// commit needs -- its data AND the parity its new nodes list -- is in its
/// FIRST send, and no confirmation afterwards makes a new PUT.
#[test]
fn every_put_of_a_commit_leaves_in_its_first_send() {
    let mut r = Rig::base();
    let first = r.step(Event::forced_write(ClientId(1), WriteId(2), vec![put("k/000100", b"two"), put("k/000400", b"four")]));
    let sent = puts(&first);
    let listed: BTreeSet<Cid> = sent.values().filter_map(|b| Node::parse(b).ok()).flat_map(|n| n.parity().collect::<Vec<_>>()).collect();
    let coded: BTreeSet<Cid> = sent.iter().filter(|(id, b)| is_parity(id, b)).map(|(id, _)| *id).collect();
    assert!(!coded.is_empty(), "the commit put no parity in its first send");
    // Every parity id a new node lists that is not already on the node (a
    // reused group's) is in the first send.
    let missing: Vec<&Cid> = listed.iter().filter(|p| !coded.contains(*p) && !r.known.contains_key(*p)).collect();
    assert!(missing.is_empty(), "{} parity block(s) a new node lists were not in the commit's first send", missing.len());
    for id in sent.keys() {
        let fx = r.step(Event::PutConfirmed(*id));
        assert!(puts(&fx).is_empty(), "a PUT was sequenced after a data ack: {:?}", puts(&fx).keys().collect::<Vec<_>>());
    }
}

/// §P 4: a commit whose ROOT is un-acked does not sign (nothing can rebuild a
/// root), and neither does one with a changed group missing k+1 -- here its
/// new leaf AND its PARITY parity (k-1 of k+PARITY left).
#[test]
fn neither_an_unacked_root_nor_a_group_below_k_signs() {
    // The root: everything acked but it.
    let mut r = Rig::base();
    let fx = r.step(Event::forced_write(ClientId(1), WriteId(2), vec![put("k/000100", b"two")]));
    let root = r.e.root();
    let mut all = fx.clone();
    for id in puts(&fx).keys().filter(|id| **id != root) {
        all.extend(r.step(Event::PutConfirmed(*id)));
    }
    assert!(head(&all).is_none(), "the head was signed with the ROOT un-acked");
    let more = r.step(Event::PutConfirmed(root));
    assert!(head(&more).is_some(), "with the root acked too, the head did not sign");

    // A group below k: its new leaf and all PARITY of its parity held.
    let l2 = l2();
    let mut r = Rig::base();
    let fx = r.step(Event::forced_write(ClientId(1), WriteId(2), vec![put("k/000100", b"two")]));
    let sent = puts(&fx);
    let parent = sent.values().filter_map(|b| Node::parse(b).ok()).find(|n| !n.is_leaf() && parity::group_members(n).iter().any(|(_, m)| m.contains(&l2))).expect("the leaf's parent");
    let g = parity::group_members(&parent).into_iter().position(|(_, m)| m.contains(&l2)).unwrap();
    let pids: Vec<Cid> = parent.parity().collect();
    let hold: BTreeSet<Cid> = std::iter::once(l2).chain(pids[PARITY * g..PARITY * (g + 1)].iter().copied()).collect();
    let mut all = fx.clone();
    for id in sent.keys().filter(|id| !hold.contains(*id)) {
        all.extend(r.step(Event::PutConfirmed(*id)));
    }
    assert!(head(&all).is_none(), "the head was signed with a changed group at k-1 of k+m");
}

/// The GROUPING BY THE TREE'S PARITY (the architect, #351): the leaf's parent
/// lists `PARITY` ids per group, the commit puts every one in its first send,
/// and the group signs with ANY `PARITY` of its `PARITY + 1` new blocks still
/// out -- exactly k -- where one more (the test above) does not. A count
/// written by hand is right only while it equals the tree's.
#[test]
fn a_group_signs_with_exactly_parity_of_its_new_blocks_out() {
    let l2 = l2();
    let mut r = Rig::base();
    let fx = r.step(Event::forced_write(ClientId(1), WriteId(2), vec![put("k/000100", b"two")]));
    let sent = puts(&fx);
    let parent = sent.values().filter_map(|b| Node::parse(b).ok()).find(|n| !n.is_leaf() && parity::group_members(n).iter().any(|(_, m)| m.contains(&l2))).expect("the leaf's parent");
    let groups = parity::group_members(&parent);
    let pids: Vec<Cid> = parent.parity().collect();
    assert_eq!(pids.len(), PARITY * groups.len(), "the parent lists {} parity ids for {} group(s)", pids.len(), groups.len());
    let g = groups.iter().position(|(_, m)| m.contains(&l2)).unwrap();
    let group_par = &pids[PARITY * g..PARITY * (g + 1)];
    assert!(group_par.iter().all(|p| sent.contains_key(p)), "the group's parity is not all in the first send");
    // The new blocks of the group: the re-written leaf and its PARITY parity.
    // Hold PARITY of them -- all the parity -- and ack the rest: k is met.
    let hold: BTreeSet<Cid> = group_par.iter().copied().collect();
    assert_eq!(hold.len(), PARITY);
    let mut all = fx.clone();
    for id in sent.keys().filter(|id| !hold.contains(*id)) {
        all.extend(r.step(Event::PutConfirmed(*id)));
    }
    assert!(head(&all).is_some(), "the head did not sign with exactly PARITY of the group's new blocks out");
}

/// §P 6: the largest write that fit before still publishes, with its parity
/// on top (parity rides above `max_commit_blocks`: ≤ PARITY per changed group).
#[test]
fn the_largest_pre_race_put_write_still_publishes_with_its_parity() {
    let p = Params::default();
    let mut r = Rig { e: common::new_store_params(p), known: BTreeMap::new() };
    // Values by reference: one block each, so the data blocks approach the cap.
    let n = (p.max_commit_blocks as u32).saturating_sub(12);
    let ops: Vec<(Vec<u8>, Op)> = (0..n).map(|i| put(&format!("v/{i:06}"), &vec![(i % 251) as u8; 2000])).collect();
    let all = r.commit(1, ops, |_, _| true);
    let sent = puts(&all);
    let parity = sent.iter().filter(|(id, b)| is_parity(id, b)).count();
    let data = sent.len() - parity;
    println!("  {n} rows: {data} data block(s) (cap {}), {parity} parity", p.max_commit_blocks);
    assert!(data + 12 >= p.max_commit_blocks, "the fixture put {data} data blocks: not near the cap");
    assert!(parity > 0);
    assert!(states(&all, 1).contains(&State::Published), "{:?}", states(&all, 1));
    assert!(states(&all, 1).contains(&State::ParityComplete), "{:?}", states(&all, 1));
}
