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
pub mod read;

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

// `Event` is not `Eq`: a scan carries a `Range`, whose bounds are
// `Bound<Vec<u8>>` and which the library does not make `Eq`. Tests compare
// what an event PRODUCES, which is what matters.
#[derive(Clone, Debug)]
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

    // ---- the read path ----
    Get {
        client: ClientId,
        req_id: read::ReqId,
        key: Vec<u8>,
    },
    Scan {
        client: ClientId,
        req_id: read::ReqId,
        range: Box<freenet_prolly::range::Range>,
    },
    /// Advisory: bring these roots' blocks nearer, within the budget. It may
    /// make later reads SOONER; it may never make them more or wider.
    Preload {
        client: ClientId,
        roots: Vec<Cid>,
    },
    /// A fetched block came back. Hash-checked before it is believed.
    BlockArrived {
        id: Cid,
        bytes: Vec<u8>,
    },
    /// A bounded attempt ended without an answer. Not a failure of the read:
    /// the next attempt is issued, until the budget runs out.
    BlockMissed(Cid),
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

    // ---- the read path ----
    FetchBlock {
        id: Cid,
        via: read::Via,
        /// Which try this is. The shell may race or widen on a later attempt;
        /// the core only says the earlier one did not answer.
        attempt: u32,
    },
    Reply {
        client: ClientId,
        req_id: read::ReqId,
        result: read::ReadResult,
    },
    Progress {
        client: ClientId,
        req_id: read::ReqId,
        levels_done: usize,
        levels_total: usize,
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
    /// Find superseded groups by scanning the WHOLE tree instead of only what
    /// changed. This is what the engine used to do: correct, and proportional
    /// to the tree's size on every write. Kept as the control for the cost
    /// bound — a bound nothing can blow is not a bound.
    pub whole_tree_supersede_scan: bool,
    /// Move a superseded group's waiters onto the groups that replaced it.
    /// Off, a write whose group is re-coded is reported `ParityComplete`
    /// immediately — which is the control, and a false durability claim: its
    /// data now sits in a coding whose parity is not on the network.
    pub transfer_superseded_waiters: bool,

    // ---- the read path ----
    /// How many blocks one attempt may ask for. `Page::need` can name the
    /// whole remainder of a range, and asking for all of it is how one scan
    /// becomes an unbounded fan-out. This is a bound on the CORE's appetite,
    /// not a batch size: the shell still decides how many it issues at once.
    pub max_fetch_per_round: usize,
    /// How many times a block is asked for before the read is answered
    /// `Unavailable`. Attempts are RE-ISSUED, not waited on (ARCHITECTURE §7).
    pub max_attempts: u32,
    /// Two requests needing one block share its fetch. Off = the control.
    pub share_fetches: bool,
    /// Bytes the warm set may hold. Eviction never drops a block a parked
    /// read or an unpublished commit still needs.
    pub max_warm_bytes: usize,
    /// Ceilings on an advisory preload: roots, blocks, bytes. A preload may
    /// make reads SOONER, never more or wider, so a hostile manifest costs
    /// the budget and not what it asked for.
    pub preload_roots: usize,
    pub preload_blocks: usize,
    pub preload_bytes: usize,
    /// On a miss, also ask for every block the warm nodes name — a plausible
    /// "fetch ahead" a reader might write, and the control for the cost
    /// bound. A bound no implementation can exceed is not a bound.
    pub fetch_greedily: bool,
    /// Pin every block a parked read has been handed, until it replies. Off
    /// is the control, and the state this engine was in: a tight warm set
    /// evicts the path a read just paid for, the read re-fetches it, the
    /// fetch SUCCEEDS so the attempt budget never trips, and nothing ever
    /// ends.
    pub pin_parked_reads: bool,
    /// Count the nodes a descent would touch. It is a second walk, for the
    /// cost gate and nothing else, so it is off unless a test asks.
    pub count_descent: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            max_pack: 1024 * 1024,
            max_packed_value: 64 * 1024,
            max_backlog: 8 * 1024 * 1024,
            parity_age: 32,
            coalesce_parity: true,
            whole_tree_supersede_scan: false,
            transfer_superseded_waiters: true,
            max_fetch_per_round: 8,
            max_attempts: 3,
            share_fetches: true,
            max_warm_bytes: 64 * 1024 * 1024,
            preload_roots: 4,
            preload_blocks: 256,
            preload_bytes: 4 * 1024 * 1024,
            fetch_greedily: false,
            pin_parked_reads: true,
            count_descent: false,
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
    /// The pack BODIES, kept so a failed pack can be sent again. A pack is
    /// not a block of the tree and is nowhere else: without this, a failed
    /// pack put re-emitted nothing and the commit stalled for ever, waiting
    /// for a confirmation for something that had already failed.
    packs: BTreeMap<Cid, Vec<u8>>,
    /// The parity groups this commit coded. A write is parity-complete when
    /// none of ITS commit's groups is still owed — not when some later
    /// commit's groups are done.
    groups: BTreeSet<ParityIds>,
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
    /// The groups each write is still waiting on.
    ///
    /// A SET and not a count, because a superseded group is replaced by the
    /// groups that now cover the same members, and a count cannot express
    /// "swap one for two" without drifting. `ParityComplete` is a state of the
    /// WRITE — reported once, when this set empties.
    parity_waiting: BTreeMap<(ClientId, WriteId), BTreeSet<ParityIds>>,
    /// Notifications raised while applying a write — a group superseded part
    /// way through — collected here so `on_write` can return them with the
    /// rest rather than dropping them.
    pending_notifications: Vec<Effect>,
    /// Blocks written since the last commit shipped. Accumulated as each
    /// write emits them rather than recovered later by comparing two trees:
    /// the emitting is where the answer is already known, and walking for it
    /// afterwards is the same answer computed again, more expensively and
    /// with a chance of disagreeing.
    unpublished: Vec<(Cid, Vec<u8>)>,
    /// Groups CODED since the last commit shipped.
    ///
    /// Not "everything currently owed": that includes groups an earlier
    /// commit coded and has not yet put, and waiting on those makes a write's
    /// `ParityComplete` depend on redundancy for data it never touched. It
    /// also hides a superseded group behind the others, so the write never
    /// notices the one covering ITS data was replaced.
    coded_since_commit: BTreeSet<ParityIds>,
    /// Everything the read path is waiting on.
    reads: read::Reads,
    /// Bytes held warm, kept as a RUNNING total. Re-summing every block on
    /// every arrival is O(n) per block and so O(n^2) to warm a tree.
    warm_bytes: usize,
    /// When each warm block was last written or delivered, for LRU eviction.
    last_used: BTreeMap<Cid, u64>,
    use_clock: u64,
    /// Nodes parsed on the write path. A cost counter, not a statistic: the
    /// whole point of the diff walk is that this stays proportional to the
    /// tree's DEPTH, and a test that does not measure it would not notice the
    /// day it goes back to being proportional to its SIZE.
    nodes_parsed: usize,
    now: u64,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new(Params::default())
    }
}

impl Engine {
    /// Anything a caller can set must not be able to panic the core later.
    /// `max_packed_value` above `max_pack` describes a value that must ride in
    /// a pack and cannot fit in one; the planner would reach an `unreachable!`
    /// three steps away, where nothing points back at the setting that caused
    /// it. Refused here instead, naming both numbers.
    pub fn new(params: Params) -> Self {
        assert!(
            params.max_packed_value + pack::member_cost(0) + pack::PACK_HEADER <= params.max_pack,
            "max_packed_value ({}) cannot fit in a pack of max_pack ({}): a value \
             that must be packed and cannot be is a commit that can never ship",
            params.max_packed_value,
            params.max_pack
        );
        assert!(
            params.max_pack > pack::PACK_HEADER,
            "max_pack holds no members"
        );
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
            parity_waiting: BTreeMap::new(),
            pending_notifications: Vec::new(),
            unpublished: Vec::new(),
            coded_since_commit: BTreeSet::new(),
            reads: read::Reads::default(),
            warm_bytes: 0,
            last_used: BTreeMap::new(),
            use_clock: 0,
            nodes_parsed: 0,
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

    /// Nodes parsed on the write path since the last [`Engine::reset_cost`].
    ///
    /// Exposed so a test can assert the write path is proportional to the
    /// tree's depth and not to its size. It is the only honest way to keep
    /// that true: the O(tree) version was correct, passed every test, and
    /// would have cost roughly 10^5 node parses per keystroke at a million
    /// keys, inside a 5 s delegate call.
    pub fn nodes_parsed(&self) -> usize {
        self.nodes_parsed
    }

    pub fn reset_cost(&mut self) {
        self.nodes_parsed = 0;
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
            Event::Get {
                client,
                req_id,
                key,
            } => self.on_read(client, req_id, read::Want::Get(key)),
            Event::Scan {
                client,
                req_id,
                range,
            } => self.on_read(client, req_id, read::Want::Scan(range)),
            Event::Preload { client, roots } => self.on_preload(client, roots),
            Event::BlockArrived { id, bytes } => self.on_arrived(id, bytes),
            Event::BlockMissed(id) => self.on_missed(id),
        }
    }

    /// Try a read, reply if it is answerable now, park it if it is not.
    ///
    /// Rule 1: a warm hit replies in the same `step`, with no other effect. A
    /// read that is already answerable must not cost a round trip, because
    /// most reads in a live app are answerable.
    fn on_read(&mut self, client: ClientId, req_id: read::ReqId, want: read::Want) -> Vec<Effect> {
        let root = self.published_root;
        self.reads.parked.insert(
            req_id,
            read::Parked {
                client,
                want,
                root,
                levels_done: 0,
                held: BTreeSet::from([root]),
            },
        );
        self.drive(req_id)
    }

    /// Advance one parked read as far as what is warm allows.
    fn drive(&mut self, req_id: read::ReqId) -> Vec<Effect> {
        let Some(p) = self.reads.parked.get(&req_id).cloned() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match read::attempt(
            &self.blocks,
            &self.params,
            &p.want,
            &p.root,
            &mut self.nodes_parsed,
        ) {
            read::Attempt::Done(result) => {
                self.reads.parked.remove(&req_id);
                self.forget_waiting(req_id);
                out.push(Effect::Reply {
                    client: p.client,
                    req_id,
                    result,
                });
            }
            read::Attempt::Broken(cid) => {
                // The tree names a block whose content is not that block. No
                // number of fetches fixes it, and answering "absent" would be
                // a wrong answer rather than a missing one.
                self.reads.parked.remove(&req_id);
                self.forget_waiting(req_id);
                out.push(Effect::Reply {
                    client: p.client,
                    req_id,
                    result: read::ReadResult::Unavailable(cid),
                });
            }
            read::Attempt::Need(ids) => {
                if let Some(q) = self.reads.parked.get_mut(&req_id) {
                    q.levels_done += 1;
                }
                let levels_done = self.reads.parked.get(&req_id).map_or(0, |q| q.levels_done);
                out.push(Effect::Progress {
                    client: p.client,
                    req_id,
                    levels_done,
                    // Not known until the walk ends; one more than what is
                    // done is the honest floor rather than a guessed height.
                    levels_total: levels_done + 1,
                });
                let mut ids = ids;
                if self.params.fetch_greedily {
                    // Everything any warm node names, whether or not this read
                    // needs it.
                    let named: Vec<Cid> = self
                        .blocks
                        .0
                        .values()
                        .filter_map(|b| Node::parse(b).ok())
                        .filter(|n| !n.is_leaf())
                        .flat_map(|n| (0..n.len()).map(move |i| n.child(i).0).collect::<Vec<_>>())
                        .collect();
                    // Capped, or the control never terminates and measures
                    // nothing. It still blows the bound many times over.
                    ids.extend(named.into_iter().take(64));
                }
                // Never ask for what is already here. `Page::need` can name
                // blocks a previous round already brought in, and a reader
                // that re-fetches them makes no progress at all.
                ids.retain(|id| self.blocks.get(id).is_none());
                let cap = if self.params.fetch_greedily {
                    usize::MAX
                } else {
                    self.params.max_fetch_per_round
                };
                for id in ids.into_iter().take(cap) {
                    if self.reads.want(id, req_id, self.params.share_fetches) {
                        let attempt = *self.reads.attempts.entry(id).or_insert(0);
                        let via = self
                            .reads
                            .in_pack
                            .get(&id)
                            .copied()
                            .map_or(read::Via::Direct, read::Via::Pack);
                        self.reads.fetches += 1;
                        out.push(Effect::FetchBlock { id, via, attempt });
                    }
                }
            }
        }
        out
    }

    fn forget_waiting(&mut self, req_id: read::ReqId) {
        self.reads.waiting.retain(|_, reqs| {
            reqs.remove(&req_id);
            !reqs.is_empty()
        });
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

        let old_root = self.root;
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
        self.record_owed(old_root, &applied.parity, &emitted);
        // What this commit, or the next one, must ship. Collected here because
        // this is where it is known.
        self.unpublished.extend(emitted.iter().cloned());

        let mut out = vec![Effect::Notify {
            client,
            write_id,
            state: State::Accepted,
        }];
        out.extend(std::mem::take(&mut self.pending_notifications));
        self.folded.push((client, write_id));
        self.folded_bytes += size;
        if self.pending.is_none() {
            let to_ship = self.take_unpublished();
            out.extend(self.start_commit(to_ship));
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
    fn record_owed(
        &mut self,
        old_root: Cid,
        parity: &[(Cid, Vec<u8>)],
        emitted: &[(Cid, Vec<u8>)],
    ) {
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
        // Which groups this write SUPERSEDED, found by walking only what
        // changed.
        //
        // A whole-tree scan answers the same question and is what this used to
        // do; at a million keys that is about 10^5 node parses on every write.
        // The old tree and the new one share everything except the path that
        // changed, and the new nodes are already in hand -- so the nodes the
        // write REPLACED are exactly those reachable from the old root that
        // the new tree does not contain, and the walk stops the moment it
        // meets a node that survived.
        let mut survivors: BTreeSet<Cid> = BTreeSet::new();
        survivors.insert(self.root);
        let mut fresh: BTreeSet<Cid> = BTreeSet::new();
        for (id, bytes) in emitted {
            if let Ok(n) = Node::parse(bytes) {
                fresh.insert(*id);
                if !n.is_leaf() {
                    for i in 0..n.len() {
                        survivors.insert(n.child(i).0);
                    }
                }
            }
        }
        // A node the write emitted is not a survivor of the OLD tree even if
        // an old node had the same id; it is the new tree's own.
        for id in &fresh {
            survivors.remove(id);
        }
        let mut was_listed: BTreeSet<Cid> = BTreeSet::new();
        if self.params.whole_tree_supersede_scan {
            survivors.clear();
        }
        let mut stack = vec![old_root];
        let mut seen: BTreeSet<Cid> = BTreeSet::new();
        while let Some(cid) = stack.pop() {
            if survivors.contains(&cid) || !seen.insert(cid) {
                continue;
            }
            let Some(bytes) = self.blocks.get(&cid) else {
                continue;
            };
            self.nodes_parsed += 1;
            let Ok(n) = Node::parse(bytes) else {
                continue;
            };
            was_listed.extend(n.parity());
            if !n.is_leaf() {
                for i in 0..n.len() {
                    stack.push(n.child(i).0);
                }
            }
        }
        // Still listed by what replaced them? Then the group survived the
        // rewrite and is not superseded.
        let mut now_listed: BTreeSet<Cid> = BTreeSet::new();
        for (_, bytes) in emitted {
            if let Ok(n) = Node::parse(bytes) {
                now_listed.extend(n.parity());
            }
        }
        let gone: BTreeSet<Cid> = was_listed.difference(&now_listed).copied().collect();
        let now = self.now;
        let dropped: Vec<ParityIds> = self
            .owed
            .iter()
            // Never drop one already emitted: it is out there being confirmed,
            // and forgetting it would lose the ParityComplete it owes.
            .filter(|(key, o)| !o.sent && gone.contains(&key[0]))
            .map(|(key, _)| *key)
            .collect();
        // What now covers the members those groups held.
        let replacements: BTreeSet<ParityIds> = by_group.keys().copied().collect();
        for key in dropped {
            self.owed.remove(&key);
            self.coded_since_commit.remove(&key);
            let n = self.transfer_waiters(key, &replacements);
            self.pending_notifications.extend(n);
        }

        for (key, blocks) in by_group {
            self.coded_since_commit.insert(key);
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

    /// A group stopped being owed: credit every write waiting on it, and tell
    /// the ones with nothing left to wait for.
    ///
    /// Called when a group's three blocks are all confirmed, and when a group
    /// is SUPERSEDED — re-coded by a later write before its parity went out.
    /// Both end the wait honestly: `ParityComplete` says *nothing is
    /// outstanding on this write's behalf*, and a superseded group has been
    /// replaced by a newer coding of the same members, which the write that
    /// caused it is now waiting on. Treating a supersede as still-pending
    /// would leave the earlier write waiting for a put that will never
    /// happen, on purpose.
    fn settle_group(&mut self, group: ParityIds) -> Vec<Effect> {
        let mut out = Vec::new();
        let mut done: Vec<(ClientId, WriteId)> = Vec::new();
        for (w, waiting) in self.parity_waiting.iter_mut() {
            if waiting.remove(&group) && waiting.is_empty() {
                done.push(*w);
            }
        }
        for w in done {
            self.parity_waiting.remove(&w);
            out.push(Effect::Notify {
                client: w.0,
                write_id: w.1,
                state: State::ParityComplete,
            });
        }
        out
    }

    /// A group was re-coded before its parity went out. The write that is
    /// waiting on it must now wait on whatever covers those members INSTEAD.
    ///
    /// Not settled. The write's data did not go away — it sits in the newer
    /// coding, whose parity is not on the network either, so reporting
    /// `ParityComplete` here would tell a client that redundancy exists for
    /// its data when none does. `ParityComplete` means *every group that
    /// currently covers what this write changed has its parity on the
    /// network*, never *nothing is outstanding under this write's name*.
    ///
    /// The transfer is deliberately generous: every group the superseding
    /// write coded, not an attempt to work out which one inherited these
    /// members. Over-waiting delays a notification; a false completion is a
    /// durability claim that is not true.
    fn transfer_waiters(&mut self, from: ParityIds, to: &BTreeSet<ParityIds>) -> Vec<Effect> {
        if !self.params.transfer_superseded_waiters {
            return self.settle_group(from);
        }
        let mut out = Vec::new();
        let mut done: Vec<(ClientId, WriteId)> = Vec::new();
        for (w, waiting) in self.parity_waiting.iter_mut() {
            if !waiting.remove(&from) {
                continue;
            }
            waiting.extend(to.iter().copied());
            // Nothing covers those members any more — the write was a delete,
            // or what it wrote is gone. There is no redundancy left to owe.
            if waiting.is_empty() {
                done.push(*w);
            }
        }
        for w in done {
            self.parity_waiting.remove(&w);
            out.push(Effect::Notify {
                client: w.0,
                write_id: w.1,
                state: State::ParityComplete,
            });
        }
        out
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

        let mut pack_bodies: BTreeMap<Cid, Vec<u8>> = BTreeMap::new();
        for members in packs {
            let body = match pack::build(&members) {
                Ok(b) => b,
                // Planning is what keeps a pack within its bounds, and
                // `Engine::new` refuses the settings that could make one
                // unplannable, so a refusal here is a bug in the planner.
                Err(e) => unreachable!("the engine planned an unbuildable pack: {e:?}"),
            };
            let id = pack::pack_id(&body);
            data.insert(id);
            pack_bodies.insert(id, body.clone());
            out.push(Effect::PutPack {
                id,
                bytes: body,
                after: Vec::new(),
            });
        }

        // The groups this commit CODED. A write waits on these and on nothing
        // else.
        let groups = std::mem::take(&mut self.coded_since_commit);
        self.next_seq += 1;
        self.pending = Some(Commit {
            seq,
            root: self.root,
            data,
            packs: pack_bodies,
            groups,
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
                out.extend(self.settle_group(group));
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
        // A pack first: its body exists only here, so if this does not send it
        // again nothing will, and the commit waits for ever on a put that has
        // already failed.
        if let Some(body) = c.packs.get(&id) {
            return vec![Effect::PutPack {
                id,
                bytes: body.clone(),
                after: Vec::new(),
            }];
        }
        // A value block: it is in the warm tree.
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
        // Only the groups THIS commit coded. Assigning every currently-owed
        // group to it would make a write wait on redundancy for data it never
        // touched, coded by a commit it has nothing to do with.
        //
        // Groups already confirmed between the commit shipping and its head
        // landing are not waited on: `owed` no longer holds them.
        let still: BTreeSet<ParityIds> = c
            .groups
            .iter()
            .filter(|g| self.owed.contains_key(*g))
            .copied()
            .collect();
        for w in &c.writes {
            if still.is_empty() {
                // A commit that coded no groups — a small write with no
                // referenced values — owes nothing, so its writes are
                // parity-complete as soon as they are published. Waiting for a
                // group that does not exist is how a state gets skipped
                // without `failed`.
                self.parity_waiting.remove(w);
                out.push(Effect::Notify {
                    client: w.0,
                    write_id: w.1,
                    state: State::ParityComplete,
                });
            } else {
                self.parity_waiting.insert(*w, still.clone());
            }
        }
        // Coalescing off is the control: put each group's parity the moment
        // its commit is published, so two commits touching one group pay
        // twice.
        if !self.params.coalesce_parity {
            out.extend(self.emit_parity(|_| true));
        }
        // Whatever arrived while this commit was in flight becomes the next.
        if !self.folded.is_empty() {
            let to_ship = self.take_unpublished();
            out.extend(self.start_commit(to_ship));
        }
        out
    }

    /// The blocks written since the last commit shipped, each once.
    ///
    /// This used to be recovered by walking the published tree and the warm
    /// tree and taking the difference — two whole-tree walks per follow-on
    /// commit, to recompute something the write path already knew. A write
    /// that emits a block is the moment the answer exists; collecting it then
    /// costs nothing and cannot disagree with itself.
    ///
    /// De-duplicated by id, because two writes in one commit can touch the
    /// same node and a pack is a set.
    fn take_unpublished(&mut self) -> Vec<(Cid, Vec<u8>)> {
        let mut seen: BTreeSet<Cid> = BTreeSet::new();
        std::mem::take(&mut self.unpublished)
            .into_iter()
            .filter(|(c, _)| seen.insert(*c))
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

fn kind_raw() -> u8 {
    freenet_prolly::kind::RAW
}

impl Engine {
    /// A fetched block came back.
    ///
    /// Rule 3: hash-checked against the id it was asked for BEFORE it touches
    /// the warm tree. A block that fails is treated exactly as a miss — it is
    /// not cached, not parsed, and not an error the caller sees, because a
    /// node that sends rubbish must not be able to break a reader that asked
    /// for something real.
    fn on_arrived(&mut self, id: Cid, bytes: Vec<u8>) -> Vec<Effect> {
        if !read::matches_id(&id, &bytes) {
            return self.on_missed(id);
        }
        let mut out = Vec::new();
        let mut landed: Vec<Cid> = vec![id];
        // A pack carries many blocks, and one fetch of it can answer several
        // parked reads at once. Its members are checked the same way: a pack
        // is a transport, and the blocks inside are the same blocks with the
        // same ids.
        if freenet_prolly::block_id(pack::PACK_KIND, &bytes) == id {
            for (mid, mbytes) in pack::members(&bytes) {
                if read::matches_id(&mid, &mbytes) {
                    out.extend(self.remember(mid, &mbytes));
                    self.reads.in_pack.insert(mid, id);
                    landed.push(mid);
                }
            }
        } else {
            let n = self.remember(id, &bytes);
            out.extend(n);
        }

        let mut woken: BTreeSet<read::ReqId> = BTreeSet::new();
        for l in &landed {
            self.reads.attempts.remove(l);
            if let Some(reqs) = self.reads.waiting.remove(l) {
                for r in &reqs {
                    // Pinned for as long as this read is parked: it will
                    // re-descend through this block on its next attempt.
                    if let Some(p) = self.reads.parked.get_mut(r) {
                        p.held.extend(landed.iter().copied());
                    }
                }
                woken.extend(reqs);
            }
        }
        for req in woken {
            // A read answered by eviction above is gone; driving it is a no-op.
            out.extend(self.drive(req));
        }
        out
    }

    /// An attempt ended without an answer.
    ///
    /// Rule 5: re-issued, not waited on. Only when the budget runs out does
    /// the read get an answer — `Unavailable`, which is a reply. A read that
    /// never answers is indistinguishable from a wedged node, and the caller
    /// can do nothing about either.
    fn on_missed(&mut self, id: Cid) -> Vec<Effect> {
        let Some(reqs) = self.reads.waiting.get(&id).cloned() else {
            return Vec::new();
        };
        let attempt = self.reads.attempts.entry(id).or_insert(0);
        *attempt += 1;
        let attempt = *attempt;
        if attempt < self.params.max_attempts {
            let via = self
                .reads
                .in_pack
                .get(&id)
                .copied()
                .map_or(read::Via::Direct, read::Via::Pack);
            self.reads.fetches += 1;
            return vec![Effect::FetchBlock { id, via, attempt }];
        }
        // Out of attempts. Everyone waiting on this block is told, once.
        self.reads.waiting.remove(&id);
        self.reads.attempts.remove(&id);
        let mut out = Vec::new();
        for req in reqs {
            if let Some(p) = self.reads.parked.remove(&req) {
                self.forget_waiting(req);
                out.push(Effect::Reply {
                    client: p.client,
                    req_id: req,
                    result: read::ReadResult::Unavailable(id),
                });
            }
        }
        out
    }

    /// Rule 7: preload is advisory and budgeted.
    ///
    /// It may make a later read SOONER; it may never make one more or wider.
    /// So it is truncated to the budget rather than refused, and it only
    /// walks roots this session already holds — a manifest naming a million
    /// roots costs the budget, not the manifest.
    fn on_preload(&mut self, _client: ClientId, roots: Vec<Cid>) -> Vec<Effect> {
        let mut out = Vec::new();
        let mut blocks = 0usize;
        for root in roots.into_iter().take(self.params.preload_roots) {
            // Only over a root the session already holds: a preload is a hint
            // about what is already ours, not an invitation to fetch a
            // stranger's tree.
            if self.blocks.get(&root).is_none() {
                continue;
            }
            let mut stack = vec![root];
            while let Some(cid) = stack.pop() {
                if blocks >= self.params.preload_blocks {
                    return out;
                }
                let Some(b) = self.blocks.get(&cid) else {
                    // Not held: this is what a preload is FOR.
                    if self.reads.waiting.contains_key(&cid) {
                        continue;
                    }
                    blocks += 1;
                    self.reads.fetches += 1;
                    out.push(Effect::FetchBlock {
                        id: cid,
                        via: read::Via::Direct,
                        attempt: 0,
                    });
                    continue;
                };
                self.nodes_parsed += 1;
                let Ok(n) = Node::parse(b) else { continue };
                if !n.is_leaf() {
                    for i in 0..n.len() {
                        stack.push(n.child(i).0);
                    }
                }
            }
        }
        out
    }

    /// Put a block in the warm set, within its bound.
    ///
    /// Rule 8: eviction never drops a block a parked read or an unpublished
    /// commit still needs — evicting those would turn a bounded cache into a
    /// cause of the very fetches it exists to avoid, and could lose a block
    /// that exists nowhere else yet.
    fn remember(&mut self, id: Cid, bytes: &[u8]) -> Vec<Effect> {
        if self.blocks.get(&id).is_none() {
            self.warm_bytes += bytes.len();
        }
        self.blocks.insert(id, bytes);
        self.use_clock += 1;
        self.last_used.insert(id, self.use_clock);
        if self.warm_bytes <= self.params.max_warm_bytes {
            return Vec::new();
        }

        // What must not be evicted. Every block a parked read has been HANDED,
        // not just its root: a read re-descends from the root on each resume,
        // so dropping any of the path sends it back for a block it just had —
        // and that fetch succeeds, so the attempt budget never trips and the
        // read never ends.
        let mut pinned: BTreeSet<Cid> = self.unpublished.iter().map(|(c, _)| *c).collect();
        pinned.insert(self.root);
        pinned.insert(self.published_root);
        if self.params.pin_parked_reads {
            for p in self.reads.parked.values() {
                pinned.extend(p.held.iter().copied());
            }
        } else {
            for p in self.reads.parked.values() {
                pinned.insert(p.root);
            }
        }

        // Least recently USED first. Eviction in id order is eviction by
        // BLAKE3, which is to say at random, and the block that just arrived
        // is as likely a victim as any other.
        let mut victims: Vec<(u64, Cid)> = self
            .blocks
            .0
            .keys()
            .filter(|c| !pinned.contains(*c))
            .map(|c| (self.last_used.get(c).copied().unwrap_or(0), *c))
            .collect();
        victims.sort_unstable();
        for (_, v) in victims {
            if self.warm_bytes <= self.params.max_warm_bytes {
                break;
            }
            if let Some(b) = self.blocks.0.remove(&v) {
                self.warm_bytes -= b.len();
                self.last_used.remove(&v);
            }
        }
        if self.warm_bytes <= self.params.max_warm_bytes {
            return Vec::new();
        }

        // Still over, with nothing left to drop: what the parked reads need at
        // once does not fit. They are ANSWERED rather than left to evict each
        // other's paths for ever. The largest goes first, and only as many as
        // it takes.
        let mut by_size: Vec<(usize, read::ReqId)> = self
            .reads
            .parked
            .iter()
            .map(|(r, p)| (p.held.len(), *r))
            .collect();
        by_size.sort_unstable_by(|a, b| b.cmp(a));
        let mut out = Vec::new();
        for (_, req) in by_size {
            if self.warm_bytes <= self.params.max_warm_bytes {
                break;
            }
            let Some(p) = self.reads.parked.remove(&req) else {
                continue;
            };
            self.forget_waiting(req);
            out.push(Effect::Reply {
                client: p.client,
                req_id: req,
                result: read::ReadResult::OutOfWarmSpace,
            });
            // Its pins are released; drop what is now unpinned.
            let mut pinned: BTreeSet<Cid> = self.unpublished.iter().map(|(c, _)| *c).collect();
            pinned.insert(self.root);
            pinned.insert(self.published_root);
            for q in self.reads.parked.values() {
                pinned.extend(q.held.iter().copied());
            }
            let mut victims: Vec<(u64, Cid)> = self
                .blocks
                .0
                .keys()
                .filter(|c| !pinned.contains(*c))
                .map(|c| (self.last_used.get(c).copied().unwrap_or(0), *c))
                .collect();
            victims.sort_unstable();
            for (_, v) in victims {
                if self.warm_bytes <= self.params.max_warm_bytes {
                    break;
                }
                if let Some(b) = self.blocks.0.remove(&v) {
                    self.warm_bytes -= b.len();
                    self.last_used.remove(&v);
                }
            }
        }
        out
    }

    /// Start from a published root this engine did not write.
    ///
    /// What a cold reader has: a head, and nothing else. Only for tests — a
    /// real engine learns its root by reading its own head.
    pub fn adopt_root_for_test(&mut self, root: Cid) {
        self.root = root;
        self.published_root = root;
    }

    /// Fetches emitted, for the cost gate.
    pub fn fetches(&self) -> usize {
        self.reads.fetches
    }
}
