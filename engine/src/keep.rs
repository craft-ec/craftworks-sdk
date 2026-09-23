//! THE KEEPER'S PASS (Phase 4, DURABILITY-STATE gap 5; design:
//! craftworks-docs `docs/design/KEEPER.md`).
//!
//! A pass is a WALK from a kept head's root -- the one read path -- that asks
//! for EVERY block the root reaches: each tree node, each referenced value,
//! and each PARITY id a node lists (a read never asks for parity; a keeper
//! must). Every ask is also demand: the node that answers it hosts the block.
//!
//! Sans-IO, like the engine: the pass says what to fetch and what to put
//! ([`KeepEffect`]) and is told what arrived. It never reaches the network;
//! the page's one sender carries every effect, with its re-send until
//! answered.
//!
//! Per sibling group (k members + 3 parity), once all `k + 3` have ANSWERED:
//! * all there: nothing to do;
//! * some said ABSENT, ≥ k there: each absent member is rebuilt with
//!   [`crate::repair::rebuild`] (hash-checked against its id), each absent
//!   parity block recomputed from the members with `parity::encode_group`
//!   (hash-checked against its id), and each is PUT until acked;
//! * fewer than k: the group is named DAMAGED by its first parity id, and
//!   nothing is guessed.
//!
//! A block that arrives but does not hash to its id is REFUSED and counted as
//! absent. A block nobody answers stays pending: the pass never waits on one
//! block, and the report says how many are still out.
//!
//! Nothing is remembered across passes: the pass is a function of the tree
//! and of what the node answers, so running it twice lands on the same result.

use crate::repair::{self, Group};
use freenet_prolly::node::{Node, Value};
use freenet_prolly::{block_id, kind, parity, Cid};
use std::collections::{BTreeMap, BTreeSet};

/// What the pass needs done. The page turns each into its one sender's op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeepEffect {
    /// GET this block.
    Fetch(Cid),
    /// PUT this block (`kind ‖ body` is `kind` then `bytes`), until acked.
    Put { id: Cid, kind: u8, bytes: Vec<u8> },
}

/// What a pass found and did. A return value, not a log line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Distinct blocks the root reaches (nodes, referenced values, parity).
    pub blocks: usize,
    /// Answered present and hashing to their id.
    pub held: usize,
    /// Asked and not yet answered.
    pub pending: usize,
    /// Members rebuilt from their group and put.
    pub repaired: usize,
    /// Parity blocks recomputed from their group and put.
    pub parity_reput: usize,
    /// Puts not yet acked.
    pub puts_pending: usize,
    /// Groups with fewer than k of their blocks: named by first parity id.
    pub damaged: Vec<Cid>,
    /// Arrivals that did not hash to their id.
    pub refused: usize,
    /// The root itself was answered absent: it is in no group, so nothing
    /// here can rebuild it.
    pub root_missing: bool,
    /// Bytes of every block answered present (the keep-set's size, for the
    /// capacity report).
    pub bytes_kept: u64,
}

impl Report {
    /// Nothing asked is still out and nothing put is still unacked.
    pub fn done(&self) -> bool {
        self.pending == 0 && self.puts_pending == 0
    }
}

/// What an asked block turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Got {
    Asked,
    /// Here, and its bytes kept while a group it is in is still unsettled.
    Present(Vec<u8>),
    /// Here, and its bytes let go: every group it is in has been judged.
    Held,
    Absent,
}

/// One group being judged.
#[derive(Clone, Debug)]
struct Watch {
    /// Slots `0..k` members, `k..k+3` parity, in the code's column order.
    group: Group,
    settled: bool,
}

/// One pass over one root.
pub struct Pass {
    root: Cid,
    /// Every block asked, and what it is.
    got: BTreeMap<Cid, Got>,
    /// The kind each asked block is stored under (for the hash check).
    kind_of: BTreeMap<Cid, u8>,
    /// Blocks that are tree nodes: parsed and descended when present.
    nodes: BTreeSet<Cid>,
    /// Groups not yet settled, and which groups each block is a slot of.
    groups: Vec<Watch>,
    in_group: BTreeMap<Cid, Vec<usize>>,
    /// Puts not yet acked.
    putting: BTreeSet<Cid>,
    /// Blocks already counted in `held` / `bytes_kept` (a re-ask is not a
    /// second block).
    counted: BTreeSet<Cid>,
    out: Vec<KeepEffect>,
    report: Report,
}

impl Pass {
    /// A pass over the tree under `root`. Its first effect asks for the root.
    pub fn new(root: Cid) -> Pass {
        let mut p = Pass {
            root,
            got: BTreeMap::new(),
            kind_of: BTreeMap::new(),
            nodes: BTreeSet::new(),
            groups: Vec::new(),
            in_group: BTreeMap::new(),
            putting: BTreeSet::new(),
            counted: BTreeSet::new(),
            out: Vec::new(),
            report: Report::default(),
        };
        p.ask(root, kind::TREE_NODE, true);
        p
    }

    /// The effects produced since the last call.
    pub fn take(&mut self) -> Vec<KeepEffect> {
        std::mem::take(&mut self.out)
    }

    /// Is this pass waiting on `id` (a GET answer, or a PUT ack)?
    pub fn wants(&self, id: &Cid) -> bool {
        matches!(self.got.get(id), Some(Got::Asked)) || self.putting.contains(id)
    }

    /// The report as it stands.
    pub fn report(&self) -> Report {
        let mut r = self.report.clone();
        r.pending = self.got.values().filter(|g| **g == Got::Asked).count();
        r.puts_pending = self.putting.len();
        r
    }

    /// `id` arrived with `bytes` (the body, without its kind byte).
    pub fn on_got(&mut self, id: Cid, bytes: &[u8]) {
        if !matches!(self.got.get(&id), Some(Got::Asked)) {
            return;
        }
        let kind = self.kind_of[&id];
        if block_id(kind, bytes) != id {
            self.report.refused += 1;
            self.absent(id);
            return;
        }
        self.present(id, bytes.to_vec());
    }

    /// The node answered that `id` is not there.
    pub fn on_missed(&mut self, id: Cid) {
        if matches!(self.got.get(&id), Some(Got::Asked)) {
            self.absent(id);
        }
    }

    /// The node acknowledged the put of `id`.
    pub fn on_put_ok(&mut self, id: Cid) {
        self.putting.remove(&id);
    }

    fn ask(&mut self, id: Cid, kind: u8, node: bool) {
        if node {
            self.nodes.insert(id);
        }
        if self.got.contains_key(&id) {
            return;
        }
        self.got.insert(id, Got::Asked);
        self.kind_of.insert(id, kind);
        self.report.blocks += 1;
        self.out.push(KeepEffect::Fetch(id));
    }

    fn present(&mut self, id: Cid, bytes: Vec<u8>) {
        if self.counted.insert(id) {
            self.report.held += 1;
            self.report.bytes_kept += bytes.len() as u64 + 1;
        }
        if self.nodes.contains(&id) {
            self.descend(&bytes);
        }
        self.got.insert(id, Got::Present(bytes));
        self.settle_groups_of(id);
        self.release(id);
    }

    /// Let go of `id`'s bytes once no unsettled group needs them. A pass holds
    /// what its groups in flight need, not the whole tree.
    fn release(&mut self, id: Cid) {
        let unsettled = self.in_group.get(&id).is_some_and(|gs| gs.iter().any(|&g| !self.groups[g].settled));
        if !unsettled && matches!(self.got.get(&id), Some(Got::Present(_))) {
            self.got.insert(id, Got::Held);
        }
    }

    fn absent(&mut self, id: Cid) {
        self.got.insert(id, Got::Absent);
        if id == self.root {
            self.report.root_missing = true;
        }
        self.settle_groups_of(id);
    }

    /// A node is here: ask for everything it names, and watch its groups.
    fn descend(&mut self, bytes: &[u8]) {
        let Ok(node) = Node::parse(bytes) else { return };
        let leaf = node.is_leaf();
        if leaf {
            for i in 0..node.len() {
                if let Value::Ref { cid, .. } = node.value(i) {
                    self.ask(cid, kind::RAW, false);
                }
            }
        } else {
            for i in 0..node.len() {
                self.ask(node.child(i).0, kind::TREE_NODE, true);
            }
        }
        let ids: Vec<Cid> = node.parity().collect();
        for (g, (_, members)) in parity::group_members(&node).into_iter().enumerate() {
            let Some(par) = ids.get(3 * g..3 * g + 3) else { continue };
            for p in par {
                self.ask(*p, kind::PARITY, false);
            }
            let k = members.len();
            let mut slots = members;
            slots.extend_from_slice(par);
            let group = Group {
                missing: slots[0],
                missing_ix: 0,
                kind: if leaf { kind::RAW } else { kind::TREE_NODE },
                slots,
                k,
                max_len: if leaf { parity::MAX_MEMBER_VALUE } else { parity::MAX_MEMBER_NODE },
            };
            let ix = self.groups.len();
            for s in &group.slots {
                self.in_group.entry(*s).or_default().push(ix);
                // The same block in an earlier, settled group (identical
                // content) had its bytes let go: ask again, so this group
                // can be judged on bytes and not on a memory of them.
                if self.got.get(s) == Some(&Got::Held) {
                    self.got.insert(*s, Got::Asked);
                    self.out.push(KeepEffect::Fetch(*s));
                }
            }
            self.groups.push(Watch { group, settled: false });
            self.settle(ix);
        }
    }

    fn settle_groups_of(&mut self, id: Cid) {
        for ix in self.in_group.get(&id).cloned().unwrap_or_default() {
            self.settle(ix);
        }
    }

    /// Judge group `ix` once every slot has answered.
    fn settle(&mut self, ix: usize) {
        if self.groups[ix].settled {
            return;
        }
        let group = self.groups[ix].group.clone();
        let answered = group.slots.iter().all(|s| matches!(self.got.get(s), Some(Got::Present(_) | Got::Absent)));
        // `Held` cannot occur here: bytes are let go only once every group
        // the block is in has settled, and this one has not.
        if !answered {
            return;
        }
        self.groups[ix].settled = true;
        let absent: Vec<usize> = (0..group.slots.len()).filter(|&i| self.got.get(&group.slots[i]) == Some(&Got::Absent)).collect();
        if absent.is_empty() {
            self.release_group(ix);
            return;
        }
        let have: Vec<Option<Vec<u8>>> = (0..group.slots.len())
            .map(|i| match self.got.get(&group.slots[i]) {
                Some(Got::Present(b)) => Some(group.stored(i, b)),
                _ => None,
            })
            .collect();
        if have.iter().filter(|h| h.is_some()).count() < group.k {
            self.report.damaged.push(group.slots[group.k]);
            self.release_group(ix);
            return;
        }
        // Every member, rebuilt where absent: the parity is a function of all
        // of them.
        let mut members: Vec<Vec<u8>> = Vec::with_capacity(group.k);
        for i in 0..group.k {
            match self.got.get(&group.slots[i]) {
                Some(Got::Present(b)) => members.push(b.clone()),
                _ => {
                    let g = Group { missing: group.slots[i], missing_ix: i, ..group.clone() };
                    match repair::rebuild(&g, &have) {
                        Ok(body) => {
                            self.report.repaired += 1;
                            self.put(group.slots[i], group.kind, body.clone());
                            members.push(body);
                        }
                        Err(_) => {
                            self.report.damaged.push(group.slots[group.k]);
                            self.release_group(ix);
                            return;
                        }
                    }
                }
            }
        }
        if absent.iter().any(|&i| group.is_parity(i)) {
            let states: Vec<Vec<u8>> = members
                .iter()
                .map(|b| {
                    let mut st = Vec::with_capacity(1 + b.len());
                    st.push(group.kind);
                    st.extend_from_slice(b);
                    st
                })
                .collect();
            let Ok(par) = parity::encode_group(&states) else {
                self.report.damaged.push(group.slots[group.k]);
                self.release_group(ix);
                return;
            };
            for &i in absent.iter().filter(|&&i| group.is_parity(i)) {
                let p = &par[i - group.k];
                if block_id(kind::PARITY, p) != group.slots[i] {
                    // The node lists parity these members do not produce:
                    // nothing true can be put under that id.
                    self.report.damaged.push(group.slots[group.k]);
                    continue;
                }
                self.report.parity_reput += 1;
                self.put(group.slots[i], kind::PARITY, p.clone());
            }
        }
        // A rebuilt tree node is also a node to descend.
        for &i in absent.iter().filter(|&&i| !group.is_parity(i)) {
            let id = group.slots[i];
            if self.nodes.contains(&id) {
                if let Some(body) = members.get(i).cloned() {
                    self.descend(&body);
                }
            }
        }
        self.release_group(ix);
    }

    fn release_group(&mut self, ix: usize) {
        for s in self.groups[ix].group.slots.clone() {
            self.release(s);
        }
    }

    fn put(&mut self, id: Cid, kind: u8, bytes: Vec<u8>) {
        if self.putting.insert(id) {
            self.out.push(KeepEffect::Put { id, kind, bytes });
        }
    }
}
