// NO CATCH-ALL over the record's enums (CLAUDE.md "Structure before code"): a new state, event or key must fail to
// compile until each match handles it. Both lints: `_`/`other =>` over several left, and `_` over one left.
#![deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]
//! THE OP RECORD (craftworks-docs/docs/design/OP-LIFE.md): what the page waits on, per op key (`Need`), and the ONE
//! background send the node still holds that no need rides (`Held`). Both are written ONLY by [`Page::on`], the
//! transition function of the document's two tables: the record's fields are private to this module, so nothing else
//! in the crate can write them, and a source test (tests/one_site.rs) holds the one door. Everything the rest of the
//! page asks of an op -- is it on the wire, which place does it hold, is it due, how long has it waited -- is DERIVED
//! here from the record, never kept beside it.
use crate::*;

/// One send the node holds (DEADLINES.md owns its timing: `at`, `armed`, the sample rules).
#[derive(Debug, Clone)]
pub(crate) struct Send {
    pub(crate) op: Op,
    pub(crate) at: u64,
    /// The deadline it was given when SENT: a re-arm never moves it past this (sdk#378).
    pub(crate) armed: u64,
    pub(crate) sent_at: u64,
    pub(crate) attempt: u32,
    /// Re-sent on a NEW socket after a reconnect (sdk#376): no RTT sample (Karn).
    pub(crate) resent: bool,
    /// Its place in the page's SEND ORDER: the recording's label, and its place in the node's one queue (F61).
    pub(crate) seq: u32,
    /// Sent into an EMPTY queue: the only answer that is an RTT sample (sdk#390).
    pub(crate) alone: bool,
    /// A GET past its deadline whose node GET may still be running (sdk#447): no place in `ahead`, never re-armed,
    /// no sample.
    pub(crate) silent: bool,
}

/// WHAT THE PAGE WAITS ON for one op key: OP-LIFE.md's Table 1. There is no entry for an `Absent` need. Each case
/// carries its own data; no flag says what the case already says.
#[derive(Debug, Clone)]
pub(crate) enum Need {
    /// Waiting for a PLACE: an interactive GET behind the full window, or a background need behind the slot (a
    /// background need queued on the held send's key is that send's SUCCESSOR, and goes first). `attempt`: the sends
    /// made so far (0: never sent; ≥ 1: its send was LOST, or it timed out and yielded -- the next send is attempt+1).
    /// Also, within ONE tick only, an interactive non-GET that timed out, waiting for its owner's rebuilt re-send.
    Queued { lane: Lane, attempt: u32, op: Op, order: u64, first_at: Option<u64> },
    /// On the wire: it holds a window place (an interactive GET) or THE slot (background).
    OnWire { lane: Lane, send: Send, first_at: u64 },
    /// Answered without what it waits for (NotFound, bytes that are not its id, a head below the floor, a PUT
    /// refused transiently): asked again at `at`, on its own back-off. Not on the wire; coming due is no timeout.
    Parked { lane: Lane, at: u64, attempt: u32, op: Op, first_at: u64 },
}

impl Need {
    pub(crate) fn lane(&self) -> Lane {
        match self {
            Need::Queued { lane, .. } | Need::OnWire { lane, .. } | Need::Parked { lane, .. } => *lane,
        }
    }

    pub(crate) fn op(&self) -> &Op {
        match self {
            Need::Queued { op, .. } | Need::Parked { op, .. } => op,
            Need::OnWire { send, .. } => &send.op,
        }
    }

    /// The send on the wire, if it is on the wire.
    pub(crate) fn send(&self) -> Option<&Send> {
        match self {
            Need::OnWire { send, .. } => Some(send),
            Need::Queued { .. } | Need::Parked { .. } => None,
        }
    }

    /// Attempts made so far.
    pub(crate) fn attempt(&self) -> u32 {
        match self {
            Need::Queued { attempt, .. } | Need::Parked { attempt, .. } => *attempt,
            Need::OnWire { send, .. } => send.attempt,
        }
    }

    fn first_at(&self) -> Option<u64> {
        match self {
            Need::Queued { first_at, .. } => *first_at,
            Need::OnWire { first_at, .. } | Need::Parked { first_at, .. } => Some(*first_at),
        }
    }

    /// When it is next due (a deadline on the wire, a parked op's re-ask), if it has one.
    pub(crate) fn due(&self) -> Option<u64> {
        match self {
            Need::Queued { .. } => None,
            Need::OnWire { send, .. } => Some(send.at),
            Need::Parked { at, .. } => Some(*at),
        }
    }
}

/// A BACKGROUND send the node still holds that no need rides: withdrawn, superseded, or re-sent after a reconnect
/// (OP-LIFE.md Table 2). `answerable` is false once a reconnect replaced the socket that carried it.
#[derive(Debug, Clone)]
pub(crate) struct Held {
    pub(crate) send: Send,
    pub(crate) answerable: bool,
}

/// Which fact a send the node holds belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    /// An on-wire need's.
    Need,
    /// The held background send.
    Held,
    /// A stale interactive send.
    Stale,
}

/// Where a send counts: a window place, THE slot, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Window,
    Slot,
    None,
}

/// THE RECORD. Its fields are private to this module: only [`Page::on`] writes them.
#[derive(Debug)]
pub(crate) struct Ops {
    need: BTreeMap<Waiting, Need>,
    /// PAGE-level, so "at most one held send" (L1) is a type (the architect).
    held: Option<(Waiting, Held)>,
    /// INTERACTIVE sends the page stopped waiting on (withdrawn, or superseded) that the node may still answer, per
    /// key, oldest first (OP-LIFE.md's tenth cell). They hold NOTHING -- no window place, no slot, no place in `ahead`
    /// (DEADLINES' withdraw row) -- and only remember that the key's next answer may be theirs (C1), or that a new need
    /// may ADOPT one (one node GET per key). Each leaves at its answer or its deadline, its end recorded then.
    stale: BTreeMap<Waiting, Vec<Send>>,
    /// GETs LOST past the node's bound and asked again (sdk#447): when, and whether one answer has come since. It
    /// outlives the need (a SECOND answer, arriving when nothing waits, is the known overlap, counted).
    reasked: BTreeMap<Cid, (u64, bool)>,
    /// The next queue place.
    order: u64,
    /// N2 (the architect's ruling): per RECURRING key, the sends the page stopped counting while they could still
    /// answer -- lost at a need's deadline, a held send released at its deadline, a stale send expired. Each is an
    /// answer that may yet arrive; the key's next answer is taken as one of them. A GET's entry carries when the node's
    /// own GET is certainly over (it decays then); any other's has none. At most [`UNTRACKED_KEYS_MAX`] keys: past it
    /// the oldest key's entries are dropped and counted (`untracked_evicted`) -- the one stated residual.
    untracked: BTreeMap<Waiting, Untracked>,
    /// Keys whose untracked sends were dropped at the cap: a diagnostic, never silent.
    untracked_evicted: u64,
    /// The most keys N2 has remembered at once (a diagnostic: the cap's headroom).
    untracked_peak: usize,
    /// Orphans N2 made (a need served by an older send's late answer), by send ordinal. They are ordinary orphans --
    /// they take their own answer (C1) and may be adopted -- but LEAVE EARLY (the architect's rule (A)): if the node
    /// DROPPED the older send, the answer was really theirs, and a need must not wait a whole deadline for it. One that
    /// leaves by its deadline, unanswered, is a drop's evidence: counted (`early_left`).
    n2_orphans: BTreeSet<u32>,
    /// N2 orphans that left by their (shortened) deadline, unanswered: how often the node dropped a request it took.
    early_left: u64,
    /// The early leaves of the last tick, by send ordinal, for the gate's check that they ended nothing.
    #[cfg(test)]
    early_left_seqs: Vec<u32>,
}

/// The cap on keys N2 remembers (the architect: bounded memory). Label keys are few; a Put per block over a long
/// session is what reaches it.
pub(crate) const UNTRACKED_KEYS_MAX: usize = 256;

/// One key's untracked sends: when the key first entered the map (the eviction order), and each send.
#[derive(Debug, Clone)]
pub(crate) struct Untracked {
    since: u64,
    sends: Vec<Lost>,
}

/// An untracked send: its place in the send order (C1 compares it), the request id it carries (the named row), and
/// when it decays (`Some` for a GET: the node's GET is over then; `None` for any other).
#[derive(Debug, Clone)]
pub(crate) struct Lost {
    seq: u32,
    request: Option<u32>,
    decay: Option<u64>,
    /// Its End is NOT recorded yet: an N2 orphan that left EARLY -- the early leave ends nothing (the architect) -- so
    /// its End is recorded when it really leaves: its answer, its decay, a reconnect or the cap.
    open: bool,
}

/// Does N2 remember this key's lost sends? Only a key that RECURS -- a later need can have it, and so meet an earlier
/// send's late answer (a `Held` batch number is never reused) -- AND can be BACKGROUND work, the lane whose one-send
/// promise (L1) N2 keeps exact. The data tree's head keys are interactive only (`lane_of`), and a head register's answer
/// is not one send's (one `Answer::Head` ends every head read), so routing it by send order would starve them; the app's
/// PUT and the signer's setup are interactive only.
pub(crate) fn recurs(w: &Waiting) -> bool {
    match w {
        Waiting::Put(_) | Waiting::Get(_) => true,
        Waiting::Sign(l) | Waiting::Update(l) | Waiting::ReadBack(l) => !matches!(l, Label::Head),
        Waiting::Held(_) | Waiting::Warm | Waiting::RecoverHead | Waiting::Verify | Waiting::Hint | Waiting::PutApp(_) | Waiting::Ext(_) => false,
    }
}

impl Ops {
    /// An empty record. No `Default`: `mem::take` of the page's record would not compile (one writer, by type).
    pub(crate) fn new() -> Ops {
        Ops {
            need: BTreeMap::new(),
            held: None,
            stale: BTreeMap::new(),
            reasked: BTreeMap::new(),
            order: 0,
            untracked: BTreeMap::new(),
            untracked_evicted: 0,
            untracked_peak: 0,
            n2_orphans: BTreeSet::new(),
            early_left: 0,
            #[cfg(test)]
            early_left_seqs: Vec::new(),
        }
    }

    /// Sends that LEFT EARLY and whose End is not recorded yet (N2): open requests in the recording, since recorder
    /// ordinal `from`.
    pub(crate) fn open_lost(&self, from: u32) -> usize {
        self.untracked.values().flat_map(|u| u.sends.iter()).filter(|l| l.open && l.seq >= from).count()
    }

    /// N2 orphans that left early by their deadline, unanswered: a node's drops (the architect's diagnostic).
    pub(crate) fn early_left(&self) -> u64 {
        self.early_left
    }

    /// Keys dropped at N2's cap, counted.
    pub(crate) fn evicted(&self) -> u64 {
        self.untracked_evicted
    }

    pub(crate) fn untracked_peak(&self) -> usize {
        self.untracked_peak
    }

    /// Untracked sends per key (N2).
    #[cfg(test)]
    pub(crate) fn untracked(&self) -> impl Iterator<Item = (&Waiting, usize)> {
        self.untracked.iter().map(|(w, u)| (w, u.sends.len()))
    }

    #[cfg(test)]
    pub(crate) fn need(&self, w: &Waiting) -> Option<&Need> {
        self.need.get(w)
    }

    pub(crate) fn contains(&self, w: &Waiting) -> bool {
        self.need.contains_key(w)
    }

    pub(crate) fn needs(&self) -> impl Iterator<Item = (&Waiting, &Need)> {
        self.need.iter()
    }

    #[cfg(test)]
    pub(crate) fn held(&self) -> Option<(&Waiting, &Held)> {
        self.held.as_ref().map(|(w, h)| (w, h))
    }

    /// Nothing waited on and nothing held in the slot (a stale send waits on nobody and holds nothing).
    pub(crate) fn is_empty(&self) -> bool {
        self.need.is_empty() && self.held.is_none()
    }

    /// The op a key's answer can be about: its need's, the held send's, or its oldest stale send's. `None`: nothing
    /// of this key is waited on or at the node.
    pub(crate) fn op_of(&self, w: &Waiting) -> Option<&Op> {
        self.need
            .get(w)
            .map(Need::op)
            .or_else(|| self.held.as_ref().filter(|(k, _)| k == w).map(|(_, h)| &h.send.op))
            .or_else(|| self.stale.get(w).and_then(|v| v.iter().min_by_key(|s| s.seq)).map(|s| &s.op))
    }

    /// Where a need's send counts (OP-LIFE.md's `place()`).
    fn place_of(w: &Waiting, n: &Need) -> Place {
        match n {
            Need::OnWire { lane: Lane::Background, .. } => Place::Slot,
            Need::OnWire { lane: Lane::Interactive, .. } if matches!(w, Waiting::Get(_)) => Place::Window,
            Need::OnWire { lane: Lane::Interactive, .. } | Need::Queued { .. } | Need::Parked { .. } => Place::None,
        }
    }

    /// EVERY send the node holds (OP-LIFE.md's `at_node()`): each on-wire need's send, the held send, and the stale
    /// sends; with which fact each belongs to and where it counts. The ONE derivation every "on the wire" question reads.
    pub(crate) fn at_node(&self) -> impl Iterator<Item = (&Waiting, &Send, Holder, Place)> {
        self.need
            .iter()
            .filter_map(|(w, n)| n.send().map(|s| (w, s, Holder::Need, Ops::place_of(w, n))))
            .chain(self.held.iter().map(|(w, h)| (w, &h.send, Holder::Held, Place::Slot)))
            .chain(self.stale.iter().flat_map(|(w, v)| v.iter().map(move |s| (w, s, Holder::Stale, Place::None))))
    }

    /// Sends holding THE slot: 0 or 1 (L1).
    pub(crate) fn in_slot(&self) -> usize {
        self.at_node().filter(|(.., p)| *p == Place::Slot).count()
    }

    /// Sends holding a window place.
    pub(crate) fn in_window(&self) -> usize {
        self.at_node().filter(|(.., p)| *p == Place::Window).count()
    }

    /// Is this key's need on the wire?
    pub(crate) fn on_wire(&self, w: &Waiting) -> bool {
        self.need.get(w).is_some_and(|n| n.send().is_some())
    }

    /// The oldest first send among needs of `lane` that have been sent (on the wire or parked), for "not answering".
    pub(crate) fn oldest_waiting(&self, lane: Lane) -> Option<(&Waiting, u64)> {
        self.need
            .iter()
            .filter(|(_, n)| n.lane() == lane && !matches!(n, Need::Queued { .. }))
            .filter_map(|(w, n)| n.first_at().map(|at| (w, at)))
            .min_by_key(|(_, at)| *at)
    }

    /// The earliest due of anything on the record (a need's, or the held send's).
    pub(crate) fn next_due(&self) -> Option<u64> {
        self.need.values().filter_map(Need::due).chain(self.held.iter().map(|(_, h)| h.send.at)).chain(self.stale.values().flatten().map(|s| s.at)).min()
    }
}

/// What happens to an op: OP-LIFE.md's events (engine-withdraw is `Withdraw`, from its three doors).
#[derive(Debug, Clone)]
pub(crate) enum OpEvent {
    /// send·I / send·B: a need, in the lane its waiters class it (`lane_of`).
    Send { op: Op, lane: Lane },
    /// join·I: its waiters now class it Interactive, with no new send.
    Join,
    /// The node answered with what it waits for.
    Answer,
    /// An answer that NAMES its send -- the signer's request id (SG02) -- and so is that send's whatever its age: the
    /// need's, when its send carries the id; else a stale send's that does (released); else nobody's. C1's order
    /// decides only answers that name no send.
    AnswerNamed { request: u32 },
    /// The node answered without it; the need parks with `op` (the same op, re-asked on its back-off).
    AnswerWithout { op: Op },
    /// Nobody needs it (the page, a person, or the engine's three doors).
    Withdraw,
}

/// The request id an op's answer NAMES it by (SG02: the signer's), if it carries one.
pub(crate) fn request_of(op: &Op) -> Option<u32> {
    match op {
        Op::Sign { id, .. } => Some(*id),
        Op::Put { .. } | Op::Get { .. } | Op::Update { .. } | Op::ReadHead { .. } | Op::AskHeld { .. } | Op::PutApp { .. } | Op::Ext(_) => None,
    }
}

/// What an event did, for the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Did {
    /// Nothing a caller acts on (a join, a queue, a drop, a withdraw).
    Nothing,
    /// The NEED was answered (or parked by an answer): its op and the attempt answered.
    Answered { op: Op, attempt: u32 },
}

/// What a need's deadline did, for the tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Due {
    /// Silent, re-queued, re-sent or ended by the record itself.
    Handled,
    /// An interactive non-GET timed out: its owner re-sends it NOW (a Sign is rebuilt under a fresh id); what it
    /// does not send ends.
    ReSend { op: Op, attempt: u32 },
}

impl Page {
    /// THE TRANSITION FUNCTION of OP-LIFE.md's two tables, for one op key: the only writer of the record, with the
    /// time events in [`Page::tick_ops`] and [`Page::reconnect_ops`] beside it. Every event ends with PLACE-FREES
    /// (L2: a free place is given in the same step).
    pub(crate) fn on(&mut self, w: &Waiting, ev: OpEvent) -> Did {
        let did = match ev {
            OpEvent::Send { op, lane } => {
                self.on_send(w, op, lane);
                Did::Nothing
            }
            OpEvent::Join => {
                self.on_join(w);
                Did::Nothing
            }
            OpEvent::Answer => self.on_answer(w, None),
            OpEvent::AnswerNamed { request } => self.on_answer_named(w, request),
            OpEvent::AnswerWithout { op } => self.on_answer(w, Some(op)),
            OpEvent::Withdraw => {
                self.on_withdraw(w);
                Did::Nothing
            }
        };
        self.place_frees();
        self.prune_uncertain();
        self.assert_pairs();
        did
    }

    /// An N2 orphan's mark leaves with it.
    fn prune_uncertain(&mut self) {
        let Ops { held, stale, n2_orphans, .. } = &mut self.ops;
        n2_orphans.retain(|seq| held.as_ref().is_some_and(|(_, h)| h.send.seq == *seq) || stale.values().flatten().any(|s| s.seq == *seq));
    }

    /// The IMPOSSIBLE PAIRS across the two facts (OP-LIFE.md): a held send beside an on-wire background need.
    fn assert_pairs(&self) {
        debug_assert!(self.ops.held.is_none() || !self.ops.need.values().any(|n| matches!(n, Need::OnWire { lane: Lane::Background, .. })), "OP-LIFE: a held send beside an on-wire background need (L1)");
        debug_assert!(self.ops.in_slot() <= 1, "OP-LIFE: {} sends in the slot (L1)", self.ops.in_slot());
    }

    /// send·I / send·B (Table 1's first two rows; Table 2's adoption).
    fn on_send(&mut self, w: &Waiting, op: Op, lane: Lane) {
        let identical = |n: &Need| matches!(w, Waiting::Get(_)) || *n.op() == op;
        match self.ops.need.get(w).cloned() {
            None => {
                // ADOPTION (Table 2): the need rides an answerable held send of the identical op.
                if let Some((hw, h)) = self.ops.held.clone() {
                    if hw == *w && h.answerable && (matches!(w, Waiting::Get(_)) || h.send.op == op) {
                        self.ops.held = None;
                        let first_at = h.send.sent_at;
                        self.ops.need.insert(w.clone(), Need::OnWire { lane, send: h.send, first_at });
                        return;
                    }
                }
                self.send_new(w, op, lane, 0, None);
            }
            Some(Need::Queued { lane: Lane::Interactive, attempt, first_at, .. }) if !matches!(w, Waiting::Get(_)) => {
                // A timed-out interactive non-GET's re-send, by its owner, in the same tick.
                self.ops.need.remove(w);
                self.send_new(w, op, Lane::Interactive, attempt, first_at);
            }
            Some(Need::Queued { lane: Lane::Interactive, .. }) => {}
            Some(Need::Queued { lane: Lane::Background, attempt, first_at, .. }) if lane == Lane::Interactive => {
                // PROMOTE: sent now as interactive work.
                self.ops.need.remove(w);
                self.send_new(w, op, Lane::Interactive, attempt, first_at);
            }
            Some(Need::Queued { lane: Lane::Background, attempt, order, first_at, .. }) => {
                // JOINS; the payload is the newest (Q2).
                self.ops.need.insert(w.clone(), Need::Queued { lane: Lane::Background, attempt, op, order, first_at });
            }
            Some(Need::OnWire { lane: Lane::Interactive, send, first_at }) => {
                // A GET JOINS (one node GET per key, sdk#447). Any other op SUPERSEDES, the same op included
                // (DEADLINES; the recording pin a_send_superseding_one_on_the_wire_closes_it_withdrawn): the send on
                // the wire ends, a new first send; it stays interactive (L6).
                if !matches!(w, Waiting::Get(_)) {
                    // The superseded send becomes STALE, not forgotten: its answer is routed by C1.
                    self.ops.need.remove(w);
                    self.ops.stale.entry(w.clone()).or_default().push(send);
                    self.put_on_wire(w, op, Lane::Interactive, 1, Some(first_at), false);
                }
            }
            Some(n @ Need::OnWire { lane: Lane::Background, .. }) => {
                let Need::OnWire { send, first_at, .. } = n.clone() else { unreachable!("matched") };
                match (identical(&n), lane) {
                    // PROMOTE: the send on the wire serves; it moves to the interactive count.
                    (true, Lane::Interactive) => {
                        self.ops.need.insert(w.clone(), Need::OnWire { lane: Lane::Interactive, send, first_at });
                    }
                    (true, Lane::Background) => {}
                    // A DIFFERENT op while the node holds this one: it is HELD (keeping the slot), and the new op is
                    // sent now as interactive work, or waits as its successor (defect 9).
                    (false, _) => {
                        self.ops.held = Some((w.clone(), Held { send, answerable: true }));
                        self.ops.need.remove(w);
                        self.send_new(w, op, lane, 0, Some(first_at));
                    }
                }
            }
            Some(Need::Parked { lane: was, at, attempt, first_at, .. }) => {
                // JOINS its wait (DEADLINES rule 5): the payload is replaced; an interactive waiter PROMOTES it.
                let lane = if lane == Lane::Interactive { Lane::Interactive } else { was };
                self.ops.need.insert(w.clone(), Need::Parked { lane, at, attempt, op, first_at });
            }
        }
    }

    /// N2: a send of `w` the page stops counting while it can still answer becomes UNTRACKED -- if its key recurs (a
    /// later need could meet its late answer). A GET's decays when the node's GET is certainly over.
    fn untrack(&mut self, w: &Waiting, send: &Send, open: bool) {
        if !recurs(w) {
            if open {
                self.record_end(w, send, End::Withdrawn);
            }
            return;
        }
        let decay = matches!(w, Waiting::Get(_)).then(|| node_get_over_at(send.sent_at, rto::RTO_MAX_MS as u64));
        if decay.is_some_and(|at| self.now >= at) {
            if open {
                self.record_end(w, send, End::Withdrawn);
            }
            return;
        }
        if !self.ops.untracked.contains_key(w) && self.ops.untracked.len() >= UNTRACKED_KEYS_MAX {
            // THE CAP: the oldest key's untracked sends are dropped, and COUNTED (the one stated residual).
            if let Some(oldest) = self.ops.untracked.iter().min_by_key(|(_, u)| u.since).map(|(k, _)| k.clone()) {
                if let Some(u) = self.ops.untracked.remove(&oldest) {
                    for l in u.sends.iter().filter(|l| l.open) {
                        self.record_end_seq(&oldest, l.seq, End::Withdrawn);
                    }
                }
                self.ops.untracked_evicted += 1;
            }
        }
        self.ops.order += 1;
        let since = self.ops.order;
        let lost = Lost { seq: send.seq, request: request_of(&send.op), decay, open };
        self.ops.untracked.entry(w.clone()).or_insert(Untracked { since, sends: Vec::new() }).sends.push(lost);
        self.ops.untracked_peak = self.ops.untracked_peak.max(self.ops.untracked.len());
    }

    /// The oldest untracked send of `w` whose request id is `request` (`None`: an unnamed answer's), by send order.
    fn oldest_untracked(&self, w: &Waiting, request: Option<u32>) -> Option<u32> {
        self.ops.untracked.get(w).and_then(|u| u.sends.iter().filter(|l| l.request == request).map(|l| l.seq).min())
    }

    /// That untracked send leaves: its late answer came. An OPEN one's End is recorded now (its answer).
    fn take_untracked(&mut self, w: &Waiting, seq: u32) {
        let open = self.ops.untracked.get(w).is_some_and(|u| u.sends.iter().any(|l| l.seq == seq && l.open));
        if open {
            self.record_end_seq(w, seq, End::Answered);
        }
        if let Some(u) = self.ops.untracked.get_mut(w) {
            u.sends.retain(|l| l.seq != seq);
            if u.sends.is_empty() {
                self.ops.untracked.remove(w);
            }
        }
    }

    /// N2: a LOST send's late answer serves the key's need. The need's own send, still on the wire, is an ORPHAN. A
    /// BACKGROUND one is HELD (it keeps the slot: L1 exact whenever the node answers what it took), and its own answer
    /// releases it (C1); by the architect's rule (A) it LEAVES EARLY -- a GET at its node bound (`node_get_over_at`), any
    /// other op after ONE RTO -- because if the node DROPPED the older send, the answer that came was really this one's;
    /// a drop so costs the key's next need at most that, never L1. An INTERACTIVE one holds no slot and so has nothing
    /// to leave early from: the need simply takes the answer (DEADLINES), and its own send is remembered as untracked.
    /// The principle (OP-LIFE N2): memory is lane-independent because it describes the NODE; orphaning is the
    /// Background lane's ACCOUNTING.
    fn orphan_the_needs_send(&mut self, w: &Waiting, lane: Lane, mut send: Send) {
        match lane {
            Lane::Background => {
                let early = if matches!(w, Waiting::Get(_)) { node_get_over_at(send.sent_at, self.rto.rto_ms()) } else { self.now + self.rto.rto_ms() };
                send.at = send.at.min(early);
                self.ops.n2_orphans.insert(send.seq);
                debug_assert!(self.ops.held.is_none(), "OP-LIFE: a held send beside an on-wire background need");
                self.ops.held = Some((w.clone(), Held { send, answerable: true }));
            }
            // An interactive need's own send counts toward nothing but the window, which its need's leaving already
            // frees: it is only REMEMBERED (untracked, its End at its real leave), so a later need of the key, a
            // background one included, cannot take its answer for its own (L1).
            Lane::Interactive => self.untrack(w, &send, true),
        }
    }

    /// The OLDEST stale send of `w` a need for `op` may adopt (the identical op: a GET is always its key's), removed.
    fn take_stale(&mut self, w: &Waiting, op: &Op) -> Option<Send> {
        let v = self.ops.stale.get_mut(w)?;
        let now = self.now;
        let i = v.iter().enumerate().filter(|(_, s)| (matches!(w, Waiting::Get(_)) || s.op == *op) && now < s.at).min_by_key(|(_, s)| s.seq).map(|(i, _)| i)?;
        let send = v.remove(i);
        if v.is_empty() {
            self.ops.stale.remove(w);
        }
        Some(send)
    }

    /// A need sent as from Absent, after `attempt` earlier sends: into its lane's place, or queued for one.
    fn send_new(&mut self, w: &Waiting, op: Op, lane: Lane, attempt: u32, first_at: Option<u64>) {
        // A STALE send it may ADOPT is already at the node: no place to wait for in the window (the slot still is).
        let adoptable = self.ops.stale.get(w).is_some_and(|v| v.iter().any(|s| (matches!(w, Waiting::Get(_)) || s.op == op) && self.now < s.at));
        let queue = match lane {
            Lane::Background => self.ops.in_slot() >= 1,
            Lane::Interactive => !adoptable && matches!(w, Waiting::Get(_)) && self.ops.in_window() >= self.window.size(),
        };
        if queue {
            self.ops.order += 1;
            let order = self.ops.order;
            self.ops.need.insert(w.clone(), Need::Queued { lane, attempt, op, order, first_at });
        } else {
            self.put_on_wire(w, op, lane, attempt + 1, first_at, true);
        }
    }

    /// THE ONE WAY a need's send goes on the wire (a first send, or a re-send at `attempt`), recorded -- or, with
    /// `adopt`, the need ADOPTS an identical stale send the node still runs (the tenth cell: one node GET per key). A
    /// SUPERSEDE never adopts: it sends anew, as DEADLINES and the recording pin hold.
    fn put_on_wire(&mut self, w: &Waiting, op: Op, lane: Lane, attempt: u32, first_at: Option<u64>, adopt: bool) {
        if adopt {
            if let Some(send) = self.take_stale(w, &op) {
                let first_at = first_at.unwrap_or(send.sent_at);
                self.ops.need.insert(w.clone(), Need::OnWire { lane, send, first_at });
                return;
            }
        }
        // A FIRST send waits its place in the node's one queue (sdk#390, F61): an RTO for each send ahead of it --
        // every send the node holds, whatever its lane (OP-LIFE: the node's queue holds them all) -- and one for
        // itself. A re-send keeps its back-off. A SILENT GET is no place (sdk#447).
        let ahead = self.ops.at_node().filter(|(k, s, h, _)| *h != Holder::Stale && !s.silent && *k != w).count() as u64;
        let at = self.now + if attempt == 1 { queued_wait(self.rto.rto_ms(), ahead) } else { self.backoff(attempt) };
        let seq = self.next_send();
        let send = Send { op: op.clone(), at, armed: at, sent_at: self.now, attempt, resent: false, seq, alone: ahead == 0, silent: false };
        self.record_send(w, &send);
        let first_at = first_at.unwrap_or(self.now);
        self.ops.need.insert(w.clone(), Need::OnWire { lane, send, first_at });
        self.out.push(op);
    }

    /// join·I: its waiters class it Interactive now; a background need is PROMOTED (never demoted back, L6).
    fn on_join(&mut self, w: &Waiting) {
        if self.lane_of(w) != Lane::Interactive {
            return;
        }
        match self.ops.need.get(w).cloned() {
            Some(Need::Queued { lane: Lane::Background, attempt, op, first_at, .. }) => {
                self.ops.need.remove(w);
                self.send_new(w, op, Lane::Interactive, attempt, first_at);
            }
            Some(Need::OnWire { lane: Lane::Background, send, first_at }) => {
                self.ops.need.insert(w.clone(), Need::OnWire { lane: Lane::Interactive, send, first_at });
            }
            Some(Need::Parked { lane: Lane::Background, at, attempt, op, first_at }) => {
                self.ops.need.insert(w.clone(), Need::Parked { lane: Lane::Interactive, at, attempt, op, first_at });
            }
            Some(Need::Queued { lane: Lane::Interactive, .. } | Need::OnWire { lane: Lane::Interactive, .. } | Need::Parked { lane: Lane::Interactive, .. }) | None => {}
        }
    }

    /// An ANSWER on `w` (C1, then the need's cell): with what it waits for (`park: None`), or without it (`park:
    /// Some(op)`: the need parks with `op` on its back-off).
    fn on_answer(&mut self, w: &Waiting, park: Option<Op>) -> Did {
        // A GET re-asked past the node's bound (sdk#447): the key's first answer since the re-ask is taken; a later
        // one, while nothing waits, is the known overlap -- COUNTED, and routed on by C1 (it releases the orphan it
        // belongs to, if any).
        if let Waiting::Get(id) = w {
            let waited = self.ops.need.contains_key(w);
            match self.ops.reasked.get_mut(id) {
                Some((_, once)) if waited => *once = true,
                Some((_, true)) => {
                    self.ops.reasked.remove(id);
                    self.answer_after_reask(*id);
                }
                Some((_, false)) | None => {}
            }
        }
        // C1 (F61: the node serves one client's ops in one sequence): a key's answer belongs to the OLDEST send the
        // node may still answer for it -- the held send, a stale one, an UNTRACKED (lost) one (N2), or the need's own.
        // An orphan's answer RELEASES it (its one end, now) and the need does not see it; a lost send's late answer
        // serves the need (N2). If the node is ever seen answering one client out of order, this is the cell to
        // revisit.
        // An UNNAMED answer never matches a send that carries an id (a Sign): those are the named row's.
        let unnamed = |s: &Send| request_of(&s.op).is_none();
        let need_seq = self.ops.need.get(w).and_then(Need::send).filter(|s| unnamed(s)).map(|s| s.seq);
        let held_seq = self.ops.held.as_ref().filter(|(hw, h)| hw == w && h.answerable && unnamed(&h.send)).map(|(_, h)| h.send.seq);
        let stale_seq = self.ops.stale.get(w).and_then(|v| v.iter().filter(|s| unnamed(s)).map(|s| s.seq).min());
        let lost_seq = self.oldest_untracked(w, None);
        let oldest = [need_seq, held_seq, stale_seq, lost_seq].into_iter().flatten().min();
        let late = oldest.is_some() && oldest == lost_seq;
        if let Some(seq) = lost_seq.filter(|_| late) {
            self.take_untracked(w, seq);
        }
        if oldest.is_some() && oldest == held_seq {
            let (_, h) = self.ops.held.take().expect("the held send");
            self.record_end(w, &h.send, End::Withdrawn);
            self.place_frees();
            return Did::Nothing;
        }
        if oldest.is_some() && oldest == stale_seq {
            let v = self.ops.stale.get_mut(w).expect("a stale send");
            let i = v.iter().enumerate().filter(|(_, s)| unnamed(s)).min_by_key(|(_, s)| s.seq).map(|(i, _)| i).expect("one");
            let send = v.remove(i);
            if v.is_empty() {
                self.ops.stale.remove(w);
            }
            self.record_end(w, &send, End::Withdrawn);
            return Did::Nothing;
        }
        let Some(n) = self.ops.need.get(w).cloned() else { return Did::Nothing };
        // A need whose send carries an id is answered only by the named row.
        if n.send().is_some_and(|s| !unnamed(s)) {
            return Did::Nothing;
        }
        let attempt = n.attempt();
        match n {
            Need::OnWire { lane, send, first_at } => {
                self.ops.need.remove(w);
                if late {
                    // N2: a lost send's late answer serves the need; its own send stays at the node (an orphan).
                    self.orphan_the_needs_send(w, lane, send.clone());
                    self.rearm_first_sends();
                } else {
                    self.record_end(w, &send, End::Answered);
                    self.answered_send(w, &send, lane);
                }
                if let Some(op) = park {
                    let at = self.now + self.backoff(attempt);
                    self.ops.need.insert(w.clone(), Need::Parked { lane, at, attempt, op, first_at });
                }
                Did::Answered { op: send.op, attempt }
            }
            // A need queued before its first send: this answer is to an earlier send nobody waits on (defect 8).
            Need::Queued { attempt: 0, .. } => Did::Nothing,
            // A LOST (or yielded) need's late answer is TAKEN: no sample, no window step.
            Need::Queued { lane, op, first_at, .. } => {
                self.ops.need.remove(w);
                if let Some(pop) = park {
                    let at = self.now + self.backoff(attempt);
                    let first_at = first_at.unwrap_or(self.now);
                    self.ops.need.insert(w.clone(), Need::Parked { lane, at, attempt, op: pop, first_at });
                }
                Did::Answered { op, attempt }
            }
            // An earlier send's late answer: TAKEN (or re-parked) -- no sample, no window step.
            Need::Parked { lane, op, first_at, .. } => {
                self.ops.need.remove(w);
                if let Some(pop) = park {
                    let at = self.now + self.backoff(attempt);
                    self.ops.need.insert(w.clone(), Need::Parked { lane, at, attempt, op: pop, first_at });
                }
                Did::Answered { op, attempt }
            }
        }
    }

    /// An answer that NAMES its send by the signer's request id (SG02): OP-LIFE.md's named row, one cell per target.
    fn on_answer_named(&mut self, w: &Waiting, request: u32) -> Did {
        let names = |op: &Op| request_of(op) == Some(request);
        // An UNTRACKED (lost) send carrying the id, OLDER than the need's own (a re-send carries its ask's id: a site's
        // sign is re-sent as the same op): the lost send's late answer, by C1's order. It serves the need, whose own
        // send stays at the node as an orphan (N2).
        let need_named = self.ops.need.get(w).and_then(Need::send).filter(|s| names(&s.op)).map(|s| s.seq);
        if let Some(lost) = self.oldest_untracked(w, Some(request)) {
            if need_named.is_none_or(|n| lost < n) {
                self.take_untracked(w, lost);
                if let Some(Need::OnWire { lane, send, .. }) = self.ops.need.get(w).cloned().filter(|_| need_named.is_some()) {
                    self.ops.need.remove(w);
                    self.orphan_the_needs_send(w, lane, send.clone());
                    self.rearm_first_sends();
                    return Did::Answered { op: send.op, attempt: send.attempt };
                }
                return Did::Nothing;
            }
        }
        // The NEED's send: its answer (Table 1's answer row).
        if let Some(Need::OnWire { lane, send, .. }) = self.ops.need.get(w).cloned() {
            if names(&send.op) {
                self.ops.need.remove(w);
                self.record_end(w, &send, End::Answered);
                self.answered_send(w, &send, lane);
                return Did::Answered { op: send.op, attempt: send.attempt };
            }
        }
        // The HELD send: released, the slot freed; the need untouched.
        if self.ops.held.as_ref().is_some_and(|(hw, h)| hw == w && names(&h.send.op)) {
            let (_, h) = self.ops.held.take().expect("the held send");
            self.record_end(w, &h.send, End::Withdrawn);
            self.place_frees();
            return Did::Nothing;
        }
        // A STALE send: it leaves; the need untouched.
        if let Some(v) = self.ops.stale.get_mut(w) {
            if let Some(i) = v.iter().position(|s| names(&s.op)) {
                let send = v.remove(i);
                if v.is_empty() {
                    self.ops.stale.remove(w);
                }
                self.record_end(w, &send, End::Withdrawn);
                return Did::Nothing;
            }
        }
        // Nothing the page holds carries the id: dropped, counted, no end.
        self.answer_unexpected(w);
        Did::Nothing
    }

    /// The answer to a need's send on the wire: the RTT sample rules (DEADLINES), the re-arm of every first send, and
    /// a window step for an interactive GET.
    fn answered_send(&mut self, w: &Waiting, d: &Send, lane: Lane) {
        // A QUEUED first send answered: the back-off ends (RFC 6298 §5.7), but its time is its wait too: no sample.
        if d.attempt == 1 && !d.resent && !d.alone && !d.silent {
            self.rto.answered_queued();
        }
        // A SILENT GET's time is the node's network fetch, not the path: never a sample (sdk#447).
        if d.attempt == 1 && !d.resent && d.alone && !d.silent {
            let r = self.now.saturating_sub(d.sent_at);
            // IMPOSSIBLE, so loud: a round trip longer than the page has existed was dated against another clock's
            // origin (sdk#397).
            debug_assert!(r <= self.now.saturating_sub(self.born), "an RTT sample of {r} ms on a page {} ms old: a request was dated against another clock's origin", self.now.saturating_sub(self.born));
            self.rto.sample(r);
            // The sample AS COMPUTED, before anything clamps it (the architect): a wrong clock shows here.
            if let Some(rec) = self.rec.as_ref().filter(|_| d.seq >= self.rec_from) {
                use instrument::{vocab::coarsen_ms, Entry, Event, Key, Probe};
                if let Some(id) = self.send_label(d.seq) {
                    let (site, op) = (op_site(w), id.op());
                    rec.event(Event::Counter { site, op, entry: Entry { key: Key::SampleMs, value: coarsen_ms(r) } });
                    rec.event(Event::Counter { site, op, entry: Entry { key: Key::RtoMs, value: coarsen_ms(self.rto.rto_ms()) } });
                }
            }
        }
        // ANY answer re-arms (sdk#390 (b)).
        self.rearm_first_sends();
        if lane == Lane::Interactive && matches!(w, Waiting::Get(_)) {
            self.window.opened();
        }
    }

    /// EVERY answer restarts the timer of every FIRST send still on the wire from THIS answer, at its place in the
    /// node's one queue now, never later than the deadline it was sent with (sdk#378, sdk#390; DEADLINES rule 2).
    /// One pass in send order over every send the node holds (a held one included: it is a place, and re-armed).
    fn rearm_first_sends(&mut self) {
        use instrument::{Entry, Event, Key, Kind, Probe};
        let now = self.now;
        let rto = self.rto.rto_ms();
        let (rec, start, from) = (&self.rec, self.rec_start, self.rec_from);
        let Ops { need, held, .. } = &mut self.ops;
        let mut wire: Vec<(&Waiting, &mut Send)> = need
            .iter_mut()
            .filter_map(|(w, n)| match n {
                Need::OnWire { send, .. } => Some((w, send)),
                Need::Queued { .. } | Need::Parked { .. } => None,
            })
            .chain(held.iter_mut().map(|(w, h)| (&*w, &mut h.send)))
            .filter(|(_, s)| !s.silent)
            .collect();
        wire.sort_unstable_by_key(|(_, s)| s.seq);
        for (ahead, (w, d)) in wire.into_iter().enumerate() {
            if d.attempt == 1 && !d.resent {
                let at = d.armed.min(now + queued_wait(rto, ahead as u64));
                if at != d.at && d.seq >= from {
                    if let Some(rec) = rec {
                        if let Some(op) = instrument::Label::new(Kind::Request, d.seq).map(|l| l.op()) {
                            let value = instrument::vocab::coarsen_ms(at.saturating_sub(start));
                            rec.event(Event::Counter { site: op_site(w), op, entry: Entry { key: Key::ReArmedAtMs, value } });
                        }
                    }
                }
                d.at = at;
            }
        }
    }

    /// WITHDRAW: nobody needs it. A background send the node holds is HELD, keeping the slot, its end recorded when it
    /// leaves (defects 1, 3); an interactive one ends at once, freeing its window place.
    fn on_withdraw(&mut self, w: &Waiting) {
        match self.ops.need.remove(w) {
            Some(Need::OnWire { lane: Lane::Background, send, .. }) => {
                debug_assert!(self.ops.held.is_none(), "OP-LIFE: a held send beside an on-wire background need");
                self.ops.held = Some((w.clone(), Held { send, answerable: true }));
            }
            // STALE (the tenth cell): its window place frees at once; the node may still answer it (C1).
            Some(Need::OnWire { lane: Lane::Interactive, send, .. }) => self.ops.stale.entry(w.clone()).or_default().push(send),
            Some(Need::Queued { .. } | Need::Parked { .. }) | None => {}
        }
    }

    /// PLACE-FREES (L2): the slot to the OLDEST queued background need -- a held send's successor included, with no
    /// priority (round-robin: a successor that went first could keep the slot across repeated supersedes or
    /// reconnects, found by the gate's L5) -- and the window's queue in order.
    fn place_frees(&mut self) {
        while self.ops.in_slot() == 0 {
            let next = self
                .ops
                .need
                .iter()
                .filter_map(|(w, n)| match n {
                    Need::Queued { lane: Lane::Background, order, .. } => Some((*order, w.clone())),
                    Need::Queued { lane: Lane::Interactive, .. } | Need::OnWire { .. } | Need::Parked { .. } => None,
                })
                .min()
                .map(|(_, w)| w);
            let Some(w) = next else { break };
            let Some(Need::Queued { attempt, op, first_at, .. }) = self.ops.need.remove(&w) else { unreachable!("picked a queued need") };
            // A need its waiters now class Interactive goes as that; one the node still runs stale is ADOPTED.
            let lane = self.lane_of(&w);
            self.put_on_wire(&w, op, lane, attempt + 1, first_at, true);
        }
        while self.ops.in_window() < self.window.size() {
            let next = self
                .ops
                .need
                .iter()
                .filter_map(|(w, n)| match n {
                    Need::Queued { lane: Lane::Interactive, order, .. } => matches!(w, Waiting::Get(_)).then(|| (*order, w.clone())),
                    Need::Queued { lane: Lane::Background, .. } | Need::OnWire { .. } | Need::Parked { .. } => None,
                })
                .min()
                .map(|(_, w)| w);
            let Some(w) = next else { break };
            let Some(Need::Queued { attempt, op, first_at, .. }) = self.ops.need.remove(&w) else { unreachable!("picked a queued need") };
            self.put_on_wire(&w, op, Lane::Interactive, attempt + 1, first_at, true);
        }
    }

    /// THE CLOCK's part of the record (Table 1's deadline row, Table 2's held-deadline): the held send first; then
    /// every on-wire GET whose node GET is not over goes SILENT; the RTO backs off ONCE if any send timed out; a lost
    /// interactive GET is a window loss. Returns the needs now due, in key order, for [`Page::on_due`].
    pub(crate) fn tick_ops(&mut self, now: u64) -> Vec<Waiting> {
        #[cfg(test)]
        self.ops.early_left_seqs.clear();
        // FIRST, a STALE send past its deadline leaves -- the page stops counting it, its one end now -- so nothing
        // this tick frees can ADOPT a send that has already left.
        let expired: Vec<(Waiting, Send)> = self.ops.stale.iter().flat_map(|(w, v)| v.iter().filter(|s| now >= s.at).map(move |s| (w.clone(), s.clone()))).collect();
        for (w, send) in expired {
            // A stale send is interactive, never an N2 orphan: this is its one end; the node may still answer it (N2).
            self.record_end(&w, &send, End::Withdrawn);
            self.untrack(&w, &send, false);
        }
        // A GET's untracked sends decay once the node's own GET is certainly over: an OPEN one really leaves then.
        let decayed: Vec<(Waiting, u32)> = self.ops.untracked.iter().flat_map(|(w, u)| u.sends.iter().filter(|l| l.open && l.decay.is_some_and(|at| now >= at)).map(move |l| (w.clone(), l.seq))).collect();
        for (w, seq) in decayed {
            self.record_end_seq(&w, seq, End::Withdrawn);
        }
        self.ops.untracked.retain(|_, u| {
            u.sends.retain(|l| l.decay.is_none_or(|at| now < at));
            !u.sends.is_empty()
        });
        self.ops.stale.retain(|_, v| {
            v.retain(|s| now < s.at);
            !v.is_empty()
        });
        if let Some((hw, h)) = self.ops.held.clone() {
            if now >= h.send.at {
                self.ops.held = None;
                let marked = self.ops.n2_orphans.remove(&h.send.seq);
                if marked {
                    self.ops.early_left += 1;
                }
                // An early leave ends nothing -- unless the send can no longer be answered at all (a reconnect replaced
                // its socket): then this is its real, and only, leave.
                let early = marked && h.answerable && !matches!(hw, Waiting::Get(_));
                if early {
                    #[cfg(test)]
                    self.ops.early_left_seqs.push(h.send.seq);
                } else {
                    self.record_end(&hw, &h.send, End::Withdrawn);
                }
                if h.answerable {
                    self.untrack(&hw, &h.send, early);
                }
                self.place_frees();
            }
        }
        // A re-asked key's earlier node GET cannot answer once its own bound has passed again since the re-ask.
        self.ops.reasked.retain(|_, (at, _)| now < node_get_over_at(*at, rto::RTO_MAX_MS as u64));
        let rto_now = self.rto.rto_ms();
        let due: Vec<Waiting> = self.ops.need.iter().filter(|(_, n)| n.due().is_some_and(|at| now >= at)).map(|(w, _)| w.clone()).collect();
        let mut late = Vec::new();
        let mut timed_out = false;
        for w in due {
            let Some(Need::OnWire { lane, send, first_at }) = self.ops.need.get(&w).cloned() else {
                late.push(w);
                continue;
            };
            // A GET ON THE WIRE whose node GET is not over is SILENT, not lost (sdk#447).
            if matches!(w, Waiting::Get(_)) && now < node_get_over_at(send.sent_at, rto_now) {
                let at = node_get_over_at(send.sent_at, rto_now);
                self.ops.need.insert(w, Need::OnWire { lane, send: Send { silent: true, at, ..send }, first_at });
                continue;
            }
            timed_out = true;
            if lane == Lane::Interactive && matches!(w, Waiting::Get(_)) {
                self.window.lost(send.sent_at, send.attempt, now);
            }
            late.push(w);
        }
        // RFC 6298 §5.5 -- ONCE per tick, however many timed out. A PARKED op coming due timed nothing out.
        if timed_out {
            self.rto.timed_out();
        }
        self.prune_uncertain();
        late
    }

    /// A NEED's deadline (Table 1's deadline row), after [`Page::tick_ops`].
    pub(crate) fn on_due(&mut self, w: &Waiting) -> Due {
        let due = match self.ops.need.get(w).cloned() {
            Some(Need::OnWire { lane, send, first_at }) => {
                self.ops.need.remove(w);
                self.record_end(w, &send, End::TimedOut);
                // LOST: the page stops counting it; the node may still answer it (N2 remembers it, whatever its lane: a
                // later BACKGROUND need of the key must not take its late answer for its own, L1).
                self.untrack(w, &send, false);
                self.ops.order += 1;
                let order = self.ops.order;
                match (lane, w) {
                    // A GET LOST: the one re-asked beside it may overlap it (sdk#447); its re-send queues for a place.
                    (_, Waiting::Get(id)) => {
                        self.ops.reasked.insert(*id, (self.now, false));
                        self.ops.need.insert(w.clone(), Need::Queued { lane, attempt: send.attempt, op: send.op, order, first_at: Some(first_at) });
                        Due::Handled
                    }
                    // A background op YIELDS: to the back of the queue, the slot freed (defect 4).
                    (Lane::Background, _) => {
                        self.ops.need.insert(w.clone(), Need::Queued { lane, attempt: send.attempt, op: send.op, order, first_at: Some(first_at) });
                        Due::Handled
                    }
                    // An interactive op: its owner re-sends it now (a Sign rebuilt under a fresh id).
                    (Lane::Interactive, _) => {
                        self.ops.need.insert(w.clone(), Need::Queued { lane, attempt: send.attempt, op: send.op.clone(), order, first_at: Some(first_at) });
                        Due::ReSend { op: send.op, attempt: send.attempt }
                    }
                }
            }
            // A PARKED op coming due: asked again (not a timeout), or ENDED if nobody needs it (engine-withdraw's
            // due-time door).
            Some(Need::Parked { lane, attempt, op, first_at, .. }) => {
                self.ops.need.remove(w);
                let unneeded = if let Waiting::Get(id) = w { self.engine.blocks().get(id).is_some() || !self.engine.awaits_block(id) } else { false };
                if !unneeded {
                    self.send_new(w, op, lane, attempt, Some(first_at));
                }
                Due::Handled
            }
            Some(Need::Queued { .. }) | None => Due::Handled,
        };
        self.place_frees();
        self.assert_pairs();
        due
    }

    /// After a timed-out interactive op's owner had its turn: a re-send it did not make ends the need.
    pub(crate) fn end_unsent(&mut self, w: &Waiting) {
        if matches!(self.ops.need.get(w), Some(Need::Queued { lane: Lane::Interactive, .. })) && !matches!(w, Waiting::Get(_)) {
            self.ops.need.remove(w);
        }
    }

    /// RECONNECT (Table 1's and 2's reconnect rows): every interactive send on the wire is re-sent at once on the new
    /// socket in send order (DEADLINES; a GET is N1's second node GET); a background one is HELD, not answerable,
    /// keeping the slot until its deadline, with its need queued as its SUCCESSOR (defect 9). Returns whether a head
    /// read was among the re-sends.
    pub(crate) fn reconnect_ops(&mut self) -> bool {
        if let Some((_, h)) = self.ops.held.as_mut() {
            h.answerable = false;
        }
        // An UNTRACKED (lost) send's answer cannot come on the new socket either: N2's memory of it goes (an OPEN one
        // really leaves now: its End).
        for (w, u) in std::mem::take(&mut self.ops.untracked) {
            for l in u.sends.iter().filter(|l| l.open) {
                self.record_end_seq(&w, l.seq, End::Withdrawn);
            }
        }
        // A stale send's answer cannot come on the new socket: it leaves now, its one end recorded.
        for (w, v) in std::mem::take(&mut self.ops.stale) {
            for send in v {
                self.record_end(&w, &send, End::Withdrawn);
            }
        }
        let mut on_wire: Vec<(u32, Waiting)> = self.ops.need.iter().filter_map(|(w, n)| n.send().map(|s| (s.seq, w.clone()))).collect();
        on_wire.sort_unstable();
        let mut head_read = false;
        for (_, w) in on_wire {
            let Some(Need::OnWire { lane, send, first_at }) = self.ops.need.get(&w).cloned() else { unreachable!("listed") };
            match lane {
                Lane::Interactive => {
                    let at = self.now + self.backoff(send.attempt);
                    // The send on the old socket is WITHDRAWN (its answer cannot come), and the re-send is a new send.
                    self.record_end(&w, &send, End::Withdrawn);
                    let seq = self.next_send();
                    head_read |= matches!(send.op, Op::ReadHead { label: Label::Head });
                    let send = Send { at, armed: at, sent_at: self.now, resent: true, silent: false, seq, ..send };
                    self.record_send(&w, &send);
                    self.out.push(send.op.clone());
                    self.ops.need.insert(w, Need::OnWire { lane, send, first_at });
                }
                Lane::Background => {
                    debug_assert!(self.ops.held.is_none(), "OP-LIFE: a held send beside an on-wire background need");
                    self.ops.need.remove(&w);
                    self.ops.order += 1;
                    let order = self.ops.order;
                    // The successor HAS been sent (its held send is that send): its attempts are kept, so a late answer
                    // to an earlier one is TAKEN (the answer row's attempt ≥ 1 cell).
                    self.ops.need.insert(w.clone(), Need::Queued { lane, attempt: send.attempt, op: send.op.clone(), order, first_at: Some(first_at) });
                    self.ops.held = Some((w, Held { send, answerable: false }));
                }
            }
        }
        self.place_frees();
        self.prune_uncertain();
        self.assert_pairs();
        head_read
    }
}

#[cfg(test)]
mod test_view {
    //! THE TESTS' READ-ONLY VIEW of the record, derived from it.
    use super::*;

    /// THE TESTS' READ-ONLY VIEW of the record, in the old `Deadline`'s terms (the deadline-table pins read these names):
    /// every need with a deadline (on the wire or parked) and the held send, DERIVED from the record, never a second one.
    #[allow(dead_code)]
    #[derive(Debug, Clone)]
    pub(crate) struct DeadlineView {
        pub(crate) lane: Lane,
        /// The held send: the page no longer waits on it; the node still holds it.
        pub(crate) withdrawn: bool,
        pub(crate) at: u64,
        pub(crate) armed: u64,
        pub(crate) op: Op,
        pub(crate) sent_at: u64,
        pub(crate) attempt: u32,
        /// On the wire (else parked).
        pub(crate) sent: bool,
        pub(crate) resent: bool,
        pub(crate) seq: u32,
        pub(crate) alone: bool,
        pub(crate) silent: bool,
    }

    impl DeadlineView {
        fn of_send(lane: Lane, withdrawn: bool, s: &Send) -> DeadlineView {
            DeadlineView { lane, withdrawn, at: s.at, armed: s.armed, op: s.op.clone(), sent_at: s.sent_at, attempt: s.attempt, sent: true, resent: s.resent, seq: s.seq, alone: s.alone, silent: s.silent }
        }
    }

    impl Page {
        /// Every deadline: each need on the wire or parked, and the held send.
        pub(crate) fn dl(&self) -> BTreeMap<Waiting, DeadlineView> {
            let mut out: BTreeMap<Waiting, DeadlineView> = self
                .ops
                .need
                .iter()
                .filter_map(|(w, n)| match n {
                    Need::OnWire { lane, send, .. } => Some((w.clone(), DeadlineView::of_send(*lane, false, send))),
                    Need::Parked { lane, at, attempt, op, .. } => Some((w.clone(), DeadlineView { lane: *lane, withdrawn: false, at: *at, armed: *at, op: op.clone(), sent_at: 0, attempt: *attempt, sent: false, resent: false, seq: 0, alone: false, silent: false })),
                    Need::Queued { .. } => None,
                })
                .collect();
            if let Some((w, h)) = &self.ops.held {
                out.entry(w.clone()).or_insert_with(|| DeadlineView::of_send(Lane::Background, true, &h.send));
            }
            out
        }

        /// The last tick's early orphan leaves (send ordinals).
        pub(crate) fn early_left_seqs(&self) -> &[u32] {
            &self.ops.early_left_seqs
        }

        /// GETs holding a window place.
        pub(crate) fn gets_in_flight(&self) -> usize {
            self.ops.in_window()
        }

        /// Does a need for `w` keep attempts already made (a lost GET queued, a parked op)?
        pub(crate) fn attempt_kept(&self, w: &Waiting) -> bool {
            self.ops.need.get(w).is_some_and(|n| n.attempt() >= 1)
        }

        /// GETs re-asked past the node's bound (sdk#447).
        pub(crate) fn reasked(&self) -> &BTreeMap<Cid, (u64, bool)> {
            &self.ops.reasked
        }

        /// A send made as its `attempt`-th (a deadline-table pin's re-send), through the one door onto the wire.
        pub(crate) fn send_attempt(&mut self, w: Waiting, op: Op, attempt: u32) {
            let lane = self.lane_of(&w);
            self.put_on_wire(&w, op, lane, attempt, None, false);
        }

        /// Interactive GETs waiting for a window place, in queue order.
        pub(crate) fn get_queued(&self) -> Vec<Cid> {
            let mut q: Vec<(u64, Cid)> = self
                .ops
                .need
                .iter()
                .filter_map(|(w, n)| match n {
                    Need::Queued { lane: Lane::Interactive, order, .. } => if let Waiting::Get(id) = w { Some((*order, *id)) } else { None },
                    Need::Queued { lane: Lane::Background, .. } | Need::OnWire { .. } | Need::Parked { .. } => None,
                })
                .collect();
            q.sort_unstable();
            q.into_iter().map(|(_, id)| id).collect()
        }

        /// Background needs waiting for the slot, in queue order.
        pub(crate) fn bg_queued(&self) -> Vec<(Waiting, Op)> {
            let mut q: Vec<(u64, Waiting, Op)> = self
                .ops
                .need
                .iter()
                .filter_map(|(w, n)| match n {
                    Need::Queued { lane: Lane::Background, order, op, .. } => Some((*order, w.clone(), op.clone())),
                    Need::Queued { lane: Lane::Interactive, .. } | Need::OnWire { .. } | Need::Parked { .. } => None,
                })
                .collect();
            q.sort_unstable_by_key(|(o, ..)| *o);
            q.into_iter().map(|(_, w, op)| (w, op)).collect()
        }
    }
}

#[cfg(test)]
mod adapter;
#[cfg(test)]
mod model;
#[cfg(test)]
mod named;
#[cfg(test)]
mod scenarios;
