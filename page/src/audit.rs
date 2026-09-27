//! THE ASSETS DASHBOARD'S AUDIT PASS (KEEPER §4, §7): a WALK of every node the root reaches (the engine's
//! `Walk::Nodes`, the one read path), then each group's blocks asked `Held` of the page's own node in batches, then
//! each block it does not hold asked with a GET, one at a time. ONE audit op is in flight at a time -- a walk fetch, a
//! Held batch or a GET -- so an app's request waits behind at most one. It measures; it repairs nothing (step 3).
//!
//! Health per GROUP (KEEPER §4.3): margin = its blocks held or fetched, minus k (its members). Whole: margin = m (its
//! parity). Degraded: 0 <= margin < m. Damaged: margin < 0, named by its first parity id. A block silent at its GET's
//! deadline is PENDING: counted, never guessed present or absent; the next pass asks it again.

// No catch-all over a state or an event: a new case fails the build until it is handled (clippy, run by the gate
// with -D warnings).
#![deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]

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

/// A pass in progress. ONE WRITER, BY TYPE (CLAUDE.md): every field is private to this module, and the pass
/// changes only through its `&mut` methods below -- the page reads it through accessors and can write nothing else.
pub(crate) struct Audit {
    root: Cid,
    /// Where the walk's next page starts; `None` with `walked` once every level is walked.
    next: Option<NodesAt>,
    walked: bool,
    /// The walk's read in flight (its engine request id), if any.
    walk_req: Option<u64>,
    reqs: u64,
    /// Every group the walk found: (members, parity ids).
    groups: Vec<(Vec<Cid>, Vec<Cid>)>,
    seen: BTreeMap<Cid, Seen>,
    /// Blocks still to ask `Held` about, and blocks the node did not hold, still to GET.
    to_hold: VecDeque<Cid>,
    to_get: VecDeque<Cid>,
    /// The ONE GET the pass waits on (sdk#530: a READER of that block's one GET key, which an app read may share and
    /// so carry in the Interactive lane -- the lane's slot cannot tell the pass it is still waiting; this does).
    getting: Option<Cid>,
    /// The page had no signer to ask `Held` (a reader's page): nothing was measured.
    unmeasured: bool,
    /// When the pass began (the page's clock, ms).
    started_at: u64,
}

/// The pass's next op, as [`Audit::next`] decides it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The walk's read is out (or parked on its fetch): nothing new.
    Walking,
    /// Walk the next page from `at`, as engine read `req`.
    Walk { req: u64, at: Option<NodesAt> },
    /// Ask `Held` about these.
    AskHeld(Vec<Cid>),
    /// GET this one.
    Get(Cid),
    /// Its GET is out: nothing new until the node answers (or is silent at its deadline).
    Getting,
    /// Nothing left: the pass is done.
    Done,
}

impl Audit {
    pub fn new(root: Cid, started_at: u64) -> Audit {
        Audit { root, next: None, walked: false, walk_req: None, reqs: 0, groups: Vec::new(), seen: BTreeMap::new(), to_hold: VecDeque::new(), to_get: VecDeque::new(), getting: None, unmeasured: false, started_at }
    }

    /// The root the pass audits.
    pub fn root(&self) -> Cid {
        self.root
    }

    /// Is `req` the walk's read in flight?
    pub fn walks(&self, req: u64) -> bool {
        self.walk_req == Some(req)
    }

    /// Is the walk's read still out?
    pub fn walking(&self) -> bool {
        self.walk_req.is_some()
    }

    /// What the pass learned of one block (its GET's answer, or its silence); the pass moves on from that GET.
    pub fn saw(&mut self, id: Cid, seen: Seen) {
        self.seen.insert(id, seen);
        if self.getting == Some(id) {
            self.getting = None;
        }
    }

    /// The block whose GET the pass is waiting on, if any.
    pub fn getting(&self) -> Option<Cid> {
        self.getting
    }

    /// NO SIGNER TO ASK: UNMEASURED -- nothing more is asked, nothing is absent.
    pub fn unasked(&mut self) {
        self.unmeasured = true;
        self.to_hold.clear();
        self.to_get.clear();
        self.getting = None;
    }

    /// A `Held` batch's answer for `ids`: held, or queued for a GET; past a short answer's end, asked again.
    pub fn held(&mut self, ids: Vec<Cid>, present: &[bool]) {
        for (i, id) in ids.into_iter().enumerate() {
            match present.get(i) {
                Some(true) => {
                    self.seen.insert(id, Seen::Held);
                }
                Some(false) => self.to_get.push_back(id),
                None => self.to_hold.push_back(id),
            }
        }
    }

    /// The pass's next op (KEEPER §7), taken: the walk's next page, a Held batch of at most `max_held`, or one GET.
    pub fn next(&mut self, max_held: usize) -> Next {
        if self.walk_req.is_some() {
            return Next::Walking;
        }
        if !self.walked {
            self.reqs += 1;
            self.walk_req = Some(self.reqs);
            return Next::Walk { req: self.reqs, at: self.next.clone() };
        }
        if !self.to_hold.is_empty() {
            let n = self.to_hold.len().min(max_held);
            return Next::AskHeld(self.to_hold.drain(..n).collect());
        }
        if self.getting.is_some() {
            return Next::Getting;
        }
        match self.to_get.pop_front() {
            Some(id) => {
                self.getting = Some(id);
                Next::Get(id)
            }
            None => Next::Done,
        }
    }

    /// The walk could not be served: what was walked is measured.
    pub fn walk_failed(&mut self) {
        self.walked = true;
        self.walk_req = None;
    }

    /// For a unit test of one cell: the walk done, so the pass goes straight to its asks.
    #[cfg(test)]
    pub fn walked_for_test(&mut self) {
        self.walked = true;
    }

    /// The blocks queued for a GET (a test reads them).
    #[cfg(test)]
    pub fn to_get(&self) -> Vec<Cid> {
        self.to_get.iter().copied().collect()
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

    /// Progress: (blocks asked so far, blocks the walk has reached). `of` grows while the walk runs.
    pub fn progress(&self) -> (usize, usize) {
        let of = self.seen.len() + self.to_hold.len();
        (self.seen.len() + self.to_get.len(), of)
    }

    /// The report, from what was seen, as of `now` (the page's clock).
    pub fn report(&self, now: u64) -> Report {
        let mut r = Report {
            root: self.root,
            measured: !self.unmeasured,
            health: Health::Unmeasured,
            groups: self.groups.len(),
            whole: 0,
            degraded: 0,
            damaged: Vec::new(),
            margins: BTreeMap::new(),
            pending: 0,
            rejected: Vec::new(),
            started_at: self.started_at,
            finished_at: now,
        };
        if self.unmeasured {
            return r;
        }
        for (members, parity) in &self.groups {
            let have = members.iter().chain(parity.iter()).filter(|id| matches!(self.seen.get(*id), Some(Seen::Held | Seen::Fetched))).count() as i64;
            let margin = have - members.len() as i64;
            *r.margins.entry(margin).or_default() += 1;
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
        r.health = Health::of(r.damaged.len(), r.degraded);
        r
    }
}

/// An asset's health in ONE word, derived here and nowhere else (the Assets tab shows it; KEEPER §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// No signer to ask: nothing measured (never all-absent).
    Unmeasured,
    /// A group below k.
    Damaged,
    /// Every group at or above k, one below k + m.
    Degraded,
    /// Every group whole.
    Whole,
}

impl Health {
    /// THE word from a pass's counts -- a report's, or a keep record's stored ones (the Assets tab): one derivation.
    /// `Unmeasured` is never derived from counts; it is a pass with nothing measured.
    pub fn of(damaged: usize, degraded: usize) -> Health {
        if damaged > 0 {
            Health::Damaged
        } else if degraded > 0 {
            Health::Degraded
        } else {
            Health::Whole
        }
    }
}

/// A pass's result (KEEPER §5's report, measured part): groups by health, the damaged named, blocks pending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The asset's health word (see [`Health`]).
    pub health: Health,
    /// How many groups have each margin (blocks to spare): what a `warn_below` is judged against.
    pub margins: BTreeMap<i64, usize>,
    /// When the pass began and ended (the page's clock, ms).
    pub started_at: u64,
    pub finished_at: u64,
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

#[cfg(test)]
mod tests {
    use super::Health;

    /// **The word from counts**: one damaged group outranks any number degraded; none of either is whole.
    #[test]
    fn a_damaged_group_outranks_degraded_ones_and_none_is_whole() {
        assert_eq!(Health::of(1, 5), Health::Damaged);
        assert_eq!(Health::of(0, 1), Health::Degraded);
        assert_eq!(Health::of(0, 0), Health::Whole);
    }
}
