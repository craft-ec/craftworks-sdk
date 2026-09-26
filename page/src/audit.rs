//! THE ASSETS DASHBOARD'S AUDIT PASS (KEEPER §4, §7): a WALK of every node the root reaches (the engine's
//! `Walk::Nodes`, the one read path), then each group's blocks asked `Held` of the page's own node in batches, then
//! each block it does not hold asked with a GET, one at a time. ONE audit op is in flight at a time -- a walk fetch, a
//! Held batch or a GET -- so an app's request waits behind at most one. It measures; it repairs nothing (step 3).
//!
//! Health per GROUP (KEEPER §4.3): margin = its blocks held or fetched, minus k (its members). Whole: margin = m (its
//! parity). Degraded: 0 <= margin < m. Damaged: margin < 0, named by its first parity id. A block silent at its GET's
//! deadline is PENDING: counted, never guessed present or absent; the next pass asks it again.

use engine::read::{NodeGroups, NodesAt};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What the pass knows of one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The node holds it (`Held`).
    Held,
    /// Not held; its GET brought it, verified (the node holds it again: F6).
    Fetched,
    /// Not held, and its GET answered NotFound (or bytes that are not it).
    Absent,
    /// Its GET was silent at its deadline.
    Pending,
    /// The node's Block contract REJECTED it (sdk#433, `Engine::rejected_blocks`): ABSENT toward its group's margin,
    /// never asked, never fetched, never repaired, and listed in the report's `rejected` (KEEPER §5).
    Rejected,
}

/// A pass in progress.
pub(crate) struct Audit {
    pub root: Cid,
    /// Where the walk's next page starts; `None` with `walked` once every level is walked.
    pub next: Option<NodesAt>,
    pub walked: bool,
    /// The walk's read in flight (its engine request id), if any.
    pub walk_req: Option<u64>,
    pub reqs: u64,
    /// Every group the walk found: (members, parity ids).
    pub groups: Vec<(Vec<Cid>, Vec<Cid>)>,
    pub seen: BTreeMap<Cid, Seen>,
    /// Blocks still to ask `Held` about, and blocks the node did not hold, still to GET.
    pub to_hold: VecDeque<Cid>,
    pub to_get: VecDeque<Cid>,
    /// The page had no signer to ask `Held` (a reader's page): nothing was measured.
    pub unmeasured: bool,
}

impl Audit {
    pub fn new(root: Cid) -> Audit {
        Audit { root, next: None, walked: false, walk_req: None, reqs: 0, groups: Vec::new(), seen: BTreeMap::new(), to_hold: VecDeque::new(), to_get: VecDeque::new(), unmeasured: false }
    }

    /// A group to measure: joined, each of its blocks queued to be asked once -- except one the node REJECTED
    /// (`rejected`, the engine's record): absent, and never asked.
    pub fn add_group(&mut self, members: Vec<Cid>, parity: Vec<Cid>, rejected: &BTreeSet<Cid>) {
        for id in members.iter().chain(parity.iter()) {
            if rejected.contains(id) {
                self.seen.insert(*id, Seen::Rejected);
            } else if !self.to_hold.contains(id) && !self.seen.contains_key(id) {
                self.to_hold.push_back(*id);
            }
        }
        self.groups.push((members, parity));
    }

    /// A page of the walk: its groups joined.
    pub fn walked_page(&mut self, nodes: Vec<NodeGroups>, next: Option<NodesAt>, rejected: &BTreeSet<Cid>) {
        for n in nodes {
            for (members, parity) in n.groups {
                self.add_group(members, parity, rejected);
            }
        }
        self.walked = next.is_none();
        self.next = next;
        self.walk_req = None;
    }

    /// The report, from what was seen.
    pub fn report(&self) -> Report {
        let mut r = Report { root: self.root, measured: !self.unmeasured, groups: self.groups.len(), whole: 0, degraded: 0, damaged: Vec::new(), pending: 0, rejected: Vec::new() };
        if self.unmeasured {
            return r;
        }
        for (members, parity) in &self.groups {
            let have = members.iter().chain(parity.iter()).filter(|id| matches!(self.seen.get(*id), Some(Seen::Held | Seen::Fetched))).count() as i64;
            let margin = have - members.len() as i64;
            if margin >= parity.len() as i64 {
                r.whole += 1;
            } else if margin >= 0 {
                r.degraded += 1;
            } else {
                r.damaged.push(parity.first().or(members.first()).copied().unwrap_or([0; 32]));
            }
        }
        r.pending = self.seen.values().filter(|s| **s == Seen::Pending).count();
        r.rejected = self.seen.iter().filter(|(_, s)| **s == Seen::Rejected).map(|(id, _)| *id).collect();
        r
    }
}

/// A pass's result (KEEPER §5's report, measured part): groups by health, the damaged named, blocks pending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The root the pass measured (the page's published root when it began).
    pub root: Cid,
    /// `false`: the page had no signer to ask (a reader's page) -- the asset is UNMEASURED, and its counts say
    /// nothing (never all-absent).
    pub measured: bool,
    pub groups: usize,
    pub whole: usize,
    pub degraded: usize,
    /// Each damaged group, named by its first parity id.
    pub damaged: Vec<Cid>,
    /// Blocks whose GET was silent at its deadline.
    pub pending: usize,
    /// Blocks the node REJECTED (sdk#433): each counted absent in its group, never repaired (KEEPER §5). `damaged`
    /// keeps its one meaning -- groups whose margin is below 0.
    pub rejected: Vec<Cid>,
}
