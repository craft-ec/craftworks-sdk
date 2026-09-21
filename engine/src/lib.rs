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
//! | `Accepted` | applied to the tree, not yet shipped. Survives a tab close, NOT a node restart. |
//! | `Stalled` | the commit carrying it cannot publish, and is not moving. Non-terminal, reported once. |
//! | `Published` | packs read back, THEN the head read back. Survives a restart, and what a UI may call "saved". |
//! | `ParityComplete` | the redundancy the new nodes promise actually exists. |
//! | `Busy` | TERMINAL, and nothing was applied: a commit was already in flight, or a cap was reached. The client still holds the write and may re-submit it. |
//! | `Failed` | TERMINAL. The edit is NOT in the tree and never will be, so a re-submit applies it once. Never reported for a write whose edit IS in the tree. |
//! | `Lost` | TERMINAL. The commit carrying it will not publish and the engine no longer has it; the client is the only thing that does. |
//!
//! Between `Published` and `ParityComplete` the tree is correct and its groups
//! have no redundancy. That is *absent redundancy, not an error*
//! (ARCHITECTURE §7): a keeper's repair and the writer's late put are the same
//! bytes under the same id, so nothing has to be flagged, only done.
//!
//! # THE RULE FOR ADDING ANYTHING HERE
//!
//! **Every call is a rehydration. State that only matters ACROSS calls has to
//! be IN the context, or it does not exist.**
//!
//! This engine reads as an ordinary long-lived state machine, and in its own
//! tests it is one — a `Vec<Effect>` is driven to completion in a single
//! process. In production it never is. A delegate gets a fresh linear memory
//! on every `process()` call (F32); the engine is rebuilt from
//! [`Engine::from_context_or_new`] and thrown away at the end of the call,
//! and the only thing that survives is what [`Engine::to_context`] wrote.
//!
//! So a field is not "state the engine keeps". It is state the engine keeps
//! *for the rest of this call*, unless the context carries it. The failure
//! mode is silent by construction: the field holds its default at the top of
//! every call, the code reads it, takes the early return, and reports
//! nothing.
//!
//! Four defects of exactly this shape have been found, and all four were
//! invisible to every test in `engine/tests/` because those tests drive one
//! process:
//!
//! * `in_flight_since` (sdk#81) — the stall timer. `None` at the top of every
//!   call but the one that started the commit, so `Stalled` could never be
//!   reported at all;
//! * `told_stalled` (sdk#81) — which writes had been told. Without it the
//!   notice repeats on every tick, which is noise a caller learns to ignore;
//! * `in_flight_parity` (sdk#83) — the map from a parity block to its group.
//!   The ack arrives in a LATER call, so no group ever settled and the same
//!   three blocks went out on every tick for ever;
//! * the delegate scheduler's `confirmed` set (sdk#83) — not engine state,
//!   but the same rule one layer out. It learns what the node holds from
//!   acks in THIS call, so a `PutParity` gated `after: [published_root]` was
//!   held on a dependency nothing would confirm, and dropped. Every call.
//!
//! The question to ask of any new field, effect dependency or deadline:
//! **what does this hold at the top of a call that did not create it, and is
//! that the right answer?** If the honest answer is "whatever it defaults
//! to", either carry it — and bump `CONTEXT_VERSION` — or derive it from
//! something already carried, which is better: two copies of one fact can
//! disagree, and a derived one cannot.
//!
//! And an EMISSION is not a fact (sdk#150). An effect the engine returns can
//! be held and dropped by the scheduler, cut by a per-return limit, or never
//! answered; "I asked, therefore it happened" made each of those a permanent
//! loss. What is owed is derived from answers; `asks` only paces re-asking.
//!
//! And the test has to span calls. An assertion that passes on an engine
//! driven straight through proves nothing about this.

use freenet_prolly::apply::{apply_with, ApplyError, Edit as TreeEdit, Options as ApplyOptions};
use freenet_prolly::chunk::empty_leaf;
use freenet_prolly::node::Node;
use freenet_prolly::store::{Blocks, ReadError};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

pub mod asks;
pub mod pack;
pub mod read;
pub mod subs;

/// Which client a write came from. Two tabs are two clients.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ClientId(pub u64);

/// The client's own id for a write. Echoed in every state change, so a caller
/// never has to guess which of its writes a notification is about.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct WriteId(pub u64);

/// What a client asked for. Mirrors the tree's own edit vocabulary; the engine
/// never looks inside a value.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Op {
    Put(Vec<u8>),
    Delete,
}

/// How far along a write is.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum State {
    Accepted,
    /// Still held, not yet saved, and the engine cannot publish it right now.
    ///
    /// NON-TERMINAL, and reported once. The write is still tracked, its edit
    /// is still in the tree, and it reaches `Published` normally when the
    /// commit blocking it confirms. It tells a caller that "saving..." has
    /// stopped being routine -- not that anything has been given up.
    ///
    /// It exists because reporting `Failed` here was a FALSE statement: the
    /// edit stayed in the tree and published with the next commit, so a
    /// client that re-submitted would apply it twice, and could overwrite a
    /// newer write someone else made in between.
    Stalled,
    Published,
    ParityComplete,
    /// TERMINAL: this edit is not in the tree and never will be.
    Failed,
    /// TERMINAL: this engine has never heard of that write.
    ///
    /// After a restart, what the head does not name was never published and
    /// is gone. A client holding the id in its outbox is TOLD so, and
    /// re-submits. Silence would leave it waiting on a write nobody will ever
    /// finish -- the one outcome a client cannot recover from.
    Lost,
    /// TERMINAL: refused without being accepted, for now -- a commit is in
    /// flight, or a cold write was declined while its blocks are fetched.
    /// Re-sending later can succeed.
    Busy,
    /// TERMINAL: refused without being accepted, and NEVER acceptable as it
    /// is: it is over `limit` of `bound` by `got` (craftworks-sdk#136). Unlike
    /// `Busy`, re-sending it is refused the same way every time.
    TooLarge {
        bound: WriteBound,
        limit: usize,
        got: usize,
    },
}

/// Which bound a [`State::TooLarge`] write is over.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum WriteBound {
    /// Distinct blocks one commit would write ([`Params::max_commit_blocks`]).
    CommitBlocks,
    /// Bytes in one write ([`Params::max_write_bytes`]).
    WriteBytes,
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
    /// The client has gone. Ship what is waiting; there will be no more ticks.
    ///
    /// The shell sends this on disconnect. `Tick` comes only from a connected
    /// client (W4: the node fires no wake-ups), so nothing the page-closed
    /// promise depends on may need one — a commit in flight advances on its
    /// confirmations, and everything else that a tick would have got round to
    /// happens here instead.
    Flush,

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
    /// What changed in this range since the root the reader last saw?
    ///
    /// **No subscription is involved, and that is the point.** A delta is a
    /// capability of the TREE — equal ids mean equal subtrees, so the
    /// difference between two roots costs the changes and not the size — and
    /// it is available to any reader holding a root, live or not. An ordinary
    /// reload uses it: the client remembers the root it last read at and asks
    /// only for what moved.
    ///
    /// Being TOLD that something changed is the separate, declared thing. It
    /// triggers this; it is not required for it.
    ChangesSince {
        client: ClientId,
        req_id: read::ReqId,
        /// The root the reader is standing on.
        from: Cid,
        range: subs::SubRange,
        max_entries: usize,
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

    // ---- recovery ----
    /// A new engine value, holding only its device key and the epoch table.
    ///
    /// Where the authority came from is STATED here, never inferred: an
    /// engine that invented its own would be a second writer for the same
    /// device (sdk#14).
    Start {
        key: KeySource,
        /// Code epochs to try, newest first. An engine's own head may still
        /// sit under the previous epoch after an upgrade.
        epochs: Vec<Epoch>,
    },
    HeadRead {
        epoch: Epoch,
        seq: u64,
        root: Cid,
    },
    /// No head under any epoch: a brand-new device, empty tree, seq 0.
    HeadMissing,
    /// Another engine holds this device key and is ahead. The loser re-reads
    /// and rebases; it never publishes a fork.
    HeadConflict {
        seq: u64,
        root: Cid,
    },
    /// A client asking after a write it still holds in its outbox.
    AskWrite {
        client: ClientId,
        write_id: WriteId,
    },

    // ---- subscriptions ----
    /// Tell me when anything between these keys changes.
    SubscribeRange {
        client: ClientId,
        sub_id: u64,
        range: subs::SubRange,
    },
    Unsubscribe {
        client: ClientId,
        sub_id: u64,
    },
    /// This client is gone. Everything it was watching goes with it.
    ///
    /// Separate from `Flush`, which is about shipping work: a tab that closed
    /// still has writes worth publishing, and none worth notifying.
    ClientGone {
        client: ClientId,
    },
}

/// Which code epoch a head was written under.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Epoch(pub u32);

/// Where this engine's authority came from.
///
/// Stated at `Start` and never inferred. The engine does not mint authority:
/// it is handed some, or it has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum KeySource {
    SecretStore,
    Reissued,
    /// Handed in from outside and kept in the secret store, unmodified.
    ///
    /// The delegate derives NOTHING: it is given a key and it uses that key.
    /// Where a real device key comes from is sdk#14's, and this is the
    /// parameter that lets it slot in without the shell changing — the shell
    /// only ever knows that it was handed one.
    ///
    /// Today the only provisioner is a test harness, which generates a fresh
    /// key per run and removes it afterwards. `Test` says so IN THE TYPE, so
    /// a key that is not for real use cannot be mistaken for one that is by
    /// anything reading this.
    Provisioned(Provisioned),
}

/// Who provisioned a key, and whether it is real.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Provisioned {
    /// Generated for one run by a driver, and removed at the end of it.
    /// Never read from disk, never a real key.
    Test,
}

/// A group's three parity ids. The unit redundancy comes in: three blocks are
/// one group's protection and are worth nothing separately.
pub type ParityIds = [Cid; 3];

/// A parity group's three blocks, as `(id, bytes)`.
type ParityBlocks = Vec<(Cid, Vec<u8>)>;

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

    // ---- recovery ----
    /// Read this device's own head, under this epoch.
    ReadHead { epoch: Epoch },

    // ---- subscriptions ----
    /// A subscribe was taken, or refused and WHY.
    ///
    /// Answered either way. A client that believes it is subscribed and is
    /// not waits for ever for a notification nobody is going to send, and
    /// there is nothing in what it can see that would tell it so.
    Subscribed {
        client: ClientId,
        sub_id: u64,
        accepted: subs::Accepted,
    },
    /// Something in a subscribed range changed.
    ///
    /// **The new root ONLY.** No key list, no previous root. The CLIENT owns
    /// "the last root I saw" and passes it to `ChangesSince`, and that is what
    /// makes a dropped notification self-healing: the next one, or the
    /// client's own backstop, diffs across the whole gap in a single call. An
    /// engine that carried the previous root would be asserting where the
    /// client stood, and it does not know — the client may have missed three.
    ///
    /// Delivery is lossy by construction (the node's notification channel
    /// drops when full, and a subscription can be evicted without telling
    /// anyone), so "the client missed some" is the ordinary case rather than
    /// the exceptional one, and the design has to be right for it.
    Changed {
        client: ClientId,
        sub_id: u64,
        new_root: Cid,
        seq: u64,
        why: subs::Why,
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
    /// Whether a commit ships a PACK as well as its blocks.
    ///
    /// FALSE in Phase 3, and the reason is arithmetic rather than taste: a
    /// pack is transient, so the network holds pack + members either way and
    /// the pack only moves who pays. With no keepers, that is the writer
    /// both times.
    ///
    /// Kept as a parameter, not deleted, because Phase 4 turns it back on
    /// with the other half in place — keepers putting the members, and reads
    /// resolving through the head's packs.
    pub pack_on_write: bool,
    /// The largest ONE write, in bytes (keys plus values).
    ///
    /// Named for what it bounds. It was `max_backlog`, but a write is refused
    /// `Busy` whenever a commit is pending or a write is parked, so by the time
    /// this is checked there is no backlog at all: it bounds a single write
    /// (craftworks-sdk#136), and a write over it is `TooLarge`, not `Busy`.
    pub max_write_bytes: usize,
    /// Ticks after which an owed group's parity is put even while its members
    /// keep changing, so a hot group cannot stay unprotected for ever.
    pub parity_age: u64,
    /// Ticks an ask goes unanswered before it is asked again (sdk#150).
    ///
    /// An ask is not a fact: the effect may have been stranded, cut by a
    /// return's limit or never answered, and nothing reports that. So an
    /// unanswered ask is re-made after this long. Every ask is idempotent,
    /// so a slow answer costs one more put, never a wrong state.
    pub reask_after: u64,
    /// The most asks outstanding at once. Unit: one ask, at most
    /// `asks::ASK_BYTES` of context. Default: one return's worth (`max_puts`
    /// 128 + `max_gets` 4). A full table defers a NEW ask -- what it asks for
    /// stays owed and goes when an answer frees a place -- and never drops
    /// one already made. Checked at construction against
    /// `asks::CONTEXT_SHARE` of `max_context_bytes`.
    pub max_asks: usize,
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
    ///
    /// And never MORE than one return carries (the shell's `max_gets`,
    /// refused at the shell's construction otherwise): what a return cannot
    /// carry is dropped at the end of the call and nothing asks for it again,
    /// so a read that needed more than that in one round never came back
    /// (sdk#150; live on two private nodes: 0 of 60 rows, 0 of 300).
    pub max_fetch_per_round: usize,
    /// How many times a block is asked for before the read is answered
    /// `Unavailable`. Attempts are RE-ISSUED, not waited on (ARCHITECTURE §7).
    pub max_attempts: u32,
    /// Fetch rounds one read may make before it is answered `Unavailable`.
    ///
    /// Bounds the read itself, which `max_attempts` cannot: that counts a
    /// BLOCK's failures, and a node that serves a block and then evicts it
    /// produces an endless series of successes. Generous enough that a deep
    /// tree on a slow node finishes, small enough that a read cannot spin for
    /// a whole delegate slice.
    pub max_read_rounds: u32,
    /// Two requests needing one block share its fetch. Off = the control.
    pub share_fetches: bool,
    /// Ceilings on an advisory preload: roots, blocks, bytes. A preload may
    /// make reads SOONER, never more or wider, so a hostile manifest costs
    /// the budget and not what it asked for.
    pub preload_roots: usize,
    pub preload_blocks: usize,
    pub preload_bytes: usize,
    /// Ask again for blocks the node already holds.
    ///
    /// Kept, but it is NOT a control for the cost bound and must not be sold
    /// as one: fetches come from `Need`, and `Need` only ever names blocks
    /// that are missing, so there is nothing already-held to re-ask for. It
    /// measured 3 fetches against a bound of 4 — inert.
    pub refetch_held: bool,
    /// Remember which blocks are already requested, across calls.
    ///
    /// Carried in the context, and it does suppress a duplicate fetch when two
    /// READS want the same block. It is NOT what stops a waiting read
    /// re-asking on every entry — nothing re-descends a parked read except
    /// its own block arriving — so it is not a control for the cost bound,
    /// and measuring it as one gives the same number either way.
    pub dedupe_in_flight: bool,
    /// Re-drive every parked read on every entry.
    ///
    /// The plausible wrong implementation, and the honest control for "a cold
    /// lookup costs the same however often the engine is entered": it is the
    /// SIMPLER thing to write — resume everything and let each read work out
    /// whether it can progress — and it makes the cost grow with how busy the
    /// node is rather than with the depth of the tree.
    pub redescend_on_entry: bool,
    /// On a miss, also ask for every child of every branch already readable.
    ///
    /// A plausible "fetch ahead" a reader might write, and the control that
    /// keeps the depth bound honest: it asks for a level where the descent
    /// needs one block.
    pub fetch_ahead: bool,
    /// The context budget. The platform caps a delegate's context at exactly
    /// 400 KiB (`DelegateContext::MAX_SIZE` = 4096*10*10), so this sits below
    /// it with headroom: exceeding the platform's cap is a refusal the engine
    /// never sees coming, and exceeding its own is a `Busy` it can report.
    pub max_context_bytes: usize,
    /// Count the nodes a descent would touch. It is a second walk, for the
    /// cost gate and nothing else, so it is off unless a test asks.
    pub count_descent: bool,
    /// Ticks a write may sit merely ACCEPTED — in memory, in no commit —
    /// before the engine stops promising anything about it.
    ///
    /// `accepted` survives a refresh and not a restart, so it is exposure,
    /// and exposure has to be bounded rather than journalled: only the head
    /// makes anything findable, and a write not yet in a commit is not on its
    /// way to a head. One commit is in flight at a time, so a write arriving
    /// behind a commit that cannot publish would otherwise wait for ever.
    /// At T it is reported `Failed`, which is a reply the SDK can re-submit
    /// from its outbox — unlike silence.
    pub max_accept_age: u64,
    /// Off = the control: writes fold for ever behind a stuck commit.
    pub bound_accept_age: bool,
    /// Whether the context carries the commit in flight.
    ///
    /// Always true in production. It exists as a CONTROL: a both-modes test
    /// asserts Live and Rehydrate agree, and an assertion that two runs agree
    /// is worth nothing until something can make them disagree. Turning this
    /// off leaves one field out of the context, which is exactly the defect
    /// class the comparison is there to catch, and the control asserts the
    /// run really does diverge.
    pub context_carries_pending: bool,
    /// Rounds a write may spend waiting for a cold tree path before it is
    /// refused. Each round is one fetch-and-retry of the whole apply.
    pub max_apply_rounds: u32,
    /// Largest write that may be parked. Its ops ride in the context, so this
    /// is a slice of the 400 KiB the platform allows.
    pub max_parked_write_bytes: usize,
    /// How many reads may be waiting on the network at once.
    ///
    /// Each costs the context about 130 B, measured. Unbounded, ten thousand
    /// of them make a 1.3 MB context -- three times what the platform will
    /// store -- and `to_context` then fails, which loses the IN-FLIGHT COMMIT
    /// as well. A read burst must not be able to destroy a write, so reads
    /// get a slice of the budget and are refused at its edge.
    pub max_parked_reads: usize,
    /// Blocks one commit may name.
    ///
    /// TWO bounds meet here and they are one number.
    ///
    /// The context: a commit's bookkeeping carries a Cid per block it waits
    /// on, ~54 B each (measured), and `max_write_bytes` bounds a write in BYTES
    /// which says nothing about how many blocks those bytes become.
    ///
    /// The PLATFORM: a pack's bytes exist only in the return that made them
    /// (`Commit::packs` is `#[serde(skip)]`, deliberately — a pack is the
    /// largest thing the engine touches and the context is 400 KiB), so the
    /// shell cannot hold a put back for the next entry. Whatever a commit
    /// emits must go out in ONE `process()` return.
    ///
    /// 128 is the largest k MEASURED to work: k = 1..128 PUTs from one
    /// return, each acknowledged, zero errors, on a private network-mode
    /// node. The limit was NOT reached — 128 is where the search stopped,
    /// not where the node refused. F21's "8" was a probe's `--max-k` default
    /// that had been read as a platform constant; this is a measurement, and
    /// it stays a measurement rather than becoming the next inherited number.
    pub max_commit_blocks: usize,
    /// Blocks one call may read while recomputing owed parity.
    ///
    /// The context carries owed groups as IDS, so a rehydrated engine has to
    /// find the node that lists them before it can code anything — a walk of
    /// the tree, which is a READ and must be bounded like one. What it does
    /// not finish this call, it finishes on the next: the groups stay owed.
    pub max_parity_scan_blocks: usize,
    /// Emit the head as soon as the commit is planned, without waiting for
    /// its packs to be read back. The control for (c): a head that names a
    /// root whose blocks are not all there is a tree no reader can walk, and
    /// a crash in that window leaves one behind for ever.
    pub head_before_packs: bool,

    // ---- subscriptions ----
    /// Standing range subscriptions this engine will hold at once.
    ///
    /// They live in the 400 KiB context beside everything else, so this is a
    /// slice of the same budget the reads and the in-flight commit come out
    /// of. Over it a subscribe is REFUSED and the client told — a subscriber
    /// that thinks it is watching and is not waits for ever, and nothing it
    /// can see would tell it otherwise.
    pub max_subscriptions: usize,
    /// Standing subscriptions ONE client — one page load — may hold (sdk#146),
    /// so one page cannot take every other's room. Half the engine-wide cap:
    /// two live tabs fit side by side without either evicting the other.
    pub max_subscriptions_per_client: usize,
    /// The longest bound a subscription may carry.
    ///
    /// The bounds are client-chosen byte strings. Without this one
    /// subscription is an allocation whose size the sender picks — which is
    /// the thing the wire's `MAX_MESSAGE` exists to prevent, one layer in.
    pub max_sub_key: usize,
    /// Consecutive `BlockMissing` answers after which a subscribed range is
    /// declared `Stale` and stops being compared.
    ///
    /// A missing block on the OLD root is usually durable — superseded,
    /// unpinned, evicted — so a range in that state would otherwise notify on
    /// every commit for ever with a `why` the client can do nothing about. It
    /// is told once and then left quiet; a reload re-subscribes and resets it.
    /// A declared degradation beats an undeclared storm.
    pub stale_after_missing: u32,
    /// Changes one delta page may carry.
    ///
    /// A delta is a READ and is bounded like one. Unbounded, "what changed
    /// since the root I saw a week ago" is the whole tree, held in memory at
    /// once, in a delegate with 400 KiB of context and one 5-second call.
    pub max_delta_entries: usize,
    /// Answer every subscriber on every root move without comparing the
    /// trees.
    ///
    /// OFF. Two jobs. It is the CONTROL for the range filter — with it on, a
    /// commit that touched nothing a subscriber watches still notifies it, so
    /// a test asserting "no notification for an untouched range" has
    /// something that can make it fail. And it is the shape a DOWNGRADE takes:
    /// an engine under pressure that stops comparing still tells its
    /// subscribers, honestly, that it did not look.
    pub notify_without_diff: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            max_pack: 1024 * 1024,
            max_packed_value: 64 * 1024,
            pack_on_write: false,
            max_write_bytes: 8 * 1024 * 1024,
            parity_age: 32,
            reask_after: 16,
            max_asks: 132,
            coalesce_parity: true,
            whole_tree_supersede_scan: false,
            transfer_superseded_waiters: true,
            max_fetch_per_round: 4,
            max_attempts: 3,
            max_read_rounds: 64,
            share_fetches: true,
            preload_roots: 4,
            preload_blocks: 256,
            preload_bytes: 4 * 1024 * 1024,
            refetch_held: false,
            dedupe_in_flight: true,
            redescend_on_entry: false,
            fetch_ahead: false,
            max_context_bytes: 320 * 1024,
            count_descent: false,
            max_accept_age: 64,
            bound_accept_age: true,
            context_carries_pending: true,
            max_apply_rounds: 32,
            max_parked_write_bytes: 128 * 1024,
            max_parked_reads: 1000,
            max_commit_blocks: 128,
            max_parity_scan_blocks: 512,
            head_before_packs: false,
            stale_after_missing: 3,
            max_delta_entries: 256,
            max_subscriptions: 32,
            max_subscriptions_per_client: 16,
            max_sub_key: 256,
            notify_without_diff: false,
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
}

/// A write whose apply stopped on a block the node does not hold.
///
/// The tree is a persistent structure over content-addressed blocks, so an
/// apply reads the path it is about to rewrite. A delegate reads through a
/// SYNC host call that returns what the node happens to hold right now (F14),
/// and that call does not register demand (F33) -- so a path can simply be
/// cold, with nothing wrong and nothing to blame.
///
/// Reporting `Failed` there was a false statement about a transient miss: it
/// tells a client its edit will never be in the tree, when fetching one block
/// would have applied it. So the write is PARKED with its ops, the blocks are
/// fetched, and the apply runs again from the start -- it is a pure function
/// of (root, ops), so re-running it costs reads and cannot produce a
/// different tree.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ParkedWrite {
    client: ClientId,
    write_id: WriteId,
    /// The ops AS GIVEN. Not the sorted batch: the last-writer-wins collapse
    /// is part of the apply and is redone each round, so what is carried is
    /// the client's own request and nothing derived from it.
    ops: Vec<(Vec<u8>, Op)>,
    /// The root the apply started from. A write parked across a head change
    /// is stale: its edits belong to a tree that is no longer current.
    root: Cid,
    /// Rounds spent waiting. Bounded, because the node may simply not have
    /// the block: F14 reads what is held and registers no demand, so "fetch
    /// and try again" can be true for ever.
    rounds: u32,
    /// Blocks this write is waiting on, so an arrival for something else does
    /// not re-run the apply.
    needs: BTreeSet<Cid>,
    /// Bytes of the ops, for the backlog bound.
    bytes: usize,
}

/// A commit in flight: one apply, one head bump.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Commit {
    seq: u64,
    root: Cid,
    /// Pack and block ids this commit must land before its head may move.
    data: BTreeSet<Cid>,
    /// The pack bodies, for THIS call only.
    ///
    /// A pack is never carried in the context — it is the largest thing the
    /// engine touches and the context is 400 KiB. So a pack that fails within
    /// the call that made it is re-sent from here; one that fails after the
    /// engine has been re-hydrated cannot be, and the commit's writes are
    /// reported `Failed` instead. That is TRUE now in a way it was not
    /// before: the core holds no tree, so nothing of that write is anywhere.
    #[serde(skip)]
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
/// A block source that also answers for the EMPTY LEAF.
///
/// A brand-new device's tree is one empty leaf, and until its first commit is
/// published nothing on the network holds it. That is not a block the engine
/// "keeps": `empty_leaf()` is a pure function of the format, so knowing it is
/// knowing a constant, the same way the engine knows what a pack header looks
/// like. Everything else goes to the real source.
struct WithEmptyLeaf<'a, B: Blocks> {
    inner: &'a B,
    empty_cid: Cid,
    empty_bytes: &'a [u8],
    /// Blocks handed to the engine THIS CALL and not yet visible through
    /// `inner` (sdk#34).
    ///
    /// A `BlockArrived` event carries the block's bytes. Until now they were
    /// checked and dropped, because the node that served them also holds them,
    /// so the next call could read them back through `Blocks`. That is true of
    /// a node that RETAINS. A node is free not to — a sync read does not
    /// refresh hosting (F33) — and then the bytes the engine was handed are
    /// the only copy in existence, and it had thrown them away.
    ///
    /// So they are used for the rest of THIS call and then dropped. Nothing is
    /// stored: this map never reaches the context, and the next call starts
    /// without it.
    arrived: &'a BTreeMap<Cid, Vec<u8>>,
}

impl<B: Blocks> Blocks for WithEmptyLeaf<'_, B> {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        // The real source first: once the leaf is published the node's copy is
        // the one to use, and it is byte-identical anyway.
        self.inner
            .get(cid)
            .or_else(|| self.arrived.get(cid).map(|b| b.as_slice()))
            .or_else(|| {
                if *cid == self.empty_cid {
                    Some(self.empty_bytes)
                } else {
                    None
                }
            })
    }
}

pub struct Engine<B: Blocks> {
    /// The empty leaf, which a device with no head has as its root and which
    /// nothing on the network holds until the first commit publishes it.
    /// A constant of the format, not a cache.
    empty: freenet_prolly::chunk::Closed,
    /// Where blocks come from. The core owns NO block bytes: a delegate's
    /// memory is fresh on every call, so anything it "kept" would be gone by
    /// the next one. What it can do is ASK — and on a node that is the
    /// synchronous local read (F14), which sees what this node already holds.
    blocks: B,
    /// Blocks handed to this engine during the CURRENT call, and dropped when
    /// it ends (sdk#34). Never in the context — see [`WithEmptyLeaf::arrived`].
    arrived: BTreeMap<Cid, Vec<u8>>,
    params: Params,
    /// The warm tree: every block this engine has written. In slice 1 it is
    /// also the only place they exist, because there is no node yet.
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
    /// Parity blocks of owed groups the node has CONFIRMED -- the fact a
    /// group settles on, when all three are in. What was merely emitted is
    /// in `asks`, which paces and decides nothing.
    parity_confirmed: BTreeSet<Cid>,
    /// Unanswered asks, for pacing only (`asks`).
    asks: asks::Asks,
    /// The groups each write is still waiting on.
    ///
    /// A SET and not a count, because a superseded group is replaced by the
    /// groups that now cover the same members, and a count cannot express
    /// "swap one for two" without drifting. `ParityComplete` is a state of the
    /// WRITE — reported once, when this set empties.
    parity_waiting: BTreeMap<(ClientId, WriteId), BTreeSet<ParityIds>>,
    /// A write whose tree path is not held, waiting for the blocks it needs.
    ///
    /// At most ONE, and it costs the context its ops. That is affordable only
    /// because one commit at a time already means a second write is refused:
    /// a queue of parked writes would put an unbounded number of client values
    /// in a 400 KiB context.
    parked_write: Option<ParkedWrite>,
    /// When the commit now in flight was started.
    ///
    /// It used to be "when the oldest write not yet in a commit was
    /// accepted", back when writes folded behind an open commit. Under one
    /// commit at a time nothing waits outside a commit — a write arriving
    /// behind one is refused — so the write left sitting `Accepted` is the
    /// in-flight commit's own, and this is the clock for it.
    in_flight_since: Option<u64>,
    /// Writes already told they are stalled, so the notice is sent once.
    told_stalled: BTreeSet<(ClientId, WriteId)>,
    /// Where this engine's authority came from, as STATED at `Start`.
    key: Option<KeySource>,
    /// Epochs still to try when looking for this device's head.
    epochs: Vec<Epoch>,
    head_epoch: Option<Epoch>,
    recovered: bool,
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
    /// Who is watching which range.
    subs: subs::Subs,
    /// Nodes parsed on the write path. A cost counter, not a statistic: the
    /// whole point of the diff walk is that this stays proportional to the
    /// tree's DEPTH, and a test that does not measure it would not notice the
    /// day it goes back to being proportional to its SIZE.
    nodes_parsed: usize,
    now: u64,
}

impl<B: Blocks> Engine<B> {
    /// Anything a caller can set must not be able to panic the core later.
    /// `max_packed_value` above the limit describes a value that must ride in
    /// a pack and cannot; the planner would reach an `unreachable!` three steps
    /// away, where nothing points back at the setting that caused it. Refused
    /// here instead, naming both numbers.
    ///
    /// **The limit is the PER-MEMBER one, not the pack's.** This bounded
    /// `max_packed_value` against `max_pack` — the container — and packed
    /// members are `RAW`, so every setting in [262,209, 1,048,565] passed here
    /// and produced packs the contract refuses per member: a 786,357-wide band
    /// the engine accepted and the network would not. That is worse than a
    /// panic, because the refusal happens REMOTELY, where a node can log
    /// nothing at the moment it refuses (F48). Both numbers are needed: a
    /// member must fit its kind's limit AND the pack must be able to hold one
    /// (craftworks-sdk#117).
    pub fn new(params: Params, blocks: B) -> Self {
        assert!(
            params.max_packed_value <= pack::max_body(freenet_prolly::kind::RAW),
            "max_packed_value ({}) is above what the network accepts for one \
             member of its kind ({}): the pack would be built here and refused \
             THERE, per member, where nothing local says why",
            params.max_packed_value,
            pack::max_body(freenet_prolly::kind::RAW)
        );
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
        // The asks table is carried, so its worst case is part of the
        // context's. This bounds ITS share; whether every carried cap sums
        // under the bound is sdk#162's question, not answered here.
        assert!(
            params.max_asks.saturating_mul(asks::ASK_BYTES)
                <= params.max_context_bytes / asks::CONTEXT_SHARE,
            "max_asks ({}) x {} B is over 1/{} of max_context_bytes ({})",
            params.max_asks,
            asks::ASK_BYTES,
            asks::CONTEXT_SHARE,
            params.max_context_bytes
        );
        let empty = empty_leaf();
        let root = empty.cid;
        Engine {
            params,
            empty,
            blocks,
            arrived: BTreeMap::new(),
            root,
            published_seq: 0,
            published_root: root,
            next_seq: 1,
            pending: None,
            folded: Vec::new(),
            folded_bytes: 0,
            owed: BTreeMap::new(),
            parity_confirmed: BTreeSet::new(),
            asks: asks::Asks::default(),
            parity_waiting: BTreeMap::new(),
            in_flight_since: None,
            parked_write: None,
            told_stalled: BTreeSet::new(),
            key: None,
            epochs: Vec::new(),
            head_epoch: None,
            recovered: false,
            pending_notifications: Vec::new(),
            unpublished: Vec::new(),
            coded_since_commit: BTreeSet::new(),
            reads: read::Reads::default(),
            subs: subs::Subs::default(),
            nodes_parsed: 0,
            now: 0,
        }
    }

    /// The warm tree's current root, including writes not yet published.
    pub fn root(&self) -> Cid {
        self.root
    }

    /// The root other readers can see.
    /// The seq of the head this engine has published, or 0 if it has none.
    ///
    /// 0 means no head exists ANYWHERE for this key — not merely that this
    /// engine has not written one — because a fresh start re-reads the head
    /// before anything else and adopts whatever is there.
    pub fn published_seq(&self) -> u64 {
        self.published_seq
    }

    /// Blocks of the commit in flight that this engine has already seen
    /// confirmed ON THE NODE -- carried in the context, so true at the top of
    /// a call that did not see the confirmation (sdk#150). The head bump is
    /// emitted only once every one of them is here, naming them as `after`.
    pub fn confirmed_in_flight(&self) -> impl Iterator<Item = Cid> + '_ {
        self.pending
            .iter()
            .flat_map(|c| c.confirmed.iter().copied())
    }

    pub fn published_root(&self) -> Cid {
        self.published_root
    }

    /// Groups whose redundancy does not exist yet.
    /// The parameters this engine runs under.
    pub fn params(&self) -> &Params {
        &self.params
    }

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
        if self.params.redescend_on_entry {
            // The control: every entry re-drives every parked read, whether
            // or not anything it waits on has changed.
            let parked: Vec<read::ReqId> = self.reads.parked.keys().copied().collect();
            let mut out = Vec::new();
            for r in parked {
                out.extend(self.drive(r));
            }
            out.extend(self.step_inner(event));
            return out;
        }
        self.step_inner(event)
    }

    fn step_inner(&mut self, event: Event) -> Vec<Effect> {
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
            Event::Flush => self.on_flush(),
            Event::Get {
                client,
                req_id,
                key,
            } => self.on_read(client, req_id, read::Want::Get(key)),
            Event::Scan {
                client,
                req_id,
                range,
            } => self.on_read(
                client,
                req_id,
                read::Want::Scan(Box::new(range.as_ref().into())),
            ),
            Event::ChangesSince {
                client,
                req_id,
                from,
                range,
                max_entries,
            } => self.on_read(
                client,
                req_id,
                read::Want::Delta(Box::new(read::DeltaSpec {
                    from,
                    lo: range.lo,
                    hi: range.hi,
                    // Clamped here, where the engine's own appetite is
                    // decided. A caller asking for a million changes is
                    // asking the engine to hold a million changes.
                    max_entries: max_entries.clamp(1, self.params.max_delta_entries),
                })),
            ),
            Event::Preload { client, roots } => self.on_preload(client, roots),
            Event::SubscribeRange {
                client,
                sub_id,
                range,
            } => {
                let (accepted, evicted) = self.subs.add(
                    client,
                    sub_id,
                    range,
                    subs::SubLimits {
                        max_subs: self.params.max_subscriptions,
                        max_per_client: self.params.max_subscriptions_per_client,
                        max_key: self.params.max_sub_key,
                    },
                );
                // The oldest client's subscriptions are dropped UNTOLD, and on
                // purpose. A `Changed` names no session and is delivered to
                // whichever connection's call produced it — this one, the NEW
                // page's — so a `Stale` for the evicted ids would reach the
                // wrong tab, under sub ids it may hold itself (sdk#166: reads
                // and notifications are not yet sessioned). A victim that is
                // still live loses push and keeps its tick backstop — slower,
                // never wrong — and re-asserts its subscriptions on reconnect.
                // Almost always the victim is a page that reloaded away.
                let _ = evicted;
                vec![Effect::Subscribed {
                    client,
                    sub_id,
                    accepted,
                }]
            }
            Event::Unsubscribe { client, sub_id } => {
                self.subs.remove(client, sub_id);
                Vec::new()
            }
            Event::ClientGone { client } => {
                self.subs.drop_client(client);
                Vec::new()
            }
            Event::BlockArrived { id, bytes } => self.on_arrived(id, bytes),
            Event::BlockMissed(id) => self.on_missed(id),
            Event::Start { key, epochs } => self.on_start(key, epochs),
            Event::HeadRead { epoch, seq, root } => self.on_head_read(epoch, seq, root),
            Event::HeadMissing => self.on_head_missing(),
            Event::HeadConflict { seq, root } => self.on_head_conflict(seq, root),
            Event::AskWrite { client, write_id } => self.on_ask(client, write_id),
        }
    }

    /// A new engine value, holding only its device key.
    ///
    /// NOTHING IS REPLAYED. What the head does not name was never published:
    /// a pack's key is its content hash, so an engine that restarts with only
    /// its device key cannot enumerate packs its head does not name, and a
    /// separate journal would not remove the crash window — it would move it,
    /// at a PUT and a read-back on every commit. So the head is the journal,
    /// and recovery is reading it.
    fn on_start(&mut self, key: KeySource, epochs: Vec<Epoch>) -> Vec<Effect> {
        self.key = Some(key);
        self.epochs = epochs;
        self.recovered = false;
        // Newest epoch first: this engine's own head may still sit under the
        // previous one after an upgrade, and the first write after recovery
        // goes to the current one either way.
        match self.epochs.first().copied() {
            Some(epoch) => vec![Effect::ReadHead { epoch }],
            None => self.on_head_missing(),
        }
    }

    fn on_head_read(&mut self, epoch: Epoch, seq: u64, root: Cid) -> Vec<Effect> {
        self.head_epoch = Some(epoch);
        let changed = self.adopt(seq, root);
        self.recovered = true;
        // The tree is not walked here. Reads warm it lazily (slice 2), which
        // is also what makes a restart cheap: the engine is usable the moment
        // it knows its root.
        //
        // This is the path a SECOND client learns on: its own engine never
        // wrote anything, it re-read the head, and the root moved.
        changed
    }

    /// No head under the newest epoch: try the one before it, and only when
    /// they are exhausted is this a device that has never written.
    fn on_head_missing(&mut self) -> Vec<Effect> {
        if !self.epochs.is_empty() {
            self.epochs.remove(0);
        }
        if let Some(epoch) = self.epochs.first().copied() {
            return vec![Effect::ReadHead { epoch }];
        }
        // A device with no head starts from the empty tree. Its root is a
        // constant, not something to fetch.
        let changed = self.adopt(0, self.empty.cid);
        self.recovered = true;
        changed
    }

    /// Another engine holds this device key and is ahead.
    ///
    /// The Register refuses the lower or equal seq, so the loser here is the
    /// one that must give way: it takes the winner's head and rebases what it
    /// had accepted on top. It never publishes a fork — there is only ever one
    /// head, and the writes that were in flight are re-applied to the tree
    /// that won.
    fn on_head_conflict(&mut self, seq: u64, root: Cid) -> Vec<Effect> {
        let rebasing: Vec<(ClientId, WriteId)> = self
            .pending
            .take()
            .map(|c| c.writes)
            .into_iter()
            .flatten()
            .chain(std::mem::take(&mut self.folded))
            .collect();
        self.folded_bytes = 0;
        self.in_flight_since = None;
        self.unpublished.clear();
        self.coded_since_commit.clear();
        let mut out = self.adopt(seq, root);

        // The writes are not silently dropped and not silently re-applied:
        // their EDITS are gone with the commit that never published, so the
        // clients are told, and re-submit against the head that won.
        out.extend(
            rebasing
                .into_iter()
                .map(|(client, write_id)| Effect::Notify {
                    client,
                    write_id,
                    state: State::Lost,
                }),
        );
        out
    }

    /// A client asking after a write this engine has never heard of.
    fn on_ask(&mut self, client: ClientId, write_id: WriteId) -> Vec<Effect> {
        let known = self.folded.iter().any(|(_, w)| *w == write_id)
            || self
                .pending
                .as_ref()
                .is_some_and(|c| c.writes.iter().any(|(_, w)| *w == write_id))
            || self.parity_waiting.keys().any(|(_, w)| *w == write_id);
        if known {
            return Vec::new();
        }
        vec![Effect::Notify {
            client,
            write_id,
            state: State::Lost,
        }]
    }

    /// Take a root as the published one, and tell whoever was watching.
    ///
    /// The notification is INSIDE this rather than beside each caller. Every
    /// way the published root can move goes through here or through a commit
    /// landing, and a subscriber that heard about only some of them would
    /// miss exactly the changes another writer made — which is most of what a
    /// second tab is for. Making it structural means a fourth caller added
    /// later cannot forget.
    #[must_use = "a root move that tells nobody is a subscription that misses it"]
    fn adopt(&mut self, seq: u64, root: Cid) -> Vec<Effect> {
        let was = self.published_root;
        self.root = root;
        self.published_root = root;
        self.published_seq = seq;
        self.next_seq = seq + 1;
        self.notify_subs(was)
    }

    /// Tell every subscriber whose range a root move touched.
    ///
    /// Called wherever `published_root` moves, with the root it moved FROM.
    /// There are exactly two such places — this engine's own commit landing,
    /// and a head read adopting someone else's root — and a subscriber that
    /// only hears about the first would miss every change another writer
    /// made, which is most of what a second tab is for.
    ///
    /// `a == b` reads nothing, so calling this on a move that moved nothing
    /// is free rather than merely harmless.
    fn notify_subs(&mut self, from: Cid) -> Vec<Effect> {
        if self.subs.is_empty() {
            return Vec::new();
        }
        let to = self.published_root;
        let seq = self.published_seq;
        let stale_after = self.params.stale_after_missing;
        if self.params.notify_without_diff {
            // The CONTROL. Every subscriber hears about every move, with no
            // comparison at all — which is what the range filter is supposed
            // to improve on, and a filter nothing can make fail is not a
            // filter. `Unknown` is the honest `why`: nothing was compared.
            if from == to {
                return Vec::new();
            }
            return self
                .subs
                .touched_without_diff(&from, &to)
                .into_iter()
                .map(|(client, sub_id, why)| Effect::Changed {
                    client,
                    sub_id,
                    new_root: to,
                    seq,
                    why,
                })
                .collect();
        }
        // Through `source()`, not `blocks`, and the shell test is what found
        // it. A brand-new device's root is the EMPTY LEAF, which is a constant
        // of the format that nothing on the network holds until the first
        // commit publishes it — so a diff from it against the node's store
        // alone stops immediately on a block that is not missing at all. Every
        // subscriber then got a spurious `BlockMissing` on the very first
        // commit, on any range, including ranges the commit came nowhere near.
        //
        // The fields are disjoint, so this is a borrow split rather than a
        // clone: `source()` reads `blocks` and `empty`, `touched` mutates
        // `subs`.
        let Engine {
            subs,
            blocks,
            empty,
            ..
        } = self;
        let source = WithEmptyLeaf {
            inner: blocks,
            empty_cid: empty.cid,
            empty_bytes: &empty.bytes,
            arrived: &BTreeMap::new(),
        };
        subs.touched(&source, &from, &to, stale_after)
            .into_iter()
            .map(|(client, sub_id, why)| Effect::Changed {
                client,
                sub_id,
                new_root: to,
                seq,
                why,
            })
            .collect()
    }

    /// What a read that has GIVEN UP answers.
    ///
    /// One place, because there are four ways a read ends without an answer
    /// and a delta must come out of all of them saying the same thing. Its
    /// degraded answer is `FullReloadRequired` — a defined outcome the client
    /// acts on by doing a plain range read — rather than `Unavailable`, which
    /// is a failure the client would have to invent a recovery for. The old
    /// root's blocks being gone is ORDINARY for a reader that was away, so
    /// the path that handles it has to be the ordinary one.
    fn gave_up(&self, want: &read::Want, blocked: Cid) -> read::ReadResult {
        match want {
            read::Want::Delta(_) => read::ReadResult::FullReloadRequired {
                new_root: self.published_root,
            },
            _ => read::ReadResult::Unavailable(blocked),
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
                frontier: None,
                levels_done: 0,
                rounds: 0,
                held: BTreeSet::from([root]),
            },
        );
        let out = self.drive(req_id);
        // The cap is checked AFTER the attempt, not before it. A read the
        // node can already answer is parked and unparked within this call and
        // costs the context nothing, so refusing it at the door would refuse
        // the cheap case to protect a budget it never touches. What is
        // bounded is the reads still WAITING when the call ends.
        if self.reads.parked.len() > self.params.max_parked_reads
            && self.reads.parked.contains_key(&req_id)
        {
            let blocked = out
                .iter()
                .find_map(|f| match f {
                    Effect::FetchBlock { id, .. } => Some(*id),
                    _ => None,
                })
                .unwrap_or(root);
            let result = self
                .reads
                .parked
                .remove(&req_id)
                .map(|p| self.gave_up(&p.want, blocked))
                .unwrap_or(read::ReadResult::Unavailable(blocked));
            self.forget_waiting(req_id);
            // A reply, not silence. The caller can retry when the burst
            // clears; a read that is simply dropped leaves it waiting for an
            // answer that is never coming.
            return vec![Effect::Reply {
                client,
                req_id,
                result,
            }];
        }
        out
    }

    /// Advance one parked read as far as what is warm allows.
    fn drive(&mut self, req_id: read::ReqId) -> Vec<Effect> {
        let Some(p) = self.reads.parked.get(&req_id).cloned() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // The counter is taken OUT for the call: `source()` borrows the
        // engine, and a cost counter is not worth an interior-mutability cell.
        let mut parsed = self.nodes_parsed;
        // Continue from the deepest node this read reached, not from the root
        // (sdk#34). A read that starts over needs every upper level again, so
        // a node evicting what it serves can make it impossible rather than
        // merely slow.
        let start = p.frontier.unwrap_or(p.root);
        let outcome = {
            let source = self.source();
            read::attempt(&source, &self.params, &p.want, &start, &mut parsed)
        };
        self.nodes_parsed = parsed;
        // Only a missing CHILD is a resume point; see `Attempt::NeedFrom`.
        // Read from a borrow, so the two Need shapes can share one arm below
        // rather than needing a branch that cannot happen.
        let advance_to = match &outcome {
            read::Attempt::NeedFrom { node, .. } => Some(*node),
            _ => None,
        };
        match outcome {
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
                let result = self.gave_up(&p.want, cid);
                self.reads.parked.remove(&req_id);
                self.forget_waiting(req_id);
                out.push(Effect::Reply {
                    client: p.client,
                    req_id,
                    result,
                });
            }
            read::Attempt::Need(ids) | read::Attempt::NeedFrom { ids, .. } => {
                // A round that asks for something is a round: if the read has
                // made too many without finishing, it ENDS. The node may be
                // evicting what it serves faster than the descent can use it,
                // and a read that cannot finish must say so rather than spin.
                let over = {
                    let q = self.reads.parked.get_mut(&req_id).expect("parked");
                    q.levels_done += 1;
                    q.rounds += 1;
                    if let Some(node) = advance_to {
                        q.frontier = Some(node);
                    }
                    q.rounds > self.params.max_read_rounds
                };
                if over {
                    let blocked = ids.first().copied().unwrap_or(p.root);
                    let result = self.gave_up(&p.want, blocked);
                    self.reads.parked.remove(&req_id);
                    self.forget_waiting(req_id);
                    return vec![Effect::Reply {
                        client: p.client,
                        req_id,
                        result,
                    }];
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
                // Never ask for what the node already holds. `Page::need` can
                // name blocks an earlier round brought in, and a reader that
                // re-fetches them makes no progress at all.
                //
                // `refetch_held` is the control for the cost bound: with it on
                // the engine asks again for everything, which is a plausible
                // implementation and blows the bound many times over. The old
                // control enumerated the whole warm set, which a core that
                // owns no blocks cannot do.
                if !self.params.refetch_held {
                    ids.retain(|id| self.blocks.get(id).is_none());
                }
                let cap = self.params.max_fetch_per_round;
                if self.params.fetch_ahead {
                    // Everything the readable branches name, whether or not
                    // this descent needs it.
                    let mut ahead: Vec<Cid> = Vec::new();
                    let mut stack = vec![p.root];
                    let mut seen: BTreeSet<Cid> = BTreeSet::new();
                    while let Some(cid) = stack.pop() {
                        if !seen.insert(cid) || ahead.len() > 128 {
                            continue;
                        }
                        let Some(bytes) = self.blocks.get(&cid) else {
                            continue;
                        };
                        let Ok(n) = Node::parse(bytes) else { continue };
                        if !n.is_leaf() {
                            for i in 0..n.len() {
                                let c = n.child(i).0;
                                ahead.push(c);
                                stack.push(c);
                            }
                        }
                    }
                    ids.extend(ahead);
                    // The ahead ids go through the same already-held filter as
                    // the rest: without it the control re-asks for what just
                    // arrived and never settles, which is a hang rather than a
                    // cost and measures nothing.
                    if !self.params.refetch_held {
                        ids.retain(|id| self.blocks.get(id).is_none());
                    }
                }
                for id in ids.into_iter().take(cap) {
                    // The waiter is ALWAYS recorded — that is what wakes the
                    // read when the block lands, and skipping it would make
                    // the control a hang rather than a cost. What the control
                    // turns off is only the SUPPRESSION: with dedupe off the
                    // fetch is emitted again every time the engine re-descends
                    // and finds the same block missing.
                    let first = self.reads.want(id, req_id, self.params.share_fetches);
                    if first || !self.params.dedupe_in_flight {
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
        // ONE COMMIT AT A TIME. A write arriving while a commit is in flight
        // is REFUSED, not folded.
        //
        // Folding needed the write's blocks to survive until the next commit,
        // and there is nowhere to put them: the core keeps no blocks and the
        // context must not carry a pack. Buffering belongs to the client,
        // which has a page and an outbox; the delegate has neither. `Busy`
        // says so, and leaves no trace — a write both applied and refused is
        // the worst of both.
        if self.pending.is_some() || self.parked_write.is_some() {
            return vec![Effect::Notify {
                client,
                write_id,
                state: State::Busy,
            }];
        }
        // Refused BEFORE it is applied, for the same reason. Nothing is
        // pending and nothing is parked here (the `Busy` above), and `folded`
        // is empty at every point an event can observe, so there is no
        // backlog: this is one write against its own bound, and a write over
        // it is over it every time -- `TooLarge`, never `Busy`.
        debug_assert_eq!(self.backlog(), 0);
        if size > self.params.max_write_bytes {
            return vec![Effect::Notify {
                client,
                write_id,
                state: State::TooLarge {
                    bound: WriteBound::WriteBytes,
                    limit: self.params.max_write_bytes,
                    got: size,
                },
            }];
        }

        self.apply_write(client, write_id, ops, size)
    }

    /// Apply a write to the tree, parking it if the path is not held.
    ///
    /// Called from `on_write` and again from `resume_parked_write` on every
    /// round. Nothing is retained between rounds but the ops, because the
    /// apply is a pure function of (root, ops): re-running it reads blocks
    /// again and produces the same tree.
    fn apply_write(
        &mut self,
        client: ClientId,
        write_id: WriteId,
        ops: Vec<(Vec<u8>, Op)>,
        size: usize,
    ) -> Vec<Effect> {
        // The tree takes a SET in key order; the client wrote a SEQUENCE. Two
        // ops on one key in a batch are the client changing its mind, so the
        // LAST wins — the SDK's rule, and the one a caller who wrote `put`
        // then `delete` expects. A sort-then-dedup keeps the FIRST of each
        // run instead, which is the same ops in the same order producing a
        // different tree; the differential against a from-scratch rebuild is
        // what caught it.
        let batch: Vec<(Vec<u8>, TreeEdit)> = ops
            .iter()
            .map(|(k, o)| {
                (
                    k.clone(),
                    match o {
                        Op::Put(v) => TreeEdit::Put(v.clone()),
                        Op::Delete => TreeEdit::Delete,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();

        let old_root = self.root;
        let mut emitted: Vec<(Cid, Vec<u8>)> = Vec::new();
        let source = WithEmptyLeaf {
            inner: &self.blocks,
            empty_cid: self.empty.cid,
            empty_bytes: &self.empty.bytes,
            arrived: &self.arrived,
        };
        let applied = match apply_with(
            ApplyOptions::default(),
            &source,
            &self.root,
            &batch,
            |c, b: &[u8]| emitted.push((c, b.to_vec())),
        ) {
            Ok(a) => a,
            // The path is not held. Nothing is wrong: park the write, fetch
            // what it asked for, and run the apply again when it arrives.
            Err(ApplyError::Read(ReadError::Need(need))) => {
                return self.park_write(client, write_id, ops, size, need)
            }
            // The SDK screens keys and values before a write reaches here, so
            // a refusal means something bypassed it. It is reported, not
            // hidden, and nothing is applied. `Failed` is the honest word
            // here and only here: the edit is NOT in the tree, so a client
            // that re-submits is not applying it twice.
            Err(_) => {
                self.parked_write = None;
                return vec![Effect::Notify {
                    client,
                    write_id,
                    state: State::Failed,
                }];
            }
        };
        // Refused AFTER the apply and BEFORE anything is kept. The apply is a
        // pure function that wrote to a local sink, so discarding it costs
        // the work and nothing else — and the count is not knowable before
        // running it. Nothing below this line has happened yet, so the write
        // leaves no trace, which is what `Busy` promises.
        if emitted.len() > self.params.max_commit_blocks {
            self.parked_write = None;
            // `emitted` counts DISTINCT blocks -- a function of the tree and
            // the contents, not of how many ops the write had -- and it is the
            // same on every re-send against the same tree. So: `TooLarge`,
            // with the count, never `Busy`, which the outbox re-sends for ever.
            return vec![Effect::Notify {
                client,
                write_id,
                state: State::TooLarge {
                    bound: WriteBound::CommitBlocks,
                    limit: self.params.max_commit_blocks,
                    got: emitted.len(),
                },
            }];
        }
        // It applied, so it is no longer parked.
        self.parked_write = None;
        // A WRITE THAT CHANGES NOTHING IS NOT A COMMIT (sdk#160). The tree it
        // asks for IS the published tree, so it is published already. As a
        // commit it had no blocks, so no confirmation could ever arrive to
        // bump the head, and the commit never ended: Accepted, then Stalled,
        // and every later write Busy for ever. `Db::define` writes the same
        // schema on every open, so this was every second open of an app.
        //
        // Both conditions: no blocks AND the published root. A write that
        // changes the tree always emits its new root, so "no blocks" alone
        // would also hold for nothing else today -- but the published root is
        // the statement that makes answering `Published` TRUE, so it is the
        // one checked.
        if emitted.is_empty() && applied.root == self.published_root && self.unpublished.is_empty()
        {
            self.root = applied.root;
            let mut told = vec![State::Accepted, State::Published];
            // ParityComplete only if it is TRUE. The tree is the published
            // one, but its redundancy may still be owed -- and a write that
            // re-sends after a lost `Published`, this shortcut's main
            // customer, is exactly one whose groups ARE still owed. So it
            // waits on the groups owed now and hears it when they settle, as
            // a write covered by a superseded group does.
            let owed: BTreeSet<ParityIds> = self.owed.keys().copied().collect();
            if owed.is_empty() {
                told.push(State::ParityComplete);
            } else {
                self.parity_waiting.insert((client, write_id), owed);
            }
            return told
                .into_iter()
                .map(|state| Effect::Notify {
                    client,
                    write_id,
                    state,
                })
                .collect();
        }
        // The blocks are NOT kept. A delegate's memory is fresh on every call,
        // so anything retained here is gone by the next one; they are emitted
        // in this same step and read back from the node afterwards.
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
        // `folded` is a handoff, not a queue: nothing reached here unless
        // `pending` was none, so `start_commit` drains it in this same call
        // and it is empty at every point an event can observe.
        debug_assert!(self.pending.is_none());
        let to_ship = self.take_unpublished();
        out.extend(self.start_commit(to_ship));
        out
    }

    /// Park a write whose path is cold, and ask for what it needs.
    ///
    /// The bound is on ROUNDS, not on failures: an arrival that lets the
    /// apply get one level deeper and stop again is a SUCCESS, and a bound
    /// that counts only misses would never stop a descent that makes progress
    /// for ever. A tree this engine wrote is bounded in depth, but the root
    /// it was handed is not necessarily one it wrote.
    fn park_write(
        &mut self,
        client: ClientId,
        write_id: WriteId,
        ops: Vec<(Vec<u8>, Op)>,
        size: usize,
        need: Vec<Cid>,
    ) -> Vec<Effect> {
        let rounds = self.parked_write.as_ref().map_or(0, |p| p.rounds) + 1;
        if rounds > self.params.max_apply_rounds {
            // Nothing was applied, so `Failed` is true: the client still has
            // the write and re-submitting it applies it once.
            self.parked_write = None;
            return vec![Effect::Notify {
                client,
                write_id,
                state: State::Failed,
            }];
        }
        // The ops ride in the context until the write applies. A batch too
        // big to carry is refused now rather than parked and lost at the next
        // call, when `to_context` would be the thing that failed.
        //
        // Measured on the SERIALIZED ops, not on `size`. `size` is the payload
        // -- keys plus values -- and a batch of 130,000 one-byte keys has a
        // payload of 128 KiB and a serialized cost of megabytes, so a payload
        // cap would have let exactly the state through that it exists to
        // refuse. Serializing here is the park path, which is rare.
        let parked_cost = bincode::serialized_size(&ops).unwrap_or(u64::MAX) as usize;
        let needs: BTreeSet<Cid> = need.iter().copied().collect();
        if parked_cost > self.params.max_parked_write_bytes {
            self.parked_write = None;
            // DECLINE TO PARK, BUT STILL FETCH. Answering `Busy` without the
            // fetches refused the one thing that would make this write
            // acceptable: a cold write too big to carry stayed cold, and was
            // `Busy` for ever (craftworks-sdk#136, measured). With the blocks
            // fetched the re-send finds them warm, applies without parking,
            // and `Busy` is TRUE -- which is why this is not `TooLarge`: the
            // same write succeeds on a warm node.
            let mut out: Vec<Effect> = needs
                .iter()
                .map(|id| Effect::FetchBlock {
                    id: *id,
                    via: read::Via::Direct,
                    attempt: rounds,
                })
                .collect();
            out.push(Effect::Notify {
                client,
                write_id,
                state: State::Busy,
            });
            return out;
        }
        let out = needs
            .iter()
            .map(|id| Effect::FetchBlock {
                id: *id,
                via: read::Via::Direct,
                attempt: rounds,
            })
            .collect();
        self.parked_write = Some(ParkedWrite {
            client,
            write_id,
            ops,
            root: self.root,
            rounds,
            needs,
            bytes: size,
        });
        out
    }

    /// A block arrived: if the parked write was waiting on it, apply again.
    ///
    /// Only on a block it actually asked for. An engine that re-ran the apply
    /// on every arrival would redo the whole descent for each of a commit's
    /// own confirmations, and the cost of a cold write would be the number of
    /// blocks moving on the node rather than the depth of the tree.
    fn resume_parked_write(&mut self, id: Cid) -> Vec<Effect> {
        let Some(p) = self.parked_write.as_mut() else {
            return Vec::new();
        };
        if !p.needs.remove(&id) {
            return Vec::new();
        }
        if !p.needs.is_empty() {
            // Still waiting on the rest of this round's blocks. Running now
            // would spend a round to stop at the very next one.
            return Vec::new();
        }
        let p = self.parked_write.as_ref().expect("checked").clone();
        if p.root != self.root {
            // The tree moved under it. Its edits were computed against a root
            // that is no longer current, and applying them now would be a
            // write to a tree the client never saw.
            self.parked_write = None;
            return vec![Effect::Notify {
                client: p.client,
                write_id: p.write_id,
                state: State::Failed,
            }];
        }
        self.apply_write(p.client, p.write_id, p.ops, p.bytes)
    }

    fn backlog(&self) -> usize {
        self.folded_bytes
            + self.pending.as_ref().map_or(0, |c| c.bytes)
            + self.parked_write.as_ref().map_or(0, |p| p.bytes)
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
            // Never drop one already asked for or partly confirmed: its
            // answers are coming, and forgetting it would lose the
            // ParityComplete it owes.
            .filter(|(key, _)| !self.parity_touched(key) && gone.contains(&key[0]))
            .map(|(key, _)| *key)
            .collect();
        // What now covers the members those groups held.
        let replacements: BTreeSet<ParityIds> = by_group.keys().copied().collect();
        for key in dropped {
            self.forget_group(&key);
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
        self.in_flight_since = Some(self.now);
        // In a commit now, so no longer stalled: if it stalls again later that
        // is a new fact and deserves a new notice.
        for w in &writes {
            self.told_stalled.remove(w);
        }

        // PHASE 3 WRITES MEMBERS, NOT PACKS.
        //
        // A pack is a transport and it is inherently TRANSIENT: a head names
        // at most a few, and one leaves the network as soon as it is
        // unpacked. So the total is always pack PLUS members — a pack only
        // changes WHO pays for the members, and with no keepers in Phase 3
        // the writer pays for them anyway. Sending both cost ~188 KiB per
        // commit for a pack nothing reads.
        //
        // What makes members-only need no new read path is F35: a delegate's
        // own PUT is HOSTED, so a member it wrote is readable at its own key.
        //
        // The pack FORMAT stays — kind, pack.rs, its vectors — because Phase
        // 4 (#39) is where it earns its place: the writer puts the pack only,
        // keepers put the members, and reads resolve through the head's
        // packs. This is the write path, not the format.
        let (packable, direct): (Vec<_>, Vec<_>) = if self.params.pack_on_write {
            emitted
                .into_iter()
                .partition(|(_, b)| b.len() <= self.params.max_packed_value)
        } else {
            (Vec::new(), emitted)
        };

        let manifest = pack::Manifest {
            prev_seq: self.published_seq,
            prev_root: self.published_root,
            seq,
            root: self.root,
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
        // journal entry is the point, not the payload. With packing off the
        // write path there is no pack at all — the head IS the journal, and
        // a manifest nobody will read is bytes nobody should pay for.
        if self.params.pack_on_write {
            packs.push(current);
        }

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
        let head_early = self.params.head_before_packs;
        let mut out = Vec::new();
        // A parity block landing: the group it belongs to is that much closer
        // to having redundancy.
        //
        // The group is found among the OWED groups, which are their own ids,
        // so nothing maps a parity block to its group but the fact itself.
        if let Some(group) = self.owed_group_of(&id) {
            self.asks.settled(&asks::Ask::Parity(id));
            self.parity_confirmed.insert(id);
            if group.iter().all(|m| self.parity_confirmed.contains(m)) {
                self.forget_group(&group);
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
        if c.head_sent || (!head_early && c.confirmed.len() < c.data.len()) {
            return out;
        }
        // Every block of this commit has been READ BACK from our own node.
        // Only now may the head move: a head naming a root whose blocks are
        // not all there is a tree readers cannot walk.
        //
        // No state is reported here. `Durable` used to be, and it promised a
        // recovery nobody can perform: a pack's key is its content hash, so a
        // restarted engine holding only its device key cannot enumerate packs
        // its head does not name. Only the HEAD makes anything findable, so
        // the head is the journal and `published` is the first state that
        // survives a restart.
        c.head_sent = true;
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
        if let Some(group) = self.owed_group_of(&id) {
            if self.parity_confirmed.contains(&id) {
                return Vec::new();
            }
            let bytes = self
                .owed
                .get(&group)
                .and_then(|o| o.blocks.iter().find(|(c, _)| *c == id))
                .map(|(_, b)| b.clone());
            if let Some(bytes) = bytes {
                self.asks.asked(asks::Ask::Parity(id), self.now);
                return vec![Effect::PutParity {
                    group,
                    id,
                    bytes,
                    after: vec![self.published_root],
                }];
            }
            // The bytes are not on hand. A rehydrated engine carries owed
            // groups as IDS, so this is the ordinary case, not an edge one:
            // a parity put that fails in a later call than the one that sent
            // it lands here every time.
            //
            // The failure is an answer: the ask is settled, so it is due at
            // once, and the next `emit_parity` recomputes the group's blocks
            // and puts this one again.
            self.asks.settled(&asks::Ask::Parity(id));
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
        let was = self.published_root;
        self.published_seq = c.seq;
        self.published_root = c.root;
        // ONE per commit: this runs where the head is confirmed, which
        // happens once per commit, rather than per write or per block.
        out.extend(self.notify_subs(was));
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
        // Nothing arrived while this commit was in flight: under one commit
        // at a time such a write was refused, not held. There is no follow-on
        // commit to start here.
        debug_assert!(self.folded.is_empty());
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

    /// The client is going away: do now what a tick would eventually do.
    ///
    /// There is no clock. `Tick` arrives only from a connected client, so
    /// once the tab closes nothing else will ever ask the engine to get on
    /// with it — and everything the page-closed promise covers must therefore
    /// be event-driven or happen HERE.
    ///
    /// Two things. Start a commit for anything applied but not yet shipped,
    /// and put ALL owed parity regardless of age: coalescing trades a little
    /// redundancy-latency for fewer puts while someone is watching, and there
    /// is nothing left to coalesce WITH once they are gone. A commit already
    /// in flight is left alone; it advances on its own confirmations, which
    /// is exactly the part that does not need a clock.
    fn on_flush(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.pending.is_none() && !self.unpublished.is_empty() {
            let to_ship = self.take_unpublished();
            out.extend(self.start_commit(to_ship));
        }
        // Every group, whatever its age and whether or not it settled.
        out.extend(self.emit_parity(|_| true));
        out
    }

    fn on_tick(&mut self, now: u64) -> Vec<Effect> {
        // THE FIRST CLOCK THIS ENGINE SEES. `now` is 0 until a tick arrives, so
        // a commit started before any tick recorded `in_flight_since = 0` --
        // and against an epoch-seconds tick that is 56 years old, which told
        // every write `Stalled` on the first tick (sdk#150, W4). A start taken
        // before there was a clock is anchored to the first clock, not
        // measured from zero. A real tick is never 0, so 0 means "unknown".
        if self.now == 0 {
            if let Some(since) = self.in_flight_since.as_mut() {
                if *since == 0 {
                    *since = now;
                }
            }
            self.asks.anchor(now);
            // A group first owed before any clock is dated from the first,
            // or its age bound would fire at once.
            for o in self.owed.values_mut() {
                if o.since == 0 {
                    o.since = now;
                }
            }
        }
        self.now = now;
        let mut out = self.age_out_accepted(now);
        if !self.params.coalesce_parity {
            return out;
        }
        let age = self.params.parity_age;
        // A group that did not change this tick has settled; one that keeps
        // changing goes out anyway once it has been unprotected long enough.
        out.extend(self.emit_parity(move |o: &Owed| {
            o.last_changed < now || now.saturating_sub(o.since) >= age
        }));
        out
    }

    /// Say so when a write has sat merely accepted too long.
    ///
    /// What this reports is a commit that cannot publish, with its own writes
    /// sitting `Accepted` and nothing moving.
    ///
    /// They are NOT dropped. Their edits are in the tree and publish when the
    /// commit confirms, so telling a client `Failed` would be a false
    /// statement with teeth: the client re-submits, the original publishes
    /// anyway, and a write someone else made in between is overwritten by the
    /// re-submission. `Stalled` says what is true -- still held, not saved,
    /// not moving. Memory stays bounded by NEW writes being refused with
    /// `Busy`, never by forgetting ones already accepted.
    /// # What the age is measured AGAINST
    ///
    /// A delegate has no clock. `now` is whatever the last `Tick` carried,
    /// and `in_flight_since` is the `now` of the tick current when the commit
    /// started. So "how long" is measured in the time the OUTSIDE has sent,
    /// and nothing else.
    ///
    /// If no tick has ever arrived, both are 0 and the difference is 0: a
    /// commit CANNOT BE STALLED YET, however long it has really been sitting.
    /// That is the honest answer rather than a guess — the engine has not
    /// been told that any time has passed, and inventing some would make the
    /// report a fiction with teeth.
    fn age_out_accepted(&mut self, now: u64) -> Vec<Effect> {
        if !self.params.bound_accept_age {
            return Vec::new();
        }
        let Some(since) = self.in_flight_since else {
            return Vec::new();
        };
        if now.saturating_sub(since) < self.params.max_accept_age {
            return Vec::new();
        }
        let Some(commit) = self.pending.as_ref() else {
            return Vec::new();
        };
        // Once per write: a notice repeated every tick is noise a caller
        // learns to ignore, and this one matters.
        let writes = commit.writes.clone();
        let mut out = Vec::new();
        for w in writes {
            if self.told_stalled.insert(w) {
                out.push(Effect::Notify {
                    client: w.0,
                    write_id: w.1,
                    state: State::Stalled,
                });
            }
        }
        out
    }

    /// Recover the BYTES behind owed groups the context carried as ids only.
    ///
    /// Parity is a pure function of a group's members, so the same group
    /// gives the same three blocks under the same three ids whoever computes
    /// them. What the context cannot carry is which NODE lists a given trio,
    /// and that is found by walking the tree — so this is a READ, and it is
    /// bounded and resumable like one. Blocks it could not read are fetched
    /// and the groups stay owed; the next call picks up where this stopped.
    ///
    /// Without it a rehydrated engine marked every owed group sent and put
    /// nothing behind it. In production EVERY call is a rehydration, so that
    /// was not an edge case: it was all of them, and the redundancy the tree
    /// promised was silently never written.
    fn recompute_owed(&mut self, of: BTreeSet<ParityIds>) -> Vec<Effect> {
        let wanted: BTreeSet<ParityIds> = of
            .into_iter()
            .filter(|k| self.owed.get(k).is_some_and(|o| o.blocks.is_empty()))
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(ParityIds, ParityBlocks)> = Vec::new();
        let mut need: BTreeSet<Cid> = BTreeSet::new();
        let mut left = self.params.max_parity_scan_blocks;
        {
            let source = self.source();
            let mut stack = vec![self.root];
            let mut seen: BTreeSet<Cid> = BTreeSet::new();
            let mut still: BTreeSet<ParityIds> = wanted.clone();
            while let Some(cid) = stack.pop() {
                if still.is_empty() || left == 0 {
                    break;
                }
                if !seen.insert(cid) {
                    continue;
                }
                let Some(bytes) = source.get(&cid) else {
                    // The walk stopped here. Ask for it; the group stays owed
                    // and the next call resumes from a warmer tree.
                    need.insert(cid);
                    continue;
                };
                left -= 1;
                let Ok(node) = Node::parse(bytes) else {
                    continue;
                };
                // Does this node list any trio still wanted?
                let lists: Vec<Cid> = node.parity().collect();
                let here: BTreeSet<ParityIds> = lists
                    .chunks_exact(3)
                    .map(|t| [t[0], t[1], t[2]])
                    .filter(|k| still.contains(k))
                    .collect();
                if !here.is_empty() {
                    match freenet_prolly::parity::blocks_of(&node, &source) {
                        Some(all) => {
                            // Matched by the ids the coding PRODUCES, not by
                            // position: a group is identified by its three
                            // parity ids, and checking them makes the match
                            // self-verifying rather than order-dependent.
                            for trio in all.chunks_exact(3) {
                                let key: ParityIds = [trio[0].0, trio[1].0, trio[2].0];
                                if still.remove(&key) {
                                    found.push((key, trio.to_vec()));
                                }
                            }
                        }
                        None => {
                            // A member is not held. Parity over a member whose
                            // bytes nobody has is parity over nothing, so ask
                            // for the members and leave the group owed.
                            for (_, members) in freenet_prolly::parity::group_members(&node) {
                                for m in members {
                                    if source.get(&m).is_none() {
                                        need.insert(m);
                                    }
                                }
                            }
                        }
                    }
                }
                if !node.is_leaf() {
                    for i in 0..node.len() {
                        stack.push(node.child(i).0);
                    }
                }
            }
        }
        for (key, blocks) in found {
            if let Some(o) = self.owed.get_mut(&key) {
                o.blocks = blocks;
            }
        }
        need.into_iter()
            .map(|id| Effect::FetchBlock {
                id,
                via: read::Via::Direct,
                attempt: 1,
            })
            .collect()
    }

    fn emit_parity(&mut self, want: impl Fn(&Owed) -> bool) -> Vec<Effect> {
        // NOTHING FOR A COMMIT THAT IS NOT PUBLISHED (sdk#150). A parity put
        // goes out `after` the published root, and a group coded by the
        // commit in flight has members that root does not hold. For the FIRST
        // commit that root is the empty tree's, which nobody puts or
        // confirms: the puts were held and dropped with the call, and under
        // `sent` the first save's redundancy was lost for good. The group
        // goes out on the first emit after its commit publishes.
        //
        // (A `published_seq == 0` guard beside this was killed by no test --
        // this filter already covers the first commit -- so it is not here.)
        let unpublished: BTreeSet<ParityIds> = self
            .pending
            .as_ref()
            .map(|c| c.groups.clone())
            .unwrap_or_default();
        let (now, every) = (self.now, self.params.reask_after);
        // What is owed, from fact: a block not confirmed. Whether to ask for
        // it NOW is the pacing table's only question.
        let due: Vec<ParityIds> = self
            .owed
            .iter()
            .filter(|(k, o)| !unpublished.contains(*k) && want(o))
            .filter(|(k, _)| {
                k.iter().any(|id| {
                    !self.parity_confirmed.contains(id)
                        && self.asks.due(&asks::Ask::Parity(*id), now, every)
                })
            })
            .map(|(k, _)| *k)
            .collect();
        // A rehydrated engine owes groups it has no bytes for: recover the
        // ones about to be asked for.
        let mut out = self.recompute_owed(due.iter().copied().collect());
        for key in due {
            let blocks = self
                .owed
                .get(&key)
                .map(|o| o.blocks.clone())
                .unwrap_or_default();
            for (id, bytes) in blocks {
                let ask = asks::Ask::Parity(id);
                if self.parity_confirmed.contains(&id) || !self.asks.due(&ask, now, every) {
                    continue;
                }
                // Full: a NEW ask waits for a place; it stays owed.
                if self.asks.get(&ask).is_none() && self.asks.len() >= self.params.max_asks {
                    continue;
                }
                self.asks.asked(ask, now);
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

    /// The owed group a parity block belongs to. A group IS its three ids.
    fn owed_group_of(&self, id: &Cid) -> Option<ParityIds> {
        self.owed.keys().find(|k| k.contains(id)).copied()
    }

    /// Whether anything of this group has been asked for or confirmed.
    fn parity_touched(&self, key: &ParityIds) -> bool {
        key.iter().any(|id| {
            self.parity_confirmed.contains(id) || self.asks.get(&asks::Ask::Parity(*id)).is_some()
        })
    }

    /// A group stopped being owed: its facts and its asks go with it.
    fn forget_group(&mut self, key: &ParityIds) {
        self.owed.remove(key);
        for id in key {
            self.parity_confirmed.remove(id);
            self.asks.settled(&asks::Ask::Parity(*id));
        }
    }
}

fn kind_raw() -> u8 {
    freenet_prolly::kind::RAW
}

impl<B: Blocks> Engine<B> {
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
        //
        // The engine does not STORE any of it. The node holds what it fetched
        // (a GET populates its store), and the engine reads through `Blocks`
        // on the next call. All this does is check what arrived and wake
        // whoever was waiting — the checking still matters, because a block
        // that is not what was asked for must not be treated as an answer.
        if freenet_prolly::block_id(pack::PACK_KIND, &bytes) == id {
            for (mid, mbytes) in pack::members(&bytes) {
                if read::matches_id(&mid, &mbytes) {
                    self.reads.in_pack.insert(mid, id);
                    landed.push(mid);
                }
            }
        }

        // The bytes just handed over, usable for the rest of this call. A
        // node that evicts what it serves leaves these as the only copy, and
        // a read that must re-read them through `Blocks` next call cannot
        // finish at all (sdk#34).
        self.arrived.insert(id, bytes.clone());
        if freenet_prolly::block_id(pack::PACK_KIND, &bytes) == id {
            for (mid, mbytes) in pack::members(&bytes) {
                if read::matches_id(&mid, &mbytes) {
                    self.arrived.insert(mid, mbytes.to_vec());
                }
            }
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
        // A write parked on a cold path takes the same arrivals, including a
        // block that came inside a pack: the node holds a pack's members
        // under their own ids, so the apply can read them.
        for l in &landed {
            out.extend(self.resume_parked_write(*l));
        }
        // The call is over for these: the engine owns no block bytes across
        // calls, and this is what keeps that true.
        self.arrived.clear();
        out
    }

    /// An attempt ended without an answer.
    ///
    /// Rule 5: re-issued, not waited on. Only when the budget runs out does
    /// the read get an answer — `Unavailable`, which is a reply. A read that
    /// never answers is indistinguishable from a wedged node, and the caller
    /// can do nothing about either.
    fn on_missed(&mut self, id: Cid) -> Vec<Effect> {
        // A miss is the round the parked write spent. Re-ask rather than wait:
        // the same block may be held by the time the next call runs, and the
        // round bound is what stops this, not the node's willingness to answer.
        //
        // `parked_write` is deliberately NOT cleared first: `park_write` reads
        // the round count off it, so clearing would reset the count on every
        // miss and the bound would never be reached.
        let mut out = Vec::new();
        if let Some(p) = self.parked_write.clone() {
            if p.needs.contains(&id) {
                out.extend(self.park_write(
                    p.client,
                    p.write_id,
                    p.ops,
                    p.bytes,
                    p.needs.into_iter().collect(),
                ));
            }
        }
        // ...and the read path still gets the miss: one block can be the one a
        // parked READ is waiting on as well, and an early return would leave
        // that read waiting on an attempt that already came back.
        out.extend(self.on_missed_read(id));
        out
    }

    fn on_missed_read(&mut self, id: Cid) -> Vec<Effect> {
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
                let result = self.gave_up(&p.want, id);
                self.forget_waiting(req);
                out.push(Effect::Reply {
                    client: p.client,
                    req_id: req,
                    result,
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
        // WORK, NOT MISSES.
        //
        // This counted only blocks it had to FETCH, so a block the node
        // already held fell through to `Node::parse`, pushed its children,
        // and cost the budget nothing. On a tree the node holds, the budget
        // could never fire and the walk ran to completion — so the preload
        // did its most work in exactly the case where it had nothing to do,
        // and that case is the common one: every open after the first,
        // against the user's own tree.
        //
        // Measured before this: 8,972 nodes parsed against a budget of 256,
        // in ONE `process()` call, in a wasm delegate, for ZERO fetches
        // (sdk#120). A bound that only the cold path could reach was a bound
        // on the path that did not need one.
        //
        // Both a parse and a fetch are one unit of work now. The cold path is
        // unchanged — a miss still costs one and still stops at the guard.
        let mut work = 0usize;
        let mut bytes = 0usize;
        for root in roots.into_iter().take(self.params.preload_roots) {
            // Only over a root the session already holds: a preload is a hint
            // about what is already ours, not an invitation to fetch a
            // stranger's tree.
            if self.blocks.get(&root).is_none() {
                continue;
            }
            let mut stack = vec![root];
            while let Some(cid) = stack.pop() {
                if work >= self.params.preload_blocks || bytes >= self.params.preload_bytes {
                    return out;
                }
                let Some(b) = self.blocks.get(&cid) else {
                    // Not held: this is what a preload is FOR.
                    if self.reads.waiting.contains_key(&cid) {
                        continue;
                    }
                    work += 1;
                    self.reads.fetches += 1;
                    out.push(Effect::FetchBlock {
                        id: cid,
                        via: read::Via::Direct,
                        attempt: 0,
                    });
                    continue;
                };
                // A PARSE IS WORK, and its BYTES are the dimension that
                // actually costs: a few large nodes can be more work than
                // many small ones, which a count alone cannot see.
                // `preload_bytes` was declared, defaulted and read NOWHERE —
                // a parameter nobody reads is a claim, not a bound, and it
                // sat in a settings surface as though it constrained
                // something.
                work += 1;
                bytes += b.len();
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

    /// Start from a published root this engine did not write.
    ///
    /// What a cold reader has: a head, and nothing else. Only for tests — a
    /// real engine learns its root by reading its own head.
    pub fn adopt_root_for_test(&mut self, root: Cid) {
        self.root = root;
        self.published_root = root;
    }

    /// The block source this engine reads through.
    pub fn blocks(&self) -> &B {
        &self.blocks
    }

    /// What the engine reads through: the node, plus the one constant it can
    /// answer for itself.
    fn source(&self) -> WithEmptyLeaf<'_, B> {
        WithEmptyLeaf {
            inner: &self.blocks,
            empty_cid: self.empty.cid,
            empty_bytes: &self.empty.bytes,
            arrived: &self.arrived,
        }
    }

    /// Fetches emitted, for the cost gate.
    pub fn fetches(&self) -> usize {
        self.reads.fetches
    }
}

/// Everything that must survive a `process()` call.
///
/// The delegate's memory is fresh every time, so this is the ONLY thing the
/// engine carries forward — and it has 400 KiB to do it in (F32). So what is
/// here is bookkeeping, never payload: ids, sequence numbers, and what each
/// client is waiting on. No block bytes, and above all no PACK: a pack is the
/// largest thing the engine touches and would blow the budget on its own.
///
/// Versioned, because a delegate upgrade meets a context written by the
/// previous code. A context whose version is not understood is REFUSED, and
/// the engine starts from its head instead — which is always safe, because
/// the head is the journal.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Context {
    version: u16,
    published_seq: u64,
    published_root: Cid,
    root: Cid,
    next_seq: u64,
    pending: Option<Commit>,
    /// Groups whose parity is owed, by their ids. The BYTES are not here:
    /// parity is a pure function of a group's members, so a re-hydrated
    /// engine recomputes them from the node's blocks and puts the same bytes
    /// under the same ids. Idempotent by construction.
    /// With each group's ages, `(ids, last_changed, since)`: rebuilt as 0 on
    /// every rehydrate, they made a group's parity fire on the first tick
    /// after ANY call boundary, whatever `parity_age` said (sdk#150).
    owed_groups: Vec<(ParityIds, u64, u64)>,
    /// Parity blocks of owed groups the node CONFIRMED. A group settles when
    /// all three are in; the ack for each arrives in its own later call.
    parity_confirmed: Vec<Cid>,
    /// Asks not yet answered, and when each last went out: PACING only
    /// (`asks`). It replaced `in_flight_parity`, from which `Owed::sent` was
    /// re-derived on every call -- so an emission recorded as a fact was
    /// re-recorded by the context itself (sdk#150).
    asks: Vec<(asks::Ask, asks::Pace)>,
    parked: Vec<(read::ReqId, read::Parked)>,
    waiting: Vec<(Cid, Vec<read::ReqId>)>,
    attempts: Vec<(Cid, u32)>,
    parity_waiting: Vec<((ClientId, WriteId), Vec<ParityIds>)>,
    head_epoch: Option<Epoch>,
    /// The write waiting on a cold tree path, ops and all. It is the one
    /// place client bytes ride in the context, which is why there is at most
    /// one and why `max_parked_write_bytes` bounds it.
    parked_write: Option<ParkedWrite>,
    /// Standing range subscriptions. Bounded by `max_subscriptions` and
    /// `max_sub_key`, because this is a slice of the same 400 KiB.
    subs: subs::Subs,
    /// WHEN THE COMMIT IN FLIGHT STARTED, in the engine's own terms.
    ///
    /// Carried because a delegate is rebuilt from its context on every call
    /// (F32), and this is the state that measures HOW LONG something has
    /// been stuck. Left out, it was `None` at the top of every call but the
    /// one that started the commit — so `age_out_accepted` returned early
    /// every time and `Stalled` could never be reported at all. A stuck
    /// commit spans many calls by definition; that is the whole situation it
    /// is for.
    ///
    /// 8 bytes, and only while a commit is in flight.
    in_flight_since: Option<u64>,
    /// Which writes have already been told `Stalled`.
    ///
    /// Carried for the same reason, and it is why both had to move together:
    /// without it the notice would be repeated on every tick, which is noise
    /// a caller learns to ignore — and this one matters. It only has anything
    /// in it while a commit is stuck, and it is emptied when one publishes.
    told_stalled: Vec<(ClientId, WriteId)>,
    /// THE CLOCK the two fields above are measured against: the `now` of the
    /// last tick this engine saw.
    ///
    /// `in_flight_since` joined the context in sdk#81 and the clock it is
    /// compared with did not. So a write started in a call with no `Tick`
    /// took `in_flight_since = 0`, the next real tick was seconds since 1970,
    /// and every write that outlived one tick was told `Stalled` after about
    /// a second instead of `max_accept_age` (sdk#150, W4 in the cross-call
    /// matrix).
    ///
    /// It does NOT make owed parity coalesce across calls: `Owed`'s
    /// `last_changed` and `since` are rebuilt as 0 on every rehydrate, so on a
    /// real node a group's parity fires on the first tick after any call
    /// boundary, whatever `parity_age` says. That is what fires W4's parity
    /// mid-commit; it is sdk#150 PR 3's.
    now: u64,
}

/// The version this build writes. Bumped when the shape changes.
///
/// 6: `in_flight_parity` left it for `parity_confirmed` (the answers) and
/// `asks` (pacing): an emission is not a fact (sdk#150).
///
/// 5: the engine's clock joined it -- `now`, which `in_flight_since` and the
/// parity ages are measured against (sdk#150).
///
/// 4: the parity in flight joined it — without it a confirmed parity block
/// could not be attributed to its group, so no group ever settled and every
/// tick re-put the same three blocks for ever (sdk#83).
///
/// 3: the stall timer joined it — `in_flight_since` and `told_stalled`.
/// Without them `Stalled` could never be reported (sdk#81): the state that
/// measures how long a commit has been stuck did not survive the call.
///
/// 2: subscriptions joined the context. A v1 context decodes to a DIFFERENT
/// shape rather than failing — bincode reads the fields it was asked for —
/// so the version is what refuses it, and a refused context is a fresh start
/// rather than an engine in a state nobody chose.
const CONTEXT_VERSION: u16 = 6;

/// What a context this build wrote begins with.
///
/// The context comes back from OUTSIDE the engine -- from a node's cache, as
/// bytes, with no guarantee beyond their length. Anything that is not
/// byte-for-byte what this build wrote must be refused, and refusal is FREE
/// here: an engine with no context is correct, it starts from its head and
/// reports its in-flight writes `Lost`. A context that is merely PLAUSIBLE is
/// the dangerous one -- it carries the (seq, root) of a commit in flight, so
/// an engine re-hydrated from a damaged one can emit `UpdateHead` naming a
/// root nobody has.
const CONTEXT_MAGIC: [u8; 4] = *b"CWE1";

/// magic + version + checksum, before the encoded body.
const CONTEXT_HEADER: usize = 4 + 2 + 8;

/// The first 8 bytes of BLAKE3 over the encoded body.
///
/// Truncated because this defends against DAMAGE, not against an adversary
/// who can also rewrite the checksum: the node's context cache is not a trust
/// boundary the engine can police, and a full 32 bytes would buy nothing a
/// version check and a fresh start do not already give.
fn context_checksum(body: &[u8]) -> [u8; 8] {
    let h = blake3::hash(body);
    let mut out = [0u8; 8];
    out.copy_from_slice(&h.as_bytes()[..8]);
    out
}

/// Encoding options shared by both directions.
///
/// `with_fixint_encoding` because that is what `bincode::serialize` does and
/// the two must agree. `with_limit` makes the allocation bound STRUCTURAL: a
/// damaged length field cannot ask the decoder for more than the context
/// budget, whatever today's types happen to make reachable.
fn context_opts(limit: usize) -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(limit as u64)
}

/// Why a context could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextError {
    /// Not something this build can read — a different version, or not a
    /// context at all. Never a panic: the bytes come from outside.
    Unreadable,
    /// Bigger than the budget allows.
    TooLarge(usize),
}

impl<B: Blocks> Engine<B> {
    /// What this engine must carry to its next call.
    pub fn to_context(&self) -> Result<Vec<u8>, ContextError> {
        let c = Context {
            version: CONTEXT_VERSION,
            published_seq: self.published_seq,
            published_root: self.published_root,
            root: self.root,
            next_seq: self.next_seq,
            pending: if self.params.context_carries_pending {
                self.pending.clone()
            } else {
                None
            },
            owed_groups: self
                .owed
                .iter()
                .map(|(k, o)| (*k, o.last_changed, o.since))
                .collect(),
            parity_confirmed: self.parity_confirmed.iter().copied().collect(),
            asks: self.asks.to_vec(),
            parked: self
                .reads
                .parked
                .iter()
                .map(|(r, p)| (*r, p.clone()))
                .collect(),
            waiting: self
                .reads
                .waiting
                .iter()
                .map(|(c, r)| (*c, r.iter().copied().collect()))
                .collect(),
            attempts: self.reads.attempts.iter().map(|(c, n)| (*c, *n)).collect(),
            parity_waiting: self
                .parity_waiting
                .iter()
                .map(|(w, g)| (*w, g.iter().copied().collect()))
                .collect(),
            head_epoch: self.head_epoch,
            parked_write: self.parked_write.clone(),
            subs: self.subs.clone(),
            // Only meaningful alongside the commit itself: a timer for a
            // commit that was not carried would measure the age of nothing.
            in_flight_since: if self.params.context_carries_pending {
                self.in_flight_since
            } else {
                None
            },
            told_stalled: if self.params.context_carries_pending {
                self.told_stalled.iter().copied().collect()
            } else {
                Vec::new()
            },
            now: self.now,
        };
        use bincode::Options;
        let body = context_opts(self.params.max_context_bytes)
            .serialize(&c)
            .map_err(|_| ContextError::Unreadable)?;
        let total = CONTEXT_HEADER + body.len();
        if total > self.params.max_context_bytes {
            return Err(ContextError::TooLarge(total));
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&CONTEXT_MAGIC);
        out.extend_from_slice(&CONTEXT_VERSION.to_le_bytes());
        out.extend_from_slice(&context_checksum(&body));
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Rebuild an engine from what the last call carried.
    ///
    /// Refuses rather than panics: these bytes come from outside this call and
    /// may be from another version, truncated, or nothing to do with us. A
    /// refusal is not a disaster — the caller starts from `Start` and reads
    /// its head, which is the only authority anyway.
    /// Resume from a context, or start fresh if it cannot be used.
    ///
    /// The pair `from_context(...).unwrap_or_else(|_| Engine::new(...))` does
    /// not compile: `from_context` consumes `blocks`, so a caller cannot
    /// reach for them again on the error arm. That shape pushed one caller
    /// into an `unreachable!()`, which is a PANIC on a path a damaged context
    /// reaches — and a delegate that panics on input is one a malformed
    /// context can take down. So the fallback lives here, where the blocks
    /// are still in hand.
    ///
    /// The bool says which happened. A caller that wants to report "this
    /// engine started from nothing" needs it, and inferring it from a state
    /// that merely looks fresh would be a guess.
    pub fn from_context_or_new(bytes: &[u8], params: Params, blocks: B) -> (Self, bool) {
        match Self::read_context(bytes, params) {
            Some(c) => (Self::hydrate(c, params, blocks), true),
            None => (Engine::new(params, blocks), false),
        }
    }

    /// Read and VERIFY a context, without needing the blocks.
    ///
    /// Split out so a caller can fall back to a fresh engine without having
    /// already given its blocks away.
    fn read_context(bytes: &[u8], params: Params) -> Option<Context> {
        use bincode::Options;
        // Every check below happens BEFORE the decoder sees a byte of the
        // body. A decoder that refuses malformed input is not the same thing
        // as one that refuses input this build did not write: bincode read
        // 656 of 876 single-window corruptions as a perfectly good context
        // and handed back an engine in whatever state the damage described.
        if bytes.len() < CONTEXT_HEADER || bytes.len() > params.max_context_bytes {
            return None;
        }
        if bytes[..4] != CONTEXT_MAGIC {
            return None;
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != CONTEXT_VERSION {
            return None;
        }
        let body = &bytes[CONTEXT_HEADER..];
        if bytes[6..CONTEXT_HEADER] != context_checksum(body) {
            return None;
        }
        let c: Context = context_opts(params.max_context_bytes)
            .deserialize(body)
            .ok()?;
        // Kept as well as the header's: two independent statements of the
        // same fact cost two bytes and catch a build that changed the shape
        // without changing the constant.
        if c.version != CONTEXT_VERSION {
            return None;
        }
        Some(c)
    }

    /// Build an engine from a context already verified by `read_context`.
    fn hydrate(c: Context, params: Params, blocks: B) -> Self {
        let mut e = Engine::new(params, blocks);
        e.published_seq = c.published_seq;
        e.published_root = c.published_root;
        e.subs = c.subs;
        e.root = c.root;
        e.next_seq = c.next_seq;
        e.pending = c.pending;
        e.head_epoch = c.head_epoch;
        e.parked_write = c.parked_write;
        e.in_flight_since = c.in_flight_since;
        e.told_stalled = c.told_stalled.into_iter().collect();
        e.now = c.now;
        e.recovered = true;
        // The owed groups come back as ids with no bytes. They are recomputed
        // on demand from the node's blocks, which is sound because parity is a
        // pure function of its members: the same group gives the same three
        // blocks under the same three ids, whoever computes them.
        // Nothing about an ask is re-derived here: what was confirmed is
        // carried as the node said it, and what was asked only paces.
        for (key, last_changed, since) in c.owed_groups {
            e.owed.insert(
                key,
                Owed {
                    blocks: Vec::new(),
                    last_changed,
                    since,
                },
            );
        }
        e.parity_confirmed = c.parity_confirmed.into_iter().collect();
        e.asks = asks::Asks::from_vec(c.asks);
        for (r, p) in c.parked {
            e.reads.parked.insert(r, p);
        }
        for (cid, reqs) in c.waiting {
            e.reads.waiting.insert(cid, reqs.into_iter().collect());
        }
        for (cid, n) in c.attempts {
            e.reads.attempts.insert(cid, n);
        }
        for (w, gs) in c.parity_waiting {
            e.parity_waiting.insert(w, gs.into_iter().collect());
        }
        e
    }

    pub fn from_context(bytes: &[u8], params: Params, blocks: B) -> Result<Self, ContextError> {
        match Self::read_context(bytes, params) {
            Some(c) => Ok(Self::hydrate(c, params, blocks)),
            None => Err(ContextError::Unreadable),
        }
    }
}
