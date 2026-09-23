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
//! | `Refused(Forked { read, .. })` (an OLD signer: the new one answers `NotNext`, sdk#225) | the same identity never forks: as `NotNext { current: read }` — read, then adopt. If the page already stands on `read`, the old rule refuses every ask until the register passes that seq: NOT re-asked, named once in [`Page::unusable`], and asked again when a head read shows the register past it |
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

pub mod fates;
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
    /// `ledger`: the bytes after the root in the value to sign (its PREV and
    /// every page's `through`, this page's updated: [`Page::sign_ledger_of`]).
    Sign { id: u32, prev_seq: u64, prev_root: Cid, seq: u64, root: Cid, ledger: Vec<u8> },
    /// UPDATE the head register with exactly these bytes (a signed record).
    Update { state: Vec<u8> },
    /// GET the head register.
    ReadHead,
    /// [`PutPath::Wrapper`] only: ask the signer's read-local verb whether the
    /// node holds this block now.
    AskHeld { id: Cid },
    /// PUT a contract the APP names (a published web container, builder#104),
    /// by its key. The bytes stay with the web layer, which frames the same
    /// PUT again at every deadline; the PAGE owns the deadline, the re-send
    /// and the end ([`AppPut`]), as for every other op.
    PutApp { key: String },
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
    /// The head register as read: its seq and whole VALUE ([`HeadRead`]), or
    /// `None` if there is none.
    Head(Option<HeadRead>),
    /// [`PutPath::Wrapper`]: the signer's synchronous local read of a block.
    Held { id: Cid, present: bool },
    /// The node acknowledged the app's PUT of this contract key.
    AppPutOk(String),
    /// The node refused the app's PUT of this contract key, in its words.
    AppPutRefused { key: String, said: String },
}

/// Where an app's PUT ([`Op::PutApp`]) stands. It always ENDS: acknowledged,
/// refused in the node's words, or given up at [`APP_PUT_BUDGET_MS`] — never
/// a wait nobody bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppPut {
    Pending,
    Put,
    Refused(String),
    GaveUp(String),
}

/// How long an app's PUT is re-sent before it ends as [`AppPut::GaveUp`].
/// Measured on the real network (2026-09-23, a datacentre node behind a
/// tunnel): a web container was acknowledged in up to ~40 s; the RTO caps
/// at 60 s, so this leaves at least two re-sends past the slowest seen.
pub const APP_PUT_BUDGET_MS: u64 = 120_000;

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
    /// A register read on the node's `HeadChanged` hint, or the idle
    /// backstop ([`HEAD_BACKSTOP_MS`]): is there a head this page has not
    /// adopted?
    Hint,
    /// An app's PUT of this contract key ([`Op::PutApp`]).
    PutApp(String),
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
    /// When the first read went out: the register's own budget (like a cold
    /// fetch's, [`VERIFY_BUDGET_MS`]) runs from here.
    first_at: u64,
}

/// How long the register may stay unreadable, or behind a head the signer
/// named, before this page says "the register is not answering" — and still
/// adopts nothing. The cold reads' per-block budget.
pub const VERIFY_BUDGET_MS: u64 = 30_000;

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

    /// The `through` its ledger records for `device`: the last arrival number
    /// of that page's writes this head carries (COMMIT-LIFE ⁵, the witness).
    /// `None`: no entry, or a refused ledger.
    pub fn through_of(&self, device: &[u8; 16]) -> Option<u64> {
        let h = signer_proto::head::read_value(&self.value)?;
        if h.refused {
            return None;
        }
        h.ledger.through.iter().find(|t| &t.device == device).map(|t| t.seq)
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

/// The ledger a head signed from `prev` carries: its PREV, omitted at the
/// genesis (`prev_seq == 0`), never zeros. The bytes after the root in
/// `signer_proto::Next`'s value; the one rule for every sign request.
pub fn sign_ledger(prev_seq: u64, prev_root: Cid, root: Cid) -> Vec<u8> {
    use signer_proto::head::{value, Ledger};
    let prev = (prev_seq > 0).then_some(signer_proto::Head { seq: prev_seq, root: prev_root });
    value(&root, &Ledger { prev, ..Ledger::default() })[32..].to_vec()
}

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
            device: [0; 16],
            hold_on_displace: false,
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
            old_signer_fork_at: None,
            my_records: BTreeMap::new(),
            last_head_at: 0,
            last_head: None,
            put_again: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            app_puts: BTreeMap::new(),
            rto: rto::Rto::default(),
            window: rto::Window::default(),
            get_queue: Default::default(),
            attempt_of: BTreeMap::new(),
            recover_since: None,
            recover_told: false,
            recovered: false,
            engine_has_head: false,
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
        self.client_event(Event::Write { client, write_id, ops, reads });
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
                Waiting::PutApp(key) => {
                    let since = self.app_puts.get(&key).map_or(now, |(_, at)| *at);
                    if now.saturating_sub(since) >= APP_PUT_BUDGET_MS {
                        self.attempt_of.remove(&Waiting::PutApp(key.clone()));
                        let why = format!(
                            "the node did not acknowledge the PUT of {key} in {} s ({} attempts)",
                            APP_PUT_BUDGET_MS / 1000,
                            d.attempt
                        );
                        self.app_puts.insert(key, (AppPut::GaveUp(why), since));
                    } else {
                        self.send(Waiting::PutApp(key), op);
                    }
                }
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
        // THE BACKSTOP: a hint can be dropped, so an idle page reads the
        // register at least every HEAD_BACKSTOP_MS.
        if self.engine_has_head && self.verify.is_none() && !self.reading_head() && now.saturating_sub(self.last_head_at) >= HEAD_BACKSTOP_MS {
            self.last_head_at = now;
            self.send(Waiting::Hint, Op::ReadHead);
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
            Answer::Head(read) => {
                self.last_head_at = self.now;
                let h = read.as_ref().map(|r| (r.seq, r.root()));
                self.last_head = read;
                self.answered(&Waiting::Warm);
                // S1b: the register moved past an old signer's same-seq
                // record, so it signs again.
                if let (Some(at), Some((seq, _))) = (self.old_signer_fork_at, h) {
                    if seq > at {
                        self.old_signer_fork_at = None;
                        if self.owed.is_some() {
                            self.sign_again = Some(self.now);
                        }
                    }
                }
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
                if self.answered(&Waiting::Hint).is_some() {
                    self.on_hint(h);
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
                if let Some(h) = HeadRead::from_record(&state) {
                    self.my_records.insert(h.seq, (h.root(), state.clone()));
                }
                owed.record = Some(state.clone());
                owed.stale_reads = 0;
                self.send(Waiting::Update, Op::Update { state });
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
                    let now = self.now;
                    self.verify = Some(Verify { seq: read.seq, root: read.root, landing: false, again_at: None, tries: 0, updates: 0, from: None, first_at: now });
                    self.send(Waiting::Verify, Op::ReadHead);
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
    /// the web layer frames `Op::ReadHead` as that.
    fn on_verify(&mut self, h: Option<(u64, Cid)>) {
        let Some(v) = self.verify.clone() else { return };
        let reg_seq = h.map_or(0, |(s, _)| s);
        // The register at the signer's seq, and THIS page's own record there
        // is another root that wins the tie-break: LAND it (its UPDATE has not
        // merged here yet) rather than adopt a head about to lose. Judged by
        // the record, never by what the signer named: at an equal seq the
        // signer names the REGISTER's head (rule c), so `v.root` is theirs.
        if let Some((seq, root)) = h.filter(|(s, _)| *s == v.seq) {
            if let Some(bytes) = self.my_winning_record(seq, &root) {
                if let Some(v) = self.verify.as_mut() {
                    v.landing = true;
                    v.updates += 1;
                    self.most_landing_updates = self.most_landing_updates.max(v.updates);
                }
                self.send(Waiting::Update, Op::Update { state: bytes });
                return;
            }
        }
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
    fn on_hint(&mut self, h: Option<(u64, Cid)>) {
        let Some((seq, root)) = h else { return };
        if !self.engine_has_head || self.verify.is_some() {
            return;
        }
        let (pseq, proot) = (self.engine.published_seq(), self.engine.published_root());
        if (seq, root) == (pseq, proot) || seq < pseq {
            return;
        }
        if self.owed.as_ref().is_some_and(|o| o.record.is_some() && o.seq == seq) {
            self.on_read_back(h);
            return;
        }
        if let Some(bytes) = self.my_winning_record(seq, &root) {
            self.send(Waiting::Update, Op::Update { state: bytes });
            return;
        }
        self.owed = None;
        self.step(Event::HeadConflict { seq, root });
    }

    /// The node said the head register changed (`HeadChanged`, a HINT a node
    /// can fabricate or drop): read it. ALWAYS — owed parity, an owed head
    /// or idle alike (the architect's #5): only a verify in progress, which
    /// reads the register itself, makes it redundant.
    pub fn head_hint(&mut self) {
        if self.verify.is_none() && !self.deadlines.contains_key(&Waiting::Hint) {
            self.send(Waiting::Hint, Op::ReadHead);
        }
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

    /// THIS page's own record at `seq`, if it is another root than `root` and
    /// WINS the tie-break against it: the bytes to land. A head that loses to
    /// such a record is never adopted — the register will hold mine once my
    /// UPDATE merges (the architect's attack on sdk#225, case 1).
    fn my_winning_record(&self, seq: u64, root: &Cid) -> Option<Vec<u8>> {
        let (mine_root, bytes) = self.my_records.get(&seq)?;
        if mine_root == root {
            return None;
        }
        // Theirs by its whole value, as just read (a bare root if it came some
        // other way); mine by the value the signer signed.
        let theirs = self.last_head.as_ref().filter(|h| h.seq == seq && h.root() == *root).map_or_else(|| root.to_vec(), |h| h.value().to_vec());
        let mine = HeadRead::from_record(bytes)?;
        beats(mine.value(), &theirs).then(|| bytes.clone())
    }

    /// Is a register read already in flight?
    fn reading_head(&self) -> bool {
        [Waiting::Warm, Waiting::RecoverHead, Waiting::Verify, Waiting::ReadBack, Waiting::Hint]
            .iter()
            .any(|w| self.deadlines.contains_key(w))
    }

    /// The register read back after an UPDATE: the only way a commit is
    /// Published (F56: the UPDATE's answer says nothing).
    fn on_read_back(&mut self, h: Option<(u64, Cid)>) {
        let Some(owed) = self.owed.as_mut() else { return };
        if owed.record.is_none() {
            return; // a read-back outlived its commit
        }
        let want = (owed.seq, owed.root);
        let mine_wins = matches!(h, Some((s, r)) if s == want.0 && self.my_winning_record(s, &r).is_some());
        let Some(owed) = self.owed.as_mut() else { return };
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
            // THE SAME SEQ, ANOTHER ROOT, and THIS page's record there wins
            // the tie-break: the register will hold mine once my UPDATE
            // merges (the node has not merged it yet). Adopting theirs would
            // drop a head that is about to win (the architect's attack on
            // sdk#225, case 1). Read again; after HEAD_READS, UPDATE again.
            Some((seq, _)) if seq == want.0 && mine_wins => {
                owed.stale_reads += 1;
                if owed.stale_reads >= HEAD_READS {
                    owed.stale_reads = 0;
                    let state = owed.record.clone().expect("an UPDATE was sent");
                    self.send(Waiting::Update, Op::Update { state });
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
        let ledger = self.sign_ledger_of(prev_seq, prev_root, prev_seq + 1, root);
        self.send(Waiting::Sign, Op::Sign { id, prev_seq, prev_root, seq: prev_seq + 1, root, ledger });
    }

    fn ask_sign(&mut self) {
        let Some(o) = &self.owed else { return };
        let id = self.next_request;
        self.next_request = self.next_request.checked_add(1).unwrap_or(1);
        self.sign_id = Some(id);
        // The prev is the commit's BASE, never "whatever the engine
        // publishes now": signed after a foreign winner was adopted, that
        // would name the winner as prev of a root built without it, and the
        // winner's rows would be lost from every later head.
        let (prev_seq, prev_root, seq, root) = (o.seq - 1, o.base, o.seq, o.root);
        let ledger = self.sign_ledger_of(prev_seq, prev_root, seq, root);
        let op = Op::Sign { id, prev_seq, prev_root, seq, root, ledger };
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

    /// The register as this page last READ it (any head answer), whole —
    /// what a merge reads a winner's `prev` from (sdk#225b).
    pub fn last_read(&self) -> Option<&HeadRead> {
        self.last_head.as_ref()
    }

    /// The signer answering this page predates sdk#225's rule (a stale
    /// bundle: in page mode the signer's code ships with the page), and it
    /// refuses every sign until the register moves on. The host asks for the
    /// current version rather than letting the page sit.
    pub fn needs_signer_upgrade(&self) -> bool {
        self.old_signer_fork_at.is_some()
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
        let backstop = self.engine_has_head.then_some(self.last_head_at + HEAD_BACKSTOP_MS);
        deadlines.chain(sign).chain(held).chain(verify).chain(puts).chain(backstop).min().map(Ms)
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
        value(&root, &Ledger { prev, through, ..Ledger::default() })[32..].to_vec()
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

    /// What the engine publishes now, as a head.
    fn engine_published(&self) -> (u64, Cid) {
        (self.engine.published_seq(), self.engine.published_root())
    }

    /// An owed head is LIVE only while it is ahead of what the engine has
    /// published. Once the engine adopts another head (a conflict, a recovery
    /// read), the commit that owed it is dead — its writes were told `Lost` —
    /// and nothing more is asked or sent for it. Without this the page went on
    /// asking the signer for a dead commit from a stale prev (the model found
    /// it: `NotSuccessor`, hidden behind the retries that got round it).
    fn engine_published(&self) -> (u64, Cid) {
        (self.engine.published_seq(), self.engine.published_root())
    }

    fn drop_dead_head(&mut self) {
        // Also dead: a head whose commit was built on a root the engine no
        // longer publishes (a foreign winner adopted under it).
        let published = self.engine_published();
        if self.owed.as_ref().is_some_and(|o| o.seq <= published.0 || (o.seq - 1, o.base) != published) {
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
                // A queued write's warm-apply block (R-b): kept for reads of
                // the warm root, never put -- its commit puts the same bytes.
                Effect::Keep { id, ref bytes } => self.blocks.insert(id, bytes),
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
                    // A head whose commit was built on a root the engine no
                    // longer publishes is DEAD: a foreign winner was adopted
                    // under it. The engine re-derives the commit on the new
                    // root and emits a new head.
                    Effect::UpdateHead { seq, base, .. } if (seq - 1, base) != self.engine_published() => {}
                    Effect::UpdateHead { seq, root, base, .. } => {
                        self.owed = Some(Owed { seq, root, base, record: None, stale_reads: 0 });
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

    /// Is anything still owed an answer or a re-send — an op in flight, a
    /// backed-off retry? `false` means this page is at rest until something
    /// new arrives.
    pub fn waiting(&self) -> bool {
        !self.deadlines.is_empty() || !self.put_again.is_empty() || self.sign_again.is_some() || !self.held_again.is_empty()
    }

    /// Ops to send, in order.
    /// PUT a contract the app names, by key: sent now, re-sent on the RTO,
    /// and ended by [`APP_PUT_BUDGET_MS`] if nothing answers. Asking again
    /// for a key that ENDED starts it over; one still pending is left alone.
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

    /// Writes queued and their bytes (what `QueueFull` measures).
    pub fn queue_load(&self) -> (usize, usize) {
        self.engine.queue_load()
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
/// `NotNext`, or a refusal the sign verb gives; not `Putting`, `Put`, `Held`,
/// `Provisioned`, `Register`, or a refusal only PUT-WITH-CODE or Provision gives.
fn answers_a_sign(a: &signer_proto::Answer) -> bool {
    use signer_proto::{Answer as A, Why};
    match a {
        A::Signed(_) | A::AlreadySigned(_) | A::NotNext { .. } => true,
        A::Refused(w) => !matches!(
            w,
            Why::BlockCount { .. } | Why::NotABlock { .. } | Why::KeyAlreadyProvisioned | Why::RegisterChanged
        ),
        A::Provisioned | A::Putting { .. } | A::Put { .. } | A::Held { .. } | A::Register { .. } => false,
    }
}
