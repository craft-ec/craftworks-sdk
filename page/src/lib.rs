//! # The engine library IN THE PAGE (ENGINE-SHAPE §6, sdk#213)
//!
//! The real [`engine::Engine`], run by the page, and a sans-IO EXECUTOR that
//! turns each [`Effect`] into client-API operations. It replaces what
//! engine-delegate's shell did inside a delegate: the delegate keeps only its
//! three verbs (put-with-code, read-local, sign-if-next), and the SIGNER
//! (sdk#209) is the one that owns the key.
//!
//! Sans-IO, like the engine: the page tells [`Page`] what ARRIVED ([`Answer`])
//! and what time it is, and takes back the [`Op`]s to send. Framing is not
//! here: an `Op` is turned into client-API bytes by the web layer (`wire`,
//! the signer's framing), and a node's answer is decoded there into an
//! `Answer`. So everything below is driven natively by a scripted client API
//! (`tests/model.rs`) with the REAL signer on the far side.
//!
//! ## What each effect becomes (the executor's table)
//!
//! | effect / answer | what the page does |
//! |---|---|
//! | `PutBlock` / `PutParity` | the bytes join [`PageBlocks`] (the page is now the memory a node was); held until its `after` set is confirmed, then [`Op::Put`] |
//! | `PutOk` | [`PutPath::Page`]: `PutConfirmed` on the PUT's answer (a page-PUT block is served, measured 20/20; no per-block read-back). [`PutPath::Wrapper`]: [`Op::AskHeld`], and `PutConfirmed` only on `Held { present: true }`; absent → asked again on a doubling backoff, the PUT again only after [`HELD_ABSENTS`] absents in a row |
//! | `PutRefused { transient }` | transient (F51's queue): the same PUT again at the next tick; permanent: `PutFailed` |
//! | `PutPack` | refused as the shell refuses it (no packs in this phase): `PutFailed` |
//! | `UpdateHead { seq, root }` | held until its `after` is confirmed, then [`Op::Sign`] from the engine's PUBLISHED head |
//! | `Signed(state)` (`signer_proto::Answer` under the in-flight sign's id, as `wire::signer::read_answer` decodes it; any other id is ignored) | [`Op::Update`] with exactly those bytes |
//! | `AlreadySigned(state)` | [`Op::Update`] with exactly those bytes (the signer's requirement 2: at most one signature per prev). If its root is another page's, the read-back shows this seq under that root: `HeadConflict` |
//! | `NotNext { current }` | NOT adopted yet (1b): the register is READ first (`Page::on_verify`) |
//! | … the register holds `current`, or a later head | `HeadConflict` onto it: the engine adopts it and reports the dead commit's writes `Lost`; the app submits them again, each checked against its READS where it lands (#218) |
//! | … the register is ONE behind (the signer's record is ahead, its UPDATE unlanded) | LAND it: Sign from the register's head with `next.seq = register.seq + 1` (any root) under a fresh id → `AlreadySigned(record)` (`signer::decide`'s `r.prev == *prev` arm; signer/tests/sign.rs `two_requests_from_one_prev_in_sequence_get_one_signature`) → [`Op::Update`] those bytes → read; adopted once the register shows it. Safe by invariant 0; two pages landing the same bytes is an equal decision, held bytes win. RESIDUAL: a page whose signer answered before ITS blocks were stored leaves holes, surfacing here |
//! | … the register is 2+ behind | a NAMED failure in [`Page::unusable`] — unrecoverable (one record per signer) and unreachable by 1b |
//! | … the register does not answer | read again on its deadline; after [`VERIFY_BUDGET_MS`], "the register is not answering" — nothing adopted |
//! | `Refused(RootNotHeld / HeadUnknown / RecordNotSaved)` | the same sign request after a doubling backoff from [`BACKOFF_MS`]; for `HeadUnknown` the register is READ first, which makes the signer's node hold it |
//! | `Refused(Forked)` | LOUD: [`Page::forked`] and `unusable`; never retried — a fork is news |
//! | a signer answer under any id but the in-flight sign's (SG02), or not shaped like a sign's | ignored: it does NOT clear the sign's deadline |
//! | any other `Refused(why)` | nothing more is asked; recorded in [`Page::unusable`]; the engine's own clock reports the write `Stalled` |
//! | `Updated` | a register read ([`Op::ReadHead`]) — this commit's read-back, or a landing's verify read: an UpdateResponse carries nothing (F56). On a peered node the register is read through the head SUBSCRIPTION (F55); the web layer frames it so |
//! | `Head(Some(mine))` while a head is owed | `HeadConfirmed(seq)` — the only way a commit is Published |
//! | `Head(older seq)` while owed | not visible yet: the head is read again next tick, and after [`HEAD_READS`] reads the UPDATE is re-sent |
//! | `Head(newer seq, or same seq another root)` while owed | `HeadConflict` |
//! | `Head(None)` while owed | the UPDATE is re-sent |
//! | `Head(..)` for the engine's own `ReadHead` | `HeadRead` / `HeadMissing` (recovery) — kept apart from a read-back, which only judges the owed head |
//! | `FetchBlock` | [`Op::Get`]; `Got` → verified, joins [`PageBlocks`], `BlockArrived`; `GetMissed` → `BlockMissed` |
//! | any op unanswered for its RTO ([`rto`]: RFC 6298 over this page's own completed calls, Karn, §5.5 back-off; GETs also in a congestion window) | re-sent as it was — every op here is idempotent (a block is its hash, a sign re-ask is answered AlreadySigned, a record is the same record). No fixed deadline anywhere: the 30 s budgets are give-ups, not retry timers |
//!
//! ## Invariants (each asserted by the model test, each with a mutant)
//! 0. A signer record exists only after its commit's blocks were put and
//!    confirmed: `Page::release` lets an `UpdateHead` leave only when its
//!    whole `after` set is confirmed. (What makes LAND safe.)
//! 1. A `Published` write's effect is in a head H the register was READ to
//!    hold, and every later register head descends from H through the
//!    signer's records — or a page reported the fork, LOUDLY. (A write that
//!    changes nothing is Published at the engine's published head, sdk#160,
//!    which may already be behind the register: still true.)
//!    - **1b.** The engine never ADOPTS a head (HeadConflict / HeadRead) the
//!      register has not been read to hold, and so a page signs only from
//!      such a head: the register is never 2+ behind the signer's record.
//! 2. The page UPDATEs the register only with bytes the signer returned.
//! 3. A published root is WHOLE on the node: every block it reaches was put
//!    and answered before the head was signed.
//! 4. Every write ends `Published` once the faults stop (bar a fork).
//!
//! ## Not here (next PRs)
//! * the SDK half of the read set (#148): `Db`'s own ops declaring reads;
//! * a persisted outbox / RELOAD beyond what the signer's record gives (a
//!   fresh page on the same key is answered `AlreadySigned` and lands it);
//! * the web Session over [`server::Server`], and provisioning the signer (B2);

pub mod rto;
pub mod server;

use engine::{ClientId, Effect, Engine, Epoch, Event, KeySource, Op as WriteOp, Params, State, WriteId};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Register reads that may still show the OLDER head after an UPDATE before
/// the UPDATE itself is re-sent. ASSUMPTION (the live run tells): a page's
/// own UPDATE is visible to its own GET within a few reads.
pub const HEAD_READS: u32 = 3;

/// [`PutPath::Wrapper`]: `Held` answers "absent" this many times before the
/// block is PUT again. The signer's own PUTs go out AFTER it answers
/// `Putting`, so an early absent can be honest (engineer2's review of
/// sdk#215); a re-PUT is harmless but re-sends the block.
pub const HELD_ABSENTS: u32 = 3;

/// The first wait before a backed-off re-ask (a `Held` absent, a retryable
/// signer refusal); it doubles each time, capped by [`rto::RTO_MAX_MS`].
pub const BACKOFF_MS: u64 = 100;

/// A time on the PAGE's clock, in MILLISECONDS. Every clock parameter of
/// [`Page`] and [`server::Server`] takes this, never a bare `u64`, so the
/// engine's clock — SECONDS, the protocol's `Tick` — cannot be handed a page
/// time by mistake (#215 did: every engine timer ran 1,000× fast).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Ms(pub u64);

/// THE one place a page time becomes engine time: whole SECONDS.
fn engine_seconds(now: Ms) -> u64 {
    now.0 / 1_000
}

/// The only code epoch this build writes under.
const EPOCH: Epoch = Epoch(1);

/// The blocks the page holds: every block it wrote and every block it
/// fetched, each verified against its id.
///
/// APPEND-ONLY for the page's life: the engine owns its `Blocks` and only
/// lends `&B`, and a `&[u8]` it was handed must stay valid, so nothing is ever
/// removed. The BOUND this implies: a page holds every block of every commit
/// it made and every block it read in this session — for a long session over
/// a large tree that is the tree's size in memory, until the page is closed.
/// An LRU needs the engine to stop holding borrows across calls (a follow-up,
/// not this PR).
#[derive(Clone, Default)]
pub struct PageBlocks(Rc<elsa::FrozenBTreeMap<Cid, Box<[u8]>>>);

impl PageBlocks {
    fn insert(&self, id: Cid, bytes: &[u8]) {
        if self.0.get(&id).is_none() {
            self.0.insert(id, bytes.to_vec().into_boxed_slice());
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Blocks for PageBlocks {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.get(cid)
    }
}

/// Where a block's PUT goes, and so what CONFIRMS it (`PutConfirmed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutPath {
    /// The page PUTs to its OWN node through the client API. The PUT's answer
    /// confirms it (main's ruling for sdk#213: a page-PUT block is served,
    /// measured 20/20).
    Page,
    /// The signer puts it (put-with-code, the wrapper path). No PUT answer
    /// reaches the page's connection (sdk#214: 0 in 5 s), and a client GET of
    /// a delegate-put block on a peered node is NotFound (F55). So a put's
    /// answer confirms NOTHING: the page asks the signer's read-local verb
    /// ([`Op::AskHeld`]) and only `Held { present: true }` confirms.
    Wrapper,
}

/// A client-API operation for the web layer to frame and send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// PUT the Block contract for `id` with these bytes (the block's body).
    Put { id: Cid, bytes: Vec<u8> },
    /// GET the Block contract for `id`.
    Get { id: Cid },
    /// Ask the signer to sign `(prev → seq, root)`, as signer request `id`
    /// (SG02): the answer carries it back, and only the answer under the id of
    /// the sign in flight is taken as its answer. A fresh id per ask.
    Sign { id: u32, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid },
    /// UPDATE the head register with exactly these bytes (a signed record).
    Update { state: Vec<u8> },
    /// GET the head register.
    ReadHead,
    /// [`PutPath::Wrapper`] only: ask the signer's read-local verb whether the
    /// node holds this block now.
    AskHeld { id: Cid },
}

/// What arrived, as the web layer decoded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    PutOk(Cid),
    PutRefused { id: Cid, transient: bool },
    Got { id: Cid, bytes: Vec<u8> },
    GetMissed(Cid),
    /// A signer answer and the id of the request it answers, as
    /// `wire::signer::read_answer` decoded it (SG02) — the merged type, not a
    /// copy of it.
    Signer { id: u32, answer: signer_proto::Answer },
    /// The head UPDATE was answered. It says NOTHING about which record the
    /// register kept (F56).
    Updated,
    /// The head register as read: `(seq, root)`, or `None` if there is none.
    Head(Option<(u64, Cid)>),
    /// [`PutPath::Wrapper`]: the signer's synchronous local read of a block.
    Held { id: Cid, present: bool },
}

/// An op in flight.
#[derive(Debug, Clone)]
struct Deadline {
    at: u64,
    op: Op,
    sent_at: u64,
    attempt: u32,
}

/// Which op a deadline is for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Waiting {
    Put(Cid),
    Held(Cid),
    Get(Cid),
    Sign,
    Update,
    /// A register read whose answer nobody judges: it only makes the node
    /// hold the register, so the signer's synchronous read can see a head
    /// (`Refused(HeadUnknown)`).
    Warm,
    /// The engine's own head read (recovery, `Effect::ReadHead`).
    RecoverHead,
    /// A register read that decides what a `NotNext` means (invariant 1b).
    Verify,
    /// This executor's read-back after an UPDATE.
    ReadBack,
}

/// The head this page owes the network: a commit's `(seq, root)` from the
/// engine's `UpdateHead`, and how far it got.
#[derive(Debug, Clone)]
struct Owed {
    seq: u64,
    root: Cid,
    /// The exact record to UPDATE, once the signer returned it.
    record: Option<Vec<u8>>,
    /// Register reads since the last UPDATE that still showed an older head.
    stale_reads: u32,
}

/// A head the signer named that the register has not been read to hold.
#[derive(Debug, Clone)]
struct Verify {
    seq: u64,
    root: Cid,
    /// The register was behind: this page is LANDING the signer's record.
    landing: bool,
    /// Read or land again at this time (a backoff).
    again_at: Option<u64>,
    tries: u32,
    /// UPDATEs this landing has sent (a lost one is re-sent at its deadline).
    updates: u32,
    /// The register head a landing asks FROM (its prev).
    from: Option<(u64, Cid)>,
    /// When the first read went out: the register's own budget (like a cold
    /// fetch's, [`VERIFY_BUDGET_MS`]) runs from here.
    first_at: u64,
}

/// How long the register may stay unreadable, or behind a head the signer
/// named, before this page says "the register is not answering" — and still
/// adopts nothing. The cold reads' per-block budget.
pub const VERIFY_BUDGET_MS: u64 = 30_000;

/// One page's writes: the engine, the executor state, and what to send.
pub struct Page {
    path: PutPath,
    /// The id of the sign request in flight, if one is (SG02); an answer under
    /// any other id answers something else, a superseded ask included.
    sign_id: Option<u32>,
    /// The next signer request id this page issues: from 1, as `0` is
    /// `signer_proto::UNATTRIBUTED`.
    next_request: u32,
    engine: Engine<PageBlocks>,
    blocks: PageBlocks,
    /// Blocks whose PUT was answered ok (an effect's `after` is judged here).
    confirmed: BTreeSet<Cid>,
    /// Effects held until their `after` set is confirmed, in emitted order.
    held: Vec<(BTreeSet<Cid>, Effect)>,
    owed: Option<Owed>,
    /// A sign request to repeat once the clock passes this (a retryable
    /// refusal), and how many times in a row it was refused.
    sign_again: Option<u64>,
    sign_refusals: u32,
    /// [`PutPath::Wrapper`]: blocks to ask `Held` about again, when, and how
    /// many absents in a row.
    held_again: BTreeMap<Cid, (u64, u32)>,
    /// A `NotNext{current}` not yet ADOPTED: the register must be read to hold
    /// it first (invariant 1b).
    verify: Option<Verify>,
    /// Landings started, and the most UPDATEs one landing needed.
    landings: u32,
    most_landing_updates: u32,
    /// The loud one: the signer saw this key's head FORKED (two roots at one
    /// seq). Never retried; a fork is news.
    forked: Option<String>,
    /// A PUT to repeat at the next tick (a transient refusal).
    put_again: BTreeMap<Cid, Vec<u8>>,
    /// Deadlines of the ops in flight, and each op to re-send.
    /// Every op in flight: when it is due again, the op to re-send, when it
    /// went out and on which attempt (Karn: only an attempt-1 answer samples).
    deadlines: BTreeMap<Waiting, Deadline>,
    /// The retry clock of every node call (`rto`), and the GET window.
    rto: rto::Rto,
    window: rto::Window,
    /// GETs waiting for a place in the window, in the order asked.
    get_queue: std::collections::VecDeque<Cid>,
    /// The attempt a re-send continues from (set when an op times out).
    attempt_of: BTreeMap<Waiting, u32>,
    /// When the engine's own head read first went out and got no answer — the
    /// register's budget runs from here — and whether "not answering" was said.
    recover_since: Option<u64>,
    recover_told: bool,
    /// The engine's own head read has been ANSWERED (a head, or certainly
    /// none): until then its tree is the empty one it started on, and a read
    /// answered from it would say "empty" about data that exists.
    recovered: bool,
    /// Has the engine been told its head (`HeadRead` / `HeadMissing`)?
    engine_has_head: bool,
    /// Writes made before it was, in order (see [`Page::write_reading`]).
    held_writes: Vec<Event>,
    out: Vec<Op>,
    /// Every effect for a CLIENT (a write's state, a read's answer, a
    /// subscription's news), in the order the engine emitted them.
    client_fx: Vec<Effect>,
    /// Effects this executor does not act on (the read path's replies,
    /// subscriptions), for the caller.
    unusable: Vec<String>,
    now: u64,
    /// Every record the signer returned — invariant 2's evidence.
    signer_records: BTreeSet<Vec<u8>>,
}

impl Page {
    /// A page with a fresh engine. It reads its head first (`ReadHead`), as a
    /// delegate's engine did.
    pub fn new(params: Params, path: PutPath) -> Page {
        let mut p = Page::unstarted(params, path);
        // The key is in the SIGNER's secret store; the engine only states
        // where its authority comes from.
        p.step(Event::Start { key: KeySource::SecretStore, epochs: vec![EPOCH] });
        p
    }

    /// A page whose engine has not been STARTED: [`crate::server::Server`]
    /// starts it on the client's `Identity`, as the delegate's shell did.
    pub fn unstarted(params: Params, path: PutPath) -> Page {
        let blocks = PageBlocks::default();
        let engine = Engine::new(params, blocks.clone());
        Page {
            path,
            engine,
            blocks,
            confirmed: BTreeSet::new(),
            held: Vec::new(),
            owed: None,
            sign_again: None,
            sign_refusals: 0,
            held_again: BTreeMap::new(),
            verify: None,
            landings: 0,
            most_landing_updates: 0,
            forked: None,
            put_again: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            rto: rto::Rto::default(),
            window: rto::Window::default(),
            get_queue: Default::default(),
            attempt_of: BTreeMap::new(),
            recover_since: None,
            recover_told: false,
            recovered: false,
            engine_has_head: false,
            held_writes: Vec::new(),
            out: Vec::new(),
            client_fx: Vec::new(),
            unusable: Vec::new(),
            now: 0,
            signer_records: BTreeSet::new(),
            sign_id: None,
            next_request: 1,
        }
    }

    /// Any engine event from a client (a read, a subscription, a tick, a
    /// start), carried out like every other.
    pub fn event(&mut self, ev: Event) {
        self.client_event(ev);
    }

    /// A BLIND write, as the app made it: nothing it read is checked.
    pub fn write(&mut self, client: ClientId, write_id: WriteId, ops: Vec<(Vec<u8>, WriteOp)>) {
        self.write_reading(client, write_id, ops, Vec::new());
    }

    /// A write that states what it READ (sdk#148): the engine checks each
    /// expectation against the tree the ops land on, and a read that no longer
    /// holds applies nothing and ends `Conflict`.
    pub fn write_reading(
        &mut self,
        client: ClientId,
        write_id: WriteId,
        ops: Vec<(Vec<u8>, WriteOp)>,
        reads: Vec<(Vec<u8>, engine::Expect)>,
    ) {
        self.client_event(Event::Write { client, write_id, ops, reads });
    }

    /// A WRITE before the engine has read its head is HELD, and replayed in
    /// order once the head is recovered. Committed at once, it would land on
    /// the EMPTY tree the engine starts on, and the page would sign that from
    /// the genesis over a register nobody read (1b) — main's M2 on #227 found
    /// the check that should have seen it could not. Reads are held the same
    /// way by [`server::Server`] (and by the engine itself once sdk#223 lands).
    fn client_event(&mut self, ev: Event) {
        if !self.engine_has_head && matches!(ev, Event::Write { .. }) {
            self.held_writes.push(ev);
            return;
        }
        self.step(ev);
    }

    /// The page's clock: the engine's tick, and every op past its deadline
    /// re-sent as it was.
    pub fn tick(&mut self, now: Ms) {
        let clock = now;
        let now = now.0;
        self.now = now;
        let late: Vec<Waiting> =
            self.deadlines.iter().filter(|(_, d)| now >= d.at).map(|(w, _)| w.clone()).collect();
        // RFC 6298 §5.5 — ONCE per tick, however many timed out — and a GET
        // that timed out halves the window.
        if !late.is_empty() {
            self.rto.timed_out();
        }
        if late.iter().any(|w| matches!(w, Waiting::Get(_))) {
            self.window.halved();
        }
        for w in late {
            let d = self.deadlines.remove(&w).expect("listed");
            let op = d.op.clone();
            self.attempt_of.insert(w.clone(), d.attempt);
            match w {
                // A GET that did not answer is the engine's to re-issue: it
                // counts attempts and gives up within its own budget.
                Waiting::Get(id) => self.step(Event::BlockMissed(id)),
                // Rebuilt, not replayed: the published head it names as prev
                // may have moved since it was first sent.
                // A landing's request, or this commit's.
                Waiting::Sign if self.verify.as_ref().is_some_and(|v| v.landing) => self.ask_land(),
                Waiting::Sign => self.ask_sign(),
                _ => {
                    if w == Waiting::Update {
                        if let Some(v) = self.verify.as_mut().filter(|v| v.landing) {
                            v.updates += 1;
                            self.most_landing_updates = self.most_landing_updates.max(v.updates);
                        }
                    }
                    self.send(w, op)
                }
            }
        }
        for (id, bytes) in std::mem::take(&mut self.put_again) {
            self.send(Waiting::Put(id), Op::Put { id, bytes });
        }
        if self.sign_again.is_some_and(|at| now >= at) {
            self.sign_again = None;
            self.ask_sign();
        }
        if self.verify.as_ref().is_some_and(|v| v.again_at.is_some_and(|at| now >= at)) {
            if let Some(v) = self.verify.as_mut() {
                v.again_at = None;
            }
            self.send(Waiting::Verify, Op::ReadHead);
        }
        // The engine's OWN head read (recovery) has the same budget: it is
        // asked again on its RTO for as long as it takes, and past the budget
        // this page says the register is not answering. It is NEVER turned
        // into `HeadMissing` — that would open an empty tree over a register
        // that is merely unreachable (sdk#175).
        if self.deadlines.contains_key(&Waiting::RecoverHead) {
            let since = *self.recover_since.get_or_insert(now);
            if !self.recover_told && now.saturating_sub(since) >= VERIFY_BUDGET_MS {
                self.recover_told = true;
                self.unusable.push(format!("the register is not answering: no head read within {VERIFY_BUDGET_MS} ms"));
            }
        } else {
            self.recover_since = None;
        }
        // The register's own budget: past it, "not answering" — named, and
        // nothing adopted.
        if self.verify.as_ref().is_some_and(|v| now.saturating_sub(v.first_at) >= VERIFY_BUDGET_MS) {
            let v = self.verify.take().expect("checked");
            self.deadlines.remove(&Waiting::Verify);
            self.unusable.push(format!(
                "the register is not answering: the signer named seq {} and the register never showed it within {} ms",
                v.seq, VERIFY_BUDGET_MS
            ));
        }
        let due: Vec<Cid> = self.held_again.iter().filter(|(_, (at, _))| now >= *at).map(|(id, _)| *id).collect();
        for id in due {
            self.send(Waiting::Held(id), Op::AskHeld { id });
        }
        if let Some(o) = &self.owed {
            if o.record.is_some() && o.stale_reads > 0 && !self.deadlines.contains_key(&Waiting::ReadBack) {
                self.send(Waiting::ReadBack, Op::ReadHead);
            }
        }
        // ENGINE TIME IS SECONDS: its `Tick` is the protocol's whole seconds
        // (`parity_age`, `reask_after` and the stall timer count them). The
        // page's clock is milliseconds; passed through as it was (#215), every
        // engine timer ran a thousand times fast — a put re-asked after 16 ms
        // instead of 16 s.
        self.step(Event::Tick(engine_seconds(clock)));
    }

    /// Something arrived.
    pub fn answer(&mut self, a: Answer, now: Ms) {
        let now = now.0;
        self.now = now;
        match a {
            Answer::PutOk(id) => {
                if self.answered(&Waiting::Put(id)).is_none() && self.confirmed.contains(&id) {
                    return; // a second answer to a re-sent PUT
                }
                self.put_again.remove(&id);
                match self.path {
                    PutPath::Page => self.confirm(id),
                    // An answer is not a confirmation on this path: ask.
                    PutPath::Wrapper => self.send(Waiting::Held(id), Op::AskHeld { id }),
                }
            }
            Answer::Held { id, present } => {
                let Some(_) = self.answered(&Waiting::Held(id)) else { return };
                if present {
                    self.held_again.remove(&id);
                    self.confirm(id);
                    return;
                }
                // Not there YET is the ordinary answer to an early ask: ask
                // again on a doubling backoff, and only after HELD_ABSENTS in
                // a row put it again — never at the speed of the answers.
                let absents = self.held_again.get(&id).map_or(0, |(_, n)| *n) + 1;
                if absents >= HELD_ABSENTS {
                    self.held_again.remove(&id);
                    if let Some(bytes) = self.blocks.get(&id).map(<[u8]>::to_vec) {
                        self.put_again.insert(id, bytes);
                    }
                } else {
                    let wait = (BACKOFF_MS << absents).min(rto::RTO_MAX_MS as u64);
                    self.held_again.insert(id, (now + wait, absents));
                }
            }
            Answer::PutRefused { id, transient } => {
                let Some(op) = self.answered(&Waiting::Put(id)) else { return };
                if transient {
                    if let Op::Put { bytes, .. } = op {
                        self.put_again.insert(id, bytes);
                    }
                } else {
                    self.step(Event::PutFailed(id));
                }
            }
            Answer::Got { id, bytes } => {
                if self.answered(&Waiting::Get(id)).is_none() {
                    return;
                }
                // Verified BEFORE it joins the page's memory: a block that is
                // not its id is not kept, and the engine hears a miss.
                if engine::read::matches_id(&id, &bytes) {
                    self.blocks.insert(id, &bytes);
                }
                self.step(Event::BlockArrived { id, bytes });
            }
            Answer::GetMissed(id) => {
                if self.answered(&Waiting::Get(id)).is_some() {
                    self.step(Event::BlockMissed(id));
                }
            }
            Answer::Signer { id, answer: s } => {
                // THE ID decides (SG02): only the answer under the id of the
                // sign in flight is the sign's. Anything else -- another
                // verb's answer, a `Put` (UNATTRIBUTED), or a late answer to a
                // superseded ask -- is not, and must not clear the sign's
                // deadline, or the commit sits un-asked until the engine says
                // Stalled (engineer2's review of sdk#215). The SHAPE check
                // stays as a second guard: under the sign's id, only a sign's
                // kind of answer is taken.
                if self.sign_id != Some(id) || !answers_a_sign(&s) {
                    return;
                }
                if self.answered(&Waiting::Sign).is_none() {
                    return; // an answer to a sign request already answered
                }
                self.sign_id = None;
                self.on_signer(s);
            }
            Answer::Updated => {
                if self.answered(&Waiting::Update).is_some() {
                    // A LANDING's UPDATE is judged by its own read — does the
                    // register now hold the head the signer named — never by
                    // this commit's read-back.
                    if self.verify.as_ref().is_some_and(|v| v.landing) {
                        self.send(Waiting::Verify, Op::ReadHead);
                    } else {
                        self.send(Waiting::ReadBack, Op::ReadHead);
                    }
                }
            }
            // One register read can answer both a recovery read and a
            // read-back: a head is a head, whoever asked.
            Answer::Head(h) => {
                self.answered(&Waiting::Warm);
                if self.answered(&Waiting::RecoverHead).is_some() {
                    match h {
                        Some((seq, root)) => self.step(Event::HeadRead { epoch: EPOCH, seq, root }),
                        None => self.step(Event::HeadMissing),
                    }
                    self.recovered = true;
                }
                if self.answered(&Waiting::ReadBack).is_some() {
                    self.on_read_back(h);
                }
                if self.answered(&Waiting::Verify).is_some() {
                    self.on_verify(h);
                }
            }
        }
    }

        /// A block is on the node: the one place `PutConfirmed` comes from.
    fn confirm(&mut self, id: Cid) {
        if self.confirmed.insert(id) {
            self.step(Event::PutConfirmed(id));
            self.release();
        }
    }

    fn on_signer(&mut self, s: signer_proto::Answer) {
        use signer_proto::{Answer as A, Why};
        // A LANDING's answer (see `on_verify`): the record's bytes, UPDATEd as
        // they are, then read — the same deadline and backoff as a publish of
        // this page's own.
        if self.verify.as_ref().is_some_and(|v| v.landing) {
            let backoff = |v: &mut Verify, now: u64| {
                v.tries += 1;
                v.again_at = Some(now + (BACKOFF_MS << v.tries.min(5)).min(rto::RTO_MAX_MS as u64));
            };
            match s {
                A::Signed(state) | A::AlreadySigned(state) => {
                    self.signer_records.insert(state.clone());
                    self.send(Waiting::Update, Op::Update { state });
                    let v = self.verify.as_mut().expect("checked");
                    v.updates += 1;
                    self.most_landing_updates = self.most_landing_updates.max(v.updates);
                }
                A::NotNext { current } => {
                    let now = self.now;
                    let v = self.verify.as_mut().expect("checked");
                    v.seq = v.seq.max(current.seq);
                    v.root = current.root;
                    v.landing = false;
                    backoff(v, now);
                }
                A::Refused(Why::RootNotHeld | Why::HeadUnknown | Why::RecordNotSaved | Why::NotSuccessor) => {
                    let now = self.now;
                    let v = self.verify.as_mut().expect("checked");
                    v.landing = false;
                    backoff(v, now);
                }
                // Every other refusal, named. (No FORKED arm: a landing asks
                // from the register's own head, one seq behind the record, so
                // the signer's equal-seq fork check cannot fire on it — the
                // mutant that dropped such an arm survived, as dead weight.)
                A::Refused(why) => {
                    self.unusable.push(format!("the signer refused a landing: {why:?}"));
                    self.verify = None;
                }
                other => self.unusable.push(format!("the signer answered a landing with {other:?}")),
            }
            return;
        }
        let Some(owed) = self.owed.as_mut() else { return };
        self.sign_refusals = match &s {
            A::Refused(Why::RootNotHeld | Why::HeadUnknown | Why::RecordNotSaved) => self.sign_refusals,
            _ => 0,
        };
        match s {
            A::Signed(state) => {
                self.signer_records.insert(state.clone());
                owed.record = Some(state.clone());
                owed.stale_reads = 0;
                self.send(Waiting::Update, Op::Update { state });
            }
            A::AlreadySigned(state) => {
                // Requirement 2: ONE signature per prev, and it is landed as
                // it is — its blocks were stored before it was signed.
                self.signer_records.insert(state.clone());
                owed.record = Some(state.clone());
                owed.stale_reads = 0;
                // Its root may not be this commit's (a record another page
                // of this key made at this seq). Nothing more is needed here:
                // the read-back judges by THIS commit's (seq, root), so that
                // record reads back as the same seq under another root — a
                // conflict, never Published (the model's mutant M5: a second
                // mechanism for this was dead weight).
                self.send(Waiting::Update, Op::Update { state });
            }
            // NOT ADOPTED YET (invariant 1b). The signer's truth is the later
            // of its RECORD and the register, and the record can be AHEAD:
            // signed, its UPDATE not landed (ENGINE-SHAPE §5 3b). Adopted as
            // it was, a later write was told Published at a head no reader
            // can see (the model, seed 10). So the register is read first.
            A::NotNext { current } => {
                let now = self.now;
                self.verify = Some(Verify { seq: current.seq, root: current.root, landing: false, again_at: None, tries: 0, updates: 0, from: None, first_at: now });
                self.send(Waiting::Verify, Op::ReadHead);
            }
            // RETRYABLE, on a doubling backoff (engineer2's table):
            // RootNotHeld — the root's PUT is still landing; HeadUnknown — the
            // node does not hold the register, so a page read of it is sent
            // first to make it held; RecordNotSaved — the signer could not
            // write its record and signed nothing. A node's "queue full" is
            // not a signer answer: it is re-asked by the deadline.
            A::Refused(why @ (Why::RootNotHeld | Why::HeadUnknown | Why::RecordNotSaved)) => {
                if why == Why::HeadUnknown {
                    self.send(Waiting::Warm, Op::ReadHead);
                }
                self.sign_refusals += 1;
                let wait = (BACKOFF_MS << self.sign_refusals.min(5)).min(rto::RTO_MAX_MS as u64);
                self.sign_again = Some(self.now + wait);
            }
            // LOUD: a fork is news, never a silent retry.
            A::Refused(why @ Why::Forked { .. }) => {
                let msg = format!("FORKED: the signer saw two roots at one seq for this key: {why:?}");
                self.forked = Some(msg.clone());
                self.unusable.push(msg);
            }
            // Permanent: not provisioned, not a successor, cannot sign,
            // unreadable.
            A::Refused(why) => self.unusable.push(format!("the signer refused: {why:?}")),
            other => self.unusable.push(format!("the signer answered a sign with {other:?}")),
        }
    }

    /// The register read that decides a `NotNext` (invariant 1b).
    ///
    /// * The register holds the named head, or a later one → that is the head:
    ///   adopted (`HeadConflict`).
    /// * It is ONE behind → the signer's record is ahead, its UPDATE unlanded:
    ///   this page LANDS it. It asks the signer from the register's own head
    ///   with `next.seq = register.seq + 1` (the signer checks the successor
    ///   FIRST — `next: this commit` would be `NotSuccessor`; any root). The
    ///   signer signed from that prev already, so it answers `AlreadySigned`
    ///   with the record for the named head WHATEVER the root asked (fact a:
    ///   `signer::decide`'s `r.prev == *prev` arm; pinned by signer/tests/
    ///   sign.rs `two_requests_from_one_prev_in_sequence_get_one_signature`),
    ///   and those exact bytes are UPDATEd. Two pages landing the same record
    ///   is idempotent: the register holds an equal decision, held bytes win.
    ///   SAFE because a record exists only after its blocks were put and
    ///   confirmed (invariant 0: [`Page::release`] lets an `UpdateHead` leave
    ///   only when its whole `after` set is confirmed). RESIDUAL, named: a
    ///   page whose signer answered before ITS blocks were stored leaves a
    ///   head with holes, and they surface here, on the landing page.
    /// * It is 2+ behind → UNRECOVERABLE (the signer keeps one record) and, by
    ///   1b on the sign side (a page signs only from a head the register was
    ///   read to hold), unreachable: a named failure, never a loop.
    ///
    /// On a peered node the register is read through the head SUBSCRIPTION
    /// (F55: a delegate-created register is not served to a bare client GET);
    /// the web layer frames `Op::ReadHead` as that.
    fn on_verify(&mut self, h: Option<(u64, Cid)>) {
        let Some(v) = self.verify.clone() else { return };
        let reg_seq = h.map_or(0, |(s, _)| s);
        if let Some((seq, root)) = h.filter(|(s, _)| *s >= v.seq) {
            self.verify = None;
            self.owed = None;
            self.step(Event::HeadConflict { seq, root });
            return;
        }
        if v.seq > reg_seq + 1 {
            self.verify = None;
            self.unusable.push(format!(
                "the signer's record (seq {}) is {} ahead of the register (seq {reg_seq}): unrecoverable, and 1b says unreachable",
                v.seq,
                v.seq - reg_seq
            ));
            return;
        }
        let (prev_seq, prev_root) = match h {
            Some(h) => h,
            // No head at all: the record's prev is the genesis (the empty
            // tree), which is where this engine stands if it has adopted none.
            None if self.engine.published_seq() == 0 => (0, self.engine.published_root()),
            None => {
                let now = self.now;
                let v = self.verify.as_mut().expect("present");
                v.tries += 1;
                v.again_at = Some(now + (BACKOFF_MS << v.tries.min(5)).min(rto::RTO_MAX_MS as u64));
                return;
            }
        };
        if let Some(v) = self.verify.as_mut() {
            if !v.landing {
                self.landings += 1;
            }
            v.landing = true;
            v.from = Some((prev_seq, prev_root));
        }
        self.ask_land();
    }

    /// The register read back after an UPDATE: the only way a commit is
    /// Published (F56: the UPDATE's answer says nothing).
    fn on_read_back(&mut self, h: Option<(u64, Cid)>) {
        let Some(owed) = self.owed.as_mut() else { return };
        if owed.record.is_none() {
            return; // a read-back outlived its commit
        }
        let want = (owed.seq, owed.root);
        match h {
            Some(read) if read == want => {
                let owed = self.owed.take().expect("owed");
                self.step(Event::HeadConfirmed(owed.seq));
            }
            // Not visible yet: read again; after HEAD_READS, UPDATE again.
            Some((seq, _)) if seq < want.0 => {
                owed.stale_reads += 1;
                if owed.stale_reads >= HEAD_READS {
                    owed.stale_reads = 0;
                    let state = owed.record.clone().expect("an UPDATE was sent");
                    self.send(Waiting::Update, Op::Update { state });
                } else {
                    // Asked at the next tick (tick() re-reads while stale).
                }
            }
            Some((seq, root)) => {
                self.owed = None;
                self.step(Event::HeadConflict { seq, root });
            }
            None => {
                let state = owed.record.clone().expect("an UPDATE was sent");
                self.send(Waiting::Update, Op::Update { state });
            }
        }
    }

    /// A LANDING's sign request, under a fresh id (SG02): from the register's
    /// own head, with `next.seq = register.seq + 1` — the signer checks the
    /// successor FIRST — and any root.
    fn ask_land(&mut self) {
        let Some((prev_seq, prev_root, root)) = self.verify.as_ref().and_then(|v| v.from.map(|(s, r)| (s, r, v.root))) else {
            return;
        };
        let id = self.next_request;
        self.next_request = self.next_request.checked_add(1).unwrap_or(1);
        self.sign_id = Some(id);
        self.send(Waiting::Sign, Op::Sign { id, prev_seq, prev_root, seq: prev_seq + 1, root });
    }

    fn ask_sign(&mut self) {
        let Some(o) = &self.owed else { return };
        let id = self.next_request;
        self.next_request = self.next_request.checked_add(1).unwrap_or(1);
        self.sign_id = Some(id);
        let op = Op::Sign {
            id,
            prev_seq: self.engine.published_seq(),
            prev_root: self.engine.published_root(),
            seq: o.seq,
            root: o.root,
        };
        self.send(Waiting::Sign, op);
    }

    /// An op goes out, due again one RTO from now. A GET waits for a place
    /// in the window.
    fn send(&mut self, w: Waiting, op: Op) {
        if let Waiting::Get(id) = w {
            let in_flight = self.deadlines.keys().filter(|k| matches!(k, Waiting::Get(_))).count();
            if in_flight >= self.window.size() && !self.deadlines.contains_key(&w) {
                if !self.get_queue.contains(&id) {
                    self.get_queue.push_back(id);
                }
                return;
            }
        }
        let attempt = self.attempt_of.remove(&w).map_or(1, |a| a + 1);
        let d = Deadline { at: self.now + self.rto.rto_ms(), op: op.clone(), sent_at: self.now, attempt };
        self.deadlines.insert(w, d);
        self.out.push(op);
    }

    /// An ANSWER for `w`: its deadline ends, an attempt-1 answer is a sample
    /// (Karn: a re-sent call never is), and a GET opens the window. `None`:
    /// nothing was waiting on it.
    fn answered(&mut self, w: &Waiting) -> Option<Op> {
        let d = self.deadlines.remove(w)?;
        self.attempt_of.remove(w);
        if d.attempt == 1 {
            self.rto.sample(self.now.saturating_sub(d.sent_at));
        }
        if matches!(w, Waiting::Get(_)) {
            self.window.opened();
            self.fill_gets();
        }
        Some(d.op)
    }

    /// GETs waiting on the window take the places that are free.
    fn fill_gets(&mut self) {
        while let Some(id) = self.get_queue.front().copied() {
            let in_flight = self.deadlines.keys().filter(|k| matches!(k, Waiting::Get(_))).count();
            if in_flight >= self.window.size() {
                break;
            }
            self.get_queue.pop_front();
            self.send(Waiting::Get(id), Op::Get { id });
        }
    }

    /// Has the engine recovered its head (its own head read answered)? A read
    /// before this would be answered from the empty tree it started on.
    pub fn recovered(&self) -> bool {
        self.recovered
    }

    /// When the next deadline falls (a host sets a one-shot timer for it, as
    /// the cold reads do), or `None`.
    pub fn next_due(&self) -> Option<Ms> {
        // EVERY timer the page keeps: an op's deadline, a refused sign's
        // back-off, a `Held` re-ask, a pending verify, and a PUT to repeat at
        // the next tick. A host that armed only for deadlines would sleep
        // through a back-off (the differential's RecordNotSaved case did).
        let deadlines = self.deadlines.values().map(|d| d.at);
        let sign = self.sign_again;
        let held = self.held_again.values().map(|(at, _)| *at);
        let verify = self.verify.as_ref().and_then(|v| v.again_at);
        let puts = (!self.put_again.is_empty()).then_some(self.now);
        deadlines.chain(sign).chain(held).chain(verify).chain(puts).min().map(Ms)
    }

    /// The retry clock now: `(RTO ms, SRTT ms, GET window)`.
    pub fn clock(&self) -> (u64, Option<f64>, usize) {
        (self.rto.rto_ms(), self.rto.srtt_ms(), self.window.size())
    }

    /// Step the engine and carry out what it decided.
    fn step(&mut self, ev: Event) {
        // The engine has a head once it is TOLD one, whoever tells it: the
        // held writes go the moment it is, onto the tree it now stands on.
        let recovery = matches!(ev, Event::HeadRead { .. } | Event::HeadMissing);
        let fx = self.engine.step(ev);
        self.carry_out(fx);
        self.drop_dead_head();
        if recovery && !self.engine_has_head {
            self.engine_has_head = true;
            for w in std::mem::take(&mut self.held_writes) {
                self.step(w);
            }
        }
    }

    /// An owed head is LIVE only while it is ahead of what the engine has
    /// published. Once the engine adopts another head (a conflict, a recovery
    /// read), the commit that owed it is dead — its writes were told `Lost` —
    /// and nothing more is asked or sent for it. Without this the page went on
    /// asking the signer for a dead commit from a stale prev (the model found
    /// it: `NotSuccessor`, hidden behind the retries that got round it).
    fn drop_dead_head(&mut self) {
        if self.owed.as_ref().is_some_and(|o| o.seq <= self.engine.published_seq()) {
            self.owed = None;
            self.sign_again = None;
            self.sign_refusals = 0;
            self.sign_id = None;
            for w in [Waiting::Sign, Waiting::Update, Waiting::ReadBack] {
                self.deadlines.remove(&w);
            }
        }
    }

    fn carry_out(&mut self, fx: Vec<Effect>) {
        for f in fx {
            match f {
                Effect::PutBlock { id, ref bytes, ref after } | Effect::PutParity { id, ref bytes, ref after, .. } => {
                    // The page is the memory now: the engine keeps no bytes.
                    self.blocks.insert(id, bytes);
                    let after: BTreeSet<Cid> = after.iter().copied().collect();
                    self.held.push((after, f.clone()));
                }
                Effect::UpdateHead { ref after, .. } => {
                    let after: BTreeSet<Cid> = after.iter().copied().collect();
                    self.held.push((after, f.clone()));
                }
                Effect::PutPack { id, .. } => {
                    // No packs in this phase, as the shell refuses them.
                    self.unusable.push("a pack was emitted; packs are off in this phase".into());
                    let more = self.engine.step(Event::PutFailed(id));
                    self.carry_out(more);
                }
                Effect::FetchBlock { id, .. } => {
                    if self.blocks.get(&id).is_some() {
                        let bytes = self.blocks.get(&id).expect("held").to_vec();
                        let more = self.engine.step(Event::BlockArrived { id, bytes });
                        self.carry_out(more);
                    } else {
                        self.send(Waiting::Get(id), Op::Get { id });
                    }
                }
                Effect::ReadHead { .. } => self.send(Waiting::RecoverHead, Op::ReadHead),
                client => self.client_fx.push(client),
            }
        }
        self.release();
    }

    /// Effects whose `after` set is now confirmed go out, in emitted order.
    fn release(&mut self) {
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].0.iter().all(|c| self.confirmed.contains(c)) {
                let (_, f) = self.held.remove(i);
                match f {
                    Effect::PutBlock { id, bytes, .. } | Effect::PutParity { id, bytes, .. } => {
                        if self.confirmed.contains(&id) {
                            // Already on the node: the engine hears it again.
                            let more = self.engine.step(Event::PutConfirmed(id));
                            self.carry_out(more);
                        } else {
                            self.send(Waiting::Put(id), Op::Put { id, bytes });
                        }
                    }
                    // A head held across an adopt belongs to a DEAD commit
                    // (its writes were told `Lost`): it is at or behind what
                    // the engine now publishes, and asking for it would name a
                    // prev it does not follow (the model: `NotSuccessor`).
                    Effect::UpdateHead { seq, .. } if seq <= self.engine.published_seq() => {}
                    Effect::UpdateHead { seq, root, .. } => {
                        self.owed = Some(Owed { seq, root, record: None, stale_reads: 0 });
                        self.ask_sign();
                    }
                    _ => unreachable!("only puts and heads are held"),
                }
                i = 0;
            } else {
                i += 1;
            }
        }
    }

    /// Is anything still owed an answer or a re-send — an op in flight, a
    /// backed-off retry? `false` means this page is at rest until something
    /// new arrives.
    pub fn waiting(&self) -> bool {
        !self.deadlines.is_empty() || !self.put_again.is_empty() || self.sign_again.is_some() || !self.held_again.is_empty()
    }

    /// Ops to send, in order.
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.out)
    }

    /// Write states the engine reported, for the app.
    pub fn take_notices(&mut self) -> Vec<(ClientId, WriteId, State)> {
        let mut out = Vec::new();
        self.client_fx.retain(|f| match f {
            Effect::Notify { client, write_id, state } => {
                out.push((*client, *write_id, *state));
                false
            }
            _ => true,
        });
        out
    }

    /// Every client-facing effect, in the order the engine emitted it.
    pub fn take_client(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.client_fx)
    }

    /// Effects this executor does not act on, for the caller.
    pub fn take_other(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        self.client_fx.retain(|f| {
            if matches!(f, Effect::Notify { .. }) {
                true
            } else {
                out.push(f.clone());
                false
            }
        });
        out
    }

    /// Things this page could not do, by reason.
    pub fn unusable(&self) -> &[String] {
        &self.unusable
    }

    /// The head the engine last saw published.
    pub fn published(&self) -> (u64, Cid) {
        (self.engine.published_seq(), self.engine.published_root())
    }

    /// Whether this page's owed parity covers the whole published tree
    /// (craftworks-sdk#119). The surface every redundancy report reads: a
    /// page that opened on a non-empty head says `NotScanned` — its "0 owed"
    /// means it never looked — until sdk#119's probe re-derives it.
    pub fn parity_scan(&self) -> &engine::ParityScan {
        self.engine.parity_scan()
    }

    /// Landings of a signer's record this page started, and the most UPDATEs
    /// one of them needed (a lost UPDATE is re-sent at its deadline).
    pub fn landings(&self) -> (u32, u32) {
        (self.landings, self.most_landing_updates)
    }

    /// The signer saw this key's head FORKED: shown to the person, never
    /// retried.
    pub fn forked(&self) -> Option<&str> {
        self.forked.as_deref()
    }

    /// Every record the signer returned (invariant 2's evidence).
    pub fn signer_records(&self) -> &BTreeSet<Vec<u8>> {
        &self.signer_records
    }

    /// The blocks the page holds.
    pub fn blocks(&self) -> &PageBlocks {
        &self.blocks
    }
}

/// Is this the kind of answer a SIGN request gets? `Signed`, `AlreadySigned`,
/// `NotNext`, or a refusal the sign verb gives; not `Putting`, `Put`, `Held`,
/// `Provisioned`, or a refusal only PUT-WITH-CODE or Provision gives.
fn answers_a_sign(a: &signer_proto::Answer) -> bool {
    use signer_proto::{Answer as A, Why};
    match a {
        A::Signed(_) | A::AlreadySigned(_) | A::NotNext { .. } => true,
        A::Refused(w) => !matches!(
            w,
            Why::BlockCount { .. } | Why::NotABlock { .. } | Why::KeyAlreadyProvisioned | Why::RegisterChanged
        ),
        A::Provisioned | A::Putting { .. } | A::Put { .. } | A::Held { .. } => false,
    }
}
