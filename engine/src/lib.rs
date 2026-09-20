//! The write pipeline as a pure state machine.
//!
//! `step(event) -> Vec<Effect>`: no sockets, no clock, no randomness. Every
//! interleaving a live node can produce — responses out of order, a PUT that
//! never acknowledges, a duplicate confirmation, a restart mid-commit — is an
//! ordinary deterministic test here instead of a flake against a real network.
//!
//! **Effects carry dependencies, never a batch size.** How many operations fit
//! in one delegate round is a platform fact that has already changed once
//! (F15, F21) and is the shell's business. The core says what must happen
//! before what; the shell decides how many at a time.
//!
//! The states a write moves through, and what each one promises:
//!
//! | state | promise |
//! |---|---|
//! | `Accepted` | in the engine's memory. Survives a tab close, NOT a node restart. |
//! | `Durable` | its pack read back from our own node. Survives a restart. What a UI may call "saved". |
//! | `Published` | the head read back. Other readers can find it. |
//! | `ParityComplete` | the redundancy the new nodes promise actually exists. |
//!
//! Between `Published` and `ParityComplete` the tree is correct and its groups
//! have no redundancy. That is *absent redundancy, not an error*
//! (ARCHITECTURE §7): a keeper's repair and the writer's late put are the same
//! bytes under the same id, so nothing has to be flagged, only done.

use freenet_prolly::apply::{apply_with, Edit as TreeEdit, Options as ApplyOptions};
use freenet_prolly::build::init;
use freenet_prolly::node::Node;
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

pub mod pack;

/// Which client a write came from. Two tabs are two clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u64);

/// The client's own id for a write. Echoed in every state change, so a caller
/// never has to guess which of its writes a notification is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriteId(pub u64);

/// What a client asked for. Mirrors the tree's own edit vocabulary; the engine
/// never looks inside a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Put(Vec<u8>),
    Delete,
}

/// How far along a write is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Accepted,
    Durable,
    Published,
    ParityComplete,
    Failed,
    /// Refused without being accepted: the backlog is full.
    Busy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Write {
        client: ClientId,
        write_id: WriteId,
        ops: Vec<(Vec<u8>, Op)>,
    },
    /// A block was READ BACK from our own node. Never an ack: W1 says an
    /// acknowledgement is not evidence the block is there.
    PutConfirmed(Cid),
    PutFailed(Cid),
    HeadConfirmed(u64),
    Tick(u64),
}

/// A group's three parity ids. The unit redundancy comes in: three blocks are
/// one group's protection and are worth nothing separately.
pub type ParityIds = [Cid; 3];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    PutPack {
        id: Cid,
        bytes: Vec<u8>,
        after: Vec<Cid>,
    },
    /// A value too large to ride in a pack.
    PutBlock {
        id: Cid,
        bytes: Vec<u8>,
        after: Vec<Cid>,
    },
    UpdateHead {
        seq: u64,
        root: Cid,
        after: Vec<Cid>,
    },
    PutParity {
        group: ParityIds,
        id: Cid,
        bytes: Vec<u8>,
        after: Vec<Cid>,
    },
    Notify {
        client: ClientId,
        write_id: WriteId,
        state: State,
    },
}

/// Everything tunable, in one place, so nothing downstream reads a literal.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Ceiling on a pack body. A parameter because the engine's default is
    /// policy and has already moved once; the FORMAT's limit is elsewhere.
    pub max_pack: usize,
    /// A value at or under this rides inside a pack; above it, its own PUT.
    pub max_packed_value: usize,
    /// Accepted-but-not-durable bytes beyond which a `Write` is refused. A
    /// queue with no bound is a queue that eventually eats the node.
    pub max_backlog: usize,
    /// Ticks after which an owed group's parity is put even while its members
    /// keep changing, so a hot group cannot stay unprotected for ever.
    pub parity_age: u64,
    /// Off = the negative control for coalescing.
    pub coalesce_parity: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            max_pack: 1024 * 1024,
            max_packed_value: 64 * 1024,
            max_backlog: 8 * 1024 * 1024,
            parity_age: 32,
            coalesce_parity: true,
        }
    }
}

/// One group's owed redundancy: the blocks to put, and when it was first owed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Owed {
    blocks: Vec<(Cid, Vec<u8>)>,
    /// The last tick at which this group was re-coded. A group that changed
    /// this tick is still moving, so putting its parity now buys redundancy
    /// for members that are about to be superseded.
    last_changed: u64,
    /// The tick this group was first owed, for the age bound. Re-coding a
    /// group does NOT reset it: what the bound protects is how long a group
    /// has gone unprotected, and a group rewritten every tick has gone
    /// unprotected the whole time.
    since: u64,
    /// Whether its puts have been emitted and are outstanding.
    sent: bool,
}

/// A commit in flight: one apply, one head bump.
#[derive(Clone, Debug)]
struct Commit {
    seq: u64,
    root: Cid,
    /// Pack and block ids this commit must land before its head may move.
    data: BTreeSet<Cid>,
    confirmed: BTreeSet<Cid>,
    /// The writes folded into it, in arrival order.
    writes: Vec<(ClientId, WriteId)>,
    head_sent: bool,
    /// Bytes of the accepted-but-not-durable writes, for the backlog bound.
    bytes: usize,
}

/// The write pipeline.
pub struct Engine {
    params: Params,
    /// The warm tree: every block this engine has written. In slice 1 it is
    /// also the only place they exist, because there is no node yet.
    blocks: MemBlocks,
    root: Cid,
    /// The last head the network has confirmed, and its root. Recovery starts
    /// from here, never from the warm tree.
    published_seq: u64,
    published_root: Cid,
    next_seq: u64,
    /// The commit in flight. One at a time: a second head bump before the
    /// first is confirmed is a fork of a single-writer tree.
    pending: Option<Commit>,
    /// Writes that arrived during a commit. They are already applied to the
    /// warm tree — folding is about which COMMIT carries them, not about
    /// whether they took effect.
    folded: Vec<(ClientId, WriteId)>,
    folded_bytes: usize,
    /// Every group whose parity is coded but not put. A MAP, newest wins: if a
    /// group is re-coded before its parity goes out, the old parity is
    /// superseded and must never be put.
    owed: BTreeMap<ParityIds, Owed>,
    /// Parity blocks emitted and awaiting confirmation, back to their group.
    in_flight_parity: BTreeMap<Cid, ParityIds>,
    /// Which commit's writes are waiting on which group, so ParityComplete is
    /// reported to the right writes.
    parity_owners: BTreeMap<ParityIds, Vec<(ClientId, WriteId)>>,
    now: u64,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new(Params::default())
    }
}

impl Engine {
    pub fn new(params: Params) -> Self {
        let mut blocks = MemBlocks::default();
        let root = init(&mut blocks);
        Engine {
            params,
            blocks,
            root,
            published_seq: 0,
            published_root: root,
            next_seq: 1,
            pending: None,
            folded: Vec::new(),
            folded_bytes: 0,
            owed: BTreeMap::new(),
            in_flight_parity: BTreeMap::new(),
            parity_owners: BTreeMap::new(),
            now: 0,
        }
    }

    /// The warm tree's current root, including writes not yet published.
    pub fn root(&self) -> Cid {
        self.root
    }

    /// The root other readers can see.
    pub fn published_root(&self) -> Cid {
        self.published_root
    }

    /// Groups whose redundancy does not exist yet.
    pub fn owed_groups(&self) -> usize {
        self.owed.len()
    }

    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Write {
                client,
                write_id,
                ops,
            } => self.on_write(client, write_id, ops),
            Event::PutConfirmed(id) => self.on_confirmed(id),
            Event::PutFailed(id) => self.on_failed(id),
            Event::HeadConfirmed(seq) => self.on_head(seq),
            Event::Tick(now) => self.on_tick(now),
        }
    }

    fn on_write(
        &mut self,
        client: ClientId,
        write_id: WriteId,
        ops: Vec<(Vec<u8>, Op)>,
    ) -> Vec<Effect> {
        let size: usize = ops
            .iter()
            .map(|(k, o)| k.len() + if let Op::Put(v) = o { v.len() } else { 0 })
            .sum();
        // Refused BEFORE it is applied. A write answered Busy must leave no
        // trace: the client will send it again, and a half-applied write that
        // was also refused is the worst of both.
        if self.backlog() + size > self.params.max_backlog {
            return vec![Effect::Notify {
                client,
                write_id,
                state: State::Busy,
            }];
        }

        // The tree takes a SET in key order; the client wrote a SEQUENCE. Two
        // ops on one key in a batch are the client changing its mind, so the
        // LAST wins — the SDK's rule, and the one a caller who wrote `put`
        // then `delete` expects. A sort-then-dedup keeps the FIRST of each
        // run instead, which is the same ops in the same order producing a
        // different tree; the differential against a from-scratch rebuild is
        // what caught it.
        let batch: Vec<(Vec<u8>, TreeEdit)> = ops
            .into_iter()
            .map(|(k, o)| {
                (
                    k,
                    match o {
                        Op::Put(v) => TreeEdit::Put(v),
                        Op::Delete => TreeEdit::Delete,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();

        let mut emitted: Vec<(Cid, Vec<u8>)> = Vec::new();
        let applied = match apply_with(
            ApplyOptions::default(),
            &self.blocks,
            &self.root,
            &batch,
            |c, b: &[u8]| emitted.push((c, b.to_vec())),
        ) {
            Ok(a) => a,
            // The SDK screens keys and values before a write reaches here, so
            // a refusal means something bypassed it. It is reported, not
            // hidden, and nothing is applied.
            Err(_) => {
                return vec![Effect::Notify {
                    client,
                    write_id,
                    state: State::Failed,
                }]
            }
        };
        for (c, b) in &emitted {
            self.blocks.insert(*c, b);
        }
        self.root = applied.root;
        self.record_owed(&applied.parity, &emitted);

        let mut out = vec![Effect::Notify {
            client,
            write_id,
            state: State::Accepted,
        }];
        self.folded.push((client, write_id));
        self.folded_bytes += size;
        if self.pending.is_none() {
            out.extend(self.start_commit(emitted));
        }
        out
    }

    fn backlog(&self) -> usize {
        self.folded_bytes + self.pending.as_ref().map_or(0, |c| c.bytes)
    }

    /// Record what a commit coded, newest wins.
    ///
    /// The map is keyed by the group's three parity ids, which ARE the group's
    /// identity: parity is a pure function of the members and the class, so
    /// two codings with the same ids are the same redundancy and one put
    /// serves both. When a group is re-coded its ids change, and the entry it
    /// replaces is found through the node that lists them.
    fn record_owed(&mut self, parity: &[(Cid, Vec<u8>)], emitted: &[(Cid, Vec<u8>)]) {
        // Which three ids belong together is stated by the NODE that lists
        // them — the same place a checker reads the grouping from, so the
        // engine cannot drift from the format.
        let mut by_group: BTreeMap<ParityIds, Vec<(Cid, Vec<u8>)>> = BTreeMap::new();
        let have: BTreeMap<&Cid, &Vec<u8>> = parity.iter().map(|(c, b)| (c, b)).collect();
        for (_, bytes) in emitted {
            let Ok(node) = Node::parse(bytes) else {
                continue;
            };
            let ids: Vec<Cid> = node.parity().collect();
            for trio in ids.chunks_exact(3) {
                let key: ParityIds = [trio[0], trio[1], trio[2]];
                // A group whose blocks this commit coded is owed. One whose
                // blocks it did not is either already out or owed from before.
                if trio.iter().all(|c| have.contains_key(c)) {
                    let blocks: Vec<(Cid, Vec<u8>)> =
                        trio.iter().map(|c| (*c, have[c].clone())).collect();
                    by_group.insert(key, blocks);
                }
            }
        }

        // A group still owed but no longer listed by ANY node in the warm tree
        // has been SUPERSEDED: its members moved on, and putting its parity
        // now would buy redundancy for bytes no reader will ever ask for.
        // That is the coalescing rule, and it is decided against the whole
        // tree rather than against one node, because a shifting boundary can
        // carry a group into a different node than it started in.
        // Reachable from the CURRENT root, not every block the store holds.
        // Superseded nodes are never deleted, so a scan of the whole store
        // finds every parity id ever listed and concludes nothing was ever
        // superseded -- coalescing then silently does nothing, which is
        // exactly what the control measured before this was fixed.
        let mut reachable: BTreeSet<Cid> = BTreeSet::new();
        collect(&self.blocks, &self.root, &mut reachable);
        let mut live: BTreeSet<Cid> = BTreeSet::new();
        for id in &reachable {
            if let Some(bytes) = self.blocks.get(id) {
                if let Ok(n) = Node::parse(bytes) {
                    live.extend(n.parity());
                }
            }
        }
        let now = self.now;
        self.owed.retain(|key, o| {
            // Never drop one already emitted: it is out there being confirmed,
            // and forgetting it would lose the ParityComplete it owes.
            o.sent || live.contains(&key[0])
        });

        for (key, blocks) in by_group {
            let e = self.owed.entry(key).or_insert(Owed {
                blocks: blocks.clone(),
                last_changed: now,
                since: now,
                sent: false,
            });
            e.blocks = blocks;
            e.last_changed = now;
        }
    }

    /// Plan the packs for what a commit emitted, and send them.
    fn start_commit(&mut self, emitted: Vec<(Cid, Vec<u8>)>) -> Vec<Effect> {
        let seq = self.next_seq;
        let writes = std::mem::take(&mut self.folded);
        let bytes = std::mem::take(&mut self.folded_bytes);

        // Big values do not ride in a pack: one PUT each, and the pack stays
        // within a size the network is willing to move.
        let (packable, direct): (Vec<_>, Vec<_>) = emitted
            .into_iter()
            .partition(|(_, b)| b.len() <= self.params.max_packed_value);

        let manifest = pack::Manifest {
            prev_seq: self.published_seq,
            prev_root: self.published_root,
            seq,
            root: self.root,
            owed: self.owed.keys().copied().collect(),
        }
        .encode();

        let mut out: Vec<Effect> = Vec::new();
        let mut data: BTreeSet<Cid> = BTreeSet::new();

        for (id, b) in &direct {
            data.insert(*id);
            out.push(Effect::PutBlock {
                id: *id,
                bytes: b.clone(),
                after: Vec::new(),
            });
        }

        // Every pack carries the manifest, so recovery can read any one of a
        // commit's packs and know the whole commit.
        let mut current: Vec<(u8, Vec<u8>)> = vec![(kind_raw(), manifest.clone())];
        let mut used = pack::PACK_HEADER + pack::member_cost(manifest.len());
        let mut packs: Vec<Vec<(u8, Vec<u8>)>> = Vec::new();
        for (_, b) in packable {
            let cost = pack::member_cost(b.len());
            if used + cost > self.params.max_pack && current.len() > 1 {
                packs.push(std::mem::take(&mut current));
                current = vec![(kind_raw(), manifest.clone())];
                used = pack::PACK_HEADER + pack::member_cost(manifest.len());
            }
            used += cost;
            current.push((pack::member_kind(&b), b));
        }
        // A commit that emitted nothing packable still ships its manifest: the
        // journal entry is the point, not the payload.
        packs.push(current);

        for members in packs {
            let body = match pack::build(&members) {
                Ok(b) => b,
                // Planning is what keeps a pack within its bounds, so a
                // refusal here is a bug in the planner, not a caller error.
                Err(e) => unreachable!("the engine planned an unbuildable pack: {e:?}"),
            };
            let id = pack::pack_id(&body);
            data.insert(id);
            out.push(Effect::PutPack {
                id,
                bytes: body,
                after: Vec::new(),
            });
        }

        self.next_seq += 1;
        self.pending = Some(Commit {
            seq,
            root: self.root,
            data,
            confirmed: BTreeSet::new(),
            writes,
            head_sent: false,
            bytes,
        });
        out
    }

    fn on_confirmed(&mut self, id: Cid) -> Vec<Effect> {
        let mut out = Vec::new();
        // A parity block landing: the group it belongs to is that much closer
        // to having redundancy.
        if let Some(group) = self.in_flight_parity.remove(&id) {
            let done = !self.in_flight_parity.values().any(|g| *g == group);
            if done {
                self.owed.remove(&group);
                if let Some(owners) = self.parity_owners.remove(&group) {
                    for (client, write_id) in owners {
                        out.push(Effect::Notify {
                            client,
                            write_id,
                            state: State::ParityComplete,
                        });
                    }
                }
            }
            return out;
        }

        let Some(c) = self.pending.as_mut() else {
            // A confirmation for something no commit is waiting on. Duplicates
            // and stragglers are normal on a network; they are not errors and
            // they are not events.
            return out;
        };
        if !c.data.contains(&id) || !c.confirmed.insert(id) {
            return out;
        }
        if c.confirmed.len() < c.data.len() || c.head_sent {
            return out;
        }
        // Every block of this commit has been READ BACK from our own node, so
        // the writes in it survive a restart. Only now may the head move: a
        // head naming a root whose blocks are not all there is a tree readers
        // cannot walk.
        c.head_sent = true;
        for (client, write_id) in &c.writes {
            out.push(Effect::Notify {
                client: *client,
                write_id: *write_id,
                state: State::Durable,
            });
        }
        out.push(Effect::UpdateHead {
            seq: c.seq,
            root: c.root,
            after: c.data.iter().copied().collect(),
        });
        out
    }

    fn on_failed(&mut self, id: Cid) -> Vec<Effect> {
        // Re-emit exactly what is missing, and nothing else. A retry that
        // re-sends the whole commit pays for every block again, and a retry
        // that re-sends nothing stalls it for ever.
        if let Some(group) = self.in_flight_parity.get(&id).copied() {
            if let Some(o) = self.owed.get(&group) {
                if let Some((_, bytes)) = o.blocks.iter().find(|(c, _)| *c == id) {
                    return vec![Effect::PutParity {
                        group,
                        id,
                        bytes: bytes.clone(),
                        after: vec![self.published_root],
                    }];
                }
            }
            return Vec::new();
        }
        let Some(c) = self.pending.as_ref() else {
            return Vec::new();
        };
        if !c.data.contains(&id) || c.confirmed.contains(&id) {
            return Vec::new();
        }
        // The bytes are not kept a second time: the block is in the warm tree,
        // and a pack is rebuilt from the same members to the same id.
        if let Some(bytes) = self.blocks.get(&id) {
            return vec![Effect::PutBlock {
                id,
                bytes: bytes.to_vec(),
                after: Vec::new(),
            }];
        }
        Vec::new()
    }

    fn on_head(&mut self, seq: u64) -> Vec<Effect> {
        let mut out = Vec::new();
        let Some(c) = self.pending.as_ref() else {
            return out;
        };
        if c.seq != seq {
            return out;
        }
        let c = self.pending.take().expect("checked");
        self.published_seq = c.seq;
        self.published_root = c.root;
        for (client, write_id) in &c.writes {
            out.push(Effect::Notify {
                client: *client,
                write_id: *write_id,
                state: State::Published,
            });
        }
        // The writes of this commit are the ones waiting on its groups'
        // redundancy.
        for key in self.owed.keys().copied().collect::<Vec<_>>() {
            self.parity_owners
                .entry(key)
                .or_default()
                .extend(c.writes.iter().copied());
        }
        // Coalescing off is the control: put each group's parity the moment
        // its commit is published, so two commits touching one group pay
        // twice.
        if !self.params.coalesce_parity {
            out.extend(self.emit_parity(|_| true));
        }
        // Whatever arrived while this commit was in flight becomes the next.
        if !self.folded.is_empty() {
            let emitted = self.blocks_of_unpublished();
            out.extend(self.start_commit(emitted));
        }
        out
    }

    /// The blocks the warm tree holds that the published head does not name.
    ///
    /// Recomputed rather than remembered: what a commit must ship is a
    /// property of the two trees, and a list maintained alongside is a second
    /// answer to the same question that can disagree with the first.
    fn blocks_of_unpublished(&self) -> Vec<(Cid, Vec<u8>)> {
        let mut reachable: BTreeSet<Cid> = BTreeSet::new();
        collect(&self.blocks, &self.published_root, &mut reachable);
        let mut now: BTreeSet<Cid> = BTreeSet::new();
        collect(&self.blocks, &self.root, &mut now);
        now.difference(&reachable)
            .filter_map(|c| self.blocks.get(c).map(|b| (*c, b.to_vec())))
            .collect()
    }

    fn on_tick(&mut self, now: u64) -> Vec<Effect> {
        self.now = now;
        if !self.params.coalesce_parity {
            return Vec::new();
        }
        let age = self.params.parity_age;
        // A group that did not change this tick has settled; one that keeps
        // changing goes out anyway once it has been unprotected long enough.
        self.emit_parity(move |o: &Owed| o.last_changed < now || now.saturating_sub(o.since) >= age)
    }

    fn emit_parity(&mut self, want: impl Fn(&Owed) -> bool) -> Vec<Effect> {
        let mut out = Vec::new();
        let keys: Vec<ParityIds> = self
            .owed
            .iter()
            .filter(|(_, o)| !o.sent && want(o))
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            let blocks = {
                let o = self.owed.get_mut(&key).expect("just listed");
                o.sent = true;
                o.blocks.clone()
            };
            for (id, bytes) in blocks {
                self.in_flight_parity.insert(id, key);
                out.push(Effect::PutParity {
                    group: key,
                    id,
                    bytes,
                    after: vec![self.published_root],
                });
            }
        }
        out
    }
}

/// Every block reachable from `root`, following children and referenced values.
fn collect(blocks: &MemBlocks, root: &Cid, out: &mut BTreeSet<Cid>) {
    if !out.insert(*root) {
        return;
    }
    let Some(bytes) = blocks.get(root) else {
        return;
    };
    let Ok(node) = Node::parse(bytes) else {
        return;
    };
    for i in 0..node.len() {
        if node.is_leaf() {
            if let freenet_prolly::node::Value::Ref { cid, .. } = node.value(i) {
                out.insert(cid);
            }
        } else {
            collect(blocks, &node.child(i).0, out);
        }
    }
}

fn kind_raw() -> u8 {
    freenet_prolly::kind::RAW
}
