//! THE ASSETS DASHBOARD'S AUDIT PASS (KEEPER §4, §5, §7): a WALK of every node the root reaches (the engine's
//! `Walk::Nodes`, the one read path), then each group's blocks asked `Held` of the page's own node in batches, then
//! each block it does not hold asked with a GET, one at a time; then, under the asset's [`Repair`] policy, each group
//! the policy names is REPAIRED (step 3). ONE audit op is in flight at a time (K1).
//!
//! A pass is TWO state machines (KEEPER §5 "A pass's life", the tables this file implements): each block's [`Seen`]
//! and the pass's [`Phase`], one enum each whose cases carry their data, changed ONLY by [`Audit::on`] -- the one
//! method here that takes `&mut self` (a source test holds it). Everything else is DERIVED from them: the blocks to ask
//! (no state yet), the blocks to GET (`NotHeld`), a group's margin, the groups to repair, and the report's counts.
//!
//! REPAIR (KEEPER §5; the architect's D1/D2): a group is repaired only when the policy names it and it has k blocks to
//! rebuild from. A block whose verified bytes the page holds goes out as held; a member is otherwise rebuilt with
//! `repair::rebuild` (hash-checked); parity is recomputed per group (`parity::repair_group` states ->
//! `parity::encode_group`) and goes out only under its listed id. A repair PUT is re-sent until ANSWERED (rules 7/8).

use engine::read::{NodeGroups, NodesAt};
use engine::repair::Group;
use freenet_prolly::{block_id, kind, parity, Cid};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What a pass may repair (KEEPER §3): the asset's `keep` policy, owned here (the page acts on it; `keep::Repair` is
/// this type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repair {
    /// Watch only.
    Off,
    /// Keep BACKED_UP: repair anything missing.
    Always,
    /// Repair a group once its margin is below N.
    Below(u8),
}

impl Repair {
    /// Does this policy repair a group with `margin` blocks to spare of its `parity`? (One decision; the group must also
    /// have k.)
    pub fn repairs(self, margin: i64, parity: usize) -> bool {
        match self {
            Repair::Off => false,
            Repair::Always => margin < parity as i64,
            Repair::Below(n) => margin < i64::from(n),
        }
    }
}

/// A block's state in this pass (KEEPER §5, the block table). A block with NO state was walked and not yet asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// `Held` said the node does not hold it: its GET is owed.
    NotHeld,
    /// The node holds it (`Held`).
    Held,
    /// Not held; its GET brought it, verified (the node holds it again: F6).
    Fetched,
    /// Not held, and its GET answered NotFound (or bytes that are not it).
    Absent,
    /// A GET of it was silent at its deadline: unknown, never repaired; the next pass asks again.
    Pending,
    /// The node's Block contract REJECTED it (sdk#433): absent toward its group, never asked, fetched or PUT.
    Rejected,
    /// Its repair PUT is out, not yet answered.
    Putting { bytes: u32 },
    /// Its repair PUT was acked: the node holds it again.
    Repaired { bytes: u32 },
}

impl Seen {
    /// Does the node have it (toward its group's margin)?
    fn held(self) -> bool {
        match self {
            Seen::Held | Seen::Fetched | Seen::Repaired { .. } => true,
            Seen::NotHeld | Seen::Absent | Seen::Pending | Seen::Rejected | Seen::Putting { .. } => false,
        }
    }
}

/// One group the walk found.
#[derive(Clone, Debug)]
pub(crate) struct Grp {
    pub members: Vec<Cid>,
    pub parity: Vec<Cid>,
    /// A leaf's group (its values, `RAW`) or a branch's (its children, `TREE_NODE`; the root's group of one too).
    pub leaf: bool,
}

impl Grp {
    /// The group as a repair takes it (`engine::repair::Group`), rebuilding the slot at `missing_ix`.
    fn as_repair(&self, missing_ix: usize) -> Group {
        let mut slots = self.members.clone();
        slots.extend_from_slice(&self.parity);
        Group {
            missing: slots[missing_ix],
            missing_ix,
            kind: if self.leaf { kind::RAW } else { kind::TREE_NODE },
            slots,
            k: self.members.len(),
            max_len: if self.leaf { parity::MAX_MEMBER_VALUE } else { parity::MAX_MEMBER_NODE },
        }
    }

    fn slots(&self) -> impl Iterator<Item = &Cid> {
        self.members.iter().chain(self.parity.iter())
    }
}

/// The group being repaired (KEEPER §5, `Repairing{next, job}`).
#[derive(Clone, Debug)]
pub(crate) enum Job {
    /// Gathering k slots' bytes: from the page's store at once, then GET, one held slot at a time.
    Gathering { group: usize, have: BTreeMap<Cid, Vec<u8>> },
    /// Built: its blocks still to PUT, in order.
    Putting { group: usize, puts: VecDeque<(Cid, Vec<u8>)> },
}

/// The pass's phase (KEEPER §5, the phase table). The ENDED phases are the page's: a pass that ends hands its report to
/// the page ([`Act::Finish`]) and is gone.
#[derive(Clone, Debug)]
pub(crate) enum Phase {
    /// Reading the walk: where its next page starts, and the read in flight (its engine request id), if any.
    Walking { at: Option<NodesAt>, reading: Option<u64> },
    Asking,
    Getting,
    /// Repairing: the next group index to consider, and the group in hand.
    Repairing { next: usize, job: Option<Job> },
}

/// What happens to a pass (KEEPER §5's columns).
#[derive(Debug)]
pub(crate) enum Ev {
    /// The Background lane's slot is free (K1): the pass may send its next op.
    Free,
    /// The walk's read answered (request id, its nodes, where the next page starts, the root's group-of-one parity when
    /// the root is on this page).
    WalkPage { req: u64, nodes: Vec<NodeGroups>, next: Option<NodesAt>, root_parity: Option<Vec<Cid>> },
    /// The walk's read could not be served.
    WalkUnusable { req: u64, said: String },
    /// A `Held` batch's answer: each id, and `Some(present)` or `None` (past a short answer).
    Held(Vec<(Cid, Option<bool>)>),
    /// `HeldUnasked`: impossible in a started pass (KEEPER §5 ¹⁰: the signer is known at START).
    HeldUnasked,
    /// A GET's answer: bytes that are the id (`true`) or not, with the bytes.
    Got { id: Cid, good: bool, bytes: Vec<u8> },
    Missed(Cid),
    /// A GET silent at its deadline.
    Silent(Cid),
    PutOk(Cid),
    /// A repair PUT refused; its bytes (the op's), for the PUT again.
    PutRefused { id: Cid, transient: bool, bytes: Vec<u8> },
    /// A repair PUT's deadline passed unanswered; its bytes, for the PUT again.
    PutDeadline { id: Cid, bytes: Vec<u8> },
}

/// What the pass asks the page to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Act {
    /// Read the walk's next page (request id, its spec).
    Walk { req: u64, at: Option<NodesAt> },
    AskHeld(Vec<Cid>),
    Get(Cid),
    Put(Cid, Vec<u8>),
    /// The node REJECTED this block (the engine's one record, `Event::PutRejected`).
    Rejected(Cid),
    /// Something to say (`unusable`): a walk that ended, a block that could not be rebuilt.
    Say(String),
    /// The pass is over: the page keeps its report.
    Finish,
}

/// What the pass reads from the page, never writes: its store (verified blocks), the engine's rejected record, and the
/// batch bound.
pub(crate) struct Ctx<'a> {
    pub stored: &'a dyn Fn(&Cid) -> Option<Vec<u8>>,
    pub rejected: &'a BTreeSet<Cid>,
    pub max_held: usize,
}

/// A pass in progress.
pub(crate) struct Audit {
    root: Cid,
    policy: Repair,
    started_at: u64,
    groups: Vec<Grp>,
    seen: BTreeMap<Cid, Seen>,
    phase: Phase,
    /// The walk's request ids, allocated in order.
    reqs: u64,
    /// Events that arrived in a cell the table calls impossible: 0 in a correct run (a model test asserts it).
    impossible: usize,
}

impl Audit {
    /// A pass at `root`, walking (KEEPER §5: a page WITH a signer; one without never starts one).
    pub fn new(root: Cid, policy: Repair, now: u64) -> Audit {
        Audit {
            root,
            policy,
            started_at: now,
            groups: Vec::new(),
            seen: BTreeMap::new(),
            phase: Phase::Walking { at: None, reading: None },
            reqs: 0,
            impossible: 0,
        }
    }

    /// THE TRANSITION FUNCTION (KEEPER §5): with [`Audit::step`], the only writer of this pass's state. Returns what the
    /// page must do.
    pub fn on(&mut self, ev: Ev, cx: &Ctx) -> Vec<Act> {
        let phase = std::mem::replace(&mut self.phase, Phase::Asking);
        let (phase, mut acts, again) = self.step(phase, ev, cx);
        self.phase = phase;
        // A phase that changed on `Free` takes its own first `Free` at once (an empty phase passes straight through).
        if again {
            acts.extend(self.on(Ev::Free, cx));
        }
        acts
    }

    /// One cell of the phase table (and the block table's, through [`Audit::set`]): (the next phase, the acts, whether
    /// the next phase takes a `Free` now).
    fn step(&mut self, phase: Phase, ev: Ev, cx: &Ctx) -> (Phase, Vec<Act>, bool) {
        let mut acts = Vec::new();
        let next = match (phase, ev) {
            // ---- Walking
            (Phase::Walking { reading: None, at }, Ev::Free) => {
                self.reqs += 1;
                acts.push(Act::Walk { req: self.reqs, at: at.clone() });
                Phase::Walking { at, reading: Some(self.reqs) }
            }
            (p @ Phase::Walking { reading: Some(_), .. }, Ev::Free) => p,
            (Phase::Walking { reading: Some(r), at }, Ev::WalkPage { req, nodes, next, root_parity }) => {
                if r != req {
                    self.impossible += 1;
                    return (Phase::Walking { reading: Some(r), at }, acts, false);
                }
                if let Some(parity) = root_parity {
                    self.add_group(vec![self.root], parity, false, cx.rejected);
                }
                for n in nodes {
                    for (members, parity) in n.groups {
                        self.add_group(members, parity, n.level == 0, cx.rejected);
                    }
                }
                let next = match next {
                    Some(at) => Phase::Walking { at: Some(at), reading: None },
                    None => Phase::Asking,
                };
                return (next, acts, true);
            }
            (Phase::Walking { reading: Some(r), at }, Ev::WalkUnusable { req, said }) => {
                if r != req {
                    self.impossible += 1;
                    return (Phase::Walking { reading: Some(r), at }, acts, false);
                }
                acts.push(Act::Say(format!("the assets audit's walk ended without its nodes: {said}")));
                return (Phase::Asking, acts, true);
            }
            // ---- Asking
            (Phase::Asking, Ev::Free) => {
                let ask: Vec<Cid> = self.to_ask().into_iter().take(cx.max_held).collect();
                if ask.is_empty() {
                    return (Phase::Getting, acts, true);
                }
                acts.push(Act::AskHeld(ask));
                Phase::Asking
            }
            (Phase::Asking, Ev::Held(answers)) => {
                for (id, present) in answers {
                    match present {
                        Some(true) => self.set(id, Seen::Held),
                        Some(false) => self.set(id, Seen::NotHeld),
                        // Past a short answer: no state, so it is asked again (derived).
                        None => {}
                    }
                }
                Phase::Asking
            }
            // ---- Getting
            (Phase::Getting, Ev::Free) => match self.seen.iter().find(|(_, s)| **s == Seen::NotHeld) {
                Some((id, _)) => {
                    acts.push(Act::Get(*id));
                    Phase::Getting
                }
                None => return (Phase::Repairing { next: 0, job: None }, acts, true),
            },
            (Phase::Getting, Ev::Got { id, good, .. }) if self.seen.get(&id) == Some(&Seen::NotHeld) => {
                self.set(id, if good { Seen::Fetched } else { Seen::Absent });
                Phase::Getting
            }
            (Phase::Getting, Ev::Missed(id)) if self.seen.get(&id) == Some(&Seen::NotHeld) => {
                self.set(id, Seen::Absent);
                Phase::Getting
            }
            (Phase::Getting, Ev::Silent(id)) if self.seen.get(&id) == Some(&Seen::NotHeld) => {
                self.set(id, Seen::Pending);
                Phase::Getting
            }
            // ---- Repairing: no job -> the next group the policy names, or the pass is over.
            (Phase::Repairing { next, job: None }, Ev::Free) => match (next..self.groups.len()).find(|&g| self.names(g)) {
                None => {
                    acts.push(Act::Finish);
                    Phase::Repairing { next, job: None }
                }
                Some(group) => {
                    let have = self.groups[group].slots().filter_map(|id| (cx.stored)(id).map(|b| (*id, b))).collect();
                    return (Phase::Repairing { next: group + 1, job: Some(Job::Gathering { group, have }) }, acts, true);
                }
            },
            // Gathering: a held slot's bytes still needed -> GET it; else build the group's PUTs.
            (Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }, Ev::Free) => match self.slot_to_fetch(group, &have) {
                Some(id) => {
                    acts.push(Act::Get(id));
                    Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }
                }
                None => {
                    let (puts, said) = self.build(group, &have);
                    acts.extend(said.into_iter().map(Act::Say));
                    return (Phase::Repairing { next, job: Some(Job::Putting { group, puts }) }, acts, true);
                }
            },
            (Phase::Repairing { next, job: Some(Job::Gathering { group, mut have }) }, Ev::Got { id, good, bytes }) if self.slot_of(group, &id) => {
                if good {
                    have.insert(id, bytes);
                } else {
                    self.set(id, Seen::Absent);
                }
                Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }
            }
            (Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }, Ev::Missed(id)) if self.slot_of(group, &id) => {
                self.set(id, Seen::Absent);
                Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }
            }
            (Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }, Ev::Silent(id)) if self.slot_of(group, &id) => {
                self.set(id, Seen::Pending);
                Phase::Repairing { next, job: Some(Job::Gathering { group, have }) }
            }
            // Putting: the next block out; none left -> the next group.
            (Phase::Repairing { next, job: Some(Job::Putting { group, mut puts }) }, Ev::Free) => match puts.pop_front() {
                Some((id, bytes)) => {
                    self.set(id, Seen::Putting { bytes: bytes.len() as u32 });
                    acts.push(Act::Put(id, bytes));
                    Phase::Repairing { next, job: Some(Job::Putting { group, puts }) }
                }
                None => return (Phase::Repairing { next, job: None }, acts, true),
            },
            // ---- The block table's PUT columns (a withdrawn PUT's answer never reaches a pass: KEEPER §5 ⁹).
            (p, Ev::PutOk(id)) if self.is_putting(&id) => {
                let bytes = match self.seen[&id] {
                    Seen::Putting { bytes } => bytes,
                    Seen::NotHeld | Seen::Held | Seen::Fetched | Seen::Absent | Seen::Pending | Seen::Rejected | Seen::Repaired { .. } => 0,
                };
                self.set(id, Seen::Repaired { bytes });
                p
            }
            (p, Ev::PutRefused { id, transient: false, .. }) if self.is_putting(&id) => {
                self.set(id, Seen::Rejected);
                acts.push(Act::Rejected(id));
                p
            }
            // Transient refusal, or its deadline: the same PUT again (rule 7), while the pass lives.
            (p, Ev::PutRefused { id, transient: true, bytes } | Ev::PutDeadline { id, bytes }) if self.is_putting(&id) => {
                acts.push(Act::Put(id, bytes));
                p
            }
            // ---- Every other cell is one the tables call impossible: counted, never acted on. Every phase and every
            // event but `Free` (placed in every phase above) is NAMED here, so a new case of either does not compile
            // until the tables place it.
            (
                p @ (Phase::Walking { .. } | Phase::Asking | Phase::Getting | Phase::Repairing { .. }),
                Ev::WalkPage { .. }
                | Ev::WalkUnusable { .. }
                | Ev::Held(_)
                | Ev::HeldUnasked
                | Ev::Got { .. }
                | Ev::Missed(_)
                | Ev::Silent(_)
                | Ev::PutOk(_)
                | Ev::PutRefused { .. }
                | Ev::PutDeadline { .. },
            ) => {
                self.impossible += 1;
                p
            }
        };
        (next, acts, false)
    }

    // ---- The one writer's helpers: called only from `on` (they are not `&mut self` methods of their own).

    fn set(&mut self, id: Cid, s: Seen) {
        self.seen.insert(id, s);
    }

    fn add_group(&mut self, members: Vec<Cid>, parity: Vec<Cid>, leaf: bool, rejected: &BTreeSet<Cid>) {
        for id in members.iter().chain(parity.iter()) {
            if rejected.contains(id) {
                self.seen.insert(*id, Seen::Rejected);
            }
        }
        self.groups.push(Grp { members, parity, leaf });
    }

    // ---- DERIVED (KEEPER §5): read the two states, write nothing.

    /// The blocks walked and not yet asked, in walk order, each once.
    fn to_ask(&self) -> Vec<Cid> {
        let mut seen_now = BTreeSet::new();
        self.groups.iter().flat_map(Grp::slots).filter(|id| !self.seen.contains_key(*id) && seen_now.insert(**id)).copied().collect()
    }

    /// A group's blocks to spare: held (or fetched, or repaired) minus k.
    fn margin(&self, g: &Grp) -> i64 {
        g.slots().filter(|id| self.seen.get(*id).is_some_and(|s| s.held())).count() as i64 - g.members.len() as i64
    }

    /// Does the policy name group `g`? At least one block known ABSENT, k to rebuild from, and the policy's margin.
    fn names(&self, g: usize) -> bool {
        let grp = &self.groups[g];
        let margin = self.margin(grp);
        margin >= 0 && grp.slots().any(|id| self.seen.get(id) == Some(&Seen::Absent)) && self.policy.repairs(margin, grp.parity.len())
    }

    /// The next held slot of `group` whose bytes a rebuild still needs, if any: none once every absent block is one the
    /// page holds, or the job has k.
    fn slot_to_fetch(&self, group: usize, have: &BTreeMap<Cid, Vec<u8>>) -> Option<Cid> {
        let g = &self.groups[group];
        let absent_all_held_here = g.slots().filter(|id| self.seen.get(*id) == Some(&Seen::Absent)).all(|id| have.contains_key(id));
        if absent_all_held_here || g.slots().filter(|id| have.contains_key(*id)).count() >= g.members.len() {
            return None;
        }
        g.slots().find(|id| !have.contains_key(*id) && self.seen.get(*id).is_some_and(|s| matches!(s, Seen::Held | Seen::Fetched))).copied()
    }

    /// The PUTs for `group`'s ABSENT blocks: the page's own bytes when it holds them, else a member REBUILT
    /// (`repair::rebuild`, hash-checked) or a parity RECOMPUTED (id-checked). With fewer than k slots nothing is rebuilt
    /// (the group is below k, and the report says damaged).
    fn build(&self, group: usize, have: &BTreeMap<Cid, Vec<u8>>) -> (VecDeque<(Cid, Vec<u8>)>, Vec<String>) {
        let g = &self.groups[group];
        let rg = g.as_repair(0);
        let can_solve = g.slots().filter(|id| have.contains_key(*id)).count() >= rg.k;
        let slots: Vec<Option<Vec<u8>>> = rg.slots.iter().enumerate().map(|(i, id)| have.get(id).map(|b| rg.stored(i, b))).collect();
        let (mut puts, mut said) = (VecDeque::new(), Vec::new());
        let mut parity_blocks: Option<Vec<Vec<u8>>> = None;
        for (i, id) in rg.slots.iter().enumerate() {
            if self.seen.get(id) != Some(&Seen::Absent) {
                continue;
            }
            if let Some(b) = have.get(id) {
                puts.push_back((*id, b.clone()));
            } else if !can_solve {
            } else if !rg.is_parity(i) {
                match engine::repair::rebuild(&g.as_repair(i), &slots) {
                    Ok(body) => puts.push_back((*id, body)),
                    Err(e) => said.push(format!("the assets audit could not rebuild member {}: {e}", engine::short_id(id))),
                }
            } else {
                let p = parity_blocks.get_or_insert_with(|| match parity::repair_group(rg.k, &slots, rg.max_len).map(|st| parity::encode_group(&st)) {
                    Ok(Ok(p)) => p,
                    other => {
                        said.push(format!("the assets audit could not recompute group parity: {:?}", other.err()));
                        Vec::new()
                    }
                });
                match p.get(i - rg.k) {
                    Some(p) if block_id(kind::PARITY, p) == *id => puts.push_back((*id, p.clone())),
                    Some(_) => said.push(format!("the assets audit's recomputed parity is not {}: never PUT", engine::short_id(id))),
                    None => {}
                }
            }
        }
        (puts, said)
    }

    /// Is `id` a slot of group `group`?
    fn slot_of(&self, group: usize, id: &Cid) -> bool {
        self.groups[group].slots().any(|s| s == id)
    }

    /// Is `id`'s repair PUT out?
    fn is_putting(&self, id: &Cid) -> bool {
        matches!(self.seen.get(id), Some(Seen::Putting { .. }))
    }

    /// Are `bytes` the block `id` names? Checked by its SLOT's kind (`repair::Group::fits`).
    pub fn fits(&self, id: &Cid, bytes: &[u8]) -> bool {
        self.groups.iter().find_map(|g| g.slots().position(|s| s == id).map(|i| g.as_repair(i).fits(i, bytes))).unwrap_or(false)
    }

    /// Progress: (blocks settled so far, blocks the walk has reached).
    pub fn progress(&self) -> (usize, usize) {
        let reached: BTreeSet<&Cid> = self.groups.iter().flat_map(Grp::slots).collect();
        (self.seen.len(), reached.len())
    }

    /// The root this pass walks (the page's published root when it began).
    pub fn root(&self) -> Cid {
        self.root
    }

    /// Events that arrived in an impossible cell.
    pub fn impossible(&self) -> usize {
        self.impossible
    }

    /// The report, from the two states, as of `now` (the page's clock). `pending` = `Pending` + `Putting` (a repair PUT
    /// has no deadline: it is pending only when the pass was cancelled or replaced).
    pub fn report(&self, now: u64) -> Report {
        let mut r = Report {
            root: self.root,
            measured: true,
            health: Health::Unmeasured,
            groups: self.groups.len(),
            whole: 0,
            degraded: 0,
            damaged: Vec::new(),
            margins: BTreeMap::new(),
            pending: 0,
            rejected: Vec::new(),
            repaired: 0,
            bytes: 0,
            started_at: self.started_at,
            finished_at: now,
        };
        for g in &self.groups {
            let margin = self.margin(g);
            *r.margins.entry(margin).or_default() += 1;
            if margin >= g.parity.len() as i64 {
                r.whole += 1;
            } else if margin >= 0 {
                r.degraded += 1;
            } else {
                r.damaged.push(g.parity.first().or(g.members.first()).copied().unwrap_or([0; 32]));
            }
        }
        for (id, s) in &self.seen {
            match s {
                Seen::Pending | Seen::Putting { .. } => r.pending += 1,
                Seen::Rejected => r.rejected.push(*id),
                Seen::Repaired { bytes } => {
                    r.repaired += 1;
                    r.bytes += u64::from(*bytes);
                }
                Seen::NotHeld | Seen::Held | Seen::Fetched | Seen::Absent => {}
            }
        }
        r.health = Health::of(r.damaged.len(), r.degraded);
        r
    }

    /// A signer-less page's report (KEEPER §5 ¹⁰): UNMEASURED, from nothing -- no walk, no op.
    pub fn unmeasured(root: Cid, now: u64) -> Report {
        Report {
            root,
            measured: false,
            health: Health::Unmeasured,
            groups: 0,
            whole: 0,
            degraded: 0,
            damaged: Vec::new(),
            margins: BTreeMap::new(),
            pending: 0,
            rejected: Vec::new(),
            repaired: 0,
            bytes: 0,
            started_at: now,
            finished_at: now,
        }
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
    /// Blocks this pass could not settle: a GET silent at its deadline, or a repair PUT still unanswered when the pass
    /// was cancelled or replaced (a repair PUT has no deadline: rules 7/8).
    pub pending: usize,
    /// Blocks this pass PUT again and the node acked, and their bytes (KEEPER §5).
    pub repaired: usize,
    pub bytes: u64,
    /// Blocks the node REJECTED (sdk#433): each counted absent in its group, never repaired (KEEPER §5). `damaged`
    /// keeps its one meaning -- groups whose margin is below 0.
    pub rejected: Vec<Cid>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a pass through [`Audit::on`] to its `Finish`, as the page would: every op answered from `node`, every PUT
    /// acked. Returns (the PUTs, what it said).
    fn run(a: &mut Audit, node: &BTreeMap<Cid, Vec<u8>>, stored: &dyn Fn(&Cid) -> Option<Vec<u8>>) -> (Vec<(Cid, Vec<u8>)>, Vec<String>) {
        let rejected = BTreeSet::new();
        let cx = Ctx { stored, rejected: &rejected, max_held: 128 };
        let (mut puts, mut said, mut ev) = (Vec::new(), Vec::new(), Ev::Free);
        for _ in 0..1_000 {
            let mut next = Ev::Free;
            for act in a.on(ev, &cx) {
                match act {
                    Act::Walk { .. } => panic!("THE SETUP: the walk is fed by the test"),
                    Act::AskHeld(ids) => next = Ev::Held(ids.iter().map(|id| (*id, Some(node.contains_key(id)))).collect()),
                    Act::Get(id) => {
                        next = match node.get(&id) {
                            Some(b) => Ev::Got { id, good: true, bytes: b.clone() },
                            None => Ev::Missed(id),
                        }
                    }
                    Act::Put(id, bytes) => {
                        puts.push((id, bytes));
                        next = Ev::PutOk(id);
                    }
                    Act::Say(s) => said.push(s),
                    Act::Rejected(_) => {}
                    Act::Finish => return (puts, said),
                }
            }
            ev = next;
        }
        panic!("the pass never finished");
    }

    /// **A recomputed parity block goes out only under its listed id** (the architect's D1), driven through the ONE
    /// transition function: a leaf group of 3 members whose m parity ids are the real ones but the third, which is
    /// FORGED; every parity block absent from the node. The real ones are PUT with exactly `encode_group`'s bytes; the
    /// forged one is never PUT, and it is said.
    #[test]
    fn a_recomputed_parity_block_goes_out_only_under_its_listed_id() {
        let values: Vec<Vec<u8>> = (0..3u8).map(|i| vec![i; 100]).collect();
        let members: Vec<Cid> = values.iter().map(|v| block_id(kind::RAW, v)).collect();
        let states: Vec<Vec<u8>> = values.iter().map(|v| [&[kind::RAW][..], v].concat()).collect();
        let published = parity::encode_group(&states).expect("encodes");
        let mut ids: Vec<Cid> = published.iter().map(|p| block_id(kind::PARITY, p)).collect();
        ids[2] = [0xee; 32];
        let node: BTreeMap<Cid, Vec<u8>> = members.iter().copied().zip(values.iter().cloned()).collect();
        let mut a = Audit::new([1; 32], Repair::Always, 0);
        let rejected = BTreeSet::new();
        let none = |_: &Cid| None;
        let cx = Ctx { stored: &none, rejected: &rejected, max_held: 128 };
        assert!(matches!(a.on(Ev::Free, &cx).as_slice(), [Act::Walk { req: 1, .. }]), "THE SETUP: the pass did not walk");
        let page = vec![NodeGroups { id: [9; 32], level: 0, groups: vec![(members.clone(), ids.clone())] }];
        let mut first = a.on(Ev::WalkPage { req: 1, nodes: page, next: None, root_parity: None }, &cx);
        let Some(Act::AskHeld(asked)) = first.pop() else { panic!("THE SETUP: the pass did not ask Held") };
        let _ = a.on(Ev::Held(asked.iter().map(|id| (*id, Some(node.contains_key(id)))).collect()), &cx);
        let (puts, said) = run(&mut a, &node, &none);
        let real: Vec<(Cid, Vec<u8>)> = ids.iter().copied().zip(published.iter().cloned()).enumerate().filter(|(i, _)| *i != 2).map(|(_, p)| p).collect();
        assert_eq!(puts, real, "the real parity was not PUT byte for byte");
        assert!(!puts.iter().any(|(id, _)| *id == ids[2]), "a recompute was PUT under an id it does not hash to");
        assert_eq!(said.len(), 1, "the forged id was not said: {said:?}");
        assert_eq!(a.impossible(), 0, "the pass met an impossible cell");
    }

    /// **ONE WRITER** (KEEPER §5; "Structure before code"): in this file only the transition function and its own
    /// helpers take `&mut self` -- every other method reads the two states. (The fields are private, so nothing outside
    /// this file can write them at all.)
    #[test]
    fn only_the_transition_function_writes_the_pass() {
        let src = include_str!("audit.rs");
        let writers: Vec<&str> = src
            .lines()
            .filter(|l| l.contains("(&mut self") && (l.trim_start().starts_with("fn ") || l.trim_start().starts_with("pub fn ")))
            .filter_map(|l| l.split("fn ").nth(1).and_then(|r| r.split('(').next()))
            .collect();
        assert_eq!(writers, vec!["on", "step", "set", "add_group"], "a method other than the transition function writes the pass");
    }

    /// **The word from counts**: one damaged group outranks any number degraded; none of either is whole.
    #[test]
    fn a_damaged_group_outranks_degraded_ones_and_none_is_whole() {
        assert_eq!(Health::of(1, 5), Health::Damaged);
        assert_eq!(Health::of(0, 1), Health::Degraded);
        assert_eq!(Health::of(0, 0), Health::Whole);
    }
}
