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
//! | `PutBlock` (data AND parity, §P) | the bytes join [`PageBlocks`] (the page is now the memory a node was); held until its `after` set is confirmed, then [`Op::Put`] |
//! | `PutOk` | [`PutPath::Page`]: `PutConfirmed` on the PUT's answer (a page-PUT block is served, measured 20/20; no per-block read-back). [`PutPath::Wrapper`]: [`Op::AskHeld`], and `PutConfirmed` only on `Held { present: true }`; absent → asked again on a doubling backoff, the PUT again only after [`HELD_ABSENTS`] absents in a row |
//! | `PutRefused { transient }` | transient (F51's queue): the same PUT again at the next tick; permanent (the node's Block contract rejected the bytes, sdk#433): `PutRejected`, never put again -- a repair PUT's is dropped and counted |
//! | `PutPack` | refused as the shell refuses it (no packs in this phase): `PutFailed` |
//! | `ConfirmHeld { id }` (another member of a changed group, asked at the commit's start: SAVED and BACKED_UP need the node's word, sdk#416 / class 2) | already confirmed here: `PutConfirmed` at once, no op. Else `HeldUnknown` (the engine counts it absent and sends one more of its group's parity) and [`Op::AskHeld`]; `Held { present: true }` → `PutConfirmed`; absent → asked again on a doubling backoff for as long as it takes (rule 7), put from here only if the page holds its bytes |
//! | `UpdateHead { seq, root }` | held until its `after` is confirmed, then [`Op::Sign`] from the engine's PUBLISHED head |
//! | `Signed(state)` (`signer_proto::Answer` under the in-flight sign's id, as `wire::signer::read_answer` decodes it; any other id is ignored) | [`Op::Update`] with exactly those bytes |
//! | `AlreadySigned(state)` | [`Op::Update`] with exactly those bytes (the signer's requirement 2: at most one signature per prev). If its root is another page's, the read-back shows this seq under that root: `HeadConflict` |
//! | `NotNext { current }` | NOT adopted yet (1b): the register is READ first (`Page::on_verify`) |
//! | … the register holds `current`, or a later head | `HeadConflict` onto it: the engine adopts it and reports the dead commit's writes `Lost`; the app submits them again, each checked against its READS where it lands (#218) |
//! | … the register is ONE behind (the signer's record is ahead, its UPDATE unlanded) | LAND it: Sign from the register's head with `next.seq = register.seq + 1` (any root) under a fresh id → `AlreadySigned(record)` (`signer::decide`'s `r.prev == *prev` arm; signer/tests/sign.rs `two_requests_from_one_prev_in_sequence_get_one_signature`) → [`Op::Update`] those bytes → read; adopted once the register shows it. Safe by invariant 0; two pages landing the same bytes is an equal decision, held bytes win. RESIDUAL: a page whose signer answered before ITS blocks were stored leaves holes, surfacing here |
//! | … the register is 2+ behind | a NAMED failure in [`Page::unusable`] — unrecoverable (one record per signer) and unreachable by 1b |
//! | … the register does not answer | read again on its deadline, for as long as it takes (rules 7, 8); [`Page::not_answering`] says for how long — nothing adopted |
//! | `Refused(RootNotHeld / HeadUnknown / RecordNotSaved)` | the same sign request after a doubling backoff from [`BACKOFF_MS`]; for `HeadUnknown` the register is READ first, which makes the signer's node hold it |
//! | `Refused(Forked { read, .. })` (an OLD signer: the new one answers `NotNext`, sdk#225) | the same identity never forks: as `NotNext { current: read }` — read, then adopt. If the page already stands on `read`, the old rule refuses every ask until the register passes that seq: NOT re-asked, named once in [`Page::unusable`], and asked again when a head read shows the register past it |
//! | a signer answer under any id but the in-flight sign's (SG02), or not shaped like a sign's | ignored: it does NOT clear the sign's deadline |
//! | any other `Refused(why)` | nothing more is asked; recorded in [`Page::unusable`]; the engine's own clock reports the write `Stalled` |
//! | `Updated` | a register read ([`Op::ReadHead { label: Label::Head }`]) — this commit's read-back, or a landing's verify read: an UpdateResponse carries nothing (F56). On a peered node the register is read through the head SUBSCRIPTION (F55); the web layer frames it so |
//! | `Head(Some(mine))` while a head is owed | `HeadConfirmed(seq)` — the only way a commit is Published (a pushed FULL state showing exactly mine is judged by this same row: below) |
//! | `Head(older seq)` while owed | not visible yet: the head is read again next tick, and after [`HEAD_READS`] reads the UPDATE is re-sent |
//! | `Head(newer seq, or same seq another root)` while owed | `HeadConflict` |
//! | `Head(None)` while owed | the UPDATE is re-sent |
//! | `Head(..)` for the engine's own `ReadHead` | `HeadRead` / `HeadMissing` (recovery) — kept apart from a read-back, which only judges the owed head |
//! | `HeadChanged` hint (no full state, or not this page's owed head) | a register read (`Waiting::Hint`) — unless a verify or this page's own READ-BACK is owed: that read judges whatever the register holds, so a second is only a node op on the node's one queue (sdk#378 P2, F61) |
//! | `HeadChanged` with a FULL state showing EXACTLY this page's owed (seq, root) | the read-back is done: `HeadConfirmed(seq)` through the read-back rule; the UPDATE's and read-back's waits end with no RTT sample, and the UPDATE's later answer asks nothing. Any other pushed state is the hint above; a dropped push leaves the read-back GET to its deadline (sdk#378 P3, the architect's three conditions) |
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

pub mod fates;
mod judge;
pub mod loader;
pub mod obs;
pub mod rto;
pub mod server;

use engine::{ClientId, Effect, Engine, Epoch, Event, KeySource, Op as WriteOp, Params, State, WriteId};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use judge::{judge, judge_site, Heard, Judged, SiteJudged};

/// How a judged head is told to the engine: the answer to its own recovery read (`HeadRead`), or another writer's
/// head found on a read (`HeadConflict`).
#[derive(Clone, Copy)]
enum Adopt {
    Recovery,
    Conflict,
}

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
    /// `ledger`: the bytes after the root in the value to sign (its PREV and
    /// every page's `through`, this page's updated: [`Page::sign_ledger_of`]).
    Sign { id: u32, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid, ledger: Vec<u8>, label: Label },
    /// UPDATE the label's record with exactly these bytes (a signed record): the head Register's UPDATE, or a
    /// site's PUT of its framing around them (builder#117; page-io holds the web part).
    Update { label: Label, state: Vec<u8> },
    /// GET the label's record: the head Register, or a site contract (GET with subscribe either way).
    ReadHead { label: Label },
    /// [`PutPath::Wrapper`] only: ask the signer's read-local verb whether the
    /// node holds this block now.
    AskHeld { id: Cid },
    /// PUT a contract the APP names (a published web container, builder#104),
    /// by its key. The bytes stay with the web layer, which frames the same
    /// PUT again at every deadline; the PAGE owns the deadline, the re-send
    /// and the end ([`AppPut`]), as for every other op.
    PutApp { key: String },
    /// A request page-io frames ([`Ext`]); the page owns its deadline.
    Ext(Ext),
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
    /// The label's UPDATE (a site's PUT) was answered. It says NOTHING about
    /// which record the node kept (F56).
    Updated { label: Label },
    /// The label's record as read: its seq and whole VALUE ([`HeadRead`]), or
    /// `None` if there is none (a NotFound: the GetFail split).
    Head { label: Label, read: Option<HeadRead> },
    /// [`PutPath::Wrapper`]: the signer's synchronous local read of a block.
    Held { id: Cid, present: bool },
    /// The node acknowledged the app's PUT of this contract key.
    AppPutOk(String),
    /// The node refused the app's PUT of this contract key, in its words.
    AppPutRefused { key: String, said: String },
    /// An op this page sent that its HOST will not send (a read-only page's
    /// `Update` or `Sign`: never produced, so this is loud), in the host's
    /// words. It ENDS every wait on that op -- nothing waits on a frame that
    /// never left. (An app's PUT has its own: [`Answer::AppPutRefused`].)
    NotSent { op: Op, why: String },
    /// The node refused a site's PUT, in its words (builder#117): the publication ENDS `Refused`.
    SiteRefused { app: String, said: String },
}

/// Where an app's PUT ([`Op::PutApp`]) stands. It is re-sent on the RTO
/// until the node ANSWERS (rule 7) — acknowledged, or refused in the node's
/// words — or a person CANCELS it: no time ends it (rule 8). While it waits,
/// [`Page::not_answering`] says for how long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppPut {
    Pending,
    Put,
    Refused(String),
    Cancelled,
}

/// A request page-io frames that is not an engine op — the signer's
/// registration, its first request, the record query — sent, re-sent and
/// answered through THIS page's sender like every op (rule 5): one deadline,
/// one RTO, no timer of page-io's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ext {
    /// Register the signer delegate.
    RegisterSigner,
    /// The signer's first request: which Register it signs for, or provision.
    SignerFirst,
    /// Ask the signer whether it holds a record for this register.
    AskRecord,
}

/// What a waiting request is, in words a page can show.
fn waiting_name(w: &Waiting) -> String {
    match w {
        Waiting::Put(_) => "a block's save".into(),
        Waiting::Held(_) => "a block check".into(),
        Waiting::Get(_) => "a block read".into(),
        Waiting::Sign(Label::Head) => "the signer".into(),
        Waiting::Update(Label::Head) => "the head's update".into(),
        Waiting::Warm | Waiting::RecoverHead | Waiting::Verify | Waiting::ReadBack(Label::Head) | Waiting::Hint => "the head read".into(),
        Waiting::Sign(Label::Site(app)) => format!("the signer (site {app})"),
        Waiting::Update(Label::Site(app)) => format!("site {app}'s publication"),
        Waiting::ReadBack(Label::Site(app)) => format!("site {app}'s read"),
        Waiting::PutApp(_) => "the app's publication".into(),
        Waiting::Ext(Ext::RegisterSigner) => "the signer's registration".into(),
        Waiting::Ext(Ext::SignerFirst) => "the signer".into(),
        Waiting::Ext(Ext::AskRecord) => "the signer's record".into(),
    }
}

/// An op in flight.
#[derive(Debug, Clone)]
struct Deadline {
    at: u64,
    /// The deadline it was given when SENT: a re-arm never moves it past this
    /// (sdk#378).
    armed: u64,
    op: Op,
    sent_at: u64,
    attempt: u32,
    /// `false`: PARKED -- a GET the node answered NotFound (or with bytes
    /// that are not its id), due to be sent again at `at` on the same
    /// per-op backoff as a silent one. Not on the wire: it holds no window
    /// place, and coming due is not a timeout.
    sent: bool,
    /// Re-sent on a NEW socket after a reconnect (sdk#376): its answer can only
    /// be to that re-send, but it is still not a first send, so (Karn) its
    /// answer is no RTT sample.
    resent: bool,
    /// The send this deadline is for, in the page's own SEND ORDER: the recording's label for it
    /// (`Label { Request, ordinal }`). A re-send is a new send. Never the op's block id (a foreign id).
    /// Also its place in the node's ONE queue for this client (F61, sdk#390): the order it went on the wire.
    seq: u32,
    /// Sent into an EMPTY queue: nothing was on the wire ahead of it, so its answer times the node's service plus the
    /// path -- the only answer that is an RTT sample (sdk#390). A queued op's answer also times its wait.
    alone: bool,
}

/// The wait of a FIRST send with `ahead` ops on the wire before it, at an RTO of `rto`: the node answers one client's
/// ops in one sequence (F61), so each op ahead is one more RTO, capped at the RTO's ceiling (sdk#390, the architect).
/// THE one arming formula: at the send, and at every re-arm.
fn queued_wait(rto: u64, ahead: u64) -> u64 {
    rto.saturating_mul(1 + ahead).min(rto::RTO_MAX_MS as u64)
}

/// How an op on the wire ENDED, as the page's recording says it (sdk#386's instrument work).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The node answered it.
    Answered,
    /// Its deadline passed unanswered: it is sent again (rule 7).
    TimedOut,
    /// Nobody needs it any more (superseded, cancelled, stood in for by a push, the socket replaced).
    Withdrawn,
}

/// The recording's SITE for an op: its kind, a compile-time name, and nothing of what it is about.
fn op_site(w: &Waiting) -> instrument::Site {
    use instrument::Site;
    match w {
        Waiting::Put(_) => Site::of("page::op::put"),
        Waiting::Held(_) => Site::of("page::op::held"),
        Waiting::Get(_) => Site::of("page::op::get"),
        Waiting::Sign(_) => Site::of("page::op::sign"),
        Waiting::Update(_) => Site::of("page::op::update"),
        Waiting::Warm => Site::of("page::op::warm"),
        Waiting::RecoverHead => Site::of("page::op::recover-head"),
        Waiting::Verify => Site::of("page::op::verify"),
        Waiting::ReadBack(_) => Site::of("page::op::read-back"),
        Waiting::Hint => Site::of("page::op::hint"),
        Waiting::PutApp(_) => Site::of("page::op::put-app"),
        Waiting::Ext(_) => Site::of("page::op::ext"),
    }
}

/// Which op a deadline is for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Waiting {
    Put(Cid),
    Held(Cid),
    Get(Cid),
    Sign(Label),
    Update(Label),
    /// A register read whose answer nobody judges: it only makes the node
    /// hold the register, so the signer's synchronous read can see a head
    /// (`Refused(HeadUnknown)`).
    Warm,
    /// The engine's own head read (recovery, `Effect::ReadHead`).
    RecoverHead,
    /// A register read that decides what a `NotNext` means (invariant 1b).
    Verify,
    /// The read-back after an UPDATE (a site's: also its first read, before it signs).
    ReadBack(Label),
    /// A register read on the node's `HeadChanged` hint, or the idle
    /// backstop ([`HEAD_BACKSTOP_MS`]): is there a head this page has not
    /// adopted?
    Hint,
    /// An app's PUT of this contract key ([`Op::PutApp`]).
    PutApp(String),
    /// A page-io request ([`Op::Ext`]).
    Ext(Ext),
}

/// WHICH RECORD a publication is (builder#117): the person's data HEAD, or one of their SITES by app id. ONE
/// sequence -- read, sign, write, read back -- keyed by it; what differs per row is a stated label fact (the module
/// table). The engine's events are the Head's only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Label {
    Head,
    Site(String),
}

/// One label's publication in flight: what it owes, its sign in flight, and its sign's backoff.
#[derive(Debug, Clone, Default)]
struct Pub {
    owed: Option<Owed>,
    sign_id: Option<u32>,
    sign_again: Option<u64>,
    sign_refusals: u32,
}

/// What a site publication waits on while the signer answers `HeadUnknown` (builder#117).
pub const WAITING_FOR_SITE: &str = "waiting for this node to fetch the site";

/// A SITE's publication (builder#117): how it ended, or that it has not -- the ONE owner of that fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publication {
    /// In flight. `waiting_for`: what it waits on when that is not the node's silence -- the signer said
    /// `HeadUnknown` (a record ahead of anything its node holds: this node has not fetched the site yet), re-asked
    /// on the backoff with no end (rule 8). `None` while it is simply out.
    Publishing { waiting_for: Option<&'static str> },
    /// The site record read back is this publication's: `version` is live.
    Published { version: u64 },
    /// Another publication is live at `version` (another device, or a later one): reported, never overwritten
    /// (the architect's Q2); publishing again is the person's act.
    Superseded { version: u64 },
    /// The signer or the node refused, in its words (`FromApp`, `BadLabel`, ...).
    Refused(String),
    /// The person cancelled it (the one end that is not an answer: rule 8).
    Cancelled,
}

/// The head this page owes the network: a commit's `(seq, root)` from the
/// engine's `UpdateHead`, and how far it got.
#[derive(Debug, Clone)]
struct Owed {
    seq: u64,
    root: Cid,
    /// The root the commit was built on: its head's prev is `(seq - 1, base)`.
    base: Cid,
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
}

/// With nothing else reading the register, a head read at least this often:
/// the subscription's renewal cadence. A `HeadChanged` from the node is a
/// hint the node may DROP (a full notification channel, an evicted
/// subscription), so an idle page must still learn of a head that moved
/// (the architect's attack on sdk#225).
pub const HEAD_BACKSTOP_MS: u64 = 120_000;

/// F56's equal-seq rule as the Register decides it: of two heads at ONE seq,
/// the one whose VALUE has the lower BLAKE3 wins (`(terminal, seq,
/// Reverse(BLAKE3(value)))`). A head's value today is its bare root; the
/// ledger-format PR (a versioned `root ‖ prev`) changes what is hashed, and
/// must change this with it.
/// A head register as read: its `seq` and its whole VALUE — `root ‖ ledger`,
/// the one format of `signer_proto::head` — so the equal-seq tie-break can hash
/// what the Register hashes, and a merge can read the head's `prev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadRead {
    pub seq: u64,
    /// At least a root: a shorter value is not a head, and is never built.
    value: Vec<u8>,
}

impl HeadRead {
    /// A head as a Register record states it (tolerantly: any ledger).
    pub fn from_record(state: &[u8]) -> Option<HeadRead> {
        let (seq, v) = signer_proto::head::record_of(state)?;
        HeadRead::from_value(seq, v)
    }

    /// A head from its seq and value; `None` for a value shorter than a root.
    pub fn from_value(seq: u64, value: &[u8]) -> Option<HeadRead> {
        signer_proto::head::read_value(value)?;
        Some(HeadRead { seq, value: value.to_vec() })
    }

    /// The root: the value's first 32 bytes, whatever follows.
    pub fn root(&self) -> Cid {
        self.value[..32].try_into().expect("a head's value holds a root")
    }

    /// The value as the Register holds it.
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// What this head WITNESSES of `device`'s commits (COMMIT-LIFE ⁵), for a
    /// commit that would have landed at seq `commit_seq`: its `through`
    /// entry; or, with none, `NotThere` whenever that can be KNOWN -- the list
    /// was never at its bound, or its entries show ours cannot have been
    /// evicted -- otherwise, or with a ledger that cannot be read, `Unknown`.
    /// Absent is never read as below.
    pub fn witness_of(&self, device: &[u8; 16], commit_seq: Option<u64>) -> engine::Witness {
        let Some(h) = signer_proto::head::read_value(&self.value) else { return engine::Witness::Unknown };
        if h.refused {
            return engine::Witness::Unknown;
        }
        let through = &h.ledger.through;
        match through.iter().find(|t| &t.device == device) {
            Some(t) => engine::Witness::Through(t.seq),
            // SAFE because the list never SHRINKS on a chain: every page's
            // head copies its prev's list forward (`sign_ledger_of`), LRU
            // evicts only on an insert past `THROUGH_MAX`, and a #225b merge
            // is signed on the winner, which built on the same base and so
            // carries that base's list. So an entry absent from a list still
            // below its bound was never in this chain: NotThere, not Unknown.
            None if through.len() < signer_proto::head::THROUGH_MAX => engine::Witness::NotThere,
            // A FULL list (review §3 on sdk#295: a device is minted per page
            // load, so a list fills and stays full). Eviction takes the lowest
            // (`last`, device); had our commit landed at s its entry would
            // carry `last` >= s, and every entry evicted before it sits at or
            // below it. So an entry left with `last` < s proves ours was never
            // evicted: absent means not there. STRICTLY below: at `last` == s
            // the tie is broken by device id, and ours could have gone first.
            None if commit_seq.is_some_and(|s| through.iter().any(|t| t.last < s)) => engine::Witness::NotThere,
            None => engine::Witness::Unknown,
        }
    }

    /// Every page's `through` its ledger carries.
    pub fn through(&self) -> Vec<signer_proto::head::Through> {
        signer_proto::head::read_value(&self.value).filter(|h| !h.refused).map(|h| h.ledger.through).unwrap_or_default()
    }

    /// Its §P mark and the ROOT's parity ids the mark lists (sdk#335): `None`
    /// unmarked; `Some(ids)`, ids empty when the mark lists none (or not as
    /// whole ids -- then the root is fetched singly, never rebuilt from a
    /// guess).
    pub fn mark(&self) -> Option<Vec<Cid>> {
        let h = signer_proto::head::read_value(&self.value).filter(|h| !h.refused)?;
        let body = h.ledger.parity?;
        Some(if body.len() % 32 == 0 { body.chunks_exact(32).map(|c| c.try_into().expect("32")).collect() } else { Vec::new() })
    }

    /// The head it was signed from, if its ledger says (a refused ledger says
    /// nothing: never "built on" anything).
    pub fn prev(&self) -> Option<(u64, Cid)> {
        let h = signer_proto::head::read_value(&self.value)?;
        h.ledger.prev.filter(|_| !h.refused).map(|p| (p.seq, p.root))
    }
}

/// A head with no ledger (the pre-format value, a bare root).
impl From<(u64, Cid)> for HeadRead {
    fn from((seq, root): (u64, Cid)) -> HeadRead {
        HeadRead { seq, value: root.to_vec() }
    }
}

/// The §P mark's bytes: every head this build signs is race put's
/// (COMMIT-LIFE §P), and the mark lists its ROOT's parity ids (sdk#335), the
/// root's group of one.
fn mark(root_parity: &[Cid]) -> Vec<u8> {
    root_parity.concat()
}

/// The root's parity ids always fit the mark: `PARITY` ids of 32 B within the
/// ledger's `PARITY_MAX`, which the value budget (`THROUGH_MAX`) already
/// reserves. At compile time, so a tree with more parity per group cannot
/// sign a mark the ledger would cut.
const _: () = assert!(engine::PARITY * 32 <= signer_proto::head::PARITY_MAX);

/// F56's equal-seq rule as the Register decides it: of two heads at ONE seq,
/// the one whose VALUE has the lower BLAKE3 wins (`(terminal, seq,
/// Reverse(BLAKE3(value)))`). The WHOLE value, ledger and all: two heads can
/// share a root and differ in their ledgers. Pinned against the contract's own
/// merge by `tests/tie_break.rs`.
pub fn beats(a: &[u8], b: &[u8]) -> bool {
    blake3::hash(a).as_bytes() < blake3::hash(b).as_bytes()
}

/// One page's writes: the engine, the executor state, and what to send.
pub struct Page {
    path: PutPath,
    /// This page's id in heads' `through` (COMMIT-LIFE ⁵): set once, from
    /// its first session; zeros until then.
    device: [u8; 16],
    /// Hold the engine's cut when a head displaces this page's published one
    /// at the SAME seq (review §1 on sdk#295): the Server may merge the
    /// displaced group, which must go at the front. Only a Server that
    /// releases the hold sets this.
    hold_on_displace: bool,
    /// The next signer request id this page issues: from 1, as `0` is
    /// `signer_proto::UNATTRIBUTED`.
    next_request: u32,
    engine: Engine<PageBlocks>,
    blocks: PageBlocks,
    /// Blocks whose PUT was answered ok (an effect's `after` is judged here).
    confirmed: BTreeSet<Cid>,
    /// Effects held until their `after` set is confirmed, in emitted order.
    held: Vec<(BTreeSet<Cid>, Effect)>,
    /// THE HEAD's publication in flight ([`Pub`]): the head it owes, the id of
    /// the sign request in flight (SG02: an answer under any other id answers
    /// something else, a superseded ask included), and a retryable refusal's
    /// backoff. The SAME type, and the same functions, as each site's.
    head: Pub,
    /// Each SITE's publication in flight, by app id (builder#117).
    sites: BTreeMap<String, Pub>,
    /// How each site's publication ended, or that it has not: the one owner.
    publications: BTreeMap<String, Publication>,
    /// [`PutPath::Wrapper`]: blocks to ask `Held` about again, when, and how
    /// many absents in a row.
    held_again: BTreeMap<Cid, (u64, u32)>,
    /// A `NotNext{current}` not yet ADOPTED: the register must be read to hold
    /// it first (invariant 1b).
    verify: Option<Verify>,
    /// Landings started, and the most UPDATEs one landing needed.
    landings: u32,
    most_landing_updates: u32,
    /// Repair PUTs the node's Block contract rejected (sdk#433): dropped, counted.
    repairs_rejected: u64,
    /// An OLD signer refused on a same-seq record while this page already
    /// stood on the register's head at that seq: the sign waits until a head
    /// read shows the register past it (sdk#225's S1b).
    old_signer_fork_at: Option<u64>,
    /// The records the signer signed for THIS page, by seq: `(root, bytes)`.
    /// What an equal-seq sighting is judged against — this page's claim at
    /// that seq, whose bytes it can land again if they win the tie-break.
    my_records: BTreeMap<u64, (Cid, Vec<u8>)>,
    /// When the register was last read (any head answer), for the backstop.
    last_head_at: u64,
    /// That read, whole: what an equal-seq tie-break hashes.
    last_head: Option<HeadRead>,
    /// A PUT to repeat at the next tick (a transient refusal).
    put_again: BTreeMap<Cid, Vec<u8>>,
    /// Blocks rebuilt from their group and PUT back (sdk#303): sent like a commit's PUT, but answered for NO
    /// commit -- an answer confirms nothing a commit or a head waits on. Left when a commit puts the same block.
    repair_puts: BTreeSet<Cid>,
    /// Deadlines of the ops in flight, and each op to re-send.
    /// Every op in flight: when it is due again, the op to re-send, when it
    /// went out and on which attempt (Karn: only an attempt-1 answer samples).
    deadlines: BTreeMap<Waiting, Deadline>,
    /// The app's PUTs: where each stands, and when it was first sent (its
    /// budget runs from there).
    app_puts: BTreeMap<String, (AppPut, u64)>,
    /// The retry clock of every node call (`rto`), and the GET window.
    rto: rto::Rto,
    window: rto::Window,
    /// GETs waiting for a place in the window, in the order asked.
    get_queue: std::collections::VecDeque<Cid>,
    /// The attempt a re-send continues from (set when an op times out).
    attempt_of: BTreeMap<Waiting, u32>,
    /// When each op still unanswered was FIRST sent: what "not answering for
    /// N s" counts from. Never a deadline (rule 8).
    first_of: BTreeMap<Waiting, u64>,
    /// The engine's own head read has been ANSWERED (a head, or certainly
    /// none): until then its tree is the empty one it started on, and a read
    /// answered from it would say "empty" about data that exists.
    recovered: bool,
    /// Has the engine been told its head (`HeadRead` / `HeadMissing`)?
    engine_has_head: bool,
    out: Vec<Op>,
    /// Every effect for a CLIENT (a write's state, a read's answer, a
    /// subscription's news), in the order the engine emitted them.
    client_fx: Vec<Effect>,
    /// Effects this executor does not act on (the read path's replies,
    /// subscriptions), for the caller.
    unusable: Vec<String>,
    /// A VIEW (sdk#239): this page writes nothing of its own -- no commit op,
    /// ever -- and still puts back a block it REBUILT for a read (a repair
    /// restores existing content-addressed bytes: no key, no head moved).
    /// THE one owner of "read-only": page-io and the web Session ask here.
    read_only: bool,
    /// THE PUBLISHED-HEAD FLOOR (sdk#349): the seq an app was published at,
    /// or 0 for none. A head read below it is "not yet" -- the node served a
    /// copy from before the publish -- and is never adopted: it ends no wait,
    /// so the head is asked again on the RTO (a GET with subscribe). SEQ
    /// ONLY: a same-seq race may replace the published root (#225b), so seq N
    /// with any root is the published version.
    head_floor: u64,
    /// The last head read that came in BELOW the floor (`None`: missing), and
    /// how many did: what a view says while it waits.
    below_floor: Option<(Option<u64>, u32)>,
    now: u64,
    /// The clock when the page was made. No round trip can be longer than the page has existed: a sample that is
    /// was dated against another clock's origin (sdk#397), and `answered` says so.
    born: u64,
    /// THE PAGE'S RECORDING (sdk#386's instrument work), `None` until a host attaches one: every op it sends, how it
    /// ended, and the retry clock it used. Bounded (it drops and counts, never grows), and never an input: the page
    /// holds it only as a `Probe`, which returns nothing.
    rec: Option<obs::Rec>,
    /// The page's clock when the recorder was attached: where the recording's offsets count from.
    rec_start: u64,
    /// The page's clock when the current observation WINDOW began (sdk#399): its record's offsets count from here.
    obs_start: u64,
    /// Sends so far: the last send's label ordinal.
    sends: u32,
    /// The first send the recording saw: an op sent before the recorder was attached is not in it, and neither is
    /// its end (it would be an answer to a request the recording never made).
    rec_from: u32,
    /// The send order outran what a label carries, and the recording was told (once).
    rec_overflowed: std::cell::Cell<bool>,
    /// Every record the signer returned — invariant 2's evidence.
    signer_records: BTreeSet<Vec<u8>>,
}

impl Page {
    /// A page with a fresh engine. It reads its head first (`ReadHead`), as a
    /// delegate's engine did.
    /// Its clock starts at 0: a test page, whose clock the test moves from there.
    pub fn new(params: Params, path: PutPath) -> Page {
        Page::new_at(params, path, Ms(0))
    }

    /// [`Page::new`] with its clock starting at `now` (the page model varies the origin per seed, sdk#397).
    pub fn new_at(params: Params, path: PutPath, now: Ms) -> Page {
        let mut p = Page::unstarted(params, path, now);
        p.start();
        p
    }

    /// START the engine: it reads its head first. Apart from [`Page::unstarted`] so a host can attach the page's
    /// recording ([`Page::record_into`]) before the first op goes out.
    pub fn start(&mut self) {
        // The key is in the SIGNER's secret store; the engine only states
        // where its authority comes from.
        self.step(Event::Start { key: KeySource::SecretStore, epochs: vec![EPOCH] });
    }

    /// A page whose engine has not been STARTED: [`crate::server::Server`]
    /// starts it on the client's `Identity`, as the delegate's shell did.
    ///
    /// `now` is the page's clock when it is made, in the clock's own origin (a browser page's is `Date.now()`,
    /// EPOCH ms): every op is dated by it. A page whose clock started at 0 dated its first requests at 0 and took
    /// their answers as a round trip of ~1.8e12 ms -- an SRTT no later sample could bring down, the RTO pinned at
    /// its 60 s ceiling for the page's life, so a lost PUT waited a minute (V's first save, Phase 4 realnet).
    pub fn unstarted(params: Params, path: PutPath, now: Ms) -> Page {
        let blocks = PageBlocks::default();
        let engine = Engine::new(params, blocks.clone());
        Page {
            path,
            device: [0; 16],
            hold_on_displace: false,
            engine,
            blocks,
            confirmed: BTreeSet::new(),
            held: Vec::new(),
            head: Pub::default(),
            sites: BTreeMap::new(),
            publications: BTreeMap::new(),
            held_again: BTreeMap::new(),
            verify: None,
            landings: 0,
            most_landing_updates: 0,
            repairs_rejected: 0,
            old_signer_fork_at: None,
            my_records: BTreeMap::new(),
            last_head_at: 0,
            last_head: None,
            put_again: BTreeMap::new(),
            repair_puts: BTreeSet::new(),
            deadlines: BTreeMap::new(),
            app_puts: BTreeMap::new(),
            rto: rto::Rto::default(),
            window: rto::Window::default(),
            get_queue: Default::default(),
            attempt_of: BTreeMap::new(),
            first_of: BTreeMap::new(),
            recovered: false,
            engine_has_head: false,
            out: Vec::new(),
            client_fx: Vec::new(),
            unusable: Vec::new(),
            read_only: false,
            head_floor: 0,
            below_floor: None,
            now: now.0,
            born: now.0,
            rec: None,
            rec_start: now.0,
            obs_start: now.0,
            sends: 0,
            rec_from: 1,
            rec_overflowed: std::cell::Cell::new(false),
            signer_records: BTreeSet::new(),
            next_request: 1,
        }
    }

    /// Any engine event from a client (a read, a subscription, a tick, a
    /// start), carried out like every other.
    pub fn event(&mut self, ev: Event) {
        self.client_event(ev);
    }

    /// A FORCED write (sdk#235, W8): every op key is read as `Expect::Any` — "I write
    /// this key whatever it holds" — so the engine takes it, counts it, and a
    /// `Lost` one is not re-sent. A write that depends on what was there says
    /// so with [`Page::write_reading`]; a reads-less write is refused as
    /// `Unread`, so there is no third way.
    pub fn write(&mut self, client: ClientId, write_id: WriteId, ops: Vec<(Vec<u8>, WriteOp)>) {
        let reads = ops.iter().map(|(k, _)| (k.clone(), engine::Expect::Any)).collect();
        self.write_reading(client, write_id, ops, reads);
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
        self.client_event(Event::Write { client, write_id, ops, reads, deferred: false });
    }

    /// Straight to the engine, WRITES INCLUDED.
    ///
    /// A write made before the head is recovered would land on the EMPTY tree
    /// the engine starts on, and the page would sign that from the genesis
    /// over a register nobody read (1b). The page used to hold such writes
    /// itself; the ENGINE now parks one and answers `Busy` to the rest
    /// (sdk#223), which is the same rule one layer down — and, unlike the
    /// hold, it ANSWERS: a held write was invisible to the client's outbox,
    /// so nothing re-sent it and nothing reported it.
    fn client_event(&mut self, ev: Event) {
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
        // RFC 6298 §5.5 — ONCE per tick, however many timed out. A PARKED GET
        // coming due timed nothing out: the node answered it.
        let timed_out = |w: &Waiting| self.deadlines.get(w).is_some_and(|d| d.sent);
        if late.iter().any(timed_out) {
            self.rto.timed_out();
        }
        // Every GET that timed out is LOST; the window decides whether that
        // begins a loss episode (`rto::Window::lost`).
        for w in &late {
            if let (Waiting::Get(_), Some(d)) = (w, self.deadlines.get(w).filter(|d| d.sent)) {
                self.window.lost(d.sent_at, d.attempt, now);
            }
        }
        for w in late {
            let d = self.end(&w, End::TimedOut).expect("listed");
            // A parked GET the engine no longer needs, or that the page now
            // holds (a repair rebuilt it), ends here.
            if let Waiting::Get(id) = w {
                if !d.sent && (self.blocks.get(&id).is_some() || !self.engine.awaits_block(&id)) {
                    self.drop_get(id);
                    continue;
                }
            }
            let op = d.op.clone();
            self.attempt_of.insert(w.clone(), d.attempt);
            match w {
                // A GET nobody answered is SENT AGAIN on the RTO (rule 7),
                // never turned into a miss: silence is not an answer, and a
                // miss is what the engine used to count down to
                // `Unavailable` ("block … could not be had"). The attempt
                // count carries over, so "not answering for N s" counts
                // from the first send.
                // It is LOST: it left the in-flight count when its deadline
                // was removed, and its re-send QUEUES for a place like any
                // new ask (sdk#345) -- behind the GETs already waiting, and
                // never outside the window (rule 9: every byte is paced).
                Waiting::Get(id) => {
                    if !self.get_queue.contains(&id) {
                        self.get_queue.push_back(id);
                    }
                }
                // Rebuilt, not replayed: the published head it names as prev
                // may have moved since it was first sent.
                // A landing's request, or this commit's.
                Waiting::Sign(Label::Head) if self.verify.as_ref().is_some_and(|v| v.landing) => self.ask_land(),
                Waiting::Sign(Label::Head) => self.ask_sign(),
                Waiting::PutApp(key) => self.send(Waiting::PutApp(key), op),
                _ => {
                    if w == Waiting::Update(Label::Head) {
                        if let Some(v) = self.verify.as_mut().filter(|v| v.landing) {
                            v.updates += 1;
                            self.most_landing_updates = self.most_landing_updates.max(v.updates);
                        }
                    }
                    self.send(w, op)
                }
            }
        }
        // The lost GETs queued above take free places, in queue order.
        self.fill_gets();
        for (id, bytes) in std::mem::take(&mut self.put_again) {
            self.send(Waiting::Put(id), Op::Put { id, bytes });
        }
        if self.head.sign_again.is_some_and(|at| now >= at) {
            self.head.sign_again = None;
            self.ask_sign();
        }
        let due: Vec<String> = self.sites.iter().filter(|(_, p)| p.sign_again.is_some_and(|at| now >= at)).map(|(a, _)| a.clone()).collect();
        for app in due {
            self.pub_mut(&Label::Site(app.clone())).sign_again = None;
            self.ask_sign_for(Label::Site(app));
        }
        if self.verify.as_ref().is_some_and(|v| v.again_at.is_some_and(|at| now >= at)) {
            if let Some(v) = self.verify.as_mut() {
                v.again_at = None;
            }
            self.send(Waiting::Verify, Op::ReadHead { label: Label::Head });
        }
        // THE BACKSTOP: a hint can be dropped, so an idle page reads the
        // register at least every HEAD_BACKSTOP_MS.
        if self.engine_has_head && self.verify.is_none() && !self.reading_head() && now.saturating_sub(self.last_head_at) >= HEAD_BACKSTOP_MS {
            self.last_head_at = now;
            self.send(Waiting::Hint, Op::ReadHead { label: Label::Head });
        }
        // A register that does not answer is asked again on the RTO for as
        // long as it takes (rules 7, 8) — the engine's own recovery read, and
        // the read that decides what a `NotNext` means — and `not_answering`
        // says for how long. Never turned into `HeadMissing` (that would open
        // an empty tree over a register merely unreachable, sdk#175), and
        // never given up.
        let due: Vec<Cid> = self.held_again.iter().filter(|(_, (at, _))| now >= *at).map(|(id, _)| *id).collect();
        for id in due {
            self.send(Waiting::Held(id), Op::AskHeld { id });
        }
        let stale: Vec<Label> = std::iter::once((Label::Head, &self.head))
            .chain(self.sites.iter().map(|(a, p)| (Label::Site(a.clone()), p)))
            .filter(|(l, p)| p.owed.as_ref().is_some_and(|o| o.record.is_some() && o.stale_reads > 0) && !self.deadlines.contains_key(&Waiting::ReadBack(l.clone())))
            .map(|(l, _)| l)
            .collect();
        for label in stale {
            self.send(Waiting::ReadBack(label.clone()), Op::ReadHead { label });
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
            Answer::NotSent { op, why } => {
                // WHICH WAIT AN OP HAS is stated once: the deadline `send`
                // recorded for it (an op alone does not say -- a head read
                // is sent for five different waits). Every wait on this op
                // ends; there is no kind for which nothing does.
                let ended: Vec<Waiting> = self.deadlines.iter().filter(|(_, d)| d.op == op).map(|(w, _)| w.clone()).collect();
                for w in &ended {
                    self.answered(w);
                }
                // A site's publication waits on nothing else: it ENDS, named (every publication ends).
                for w in &ended {
                    if let Waiting::Sign(Label::Site(app)) | Waiting::Update(Label::Site(app)) | Waiting::ReadBack(Label::Site(app)) = w {
                        self.end_site(app, Publication::Refused(format!("not sent: {why}")));
                    }
                }
                self.unusable.push(format!("not sent: {why}"));
            }
            Answer::SiteRefused { app, said } => {
                if self.answered(&Waiting::Update(Label::Site(app.clone()))).is_some() {
                    self.end_site(&app, Publication::Refused(format!("the node refused the site's PUT: {said}")));
                }
            }
            Answer::AppPutOk(key) => {
                if self.answered(&Waiting::PutApp(key.clone())).is_some() {
                    if let Some(p) = self.app_puts.get_mut(&key) {
                        p.0 = AppPut::Put;
                    }
                }
            }
            Answer::AppPutRefused { key, said } => {
                if self.answered(&Waiting::PutApp(key.clone())).is_some() {
                    if let Some(p) = self.app_puts.get_mut(&key) {
                        p.0 = AppPut::Refused(said);
                    }
                }
            }
            Answer::PutOk(id) => {
                if self.answered(&Waiting::Put(id)).is_none() && self.confirmed.contains(&id) {
                    return; // a second answer to a re-sent PUT
                }
                self.put_again.remove(&id);
                // A repaired block's PUT is done: it confirms nothing (the architect's (b)).
                if self.repair_puts.contains(&id) {
                    return;
                }
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
                let bytes = self.blocks.get(&id).map(<[u8]>::to_vec);
                if let (true, Some(bytes)) = (absents >= HELD_ABSENTS, bytes) {
                    self.held_again.remove(&id);
                    self.put_again.insert(id, bytes);
                } else {
                    // A block this page has no bytes for (a FOREIGN member it only asks about, safety gap class 2)
                    // is never put from here: it is asked again, for as long as it takes (rule 7).
                    let wait = (BACKOFF_MS << absents.min(16)).min(rto::RTO_MAX_MS as u64);
                    self.held_again.insert(id, (now + wait, absents));
                }
            }
            Answer::PutRefused { id, transient } => {
                let Some(op) = self.answered(&Waiting::Put(id)) else { return };
                if transient {
                    if let Op::Put { bytes, .. } = op {
                        self.put_again.insert(id, bytes);
                    }
                } else if self.repair_puts.remove(&id) {
                    // A REPAIR's PUT rejected (sdk#433): dropped, and counted -- never silently.
                    self.repairs_rejected += 1;
                } else {
                    // FINAL (sdk#433): the node's Block contract refused these bytes; never put again.
                    self.step(Event::PutRejected(id));
                }
            }
            Answer::Got { id, bytes } => {
                let Some(attempt) = self.answered_get(id) else { return };
                // Verified BEFORE it joins the page's memory: a block that is
                // not its id is not kept, and the engine hears a miss.
                let good = engine::read::matches_id(&id, &bytes);
                if good {
                    self.blocks.insert(id, &bytes);
                } else {
                    // Parked BEFORE the engine hears it: the engine re-asks
                    // inside that step, and must already see the GET pending,
                    // or the re-ask goes out at once.
                    self.park_get(id, attempt);
                }
                self.step(Event::BlockArrived { id, bytes });
            }
            Answer::GetMissed(id) => {
                if let Some(attempt) = self.answered_get(id) {
                    // A real answer: the engine hears it (a NotFound starts a
                    // repair from the block's group), and the block itself is
                    // asked again on a backoff -- a node that has not got it
                    // YET is the ordinary case, and one that lost it may have
                    // it back from a keeper or a late PUT.
                    // Parked BEFORE the engine hears it: the engine re-asks
                    // inside that step, and must already see the GET pending,
                    // or the re-ask goes out at the speed of the answers
                    // (3,001 GETs in 5 min, measured).
                    self.park_get(id, attempt);
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
                if !answers_a_sign(&s) {
                    return;
                }
                // Which label's sign it answers: the one whose sign in flight has this id.
                let label = if self.head.sign_id == Some(id) {
                    Label::Head
                } else if let Some(app) = self.sites.iter().find(|(_, p)| p.sign_id == Some(id)).map(|(a, _)| a.clone()) {
                    Label::Site(app)
                } else {
                    return;
                };
                if self.answered(&Waiting::Sign(label.clone())).is_none() {
                    return; // an answer to a sign request already answered
                }
                self.pub_mut(&label).sign_id = None;
                match label {
                    Label::Head => self.on_signer(s),
                    Label::Site(app) => self.on_site_signer(&app, s),
                }
            }
            Answer::Updated { label: Label::Site(app) } => {
                // A site's PUT answered: its record is read back, the one way it is Published.
                if self.answered(&Waiting::Update(Label::Site(app.clone()))).is_some() {
                    self.send(Waiting::ReadBack(Label::Site(app.clone())), Op::ReadHead { label: Label::Site(app) });
                }
            }
            Answer::Updated { label: Label::Head } => {
                // Its read-back already done by the pushed state (sdk#378 P3): the
                // UPDATE's wait ended there, so this answer asks nothing more.
                if self.answered(&Waiting::Update(Label::Head)).is_some() {
                    // A LANDING's UPDATE is judged by its own read — does the
                    // register now hold the head the signer named — never by
                    // this commit's read-back.
                    if self.verify.as_ref().is_some_and(|v| v.landing) {
                        self.send(Waiting::Verify, Op::ReadHead { label: Label::Head });
                    } else {
                        self.send(Waiting::ReadBack(Label::Head), Op::ReadHead { label: Label::Head });
                    }
                }
            }
            // One register read can answer both a recovery read and a
            // read-back: a head is a head, whoever asked.
            // A SITE's read answers only its own read-back: the engine never hears a site (architect).
            Answer::Head { label: Label::Site(app), read } => {
                if self.answered(&Waiting::ReadBack(Label::Site(app.clone()))).is_some() {
                    self.on_site_read(&app, read);
                }
            }
            Answer::Head { label: Label::Head, read } => {
                // Below the published-head floor: not yet. A REAL answer (the
                // node is answering, with a copy from before the publish), so
                // it ends each head read it answers -- and adopts nothing:
                // each is PARKED at the one backoff and asked again (a GET with
                // subscribe), as a GET answered without its block is.
                if self.head_floor > 0 && read.as_ref().is_none_or(|r| r.seq < self.head_floor) {
                    let seen = read.as_ref().map(|r| r.seq);
                    let n = self.below_floor.map_or(0, |(_, n)| n);
                    self.below_floor = Some((seen, n + 1));
                    let reads = [Waiting::Warm, Waiting::RecoverHead, Waiting::ReadBack(Label::Head), Waiting::Verify, Waiting::Hint];
                    for w in reads {
                        let Some(attempt) = self.deadlines.get(&w).filter(|d| d.sent).map(|d| d.attempt) else { continue };
                        self.answered(&w);
                        self.park(w, Op::ReadHead { label: Label::Head }, attempt);
                    }
                    return;
                }
                self.last_head_at = self.now;
                let h = read.as_ref().map(|r| (r.seq, r.root()));
                self.last_head = read;
                self.answered(&Waiting::Warm);
                // S1b: the register moved past an old signer's same-seq
                // record, so it signs again.
                if let (Some(at), Some((seq, _))) = (self.old_signer_fork_at, h) {
                    if seq > at {
                        self.old_signer_fork_at = None;
                        if self.head.owed.is_some() {
                            self.head.sign_again = Some(self.now);
                        }
                    }
                }
                // THE ONE JUDGEMENT (sdk#396): every head read this answer ends acts on this verdict, never on `h`.
                let j = judge(self.last_head.as_ref(), &self.my_records);
                let recover_attempt = self.deadlines.get(&Waiting::RecoverHead).map(|d| d.attempt);
                if self.answered(&Waiting::RecoverHead).is_some() {
                    match &j {
                        Judged::NoHead => {
                            self.step(Event::HeadMissing);
                            self.recovered = true;
                        }
                        Judged::Head(heard) => {
                            self.adopt(*heard, Adopt::Recovery);
                            self.recovered = true;
                        }
                        // The engine's own re-read, mid-commit, shows a head THIS page's record beats: my UPDATE has
                        // not merged there yet. Not adopted (the commit would die for a head about to lose): the
                        // engine's read is asked again at the one backoff, and the read-back owns the landing.
                        Judged::MineWins { .. } => self.park(Waiting::RecoverHead, Op::ReadHead { label: Label::Head }, recover_attempt.unwrap_or(1)),
                    }
                }
                if self.answered(&Waiting::ReadBack(Label::Head)).is_some() {
                    self.on_read_back(&j);
                }
                if self.answered(&Waiting::Verify).is_some() {
                    self.on_verify(&j);
                }
                if self.answered(&Waiting::Hint).is_some() {
                    self.on_hint(&j);
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
                    self.note_record(&state);
                    self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state });
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
        let Some(owed) = self.head.owed.as_mut() else { return };
        self.head.sign_refusals = match &s {
            A::Refused(Why::RootNotHeld | Why::HeadUnknown | Why::RecordNotSaved) => self.head.sign_refusals,
            _ => 0,
        };
        match s {
            A::Signed(state) => {
                self.signer_records.insert(state.clone());
                if let Some(h) = HeadRead::from_record(&state) {
                    self.my_records.insert(h.seq, (h.root(), state.clone()));
                }
                owed.record = Some(state.clone());
                owed.stale_reads = 0;
                self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state });
            }
            A::AlreadySigned(state) => {
                // Requirement 2: ONE signature per prev, and it is landed as
                // it is — its blocks were stored before it was signed.
                self.signer_records.insert(state.clone());
                if let Some(h) = HeadRead::from_record(&state) {
                    self.my_records.insert(h.seq, (h.root(), state.clone()));
                }
                owed.record = Some(state.clone());
                owed.stale_reads = 0;
                // Its root may not be this commit's (a record another page
                // of this key made at this seq). Nothing more is needed here:
                // the read-back judges by THIS commit's (seq, root), so that
                // record reads back as the same seq under another root — a
                // conflict, never Published (the model's mutant M5: a second
                // mechanism for this was dead weight).
                self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state });
            }
            // NOT ADOPTED YET (invariant 1b). The signer's truth is the later
            // of its RECORD and the register, and the record can be AHEAD:
            // signed, its UPDATE not landed (ENGINE-SHAPE §5 3b). Adopted as
            // it was, a later write was told Published at a head no reader
            // can see (the model, seed 10). So the register is read first.
            A::NotNext { current } => {
                self.verify = Some(Verify { seq: current.seq, root: current.root, landing: false, again_at: None, tries: 0, updates: 0, from: None });
                self.send(Waiting::Verify, Op::ReadHead { label: Label::Head });
            }
            // RETRYABLE, on a doubling backoff (engineer2's table):
            // RootNotHeld — the root's PUT is still landing; HeadUnknown — the
            // node does not hold the register, so a page read of it is sent
            // first to make it held; RecordNotSaved — the signer could not
            // write its record and signed nothing. A node's "queue full" is
            // not a signer answer: it is re-asked by the deadline.
            A::Refused(why @ (Why::RootNotHeld | Why::HeadUnknown | Why::RecordNotSaved)) => {
                if why == Why::HeadUnknown {
                    self.send(Waiting::Warm, Op::ReadHead { label: Label::Head });
                }
                self.head.sign_refusals += 1;
                let wait = (BACKOFF_MS << self.head.sign_refusals.min(5)).min(rto::RTO_MAX_MS as u64);
                self.head.sign_again = Some(self.now + wait);
            }
            // THE SAME IDENTITY NEVER FORKS (owner, sdk#225). Only an OLD
            // signer says this (the new one answers `NotNext{read}`): another
            // device's head won the Register's tie-break at this seq. It is
            // recovered as that `NotNext` is — read, then adopt (1b) — unless
            // the page already stands on `read`: then the old rule will refuse
            // every ask until the Register passes this seq, so the sign is NOT
            // re-asked (a loop otherwise) and that is named, once.
            A::Refused(Why::Forked { read, .. }) => {
                if self.engine.published_seq() == read.seq && self.engine.published_root() == read.root {
                    if self.old_signer_fork_at != Some(read.seq) {
                        self.old_signer_fork_at = Some(read.seq);
                        self.unusable.push(format!(
                            "SIGNER UPGRADE NEEDED: this signer predates the same-identity rule (sdk#225) and refuses every sign while its record and the register differ at seq {}; load the current version (its signer ships with the page). It signs again once the register moves past that seq",
                            read.seq
                        ));
                    }
                } else {
                    self.verify = Some(Verify { seq: read.seq, root: read.root, landing: false, again_at: None, tries: 0, updates: 0, from: None });
                    self.send(Waiting::Verify, Op::ReadHead { label: Label::Head });
                }
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
    /// the web layer frames `Op::ReadHead { label: Label::Head }` as that.
    fn on_verify(&mut self, j: &Judged) {
        let Some(v) = self.verify.clone() else { return };
        let h = j.shown();
        let reg_seq = h.map_or(0, |(s, _)| s);
        // THIS page's own record at the register's seq is another root that
        // wins the tie-break: LAND it (its UPDATE has not merged here yet)
        // rather than adopt a head about to lose. Judged by the record, never
        // by what the signer named: at an equal seq the signer names the
        // REGISTER's head (rule c), so `v.root` is theirs.
        if let Judged::MineWins { seq, record, .. } = j {
            if *seq >= v.seq {
                if let Some(v) = self.verify.as_mut() {
                    v.landing = true;
                    v.updates += 1;
                    self.most_landing_updates = self.most_landing_updates.max(v.updates);
                }
                self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state: record.clone() });
                return;
            }
        }
        if let Judged::Head(heard) = j {
            if heard.seq() >= v.seq {
                self.verify = None;
                self.head.owed = None;
                self.adopt(*heard, Adopt::Conflict);
                return;
            }
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

    /// A register read on a hint (the node's `HeadChanged`, or the idle
    /// backstop): the RELOAD TRIGGER (sdk#225). This read IS the register read
    /// 1b asks for, so what it shows may be adopted as it is.
    ///
    /// * No head, or the head the engine stands on, or an older one: nothing
    ///   (a hint never opens an empty tree).
    /// * This page's owed head, or another root at its seq: the read-back's
    ///   judgement, which knows this page's claim there.
    /// * The SAME seq as the published head under another root, and this
    ///   page's record there wins the tie-break: it is landed again (its
    ///   UPDATE has not merged here), never displaced.
    /// * Otherwise a newer head, or a same-seq winner: ADOPTED
    ///   (`HeadConflict`). A commit in flight dies `Lost`, as in any conflict.
    fn on_hint(&mut self, j: &Judged) {
        let Some((seq, root)) = j.shown() else { return };
        if !self.engine_has_head || self.verify.is_some() {
            return;
        }
        let (pseq, proot) = (self.engine.published_seq(), self.engine.published_root());
        if (seq, root) == (pseq, proot) || seq < pseq {
            return;
        }
        if self.head.owed.as_ref().is_some_and(|o| o.record.is_some() && o.seq == seq) {
            self.on_read_back(j);
            return;
        }
        match j {
            Judged::MineWins { record, .. } => self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state: record.clone() }),
            Judged::Head(heard) => {
                self.head.owed = None;
                self.adopt(*heard, Adopt::Conflict);
            }
            Judged::NoHead => {}
        }
    }

    /// The socket was REPLACED (sdk#376): EVERY op on the wire went out on the
    /// socket that is gone -- block GETs, PUTs, the sign, the update, the head
    /// reads and the subscription they carry -- so each is sent again NOW, on
    /// the new one. Not a timeout: nothing was lost on the PATH, so no loss is
    /// recorded, the window is not halved, the RTO does not back off, and the
    /// attempt stays what it was (a re-send is no RTT sample, Karn). Parked
    /// ops (`sent == false`) were not on the wire and keep their backoff. With
    /// no head read among them, a page that has a head reads it now: a GET
    /// with subscribe, the one path, so the new connection is subscribed at
    /// once and not at the 120 s backstop.
    pub fn reconnected(&mut self, now: Ms) {
        self.now = now.0;
        // Re-sent in the order they first queued, and queued anew in it (sdk#390).
        let mut on_wire: Vec<(u32, Waiting)> = self.deadlines.iter().filter(|(_, d)| d.sent).map(|(w, d)| (d.seq, w.clone())).collect();
        on_wire.sort_unstable();
        let mut head_read = false;
        for (_, w) in on_wire {
            let at = self.now + self.backoff(self.deadlines[&w].attempt);
            // The send on the old socket is WITHDRAWN (its answer cannot come), and the re-send is a new send.
            self.record_end(&w, &self.deadlines[&w], End::Withdrawn);
            let seq = self.next_send();
            let d = self.deadlines.get_mut(&w).expect("listed");
            d.at = at;
            d.armed = at;
            d.sent_at = self.now;
            d.resent = true;
            d.seq = seq;
            // Every label's in-flight read is re-sent (each re-subscribes its own register);
            // only an in-flight HEAD read stands in for the fallback below.
            head_read |= matches!(d.op, Op::ReadHead { label: Label::Head });
            self.out.push(d.op.clone());
            self.record_send(&w, &self.deadlines[&w]);
        }
        if !head_read && self.engine_has_head {
            self.last_head_at = self.now;
            self.send(Waiting::Hint, Op::ReadHead { label: Label::Head });
        }
    }

    /// The node said the head register changed (`HeadChanged`, a HINT a node
    /// can fabricate or drop): read it. ALWAYS — owed parity, an owed head
    /// or idle alike (the architect's #5) — except while a read of the same
    /// register is already owed: a verify in progress, or this page's own
    /// READ-BACK (sdk#378 P2: its UPDATE is out, and the read-back that
    /// follows its answer reads the register and judges whatever it holds —
    /// this page's head, a newer one, a same-seq winner — so a second read
    /// adds nothing but a node op on the node's one queue, F61).
    pub fn head_hint(&mut self) {
        if self.verify.is_none() && !self.read_back_owed() && !self.deadlines.contains_key(&Waiting::Hint) {
            self.send(Waiting::Hint, Op::ReadHead { label: Label::Head });
        }
    }

    /// Is this page's own read-back owed: a head signed, its UPDATE sent, not yet confirmed (sdk#378)?
    fn read_back_owed(&self) -> bool {
        self.head.owed.as_ref().is_some_and(|o| o.record.is_some())
    }

    /// The node pushed the head register's FULL state (sdk#378 P3, the
    /// architect's three conditions): (1) only a full state gets here
    /// (`wire`'s `HeadChanged.state`); (2) it is judged by the ONE read-back
    /// rule, and only as THIS page's own owed head -- exactly the owed seq and
    /// root confirms it, as a read-back GET showing it would; anything else is
    /// the hint it always was, answered by a real register read; (3) a dropped
    /// push changes nothing: the read-back GET after the UPDATE's answer does
    /// the job on its deadline. Nothing is ever ADOPTED from a push.
    pub fn head_pushed(&mut self, read: HeadRead) {
        let confirms = self.verify.is_none() && self.confirms(&read);
        if !confirms {
            // ALREADY KNOWN (the architect's done x E1 cell, #378): a full state that IS the head this page last
            // read -- the whole value, not only (seq, root) -- and the engine stands on, is news to nobody. No
            // read (one op on the node's one queue, F61); the backstop's clock restarts, as for a read. A DELTA
            // push carries no state and never gets here (`head_hint`, one read), nor does any other head.
            let known = self.last_head.as_ref().is_some_and(|h| h.seq == read.seq && h.value() == read.value());
            if known && (read.seq, read.root()) == self.published() {
                self.last_head_at = self.now;
                return;
            }
            return self.head_hint();
        }
        self.last_head_at = self.now;
        self.last_head = Some(read);
        // Whatever is still on the wire for the read-back this push stands in for -- the UPDATE whose answer would
        // ask it, or the GET itself -- ends with the owed head, in `drop_dead_head`, once the engine has published
        // its seq (COMMIT-LIFE ⁹): no RTT sample (a push is not an answer to either request).
        let j = judge(self.last_head.as_ref(), &self.my_records);
        self.on_read_back(&j);
    }

    /// A record the signer returned (`Signed`, or `AlreadySigned`: the one it
    /// already made for that prev, under THIS page's key): kept whole for
    /// invariant 2, and by the head it names for the tie-break.
    fn note_record(&mut self, state: &[u8]) {
        self.signer_records.insert(state.to_vec());
        if let Some(h) = HeadRead::from_record(state) {
            self.my_records.insert(h.seq, (h.root(), state.to_vec()));
        }
    }

    /// Is a register read already in flight?
    fn reading_head(&self) -> bool {
        [Waiting::Warm, Waiting::RecoverHead, Waiting::Verify, Waiting::ReadBack(Label::Head), Waiting::Hint]
            .iter()
            .any(|w| self.deadlines.contains_key(w))
    }

    /// Does `read` CONFIRM this page's commit? Exactly the owed head's (seq, root), once signed -- or, the owed head
    /// gone, exactly the head the engine already published, as THIS page's own record, whole (the Register's
    /// tie-break is over values: the same (seq, root) under another ledger is not it): the SECOND of a commit's two
    /// confirmations (E1 and E4, either order). Both go to the engine, which alone decides (COMMIT-LIFE ⁹, sdk#414):
    /// its `pending` take makes the second a no-op. The root is part of it: a record another page of this key made at
    /// the same seq is not this commit (`AlreadySigned`), and the engine's take checks the seq only.
    fn confirms(&self, read: &HeadRead) -> bool {
        let pair = (read.seq, read.root());
        match self.head.owed.as_ref() {
            Some(o) => o.record.is_some() && (o.seq, o.root) == pair,
            None => {
                pair == self.engine_published()
                    && self.my_records.get(&read.seq).and_then(|(_, record)| HeadRead::from_record(record)).is_some_and(|mine| mine.value() == read.value())
            }
        }
    }

    /// The register read back after an UPDATE: the only way a commit is
    /// Published (F56: the UPDATE's answer says nothing).
    fn on_read_back(&mut self, j: &Judged) {
        // `last_head` is the read `j` was judged from (both callers judge it just before).
        if let (Judged::Head(heard), Some(read)) = (j, self.last_head.as_ref()) {
            if self.confirms(read) {
                return self.step(Event::HeadConfirmed(heard.seq()));
            }
        }
        let Some(owed) = self.head.owed.as_mut() else { return };
        if owed.record.is_none() {
            return; // a read-back outlived its commit
        }
        let want = (owed.seq, owed.root);
        match j {
            // Not visible yet -- or THIS page's record wins the tie-break
            // against what the register shows (the node has not merged my
            // UPDATE; adopting theirs would drop a head that is about to win,
            // the architect's attack on sdk#225, case 1): read again; after
            // HEAD_READS, UPDATE again.
            Judged::Head(heard) if heard.seq() < want.0 => {
                owed.stale_reads += 1;
                if owed.stale_reads >= HEAD_READS {
                    owed.stale_reads = 0;
                    let state = owed.record.clone().expect("an UPDATE was sent");
                    self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state });
                }
            }
            Judged::MineWins { .. } => {
                owed.stale_reads += 1;
                if owed.stale_reads >= HEAD_READS {
                    owed.stale_reads = 0;
                    let state = owed.record.clone().expect("an UPDATE was sent");
                    self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state });
                }
            }
            Judged::Head(heard) => {
                self.head.owed = None;
                self.adopt(*heard, Adopt::Conflict);
            }
            Judged::NoHead => {
                let state = owed.record.clone().expect("an UPDATE was sent");
                self.send(Waiting::Update(Label::Head), Op::Update { label: Label::Head, state });
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
        self.head.sign_id = Some(id);
        let ledger = self.sign_ledger_of(prev_seq, prev_root, prev_seq + 1, root);
        self.send(Waiting::Sign(Label::Head), Op::Sign { id, prev_seq, prev_root, seq: prev_seq + 1, root, ledger, label: Label::Head });
    }

    fn ask_sign(&mut self) {
        self.ask_sign_for(Label::Head);
    }

    /// THE sign request, for any label: from the owed's prev `(seq - 1, base)`, under a fresh id (SG02). A head's
    /// value carries its ledger; a site's is the bundle hash alone.
    fn ask_sign_for(&mut self, label: Label) {
        let Some(o) = self.pub_ref(&label).and_then(|p| p.owed.clone()) else { return };
        let id = self.next_request;
        self.next_request = self.next_request.checked_add(1).unwrap_or(1);
        self.pub_mut(&label).sign_id = Some(id);
        // The prev is the commit's BASE, never "whatever the engine
        // publishes now": signed after a foreign winner was adopted, that
        // would name the winner as prev of a root built without it, and the
        // winner's rows would be lost from every later head.
        let (prev_seq, prev_root, seq, root) = (o.seq - 1, o.base, o.seq, o.root);
        let ledger = match label {
            Label::Head => self.sign_ledger_of(prev_seq, prev_root, seq, root),
            Label::Site(_) => Vec::new(),
        };
        let op = Op::Sign { id, prev_seq, prev_root, seq, root, ledger, label: label.clone() };
        self.send(Waiting::Sign(label), op);
    }

    /// A label's publication: the head's, or a site's (made on first use).
    fn pub_mut(&mut self, label: &Label) -> &mut Pub {
        match label {
            Label::Head => &mut self.head,
            Label::Site(app) => self.sites.entry(app.clone()).or_default(),
        }
    }

    fn pub_ref(&self, label: &Label) -> Option<&Pub> {
        match label {
            Label::Head => Some(&self.head),
            Label::Site(app) => self.sites.get(app),
        }
    }

    /// PUBLISH A SITE (builder#117): `value` is blake3 of its web part, which page-io holds and frames. First the
    /// site is READ -- NotFound is no site yet (the genesis), a record is where the next version follows from, a
    /// refused read is silence re-asked on the RTO -- then it is signed, written and read back as the head is.
    /// Publishing again while one is in flight starts over with the new bundle.
    pub fn publish_site(&mut self, app: &str, value: Cid, now: Ms) {
        self.now = now.0;
        let label = Label::Site(app.to_string());
        for w in [Waiting::Sign(label.clone()), Waiting::Update(label.clone()), Waiting::ReadBack(label.clone())] {
            self.end(&w, End::Withdrawn);
            self.attempt_of.remove(&w);
        }
        // `seq == 0`: the site's version is not read yet (its first read decides it).
        self.sites.insert(app.to_string(), Pub { owed: Some(Owed { seq: 0, root: value, base: [0u8; 32], record: None, stale_reads: 0 }), ..Pub::default() });
        self.publications.insert(app.to_string(), Publication::Publishing { waiting_for: None });
        self.send(Waiting::ReadBack(label.clone()), Op::ReadHead { label });
    }

    /// How a site's publication stands (builder#117): the one owner of that fact. `None`: never asked.
    pub fn publication(&self, app: &str) -> Option<&Publication> {
        self.publications.get(app)
    }

    /// The person CANCELS a site's publication: every wait of it ends, and it says so.
    pub fn cancel_site(&mut self, app: &str) {
        let label = Label::Site(app.to_string());
        for w in [Waiting::Sign(label.clone()), Waiting::Update(label.clone()), Waiting::ReadBack(label.clone())] {
            self.end(&w, End::Withdrawn);
            self.attempt_of.remove(&w);
        }
        if self.sites.remove(app).is_some() {
            self.publications.insert(app.to_string(), Publication::Cancelled);
        }
    }

    /// A site's publication ENDS (published, superseded, refused): its state and waits go.
    fn end_site(&mut self, app: &str, how: Publication) {
        self.cancel_site(app);
        self.publications.insert(app.to_string(), how);
    }

    /// A SITE's signer answer (the module table's Site column): Signed -> the site's write; AlreadySigned with THIS
    /// bundle -> the same; with another -> ask FROM that record (a site record cannot be landed without its web
    /// bytes, which this page does not hold: the livelock ends here); NotNext -> ask from `current` (nothing is
    /// adopted, so 1b's read-first is not needed: the architect's Q1); a retryable refusal -> the head's backoff;
    /// any other refusal -> named, and the publication ends.
    fn on_site_signer(&mut self, app: &str, s: signer_proto::Answer) {
        use signer_proto::{Answer as A, Why};
        let label = Label::Site(app.to_string());
        let Some(owed) = self.sites.get(app).and_then(|p| p.owed.clone()) else { return };
        if !matches!(s, A::Refused(Why::HeadUnknown | Why::RecordNotSaved)) {
            self.pub_mut(&label).sign_refusals = 0;
        }
        // What the publication waits on, stated (not "not answering": the signer answered).
        let waiting_for = matches!(s, A::Refused(Why::HeadUnknown)).then_some(WAITING_FOR_SITE);
        if let Some(Publication::Publishing { waiting_for: w }) = self.publications.get_mut(app) {
            *w = waiting_for;
        }
        match s {
            A::Signed(state) => self.site_write(app, state),
            A::AlreadySigned(state) => match HeadRead::from_record(&state) {
                Some(h) if h.value() == owed.root.as_slice() => self.site_write(app, state),
                Some(h) => {
                    let Ok(v) = <[u8; 32]>::try_from(h.value()) else { return self.end_site(app, Publication::Refused("the signer's record is not a site's".into())) };
                    self.rebase_site(app, h.seq, v);
                }
                None => self.end_site(app, Publication::Refused("the signer's record does not read".into())),
            },
            A::NotNext { current } => self.rebase_site(app, current.seq, current.root),
            A::Refused(Why::HeadUnknown | Why::RecordNotSaved) => {
                let now = self.now;
                let p = self.pub_mut(&label);
                p.sign_refusals += 1;
                let wait = (BACKOFF_MS << p.sign_refusals.min(5)).min(rto::RTO_MAX_MS as u64);
                p.sign_again = Some(now + wait);
            }
            A::Refused(why) => self.end_site(app, Publication::Refused(format!("{why:?}"))),
            other => self.end_site(app, Publication::Refused(format!("the signer answered a site's sign with {other:?}"))),
        }
    }

    /// Sign the site's bundle as the version after `(seq, value)`.
    fn rebase_site(&mut self, app: &str, seq: u64, value: Cid) {
        if let Some(o) = self.sites.get_mut(app).and_then(|p| p.owed.as_mut()) {
            o.seq = seq + 1;
            o.base = value;
            o.record = None;
            o.stale_reads = 0;
        }
        self.ask_sign_for(Label::Site(app.to_string()));
    }

    /// The site's write: the signer's exact bytes (invariant 2), PUT in the site's framing by page-io.
    fn site_write(&mut self, app: &str, state: Vec<u8>) {
        if let Some(o) = self.sites.get_mut(app).and_then(|p| p.owed.as_mut()) {
            o.record = Some(state.clone());
            o.stale_reads = 0;
        }
        let label = Label::Site(app.to_string());
        self.send(Waiting::Update(label.clone()), Op::Update { label, state });
    }

    /// A SITE's read (the module table's Site column). Before it signs: where the next version follows from
    /// (NotFound = the genesis). After its write: its record = Published; older or none = read again, and after
    /// HEAD_READS write again; the same version with another bundle that MY record beats = read again (the merge
    /// keeps mine); newer, or the same version won by another = Superseded, reported and never overwritten.
    /// Never the engine's: a site adopts nothing.
    fn on_site_read(&mut self, app: &str, read: Option<HeadRead>) {
        let Some(owed) = self.sites.get(app).and_then(|p| p.owed.clone()) else { return };
        let label = Label::Site(app.to_string());
        if owed.seq == 0 {
            let (seq, value) = match &read {
                None => (0, [0u8; 32]),
                Some(h) => match <[u8; 32]>::try_from(h.value()) {
                    Ok(v) => (h.seq, v),
                    Err(_) => return self.end_site(app, Publication::Refused("the site holds a record that is not a site's".into())),
                },
            };
            return self.rebase_site(app, seq, value);
        }
        let Some(record) = owed.record.clone() else { return };
        match judge_site(owed.seq, owed.root.as_slice(), read.as_ref()) {
            SiteJudged::Mine => self.end_site(app, Publication::Published { version: owed.seq }),
            SiteJudged::Superseded(version) => self.end_site(app, Publication::Superseded { version }),
            SiteJudged::NotYet => {
                let o = self.pub_mut(&label).owed.as_mut().expect("owed");
                o.stale_reads += 1;
                if o.stale_reads >= HEAD_READS {
                    o.stale_reads = 0;
                    self.send(Waiting::Update(label.clone()), Op::Update { label, state: record });
                }
            }
        }
    }

    /// An op goes out, due again one RTO from now. A GET waits for a place
    /// in the window.
    fn send(&mut self, w: Waiting, op: Op) {
        if let Waiting::Get(id) = w {
            if self.gets_in_flight() >= self.window.size() && !self.deadlines.contains_key(&w) {
                if !self.get_queue.contains(&id) {
                    self.get_queue.push_back(id);
                }
                return;
            }
        }
        let attempt = self.attempt_of.remove(&w).map_or(1, |a| a + 1);
        self.first_of.entry(w.clone()).or_insert(self.now);
        // PER-OP backoff on top of the shared RTO (TCP backs off per
        // segment): the n-th send of one op waits RTO x 2^(n-1), capped at
        // the RTO's ceiling. The shared RTO alone is pulled back down by every
        // other op's answers, so an op nobody answers was re-sent at that
        // small RTO for ever -- thousands of GETs in five minutes (measured
        // on a silent node once silence stopped ending a read).
        // A FIRST send waits its place in the node's one queue (sdk#390, F61): an RTO for each op on the wire ahead of
        // it, and one for itself. A re-send keeps its backoff.
        let ahead = self.deadlines.iter().filter(|(k, d)| d.sent && **k != w).count() as u64;
        let at = self.now + if attempt == 1 { queued_wait(self.rto.rto_ms(), ahead) } else { self.backoff(attempt) };
        let seq = self.next_send();
        let d = Deadline { at, armed: at, op: op.clone(), sent_at: self.now, attempt, sent: true, resent: false, seq, alone: ahead == 0 };
        self.record_send(&w, &d);
        // A send still on the wire for the same op is SUPERSEDED by this one: its end is said, never overwritten.
        if let Some(old) = self.deadlines.insert(w.clone(), d) {
            self.record_end(&w, &old, End::Withdrawn);
        }
        self.out.push(op);
    }

    /// The next send's place in the page's send order (the recording's label ordinal).
    fn next_send(&mut self) -> u32 {
        // SATURATES, never wraps: a wrapped send order would reuse early ordinals (instrument v2, the architect).
        self.sends = self.sends.saturating_add(1);
        self.sends
    }

    /// The recording's label for a send, or `None` once the send order has outrun what a label can carry
    /// (`instrument::label::MAX_ORDINAL`): the page then stops labelling, and says so ONCE -- never wraps into
    /// another kind's range.
    fn send_label(&self, seq: u32) -> Option<instrument::Label> {
        let label = instrument::Label::new(instrument::Kind::Request, seq);
        if label.is_none() && !self.rec_overflowed.replace(true) {
            if let Some(rec) = &self.rec {
                use instrument::{vocab::DropReason, Entry, Event, Key, OpId, Probe};
                let entry = Entry { key: Key::DroppedMsgs, value: DropReason::TooLarge.code() };
                rec.event(Event::Counter { site: instrument::Site::of("page::recording"), op: OpId::NONE, entry });
            }
        }
        label
    }

    /// AN OP ENDS: THE one way a deadline leaves (a control counts this crate's removals). What the recording
    /// says of it is decided here, once: answered, timed out, or withdrawn.
    fn end(&mut self, w: &Waiting, how: End) -> Option<Deadline> {
        let d = self.deadlines.remove(w)?;
        self.record_end(w, &d, how);
        Some(d)
    }

    /// Where the recording's offsets count from: the page's clock when the recorder was attached, never 0 and
    /// never the page's own origin (the architect on instrument#14). Coarsened by the vocabulary's rule.
    fn offset(&self, t: u64) -> u64 {
        instrument::vocab::coarsen_ms(t.saturating_sub(self.rec_start))
    }

    /// A SEND: its Request edge, labelled by its place in the send order, and the retry clock it went out on.
    fn record_send(&self, w: &Waiting, d: &Deadline) {
        use instrument::{vocab::coarsen_ms, Dir, Entry, Event, Key, Probe};
        let Some(rec) = self.rec.as_ref().filter(|_| d.seq >= self.rec_from) else { return };
        let Some(id) = self.send_label(d.seq) else { return };
        let site = op_site(w);
        rec.event(Event::Edge { site, dir: Dir::Request, id });
        for entry in [
            Entry { key: Key::Attempts, value: u64::from(d.attempt) },
            Entry { key: Key::OffsetMs, value: self.offset(d.sent_at) },
            Entry { key: Key::ArmedAtMs, value: self.offset(d.at) },
            Entry { key: Key::RtoMs, value: coarsen_ms(self.rto.rto_ms()) },
        ] {
            rec.event(Event::Counter { site, op: id.op(), entry });
        }
    }

    /// An END of an op on the wire (a parked one is not on the wire: its answer was recorded when it came).
    fn record_end(&self, w: &Waiting, d: &Deadline, how: End) {
        use instrument::{Dir, Event, Outcome, Probe};
        let Some(rec) = self.rec.as_ref().filter(|_| d.seq >= self.rec_from) else { return };
        if !d.sent {
            return;
        }
        let Some(id) = self.send_label(d.seq) else { return };
        let site = op_site(w);
        rec.event(match how {
            End::Answered => Event::Edge { site, dir: Dir::Response, id },
            End::TimedOut => Event::Exit { site, op: id.op(), outcome: Outcome::Timeout },
            End::Withdrawn => Event::Exit { site, op: id.op(), outcome: Outcome::Withdrawn },
        });
    }

    /// The wait after the `attempt`-th send of one op: RTO x 2^(attempt-1),
    /// capped at the RTO's ceiling. THE one backoff of a sent op, silent or
    /// answered NotFound.
    fn backoff(&self, attempt: u32) -> u64 {
        (self.rto.rto_ms() << (attempt - 1).min(16)).min(rto::RTO_MAX_MS as u64)
    }

    /// GETs on the wire (parked ones are not).
    fn gets_in_flight(&self) -> usize {
        self.deadlines.iter().filter(|(k, d)| matches!(k, Waiting::Get(_)) && d.sent).count()
    }

    /// A GET's answer: [`Page::answered`], and which attempt it answered.
    /// A LOST GET (timed out, its re-send queued for a place: sdk#345) is
    /// still asked: a late answer to an earlier send answers it, and its
    /// queued re-send is dropped. It held no place and is no RTO sample.
    fn answered_get(&mut self, id: Cid) -> Option<u32> {
        if let Some(d) = self.deadlines.get(&Waiting::Get(id)) {
            // On the wire: `answered` takes the Karn sample and opens the
            // window, which a queued GET must not do.
            let attempt = d.attempt;
            self.answered(&Waiting::Get(id))?;
            self.drop_get(id);
            return Some(attempt);
        }
        let attempt = self.get_queue.contains(&id).then(|| self.attempt_of.get(&Waiting::Get(id)).copied()).flatten()?;
        self.drop_get(id);
        Some(attempt)
    }

    /// A GET ENDS: THE one definition of that. It leaves every holder a GET
    /// has -- its deadline (on the wire or parked), the attempt its re-send
    /// would continue from, and its place in the window's queue. An answer,
    /// a withdrawal and a block the page already holds all end a GET here.
    fn drop_get(&mut self, id: Cid) {
        let ended = self.end(&Waiting::Get(id), End::Withdrawn);
        self.attempt_of.remove(&Waiting::Get(id));
        self.get_queue.retain(|q| *q != id);
        // A GET ended while ON THE WIRE held a place in the window: the next
        // queued GET takes it. Only an ANSWER grows the window (`answered`);
        // an end frees the place it held. Without this a read whose silent
        // block was rebuilt from its group ended that GET and left the GETs
        // queued behind it stranded, with nothing in flight to free a place
        // (sdk#321: at m = 8 a race queues past the window).
        if ended.is_some_and(|d| d.sent) {
            self.fill_gets();
        }
    }

    /// The node ANSWERED a GET without the block (NotFound, or bytes that are
    /// not its id): the GET stays pending on its own deadline, sent again at
    /// the same backoff a silent one would be (rule 7: retry until answered),
    /// never at the speed of the answers.
    fn park_get(&mut self, id: Cid, attempt: u32) {
        self.park(Waiting::Get(id), Op::Get { id }, attempt);
    }

    /// The node ANSWERED `w`, but not with what it waits for (a GET without
    /// its block; a head below the published-head floor, sdk#349): it stays
    /// pending on its own deadline, sent again at the one backoff. PARKED --
    /// not on the wire, so coming due is no timeout: the RTO does not back
    /// off and the window does not halve, because the node IS answering.
    fn park(&mut self, w: Waiting, op: Op, attempt: u32) {
        let at = self.now + self.backoff(attempt);
        self.attempt_of.insert(w.clone(), attempt);
        if let Some(old) = self.deadlines.insert(w.clone(), Deadline { at, armed: at, op, sent_at: self.now, attempt, sent: false, resent: false, seq: 0, alone: false }) {
            self.record_end(&w, &old, End::Withdrawn);
        }
    }

    /// An ANSWER for `w`: its deadline ends; the answer to a first send into an EMPTY queue is an RTT sample (Karn: a
    /// re-sent call never is; sdk#390: a queued one also times its wait), and a queued first send's answer ends the
    /// back-off without sampling; every first send still on the wire is re-armed; and a GET opens the window.
    /// `None`: nothing was waiting on it.
    fn answered(&mut self, w: &Waiting) -> Option<Op> {
        let d = self.end(w, End::Answered)?;
        self.attempt_of.remove(w);
        self.first_of.remove(w);
        // A QUEUED first send answered: the path answered a call sent once, so the back-off ends (RFC 6298 §5.7) --
        // but its time is its wait in the queue too, so it is no sample (the architect's amendment to sdk#390's (a):
        // under load no op is sent alone, and without this nothing would ever clear the back-off).
        if d.sent && d.attempt == 1 && !d.resent && !d.alone {
            self.rto.answered_queued();
        }
        if d.sent && d.attempt == 1 && !d.resent && d.alone {
            let r = self.now.saturating_sub(d.sent_at);
            // IMPOSSIBLE, so loud: a round trip longer than the page has existed was dated against another clock's
            // origin -- the defect that pinned the RTO at its ceiling for a page's life (sdk#397).
            debug_assert!(r <= self.now.saturating_sub(self.born), "an RTT sample of {r} ms on a page {} ms old: a request was dated against another clock's origin", self.now.saturating_sub(self.born));
            self.rto.sample(r);
            // The sample AS COMPUTED, before anything clamps it (the architect): a wrong clock shows here.
            if let Some(rec) = self.rec.as_ref().filter(|_| d.seq >= self.rec_from) {
                use instrument::{vocab::coarsen_ms, Entry, Event, Key, Probe};
                // Past the last label nothing is recorded -- and nothing else changes: the recording never steers.
                if let Some(id) = self.send_label(d.seq) {
                    let (site, op) = (op_site(w), id.op());
                    rec.event(Event::Counter { site, op, entry: Entry { key: Key::SampleMs, value: coarsen_ms(r) } });
                    rec.event(Event::Counter { site, op, entry: Entry { key: Key::RtoMs, value: coarsen_ms(self.rto.rto_ms()) } });
                }
            }
        }
        // ANY answer re-arms (sdk#390 (b)): the queue is one shorter and the path is alive; the `min` never extends.
        if d.sent {
            self.rearm_first_sends();
        }
        if d.sent && matches!(w, Waiting::Get(_)) {
            self.window.opened();
            self.fill_gets();
        }
        Some(d.op)
    }

    /// EVERY first-send sample restarts the timer of every op still on its
    /// FIRST send from THIS answer, never later than the deadline it was sent
    /// with: `min(armed, now + rto)` (sdk#378; RFC 6298 §5.3, the architect).
    ///
    /// A deadline fixed at send let an op sent while a cold phase had backed
    /// the shared RTO to its 60 s ceiling wait out the full minute though its
    /// siblings kept answering (measured: V's first save, one lost PUT re-sent
    /// at +59,999 ms, 12 siblings answered on first send within 2.1 s). Timed
    /// from the SEND instead (`sent_at + rto`), the first -- fastest -- answer
    /// of a batch the node serialises (F61, ~37 ms an op) would declare every
    /// sibling queued past a few places lost while still being answered:
    /// duplicate PUTs into the same queue. From the ANSWER, nothing is lost
    /// while its siblings keep answering within one RTO of each other; a
    /// re-arm may move a deadline later than a previous one, never past
    /// `armed`. A RE-SEND keeps its backoff, and so does a reconnect re-send.
    ///
    /// sdk#390: timed at the op's PLACE in the node's one queue now -- an RTO for each op still ahead of it, and one
    /// for itself (`queued_wait`) -- in one pass over the ops on the wire in send order: the place is counted from the
    /// deadlines, never kept.
    fn rearm_first_sends(&mut self) {
        use instrument::{Entry, Event, Key, Kind, Probe};
        let now = self.now;
        let rto = self.rto.rto_ms();
        let (rec, start, from) = (&self.rec, self.rec_start, self.rec_from);
        let mut wire: Vec<(&Waiting, &mut Deadline)> = self.deadlines.iter_mut().filter(|(_, d)| d.sent).collect();
        wire.sort_unstable_by_key(|(_, d)| d.seq);
        for (ahead, (w, d)) in wire.into_iter().enumerate() {
            if d.attempt == 1 && !d.resent {
                let at = d.armed.min(now + queued_wait(rto, ahead as u64));
                if at != d.at && d.seq >= from {
                    if let Some(rec) = rec {
                        // Past the last label nothing is recorded; the re-arm below happens regardless.
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

    /// GETs waiting on the window take the places that are free.
    fn fill_gets(&mut self) {
        while let Some(id) = self.get_queue.front().copied() {
            if self.gets_in_flight() >= self.window.size() {
                break;
            }
            self.get_queue.pop_front();
            self.send(Waiting::Get(id), Op::Get { id });
        }
    }

    /// The register as this page last READ it (any head answer), whole —
    /// what a merge reads a winner's `prev` from (sdk#225b).
    pub fn last_read(&self) -> Option<&HeadRead> {
        self.last_head.as_ref()
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
        let sign = self.head.sign_again.into_iter().chain(self.sites.values().filter_map(|p| p.sign_again)).min();
        let held = self.held_again.values().map(|(at, _)| *at);
        let verify = self.verify.as_ref().and_then(|v| v.again_at);
        let puts = (!self.put_again.is_empty()).then_some(self.now);
        let backstop = self.engine_has_head.then_some(self.last_head_at + HEAD_BACKSTOP_MS);
        deadlines.chain(sign).chain(held).chain(verify).chain(puts).chain(backstop).min().map(Ms)
    }

    /// The page's clock: the last time it was told.
    pub fn now(&self) -> Ms {
        Ms(self.now)
    }

    /// Ops on the wire now that the recording saw go out (sent since it was attached, unanswered, not parked): what
    /// its open Requests must equal.
    #[doc(hidden)]
    pub fn ops_on_wire(&self) -> usize {
        self.deadlines.values().filter(|d| d.sent && d.seq >= self.rec_from).count()
    }

    /// Attach the page's recording: `capacity` events, then it DROPS and counts (never grows, never panics). Its
    /// offsets count from now. Ops sent before this are not in it.
    pub fn record_into(&mut self, capacity: usize) {
        self.rec = Some(obs::Rec::new(capacity));
        self.rec_start = self.now;
        self.obs_start = self.now;
        self.rec_from = self.sends.wrapping_add(1);
    }

    /// Attach the page's recording with the LOADER's segment as its opening (sdk#386's instrument work): the fetch
    /// rounds that loaded the SDK, then this page's ops, in one recording with ONE origin -- the loader's start (the
    /// same `Date.now()` clock), so both segments' offsets count from one zero. Every loader event is validated
    /// against closed code lists and re-coarsened ([`loader::decode`]); anything unknown is refused and counted.
    pub fn record_into_after(&mut self, capacity: usize, seg: &loader::Segment) {
        use instrument::Probe;
        self.record_into(capacity);
        self.rec_start = seg.start_ms;
        // The loader's rounds are in the page's first window.
        self.obs_start = seg.start_ms;
        if let Some(rec) = &self.rec {
            for e in loader::events(seg) {
                rec.event(e);
            }
        }
    }

    /// The recording, for a test or a local dump: a SEPARATE read handle -- the page itself only ever writes.
    pub fn recording(&self) -> Option<instrument::Recording<'_>> {
        self.rec.as_ref().map(|r| r.recording())
    }

    /// CLOSE THE OBSERVATION WINDOW (sdk#399, craftworks-docs OBSERVABILITY §1-§2): this window's detail record for app
    /// `site` at `minute`, as `(key, value)` for the observation tree -- the key [`obs::detail_key`], the value the ONE
    /// publish filter's output over THIS window's events alone (the window recorder is taken and a fresh one takes its
    /// place: the drain). `header` is the build header, for the page's first window. `None` with no recording attached.
    /// The next window starts now.
    pub fn close_obs_window(&mut self, site: &[u8; 32], minute: u64, header: Option<instrument::publish::Header>) -> Option<(Vec<u8>, Vec<u8>)> {
        let rec = self.rec.as_ref()?;
        let taken = rec.take_window();
        let window = instrument::publish::Window { minute, start_ms: self.obs_start.saturating_sub(self.rec_start), dropped_at_start: 0, lost_before: 0 };
        let record = instrument::publish::publish(&taken.recording(), window, header);
        self.obs_start = self.now;
        Some((obs::detail_key(site, minute), record.encode()))
    }

    /// The tail of the recording, rendered in the instrument's vocabulary (no user content can be in it: the
    /// events carry sites, labels by send order, and numbers from the reviewed keys). Read locally, never published.
    pub fn dump(&self, last: usize) -> String {
        match &self.rec {
            Some(r) => instrument::dump::render(&r.recording(), "page ops", last),
            None => "no page recording attached\n".into(),
        }
    }

    /// The retry clock now: `(RTO ms, SRTT ms, GET window)`.
    pub fn clock(&self) -> (u64, Option<f64>, usize) {
        (self.rto.rto_ms(), self.rto.srtt_ms(), self.window.size())
    }

    /// Step the engine and carry out what it decided.
    /// The ledger a head this page signs carries (COMMIT-LIFE ⁵): its PREV,
    /// and every page's `through` copied forward from the prev head -- so a
    /// head built on another page's commit still WITNESSES that it landed --
    /// with this page's own entry set to the last arrival its commit carries.
    pub fn sign_ledger_of(&self, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid) -> Vec<u8> {
        use signer_proto::head::{value, Ledger, Through};
        let prev = (prev_seq > 0).then_some(signer_proto::Head { seq: prev_seq, root: prev_root });
        let mut through: Vec<Through> =
            self.last_read().filter(|r| (r.seq, r.root()) == (prev_seq, prev_root)).map(|r| r.through()).unwrap_or_default();
        if self.device != [0; 16] {
            if let Some(t) = self.engine.committing_through() {
                through.retain(|e| e.device != self.device);
                through.push(Through { device: self.device, seq: t, last: seq });
            }
        }
        value(&root, &Ledger { prev, through, parity: Some(mark(&self.engine.root_parity_of(&root))) })[32..].to_vec()
    }

    /// This page's device id in heads' `through` (COMMIT-LIFE ⁵). Zeros:
    /// none yet -- its heads carry no entry, and no witness is read. It MUST
    /// be unique per page LOAD (per engine): arrival numbers start again with
    /// every engine, so two engines under one id would read each other's
    /// `through` as their own and tell a write that did not land `Published`.
    /// The Server sets it from the page's first session, which a page load
    /// mints at random (one page, one session).
    pub fn set_device(&mut self, device: [u8; 16]) {
        if self.device == [0; 16] {
            self.device = device;
        }
    }

    /// THE ONLY WAY a head read from the register is adopted (sdk#396): from a [`Heard`], which only the one
    /// judgement makes -- so no answer path can adopt a head this page's own record beats.
    fn adopt(&mut self, heard: Heard, how: Adopt) {
        let (seq, root) = heard.pair();
        match how {
            Adopt::Recovery => self.step(Event::HeadRead { epoch: EPOCH, seq, root }),
            Adopt::Conflict => self.step(Event::HeadConflict { seq, root }),
        }
    }

    fn step(&mut self, ev: Event) {
        // THE WITNESS (COMMIT-LIFE ⁵): a head about to be adopted says, in its
        // ledger, how far THIS page's writes are in it -- whether a commit
        // the engine is about to call dead in fact landed unheard.
        if let Event::HeadConflict { seq, root } | Event::HeadRead { seq, root, .. } = &ev {
            // No read of that head (it came some other way): nothing is
            // witnessed, which is the old rule -- not landed.
            let witness = (self.device != [0; 16])
                .then(|| self.last_read().filter(|r| (r.seq, r.root()) == (*seq, *root)).map(|r| r.witness_of(&self.device, self.engine.committing_seq())))
                .flatten();
            self.engine.set_witness(witness);
            // The §P MARK of the head about to be adopted: the engine's parity
            // scan is Done for a marked head, NotScanned for an unmarked one,
            // and the root's parity the mark lists makes it a group of one.
            let mark = self.last_read().filter(|r| (r.seq, r.root()) == (*seq, *root)).and_then(HeadRead::mark);
            self.engine.set_head_mark(mark);
        }
        // A SAME-SEQ DISPLACEMENT of this page's head: the Server may merge
        // the displaced group, so no cut is made until it has placed it (or
        // decided not to) -- set BEFORE the step that re-derives the queue.
        if let Event::HeadConflict { seq, root } = &ev {
            if self.hold_on_displace && (*seq, *root) != self.published() && *seq == self.published().0 {
                self.engine.hold_cut();
            }
        }
        // The engine has a head once it is TOLD one, whoever tells it: the
        // held writes go the moment it is, onto the tree it now stands on.
        let recovery = matches!(ev, Event::HeadRead { .. } | Event::HeadMissing);
        let fx = self.engine.step(ev);
        self.carry_out(fx);
        self.drop_dead_head();
        if recovery && !self.engine_has_head {
            self.engine_has_head = true;
            self.last_head_at = self.now;
        }
    }

    /// Supersede the engine's read `req` if it has made no progress
    /// ([`engine::Engine::supersede_read`]); the GETs only it needed end with
    /// it. Whether it was.
    pub fn supersede_read(&mut self, req: engine::read::ReqId) -> bool {
        let done = self.engine.supersede_read(req);
        if done {
            self.end_unneeded_gets();
        }
        done
    }

    /// A GET nobody needs ENDS at once -- its deadline, its re-ask backoff and the engine's entry go together --
    /// rather than being re-sent at its next timeout for ever (sdk#303):
    /// * one the engine WITHDREW: a raced group block no read needs once its group resolved;
    /// * one for a block the page HOLDS: a member rebuilt from its group, which the node never answered.
    ///
    /// A GET still in `deadlines` would also keep `waiting()` true and count as "not answering". An answer that
    /// comes later answers no GET and is ignored, like any answer to a wait that has ended.
    fn end_unneeded_gets(&mut self) {
        let mut ended = self.engine.take_all_withdrawn();
        ended.extend(
            self.deadlines
                .keys()
                .filter_map(|w| match w {
                    Waiting::Get(id) => Some(*id),
                    _ => None,
                })
                .chain(self.get_queue.iter().copied())
                .filter(|id| self.blocks.get(id).is_some()),
        );
        for id in ended {
            self.drop_get(id);
        }
    }

    /// What the engine publishes now, as a head.
    fn engine_published(&self) -> (u64, Cid) {
        (self.engine.published_seq(), self.engine.published_root())
    }

    /// An owed head is LIVE only while it is ahead of what the engine has
    /// published. The ONE consequence of the engine's decisions on the page's
    /// owed head (COMMIT-LIFE ⁹): once the engine publishes its seq (it was
    /// CONFIRMED -- the engine's `pending` take decided that, not the page), it
    /// is owed no more, and its sign, UPDATE and read-back waits end here. Once
    /// the engine adopts another head (a conflict, a recovery
    /// read), the commit that owed it is dead — its writes were told `Lost` —
    /// and nothing more is asked or sent for it. Without this the page went on
    /// asking the signer for a dead commit from a stale prev (the model found
    /// it: `NotSuccessor`, hidden behind the retries that got round it).
    fn drop_dead_head(&mut self) {
        // Also dead: a head whose commit was built on a root the engine no
        // longer publishes (a foreign winner adopted under it).
        let published = self.engine_published();
        if self.head.owed.as_ref().is_some_and(|o| o.seq <= published.0 || (o.seq - 1, o.base) != published) {
            self.head.owed = None;
            self.head.sign_again = None;
            self.head.sign_refusals = 0;
            self.head.sign_id = None;
            for w in [Waiting::Sign(Label::Head), Waiting::Update(Label::Head), Waiting::ReadBack(Label::Head)] {
                self.end(&w, End::Withdrawn);
            }
        }
    }

    fn carry_out(&mut self, fx: Vec<Effect>) {
        for f in fx {
            match f {
                // A VIEW makes no commit op: nothing of it is held, sent or
                // waited on. The door refuses a view's writes first, so this
                // is loud -- a commit on a view is a defect, named.
                Effect::PutBlock { .. } | Effect::UpdateHead { .. } | Effect::PutPack { .. } if self.read_only => {
                    let what = match f {
                        Effect::PutBlock { .. } => "block PUT",
                        Effect::PutPack { .. } => "pack PUT",
                        _ => "head update",
                    };
                    self.unusable.push(format!("read-only: a commit's {what} was not made (a view writes nothing but a repair)"));
                }
                Effect::PutBlock { id, ref bytes, ref after } => {
                    // The page is the memory now: the engine keeps no bytes.
                    self.blocks.insert(id, bytes);
                    let after: BTreeSet<Cid> = after.iter().copied().collect();
                    self.held.push((after, f.clone()));
                }
                Effect::UpdateHead { ref after, .. } => {
                    let after: BTreeSet<Cid> = after.iter().copied().collect();
                    self.held.push((after, f.clone()));
                }
                // A queued write's warm-apply block (R-b): kept for reads of
                // the warm root, never put -- its commit puts the same bytes.
                Effect::Keep { id, ref bytes } => self.blocks.insert(id, bytes),
                // SUPERSEDED (COMMIT-LIFE §P): a later root move re-coded the
                // group this block was in. Its PUT is WITHDRAWN -- no more
                // re-sends, not sent at all if still held back -- because
                // nobody needs it; that is not a cut-off of one somebody does.
                Effect::Withdraw { id } => {
                    self.end(&Waiting::Put(id), End::Withdrawn);
                    self.attempt_of.remove(&Waiting::Put(id));
                    self.end(&Waiting::Held(id), End::Withdrawn);
                    self.attempt_of.remove(&Waiting::Held(id));
                    self.held_again.remove(&id);
                    self.put_again.remove(&id);
                    self.held.retain(|(_, f)| !matches!(f, Effect::PutBlock { id: x, .. } if *x == id));
                }
                // A FOREIGN member of a changed group (safety gap class 2): the node answers for it before BACKED_UP.
                // Known here already (a PUT answered, a Held answered): told at once, no op. Else asked, once.
                Effect::ConfirmHeld { id } => {
                    if self.confirmed.contains(&id) {
                        let more = self.engine.step(Event::PutConfirmed(id));
                        self.carry_out(more);
                    } else {
                        if !self.deadlines.contains_key(&Waiting::Held(id)) && !self.held_again.contains_key(&id) {
                            self.send(Waiting::Held(id), Op::AskHeld { id });
                        }
                        // Not known here: the engine counts it absent and sends more of its group's parity (sdk#416).
                        let more = self.engine.step(Event::HeldUnknown(id));
                        self.carry_out(more);
                    }
                }
                // A block rebuilt from its group goes back to the network by the commit's own PUT (send: the same
                // op, deadline and re-send), unless it is on its way or there already.
                Effect::PutRepaired { id, ref bytes } => {
                    self.blocks.insert(id, bytes);
                    if !self.confirmed.contains(&id) && !self.deadlines.contains_key(&Waiting::Put(id)) && !self.put_again.contains_key(&id) {
                        self.repair_puts.insert(id);
                        self.send(Waiting::Put(id), Op::Put { id, bytes: bytes.clone() });
                    }
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
                    } else if self.deadlines.contains_key(&Waiting::Get(id)) || self.get_queue.contains(&id) {
                        // ONE GET per block while one is out: its answer
                        // serves every reader, and its re-send is the RTO's
                        // (with its backoff). A second send here would reset
                        // that clock -- the engine re-asks on every tick and
                        // re-descent, so a silent block was re-sent on every
                        // one of them (2,001 GETs in 5 min, measured).
                    } else {
                        self.send(Waiting::Get(id), Op::Get { id });
                    }
                }
                Effect::ReadHead { .. } => self.send(Waiting::RecoverHead, Op::ReadHead { label: Label::Head }),
                client => self.client_fx.push(client),
            }
        }
        self.release();
        // Every engine step's effects come through here, so no GET the engine stopped needing outlives the call
        // that stopped needing it (sdk#303).
        self.end_unneeded_gets();
    }

    /// Effects whose `after` set is now confirmed go out, in emitted order.
    fn release(&mut self) {
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].0.iter().all(|c| self.confirmed.contains(c)) {
                let (_, f) = self.held.remove(i);
                match f {
                    Effect::PutBlock { id, bytes, .. } => {
                        if self.confirmed.contains(&id) {
                            // Already on the node: the engine hears it again.
                            let more = self.engine.step(Event::PutConfirmed(id));
                            self.carry_out(more);
                        } else {
                            // A commit's own now: its answer is the commit's.
                            self.repair_puts.remove(&id);
                            self.send(Waiting::Put(id), Op::Put { id, bytes });
                        }
                    }
                    // A head held across an adopt belongs to a DEAD commit
                    // (its writes were told `Lost`): it is at or behind what
                    // the engine now publishes, and asking for it would name a
                    // prev it does not follow (the model: `NotSuccessor`).
                    Effect::UpdateHead { seq, .. } if seq <= self.engine.published_seq() => {}
                    // A head whose commit was built on a root the engine no
                    // longer publishes is DEAD: a foreign winner was adopted
                    // under it. The engine re-derives the commit on the new
                    // root and emits a new head.
                    Effect::UpdateHead { seq, base, .. } if (seq - 1, base) != self.engine_published() => {}
                    Effect::UpdateHead { seq, root, base, .. } => {
                        // The owed head this one replaces is dead (the engine published it, or adopted over it):
                        // its waits end with it, before this head's own go out under the same labels.
                        self.drop_dead_head();
                        self.head.owed = Some(Owed { seq, root, base, record: None, stale_reads: 0 });
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

    /// How many parity groups the engine owes right now (what a Tick past
    /// `parity_age`, or a Flush, is for).
    /// Writes the engine took forced past their reads (`Expect::Any`,
    /// sdk#235): shown to a person, so the transitional form is a number
    /// someone can act on.
    pub fn forced_writes(&self) -> u64 {
        self.engine.forced_writes()
    }

    /// The engine's asks so far, `(wanted, raced)` (sdk#303): a read's cap counts the wanted.
    pub fn fetch_counts(&self) -> (usize, usize) {
        self.engine.fetch_counts()
    }

    /// Read repairs through parity: `(started, rebuilt, given up)`.
    pub fn repair_counts(&self) -> (u64, u64, u64) {
        self.engine.repair_counts()
    }

    /// Why the last read repair was given up, if one was.
    pub fn repair_failed(&self) -> Option<&str> {
        self.engine.repair_failed()
    }

    pub fn owed_groups(&self) -> usize {
        self.engine.owed_groups()
    }

    /// GETs the engine withdrew that this page has not ended yet (sdk#303): 0 after every step.
    pub fn withdrawn(&self) -> usize {
        self.engine.withdrawn_count()
    }

    /// Is anything still owed an answer or a re-send — an op in flight, a
    /// backed-off retry? `false` means this page is at rest until something
    /// new arrives.
    pub fn waiting(&self) -> bool {
        !self.deadlines.is_empty()
            || !self.put_again.is_empty()
            || self.head.sign_again.is_some()
            || self.sites.values().any(|p| p.sign_again.is_some())
            || !self.held_again.is_empty()
    }

    /// Ops to send, in order.
    /// PUT a contract the app names, by key: sent now, re-sent on the RTO
    /// until the node answers or a person cancels ([`Page::cancel_app_put`]).
    /// Asking again for a key that ENDED starts it over; one still pending is
    /// left alone.
    pub fn put_app(&mut self, key: String, now: Ms) {
        self.now = now.0;
        if matches!(self.app_puts.get(&key), Some((AppPut::Pending, _))) {
            return;
        }
        self.app_puts.insert(key.clone(), (AppPut::Pending, self.now));
        self.send(Waiting::PutApp(key.clone()), Op::PutApp { key });
    }

    /// Where the app's PUT of `key` stands; `None` if it was never asked.
    pub fn app_put(&self, key: &str) -> Option<&AppPut> {
        self.app_puts.get(key).map(|(p, _)| p)
    }

    /// A PERSON cancels a pending app PUT — the one end that is not the
    /// node's answer (rule 8), and it is named. Nothing more is sent for it.
    pub fn cancel_app_put(&mut self, key: &str) {
        if matches!(self.app_puts.get(key), Some((AppPut::Pending, _))) {
            let w = Waiting::PutApp(key.to_string());
            self.end(&w, End::Withdrawn);
            self.attempt_of.remove(&w);
            self.first_of.remove(&w);
            if let Some(p) = self.app_puts.get_mut(key) {
                p.0 = AppPut::Cancelled;
            }
        }
    }

    /// Send a page-io request ([`Ext`]) through this page's sender: its
    /// deadline, RTO and re-send are the page's, like every op's (rule 5).
    pub fn send_ext(&mut self, e: Ext, now: Ms) {
        self.now = now.0;
        self.send(Waiting::Ext(e), Op::Ext(e));
    }

    /// Its answer arrived: the deadline ends (and an attempt-1 answer is an
    /// RTO sample, as for every op).
    pub fn ext_answered(&mut self, e: Ext, now: Ms) {
        self.now = now.0;
        self.answered(&Waiting::Ext(e));
    }

    /// Is this request still waiting on an answer?
    pub fn ext_waiting(&self, e: Ext) -> bool {
        self.deadlines.contains_key(&Waiting::Ext(e))
    }

    /// NOT ANSWERING FOR N s: the request that has waited longest for an
    /// answer, and for how long (ms) — what a page shows while the node is
    /// slow ("not answering for N s", rule 8). Never an end: the request is
    /// still being re-sent. `None` when nothing waits.
    pub fn not_answering(&self) -> Option<(String, u64)> {
        self.first_of
            .iter()
            .filter(|(w, _)| self.deadlines.contains_key(*w))
            .min_by_key(|(_, at)| **at)
            .map(|(w, at)| (waiting_name(w), self.now.saturating_sub(*at)))
    }

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

    /// Things this page could not do, by reason.
    /// Make this page a VIEW ([`Page::read_only`]): for good, set once when
    /// the page is made a reader of somebody's head.
    pub fn set_read_only(&mut self) {
        self.read_only = true;
    }

    /// THE PUBLISHED-HEAD FLOOR (sdk#349): no head below `seq` is adopted.
    /// Set before the first head read; 0 is none.
    pub fn set_head_floor(&mut self, seq: u64) {
        self.head_floor = seq;
    }

    /// What this page WAITS on because of its floor: `(floor, last seq the
    /// node answered, how many answers were below it)` while the adopted head
    /// is below the floor; `None` once a head at or above it is adopted, or
    /// with no floor.
    pub fn head_floor_wait(&self) -> Option<(u64, Option<u64>, u32)> {
        if self.head_floor == 0 || self.published_seq_at_least(self.head_floor) {
            return None;
        }
        self.below_floor.map(|(seen, n)| (self.head_floor, seen, n)).or(Some((self.head_floor, None, 0)))
    }

    fn published_seq_at_least(&self, seq: u64) -> bool {
        self.recovered && self.engine.published_seq() >= seq
    }

    /// Is this page a VIEW: it makes no commit op, only repair PUTs.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

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

    /// Is this page's PUT of `id` on the wire, or waiting to be sent again (sdk#433: what a node's text-named
    /// refusal may be attributed to)?
    pub fn put_waiting(&self, id: &Cid) -> bool {
        self.deadlines.contains_key(&Waiting::Put(*id)) || self.put_again.contains_key(id)
    }

    /// Repair PUTs the node's Block contract rejected (sdk#433), dropped.
    pub fn repairs_rejected(&self) -> u64 {
        self.repairs_rejected
    }

    /// Blocks the node's Block contract rejected (sdk#433): damaged, for the assets dashboard.
    pub fn rejected_blocks(&self) -> &std::collections::BTreeSet<Cid> {
        self.engine.rejected_blocks()
    }

    /// Landings of a signer's record this page started, and the most UPDATEs
    /// one of them needed (a lost UPDATE is re-sent at its deadline).
    pub fn landings(&self) -> (u32, u32) {
        (self.landings, self.most_landing_updates)
    }


    /// Every record the signer returned (invariant 2's evidence).
    pub fn signer_records(&self) -> &BTreeSet<Vec<u8>> {
        &self.signer_records
    }

    /// Every write in the engine's queue with its stage (R-b).
    pub fn queue_stages(&self) -> Vec<(engine::ClientId, engine::WriteId, engine::Stage)> {
        self.engine.queue_stages().collect()
    }

    /// Does a write still applying write a key in `[lo, hi)`?
    pub fn applying_touches(&self, lo: &[u8], hi: &[u8]) -> bool {
        self.engine.applying_touches(lo, hi)
    }

    /// The stage of the last queued write that writes `key`.
    pub fn stage_of_key(&self, key: &[u8]) -> Option<engine::Stage> {
        self.engine.stage_of_key(key)
    }

    /// Writes queued and their bytes (what `QueueFull` measures; a held
    /// deferred write takes room like any other).
    pub fn queue_load(&self) -> (usize, usize) {
        self.engine.queue_load()
    }

    /// WHAT IS UNSAVED (sdk#350), its one owner the engine: writes taken
    /// and not published, a held DEFERRED write not among them. What a
    /// session's "unsaved changes" is derived from, never a tally of its own.
    pub fn unsaved_writes(&self) -> usize {
        self.engine.unsaved_writes()
    }

    /// The client of every unsaved write (the engine's rule, per write).
    pub fn unsaved_clients(&self) -> Vec<engine::ClientId> {
        self.engine.unsaved_clients().collect()
    }

    /// Own commits published and the queued writes they carried (K9: writes
    /// per commit, what group commit is measured by).
    pub fn commits_and_writes(&self) -> (u64, u64) {
        self.engine.commits_and_writes()
    }

    /// Writes told `Published` because a head's ledger witnessed their group
    /// landed unheard (COMMIT-LIFE ⁵).
    pub fn landed_by_witness(&self) -> u64 {
        self.engine.landed_by_witness()
    }

    /// Writes told `Published` by a no-op group: the tree already held them.
    pub fn noop_published(&self) -> u64 {
        self.engine.noop_published()
    }

    /// See `hold_on_displace`: the Server that merges turns it on.
    pub fn hold_on_displace(&mut self) {
        self.hold_on_displace = true;
    }

    pub fn cut_held(&self) -> bool {
        self.engine.cut_held()
    }

    /// The hold ends with nothing to merge.
    pub fn release_cut(&mut self) {
        let fx = self.engine.release_cut();
        self.carry_out(fx);
    }

    /// A merge's writes, at the FRONT of the queue (review §1 on sdk#295).
    pub fn merge_front(&mut self, client: ClientId, writes: Vec<engine::MergeWrite>) {
        let fx = self.engine.merge_front(client, writes);
        self.carry_out(fx);
    }

    /// The next write taken is a re-run that spent `tries` (ONE budget).
    pub fn carry_tries(&mut self, tries: u32) {
        self.engine.carry_tries(tries);
    }

    pub fn clear_carried_tries(&mut self) {
        self.engine.clear_carried_tries();
    }

    pub fn max_write_tries(&self) -> u32 {
        self.engine.max_write_tries()
    }

    /// The seq the commit in flight would land at (`None`: none in flight).
    pub fn committing_seq(&self) -> Option<u64> {
        self.engine.committing_seq()
    }

    /// The engine's own counts the model reads (R-b): K9 rebuilds that
    /// differed, re-derivations on own publish, impossible stage moves.
    pub fn queue_counts(&self) -> (u64, u64, u64) {
        (self.engine.rebuild_differs(), self.engine.own_publish_rederived(), self.engine.impossible_transitions())
    }

    /// The WARM root: this page's accepted writes applied, published or not
    /// (READ-STATE, design B). What an own-tree walk reads.
    pub fn warm_root(&self) -> Cid {
        self.engine.root()
    }

    /// Walk the tree at `root` over the blocks this page holds, now
    /// (`engine::Engine::walk`).
    pub fn walk(&self, root: &Cid, walk: &engine::read::Walk) -> engine::read::Walked {
        self.engine.walk(root, walk)
    }

    /// The blocks the page holds.
    pub fn blocks(&self) -> &PageBlocks {
        &self.blocks
    }
}

/// Is this the kind of answer a SIGN request gets? `Signed`, `AlreadySigned`,
/// `NotNext`, or a refusal the sign verb gives; not `Held`, `Provisioned`,
/// `Register`, or a refusal only Held or Provision gives.
fn answers_a_sign(a: &signer_proto::Answer) -> bool {
    use signer_proto::{Answer as A, Why};
    match a {
        A::Signed(_) | A::AlreadySigned(_) | A::NotNext { .. } => true,
        A::Refused(w) => !matches!(
            w,
            Why::BlockCount { .. } | Why::KeyAlreadyProvisioned | Why::RegisterChanged
        ),
        A::Provisioned | A::Held { .. } | A::Register { .. } => false,
    }
}

#[cfg(test)]
mod unneeded_gets {
    use super::*;

    /// A GET still QUEUED for the window when nobody needs it any more is ended with the rest (sdk#303): it never
    /// goes out. Tested here, on the page's own queue, because no fixture through the public surface leaves a
    /// block queued at the moment it stops being needed (the window refills before the engine hears the answer
    /// that ends the need; page/tests/race_get.rs).
    #[test]
    fn a_queued_get_nobody_needs_is_ended() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        let (held, wanted) = ([1u8; 32], [2u8; 32]);
        p.blocks.insert(held, b"held");
        p.get_queue.push_back(held);
        p.get_queue.push_back(wanted);
        p.end_unneeded_gets();
        assert_eq!(p.get_queue.iter().copied().collect::<Vec<_>>(), vec![wanted], "the held block's queued GET was not ended, or the wanted one was");
    }
}

#[cfg(test)]
mod repair_put {
    use super::*;

    /// A block rebuilt from its group is PUT through the one sender, and its answer is NOBODY's commit (the
    /// architect's (b) on sdk#331): it confirms nothing a commit or a head's `after` waits on. When a commit puts
    /// the same block, the answer is the commit's again.
    #[test]
    fn a_repair_puts_answer_confirms_nothing() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        let (id, bytes) = ([7u8; 32], b"rebuilt".to_vec());
        p.carry_out(vec![Effect::PutRepaired { id, bytes: bytes.clone() }]);
        let puts: Vec<Op> = p.take_ops().into_iter().filter(|o| matches!(o, Op::Put { .. })).collect();
        assert_eq!(puts, vec![Op::Put { id, bytes: bytes.clone() }], "the repair was not PUT through the sender");
        assert!(p.deadlines.contains_key(&Waiting::Put(id)), "the repair PUT has no deadline: it would not be re-sent");
        p.answer(Answer::PutOk(id), Ms(1));
        assert!(!p.confirmed.contains(&id), "a repair PUT's answer confirmed the block for the commits");
        assert!(!p.deadlines.contains_key(&Waiting::Put(id)), "the answered repair PUT is still waiting");

        // The same block, then put by a commit: its answer is the commit's.
        p.carry_out(vec![Effect::PutBlock { id, bytes: bytes.clone(), after: Vec::new() }]);
        p.answer(Answer::PutOk(id), Ms(2));
        assert!(p.confirmed.contains(&id), "a commit's PUT of a once-repaired block was not confirmed");
    }

    /// RACE PUT's condition (a) (COMMIT-LIFE §P, the architect): a repair
    /// re-PUT is NEVER in the race set a head waits on. The engine builds
    /// `UpdateHead::after` from what it was told is confirmed, and the page
    /// never tells it a repair's answer; here the page's side: a head whose
    /// `after` names a block that only a REPAIR has put stays held -- no
    /// signer request -- until a commit's own PUT of it is answered.
    #[test]
    fn a_head_is_never_released_by_a_repair_puts_answer() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        let _ = p.take_ops();
        let (id, bytes) = ([7u8; 32], b"rebuilt".to_vec());
        p.carry_out(vec![Effect::PutRepaired { id, bytes: bytes.clone() }]);
        p.answer(Answer::PutOk(id), Ms(1));
        let _ = p.take_ops();
        let (pseq, base) = p.engine_published();
        p.held.push((BTreeSet::from([id]), Effect::UpdateHead { seq: pseq + 1, root: [3u8; 32], base, after: vec![id] }));
        p.release();
        assert!(!p.take_ops().iter().any(|o| matches!(o, Op::Sign { .. })), "a head was released by a REPAIR PUT's answer");
        // The commit's own PUT of the block is answered: the head goes.
        p.carry_out(vec![Effect::PutBlock { id, bytes, after: Vec::new() }]);
        p.answer(Answer::PutOk(id), Ms(2));
        assert!(p.take_ops().iter().any(|o| matches!(o, Op::Sign { .. })), "the commit's own PUT answer did not release the head to be signed");
    }
}

#[cfg(test)]
mod confirm_held {
    use super::*;

    fn asks(p: &mut Page) -> usize {
        p.take_ops().iter().filter(|o| matches!(o, Op::AskHeld { .. })).count()
    }

    /// Safety gap class 2: `ConfirmHeld` is answered from what the NODE confirmed (`confirmed`, the one holder),
    /// never from what the page holds. A block this page's PUT had answered is told at once, with no op; a block in
    /// page memory only -- a READ rebuilt it from its group -- is asked of the node.
    #[test]
    fn a_block_is_confirmed_by_the_node_never_by_page_memory() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        let _ = p.take_ops();
        let (put, rebuilt) = ([7u8; 32], [8u8; 32]);
        p.confirmed.insert(put);
        p.carry_out(vec![Effect::ConfirmHeld { id: put }]);
        assert_eq!(asks(&mut p), 0, "a block the node confirmed was asked about again");
        p.blocks.insert(rebuilt, b"rebuilt from its group");
        p.carry_out(vec![Effect::ConfirmHeld { id: rebuilt }]);
        assert_eq!(asks(&mut p), 1, "a block only in page memory was taken as on the node");
        p.answer(Answer::Held { id: rebuilt, present: true }, Ms(1));
        assert!(p.confirmed.contains(&rebuilt), "the node's Held answer did not confirm it");
    }

    /// RULE 7 for a block the page has NO bytes for (a foreign member it only asks about): absent, it is asked
    /// again on the backoff for as long as it takes -- past HELD_ABSENTS, where a block with bytes would be PUT --
    /// and never dropped.
    #[test]
    fn a_foreign_block_absent_is_asked_again_for_as_long_as_it_takes() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        let _ = p.take_ops();
        let id = [9u8; 32];
        p.carry_out(vec![Effect::ConfirmHeld { id }]);
        assert_eq!(asks(&mut p), 1);
        let mut now = 0u64;
        for n in 0..(HELD_ABSENTS + 3) {
            now += 1;
            p.answer(Answer::Held { id, present: false }, Ms(now));
            now += rto::RTO_MAX_MS as u64 + 1;
            p.tick(Ms(now));
            assert_eq!(asks(&mut p), 1, "absent answer {}: the block was not asked again", n + 1);
        }
        assert!(p.put_again.is_empty(), "a block the page has no bytes for was queued to be PUT");
        p.answer(Answer::Held { id, present: true }, Ms(now + 1));
        assert!(p.confirmed.contains(&id));
    }
}

#[cfg(test)]
mod parked_get {
    use super::*;

    /// A GET the node ANSWERED without the block is PARKED on its own deadline, at the same per-op backoff as a
    /// silent one (one backoff, `send`'s): not re-sent at once, holding no window place, and not a timeout when
    /// it comes due. One the engine no longer needs ends there instead of going out again.
    #[test]
    fn a_get_answered_notfound_waits_on_its_own_deadline() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        // The page's OPENING head read answered first, through the real path: the GET goes into an EMPTY queue,
        // so its answer is a sample (sdk#390), and the tick below tests only the parked GET.
        p.now = 5;
        p.answered(&Waiting::RecoverHead);
        let id = [9u8; 32];
        p.send(Waiting::Get(id), Op::Get { id });
        assert!(p.take_ops().contains(&Op::Get { id }), "the GET did not go out");
        p.answer(Answer::GetMissed(id), Ms(10));
        // After the answer: an attempt-1 answer is an RTO sample, and it opens the window.
        let (rto_before, window_before) = (p.rto.rto_ms(), p.window.size());
        let (sent, due) = p.deadlines.get(&Waiting::Get(id)).map(|d| (d.sent, d.at)).expect("a NotFound GET is parked on its deadline, not dropped");
        assert!(!sent, "a parked GET counts as on the wire");
        assert_eq!(due, 10 + p.backoff(1), "parked at a backoff other than send()'s");
        assert_eq!(p.gets_in_flight(), 0, "a parked GET holds a window place");
        assert!(p.take_ops().iter().all(|o| !matches!(o, Op::Get { .. })), "a NotFound GET was re-sent at once");
        p.tick(Ms(due));
        assert_eq!(p.rto.rto_ms(), rto_before, "a parked GET coming due was counted as a timeout");
        assert_eq!(p.window.size(), window_before, "a parked GET coming due halved the window");
        // The engine here waits on nothing: the parked GET ends rather than going out.
        assert!(!p.deadlines.contains_key(&Waiting::Get(id)), "an unneeded parked GET was kept");
        assert!(p.take_ops().iter().all(|o| !matches!(o, Op::Get { .. })), "an unneeded parked GET was sent");
    }
}

#[cfg(test)]
mod clock_origin {
    use super::*;

    const EPOCH_MS: u64 = 1_790_253_181_367;

    /// sdk#397's guard: a round trip longer than the page has existed is IMPOSSIBLE, and says so. A page made at
    /// the browser's epoch clock answers a request that was dated 0 -- against another clock's origin, as page-io's
    /// copy of the clock once did -- and the sample trips the assert. Mutant "no assert" -> red.
    #[test]
    #[should_panic(expected = "dated against another clock's origin")]
    fn a_sample_longer_than_the_page_has_existed_is_refused_loudly() {
        // Unstarted: nothing else on the wire, so the request goes into an EMPTY queue and its answer is a sample.
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        p.send_ext(Ext::SignerFirst, Ms(0));
        p.ext_answered(Ext::SignerFirst, Ms(EPOCH_MS + 10));
    }

    /// THE CONTROL: the same request dated by the page's own clock is a 10 ms sample.
    #[test]
    fn a_request_dated_by_the_pages_clock_is_its_round_trip() {
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        p.send_ext(Ext::SignerFirst, Ms(EPOCH_MS + 100));
        p.ext_answered(Ext::SignerFirst, Ms(EPOCH_MS + 110));
        assert_eq!(p.rto.srtt_ms(), Some(10.0), "the sample was not the 10 ms round trip");
    }
}

#[cfg(test)]
mod recording {
    use super::*;
    use instrument::{Dir, Entry, Event, Key, Kind, Outcome, Record};

    const EPOCH_MS: u64 = 1_790_253_181_367;

    fn counters(r: &instrument::Recording<'_>, ordinal: u32, key: Key) -> Vec<u64> {
        let op = instrument::Label::new(Kind::Request, ordinal).expect("a small ordinal").op();
        r.events().into_iter().filter_map(|e| match e { Event::Counter { op: o, entry: Entry { key: k, value }, .. } if o == op && k == key => Some(value), _ => None }).collect()
    }

    /// WHAT THE PAGE'S RECORDING SAYS OF AN OP (sdk#386's instrument work), on a page at the browser's epoch clock
    /// with the recorder attached 5 s after it was made: a send is a Request labelled by send order, with its
    /// attempt, when it went and when it is due as OFFSETS from the attach (never the page's clock), and the RTO it
    /// used; unanswered, it is closed Timeout and its re-send is a NEW Request; a first-send answer is a Response
    /// and records its sample as computed and the RTO after it. The block's id is in none of it.
    #[test]
    fn a_send_its_timeout_its_resend_and_an_answer_are_recorded_by_send_order_and_offset() {
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        p.now = EPOCH_MS + 5_000;
        p.record_into(1024);
        let id = [7u8; 32];
        p.send(Waiting::Put(id), Op::Put { id, bytes: vec![1] });
        let r = p.recording().expect("attached");
        let reqs: Vec<u32> = r.events().into_iter().filter_map(|e| match e { Event::Edge { dir: Dir::Request, id, .. } => Some(id.ordinal()), _ => None }).collect();
        assert_eq!(reqs, vec![1], "the send is not req#1");
        assert_eq!(counters(&r, 1, Key::OffsetMs), vec![0], "the send's offset does not count from the attach");
        assert_eq!(counters(&r, 1, Key::ArmedAtMs), vec![1_000], "armed at the initial RTO after the attach");
        assert_eq!(counters(&r, 1, Key::RtoMs), vec![1_000]);
        assert_eq!(counters(&r, 1, Key::Attempts), vec![1]);
        // Unanswered: it times out and is sent again -- a new send.
        p.tick(Ms(EPOCH_MS + 6_000));
        let r = p.recording().expect("attached");
        assert!(r.events().contains(&Event::Exit { site: op_site(&Waiting::Put(id)), op: instrument::Label::new(Kind::Request, 1).expect("1").op(), outcome: Outcome::Timeout }), "the timeout was not recorded");
        assert_eq!(counters(&r, 2, Key::Attempts), vec![2], "the re-send is not req#2 on its second attempt");
        // An op answered on its first send, into an EMPTY queue (sdk#390: a queued answer is no sample): the PUT's
        // re-send is answered first, then its sample as computed, and the RTO after it.
        p.now = EPOCH_MS + 6_050;
        p.answered(&Waiting::Put(id));
        p.now = EPOCH_MS + 6_100;
        p.send_ext(Ext::SignerFirst, Ms(EPOCH_MS + 6_100));
        p.ext_answered(Ext::SignerFirst, Ms(EPOCH_MS + 6_140));
        let r = p.recording().expect("attached");
        assert_eq!(counters(&r, 3, Key::SampleMs), vec![40], "the sample is not the 40 ms round trip");
        assert_eq!(counters(&r, 3, Key::RtoMs).last(), Some(&coarse(p.rto.rto_ms())));
        assert!(r.events().contains(&Event::Edge { site: op_site(&Waiting::Ext(Ext::SignerFirst)), dir: Dir::Response, id: instrument::Label::new(Kind::Request, 3).expect("3") }));
        let dump = p.dump(40);
        assert!(!dump.contains("0707"), "the block id leaked into the dump:\n{dump}");
        println!("{dump}");
    }

    /// PER WINDOW (sdk#399, the architect's carry-over from instrument#18): the page's observation records are drained
    /// per window, so the second holds none of the first's operations -- an Exit carries no offset, so only the drain
    /// keeps an old one out. Window 1: a PUT times out (a failure: it leaves) and its re-send is left unanswered (a
    /// stall: it leaves). Window 2: that re-send is answered (Ok: stays local) and a second PUT times out. Window 2's
    /// record holds exactly ONE timeout, the second PUT's. Mutant "publish the whole recording" -> red.
    #[test]
    fn each_observation_window_holds_only_its_own_operations() {
        use instrument::publish::{PubEvent, Published};
        let site = [9u8; 32];
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        p.now = EPOCH_MS;
        p.record_into(1024);
        let (a, b) = ([7u8; 32], [8u8; 32]);
        p.send(Waiting::Put(a), Op::Put { id: a, bytes: vec![1] });
        p.tick(Ms(EPOCH_MS + 1_500));
        let (k1, v1) = p.close_obs_window(&site, 100, None).expect("a recording");
        p.answer(Answer::PutOk(a), Ms(EPOCH_MS + 2_000));
        p.now = EPOCH_MS + 2_000;
        p.send(Waiting::Put(b), Op::Put { id: b, bytes: vec![2] });
        p.tick(Ms(EPOCH_MS + 9_000));
        let (k2, v2) = p.close_obs_window(&site, 101, None).expect("a recording");
        assert_eq!((k1, k2), (obs::detail_key(&site, 100), obs::detail_key(&site, 101)));
        let exits = |v: &[u8]| -> Vec<Outcome> {
            Published::decode(v, &[]).expect("a record").events.iter().filter_map(|e| match e { PubEvent::Exit { outcome, .. } => Some(*outcome), _ => None }).collect()
        };
        let (w1, w2) = (exits(&v1), exits(&v2));
        println!("window 1 exits {w1:?}; window 2 exits {w2:?}");
        assert_eq!(w1, vec![Outcome::Timeout], "THE SETUP: window 1's record is not its one timeout");
        assert_eq!(w2.iter().filter(|o| **o == Outcome::Timeout).count(), 1, "window 2's record holds another window's operation, or lost its own: {w2:?}");
        assert!(!w2.contains(&Outcome::Ok), "an Ok operation left the device");
    }

    /// The observation detail key: `w/ ‖ site ‖ minute BE` -- one app's windows sort by time, and the site id is the
    /// only name of the app in it.
    #[test]
    fn the_detail_key_is_the_site_then_the_minute() {
        let k = obs::detail_key(&[3u8; 32], 0x0102);
        assert_eq!(&k[..2], b"w/");
        assert_eq!(&k[2..34], &[3u8; 32]);
        assert_eq!(&k[34..], &0x0102u64.to_be_bytes());
        assert!(obs::detail_key(&[3u8; 32], 1) < obs::detail_key(&[3u8; 32], 256), "windows do not sort by time");
    }

    fn coarse(ms: u64) -> u64 {
        instrument::vocab::coarsen_ms(ms)
    }

    /// PAST THE LAST LABEL THE RECORDING STILL NEVER STEERS: two pages at the label limit, one recording and one
    /// not, send and answer the same ops -- their deadlines, window and RTO end identical. An unlabellable send once
    /// made `answered` return before its re-arm and window step on the recording page only. Mutant "leave `answered`
    /// when no label can be made" -> red.
    #[test]
    fn past_the_last_label_recording_changes_nothing_the_page_does() {
        use instrument::label::MAX_ORDINAL;
        let run = |record: bool| {
            let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
            if record {
                p.record_into(64);
                p.rec_from = 0;
            }
            p.sends = MAX_ORDINAL + 5;
            for i in 1..=3u8 {
                p.send(Waiting::Get([i; 32]), Op::Get { id: [i; 32] });
            }
            p.now = EPOCH_MS + 40;
            p.answered(&Waiting::Get([1; 32]));
            let ats: Vec<u64> = p.deadlines.values().map(|d| d.at).collect();
            (ats, p.window.size(), p.rto.rto_ms())
        };
        assert_eq!(run(true), run(false), "past the last label, recording changed what the page did");
    }

    /// THE SEND ORDER OUTRUNS A LABEL (the architect on instrument v2): past `MAX_ORDINAL` the page stops labelling
    /// and says so ONCE -- it never wraps into another kind's range or reuses an early ordinal. The ops themselves
    /// still go out: recording is never an input. Mutant "wrap the send order" -> red.
    #[test]
    fn past_the_last_label_the_page_stops_labelling_and_says_so_once() {
        use instrument::label::MAX_ORDINAL;
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        p.record_into(64);
        p.sends = MAX_ORDINAL - 1;
        p.rec_from = 0;
        for i in 1..=4u8 {
            let id = [i; 32];
            p.send(Waiting::Put(id), Op::Put { id, bytes: vec![i] });
        }
        assert_eq!(p.take_ops().len(), 4, "a send the recording could not label did not go out");
        let r = p.recording().expect("attached");
        let reqs: Vec<u32> = r.events().into_iter().filter_map(|e| match e { Event::Edge { dir: Dir::Request, id, .. } => Some(id.ordinal()), _ => None }).collect();
        assert_eq!(reqs, vec![MAX_ORDINAL], "only the last labellable send is labelled: {reqs:?}");
        let overflow = r.events().into_iter().filter(|e| e.site().name() == "page::recording").count();
        assert_eq!(overflow, 1, "the overflow was not said exactly once");
        assert_eq!(p.sends, MAX_ORDINAL + 3);
        // At the END of u32: a wrapped order would come round to 0, 1 -- small ordinals a label carries -- and reuse
        // them. Saturated, nothing past here is ever labelled.
        p.sends = u32::MAX - 1;
        for i in 5..=7u8 {
            let id = [i; 32];
            p.send(Waiting::Put(id), Op::Put { id, bytes: vec![i] });
        }
        let r = p.recording().expect("attached");
        let reqs: Vec<u32> = r.events().into_iter().filter_map(|e| match e { Event::Edge { dir: Dir::Request, id, .. } => Some(id.ordinal()), _ => None }).collect();
        assert_eq!(reqs, vec![MAX_ORDINAL], "the send order wrapped and reused an early ordinal: {reqs:?}");
        assert_eq!(p.sends, u32::MAX, "the send order did not saturate");
    }

    /// THE LOADER'S SEGMENT OPENS THE RECORDING (the architect's three conditions): valid fetch rounds are kept as
    /// `fetch#n` beside the page's `req#n`; an unknown site, an unknown key and an impossible StatusClass are each
    /// REFUSED and counted (`DroppedMsgs` at `loader::handover`), never passed through; an uncoarsened offset is
    /// re-coarsened; the ring's own losses are counted; and the page's first send, 1.4 s after the loader started,
    /// is at offset 1400 in the SAME recording -- one origin. Mutant "pass an unknown site through" -> red.
    #[test]
    fn the_loaders_segment_opens_the_recording_validated_on_one_origin() {
        use loader::{Segment, COUNTER, EDGE, EXIT};
        let start = EPOCH_MS;
        let seg = Segment {
            start_ms: start,
            events: vec![
                EDGE, 0, 0, 1, 0, // fetch#1 asked
                COUNTER, 0, 1, 1, 45, // its OffsetMs, uncoarsened: 45 -> 40
                EDGE, 0, 1, 1, 0, // answered
                COUNTER, 0, 1, 2, 1, // StatusClass NotFound
                EDGE, 0, 0, 2, 0, // fetch#2 asked
                EXIT, 0, 2, 1, 0, // withdrawn (the race had enough)
                EDGE, 7, 0, 3, 0, // an UNKNOWN site
                COUNTER, 0, 3, 9, 1, // an UNKNOWN key
                COUNTER, 0, 3, 2, 99, // an impossible StatusClass
            ],
            dropped: 3,
        };
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(start + 1_400));
        p.record_into_after(1024, &seg);
        p.send(Waiting::Ext(Ext::SignerFirst), Op::Ext(Ext::SignerFirst));
        let r = p.recording().expect("attached");
        let ev = r.events();
        let fetch = |n| instrument::Label::new(Kind::Fetch, n).expect("a small ordinal");
        let site = instrument::vocab::Site::of("loader::fetch");
        assert_eq!(ev[0], Event::Edge { site, dir: Dir::Request, id: fetch(1) });
        assert_eq!(ev[1], Event::Counter { site, op: fetch(1).op(), entry: Entry { key: Key::OffsetMs, value: 40 } }, "the offset was not re-coarsened");
        assert_eq!(ev[5], Event::Exit { site, op: fetch(2).op(), outcome: Outcome::Withdrawn });
        let refused = ev.iter().filter(|e| matches!(e, Event::Counter { site, entry: Entry { key: Key::DroppedMsgs, .. }, .. } if site.name() == "loader::handover")).count();
        assert_eq!(refused, 3, "not every unknown code was refused and counted: {ev:?}");
        assert!(ev.iter().any(|e| matches!(e, Event::Counter { site, entry: Entry { key: Key::DroppedMsgs, value: 3 }, .. } if site.name() == "loader::ring")), "the ring's losses were not counted");
        assert!(!ev.iter().any(|e| e.site().name() == "loader::fetch" && matches!(e, Event::Edge { id, .. } if id.ordinal() == 3)), "an event with an unknown site was recorded");
        assert_eq!(counters(&r, 1, Key::OffsetMs), vec![1_400], "the page's send is not on the loader's origin");
        println!("{}", p.dump(40));
    }

    /// A send that SUPERSEDES one still on the wire (the same op sent again before its answer) ENDS the old one,
    /// Withdrawn: the recording never holds a request that nothing closed and nothing will answer. Found by the
    /// page model's accounting; pinned here, where no seed decides whether the path is reached. Mutant "the
    /// superseded send's end not recorded" -> red.
    #[test]
    fn a_send_superseding_one_on_the_wire_closes_it_withdrawn() {
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        p.record_into(64);
        p.send(Waiting::Ext(Ext::SignerFirst), Op::Ext(Ext::SignerFirst));
        p.send(Waiting::Ext(Ext::SignerFirst), Op::Ext(Ext::SignerFirst));
        let r = p.recording().expect("attached");
        let site = op_site(&Waiting::Ext(Ext::SignerFirst));
        assert!(r.events().contains(&Event::Exit { site, op: instrument::Label::new(Kind::Request, 1).expect("1").op(), outcome: Outcome::Withdrawn }), "the superseded send req#1 was not closed");
        let open: Vec<_> = r.answers().into_iter().filter(|(_, a)| *a == instrument::Answered::Never).map(|(l, _)| l.ordinal()).collect();
        assert_eq!(open, vec![1, 2], "req#1 closed-unanswered and req#2 on the wire");
        assert_eq!(p.ops_on_wire(), 1, "one op on the wire");
    }

}

#[cfg(test)]
mod rto_rearm {
    use super::*;

    /// sdk#378: an op sent while the RTO was backed off to its ceiling is
    /// re-sent one NEW RTO after its sibling's answer -- not after the minute
    /// it was first given -- while a RE-SEND keeps its backoff.
    #[test]
    fn a_first_send_is_rearmed_when_a_sample_lowers_the_rto_and_a_resend_is_not() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        // The opening head read answered through the real path: the first PUT goes into an EMPTY queue, so its
        // answer is a sample (sdk#390).
        p.now = 5;
        p.answered(&Waiting::RecoverHead);
        // A cold phase timed out tick after tick: the shared RTO at its ceiling.
        for _ in 0..10 {
            p.rto.timed_out();
        }
        assert_eq!(p.rto.rto_ms(), rto::RTO_MAX_MS as u64, "THE SETUP: the RTO is not at its ceiling");
        let (answered, lost, resend) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        p.now = 1_000;
        p.send(Waiting::Put(answered), Op::Put { id: answered, bytes: vec![1] });
        p.send(Waiting::Put(lost), Op::Put { id: lost, bytes: vec![2] });
        // A re-send: its first send went unanswered.
        p.attempt_of.insert(Waiting::Put(resend), 1);
        p.send(Waiting::Put(resend), Op::Put { id: resend, bytes: vec![3] });
        let _ = p.take_ops();
        let resend_at = p.deadlines[&Waiting::Put(resend)].at;
        assert_eq!(p.deadlines[&Waiting::Put(lost)].at, 1_000 + rto::RTO_MAX_MS as u64, "THE SETUP: the lost PUT was not given the ceiling");
        assert_eq!(p.deadlines[&Waiting::Put(resend)].attempt, 2, "THE SETUP: the re-send is not a second attempt");

        // Its sibling is answered on its first send after 100 ms: a sample, and the RTO falls.
        p.now = 1_100;
        p.answered(&Waiting::Put(answered));
        let rto = p.rto.rto_ms();
        assert!(rto < 1_000, "THE SETUP: the sample did not lower the RTO ({rto} ms)");
        assert_eq!(p.deadlines[&Waiting::Put(lost)].at, 1_100 + rto, "the lost PUT is not due one new RTO after its sibling's answer");
        assert_eq!(p.deadlines[&Waiting::Put(resend)].at, resend_at, "a RE-SEND was pulled in: it keeps its backoff");

        // And it is re-sent one RTO after that answer, not after a minute.
        p.tick(Ms(1_100 + rto));
        let again = p.take_ops();
        assert!(again.contains(&Op::Put { id: lost, bytes: vec![2] }), "the lost PUT was not re-sent at the new RTO: {again:?}");
        assert!(!again.iter().any(|o| matches!(o, Op::Put { id, .. } if *id == resend)), "the re-send went again early");
    }
}

#[cfg(test)]
mod rto_rearm_queue {
    use super::*;

    /// THE QUEUE (the architect, sdk#378): the node answers one client's ops
    /// one at a time (F61), so a batch's answers arrive spread out, the first
    /// the fastest. With the RTO at its ceiling, 13 PUTs go at once; 12 are
    /// answered every 175 ms from 100 ms on, one never is. No answered
    /// sibling is ever re-sent -- none is declared lost while the others keep
    /// answering -- and the lost one is re-sent one RTO after the LAST answer.
    #[test]
    fn a_serialised_batch_re_sends_only_the_lost_put_one_rto_after_the_last_answer() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        // THE COLD OPEN, through the real path: the opening head read times out until the RTO is at its
        // ceiling, then is answered on a RE-send -- no sample (Karn), so the batch meets the ceiling.
        while p.rto.rto_ms() < rto::RTO_MAX_MS as u64 {
            let due = p.deadlines[&Waiting::RecoverHead].at;
            p.tick(Ms(due));
            let _ = p.take_ops();
        }
        let start = p.now;
        p.answered(&Waiting::RecoverHead);
        assert_eq!(p.rto.rto_ms(), rto::RTO_MAX_MS as u64, "THE SETUP: the RTO is not at its ceiling");
        assert!(p.rto.srtt_ms().is_none(), "THE SETUP: a sample was taken before the batch");
        let ids: Vec<[u8; 32]> = (1..=13u8).map(|i| [i; 32]).collect();
        let lost = ids[12];
        for id in &ids {
            p.send(Waiting::Put(*id), Op::Put { id: *id, bytes: vec![id[0]] });
        }
        let _ = p.take_ops();
        let mut resent: Vec<u8> = Vec::new();
        let mut last = 0;
        for (i, id) in ids[..12].iter().enumerate() {
            let t = start + 100 + 175 * i as u64;
            p.tick(Ms(t));
            resent.extend(p.take_ops().iter().filter_map(|o| match o { Op::Put { id, .. } => Some(id[0]), _ => None }));
            p.now = t;
            p.answered(&Waiting::Put(*id));
            last = t;
        }
        assert!(resent.is_empty(), "answered siblings were declared lost and re-sent: {resent:?}");
        let rto = p.rto.rto_ms();
        println!("  12 answered 175 ms apart, last at {last}; rto {rto}; the lost PUT due at {}", p.deadlines[&Waiting::Put(lost)].at);
        assert_eq!(p.deadlines[&Waiting::Put(lost)].at, last + rto, "the lost PUT is not due one RTO after the last answer");
        p.tick(Ms(last + rto));
        assert!(p.take_ops().contains(&Op::Put { id: lost, bytes: vec![13] }), "the lost PUT was not re-sent at last answer + RTO");
    }

    /// sdk#390: the node answers one client's ops in ONE sequence, ~37 ms
    /// each (F61), so an op's wait is its place in that queue. After a FAST
    /// sample (the opening head read, 5 ms) the RTO is at its floor; a batch
    /// of 13 PUTs sent at once is answered one every 37 ms, one never is. No
    /// answered sibling is re-sent -- each was still in the node's queue --
    /// and the lost one is still re-sent within an RTO of the last answer
    /// (#386's guarantee).
    #[test]
    fn after_a_fast_sample_a_serialised_batch_re_sends_only_the_lost_put() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.now = 5;
        p.answered(&Waiting::RecoverHead);
        assert_eq!(p.rto.srtt_ms(), Some(5.0), "THE SETUP: the opening head read was not a 5 ms sample");
        println!("  after the fast sample: rto {} ms", p.rto.rto_ms());
        let ids: Vec<[u8; 32]> = (1..=13u8).map(|i| [i; 32]).collect();
        let lost = ids[12];
        let start = 1_000;
        p.now = start;
        for id in &ids {
            p.send(Waiting::Put(*id), Op::Put { id: *id, bytes: vec![id[0]] });
        }
        let _ = p.take_ops();
        let mut resent: Vec<(u64, u8)> = Vec::new();
        let mut last = 0;
        for (i, id) in ids[..12].iter().enumerate() {
            let t = start + 37 * (i as u64 + 1);
            // Every ms between answers: a deadline due inside the gap fires in it.
            for ms in last.max(start) + 1..=t {
                p.tick(Ms(ms));
                resent.extend(p.take_ops().iter().filter_map(|o| match o { Op::Put { id, .. } => Some((ms, id[0])), _ => None }));
            }
            p.now = t;
            p.answered(&Waiting::Put(*id));
            last = t;
        }
        println!("  12 answered 37 ms apart, last at {last}; rto {}; re-sent (ms, put): {resent:?}", p.rto.rto_ms());
        assert!(resent.is_empty(), "siblings still in the node's queue were re-sent: {resent:?}");
        let rto = p.rto.rto_ms();
        // Only an answer to an op sent into an EMPTY queue is a sample: a queued one times its wait too.
        assert!(rto < 200, "the batch's queue waits were taken as RTT samples: rto {rto} ms");
        let due = p.deadlines[&Waiting::Put(lost)].at;
        assert!(due <= last + rto, "the lost PUT is due at {due}, later than an RTO ({rto} ms) after the last answer ({last})");
        p.tick(Ms(due));
        assert!(p.take_ops().contains(&Op::Put { id: lost, bytes: vec![13] }), "the lost PUT was not re-sent at {due}");
    }
}

#[cfg(test)]
mod rto_queue {
    use super::*;

    /// A page whose opening head read was answered on its first send after
    /// 5 ms: the RTO at its floor, nothing on the wire.
    fn fast() -> Page {
        fast_at(0)
    }

    /// [`fast`] on a page whose clock starts at `origin` (the browser's is `Date.now()`, EPOCH ms: #397's lesson).
    fn fast_at(origin: u64) -> Page {
        let mut p = Page::new_at(Params::default(), PutPath::Page, Ms(origin));
        p.now = origin + 5;
        p.answered(&Waiting::RecoverHead);
        assert_eq!(p.rto.rto_ms(), rto::RTO_FLOOR_MS as u64, "THE SETUP: the RTO is not at its floor");
        p
    }

    const EPOCH_MS: u64 = 1_790_253_181_367;

    /// sdk#390 (i), the architect's amendment: UNDER LOAD no op is sent into an empty queue, so nothing samples -- and
    /// a sample was the only thing that ended the back-off (seed 39's shape: a page sat at the 60 s ceiling for 38 s
    /// while answers kept arriving). A QUEUED first send's answer ends the back-off, and is still no sample. Mutant
    /// "a queued answer does not clear" -> red.
    #[test]
    fn under_load_a_queued_answer_ends_the_back_off_without_sampling() {
        let mut p = Page::unstarted(Params::default(), PutPath::Page, Ms(EPOCH_MS));
        for _ in 0..10 {
            p.rto.timed_out();
        }
        assert_eq!(p.rto.rto_ms(), rto::RTO_MAX_MS as u64, "THE SETUP: the RTO is not at its ceiling");
        p.now = EPOCH_MS + 1_000;
        for i in 1..=3u8 {
            put(&mut p, i);
        }
        // The SECOND put is answered: it was queued behind the first, so it is no sample -- but the back-off ends.
        p.now = EPOCH_MS + 1_074;
        p.answered(&Waiting::Put([2; 32]));
        assert_eq!(p.rto.srtt_ms(), None, "a queued answer was taken as an RTT sample");
        assert_eq!(p.rto.rto_ms(), rto::RTO_INITIAL_MS as u64, "the back-off did not end on a queued first-send answer");
    }

    fn put(p: &mut Page, i: u8) {
        p.send(Waiting::Put([i; 32]), Op::Put { id: [i; 32], bytes: vec![i] });
    }

    /// sdk#390 (b): ANY answer re-arms the first sends, a RE-SEND's too -- no
    /// sample, but the queue is one shorter. A PUT is lost and re-sent; a
    /// batch of 3 goes out behind it, each armed an RTO per place. The
    /// re-send is answered: every op of the batch moves up one place, timed
    /// from that answer. Mutant "re-arm only on a sample" -> red.
    ///
    /// (A whole batch the node DROPS cannot show it: the head's timeout
    /// doubles the RTO, so from the head's re-send answer each op's place is
    /// timed at twice the RTO it was armed at -- never earlier than `armed`.)
    #[test]
    fn a_resends_answer_moves_the_batch_behind_it_up_one_place() {
        let mut p = fast();
        p.now = 1_000;
        put(&mut p, 9);
        p.tick(Ms(1_100));
        assert!(p.take_ops().contains(&Op::Put { id: [9; 32], bytes: vec![9] }), "THE SETUP: the lost PUT was not re-sent");
        let rto = p.rto.rto_ms();
        for i in 1..=3u8 {
            put(&mut p, i);
        }
        let _ = p.take_ops();
        let armed: Vec<u64> = (1..=3u8).map(|i| p.deadlines[&Waiting::Put([i; 32])].at).collect();
        assert_eq!(armed, vec![1_100 + 2 * rto, 1_100 + 3 * rto, 1_100 + 4 * rto], "THE SETUP: the batch is not armed an RTO per place behind the re-send");
        p.now = 1_110;
        p.answered(&Waiting::Put([9; 32]));
        assert_eq!(p.rto.rto_ms(), rto, "a re-send's answer was taken as a sample");
        let at: Vec<u64> = (1..=3u8).map(|i| p.deadlines[&Waiting::Put([i; 32])].at).collect();
        println!("  rto {rto}; batch armed {armed:?}; after the re-send's answer {at:?}");
        assert_eq!(at, vec![1_110 + rto, 1_110 + 2 * rto, 1_110 + 3 * rto], "the batch did not move up one place on the re-send's answer");
    }

    /// sdk#390: A SLOW OP AT THE HEAD costs ITS re-send, not the queue's. A
    /// batch of 3 after a fast sample; the first is answered at 37 ms, the
    /// second takes 187 ms -- past an RTO from that answer, so it times out
    /// and is re-sent (it is the head: nothing can tell slow from lost). The
    /// third, one place behind it, keeps an RTO for its place and is answered
    /// on its first send. Mutant "re-arm at one RTO whatever the place" ->
    /// red: the third is re-sent with the head.
    #[test]
    fn a_slow_head_is_re_sent_alone_and_the_op_behind_it_is_not() {
        // At a small clock AND at the browser's epoch clock (#397's lesson): the same rounds, the same re-sends.
        for o in [0, EPOCH_MS] {
            let mut p = fast_at(o);
            p.now = o + 1_000;
            for i in 1..=3u8 {
                put(&mut p, i);
            }
            let _ = p.take_ops();
            let mut resent: Vec<(u64, u8)> = Vec::new();
            let mut rto_after_first = 0;
            for ms in 1_001..=1_300u64 {
                p.tick(Ms(o + ms));
                resent.extend(p.take_ops().iter().filter_map(|op| match op { Op::Put { id, .. } => Some((ms, id[0])), _ => None }));
                let answer = match ms { 1_037 => Some(1u8), 1_187 => Some(2), 1_224 => Some(3), _ => None };
                if let Some(i) = answer {
                    p.now = o + ms;
                    p.answered(&Waiting::Put([i; 32]));
                    if i == 1 {
                        rto_after_first = p.rto.rto_ms();
                    }
                }
            }
            println!("  origin {o}: rto after the first answer {rto_after_first}; re-sent (ms, put): {resent:?}");
            assert_eq!(resent, vec![(1_037 + rto_after_first, 2)], "origin {o}: not the slow head alone was re-sent");
        }
    }

    /// sdk#390: a DEAD PATH is still found at one RTO. Position arming waits
    /// longer only for ops BEHIND another; the head of the queue times out at
    /// one RTO, and only it.
    #[test]
    fn the_head_of_the_queue_still_times_out_at_one_rto() {
        let mut p = fast();
        let rto = p.rto.rto_ms();
        p.now = 1_000;
        for i in 1..=13u8 {
            put(&mut p, i);
        }
        let _ = p.take_ops();
        let mut resent: Vec<(u64, u8)> = Vec::new();
        for ms in 1_001..=1_000 + rto {
            p.tick(Ms(ms));
            resent.extend(p.take_ops().iter().filter_map(|o| match o { Op::Put { id, .. } => Some((ms, id[0])), _ => None }));
        }
        println!("  rto {rto}; re-sent by 1000 + rto: {resent:?}");
        assert_eq!(resent, vec![(1_000 + rto, 1)], "the head of the queue did not time out alone at one RTO");
    }
}

#[cfg(test)]
mod reconnect {
    use super::*;

    /// A RECONNECT (sdk#376, the architect): every op ON THE WIRE went out on
    /// the socket that is gone, so each is sent again in the SAME call -- and
    /// it is not a timeout: nothing was lost on the path, so the window is not
    /// halved, the RTO does not back off, the attempt is what it was, and the
    /// re-send's answer is no RTT sample (Karn). A PARKED op was not on the
    /// wire and keeps its backoff.
    #[test]
    fn a_reconnect_resends_every_op_on_the_wire_at_once_and_is_not_a_timeout() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        let ids: Vec<[u8; 32]> = (1..=3u8).map(|i| [i; 32]).collect();
        for id in &ids {
            p.send(Waiting::Get(*id), Op::Get { id: *id });
        }
        let parked = [9u8; 32];
        p.send(Waiting::Get(parked), Op::Get { id: parked });
        let first = p.take_ops();
        assert_eq!(first.iter().filter(|o| matches!(o, Op::Get { .. })).count(), 4, "THE SETUP: the GETs did not go out (within the window)");
        p.answer(Answer::GetMissed(parked), Ms(10));
        let parked_at = p.deadlines[&Waiting::Get(parked)].at;
        let (rto, window) = (p.rto.rto_ms(), p.window.size());
        let attempts: Vec<u32> = ids.iter().map(|id| p.deadlines[&Waiting::Get(*id)].attempt).collect();

        p.reconnected(Ms(50));
        let again = p.take_ops();
        for id in &ids {
            assert!(again.contains(&Op::Get { id: *id }), "a GET on the wire was not re-sent on the reconnect: {again:?}");
        }
        assert!(!again.contains(&Op::Get { id: parked }), "a PARKED GET (not on the wire) was re-sent");
        assert_eq!(p.deadlines[&Waiting::Get(parked)].at, parked_at, "a parked GET lost its backoff");
        assert_eq!(p.window.size(), window, "the reconnect halved the window, as if the path were congested");
        assert_eq!(p.rto.rto_ms(), rto, "the reconnect backed the RTO off");
        let after: Vec<u32> = ids.iter().map(|id| p.deadlines[&Waiting::Get(*id)].attempt).collect();
        assert_eq!(after, attempts, "a re-send on the new socket was counted as another attempt");

        // KARN: the re-send's answer, however late, is no RTT sample.
        p.now = 50_000;
        p.answered(&Waiting::Get(ids[0]));
        assert_eq!(p.rto.rto_ms(), rto, "a re-sent op's answer was taken as an RTT sample");
    }
}

#[cfg(test)]
mod not_sent {
    use super::*;

    /// AN OP THE HOST WILL NOT SEND ENDS ITS WAIT, whatever its kind: the
    /// wait is found through the deadline `send` recorded, so no kind is left
    /// waiting (a match on the op's kind with a `_ => None` arm left a Get or
    /// a head read waiting for ever). Every op kind, and a head read under
    /// each of its five waits.
    #[test]
    fn a_not_sent_answer_ends_the_wait_of_every_op_kind() {
        let id = [9u8; 32];
        let sign = Op::Sign { id: 1, prev_seq: 0, prev_root: [0u8; 32], seq: 1, root: id, ledger: Vec::new(), label: Label::Head };
        let cases = vec![
            (Waiting::Put(id), Op::Put { id, bytes: b"x".to_vec() }),
            (Waiting::Get(id), Op::Get { id }),
            (Waiting::Held(id), Op::AskHeld { id }),
            (Waiting::Sign(Label::Head), sign),
            (Waiting::Update(Label::Head), Op::Update { label: Label::Head, state: b"s".to_vec() }),
            (Waiting::PutApp("k".into()), Op::PutApp { key: "k".into() }),
            (Waiting::Warm, Op::ReadHead { label: Label::Head }),
            (Waiting::RecoverHead, Op::ReadHead { label: Label::Head }),
            (Waiting::Verify, Op::ReadHead { label: Label::Head }),
            (Waiting::ReadBack(Label::Head), Op::ReadHead { label: Label::Head }),
            (Waiting::Hint, Op::ReadHead { label: Label::Head }),
        ];
        for (w, op) in cases {
            let mut p = Page::new(Params::default(), PutPath::Page);
            p.send(w.clone(), op.clone());
            let _ = p.take_ops();
            assert!(p.deadlines.contains_key(&w), "THE CONTROL: {w:?} was not waiting after its send");
            p.answer(Answer::NotSent { op: op.clone(), why: "refused".into() }, Ms(1));
            assert!(!p.deadlines.contains_key(&w), "a NotSent for {op:?} left {w:?} waiting");
        }
    }
}

#[cfg(test)]
mod window_loss {
    //! THE GET WINDOW UNDER LOSS (sdk#345, the architect's construction): one
    //! silent block must not collapse the window and starve every other read.
    //! Measured on the real network: the window went 9 -> 5 -> 2 -> 1 as ONE
    //! block timed out again and again, and the next head's root sat queued
    //! behind it for 160 s.
    use super::*;
    use freenet_prolly::{block_id, kind};

    /// `n` raw blocks: their ids and bytes.
    fn blocks(n: usize) -> Vec<(Cid, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let bytes = format!("block {i}").into_bytes();
                (block_id(kind::RAW, &bytes), bytes)
            })
            .collect()
    }

    /// A fake node over `held`: every GET it is sent is answered with the
    /// block after 5 ms, unless the block is `silent` or the node is silent
    /// (`quiet_until`). Checks after every step that the GETs on the wire
    /// never exceed the window, and returns the most seen above it.
    struct Node_ {
        held: BTreeMap<Cid, Vec<u8>>,
        silent: BTreeSet<Cid>,
        quiet_until: u64,
        /// Blocks answered after this many ms instead of 5.
        slow: BTreeMap<Cid, u64>,
        due: Vec<(u64, Answer)>,
        sent: Vec<(u64, Cid)>,
        /// Most GETs on the wire past the window in a ms that SENT one: rule 9
        /// paces SENDS (a halving never recalls a GET already on the wire).
        sent_past_window: usize,
    }

    impl Node_ {
        fn new(held: &[(Cid, Vec<u8>)]) -> Node_ {
            Node_ { held: held.iter().cloned().collect(), silent: BTreeSet::new(), quiet_until: 0, slow: BTreeMap::new(), due: Vec::new(), sent: Vec::new(), sent_past_window: 0 }
        }

        /// Run `p` for `ms`, one ms at a time.
        fn run(&mut self, p: &mut Page, now: &mut u64, ms: u64) {
            for _ in 0..ms {
                let ops = p.take_ops();
                if ops.iter().any(|o| matches!(o, Op::Get { .. })) {
                    self.sent_past_window = self.sent_past_window.max(p.gets_in_flight().saturating_sub(p.window.size()));
                }
                for op in ops {
                    if let Op::Get { id } = op {
                        self.sent.push((*now, id));
                        if !self.silent.contains(&id) && *now >= self.quiet_until {
                            if let Some(bytes) = self.held.get(&id) {
                                let delay = self.slow.get(&id).copied().unwrap_or(5);
                                self.due.push((*now + delay, Answer::Got { id, bytes: bytes.clone() }));
                            }
                        }
                    }
                }
                *now += 1;
                let (ready, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due).into_iter().partition(|(t, _)| *t <= *now);
                self.due = later;
                for (_, a) in ready {
                    p.answer(a, Ms(*now));
                }
                p.tick(Ms(*now));
            }
        }
    }

    /// Ask for `id` at `now` (the page's clock is set first: a page that has
    /// not ticked yet thinks it is 0, and would date the GET long past due).
    fn ask(p: &mut Page, now: u64, id: Cid) {
        p.now = now;
        p.send(Waiting::Get(id), Op::Get { id });
    }

    /// ONE BLOCK SILENT FOR EVER, 20 AVAILABLE, asked one at a time while the
    /// silent one keeps timing out: every one of the 20 is read, each within
    /// a few RTOs of being asked -- never behind the silent block's backoff
    /// (which reaches the 60 s ceiling), with the window at its floor.
    /// Mutant "window floor 1" -> red: the silent block's re-send holds the
    /// only place for its whole backoff.
    #[test]
    fn one_silent_block_does_not_starve_the_reads_behind_it() {
        let all = blocks(21);
        let (silent, rest) = (all[0].0, &all[1..]);
        let mut node = Node_::new(&all);
        node.silent.insert(silent);
        let mut p = Page::new(Params::default(), PutPath::Page);
        // Earlier loss episodes have already taken the window to its floor:
        // the place the floor keeps is the only one the reads get.
        for _ in 0..3 {
            p.window.halved();
        }
        assert_eq!(p.window.size(), rto::WINDOW_FLOOR as usize, "THE CONTROL: the window is not at its floor");
        let mut now = 1_000u64;
        ask(&mut p, now, silent);
        // Let the silent block time out several times first: the window is
        // under loss when the reads come.
        node.run(&mut p, &mut now, 30_000);
        let mut waited = Vec::new();
        for (id, _) in rest {
            let asked = now;
            ask(&mut p, now, *id);
            let mut t = 0;
            while p.blocks.get(id).is_none() && t < 120_000 {
                node.run(&mut p, &mut now, 100);
                t += 100;
            }
            assert!(p.blocks.get(id).is_some(), "a block the node serves was not read in 120 s behind one silent block (window {})", p.window.size());
            waited.push(now - asked);
            node.run(&mut p, &mut now, 5_000);
        }
        let worst = *waited.iter().max().expect("20 reads");
        println!("20 reads behind one silent block: waited {waited:?} ms, window {}", p.window.size());
        assert!(worst <= 2_000, "a read waited {worst} ms behind one silent block: {waited:?}");
        assert!(node.sent.iter().filter(|(_, id)| *id == silent).count() > 3, "THE CONTROL: the silent block did not keep timing out");
    }

    /// A LOST GET'S RE-SEND QUEUES BEHIND THE ASKS ALREADY WAITING, and a lost
    /// GET leaves the in-flight count at once. Two silent blocks fill the
    /// window's floor; a third ask waits. When the silent ones time out, the
    /// waiting ask goes out FIRST. Mutant "re-send at once" (the old
    /// `self.send` in `tick`) -> red: the silent re-sends take both places
    /// again and the waiting ask never goes.
    #[test]
    fn a_lost_gets_resend_queues_behind_the_asks_already_waiting() {
        let all = blocks(3);
        let mut node = Node_::new(&all);
        node.silent.extend([all[0].0, all[1].0]);
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.window.halved(); // 4 -> 2: the floor
        assert_eq!(p.window.size(), 2, "THE CONTROL: the window is not at its floor");
        let mut now = 1_000u64;
        ask(&mut p, now, all[0].0);
        ask(&mut p, now, all[1].0);
        ask(&mut p, now, all[2].0);
        assert_eq!(p.gets_in_flight(), 2);
        assert_eq!(p.get_queue.iter().copied().collect::<Vec<_>>(), vec![all[2].0], "THE CONTROL: the third ask did not wait");
        node.run(&mut p, &mut now, 5_000);
        assert!(p.blocks.get(&all[2].0).is_some(), "the waiting ask was starved by the lost GETs' re-sends");
        let first_resend = node.sent.iter().filter(|(_, id)| *id == all[0].0 || *id == all[1].0).nth(2).map(|(t, _)| *t).expect("a re-send");
        let third = node.sent.iter().find(|(_, id)| *id == all[2].0).map(|(t, _)| *t).expect("the third ask went out");
        assert!(third <= first_resend, "a lost GET was re-sent ({first_resend}) before the ask already waiting ({third})");
    }

    /// A LOST GET'S LATE ANSWER STILL ANSWERS IT. Three slow blocks on a
    /// window of 2, each GET armed at its place (sdk#390) behind the page's
    /// opening head read: the first times out at 3 s and the third takes its
    /// place; the second at 4 s, and the first's re-send takes its place. The
    /// second's first-send answer (3.5 s after it went, at 4.5 s) lands while
    /// it is QUEUED behind the first's re-send and the third (5 s each):
    /// the first two time out and queue behind the third, and one of them is
    /// still QUEUED when its first send's answer lands. That answer is taken,
    /// and the queued re-send is dropped: each block is sent at most twice
    /// and every one is read. Mutant "a queued GET's answer is ignored" ->
    /// red: the block is sent again and read only a whole backoff later.
    #[test]
    fn a_lost_gets_late_answer_is_taken_while_its_resend_waits() {
        let all = blocks(3);
        let mut node = Node_::new(&all);
        for ((id, _), ms) in all.iter().zip([5_000, 3_500, 5_000]) {
            node.slow.insert(*id, ms);
        }
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.window.halved();
        let mut now = 1_000u64;
        for (id, _) in &all {
            ask(&mut p, now, *id);
        }
        let mut queued_when_answered = false;
        for _ in 0..7_500 {
            let was_queued: Vec<Cid> = p.get_queue.iter().copied().filter(|q| p.attempt_of.contains_key(&Waiting::Get(*q))).collect();
            node.run(&mut p, &mut now, 1);
            queued_when_answered |= was_queued.iter().any(|id| p.blocks.get(id).is_some());
        }
        let sends: Vec<usize> = all.iter().map(|(id, _)| node.sent.iter().filter(|(_, s)| s == id).count()).collect();
        println!("sends per block {sends:?}; a queued lost GET answered: {queued_when_answered}");
        assert!(queued_when_answered, "THE CONTROL: no lost GET was still queued when its late answer landed");
        assert!(all.iter().all(|(id, _)| p.blocks.get(id).is_some()), "not every block was read in 7.5 s: sends {sends:?}");
        assert!(!p.get_queue.iter().any(|q| p.blocks.get(q).is_some()), "a block already read is still queued to be asked again");
    }

    /// ENDING A GET LEAVES NO TRACE, however it ends (`drop_get`, the one
    /// definition). Two lost GETs are queued for a place with their attempt
    /// kept: one is ended by its LATE ANSWER, the other by being WITHDRAWN
    /// (its block is now held), and so is one still ON THE WIRE. None is left
    /// in any of the three holders:
    /// deadline, attempt, queue. Mutants "drop_get forgets one holder" (each
    /// of the three) -> red.
    #[test]
    fn an_ended_get_is_in_no_holder() {
        let all = blocks(4);
        let mut node = Node_::new(&all);
        let (late, withdrawn) = (all[0].0, all[1].0);
        node.silent.extend([late, withdrawn, all[2].0, all[3].0]);
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.window.halved();
        let mut now = 1_000u64;
        // `late` and `withdrawn` take the floor's two places, time out, and
        // queue behind the two asks that were waiting (which now hold the
        // places, silent too). Each is armed at its place behind the page's
        // opening head read (sdk#390): `late` at 3 s, `withdrawn` at 4 s.
        for id in [late, withdrawn, all[2].0, all[3].0] {
            ask(&mut p, now, id);
        }
        node.run(&mut p, &mut now, 3_100);
        let holders = |p: &Page, id: Cid| {
            (p.deadlines.contains_key(&Waiting::Get(id)), p.attempt_of.contains_key(&Waiting::Get(id)), p.get_queue.contains(&id))
        };
        for id in [late, withdrawn] {
            assert!(p.get_queue.contains(&id) && p.attempt_of.contains_key(&Waiting::Get(id)) && !p.deadlines.contains_key(&Waiting::Get(id)), "THE CONTROL: {:?} is not a queued lost GET: {:?}", &id[..2], holders(&p, id));
        }
        // The late answer to `late`'s first send.
        let bytes = all[0].1.clone();
        p.answer(Answer::Got { id: late, bytes }, Ms(now));
        assert!(p.blocks.get(&late).is_some(), "the late answer was not taken");
        assert_eq!(holders(&p, late), (false, false, false), "a GET ended by its late answer left a trace (deadline, attempt, queue)");
        // `withdrawn` (queued) and one ON THE WIRE: their blocks are held now
        // (a repair rebuilt them), so nobody needs either GET.
        let on_wire = all[2].0;
        assert!(p.deadlines.get(&Waiting::Get(on_wire)).is_some_and(|d| d.sent), "THE CONTROL: {:?} is not on the wire", &on_wire[..2]);
        p.blocks.insert(withdrawn, &all[1].1);
        p.blocks.insert(on_wire, &all[2].1);
        p.end_unneeded_gets();
        assert_eq!(holders(&p, withdrawn), (false, false, false), "a withdrawn queued GET left a trace (deadline, attempt, queue)");
        assert_eq!(holders(&p, on_wire), (false, false, false), "a withdrawn GET on the wire left a trace (deadline, attempt, queue)");
    }

    /// THE WINDOW HALVES ONCE PER LOSS EPISODE. One silent GET timing out
    /// five times halves it ONCE (mutant "halve per re-timeout" -> red), and
    /// three GETs sent together, lost in three different ticks, halve it ONCE
    /// (mutant "every first timeout halves" -> red).
    #[test]
    fn the_window_halves_once_per_loss_episode() {
        let all = blocks(40);
        let mut node = Node_::new(&all);
        let mut p = Page::new(Params::default(), PutPath::Page);
        let mut now = 1_000u64;
        // Grow the window: 30 answered GETs.
        for (id, _) in &all[..30] {
            ask(&mut p, now, *id);
            node.run(&mut p, &mut now, 20);
        }
        node.run(&mut p, &mut now, 1_000);
        let grown = p.window.size();
        assert!(grown >= 16, "THE CONTROL: the window did not grow ({grown})");

        // One silent block, five timeouts.
        node.silent.insert(all[30].0);
        ask(&mut p, now, all[30].0);
        let mut t = 0;
        while node.sent.iter().filter(|(_, id)| *id == all[30].0).count() < 6 && t < 600_000 {
            node.run(&mut p, &mut now, 100);
            t += 100;
        }
        assert_eq!(node.sent.iter().filter(|(_, id)| *id == all[30].0).count(), 6, "THE CONTROL: the silent GET did not time out five times");
        let once = p.window.size();
        println!("window {grown} -> {once} after one GET timed out five times");
        assert_eq!(once, (grown / 2).max(2), "one silent GET's re-timeouts halved the window more than once");

        // A fresh episode: three GETs sent 1 ms apart, all silent, lost in three ticks.
        let before = p.window.size();
        for (id, _) in &all[31..34] {
            node.silent.insert(*id);
            ask(&mut p, now, *id);
            node.run(&mut p, &mut now, 1);
        }
        let firsts: BTreeSet<u64> = all[31..34]
            .iter()
            .map(|(id, _)| {
                while node.sent.iter().filter(|(_, s)| s == id).count() < 2 {
                    node.run(&mut p, &mut now, 1);
                }
                node.sent.iter().filter(|(_, s)| s == id).nth(1).map(|(t, _)| *t).expect("re-sent")
            })
            .collect();
        println!("three lost GETs re-sent at {firsts:?}; window {before} -> {}", p.window.size());
        assert!(before > 2, "THE CONTROL: the window is at its floor, a second halving could not show");
        assert_eq!(p.window.size(), (before / 2).max(2), "three GETs lost in one episode halved the window more than once");
    }

    /// A 200-BLOCK SCAN WITH THE NODE SILENT FOR TWO RTO CEILINGS, then back:
    /// no GET is SENT while the window is full, however many were lost and
    /// re-sent, and every block is read once the node answers. A halving never
    /// recalls a GET already on the wire (TCP's rule): with each GET armed at
    /// its place in the queue (sdk#390) they time out one at a time, so the
    /// first loss halves the window under its siblings. Mutants "re-send
    /// outside the window" and "send a new GET ignoring the window" -> red.
    #[test]
    fn a_scan_through_a_silent_node_never_sends_past_the_window() {
        let all = blocks(200);
        let mut node = Node_::new(&all);
        let mut p = Page::new(Params::default(), PutPath::Page);
        let mut now = 1_000u64;
        node.quiet_until = now + 2 * rto::RTO_MAX_MS as u64;
        for (id, _) in &all {
            ask(&mut p, now, *id);
        }
        node.run(&mut p, &mut now, 2 * rto::RTO_MAX_MS as u64 + 600_000);
        let read = all.iter().filter(|(id, _)| p.blocks.get(id).is_some()).count();
        let lost = node.sent.len() - 200;
        println!("200 blocks through a silent node: {read} read, {} GETs sent ({lost} re-sends), most past the window when one was sent {}", node.sent.len(), node.sent_past_window);
        assert!(lost > 0, "THE CONTROL: nothing was lost and re-sent");
        assert_eq!(node.sent_past_window, 0, "a GET was SENT with the window full: {} past it", node.sent_past_window);
        assert_eq!(read, 200, "not every block was read after the node came back");
        assert!(p.window.size() >= rto::WINDOW_FLOOR as usize);
    }
}

#[cfg(test)]
mod head_floor {
    use super::*;

    /// A HEAD BELOW THE PUBLISHED-HEAD FLOOR IS AN ANSWER, NOT SILENCE (the
    /// architect on sdk#354): it ends the head read it answers and adopts
    /// nothing -- the read is PARKED at the one backoff, as a GET answered
    /// without its block is. So coming due is no timeout: the RTO does not
    /// back off and the window does not halve, and the head is asked again.
    /// A later answer at the floor is adopted. Mutant "drop the answer before
    /// it ends the read" -> red: the read times out as silence.
    #[test]
    fn a_head_below_the_floor_parks_the_read_and_a_later_one_at_it_is_adopted() {
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.set_head_floor(2);
        assert!(p.take_ops().contains(&Op::ReadHead { label: Label::Head }), "THE CONTROL: the page did not read its head");
        let sent = p.deadlines.get(&Waiting::RecoverHead).map(|d| d.sent);
        assert_eq!(sent, Some(true), "THE CONTROL: the head read is not on the wire");

        p.answer(Answer::Head { label: Label::Head, read: Some(HeadRead::from((1, [1u8; 32]))) }, Ms(10));
        let parked = p.deadlines.get(&Waiting::RecoverHead).map(|d| (d.sent, d.at));
        let (sent, due) = parked.expect("a head below the floor dropped the head read instead of parking it");
        assert!(!sent, "a head below the floor left the read on the wire, to time out as silence");
        assert!(!p.recovered(), "a head below the floor was adopted");
        let (rto, window) = (p.rto.rto_ms(), p.window.size());
        p.tick(Ms(due));
        assert_eq!((p.rto.rto_ms(), p.window.size()), (rto, window), "the parked head read coming due was counted as a timeout");
        assert!(p.take_ops().contains(&Op::ReadHead { label: Label::Head }), "the parked head read was not asked again");
        assert_eq!(p.head_floor_wait(), Some((2, Some(1), 1)));

        p.answer(Answer::Head { label: Label::Head, read: Some(HeadRead::from((2, [2u8; 32]))) }, Ms(due + 10));
        assert!(p.recovered(), "a head at the floor was not adopted");
        assert_eq!(p.head_floor_wait(), None);
    }
}

#[cfg(test)]
mod head_judgement_cells {
    //! The two cells the one judgement (sdk#396) changed beyond the gap, each forced: a register answer showing a
    //! head THIS page's record beats, at a seq ABOVE the one the path was asked about, is never adopted.
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    /// THIS page's signed record at `seq` over `value` (the real Register format), and it read as a head.
    fn my_record(seq: u64, value: &[u8]) -> (Vec<u8>, HeadRead) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&KEY);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        let record = contract_keys::register::head_state(&params, &sk.to_bytes(), seq, value).expect("signs");
        let read = HeadRead::from_record(&record).expect("reads as a head");
        (record, read)
    }

    /// A head at `mine.seq` under another root whose whole value LOSES the tie-break to `mine`.
    fn a_losing_head(mine: &HeadRead) -> HeadRead {
        (1u8..=255)
            .map(|b| HeadRead::from_value(mine.seq, &[b; 32]).expect("a head"))
            .find(|h| h.root() != mine.root() && beats(mine.value(), h.value()))
            .expect("some root loses to mine")
    }

    /// A page whose opening head read is answered away (so an answer reaches only the path under test).
    fn page() -> Page {
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.answered(&Waiting::RecoverHead);
        let _ = p.take_ops();
        p
    }

    /// **VERIFY, above the signer's seq.** The signer named seq 0 (`NotNext`), the register shows seq 1 under a root
    /// THIS page's record at seq 1 beats. Mine is LANDED (its UPDATE, my record's bytes), never adopted. The old
    /// verify judged the tie-break only AT the signer's seq and adopted above it. Mutant "land only at the signer's
    /// seq (`==`)" -> no UPDATE of mine -> red.
    #[test]
    fn a_verify_above_the_signers_seq_lands_a_head_my_record_beats_and_adopts_nothing() {
        let mut p = page();
        let (record, mine) = my_record(1, &[0xAA; 32]);
        p.my_records.insert(1, (mine.root(), record.clone()));
        let theirs = a_losing_head(&mine);
        p.verify = Some(Verify { seq: 0, root: [0u8; 32], landing: false, again_at: None, tries: 0, updates: 0, from: None });
        p.send(Waiting::Verify, Op::ReadHead { label: Label::Head });
        let _ = p.take_ops();
        let before = p.published();
        p.answer(Answer::Head { label: Label::Head, read: Some(theirs.clone()) }, Ms(10));
        assert_ne!(p.published(), (theirs.seq, theirs.root()), "verify adopted a head this page's own record beats");
        assert_eq!(p.published(), before, "verify moved the head on a head this page's own record beats");
        let ops = p.take_ops();
        assert!(ops.iter().any(|o| matches!(o, Op::Update { label: Label::Head, state } if *state == record)), "my winning record was not landed (its UPDATE): {ops:?}");
    }

    /// **READ-BACK, above the owed seq.** This page owes seq 1; the register shows seq 2 under a root THIS page's
    /// record at seq 2 beats. Not adopted: the commit stays owed, and it is read again (a stale read counted). The
    /// old read-back judged the tie-break only AT the owed seq and took anything above it as a conflict. Mutant "the
    /// read-back adopts MineWins (HeadConflict)" -> the owed commit is dropped -> red.
    #[test]
    fn a_read_back_above_the_owed_seq_never_adopts_a_head_my_record_beats() {
        let mut p = page();
        let (record1, owed) = my_record(1, &[0xAA; 32]);
        let (record2, mine2) = my_record(2, &[0xBB; 32]);
        p.my_records.insert(1, (owed.root(), record1.clone()));
        p.my_records.insert(2, (mine2.root(), record2));
        p.head.owed = Some(Owed { seq: 1, root: owed.root(), base: [0u8; 32], record: Some(record1), stale_reads: 0 });
        let theirs = a_losing_head(&mine2);
        p.send(Waiting::ReadBack(Label::Head), Op::ReadHead { label: Label::Head });
        let _ = p.take_ops();
        p.answer(Answer::Head { label: Label::Head, read: Some(theirs.clone()) }, Ms(10));
        assert_ne!(p.published(), (theirs.seq, theirs.root()), "the read-back adopted a head this page's own record beats");
        let o = p.head.owed.as_ref().expect("the owed commit was dropped for a head this page's own record beats");
        assert_eq!((o.seq, o.stale_reads), (1, 1), "the read-back did not read again (one stale read counted)");
    }
}

#[cfg(test)]
mod confirmed_once {
    //! A commit is confirmed ONCE, whichever of its two confirmations lands first (#378, after #393): its read-back
    //! GET's answer (E4) or the node's push of exactly its head (E1, P3). #393 emits the held-back parity at the
    //! confirmation, so a second confirmation would send it twice. ONE owner decides it (COMMIT-LIFE ⁹, sdk#414): the
    //! engine's `pending` take in `on_head`; a second `HeadConfirmed` of the same seq is a no-op there. The page holds
    //! no guard: its owed head FOLLOWS the engine's published seq (`drop_dead_head`). So these tests count what the
    //! ENGINE emits for the commit -- its write's `Published` notices -- never the page's steps.
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    fn signer() -> (ed25519_dalek::SigningKey, Vec<u8>) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&KEY);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        (sk, params)
    }

    /// Sign `seq` over `root` and `ledger` under the test key.
    fn sign(seq: u64, root: &Cid, ledger: &[u8]) -> Vec<u8> {
        let (sk, params) = signer();
        let value = [root.as_slice(), ledger].concat();
        contract_keys::register::head_state(&params, &sk.to_bytes(), seq, &value).expect("signs")
    }

    /// A page whose first write's UPDATE is out (and `second`, when given, queued behind that commit); its record,
    /// read as a head; and the ids of every block the page put.
    fn at_update(second: bool) -> (Page, HeadRead, BTreeSet<Cid>) {
        at_update_signed(second, None)
    }

    /// As [`at_update`]; with `other_root`, the signer answers `AlreadySigned` with a record ANOTHER page of this key
    /// made at the same seq, under that root.
    fn at_update_signed(second: bool, other_root: Option<Cid>) -> (Page, HeadRead, BTreeSet<Cid>) {
        let mut p = Page::new(Params::default(), PutPath::Page);
        p.write(ClientId(1), WriteId(1), vec![(b"k".to_vec(), WriteOp::Put(b"v".to_vec()))]);
        let mut put = BTreeSet::new();
        for _ in 0..20 {
            for op in p.take_ops() {
                match op {
                    Op::ReadHead { label: Label::Head } => p.answer(Answer::Head { label: Label::Head, read: None }, Ms(10)),
                    Op::Put { id, .. } => {
                        put.insert(id);
                        p.answer(Answer::PutOk(id), Ms(10));
                    }
                    Op::AskHeld { id } => p.answer(Answer::Held { id, present: true }, Ms(10)),
                    Op::Sign { id, seq, root, ledger, .. } => {
                        if second {
                            p.write(ClientId(1), WriteId(2), vec![(b"j".to_vec(), WriteOp::Put(b"w".to_vec()))]);
                        }
                        let answer = match other_root {
                            None => signer_proto::Answer::Signed(sign(seq, &root, &ledger)),
                            Some(other) => signer_proto::Answer::AlreadySigned(sign(seq, &other, &ledger)),
                        };
                        let record = match &answer {
                            signer_proto::Answer::Signed(r) | signer_proto::Answer::AlreadySigned(r) => r.clone(),
                            _ => unreachable!(),
                        };
                        p.answer(Answer::Signer { id, answer }, Ms(10));
                        let _ = p.take_ops();
                        return (p, HeadRead::from_record(&record).expect("reads"), put);
                    }
                    other => panic!("unexpected before the sign: {other:?}"),
                }
            }
        }
        panic!("the page never asked the signer");
    }

    /// The UPDATE answered: the read-back GET goes out (the setup asserts it).
    fn read_back_out(p: &mut Page) {
        p.answer(Answer::Updated { label: Label::Head }, Ms(11));
        assert!(p.take_ops().contains(&Op::ReadHead { label: Label::Head }), "THE SETUP: the read-back GET did not go out");
    }

    /// How many times the engine told each write `Published`.
    fn published(p: &mut Page) -> BTreeMap<WriteId, usize> {
        let mut n = BTreeMap::new();
        for (_, w, s) in p.take_notices() {
            if s == State::Published {
                *n.entry(w).or_default() += 1;
            }
        }
        n
    }

    /// **E1 then E4:** the push confirms; the read-back GET, already on the wire, answers after. Published once. The
    /// ORDER pin (the architect on sdk#414): the confirmation withdrew the read-back GET, and its late answer is
    /// DROPPED, as any withdrawn request's is (the deadline table: answer x withdrawn -> no end, no sample, no re-arm)
    /// -- so here the engine hears ONE confirmation, and its take is pinned by the other order.
    #[test]
    fn a_push_then_the_read_backs_answer_confirm_once() {
        let (mut p, mine, _) = at_update(false);
        read_back_out(&mut p);
        p.head_pushed(mine.clone());
        assert!(!p.deadlines.contains_key(&Waiting::ReadBack(Label::Head)), "the confirmation did not withdraw the read-back GET");
        p.answer(Answer::Head { label: Label::Head, read: Some(mine.clone()) }, Ms(12));
        assert_eq!(published(&mut p), BTreeMap::from([(WriteId(1), 1)]), "the commit was not confirmed exactly once (E1 then E4)");
    }

    /// **E4 then E1:** the read-back's answer confirms; the node's push of the same head arrives after. Published once.
    #[test]
    fn a_read_backs_answer_then_the_push_confirm_once() {
        let (mut p, mine, _) = at_update(false);
        read_back_out(&mut p);
        p.answer(Answer::Head { label: Label::Head, read: Some(mine.clone()) }, Ms(12));
        p.head_pushed(mine.clone());
        assert_eq!(published(&mut p), BTreeMap::from([(WriteId(1), 1)]), "the commit was not confirmed exactly once (E4 then E1)");
    }

    /// THE ROOT IS PART OF A CONFIRMATION: the signer answers `AlreadySigned` with a record ANOTHER page of this key
    /// made at the same seq under another root, and the read-back shows it. That is not this commit (the engine's
    /// take checks the seq only): no `Published`.
    #[test]
    fn the_same_seq_under_another_root_is_not_a_confirmation() {
        let (mut p, theirs, _) = at_update_signed(false, Some([9u8; 32]));
        read_back_out(&mut p);
        p.answer(Answer::Head { label: Label::Head, read: Some(theirs.clone()) }, Ms(12));
        p.head_pushed(theirs.clone());
        assert_eq!(published(&mut p), BTreeMap::new(), "a head under another root was taken as this commit's confirmation");
    }

    /// Answer every op until the page is quiet, signing heads with the test key; the head a read-back sees is the
    /// last one signed.
    fn settle(p: &mut Page) {
        let mut last: Option<Vec<u8>> = None;
        for _ in 0..40 {
            let ops = p.take_ops();
            if ops.is_empty() {
                return;
            }
            for op in ops {
                match op {
                    Op::Put { id, .. } => p.answer(Answer::PutOk(id), Ms(20)),
                    Op::AskHeld { id } => p.answer(Answer::Held { id, present: true }, Ms(20)),
                    Op::Sign { id, seq, root, ledger, .. } => {
                        let record = sign(seq, &root, &ledger);
                        last = Some(record.clone());
                        p.answer(Answer::Signer { id, answer: signer_proto::Answer::Signed(record) }, Ms(20));
                    }
                    Op::Update { label: Label::Head, .. } => p.answer(Answer::Updated { label: Label::Head }, Ms(20)),
                    Op::ReadHead { label: Label::Head } => {
                        let read = last.as_deref().and_then(HeadRead::from_record);
                        p.answer(Answer::Head { label: Label::Head, read }, Ms(20));
                    }
                    other => panic!("unexpected while settling: {other:?}"),
                }
            }
        }
        panic!("the page did not settle");
    }

    /// THE CROSS-COMMIT CELL (the architect's pin on sdk#414): the confirming step itself releases the NEXT commit's
    /// head (a write queued behind the first; its blocks already on the node, so nothing holds its head back) --
    /// the owed head is replaced inside the step, before `drop_dead_head` runs after it. The new owed head and its
    /// sign wait survive; the confirmed one's waits end with it (the push path: its read-back GET was still out); the
    /// next commit publishes.
    fn the_next_head_survives_the_confirming_step(by_push: bool) {
        // The twin learns the second commit's blocks: the same writes, driven to the second commit's sign.
        let (mut twin, first, before) = at_update(true);
        read_back_out(&mut twin);
        twin.answer(Answer::Head { label: Label::Head, read: Some(first.clone()) }, Ms(12));
        let mut next = BTreeSet::new();
        for _ in 0..20 {
            for op in twin.take_ops() {
                match op {
                    Op::Put { id, .. } => {
                        next.insert(id);
                        twin.answer(Answer::PutOk(id), Ms(13));
                    }
                    Op::AskHeld { id } => twin.answer(Answer::Held { id, present: true }, Ms(13)),
                    _ => {}
                }
            }
        }
        let next: BTreeSet<Cid> = next.difference(&before).copied().collect();
        assert!(!next.is_empty(), "THE SETUP: the twin's second commit put no block of its own");

        let (mut p, mine, _) = at_update(true);
        read_back_out(&mut p);
        // The second commit's blocks are on the node already (answered earlier): its head waits on nothing.
        p.confirmed.extend(next.iter().copied());
        if by_push {
            p.head_pushed(mine.clone());
        } else {
            p.answer(Answer::Head { label: Label::Head, read: Some(mine.clone()) }, Ms(12));
        }
        let o = p.head.owed.as_ref().expect("the next commit's owed head was dropped by the confirming step");
        assert_eq!(o.seq, mine.seq + 1, "THE SETUP: the confirming step did not release the next commit's head");
        assert!(p.deadlines.contains_key(&Waiting::Sign(Label::Head)), "the next commit's sign wait was ended with the confirmed head");
        assert!(!p.deadlines.contains_key(&Waiting::ReadBack(Label::Head)), "the confirmed head's read-back wait outlived it");
        if by_push {
            // The first head's read-back GET answers late: nothing to confirm, nothing to count.
            p.answer(Answer::Head { label: Label::Head, read: Some(mine.clone()) }, Ms(13));
        }
        settle(&mut p);
        assert_eq!(p.engine_published().0, mine.seq + 1, "the next commit did not publish");
        assert_eq!(published(&mut p), BTreeMap::from([(WriteId(1), 1), (WriteId(2), 1)]), "each write was not published exactly once");
    }

    #[test]
    fn the_next_commits_head_survives_a_read_back_confirming_the_one_before() {
        the_next_head_survives_the_confirming_step(false);
    }

    #[test]
    fn the_next_commits_head_survives_a_push_confirming_the_one_before() {
        the_next_head_survives_the_confirming_step(true);
    }
}
