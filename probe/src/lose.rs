//! LOSE DATA AT THE READER (builder#176, Phase 4's finish-line demo): what `ws-lose` decides, kept apart from its
//! sockets so it is tested on real trees.
//!
//! Between a READER's page and its own node, the blocks of ONE group are made absent: their GET answers become the
//! node's own `NotFound` -- the one answer page-io reads as "absent" (wire `GetFail::NotFound`) -- so to the reader
//! those blocks are LOST on the network. Nothing is withheld from any publish and no node's store is touched: the loss
//! is what this reader's node says. Which group:
//!
//! * [`Target::Data`]: the first node group with `k >= 2` members the reader fetches (a branch's children, or a leaf's
//!   referenced values): losing up to `m` of its `k + m` blocks, DATA members first, makes the reader DECODE from
//!   parity (rule 11), not merely take a copy.
//! * [`Target::Root`]: the root's group of one (`k = 1`, sdk#335), from the head's ledger: every tree has it.
//!
//! `m` is [`PARITY`], read here, never written down. `m` lost is the margin; `m + 1` is the control.
use anyhow::{bail, Result};
use freenet_prolly::node::Node;
use freenet_prolly::parity::{group_members, PARITY};
use freenet_prolly::{kind, Cid};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Root,
    Data,
}

impl Target {
    pub fn parse(s: &str) -> Result<Target> {
        match s {
            "root" => Ok(Target::Root),
            "data" => Ok(Target::Data),
            _ => bail!("--group is `root` or `data`, not {s:?}"),
        }
    }
}

/// How many blocks of the group are lost: `m` (the margin) or `m+1` (the control), `m` = [`PARITY`].
pub fn parse_lose(s: &str) -> Result<usize> {
    match s {
        "m" => Ok(PARITY),
        "m+1" => Ok(PARITY + 1),
        _ => bail!("--lose is `m` or `m+1`, not {s:?}"),
    }
}

/// The group chosen, and which of its blocks are lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    /// `k` members, then the group's parity, in the code's column order.
    pub slots: Vec<Cid>,
    pub k: usize,
    pub lost: BTreeSet<Cid>,
}

/// What to do with one GET answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Relay,
    /// Answer the node's `NotFound` instead: this block is lost.
    NotFound(Cid),
}

#[derive(Debug)]
pub struct Lose {
    target: Target,
    n: usize,
    chosen: Option<Chosen>,
    /// GETs answered `NotFound` so far (the reader re-asks a lost block on its backoff, so this keeps growing).
    pub not_found: u64,
}

impl Lose {
    pub fn new(target: Target, n: usize) -> Lose {
        Lose { target, n, chosen: None, not_found: 0 }
    }

    pub fn chosen(&self) -> Option<&Chosen> {
        self.chosen.as_ref()
    }

    /// A BLOCK's GET answer (`state` = kind ‖ body, block `id`). Lost -> `NotFound`. Otherwise relayed -- and, for
    /// [`Target::Data`] with no group yet, a tree node with a group of `k >= 2` becomes THE group (its members are
    /// fetched after it, so none of them has been relayed yet).
    pub fn block(&mut self, id: Cid, state: &[u8]) -> Verdict {
        if self.chosen.as_ref().is_some_and(|c| c.lost.contains(&id)) {
            self.not_found += 1;
            return Verdict::NotFound(id);
        }
        if self.target == Target::Data && self.chosen.is_none() {
            if let Some((&kind::TREE_NODE, body)) = state.split_first() {
                if let Ok(node) = Node::parse(body) {
                    let parity: Vec<Cid> = node.parity().collect();
                    let group = group_members(&node).into_iter().enumerate().find(|(_, (_, m))| m.len() >= 2);
                    if let Some((g, (_, members))) = group {
                        if let Some(par) = parity.get(PARITY * g..PARITY * (g + 1)) {
                            self.choose(members, par);
                        }
                    }
                }
            }
        }
        Verdict::Relay
    }

    /// A NON-block GET answer: the head's Register record. For [`Target::Root`] with no group yet, the root's group of
    /// one -- the root and the parity its ledger lists (sdk#335).
    pub fn head(&mut self, state: &[u8]) {
        if self.target != Target::Root || self.chosen.is_some() {
            return;
        }
        let Some((_, value)) = signer_proto::head::record_of(state) else { return };
        let Some(head) = signer_proto::head::read_value(value) else { return };
        let Some(par) = head.ledger.parity.as_ref() else { return };
        let par: Vec<Cid> = par.chunks_exact(32).map(|c| c.try_into().expect("32 bytes")).collect();
        if par.is_empty() {
            return;
        }
        self.choose(vec![head.root], &par);
    }

    /// The first `n` slots are lost: DATA members first, then parity.
    fn choose(&mut self, members: Vec<Cid>, parity: &[Cid]) {
        let k = members.len();
        let mut slots = members;
        slots.extend_from_slice(parity);
        let lost = slots.iter().take(self.n).copied().collect();
        self.chosen = Some(Chosen { slots, k, lost });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_prolly::apply::{apply_into, Edit};
    use freenet_prolly::store::{Blocks, MemBlocks};

    /// A real tree big enough for its root to be a branch: its root id, its blocks, and its parity blocks.
    fn tree() -> (Cid, MemBlocks, Vec<(Cid, Vec<u8>)>) {
        let mut b = MemBlocks::default();
        let empty = freenet_prolly::build::init(&mut b);
        let edits: Vec<(Vec<u8>, Edit)> = (0..3000u32).map(|i| (format!("row/{i:06}").into_bytes(), Edit::Put(vec![7u8; 40]))).collect();
        let applied = apply_into(&mut b, &empty, &edits).expect("the tree");
        (applied.root, b, applied.parity)
    }

    fn state(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut s = vec![kind];
        s.extend_from_slice(body);
        s
    }

    /// DATA: the root's group of children is chosen when the root is relayed; the first `m` slots -- all data members
    /// here -- are lost and answered NotFound, every other block relayed. `m + 1` loses one more.
    #[test]
    fn a_data_group_loses_its_first_n_slots_data_members_first() {
        let (root, b, _) = tree();
        let root_bytes = b.get(&root).expect("the root").to_vec();
        let node = Node::parse(&root_bytes).expect("a node");
        assert!(!node.is_leaf(), "THE SETUP: the root is a leaf, so no group of children");
        for n in [PARITY, PARITY + 1] {
            let mut l = Lose::new(Target::Data, n);
            assert_eq!(l.block(root, &state(kind::TREE_NODE, &root_bytes)), Verdict::Relay, "the parent itself is relayed");
            let c = l.chosen().expect("a group of k >= 2 was chosen").clone();
            assert!(c.k >= 2, "k = {}", c.k);
            assert_eq!(c.slots.len(), c.k + PARITY);
            assert_eq!(c.lost.len(), n);
            let data_lost = c.slots[..c.k].iter().filter(|s| c.lost.contains(*s)).count();
            assert_eq!(data_lost, n.min(c.k), "data members are not lost first");
            for i in 0..node.len() {
                let child = node.child(i).0;
                let v = l.block(child, &state(kind::TREE_NODE, b.get(&child).expect("a child")));
                assert_eq!(v == Verdict::NotFound(child), c.lost.contains(&child), "child {i}");
            }
            assert_eq!(l.not_found as usize, data_lost);
        }
    }

    /// A leaf (no referenced values: no group) and a value block are never chosen as the data group.
    #[test]
    fn nothing_without_a_group_of_two_is_chosen() {
        let mut l = Lose::new(Target::Data, PARITY);
        let leaf = freenet_prolly::chunk::empty_leaf();
        assert_eq!(l.block(leaf.cid, &state(kind::TREE_NODE, &leaf.bytes)), Verdict::Relay);
        assert_eq!(l.block([9; 32], &state(kind::RAW, b"a value")), Verdict::Relay);
        assert!(l.chosen().is_none());
    }

    /// ROOT: the head's ledger names the root's parity; the root and the first parity blocks are lost.
    #[test]
    fn the_root_group_comes_from_the_head() {
        let root: Cid = [1; 32];
        let par: Vec<Cid> = (0..PARITY as u8).map(|i| [10 + i; 32]).collect();
        let ledger = signer_proto::head::Ledger { parity: Some(par.concat()), ..Default::default() };
        let value = signer_proto::head::value(&root, &ledger);
        // A real Register state, signed as the page's head is (contract-keys' one writer).
        let key = [7u8; 32];
        let vk = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key().to_bytes();
        let params = [b"RG01".as_slice(), &[0u8], &vk, b"head"].concat();
        let record = contract_keys::register::head_state(&params, &key, 7, &value).expect("a head state");
        let mut l = Lose::new(Target::Root, PARITY);
        l.head(&record);
        let c = l.chosen().expect("the root group").clone();
        assert_eq!((c.k, c.slots[0]), (1, root));
        assert!(c.lost.contains(&root) && c.lost.len() == PARITY);
        assert_eq!(l.block(root, b"\x01anything"), Verdict::NotFound(root));
        assert_eq!(l.block(par[PARITY - 1], b"\x03p"), Verdict::Relay, "the one parity left is relayed");
    }

    #[test]
    fn the_lose_counts_are_named_from_parity() {
        assert_eq!(parse_lose("m").unwrap(), PARITY);
        assert_eq!(parse_lose("m+1").unwrap(), PARITY + 1);
        assert!(parse_lose("8").is_err(), "a number would be a copy of m");
    }
}
