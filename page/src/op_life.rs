// NO CATCH-ALL over the record's enums (CLAUDE.md "Structure before code"): a new state, event or key must fail to
// compile until each match handles it. Both lints: `_`/`other =>` over several left, and `_` over one left.
#![deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]
//! THE OP RECORD (craftworks-docs/docs/design/OP-LIFE.md, draft 3.1): ONE entry per KEY -- a (contract, op kind) --
//! written ONLY by [`Page::on`] and the time events beside it; its fields are private to this module, so nothing else
//! in the crate can write them. The node's answers name no request, so the record keeps AT MOST ONE SEND IN FLIGHT PER
//! KEY (L0, I1): every answer then has exactly one owner. The one exception, by type, is a key whose re-send DUPLICATES
//! the first in the Interactive lane ([`in_flight`]): it keeps rule 7's RTO re-sends. Everything the rest of the page
//! asks of an op -- is it on the wire, which place does it hold, is it due, how long has it waited -- is DERIVED here
//! from the record, never kept beside it.
use crate::*;

/// WHAT A SECOND SEND OF A KEY IS, and so when one may go (OP-LIFE.md draft 3.1, "The type"). The node's answers name
/// no request, so this type -- not a routing rule -- is what makes every answer's owner certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Resend {
    /// A DUPLICATE of the first: a content PUT (key = blake3(bytes): any PutOk means stored) or a register UPDATE the
    /// contract joins (its answer carries nothing; the read decides, F56). In the Interactive lane it goes on the RTO.
    Duplicates,
    /// Only after the NODE BOUND: a block GET, a register or site read. Its answer verifies itself, so a late one
    /// SERVES while the key still wants it.
    AfterBound,
    /// One at a time, ever: the Sign, a Held batch, the signer's setup, a site's create PUT. A re-send after the bound
    /// carries a FRESH id where the kind has one, and the id CHECKS every answer.
    OnlyOne,
}

/// What a key may have in flight: the ONE decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InFlight {
    /// Rule 7's RTO re-sends, several identical sends at the node (node ops per key <= the re-sends in its bound).
    RtoResend,
    /// L0 (I1): one send in flight per key per socket; the only overlap is at the bound (the same payload, or an
    /// id-checked kind under a fresh id).
    OneInFlight,
}

/// THE POLICY, exhaustive: its only `RtoResend` cell is (Duplicates, Interactive). A Background key never has two
/// sends at the node (L1 is a NODE fact).
pub(crate) fn in_flight(r: Resend, lane: Lane) -> InFlight {
    match (r, lane) {
        (Resend::Duplicates, Lane::Interactive) => InFlight::RtoResend,
        (Resend::Duplicates, Lane::Background) => InFlight::OneInFlight,
        (Resend::AfterBound, Lane::Interactive) | (Resend::AfterBound, Lane::Background) => InFlight::OneInFlight,
        (Resend::OnlyOne, Lane::Interactive) | (Resend::OnlyOne, Lane::Background) => InFlight::OneInFlight,
    }
}

/// How `op` may be sent again: one exhaustive match over `Op` (OP-LIFE.md's mapping table, row for row).
pub(crate) fn resend(op: &Op) -> Resend {
    match op {
        Op::Put { .. } | Op::PutApp { .. } => Resend::Duplicates,
        Op::Update { label: Label::Head, .. } => Resend::Duplicates,
        // A site's write is a PUT of its framing (page-io): it stays one at a time (the architect, F2).
        Op::Update { label: Label::Site(_), .. } => Resend::OnlyOne,
        Op::Get { .. } | Op::ReadHead { .. } => Resend::AfterBound,
        Op::Sign { .. } | Op::AskHeld { .. } | Op::Ext(_) => Resend::OnlyOne,
    }
}

/// THE NODE BOUND of a send of `op` (OP-LIFE.md "Bounds", read at freenet v0.2.138): past it the node's work on it is
/// PROBABLY over (F1: its queue delays the take), so only the same payload may overlap it there (L0).
fn node_bound_ms(op: &Op) -> u64 {
    // The handler round trip that bounds an UPDATE's local apply and a delegate request (contract/handler.rs:409).
    const HANDLER_MS: u64 = 300_000;
    // A non-streaming PUT: local store 300 s + probe 60 s + 4 attempts and 3 infra retries of 60 s each (Q3: accepted
    // for Background PUTs without a measurement).
    const PUT_MS: u64 = 300_000 + 60_000 + 7 * 60_000;
    // A streaming PUT: one attempt, capped (operations.rs:838).
    const STREAMING_PUT_MS: u64 = 600_000;
    match op {
        Op::Get { .. } | Op::ReadHead { .. } => rto::NODE_GET_BOUND_MS as u64,
        Op::Update { .. } | Op::Sign { .. } | Op::AskHeld { .. } | Op::Ext(_) => HANDLER_MS,
        Op::Put { .. } => PUT_MS,
        Op::PutApp { .. } => STREAMING_PUT_MS,
    }
}

/// When a send's node bound has passed: its bound plus ONE RTO of margin, at the use site. For a GET this IS sdk#447's
/// `node_get_over_at` (L0's GET case), the one formula.
pub(crate) fn bound_at(op: &Op, sent_at: u64, rto: u64) -> u64 {
    match op {
        Op::Get { .. } | Op::ReadHead { .. } => node_get_over_at(sent_at, rto),
        Op::Put { .. } | Op::PutApp { .. } | Op::Update { .. } | Op::Sign { .. } | Op::AskHeld { .. } | Op::Ext(_) => sent_at.saturating_add(node_bound_ms(op)).saturating_add(rto),
    }
}

/// Does this answer NAME its send (SG02)? The name is then a CHECK against the one send in flight.
pub(crate) fn request_of(op: &Op) -> Option<u32> {
    match op {
        Op::Sign { id, .. } => Some(*id),
        Op::AskHeld { batch, .. } => Some(*batch),
        Op::Put { .. } | Op::Get { .. } | Op::Update { .. } | Op::ReadHead { .. } | Op::PutApp { .. } | Op::Ext(_) => None,
    }
}

/// A (contract, op kind): the unit L0 counts. The waits that share one are its WAITERS.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Put(Cid),
    Get(Cid),
    /// Every read of one register or site: the head's five waits share it.
    Read(Label),
    Update(Label),
    Sign(Label),
    Held(u32),
    PutApp(String),
    Ext(Ext),
}

/// The key a wait is a waiter of: one exhaustive match.
pub(crate) fn key_of(w: &Waiting) -> Key {
    match w {
        Waiting::Put(id) | Waiting::Repair(id) => Key::Put(*id),
        Waiting::Get(id) => Key::Get(*id),
        Waiting::Held(b) => Key::Held(*b),
        Waiting::Sign(l) => Key::Sign(l.clone()),
        Waiting::Update(l) => Key::Update(l.clone()),
        Waiting::ReadBack(l) => Key::Read(l.clone()),
        Waiting::Warm | Waiting::RecoverHead | Waiting::Verify | Waiting::Hint => Key::Read(Label::Head),
        Waiting::PutApp(k, _) => Key::PutApp(k.clone()),
        Waiting::Ext(e) => Key::Ext(*e),
    }
}

/// May this wait RIDE a read already in flight (OP-LIFE.md's join table)? `Warm` and `RecoverHead` need only "a read
/// of the register"; every other read means "a read sent after X" (its UPDATE, the signer's NotNext, the hint), and X
/// is taken as the wait's own ask -- after its X, and so stricter, never looser: it costs a read, never a wrong answer.
fn joins_any(w: &Waiting) -> bool {
    match w {
        Waiting::Warm | Waiting::RecoverHead => true,
        Waiting::ReadBack(_) | Waiting::Verify | Waiting::Hint => false,
        // One waiter per key: a second ask by the same wait is the same want.
        Waiting::Put(_) | Waiting::Repair(_) | Waiting::Get(_) | Waiting::Held(_) | Waiting::Sign(_) | Waiting::Update(_) | Waiting::PutApp(..) | Waiting::Ext(_) => true,
    }
}

/// One send the node holds (DEADLINES.md owns its timing: `at`, `armed`, the sample rules).
#[derive(Debug, Clone)]
pub(crate) struct Send {
    pub(crate) op: Op,
    /// Its next due: the RTO deadline, then (a `OneInFlight` send gone SILENT) its node bound.
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
    /// Past its RTO inside its node bound: shown "not answering", nothing re-sent, no place in `ahead`, no sample.
    pub(crate) silent: bool,
    /// Sent AT its key's bound while the previous send may still run (F1): its answer may be either's, so it is counted
    /// as an answer after the bound (L1's detector), whichever it is.
    pub(crate) overlap: bool,
}

/// ONE KEY's entry: OP-LIFE.md's one table. No entry means Absent. Each case carries its own data. (`OnWire` is the
/// large case, and the common one: boxing it would move every send behind a pointer for nothing a page holds many of.)
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Entry {
    /// Waiting for a PLACE (a window place, the slot), or, within ONE tick, for its owner's re-send. `attempt`: the
    /// sends made so far (0: never sent).
    Queued { lane: Lane, attempt: u32, op: Op, order: u64, first_at: Option<u64>, waiters: BTreeSet<Waiting>, overlap: bool },
    /// ONE send in flight (a `RtoResend` key: its latest). `riders`: the waits this send serves -- empty once all are
    /// withdrawn, and the send still occupies its key (and, if Background, the slot) until it leaves. `again`: waits
    /// that need a FRESH send of the same op after this one answers (a read after X). `next`: a DIFFERENT payload,
    /// sent after an ANSWER (F1), with the waits that want it. `by`: the wait that asked this send (its recording site).
    OnWire { lane: Lane, send: Send, by: Waiting, first_at: u64, riders: BTreeSet<Waiting>, again: BTreeSet<Waiting>, next: Option<(Op, BTreeSet<Waiting>)> },
    /// Answered without what it waits for: asked again at `at`, on its back-off. Coming due is no timeout.
    Parked { lane: Lane, at: u64, attempt: u32, op: Op, first_at: u64, waiters: BTreeSet<Waiting> },
}

impl Entry {
    pub(crate) fn lane(&self) -> Lane {
        match self {
            Entry::Queued { lane, .. } | Entry::OnWire { lane, .. } | Entry::Parked { lane, .. } => *lane,
        }
    }

    /// The op it waits on (on the wire: the send's).
    pub(crate) fn op(&self) -> &Op {
        match self {
            Entry::Queued { op, .. } | Entry::Parked { op, .. } => op,
            Entry::OnWire { send, .. } => &send.op,
        }
    }

    /// The send in flight, if it is on the wire.
    pub(crate) fn send(&self) -> Option<&Send> {
        match self {
            Entry::OnWire { send, .. } => Some(send),
            Entry::Queued { .. } | Entry::Parked { .. } => None,
        }
    }

    /// Attempts made so far.
    #[cfg(test)]
    pub(crate) fn attempt(&self) -> u32 {
        match self {
            Entry::Queued { attempt, .. } | Entry::Parked { attempt, .. } => *attempt,
            Entry::OnWire { send, .. } => send.attempt,
        }
    }

    fn first_at(&self) -> Option<u64> {
        match self {
            Entry::Queued { first_at, .. } => *first_at,
            Entry::OnWire { first_at, .. } | Entry::Parked { first_at, .. } => Some(*first_at),
        }
    }

    /// When it is next due (a send's deadline, a parked op's re-ask), if it has one.
    pub(crate) fn due(&self) -> Option<u64> {
        match self {
            Entry::Queued { .. } => None,
            Entry::OnWire { send, .. } => Some(send.at),
            Entry::Parked { at, .. } => Some(*at),
        }
    }

    /// Every wait the entry serves, now or next.
    fn waiters(&self) -> impl Iterator<Item = &Waiting> {
        type Set<'a> = Option<&'a BTreeSet<Waiting>>;
        let (a, b, c): (Set<'_>, Set<'_>, Set<'_>) = match self {
            Entry::Queued { waiters, .. } | Entry::Parked { waiters, .. } => (Some(waiters), None, None),
            Entry::OnWire { riders, again, next, .. } => (Some(riders), Some(again), next.as_ref().map(|(_, n)| n)),
        };
        a.into_iter().flatten().chain(b.into_iter().flatten()).chain(c.into_iter().flatten())
    }
}

/// Where a send counts: a window place, THE slot, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Window,
    Slot,
    None,
}

/// THE RECORD. Its fields are private to this module: only [`Page::on`] and the time events write them.
#[derive(Debug)]
pub(crate) struct Ops {
    entry: BTreeMap<Key, Entry>,
    /// A Background send a reconnect orphaned: the node still runs it (F61, the disconnect cancels nothing) until its
    /// bound, so the SLOT stays taken until then (L1); its KEY is free at once (L0 is per socket). ONE Option is enough
    /// (the architect, Q4): while it is set the slot is taken, so no Background send goes out, and a second reconnect
    /// inside it orphans nothing new.
    slot_owed_until: Option<u64>,
    /// The next queue place.
    order: u64,
    /// EVERY answer that arrived after its send's bound, or as a duplicate -- by (resend, lane, served): the one counter
    /// (sdk#447's AnsweredAfterReask, generalised), and L1's detector for the overlaps F1 predicts at the bound.
    late: BTreeMap<(u8, bool, bool), u64>,
}

impl Ops {
    /// An empty record. No `Default`: `mem::take` of the page's record would not compile (one writer, by type).
    pub(crate) fn new() -> Ops {
        Ops { entry: BTreeMap::new(), slot_owed_until: None, order: 0, late: BTreeMap::new() }
    }

    /// The entry a wait is a waiter of, if it waits.
    pub(crate) fn need(&self, w: &Waiting) -> Option<&Entry> {
        self.entry.get(&key_of(w)).filter(|e| e.waiters().any(|x| x == w))
    }

    /// Does this wait wait on anything?
    pub(crate) fn contains(&self, w: &Waiting) -> bool {
        self.need(w).is_some()
    }

    /// Every wait, with its key's entry.
    pub(crate) fn needs(&self) -> impl Iterator<Item = (&Waiting, &Entry)> {
        self.entry.values().flat_map(|e| e.waiters().map(move |w| (w, e)))
    }

    /// No wait waits on anything. A send no wait wants any more (withdrawn) is owed nothing: it only occupies its key
    /// (and its place, in `next_due`) until it leaves.
    pub(crate) fn is_empty(&self) -> bool {
        self.entry.values().all(|e| e.waiters().next().is_none())
    }

    /// The op this wait's key is about, if the key has an entry.
    pub(crate) fn op_of(&self, w: &Waiting) -> Option<&Op> {
        self.entry.get(&key_of(w)).map(Entry::op)
    }

    /// Where an entry's send counts (OP-LIFE.md's `place()`): a Background send holds THE slot, wanted or not; an
    /// Interactive GET a window place while it serves a wait (a withdrawn one frees its place at once: the window is the
    /// page's congestion estimate, DEADLINES).
    fn place_of(k: &Key, e: &Entry) -> Place {
        match e {
            Entry::OnWire { lane: Lane::Background, .. } => Place::Slot,
            Entry::OnWire { lane: Lane::Interactive, riders, .. } if matches!(k, Key::Get(_)) && !riders.is_empty() => Place::Window,
            Entry::OnWire { lane: Lane::Interactive, .. } | Entry::Queued { .. } | Entry::Parked { .. } => Place::None,
        }
    }

    /// EVERY send in flight (OP-LIFE.md's `at_node()`): one per key, with the wait that asked it and where it counts.
    pub(crate) fn at_node(&self) -> impl Iterator<Item = (&Waiting, &Send, Place)> {
        self.entry.iter().filter_map(|(k, e)| match e {
            Entry::OnWire { send, by, .. } => Some((by, send, Ops::place_of(k, e))),
            Entry::Queued { .. } | Entry::Parked { .. } => None,
        })
    }

    /// THE slot's holders: 0 or 1 (L1) -- a Background key's send, or a reconnect's orphan still owed.
    pub(crate) fn in_slot(&self) -> usize {
        self.at_node().filter(|(.., p)| *p == Place::Slot).count() + usize::from(self.slot_owed_until.is_some())
    }

    /// Window places held.
    pub(crate) fn in_window(&self) -> usize {
        self.at_node().filter(|(.., p)| *p == Place::Window).count()
    }

    /// The wait a NAMED answer (SG02: the signer's request id) is for: the one whose key's send IN FLIGHT carries
    /// `request`. `None`: no send of this page carries it -- the answer is dropped and counted (the check).
    pub(crate) fn named(&self, request: u32) -> Option<&Waiting> {
        self.entry.values().find_map(|e| match e {
            Entry::OnWire { send, by, .. } if request_of(&send.op) == Some(request) => Some(by),
            Entry::OnWire { .. } | Entry::Queued { .. } | Entry::Parked { .. } => None,
        })
    }

    /// The waits a read in flight of `label` serves.
    pub(crate) fn riders(&self, label: &Label) -> BTreeSet<Waiting> {
        match self.entry.get(&Key::Read(label.clone())) {
            Some(Entry::OnWire { riders, .. }) => riders.clone(),
            Some(Entry::Queued { .. } | Entry::Parked { .. }) | None => BTreeSet::new(),
        }
    }

    /// The oldest first send among waits of `lane` that have been sent (on the wire or parked), for "not answering".
    pub(crate) fn oldest_waiting(&self, lane: Lane) -> Option<(&Waiting, u64)> {
        self.needs().filter(|(_, e)| e.lane() == lane && !matches!(e, Entry::Queued { .. })).filter_map(|(w, e)| e.first_at().map(|at| (w, at))).min_by_key(|(_, at)| *at)
    }

    /// Answers after their send's bound or duplicates, by (resend, lane, served).
    pub(crate) fn late(&self, r: Resend, lane: Lane, served: bool) -> u64 {
        self.late.get(&(r as u8, lane == Lane::Background, served)).copied().unwrap_or(0)
    }

    /// A key's entry, whoever waits on it (the gate reads a withdrawn send's lane here).
    #[cfg(test)]
    pub(crate) fn entry_of(&self, k: &Key) -> Option<&Entry> {
        self.entry.get(k)
    }

    /// The earliest due of anything on the record.
    pub(crate) fn next_due(&self) -> Option<u64> {
        self.entry.values().filter_map(Entry::due).chain(self.slot_owed_until).min()
    }
}

/// What happens to an op: OP-LIFE.md's events (engine-withdraw is `Withdraw`, from its three doors).
#[derive(Debug, Clone)]
pub(crate) enum OpEvent {
    /// A need for the wait's key, in the lane its waiters give it (`lane_of`).
    Send { op: Op, lane: Lane },
    /// join·I: its waiters now class it Interactive, with no new send.
    Join,
    /// The node answered with what it waits for.
    Answer,
    /// An answer that NAMES its send (SG02): the name is CHECKED against the one send in flight.
    AnswerNamed { request: u32 },
    /// The node answered without it; its waits park with `op` (the same op, re-asked on its back-off).
    AnswerWithout { op: Op },
    /// ONE answer of a register or site read: every rider is served, except `park`, which park on the back-off (a
    /// head below the floor; the engine's own read that shows a head this page's record beats).
    AnswerRead { park: BTreeSet<Waiting> },
    /// Nobody needs it (the page, a person, or the engine's three doors).
    Withdraw,
}

/// What an event did, for the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Did {
    /// Nothing a caller acts on (a join, a queue, a drop, a withdraw).
    Nothing,
    /// The key's send was answered: its op, the attempt answered, and the waits it SERVED.
    Answered { op: Op, attempt: u32, served: Vec<Waiting> },
}

/// What a key's deadline did, for the tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Due {
    /// Silent, re-queued, re-sent or ended by the record itself.
    Handled,
    /// Its OWNER re-sends it NOW: a `RtoResend` key at its RTO (the same bytes; an UPDATE's landing counts it), or a
    /// Sign at its bound (rebuilt under a FRESH id, PUBLISH-LIFE's SignLost). What it does not send ends.
    ReSend { op: Op, attempt: u32 },
}

impl Page {
    /// THE TRANSITION FUNCTION of OP-LIFE.md's table, for one wait's key: the only writer of the record, with the time
    /// events in [`Page::tick_ops`], [`Page::on_due`] and [`Page::reconnect_ops`] beside it. Every event ends with
    /// PLACE-FREES (L2: a free place is given in the same step).
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
            OpEvent::Answer => self.on_answer(w, None, BTreeSet::new()),
            OpEvent::AnswerNamed { request } => {
                let carries = matches!(self.ops.entry.get(&key_of(w)), Some(Entry::OnWire { send, .. }) if request_of(&send.op) == Some(request));
                if carries {
                    self.on_answer(w, None, BTreeSet::new())
                } else {
                    // THE CHECK: no send in flight carries the id (an old id's late answer, or nobody's): dropped, counted.
                    let r = self.op_resend_of(w);
                    self.answer_late(w, r, self.lane_of(w), false);
                    Did::Nothing
                }
            }
            OpEvent::AnswerWithout { op } => self.on_answer(w, Some(op), BTreeSet::new()),
            OpEvent::AnswerRead { park } => self.on_answer(w, None, park),
            OpEvent::Withdraw => {
                self.on_withdraw(w);
                Did::Nothing
            }
        };
        self.place_frees();
        self.assert_l1();
        did
    }

    /// L1 as the record holds it (the model checks it from the node's side too).
    fn assert_l1(&self) {
        debug_assert!(self.ops.in_slot() <= 1, "OP-LIFE: {} sends hold the slot (L1)", self.ops.in_slot());
    }

    /// The lane a key goes in NOW: Interactive iff any of its waits is (L6: `lane_of` moves only B → I).
    fn key_lane<'a>(&self, waiters: impl IntoIterator<Item = &'a Waiting>) -> Lane {
        if waiters.into_iter().any(|w| self.lane_of(w) == Lane::Interactive) {
            Lane::Interactive
        } else {
            Lane::Background
        }
    }

    /// A need for `w`'s key (the table's two send rows).
    fn on_send(&mut self, w: &Waiting, op: Op, lane: Lane) {
        let k = key_of(w);
        match self.ops.entry.remove(&k) {
            None => {
                let waiters = BTreeSet::from([w.clone()]);
                self.send_new(&k, w, op, lane, 0, None, waiters, false);
            }
            Some(Entry::Queued { lane: was, attempt, op: _, order, first_at, mut waiters, overlap }) => {
                waiters.insert(w.clone());
                let lane = if lane == Lane::Interactive { Lane::Interactive } else { was };
                // An interactive non-GET queued within this tick is its owner's re-send: it goes NOW. A background one
                // promoted by this send goes now as interactive work (L3).
                let now = lane == Lane::Interactive && (was == Lane::Background || !matches!(k, Key::Get(_)));
                if now {
                    self.send_new(&k, w, op, Lane::Interactive, attempt, first_at, waiters, overlap);
                } else {
                    // JOINS; the payload is the newest.
                    self.ops.entry.insert(k, Entry::Queued { lane, attempt, op, order, first_at, waiters, overlap });
                }
            }
            Some(Entry::Parked { lane: was, at, attempt, op: _, first_at, mut waiters }) => {
                // JOINS its wait (DEADLINES rule 5): the payload is the newest; an interactive waiter PROMOTES it.
                waiters.insert(w.clone());
                let lane = if lane == Lane::Interactive { Lane::Interactive } else { was };
                self.ops.entry.insert(k, Entry::Parked { lane, at, attempt, op, first_at, waiters });
            }
            Some(Entry::OnWire { lane: sent_in, send, by, first_at, mut riders, mut again, next }) => {
                let same = send.op == op;
                let next = if same {
                    // THE SAME OP: a wait that may ride it JOINS (a read's may-join rule: OP-LIFE.md's join table);
                    // one that means "a read sent after my X" waits for a FRESH send after this one answers.
                    // A wait is in ONE set: riding this send, or waiting for a fresh one.
                    if joins_any(w) || riders.contains(w) {
                        again.remove(w);
                        riders.insert(w.clone());
                    } else {
                        again.insert(w.clone());
                    }
                    next
                } else {
                    // A DIFFERENT payload WAITS for this send's answer (F1): the newest wins, with every wait that wants it.
                    let mut ws = next.map(|(_, ws)| ws).unwrap_or_default();
                    riders.remove(w);
                    again.remove(w);
                    ws.insert(w.clone());
                    Some((op, ws))
                };
                // The lane of the send in flight never changes (L6): a promotion takes effect at its next send.
                self.ops.entry.insert(k, Entry::OnWire { lane: sent_in, send, by, first_at, riders, again, next });
            }
        }
    }

    /// A key sent as from Absent (or re-sent), after `attempt` earlier sends: into its lane's place, or queued for one.
    #[allow(clippy::too_many_arguments)]
    fn send_new(&mut self, k: &Key, by: &Waiting, op: Op, lane: Lane, attempt: u32, first_at: Option<u64>, waiters: BTreeSet<Waiting>, overlap: bool) {
        let queue = match lane {
            Lane::Background => self.ops.in_slot() >= 1,
            Lane::Interactive => matches!(k, Key::Get(_)) && self.ops.in_window() >= self.window.size(),
        };
        if queue {
            self.ops.order += 1;
            let order = self.ops.order;
            self.ops.entry.insert(k.clone(), Entry::Queued { lane, attempt, op, order, first_at, waiters, overlap });
        } else {
            self.put_on_wire(k, by, op, lane, attempt + 1, first_at, waiters, overlap);
        }
    }

    /// THE ONE WAY a key's send goes on the wire (a first send, or a re-send at `attempt`), recorded.
    #[allow(clippy::too_many_arguments)]
    fn put_on_wire(&mut self, k: &Key, by: &Waiting, op: Op, lane: Lane, attempt: u32, first_at: Option<u64>, riders: BTreeSet<Waiting>, overlap: bool) {
        // A FIRST send waits its place in the node's one queue (sdk#390, F61): an RTO for each send ahead of it --
        // every send in flight, whatever its lane -- and one for itself. A re-send keeps its back-off. A SILENT send
        // is no place.
        let ahead = self.ops.at_node().filter(|(_, s, _)| !s.silent).count() as u64;
        let at = self.now + if attempt == 1 { queued_wait(self.rto.rto_ms(), ahead) } else { self.backoff(attempt) };
        let seq = self.next_send();
        let send = Send { op: op.clone(), at, armed: at, sent_at: self.now, attempt, resent: false, seq, alone: ahead == 0, silent: false, overlap };
        self.record_send(by, &send);
        let first_at = first_at.unwrap_or(self.now);
        self.ops.entry.insert(k.clone(), Entry::OnWire { lane, send, by: by.clone(), first_at, riders, again: BTreeSet::new(), next: None });
        self.out.push(op);
    }

    /// join·I: its waits class the key Interactive now. Queued or parked, it is PROMOTED; ON THE WIRE its lane is fixed
    /// (L6: the promotion applies at the key's next send, and a Background send keeps counting toward L1).
    fn on_join(&mut self, w: &Waiting) {
        if self.lane_of(w) != Lane::Interactive {
            return;
        }
        let k = key_of(w);
        match self.ops.entry.remove(&k) {
            Some(Entry::Queued { lane: Lane::Background, attempt, op, first_at, waiters, overlap, .. }) => {
                let by = waiters.iter().next().cloned().unwrap_or_else(|| w.clone());
                self.send_new(&k, &by, op, Lane::Interactive, attempt, first_at, waiters, overlap);
            }
            Some(Entry::Parked { lane: Lane::Background, at, attempt, op, first_at, waiters }) => {
                self.ops.entry.insert(k, Entry::Parked { lane: Lane::Interactive, at, attempt, op, first_at, waiters });
            }
            Some(e @ (Entry::Queued { lane: Lane::Interactive, .. } | Entry::Parked { lane: Lane::Interactive, .. } | Entry::OnWire { .. })) => {
                self.ops.entry.insert(k, e);
            }
            None => {}
        }
    }

    /// An ANSWER on `w`'s key: with what it waits for (`park: None`), without it (`park: Some(op)`: its waits park
    /// with `op`), or a read's answer that parks only the waits in `park_waits`.
    fn on_answer(&mut self, w: &Waiting, park: Option<Op>, park_waits: BTreeSet<Waiting>) -> Did {
        let k = key_of(w);
        let Some(e) = self.ops.entry.remove(&k) else {
            // Absent: nothing waits. A late or duplicate answer, counted.
            let r = self.op_resend_of(w);
            self.answer_late(w, r, self.lane_of(w), false);
            return Did::Nothing;
        };
        match e {
            Entry::OnWire { lane, send, by, first_at, riders, again, next } => {
                let attempt = send.attempt;
                // Sent AT its key's bound while the previous send may still run (F1): this answer may be either's --
                // the same answer, whichever (L0's reason: late answers are INERT) -- so it is counted after the bound.
                if send.overlap {
                    self.answer_late(w, resend(&send.op), lane, true);
                }
                // Its one end: ANSWERED while it served a wait, WITHDRAWN once none wanted it.
                let how = if riders.is_empty() { End::Withdrawn } else { End::Answered };
                self.record_end(&by, &send, how);
                if !riders.is_empty() {
                    self.answered_send(&k, &send, lane);
                }
                // Who parks: every rider (`park`), the read's named waits, or nobody.
                let parked: BTreeSet<Waiting> = if park.is_some() { riders.clone() } else { riders.intersection(&park_waits).cloned().collect() };
                let served: Vec<Waiting> = riders.iter().filter(|r| !parked.contains(r)).cloned().collect();
                let park_op = park.unwrap_or_else(|| send.op.clone());
                let answered_someone = !riders.is_empty();
                self.after_answer(&k, lane, attempt, first_at, &send.op, park_op, parked, again, next);
                // A send that served no wait (all withdrawn, or its payload superseded) answers nobody.
                if answered_someone {
                    Did::Answered { op: send.op, attempt, served }
                } else {
                    Did::Nothing
                }
            }
            Entry::Queued { lane, attempt, op, waiters, .. } | Entry::Parked { lane, attempt, op, waiters, .. } if resend(&op) == Resend::AfterBound && attempt >= 1 => {
                // An AfterBound key's answer with no send in flight: a send past its bound answering LATE. It verifies
                // itself, so it SERVES its waits; counted as an answer after the bound.
                self.answer_late(w, Resend::AfterBound, lane, true);
                if let Some(pop) = park {
                    // Answered WITHOUT it: its waits park (not served), in their lane now (L3).
                    let lane = if lane == Lane::Interactive { Lane::Interactive } else { self.key_lane(&waiters) };
                    let at = self.now + self.backoff(attempt);
                    self.ops.entry.insert(k, Entry::Parked { lane, at, attempt, op: pop, first_at: self.now, waiters });
                    return Did::Answered { op, attempt, served: Vec::new() };
                }
                let served: Vec<Waiting> = waiters.iter().cloned().collect();
                Did::Answered { op, attempt, served }
            }
            e @ (Entry::Queued { .. } | Entry::Parked { .. }) => {
                // No send in flight: a late or duplicate answer, counted; the entry stands.
                let (r, lane) = (resend(e.op()), e.lane());
                self.ops.entry.insert(k, e);
                self.answer_late(w, r, lane, false);
                Did::Nothing
            }
        }
    }

    /// How a wait's key re-sends, when its entry is gone: by the op kind its key names.
    fn op_resend_of(&self, w: &Waiting) -> Resend {
        match key_of(w) {
            Key::Put(_) | Key::PutApp(_) | Key::Update(Label::Head) => Resend::Duplicates,
            Key::Update(Label::Site(_)) | Key::Sign(_) | Key::Held(_) | Key::Ext(_) => Resend::OnlyOne,
            Key::Get(_) | Key::Read(_) => Resend::AfterBound,
        }
    }

    /// THE ONE COUNTER (OP-LIFE.md): an answer after its send's bound, or a duplicate, by (resend, lane, served). A
    /// dropped one is also recorded as sdk#447's `DroppedMsgs = AnsweredAfterReask`.
    fn answer_late(&mut self, w: &Waiting, r: Resend, lane: Lane, served: bool) {
        *self.ops.late.entry((r as u8, lane == Lane::Background, served)).or_insert(0) += 1;
        if !served {
            if let Some(rec) = &self.rec {
                use instrument::{vocab::DropReason, Entry as IEntry, Event, Key as IKey, OpId, Probe};
                let entry = IEntry { key: IKey::DroppedMsgs, value: DropReason::AnsweredAfterReask.code() };
                rec.event(Event::Counter { site: op_site(w), op: OpId::NONE, entry });
            }
        }
    }

    /// After a key's send was answered: its parked waits park, and a waiting payload or a fresh send goes (F1: `next`
    /// only after an ANSWER -- this one).
    #[allow(clippy::too_many_arguments)]
    fn after_answer(&mut self, k: &Key, lane: Lane, attempt: u32, first_at: u64, sent: &Op, park_op: Op, parked: BTreeSet<Waiting>, again: BTreeSet<Waiting>, next: Option<(Op, BTreeSet<Waiting>)>) {
        if let Some((op, ws)) = next {
            // The waiting payload goes now, with every wait that still wants the key.
            let ws: BTreeSet<Waiting> = ws.into_iter().chain(again).chain(parked).collect();
            let by = ws.iter().next().cloned().expect("a waiting payload has a wait");
            let lane = self.key_lane(&ws);
            self.send_new(k, &by, op, lane, 0, None, ws, false);
            return;
        }
        if !again.is_empty() {
            // A FRESH send of the same op, for the waits that needed one sent after their X.
            let ws: BTreeSet<Waiting> = again.into_iter().chain(parked).collect();
            let by = ws.iter().next().cloned().expect("non-empty");
            let lane = self.key_lane(&ws);
            self.send_new(k, &by, sent.clone(), lane, 0, None, ws, false);
            return;
        }
        if !parked.is_empty() {
            // Parking is no send: the parked key takes its waits' lane NOW (L3: a wait promoted while it rode a
            // Background send is never parked in the Background lane).
            let lane = if lane == Lane::Interactive { Lane::Interactive } else { self.key_lane(&parked) };
            let at = self.now + self.backoff(attempt);
            self.ops.entry.insert(k.clone(), Entry::Parked { lane, at, attempt, op: park_op, first_at, waiters: parked });
        }
    }

    /// The answer to a key's send: the RTT sample rules (DEADLINES), the re-arm of every first send, and a window step
    /// for an interactive GET.
    fn answered_send(&mut self, k: &Key, d: &Send, lane: Lane) {
        // A QUEUED first send answered: the back-off ends (RFC 6298 §5.7), but its time is its wait too: no sample.
        if d.attempt == 1 && !d.resent && !d.alone && !d.silent {
            self.rto.answered_queued();
        }
        // A SILENT send's time is the node's, not the path's: never a sample (sdk#447).
        if d.attempt == 1 && !d.resent && d.alone && !d.silent {
            let r = self.now.saturating_sub(d.sent_at);
            // IMPOSSIBLE, so loud: a round trip longer than the page has existed was dated against another clock's
            // origin (sdk#397).
            debug_assert!(r <= self.now.saturating_sub(self.born), "an RTT sample of {r} ms on a page {} ms old: a request was dated against another clock's origin", self.now.saturating_sub(self.born));
            self.rto.sample(r);
            // The sample AS COMPUTED, before anything clamps it (the architect): a wrong clock shows here.
            if let Some(rec) = self.rec.as_ref().filter(|_| d.seq >= self.rec_from) {
                use instrument::{vocab::coarsen_ms, Entry as IEntry, Event, Key as IKey, Probe};
                if let Some(id) = self.send_label(d.seq) {
                    let (site, op) = (op_site_of_key(k), id.op());
                    rec.event(Event::Counter { site, op, entry: IEntry { key: IKey::SampleMs, value: coarsen_ms(r) } });
                    rec.event(Event::Counter { site, op, entry: IEntry { key: IKey::RtoMs, value: coarsen_ms(self.rto.rto_ms()) } });
                }
            }
        }
        // ANY answer re-arms (sdk#390 (b)).
        self.rearm_first_sends();
        if lane == Lane::Interactive && matches!(k, Key::Get(_)) {
            self.window.opened();
        }
    }

    /// EVERY answer restarts the timer of every FIRST send still on the wire from THIS answer, at its place in the
    /// node's one queue now, never later than the deadline it was sent with (sdk#378, sdk#390; DEADLINES rule 2).
    fn rearm_first_sends(&mut self) {
        use instrument::{Entry as IEntry, Event, Key as IKey, Kind, Probe};
        let now = self.now;
        let rto = self.rto.rto_ms();
        let (rec, start, from) = (&self.rec, self.rec_start, self.rec_from);
        let mut wire: Vec<(&Waiting, &mut Send)> = self
            .ops
            .entry
            .values_mut()
            .filter_map(|e| match e {
                Entry::OnWire { send, by, .. } => Some((&*by, send)),
                Entry::Queued { .. } | Entry::Parked { .. } => None,
            })
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
                            rec.event(Event::Counter { site: op_site(w), op, entry: IEntry { key: IKey::ReArmedAtMs, value } });
                        }
                    }
                }
                d.at = at;
            }
        }
    }

    /// WITHDRAW: nobody needs it for `w`. Queued or parked with no wait left, the key is Absent. ON THE WIRE the send
    /// STAYS on its key (and, if Background, in the slot) until it leaves -- its answer or its bound -- its one end
    /// recorded then.
    fn on_withdraw(&mut self, w: &Waiting) {
        let k = key_of(w);
        let Some(e) = self.ops.entry.remove(&k) else { return };
        match e {
            Entry::Queued { lane, attempt, op, order, first_at, mut waiters, overlap } => {
                waiters.remove(w);
                if !waiters.is_empty() {
                    self.ops.entry.insert(k, Entry::Queued { lane, attempt, op, order, first_at, waiters, overlap });
                }
            }
            Entry::Parked { lane, at, attempt, op, first_at, mut waiters } => {
                waiters.remove(w);
                if !waiters.is_empty() {
                    self.ops.entry.insert(k, Entry::Parked { lane, at, attempt, op, first_at, waiters });
                }
            }
            Entry::OnWire { lane, send, by, first_at, mut riders, mut again, next } => {
                riders.remove(w);
                again.remove(w);
                let next = next.and_then(|(op, mut ws)| {
                    ws.remove(w);
                    (!ws.is_empty()).then_some((op, ws))
                });
                // A RtoResend key is not bound by L0 (its re-sends duplicate by design): withdrawn by all, it LEAVES at
                // once, its one end Withdrawn; a late answer is then a counted duplicate. Any other stays on its key.
                if riders.is_empty() && again.is_empty() && next.is_none() && in_flight(resend(&send.op), lane) == InFlight::RtoResend {
                    self.record_end(&by, &send, End::Withdrawn);
                    return;
                }
                self.ops.entry.insert(k, Entry::OnWire { lane, send, by, first_at, riders, again, next });
            }
        }
    }

    /// PLACE-FREES (L2): the slot to the OLDEST queued background key (round-robin: a timed-out key YIELDS to the back),
    /// and the window's queue in order.
    fn place_frees(&mut self) {
        while self.ops.in_slot() == 0 {
            let next = self
                .ops
                .entry
                .iter()
                .filter_map(|(k, e)| match e {
                    Entry::Queued { lane: Lane::Background, order, .. } => Some((*order, k.clone())),
                    Entry::Queued { lane: Lane::Interactive, .. } | Entry::OnWire { .. } | Entry::Parked { .. } => None,
                })
                .min()
                .map(|(_, k)| k);
            let Some(k) = next else { break };
            let Some(Entry::Queued { attempt, op, first_at, waiters, overlap, .. }) = self.ops.entry.remove(&k) else { unreachable!("picked a queued key") };
            // A key its waits now class Interactive goes as that.
            let lane = self.key_lane(&waiters);
            let by = waiters.iter().next().cloned().expect("a queued key has a wait");
            self.put_on_wire(&k, &by, op, lane, attempt + 1, first_at, waiters, overlap);
        }
        while self.ops.in_window() < self.window.size() {
            let next = self
                .ops
                .entry
                .iter()
                .filter_map(|(k, e)| match e {
                    Entry::Queued { lane: Lane::Interactive, order, .. } => matches!(k, Key::Get(_)).then(|| (*order, k.clone())),
                    Entry::Queued { lane: Lane::Background, .. } | Entry::OnWire { .. } | Entry::Parked { .. } => None,
                })
                .min()
                .map(|(_, k)| k);
            let Some(k) = next else { break };
            let Some(Entry::Queued { attempt, op, first_at, waiters, overlap, .. }) = self.ops.entry.remove(&k) else { unreachable!("picked a queued key") };
            let by = waiters.iter().next().cloned().expect("a queued key has a wait");
            self.put_on_wire(&k, &by, op, Lane::Interactive, attempt + 1, first_at, waiters, overlap);
        }
    }

    /// THE CLOCK's part of the record: a reconnect's owed slot frees at its bound; a `OneInFlight` send past its RTO
    /// goes SILENT until its node bound (shown, nothing sent, no RTO back-off); the RTO backs off ONCE if any send
    /// timed out; a lost interactive GET is a window loss. Returns the waits whose keys are now due, one per key, for
    /// [`Page::on_due`].
    pub(crate) fn tick_ops(&mut self, now: u64) -> Vec<Waiting> {
        if self.ops.slot_owed_until.is_some_and(|at| now >= at) {
            self.ops.slot_owed_until = None;
            // L2: the slot the debt held is free NOW: the oldest queued Background key takes it.
            self.place_frees();
        }
        let rto_now = self.rto.rto_ms();
        let due: Vec<Key> = self.ops.entry.iter().filter(|(_, e)| e.due().is_some_and(|at| now >= at)).map(|(k, _)| k.clone()).collect();
        let mut late = Vec::new();
        let mut timed_out = false;
        for k in due {
            let Some(e) = self.ops.entry.get_mut(&k) else { continue };
            let rep = e.waiters().next().cloned();
            match e {
                Entry::OnWire { lane, send, .. } => {
                    let one = in_flight(resend(&send.op), *lane) == InFlight::OneInFlight;
                    // SILENT, not lost: inside its node bound nothing is sent, the RTO does not back off, the window
                    // does not halve (sdk#447, generalised by L0).
                    if one && !send.silent && now < bound_at(&send.op, send.sent_at, rto_now) {
                        send.silent = true;
                        send.at = bound_at(&send.op, send.sent_at, rto_now);
                        continue;
                    }
                    timed_out = true;
                    if *lane == Lane::Interactive && matches!(k, Key::Get(_)) {
                        self.window.lost(send.sent_at, send.attempt, now);
                    }
                }
                Entry::Parked { .. } | Entry::Queued { .. } => {}
            }
            late.push(rep.unwrap_or_else(|| waiting_of_key(&k)));
        }
        // RFC 6298 §5.5 -- ONCE per tick, however many timed out. A PARKED op coming due timed nothing out.
        if timed_out {
            self.rto.timed_out();
        }
        late
    }

    /// A KEY's deadline (the table's bound row, a RtoResend key's RTO, a parked key's re-ask), after
    /// [`Page::tick_ops`]. `w` is any wait of the key.
    pub(crate) fn on_due(&mut self, w: &Waiting) -> Due {
        let k = key_of(w);
        let due = match self.ops.entry.remove(&k) {
            Some(Entry::OnWire { lane, send, by, first_at, riders, again, next }) => {
                let mut ws: BTreeSet<Waiting> = riders.into_iter().chain(again).collect();
                if ws.is_empty() && next.is_none() {
                    // Nobody wants it: its one end is WITHDRAWN (nothing is sent again), and the key is Absent.
                    self.record_end(&by, &send, End::Withdrawn);
                    Due::Handled
                } else {
                    self.record_end(&by, &send, End::TimedOut);
                    self.ops.order += 1;
                    let order = self.ops.order;
                    // L6: never demoted -- a key sent Interactive re-sends Interactive, whatever its waits say now.
                    let now_lane = if lane == Lane::Interactive { Lane::Interactive } else { self.key_lane(ws.iter().chain(next.iter().flat_map(|(_, n)| n.iter()))) };
                    let rto_resend = in_flight(resend(&send.op), lane) == InFlight::RtoResend;
                    let sign = matches!(k, Key::Sign(_));
                    match next {
                        // An id-checked kind may send its waiting payload at the bound: its id makes the old send's
                        // answer harmless (L0).
                        Some((op, n)) if request_of(&op).is_some() && !rto_resend => {
                            ws.extend(n);
                            self.send_new(&k, &by, op, now_lane, send.attempt, Some(first_at), ws, false);
                            Due::Handled
                        }
                        next => {
                            // THE SAME PAYLOAD again (F1): a waiting payload goes only after an answer, so it rides the
                            // re-send as `next`. Only the next payload wanting the key still re-sends this one, so an
                            // answer can come.
                            if ws.is_empty() {
                                ws.extend(next.iter().flat_map(|(_, n)| n.iter().cloned()));
                            }
                            // The re-send overlaps the send it replaces unless RTO re-sends are this key's rule (they
                            // overlap by design) or its id is fresh (a Sign).
                            let overlap = !rto_resend && !sign;
                            if now_lane == Lane::Interactive && (rto_resend || sign) {
                                // Its OWNER re-sends: a Sign rebuilt under a FRESH id (PUBLISH-LIFE's SignLost); a
                                // RtoResend key the same bytes (an UPDATE's landing counts it).
                                self.ops.entry.insert(k.clone(), Entry::Queued { lane: Lane::Interactive, attempt: send.attempt, op: send.op.clone(), order, first_at: Some(first_at), waiters: ws, overlap: false });
                                self.stash_next(&k, next);
                                Due::ReSend { op: send.op, attempt: send.attempt }
                            } else if now_lane == Lane::Background {
                                // A background key YIELDS: to the back of the queue, the slot freed (defect 4).
                                self.ops.entry.insert(k.clone(), Entry::Queued { lane: Lane::Background, attempt: send.attempt, op: send.op, order, first_at: Some(first_at), waiters: ws, overlap });
                                self.stash_next(&k, next);
                                Due::Handled
                            } else if matches!(k, Key::Get(_)) {
                                // A LOST GET: its re-send QUEUES for a window place, behind the asks already waiting
                                // (sdk#345; rule 9: every byte is paced).
                                self.ops.entry.insert(k.clone(), Entry::Queued { lane: Lane::Interactive, attempt: send.attempt, op: send.op, order, first_at: Some(first_at), waiters: ws, overlap });
                                self.stash_next(&k, next);
                                Due::Handled
                            } else {
                                // Every other interactive key: the same payload now.
                                self.send_new(&k, &by, send.op, Lane::Interactive, send.attempt, Some(first_at), ws, overlap);
                                self.stash_next(&k, next);
                                Due::Handled
                            }
                        }
                    }
                }
            }
            // A PARKED key coming due: asked again (not a timeout), or ENDED if nobody needs it (engine-withdraw's
            // due-time door).
            Some(Entry::Parked { lane, attempt, op, first_at, waiters, .. }) => {
                // The key ENDS only when no reader is left (sdk#531 E2); held bytes alone never end it.
                let unneeded = if let Key::Get(id) = &k { !self.engine.readers_of(id).any() } else { false };
                if !unneeded {
                    let by = waiters.iter().next().cloned().expect("a parked key has a wait");
                    self.send_new(&k, &by, op, lane, attempt, Some(first_at), waiters, false);
                }
                Due::Handled
            }
            Some(e @ Entry::Queued { .. }) => {
                self.ops.entry.insert(k, e);
                Due::Handled
            }
            None => Due::Handled,
        };
        self.place_frees();
        self.assert_l1();
        due
    }

    /// A waiting payload rides a key's re-send as its `next` (it goes after that send's answer, F1). A key not on the
    /// wire (queued) carries it forward by taking it as its payload when it goes: the newest wins.
    fn stash_next(&mut self, k: &Key, next: Option<(Op, BTreeSet<Waiting>)>) {
        let Some((op, ws)) = next else { return };
        match self.ops.entry.remove(k) {
            Some(Entry::OnWire { lane, send, by, first_at, riders, again, next: _ }) => {
                self.ops.entry.insert(k.clone(), Entry::OnWire { lane, send, by, first_at, riders, again, next: Some((op, ws)) });
            }
            Some(Entry::Queued { lane, attempt, order, first_at, mut waiters, overlap, op: queued }) => {
                // Not yet sent again, and its re-send is still the SAME payload (F1): the waiting one goes after that
                // send's answer. With nothing to overlap (no send ran to its bound), the newest payload is the one.
                waiters.extend(ws);
                let op = if overlap { queued } else { op };
                self.ops.entry.insert(k.clone(), Entry::Queued { lane, attempt, op, order, first_at, waiters, overlap });
            }
            Some(e @ Entry::Parked { .. }) => {
                self.ops.entry.insert(k.clone(), e);
            }
            None => {}
        }
    }

    /// After a key's owner had its turn: a re-send it did not make ends the key.
    pub(crate) fn end_unsent(&mut self, w: &Waiting) {
        let k = key_of(w);
        if matches!(self.ops.entry.get(&k), Some(Entry::Queued { lane: Lane::Interactive, .. })) && !matches!(k, Key::Get(_)) {
            self.ops.entry.remove(&k);
        }
    }

    /// RECONNECT: every key is FREE at once (L0 is per socket). Each send in flight ends Withdrawn; a wanted key is
    /// re-sent on the new socket in the lane `lane_of` gives it NOW (F3): interactive at once, background queued for the
    /// slot. A Background send becomes the slot's debt until its bound (L1: the node still runs it), unless its key was
    /// promoted since (it no longer needs the slot). Returns whether a head read was among the re-sends.
    pub(crate) fn reconnect_ops(&mut self) -> bool {
        let rto_now = self.rto.rto_ms();
        let mut on_wire: Vec<(u32, Key)> = self.ops.entry.iter().filter_map(|(k, e)| e.send().map(|s| (s.seq, k.clone()))).collect();
        on_wire.sort_unstable();
        let mut head_read = false;
        for (_, k) in on_wire {
            let Some(Entry::OnWire { lane, send, by, first_at, riders, again, next }) = self.ops.entry.remove(&k) else { unreachable!("listed") };
            self.record_end(&by, &send, End::Withdrawn);
            let ws: BTreeSet<Waiting> = riders.into_iter().chain(again).collect();
            let now_lane = if lane == Lane::Interactive { Lane::Interactive } else { self.key_lane(ws.iter().chain(next.iter().flat_map(|(_, n)| n.iter()))) };
            if lane == Lane::Background && now_lane == Lane::Background {
                // The send's own bound as the page set it (a silent send's `at`; else its bound now).
                let owed = if send.silent { send.at } else { bound_at(&send.op, send.sent_at, rto_now) };
                self.ops.slot_owed_until = Some(self.ops.slot_owed_until.map_or(owed, |o| o.max(owed)));
            }
            // What is wanted goes again: the waiting payload first (nothing of the key is in flight on this socket).
            let (op, ws) = match next {
                Some((op, n)) => (op, ws.into_iter().chain(n).collect::<BTreeSet<_>>()),
                None => (send.op.clone(), ws),
            };
            if ws.is_empty() {
                continue;
            }
            head_read |= matches!(op, Op::ReadHead { label: Label::Head });
            match now_lane {
                Lane::Interactive => {
                    let at = self.now + self.backoff(send.attempt);
                    let seq = self.next_send();
                    let resent = Send { op: op.clone(), at, armed: at, sent_at: self.now, attempt: send.attempt, resent: true, seq, alone: false, silent: false, overlap: false };
                    self.record_send(&by, &resent);
                    self.out.push(op);
                    self.ops.entry.insert(k, Entry::OnWire { lane: Lane::Interactive, send: resent, by, first_at, riders: ws, again: BTreeSet::new(), next: None });
                }
                Lane::Background => {
                    self.ops.order += 1;
                    let order = self.ops.order;
                    self.ops.entry.insert(k, Entry::Queued { lane: Lane::Background, attempt: send.attempt, op, order, first_at: Some(first_at), waiters: ws, overlap: false });
                }
            }
        }
        self.place_frees();
        self.assert_l1();
        head_read
    }
}

/// A wait standing for a key with none left (a key is only due while it has an entry; this names it for the tick).
fn waiting_of_key(k: &Key) -> Waiting {
    match k {
        Key::Put(id) => Waiting::Put(*id),
        Key::Get(id) => Waiting::Get(*id),
        Key::Held(b) => Waiting::Held(*b),
        Key::Sign(l) => Waiting::Sign(l.clone()),
        Key::Update(l) => Waiting::Update(l.clone()),
        Key::Read(l) => Waiting::ReadBack(l.clone()),
        Key::PutApp(s) => Waiting::PutApp(s.clone(), Ask::Direct(String::new())),
        Key::Ext(e) => Waiting::Ext(*e),
    }
}

/// The recording's site for a key's send.
fn op_site_of_key(k: &Key) -> instrument::Site {
    op_site(&waiting_of_key(k))
}

#[cfg(test)]
mod test_view {
    //! THE TESTS' READ-ONLY VIEW of the record, derived from it.
    use super::*;

    /// THE TESTS' READ-ONLY VIEW of the record, in the old `Deadline`'s terms (the deadline-table pins read these names):
    /// every wait with a deadline (on the wire or parked), DERIVED from the record, never a second one.
    #[allow(dead_code)]
    #[derive(Debug, Clone)]
    pub(crate) struct DeadlineView {
        pub(crate) lane: Lane,
        /// Its key's send serves no wait any more (withdrawn); it still occupies its key.
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

    impl Page {
        /// Every deadline, per wait: each wait whose key is on the wire or parked (a withdrawn send under the wait that
        /// asked it).
        pub(crate) fn dl(&self) -> BTreeMap<Waiting, DeadlineView> {
            let mut out = BTreeMap::new();
            for e in self.ops.entry.values() {
                match e {
                    Entry::OnWire { lane, send, riders, .. } => {
                        let view = |withdrawn| DeadlineView { lane: *lane, withdrawn, at: send.at, armed: send.armed, op: send.op.clone(), sent_at: send.sent_at, attempt: send.attempt, sent: true, resent: send.resent, seq: send.seq, alone: send.alone, silent: send.silent };
                        // A send no wait wants any more waits on nobody: it is in no wait's deadline.
                        for w in riders {
                            out.insert(w.clone(), view(false));
                        }
                    }
                    Entry::Parked { lane, at, attempt, op, waiters, .. } => {
                        for w in waiters {
                            out.insert(w.clone(), DeadlineView { lane: *lane, withdrawn: false, at: *at, armed: *at, op: op.clone(), sent_at: 0, attempt: *attempt, sent: false, resent: false, seq: 0, alone: false, silent: false });
                        }
                    }
                    Entry::Queued { .. } => {}
                }
            }
            out
        }

        /// GETs holding a window place.
        pub(crate) fn gets_in_flight(&self) -> usize {
            self.ops.in_window()
        }

        /// Does a need for `w` keep attempts already made (a lost GET queued, a parked op)?
        pub(crate) fn attempt_kept(&self, w: &Waiting) -> bool {
            self.ops.need(w).is_some_and(|e| e.attempt() >= 1)
        }

        /// A send made as its `attempt`-th (a deadline-table pin's re-send), through the one door onto the wire.
        pub(crate) fn send_attempt(&mut self, w: Waiting, op: Op, attempt: u32) {
            let lane = self.lane_of(&w);
            let k = key_of(&w);
            self.put_on_wire(&k, &w, op, lane, attempt, None, BTreeSet::from([w.clone()]), false);
        }

        /// Interactive GETs waiting for a window place, in queue order.
        pub(crate) fn get_queued(&self) -> Vec<Cid> {
            let mut q: Vec<(u64, Cid)> = self
                .ops
                .entry
                .iter()
                .filter_map(|(k, e)| match (k, e) {
                    (Key::Get(id), Entry::Queued { lane: Lane::Interactive, order, .. }) => Some((*order, *id)),
                    _ => None,
                })
                .collect();
            q.sort_unstable();
            q.into_iter().map(|(_, id)| id).collect()
        }

        /// Background keys waiting for the slot, in queue order, each by its first wait.
        pub(crate) fn bg_queued(&self) -> Vec<(Waiting, Op)> {
            let mut q: Vec<(u64, Waiting, Op)> = self
                .ops
                .entry
                .values()
                .filter_map(|e| match e {
                    Entry::Queued { lane: Lane::Background, order, op, waiters, .. } => waiters.iter().next().map(|w| (*order, w.clone(), op.clone())),
                    Entry::Queued { lane: Lane::Interactive, .. } | Entry::OnWire { .. } | Entry::Parked { .. } => None,
                })
                .collect();
            q.sort_unstable_by_key(|(o, ..)| *o);
            q.into_iter().map(|(_, w, op)| (w, op)).collect()
        }

        /// The slot's debt from a reconnect, if any.
        pub(crate) fn slot_owed_until(&self) -> Option<u64> {
            self.ops.slot_owed_until
        }
    }
}

#[cfg(test)]
mod lane;
#[cfg(test)]
mod model;
#[cfg(test)]
mod named;
#[cfg(test)]
mod policy;
#[cfg(test)]
mod scenarios;
