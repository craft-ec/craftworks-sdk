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
use freenet_stdlib::client_api::{ClientError, ContractResponse, HostResponse};
use std::collections::BTreeSet;
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

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
    /// [`Target::Data`] BY IDENTITY (7c's realnet: "the first group of k >= 2" is the tree's SHAPE, and chose the root's
    /// children, k = 2, instead of the bulk rows): only a leaf's group of referenced values whose EVERY member's key
    /// starts with the domain's record prefix -- from the SDK's ONE record-key encoder (`craftworks_sdk::db::record_prefix`,
    /// `T_RECORD | domain | 0`), never a form written here.
    domain: Option<Vec<u8>>,
    chosen: Option<Chosen>,
    /// GETs answered `NotFound` so far (the reader re-asks a lost block on its backoff, so this keeps growing).
    pub not_found: u64,
    /// The chosen group's slots the reader has ASKED for since the choice: answered with bytes or with `NotFound`. The
    /// `m + 1` control is void unless the reader asked `>= k` of them (the architect on builder#179: a reader that never
    /// reached for the group's parity has not been refused a decode).
    pub asked: BTreeSet<Cid>,
}

impl Lose {
    pub fn new(target: Target, n: usize) -> Lose {
        Lose { target, n, domain: None, chosen: None, not_found: 0, asked: BTreeSet::new() }
    }

    /// [`Target::Data`], choosing the group of `domain`'s records by identity (see the field).
    pub fn with_domain(mut self, domain: &str) -> Lose {
        self.domain = Some(craftworks_sdk::db::record_prefix(domain));
        self
    }

    pub fn chosen(&self) -> Option<&Chosen> {
        self.chosen.as_ref()
    }

    /// A BLOCK's GET answer (`state` = kind ‖ body, block `id`). Lost -> `NotFound`. Otherwise relayed -- and, for
    /// [`Target::Data`] with no group yet, a tree node with a group of `k >= 2` becomes THE group (its members are
    /// fetched after it, so none of them has been relayed yet).
    pub fn block(&mut self, id: Cid, state: &[u8]) -> Verdict {
        if self.chosen.as_ref().is_some_and(|c| c.slots.contains(&id)) {
            self.asked.insert(id);
        }
        if self.chosen.as_ref().is_some_and(|c| c.lost.contains(&id)) {
            self.not_found += 1;
            return Verdict::NotFound(id);
        }
        if self.target == Target::Data && self.chosen.is_none() {
            if let Some((&kind::TREE_NODE, body)) = state.split_first() {
                if let Ok(node) = Node::parse(body) {
                    let parity: Vec<Cid> = node.parity().collect();
                    // Each referenced value's key (a leaf's): what `--domain` matches a group's members by.
                    let key_of: std::collections::BTreeMap<Cid, Vec<u8>> = (0..if node.is_leaf() { node.len() } else { 0 })
                        .filter_map(|i| match node.value(i) {
                            freenet_prolly::node::Value::Ref { cid, .. } => Some((cid, node.key(i))),
                            freenet_prolly::node::Value::Inline(_) => None,
                        })
                        .collect();
                    let domain = self.domain.clone();
                    let fits = |m: &Vec<Cid>| match &domain {
                        None => m.len() >= 2,
                        Some(marker) => {
                            node.is_leaf() && !m.is_empty() && m.iter().all(|c| key_of.get(c).is_some_and(|k| k.starts_with(marker)))
                        }
                    };
                    let group = group_members(&node).into_iter().enumerate().find(|(_, (_, m))| fits(m));
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

/// A block id as the log names it: its first 8 bytes in hex (core_types::hex, the one owner).
pub fn short(id: &Cid) -> String {
    core_types::hex::encode(&id[..8])
}

/// THE LOG LINE for a group chosen (`group`: `"data"` or `"root"`). LOAD-BEARING: the realnet step's VOID check reads
/// `lost` against the `not_found` lines ([`not_found_line`]); the shape is pinned by a test.
pub fn chosen_line(group: &str, c: &Chosen) -> serde_json::Value {
    // `lost_data`: the lost DATA members (the first `k` slots): a read of them is a decode (the realnet step's
    // non-vacuity check reads these).
    let lost_data: Vec<String> = c.slots[..c.k].iter().filter(|s| c.lost.contains(*s)).map(short).collect();
    serde_json::json!({ "chosen": group, "k": c.k, "slots": c.slots.len(), "lost": c.lost.iter().map(short).collect::<Vec<_>>(), "lost_data": lost_data })
}

/// THE LOG LINE for one GET answered NotFound. LOAD-BEARING (see [`chosen_line`]).
pub fn not_found_line(id: &Cid, total: u64) -> serde_json::Value {
    serde_json::json!({ "not_found": short(id), "not_found_total": total })
}

/// THE LOG LINE for a chosen slot the reader asked for the first time (bytes or NotFound). LOAD-BEARING: the `m + 1`
/// control reads `asked_total >= k` (see [`chosen_line`]).
pub fn asked_line(id: &Cid, total: usize) -> serde_json::Value {
    serde_json::json!({ "asked": short(id), "asked_total": total })
}

/// The answer this proxy sends in place of `m`: the node's own `NotFound` for a lost block, or `m` itself.
pub fn answer(lose: &Mutex<Lose>, m: Message) -> Message {
    let Message::Binary(b) = &m else { return m };
    let Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key, state, .. }))) =
        bincode::deserialize::<Result<HostResponse, ClientError>>(b)
    else {
        return m;
    };
    let mut lose = lose.lock().unwrap();
    // A BLOCK is a state whose (kind, body) hashes to the id its key names under the key's own contract code.
    let block = wire::block::block_of_state(state.as_ref())
        .filter(|(cid, _)| {
            let code: Option<[u8; 32]> = key.code_hash().as_ref().try_into().ok();
            code.is_some_and(|code| contract_keys::instance(&code, cid).as_slice() == key.id().as_bytes())
        });
    let Some((cid, _)) = block else {
        let had = lose.chosen().is_some();
        lose.head(state.as_ref());
        if !had {
            if let Some(c) = lose.chosen() {
                eprintln!("{}", chosen_line("root", c));
            }
        }
        return m;
    };
    let had = lose.chosen().is_some();
    let asked = lose.asked.len();
    let verdict = lose.block(cid, state.as_ref());
    if !had {
        if let Some(c) = lose.chosen() {
            eprintln!("{}", chosen_line("data", c));
        }
    }
    if lose.asked.len() > asked {
        eprintln!("{}", asked_line(&cid, lose.asked.len()));
    }
    match verdict {
        Verdict::Relay => m,
        Verdict::NotFound(id) => {
            eprintln!("{}", not_found_line(&id, lose.not_found));
            let nf: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(ContractResponse::NotFound { instance_id: *key.id() }));
            Message::Binary(bincode::serialize(&nf).expect("a NotFound encodes").into())
        }
    }
}

/// ws-lose's hooks on the one proxy ([`crate::proxy`]): every answer from the node through [`answer`].
pub struct LoseHooks(pub Mutex<Lose>);

impl crate::proxy::Hooks for LoseHooks {
    fn down(&self, _conn: u64, m: Message) -> Option<Message> {
        Some(answer(&self.0, m))
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
        let (root, b, parity) = tree();
        let parity: std::collections::HashMap<Cid, Vec<u8>> = parity.into_iter().collect();
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
            // ASKED: every member fetched is a slot asked (bytes or NotFound); a parity slot fetched is one more; a
            // re-ask is not.
            assert_eq!(l.asked.len(), c.k, "the reader fetched the k members");
            let p0 = c.slots[c.k];
            let _ = l.block(p0, &state(kind::PARITY, parity.get(&p0).expect("the parity block")));
            let _ = l.block(p0, &state(kind::PARITY, parity.get(&p0).expect("the parity block")));
            assert_eq!(l.asked.len(), c.k + 1, "a parity slot asked (twice) counts once");
            // THE GROUP IS THE TREE'S OWN (the architect on #525): its parity slots are parity this tree's apply made,
            // and the engine's own decoder rebuilds every lost member from the slots left when m are lost -- and has
            // fewer than k to work with when m + 1 are.
            assert!(c.slots[c.k..].iter().all(|p| parity.contains_key(p)), "a chosen parity slot is not this tree's parity");
            let group = |ix: usize| engine::repair::Group {
                missing: c.slots[ix],
                missing_ix: ix,
                kind: kind::TREE_NODE,
                slots: c.slots.clone(),
                k: c.k,
                max_len: freenet_prolly::parity::MAX_MEMBER_NODE,
            };
            let probe = group(0);
            let have: Vec<Option<Vec<u8>>> = c
                .slots
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let raw = if i < c.k { b.get(id).map(<[u8]>::to_vec) } else { parity.get(id).cloned() };
                    (!c.lost.contains(id)).then(|| probe.stored(i, &raw.expect("every slot's bytes are held")))
                })
                .collect();
            let left = have.iter().filter(|h| h.is_some()).count();
            if n == PARITY {
                assert!(left >= c.k, "m lost left {left} of k = {}", c.k);
                for ix in (0..c.k).filter(|ix| c.lost.contains(&c.slots[*ix])) {
                    let rebuilt = engine::repair::rebuild(&group(ix), &have).expect("m lost: the lost member decodes");
                    assert_eq!(Some(rebuilt.as_slice()), b.get(&c.slots[ix]), "member {ix} rebuilt to other bytes");
                }
            } else {
                assert!(left < c.k, "m + 1 lost still left {left} of k = {}: the control would pass by luck", c.k);
                assert!(engine::repair::rebuild(&group(0), &have).is_err(), "m + 1 lost, and a member still decoded");
            }
        }
    }

    /// `--domain` chooses THE domain's group by IDENTITY (7c's realnet): a tree whose first group of `k >= 2` is not the
    /// bulk rows' (small rows of another domain make the root a branch of leaves), and three referenced bulk values in
    /// one leaf. Without `--domain` the first `k >= 2` group is chosen -- not theirs; with it, exactly the bulk values'.
    #[test]
    fn a_domain_chooses_its_records_group_whatever_the_trees_shape() {
        let mut b = MemBlocks::default();
        let empty = freenet_prolly::build::init(&mut b);
        // Keys from the SDK's ONE record-key encoder: what a page's Db writes.
        let key = |domain: &str, i: u32| {
            let mut rkey = [0u8; 16];
            rkey[12..].copy_from_slice(&i.to_be_bytes());
            craftworks_sdk::db::record_key(domain, rkey)
        };
        let mut edits: Vec<(Vec<u8>, Edit)> = (0..3000u32).map(|i| (key("notes", i), Edit::Put(vec![7u8; 40]))).collect();
        edits.extend((0..3u32).map(|i| (key("bulk", i), Edit::Put(vec![b'x' + i as u8; 1400]))));
        edits.sort_by(|a, b| a.0.cmp(&b.0));
        let applied = apply_into(&mut b, &empty, &edits).expect("the tree");
        // Feed the reader's walk: every node, parents first.
        let walk = |mut l: Lose| -> Option<Chosen> {
            let mut todo = vec![applied.root];
            while let Some(id) = todo.pop() {
                let bytes = b.get(&id).expect("held").to_vec();
                let _ = l.block(id, &state(kind::TREE_NODE, &bytes));
                if l.chosen().is_some() {
                    return l.chosen().cloned();
                }
                let n = Node::parse(&bytes).expect("a node");
                if !n.is_leaf() {
                    todo.extend((0..n.len()).rev().map(|i| n.child(i).0));
                }
            }
            None
        };
        let shape = walk(Lose::new(Target::Data, PARITY)).expect("THE SETUP: no group of k >= 2 at all");
        let bulk = walk(Lose::new(Target::Data, PARITY).with_domain("bulk")).expect("--domain bulk chose no group");
        assert_eq!(bulk.k, 3, "--domain bulk chose a group of k = {}, not the three bulk values", bulk.k);
        assert_ne!(shape.slots, bulk.slots, "THE SETUP: the first k >= 2 group IS the bulk one, so this proves nothing");
        let bulk_values: BTreeSet<Cid> = (0..3u32).map(|i| freenet_prolly::block_id(kind::RAW, &vec![b'x' + i as u8; 1400])).collect();
        assert_eq!(bulk.slots[..bulk.k].iter().copied().collect::<BTreeSet<_>>(), bulk_values, "--domain chose other members");
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
        let params = wire::register_params(&vk, wire::HEAD_NAME);
        let record = contract_keys::register::head_state(&params, &key, 7, &value).expect("a head state");
        let mut l = Lose::new(Target::Root, PARITY);
        l.head(&record);
        let c = l.chosen().expect("the root group").clone();
        assert_eq!((c.k, c.slots[0]), (1, root));
        assert!(c.lost.contains(&root) && c.lost.len() == PARITY);
        assert_eq!(l.block(root, b"\x01anything"), Verdict::NotFound(root));
        assert_eq!(l.block(par[PARITY - 1], b"\x03p"), Verdict::Relay, "the one parity left is relayed");
    }

    /// THE LOG LINES' SHAPE, which the realnet step reads (builder#179's VOID check: the distinct `not_found` ids equal
    /// the chosen `lost`): renamed keys or a different id form would make every arm VOID, silently.
    #[test]
    fn the_log_lines_keep_their_shape() {
        let c = Chosen { slots: vec![[1; 32], [2; 32], [3; 32]], k: 2, lost: [[1; 32], [3; 32]].into_iter().collect() };
        let chosen = chosen_line("data", &c);
        assert_eq!(chosen, serde_json::json!({ "chosen": "data", "k": 2, "slots": 3, "lost": ["0101010101010101", "0303030303030303"], "lost_data": ["0101010101010101"] }));
        let nf = not_found_line(&[3; 32], 7);
        assert_eq!(nf, serde_json::json!({ "not_found": "0303030303030303", "not_found_total": 7 }));
        // The ids the two lines name for one block are the SAME string.
        assert!(chosen["lost"].as_array().unwrap().contains(&nf["not_found"]));
        let asked = asked_line(&[2; 32], 2);
        assert_eq!(asked, serde_json::json!({ "asked": "0202020202020202", "asked_total": 2 }));
    }

    #[test]
    fn the_lose_counts_are_named_from_parity() {
        assert_eq!(parse_lose("m").unwrap(), PARITY);
        assert_eq!(parse_lose("m+1").unwrap(), PARITY + 1);
        assert!(parse_lose("8").is_err(), "a number would be a copy of m");
    }
}
