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
//! | `NotNext { current }` | `HeadConflict` onto `current`: the engine ADOPTS the winning head and reports the dead commit's writes `Lost`; the app submits them again on the winner (with no read-set check yet — the next PR) |
//! | `Refused(RootNotHeld / HeadUnknown / RecordNotSaved)` | the same sign request after a doubling backoff from [`BACKOFF_MS`]; for `HeadUnknown` the register is READ first, which makes the signer's node hold it |
//! | `Refused(Forked)` | LOUD: [`Page::forked`] and `unusable`; never retried — a fork is news |
//! | a signer answer no sign gets (`Putting`, `Put`, `Held`, `Provisioned`, another verb's refusal) | ignored: it does NOT clear the sign's deadline (answers carry no request id until SG02) |
//! | any other `Refused(why)` | nothing more is asked; recorded in [`Page::unusable`]; the engine's own clock reports the write `Stalled` |
//! | `Updated` | a register read-back ([`Op::ReadHead`]): an UpdateResponse carries nothing (F56) |
//! | `Head(Some(mine))` while a head is owed | `HeadConfirmed(seq)` — the only way a commit is Published |
//! | `Head(older seq)` while owed | not visible yet: the head is read again next tick, and after [`HEAD_READS`] reads the UPDATE is re-sent |
//! | `Head(newer seq, or same seq another root)` while owed | `HeadConflict` |
//! | `Head(None)` while owed | the UPDATE is re-sent |
//! | `Head(..)` for the engine's own `ReadHead` | `HeadRead` / `HeadMissing` (recovery) — kept apart from a read-back, which only judges the owed head |
//! | `FetchBlock` | [`Op::Get`]; `Got` → verified, joins [`PageBlocks`], `BlockArrived`; `GetMissed` → `BlockMissed` |
//! | any op unanswered for [`SILENT_MS`] | re-sent as it was — every op here is idempotent (a block is its hash, a sign re-ask is answered AlreadySigned, a record is the same record) |
//!
//! ## Invariants (each asserted by the model test, each with a mutant)
//! 1. A write is `Published` only when the register READ BACK holds this
//!    page's (seq, root).
//! 2. The page UPDATEs the register only with bytes the signer returned.
//! 3. A published root is WHOLE on the node: every block it reaches was put
//!    and answered before the head was signed.
//! 4. Every write ends `Published` once the faults stop.
//!
//! ## Not here (next PRs)
//! * the read-set check before a rebase (M2: `Event::Write{reads}` in the
//!   engine). Today a `Lost` write submitted again by the app is applied on
//!   the winner's root whatever it read — right for blind writes only;
//! * a persisted outbox / RELOAD beyond what the signer's record gives (a
//!   fresh page on the same key is answered `AlreadySigned` and lands it);
//! * the web Session's `Db` over this, and provisioning the signer.

use engine::{ClientId, Effect, Engine, Epoch, Event, KeySource, Op as WriteOp, Params, State, WriteId};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// An op unanswered this long is re-sent as it was. From CLAUDE.md's "a put
/// confirmation ≤ ~2 s": a bound on the WAIT, not a measurement.
pub const SILENT_MS: u64 = 2_000;

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
/// signer refusal); it doubles each time, up to [`SILENT_MS`].
pub const BACKOFF_MS: u64 = 100;

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
    /// The loud one: the signer saw this key's head FORKED (two roots at one
    /// seq). Never retried; a fork is news.
    forked: Option<String>,
    /// A PUT to repeat at the next tick (a transient refusal).
    put_again: BTreeMap<Cid, Vec<u8>>,
    /// Deadlines of the ops in flight, and each op to re-send.
    deadlines: BTreeMap<Waiting, (u64, Op)>,
    out: Vec<Op>,
    notices: Vec<(ClientId, WriteId, State)>,
    /// Effects this executor does not act on (the read path's replies,
    /// subscriptions), for the caller.
    other: Vec<Effect>,
    unusable: Vec<String>,
    now: u64,
    /// Every record the signer returned — invariant 2's evidence.
    signer_records: BTreeSet<Vec<u8>>,
}

impl Page {
    /// A page with a fresh engine. It reads its head first (`ReadHead`), as a
    /// delegate's engine did.
    pub fn new(params: Params, path: PutPath) -> Page {
        let blocks = PageBlocks::default();
        let engine = Engine::new(params, blocks.clone());
        let mut p = Page {
            path,
            engine,
            blocks,
            confirmed: BTreeSet::new(),
            held: Vec::new(),
            owed: None,
            sign_again: None,
            sign_refusals: 0,
            held_again: BTreeMap::new(),
            forked: None,
            put_again: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            out: Vec::new(),
            notices: Vec::new(),
            other: Vec::new(),
            unusable: Vec::new(),
            now: 0,
            signer_records: BTreeSet::new(),
            sign_id: None,
            next_request: 1,
        };
        // The key is in the SIGNER's secret store; the engine only states
        // where its authority comes from.
        p.step(Event::Start { key: KeySource::SecretStore, epochs: vec![EPOCH] });
        p
    }

    /// A write, as the app made it.
    pub fn write(&mut self, client: ClientId, write_id: WriteId, ops: Vec<(Vec<u8>, WriteOp)>) {
        self.step(Event::Write { client, write_id, ops });
    }

    /// The page's clock: the engine's tick, and every op past its deadline
    /// re-sent as it was.
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        let late: Vec<Waiting> =
            self.deadlines.iter().filter(|(_, (at, _))| now >= *at).map(|(w, _)| w.clone()).collect();
        for w in late {
            let (_, op) = self.deadlines.remove(&w).expect("listed");
            match w {
                // A GET that did not answer is the engine's to re-issue: it
                // counts attempts and gives up within its own budget.
                Waiting::Get(id) => self.step(Event::BlockMissed(id)),
                // Rebuilt, not replayed: the published head it names as prev
                // may have moved since it was first sent.
                Waiting::Sign => self.ask_sign(),
                _ => self.send(w, op),
            }
        }
        for (id, bytes) in std::mem::take(&mut self.put_again) {
            self.send(Waiting::Put(id), Op::Put { id, bytes });
        }
        if self.sign_again.is_some_and(|at| now >= at) {
            self.sign_again = None;
            self.ask_sign();
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
        self.step(Event::Tick(now));
    }

    /// Something arrived.
    pub fn answer(&mut self, a: Answer, now: u64) {
        self.now = now;
        match a {
            Answer::PutOk(id) => {
                if self.deadlines.remove(&Waiting::Put(id)).is_none() && self.confirmed.contains(&id) {
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
                let Some((_, _)) = self.deadlines.remove(&Waiting::Held(id)) else { return };
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
                    let wait = (BACKOFF_MS << absents).min(SILENT_MS);
                    self.held_again.insert(id, (now + wait, absents));
                }
            }
            Answer::PutRefused { id, transient } => {
                let Some((_, op)) = self.deadlines.remove(&Waiting::Put(id)) else { return };
                if transient {
                    if let Op::Put { bytes, .. } = op {
                        self.put_again.insert(id, bytes);
                    }
                } else {
                    self.step(Event::PutFailed(id));
                }
            }
            Answer::Got { id, bytes } => {
                if self.deadlines.remove(&Waiting::Get(id)).is_none() {
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
                if self.deadlines.remove(&Waiting::Get(id)).is_some() {
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
                if self.deadlines.remove(&Waiting::Sign).is_none() {
                    return; // an answer to a sign request already answered
                }
                self.sign_id = None;
                self.on_signer(s);
            }
            Answer::Updated => {
                if self.deadlines.remove(&Waiting::Update).is_some() {
                    self.send(Waiting::ReadBack, Op::ReadHead);
                }
            }
            // One register read can answer both a recovery read and a
            // read-back: a head is a head, whoever asked.
            Answer::Head(h) => {
                self.deadlines.remove(&Waiting::Warm);
                if self.deadlines.remove(&Waiting::RecoverHead).is_some() {
                    match h {
                        Some((seq, root)) => self.step(Event::HeadRead { epoch: EPOCH, seq, root }),
                        None => self.step(Event::HeadMissing),
                    }
                }
                if self.deadlines.remove(&Waiting::ReadBack).is_some() {
                    self.on_read_back(h);
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
            A::NotNext { current } => {
                self.owed = None;
                self.step(Event::HeadConflict { seq: current.seq, root: current.root });
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
                let wait = (BACKOFF_MS << self.sign_refusals.min(5)).min(SILENT_MS);
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

    fn send(&mut self, w: Waiting, op: Op) {
        self.deadlines.insert(w, (self.now + SILENT_MS, op.clone()));
        self.out.push(op);
    }

    /// Step the engine and carry out what it decided.
    fn step(&mut self, ev: Event) {
        let fx = self.engine.step(ev);
        self.carry_out(fx);
        self.drop_dead_head();
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
                Effect::Notify { client, write_id, state } => self.notices.push((client, write_id, state)),
                other => self.other.push(other),
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

    /// Ops to send, in order.
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.out)
    }

    /// Write states the engine reported, for the app.
    pub fn take_notices(&mut self) -> Vec<(ClientId, WriteId, State)> {
        std::mem::take(&mut self.notices)
    }

    /// Effects this executor does not act on, for the caller.
    pub fn take_other(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.other)
    }

    /// Things this page could not do, by reason.
    pub fn unusable(&self) -> &[String] {
        &self.unusable
    }

    /// The head the engine last saw published.
    pub fn published(&self) -> (u64, Cid) {
        (self.engine.published_seq(), self.engine.published_root())
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
