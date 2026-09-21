//! Translation: what the node says becomes an `Event`, and what the core
//! emits becomes a node operation.
//!
//! Deliberately free of `freenet-stdlib` types, so the rules below can be
//! tested with no wasm and no node. The `#[delegate]` entry point does
//! nothing but convert the platform's types into these and back.
//!
//! The rule that needs the most care is the READ-BACK (F22/F31): a
//! `PutContractResponse` is the node saying it accepted the request, NOT that
//! the bytes are stored anywhere durable. Treating it as confirmation makes
//! `Published` a promise the system has not kept, and the failure appears
//! only when someone comes back for data that is not there. So an ack issues
//! a GET, and only the GET coming back with the state produces
//! `PutConfirmed`.

use crate::schedule::{Limits, Op, Scheduler};
use bincode::Options;
use engine::read::Via;
use engine::{Effect, Engine, Event, KeySource, Params, State};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use protocol::Dropped;
use std::collections::BTreeMap;

/// What arrived this call.
#[derive(Debug)]
pub enum Inbound {
    /// An application message from a client. Opaque bytes.
    Client(Vec<u8>),
    /// A GET came back. `None` = the node does not hold it.
    GotState { id: Cid, bytes: Option<Vec<u8>> },
    /// A PUT was ACKNOWLEDGED. This is not a confirmation of anything.
    PutAcked { id: Cid, ok: bool },
    /// The head's own PUT was acknowledged.
    ///
    /// Not a confirmation, by the same rule as a block: the head is read back
    /// before the commit publishes. It arrives as a PutContractResponse like
    /// any other, and only the entry point can tell that the contract it
    /// names is the Register — so it says so here rather than leaving the
    /// shell to treat the head as a block it has never heard of.
    HeadAcked { ok: bool },
    /// The head Register was read, and holds this.
    ///
    /// The same read-back rule as a block: writing the head is acknowledged,
    /// and only READING it back at the expected seq makes the commit
    /// published. A seq that is not the one written means someone else's head
    /// is there — a conflict, not a confirmation.
    GotHead { seq: u64, root: Cid },
    /// The head Register holds nothing.
    NoHead,
    /// A PUT was acknowledged, named by its CONTRACT id, as the node names
    /// it. The shell finds the block it is about (`block_for_contract`).
    PutAckedContract { contract: Cid, ok: bool },
    /// A GET found nothing, named by its CONTRACT id. A HIT needs no such
    /// variant: the block id is computed from the state that came back.
    MissedContract { contract: Cid },
    /// Anything else the node hands a delegate.
    Other,
}

/// What leaves this call.
#[derive(Debug, Default)]
pub struct Outbound {
    pub ops: Vec<Op>,
    pub replies: Vec<Vec<u8>>,
    /// Blocks put and not yet read back, at the end of the call.
    pub awaiting: usize,
    /// Puts confirmed by reading them back.
    pub read_back_hits: usize,
    /// Effects the core returned this call, before scheduling.
    pub effects: usize,
    /// Puts dropped because the delegate has no contract code yet.
    ///
    /// Counted, not silent: a client that never sent `Install` would
    /// otherwise see its write accepted and then nothing at all.
    pub refused_no_code: usize,
    /// The highest protocol version a client spoke this call.
    ///
    /// Zero when no client message arrived — a tick, or a node answer — and a
    /// v2-only message is not sent then either. Silence is not consent to a
    /// format.
    pub client_version: u16,
    /// Bytes this call handed to the node in `Op::Put`.
    ///
    /// Counted where the delegate already knows them, which is the only place
    /// they can be known honestly. Six external instruments failed to infer a
    /// PUT's cost from outside (freenet-contracts#39): an interface carries
    /// the whole machine, a per-process counter carries the node's own
    /// traffic, and a rate counter's baseline varies by more than the signal.
    /// The lesson was that the counter belongs inside the thing being
    /// measured.
    ///
    /// It is NOT what the node then sends peer-to-peer — only the node can
    /// count that. It is what this delegate offered it.
    pub put_bytes: usize,
    /// Node operations issued this call, by kind, so a reader can tell a call
    /// that wrote from one that only looked.
    pub gets: usize,
    pub puts: usize,
    /// Effects still waiting at the end of the call, and therefore LOST.
    ///
    /// The scheduler does not survive the call — nothing in a delegate does
    /// — so anything it is still holding is gone, and the core will not emit
    /// it again. This must never be non-zero, and it is reported rather than
    /// asserted so the live run can see it too.
    pub stranded: usize,
    /// What was stranded and why, when `stranded` is non-zero (sdk#150).
    /// Structure only -- see `Scheduler::stranded_report`.
    pub stranded_detail: String,
    /// What the engine shed this call to keep its context saveable, and the
    /// parity it left uncoded at the owed cap (sdk#162). Counted so a
    /// refusal is never read as silence, and parity left to the scrub never
    /// as "coming".
    pub shed: engine::Shed,
    /// Counted, never a panic. Reported to the client so a format mismatch
    /// is visible rather than a silence it waits on for ever.
    pub dropped: Vec<Dropped>,
}

/// What the shell must remember between calls, beside the engine's own.
///
/// Only ids: 32 bytes each, no payload. The blocks a read-back is waiting on
/// are already on the node — that is the whole point of asking — so nothing
/// here needs to carry bytes.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ShellState {
    /// Put, acknowledged, and not yet read back, with the rounds spent.
    awaiting: Vec<(Cid, u32)>,
    /// A head written and not yet read back, with the root it named.
    head: Option<(u64, Cid)>,
    /// Whether this session asked for a call tree.
    #[serde(default)]
    tracing: bool,
    /// The operation the next call's steps belong to, unless that call names
    /// a new one.
    ///
    /// CARRIED, and it has to be: a delegate gets a fresh memory every call,
    /// and most of a write's life happens in calls driven by node responses
    /// with no client request to name them. Without this the head bump — the
    /// step whose absence is the commonest way a write silently stops — was
    /// attributed to nothing and dropped, which the trace test caught by
    /// asking for exactly that step.
    #[serde(default)]
    tracing_of: Option<protocol::TraceOf>,
}

/// The whole shell context: the engine's, and the shell's own.
#[derive(serde::Serialize, serde::Deserialize)]
struct Carried {
    engine: Vec<u8>,
    shell: ShellState,
}

/// The most rows one page may carry, whatever a caller asks for.
///
/// A page crosses a delegate's 5-second call and a client's socket, and a
/// caller is free to ask for more than either will bear. Clamping is not a
/// refusal — the cursor means the rest is one more call away — but it IS
/// reported, because a short page read as the end of a range silently
/// truncates whatever the caller was listing.
/// The page limit, from `protocol`, so the client that ASKS and the engine
/// that answers cannot hold two different numbers.
const MAX_PAGE_ENTRIES: usize = protocol::MAX_PAGE_ENTRIES as usize;

/// And a byte ceiling, because 256 large values is a different size from 256
/// small ones and only one of them fits.
const MAX_PAGE_BYTES: usize = 512 * 1024;

/// The most trace steps one call may emit. See `Shell::step`.
const MAX_STEPS: usize = 64;

/// How many times a put is asked back for before its ack is disbelieved.
///
/// A put is readable a short time after it is acknowledged, not instantly, so
/// the first empty answer means "not yet" far more often than "never".
const MAX_READ_BACK_ROUNDS: u32 = 12;

/// A write state as its WIRE tag, so a trace step carries the same number the
/// client would see in a `WriteState` rather than a second encoding of it.
fn state_tag(s: State) -> u64 {
    match s {
        State::Accepted => 0,
        State::Stalled => 1,
        State::Published => 2,
        State::ParityComplete => 3,
        State::Busy => 4,
        State::Failed => 5,
        State::Lost => 6,
        State::TooLarge { .. } => 7,
    }
}

/// A reply's bytes, never an empty frame.
///
/// A reply too large to encode (a page over `MAX_MESSAGE`) is answered with
/// the reason instead — `Dropped { TooLarge }`, a few bytes — because the
/// client is waiting on it and silence is the one answer it cannot act on
/// (craftworks-sdk#136: `encode_reply` used to return `[]` here).
pub(crate) fn reply_bytes(r: &protocol::Reply) -> Vec<u8> {
    protocol::encode_reply(r).unwrap_or_else(|reason| {
        protocol::encode_reply(&protocol::Reply::Dropped { reason }).expect(
            "a Dropped reply is a handful of bytes; `a_dropped_reply_always_encodes` pins it",
        )
    })
}

/// How many rows a read's answer carried.
fn rows_in(r: &engine::read::ReadResult) -> u64 {
    use engine::read::ReadResult as R;
    match r {
        R::Value(v) => v.is_some() as u64,
        R::Page { entries, .. } => entries.len() as u64,
        R::Delta { changes, .. } => changes.len() as u64,
        R::FullReloadRequired { .. } | R::Unavailable(_) | R::OutOfWarmSpace => 0,
    }
}

/// The engine may never ask for more in one round than one return carries.
///
/// What a return cannot carry is dropped when the call ends, and nothing
/// asks for it again: the read waits for ever. Measured live on two private
/// nodes (sdk#150's L3): the engine asked for 8, a return carried 4, and no
/// cold read of 60 or 300 rows ever answered. So the pairing is REFUSED, not
/// tolerated.
/// The shell's own worst case must fit the part of the context bound the
/// engine leaves it (sdk#162). Its read-backs are of blocks the engine is
/// waiting on (`awaiting` is pruned to that every call), so at most a
/// commit's data plus the parity asks out at once; each is measured by
/// encoding one, with the context's own options.
fn check_reserve(params: &engine::Params) {
    use bincode::Options;
    let n = |v: Result<u64, bincode::Error>| v.map_or(usize::MAX / 64, |n| n as usize);
    let read_back = n(ctx_opts().serialized_size(&([0u8; 32], 0u32)));
    let head = n(ctx_opts().serialized_size(&Some((0u64, [0u8; 32]))));
    let tracing = n(ctx_opts().serialized_size(&Some(protocol::TraceOf::Write(0))))
        + n(ctx_opts().serialized_size(&false));
    // The engine's bytes ride in a Vec: its length prefix, and the vec for
    // the read-backs.
    let wrapper = 2 * n(ctx_opts().serialized_size(&Vec::<u8>::new()));
    let worst = wrapper
        + (params.max_commit_blocks.saturating_add(params.max_asks)).saturating_mul(read_back)
        + head
        + tracing;
    assert!(
        worst <= params.shell_context_reserve,
        "the shell's worst case ({worst} B) is over shell_context_reserve ({} B): \
         the engine at its bound plus the shell would not save",
        params.shell_context_reserve
    );
}

fn check_limits(params: &engine::Params, limits: Limits) {
    assert!(
        params.max_fetch_per_round <= limits.max_gets,
        "max_fetch_per_round ({}) is over what one return carries (max_gets {}): \
         the rest would be dropped at the end of the call and never asked for again",
        params.max_fetch_per_round,
        limits.max_gets
    );
}

/// A client, as the engine keys it: the SESSION that sent the frame and the
/// VERSION it spoke, packed into the engine's opaque 64-bit id
/// (craftworks-sdk#146).
///
/// Every tab and page load was `ClientId(1)`, so the engine's `(client,
/// write)` keys collided by default. And the version is a property of the
/// WRITE, not of the shell: one version per shell was overwritten by
/// whichever client spoke last, so a v2 writer could be sent a v3-only
/// verdict and a v3 writer a v2 one. Packed, it travels wherever the engine
/// carries the client — a parked write, a commit's writes — and every
/// `Notify` is addressed from its own write. A session is one page load, so
/// its version cannot change under it.
fn as_client(session: u64, version: u16) -> engine::ClientId {
    let s = session & ((1u64 << protocol::SESSION_BITS) - 1);
    engine::ClientId((s << 16) | version as u64)
}
fn version_of(c: engine::ClientId) -> u16 {
    (c.0 & 0xFFFF) as u16
}
fn session_of(c: engine::ClientId) -> u64 {
    c.0 >> 16
}
fn as_write_id(n: u64) -> engine::WriteId {
    engine::WriteId(n)
}
fn as_req_id(n: u64) -> engine::read::ReqId {
    engine::read::ReqId(n)
}
fn as_epoch(n: u32) -> engine::Epoch {
    engine::Epoch(n)
}

/// The shell's own context, encoded exactly.
///
/// Trailing bytes are refused for the same reason the wire refuses them: a
/// context that is not byte-for-byte what this build wrote is not this
/// build's context, and half-reading one produces a shell in a state nobody
/// chose.
fn ctx_opts() -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new().with_fixint_encoding()
}

/// How a host derives a block's contract id (`Shell::contract_of`).
pub type ContractOf = Box<dyn Fn(&Cid) -> Cid>;

pub struct Shell<B: Blocks> {
    pub engine: Engine<B>,
    /// The highest protocol version a client has spoken this call.
    ///
    /// Not persisted in the context: it is a property of THIS call's inbound
    /// messages, and a version remembered from a previous call would be a
    /// guess about who is talking now.
    client_version: u16,
    awaiting: BTreeMap<Cid, u32>,
    head: Option<(u64, Cid)>,
    head_exists: bool,
    /// How a block's CONTRACT id is derived, set by the host that knows the
    /// Block contract's code (sdk#150). An answer from the node names the
    /// contract; the block it is about is found by deriving the contract id
    /// of each block this shell and its engine are WAITING ON -- so nothing
    /// maps "what went out", and nothing can be evicted from a map. Unset,
    /// answers are taken to name the block itself (the native fixtures).
    pub contract_of: Option<ContractOf>,
    /// Bytes of contract code this call was asked to install, if any.
    pub installed: Option<usize>,
    /// Whether a signing key has been provisioned this call.
    pub provisioned: bool,
    /// Puts confirmed by a read-back this call.
    pub read_back_hits: usize,
    /// The client asked who it is talking to.
    pub identity: bool,
    /// Who sent the frame being handled: its session and version (see
    /// `as_client`). Per FRAME, never carried: the next frame may be another
    /// tab's.
    speaker: engine::ClientId,
    /// Requests in the protocol's vocabulary that this shell does not serve
    /// yet. Answered, never ignored.
    pub unserved: Vec<u64>,
    /// The page size actually used, per request, so the reply can say so.
    page_clamp: BTreeMap<u64, u32>,
    /// Whether the delegate has the contract code it writes with.
    ///
    /// Supplied by the entry point from the secret store, because only the
    /// entry point has one. Kept as a bool rather than the bytes: the shell
    /// decides WHETHER a put can be built, and the entry point builds it.
    has_code: bool,
    /// Whether the secret store holds everything a head write needs, as read
    /// at the start of this call. Reported by `Identity`, never derived here
    /// — the store is the authority and it lives outside the shell.
    head_writable: bool,
    /// An `Install` arrived at a delegate that already has everything, and
    /// was refused. Answered this call; never carried in the context.
    already_installed: bool,
    /// The head's contract instance id, as the entry point derived it this
    /// call. Reported by `Identity`; zero when there is no Register to name.
    head_id: [u8; 32],
    pub limits: Limits,
    /// Whether this session wants a call tree.
    ///
    /// Carried in the context, because a client turns it on once and the
    /// calls it wants to see are the LATER ones — a flag that lived only in
    /// this call's memory would be off again by the time anything happened.
    tracing: bool,
    /// Steps emitted this call, in order.
    trace: Vec<protocol::Reply>,
    /// Which operation the steps of this call belong to.
    ///
    /// Set by a client request, and otherwise learnt from the writes the
    /// engine notified about — a call driven by a node response has no
    /// request to name it, and attributing those to nothing would leave the
    /// whole middle of a write untraced, which is the part that breaks.
    tracing_of: Option<protocol::TraceOf>,
}

/// What the SECRET STORE holds, as the entry point found it this call.
///
/// A struct and not three positional arguments. `has_code` and
/// `head_writable` are adjacent booleans: swapped, they compile, they type-
/// check, and the delegate then reports itself provisioned when it is not —
/// which is the failure `head_writable` was added to prevent. A field name
/// at each call site is the cheapest thing that makes a swap impossible.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreFacts {
    /// The Block contract's code is in the store.
    pub has_code: bool,
    /// Everything a head write needs is there: code AND params AND key.
    pub head_writable: bool,
    /// The head's contract instance id, or zero when there is no Register.
    pub head_id: [u8; 32],
}

impl StoreFacts {
    /// A delegate that is fully set up, for tests about ENGINE behaviour
    /// where provisioning is not the subject.
    pub fn provisioned() -> StoreFacts {
        StoreFacts {
            has_code: true,
            head_writable: true,
            head_id: [0u8; 32],
        }
    }
}

impl<B: Blocks> Shell<B> {
    /// Rebuild from the context, or start fresh if it cannot be read.
    ///
    /// A context this build did not write is REFUSED by the engine, and
    /// refusal is free: the engine re-reads its head and reports whatever was
    /// in flight `Lost`. So an unreadable context is a fresh start, not an
    /// error — and never a panic, because the bytes come from outside.
    pub fn resume(ctx: &[u8], params: Params, blocks: B) -> Self {
        Self::resume_with(ctx, params, blocks, StoreFacts::provisioned())
    }

    /// As `resume`, saying whether the contract code is on hand and whether
    /// the secret store can sign a head. `resume` answers `true` to both:
    /// it is the constructor for tests about engine behaviour, where the
    /// delegate is taken as already set up. The entry point, which is the
    /// only caller that can actually look, reads both from the store.
    pub fn resume_with(ctx: &[u8], params: Params, blocks: B, store: StoreFacts) -> Self {
        let StoreFacts {
            has_code,
            head_writable,
            head_id,
        } = store;
        let carried: Option<Carried> = ctx_opts().deserialize(ctx).ok();
        // A context the shell cannot read and one the ENGINE refuses are the
        // same outcome: start fresh. Never a panic — these bytes come from
        // the node's cache, and a delegate a malformed context can take down
        // is one anybody can take down.
        let (engine_ctx, awaiting, head, tracing, tracing_of) = match carried {
            Some(c) => (
                c.engine,
                c.shell.awaiting.into_iter().collect(),
                c.shell.head,
                c.shell.tracing,
                c.shell.tracing_of,
            ),
            None => (Vec::new(), BTreeMap::new(), None, false, None),
        };
        let (engine, resumed) = Engine::from_context_or_new(&engine_ctx, params, blocks);
        check_limits(&params, Limits::default());
        check_reserve(&params);
        Shell {
            client_version: 0,
            engine,
            // Ids to read back belong to a commit the engine no longer has if
            // it did not resume, so they go with it.
            awaiting: if resumed { awaiting } else { BTreeMap::new() },
            head: if resumed { head } else { None },
            // Starts false every call and is set by what the node SAYS this
            // call — a read of the head, or its absence. Nothing carries it.
            head_exists: false,
            contract_of: None,
            installed: None,
            provisioned: false,
            read_back_hits: 0,
            identity: false,
            speaker: as_client(protocol::LEGACY_SESSION, 1),
            unserved: Vec::new(),
            page_clamp: BTreeMap::new(),
            has_code,
            head_writable,
            already_installed: false,
            head_id,
            limits: Limits::default(),
            tracing: if resumed { tracing } else { false },
            trace: Vec::new(),
            tracing_of: if resumed { tracing_of } else { None },
        }
    }

    pub fn to_context(&self) -> Option<Vec<u8>> {
        let engine = self.engine.to_context().ok()?;
        // #41's exact encoding, with slice 5's `head_exists` gone: it is
        // derived from the engine's published seq, not carried.
        let whole = ctx_opts()
            .serialize(&Carried {
                engine,
                shell: ShellState {
                    awaiting: self.awaiting.iter().map(|(c, n)| (*c, *n)).collect(),
                    head: self.head,
                    tracing: self.tracing,
                    tracing_of: self.tracing_of,
                },
            })
            .ok()?;
        // `max_context_bytes` bounds the WHOLE context, the shell's part
        // included (sdk#162): the engine keeps itself under it less
        // `shell_context_reserve`, and the shell's worst case is asserted to
        // fit the reserve where the shell is built (`check_reserve`).
        //
        // A BACKSTOP NO HONEST TEST REACHES: with both of those in force no
        // valid params exceed the bound here, so reaching it means an
        // estimate was wrong -- the engine's `worst_case_fixed_bytes` or the
        // shell's `check_reserve`. Loud in every debug run; in release, `None`,
        // which the host reports as NOT SAVED.
        let fits = whole.len() <= self.engine.params().max_context_bytes;
        debug_assert!(
            fits,
            "the whole context ({} B) is over max_context_bytes: worst_case_fixed_bytes or \
             check_reserve underestimated",
            whole.len()
        );
        fits.then_some(whole)
    }

    /// One call: everything that arrived, everything that leaves.
    pub fn handle(&mut self, inbound: Vec<Inbound>) -> Outbound {
        // `limits` is a public field, so the pairing is checked where it is
        // used as well as where the shell is built.
        check_limits(self.engine.params(), self.limits);
        let mut out = Outbound::default();
        let mut sched = Scheduler::default();
        // WHAT THE NODE CONFIRMED BEFORE THIS CALL EXISTED.
        //
        // The scheduler learns what the node holds from acks, and it is built
        // fresh every call — a delegate gets a new linear memory each time
        // (F32), so an ack from last call is not in it. An op held on a
        // dependency confirmed in an EARLIER call is therefore held on
        // something nothing in this call will ever confirm, and it is dropped
        // when the call ends.
        //
        // Owed parity is exactly that op: it goes out `after` the published
        // root. The published root is confirmed BY DEFINITION — it is a root
        // whose head the node acknowledged, and a head is only bumped once
        // its commit's blocks have landed — so seeding it is a statement of
        // what already happened, not a relaxation of the gate.
        //
        // Measured before this line existed: a group stayed owed across three
        // ticks and a Flush, its three parity puts held and dropped every
        // call. The redundancy the tree promises was never written at all.
        if self.engine.published_seq() > 0 {
            sched.confirm(self.engine.published_root());
        }
        // THE SAME RULE, for the commit in flight (sdk#150). On a real node
        // each answer to a PUT is its own call, so the block a head bump waits
        // on was confirmed in an EARLIER call -- this scheduler, which starts
        // every call knowing nothing, held the head on it, the head was
        // stranded at the end of the call, the engine (having already marked
        // it sent) never emitted it again, and the write never published.
        // Measured live on every revision back to #85. The engine carries
        // what it has seen confirmed, so the scheduler is told, not left to
        // rediscover it.
        for id in self.engine.confirmed_in_flight() {
            sched.confirm(id);
        }

        for msg in inbound {
            // Attribution BEFORE the work, so the steps the work produces
            // have somewhere to go.
            self.attribute(&msg);
            // An answer naming a CONTRACT is matched to its block first. One
            // that matches nothing this shell waits on is dropped as
            // `Unexpected`, never passed on under the contract's id: taken
            // for a block, an ack was read back as one and a miss was
            // reported against nothing (sdk#150).
            let msg = match msg {
                Inbound::PutAckedContract { contract, ok } => {
                    match self.block_for_contract(&contract) {
                        Some(id) => Inbound::PutAcked { id, ok },
                        None => {
                            out.dropped.push(Dropped::Unexpected);
                            continue;
                        }
                    }
                }
                Inbound::MissedContract { contract } => match self.block_for_contract(&contract) {
                    Some(id) => Inbound::GotState { id, bytes: None },
                    None => {
                        out.dropped.push(Dropped::Unexpected);
                        continue;
                    }
                },
                other => other,
            };
            let effects = match msg {
                Inbound::Client(bytes) => match crate::serve::serve(&bytes) {
                    crate::serve::Served::Do(r, v, session) => {
                        // The highest version any client has spoken this call,
                        // for the replies that are about the CALL (a trace,
                        // its byte counts). A write's state is addressed from
                        // the write itself: see `as_client`.
                        self.client_version = self.client_version.max(v);
                        self.speaker = as_client(session, v);
                        self.on_protocol(r)
                    }
                    // A version this build does not serve, or bytes it cannot
                    // read. Either way the client is ANSWERED: it is the one
                    // waiting, and a refusal it can act on beats a silence it
                    // cannot.
                    crate::serve::Served::Answer(reply) => {
                        out.replies.push(reply_bytes(&reply));
                        out.dropped.push(Dropped::Unparseable);
                        Vec::new()
                    }
                },
                Inbound::GotState { id, bytes } => {
                    // The scheduler starts each call knowing nothing, so anything
                    // this call learns is on the node must be told to it, or an
                    // effect that depends on the block will be held and then
                    // stranded. The head bump is exactly such an effect.
                    if bytes.is_some() {
                        sched.confirm(id);
                    }
                    self.on_got(id, bytes, &mut sched)
                }
                Inbound::PutAcked { id, ok } => self.on_acked(id, ok, &mut sched),
                Inbound::HeadAcked { ok } => self.on_head_acked(ok, &mut sched),
                Inbound::GotHead { seq, root } => self.on_head(Some((seq, root))),
                Inbound::NoHead => self.on_head(None),
                Inbound::Other => {
                    out.dropped.push(Dropped::NotForUs);
                    Vec::new()
                }
                // Matched to a block (or dropped) above, before this match.
                Inbound::PutAckedContract { .. } | Inbound::MissedContract { .. } => Vec::new(),
            };
            // EVERY effect, whatever produced it. Converting only the ones a
            // client REQUEST produced dropped every notification arising from
            // a node response — which is all of them after `Accepted`, so a
            // write published and the client was never told.
            self.reply_from(&effects, &mut out);
            // What the core DID, as it did it. A delegate has no log anyone
            // can read, so without this a break anywhere in the chain looks
            // like every other break: a write that stops at `Accepted`.
            self.step(1, protocol::Step::Effects, effects.len() as u64);
            for f in &effects {
                match f {
                    Effect::Notify {
                        write_id, state, ..
                    } => {
                        // The operation is named HERE as well as at the
                        // request, because a call driven by a node response
                        // has no request to name it — and that is most of a
                        // write's life.
                        self.tracing_of = Some(protocol::TraceOf::Write(write_id.0));
                        self.step(2, protocol::Step::Reached, state_tag(*state));
                    }
                    Effect::Reply { req_id, result, .. } => {
                        self.tracing_of = Some(protocol::TraceOf::Read(req_id.0));
                        self.step(2, protocol::Step::Reached, rows_in(result));
                    }
                    Effect::FetchBlock { .. } => self.step(2, protocol::Step::Fetch, 1),
                    _ => {}
                }
            }
            out.effects += effects.len();
            sched.offer(effects);
        }

        // Answer the protocol requests that produce no engine event.
        if self.identity {
            self.identity = false;
            out.replies.push(reply_bytes(&protocol::Reply::Identity {
                engine: concat!("craftworks-engine/", env!("CARGO_PKG_VERSION")).into(),
                // WHERE the authority came from. Never the key.
                key_source: "Provisioned(Test)".into(),
                head_seq: self.engine.published_seq(),
                head_root: self.engine.published_root(),
                // The page's provisioning decision rests on this. An
                // unprovisioned delegate answers `Identity` with a seq of
                // 0 and a zero root — INDISTINGUISHABLE from a healthy
                // engine that has simply never been written to, while it
                // silently drops every head op it is given. Without this
                // field a page cannot tell "set me up" from "already set
                // up and empty", and re-installing costs the signing key
                // and with it every head written under the old one.
                head_writable: self.head_writable,
                head_id: self.head_id,
            }));
        }
        if self.already_installed {
            self.already_installed = false;
            out.replies
                .push(reply_bytes(&protocol::Reply::AlreadyInstalled));
        }
        for req_id in std::mem::take(&mut self.unserved) {
            // Named, not silent. A client that asked and heard nothing cannot
            // tell "this build does not serve that yet" from "lost".
            out.replies.push(reply_bytes(&protocol::Reply::Unavailable {
                req_id,
                blocked_on: [0u8; 32],
            }));
        }
        // What the engine confirmed FROM FACT during this call (a tick's
        // settle, sdk#150 E2) reaches the scheduler too: the head bump it
        // emitted in the same step depends on exactly those blocks, and a
        // scheduler told only at the top of the call would hold it and lose
        // it with the call.
        for id in self.engine.confirmed_in_flight() {
            sched.confirm(id);
        }
        out.ops = sched.take(self.limits);
        // No code, no put. A delegate cannot fabricate a contract, so a PUT
        // before `Install` is one the node would refuse anyway. Refusing it
        // HERE reports it — `refused_no_code` — where letting it go would
        // spend a round trip to be told the same thing, and the commit would
        // then wait on a confirmation that is never coming.
        if !self.has_code {
            let before = out.ops.len();
            out.ops.retain(|o| !matches!(o, Op::Put { .. }));
            out.refused_no_code = before - out.ops.len();
        }
        // A head that is going out now is one to read back later. Recorded
        // HERE, where it is known to have been issued — recording it when the
        // core emitted it would remember a head the scheduler was still
        // holding for its packs.
        for op in &out.ops {
            if let Op::Head { seq, root } = op {
                self.head = Some((*seq, *root));
            }
        }
        // The node operations this call actually issued, counted after the
        // scheduler has decided which go out — the effects above say what the
        // core wanted, and these say what left.
        let puts = out
            .ops
            .iter()
            .filter(|o| matches!(o, Op::Put { .. }))
            .count();
        if puts > 0 {
            self.step(1, protocol::Step::Put, puts as u64);
        }
        for op in &out.ops {
            if let Op::Head { seq, .. } = op {
                self.step(1, protocol::Step::Head, *seq);
            }
        }
        if self.read_back_hits > 0 {
            self.step(1, protocol::Step::ReadBack, self.read_back_hits as u64);
        }
        for r in std::mem::take(&mut self.trace) {
            out.replies.push(reply_bytes(&r));
        }
        out.client_version = self.client_version;
        out.shed = self.engine.take_shed();
        // Read-backs only for blocks the engine still waits on: one for a
        // commit released Lost, or a group no longer owed, would otherwise
        // stay for ever and the shell's part of the context would grow
        // without bound (sdk#162).
        let waiting = self.engine.waiting_on();
        self.awaiting.retain(|id, _| waiting.contains(id));
        out.stranded = sched.ready_len() + sched.held_len();
        if out.stranded > 0 {
            out.stranded_detail = sched.stranded_report();
        }
        out.awaiting = self.awaiting.len();
        out.read_back_hits = self.read_back_hits;
        // Counted from the ops actually leaving this call, not from a running
        // total kept beside them — a second account of the same facts is what
        // disagreed with the per-key record in the harness.
        for op in &out.ops {
            match op {
                crate::schedule::Op::Put { bytes, .. } => {
                    out.puts += 1;
                    out.put_bytes += bytes.len();
                }
                crate::schedule::Op::Get { .. } => out.gets += 1,
                _ => {}
            }
        }
        out
    }

    /// The most steps one call may emit.
    ///
    /// A trace is diagnostics riding the same connection as the data, so it
    /// is bounded like anything else on that wire. A call that produced more
    /// than this emits the first `MAX_STEPS` — the beginning of a call tree
    /// is where the shape is, and a truncated beginning beats a complete
    /// middle nobody can see the start of.
    fn step(&mut self, depth: u8, what: protocol::Step, n: u64) {
        if !self.tracing || self.trace.len() >= MAX_STEPS {
            return;
        }
        let Some(of) = self.tracing_of else {
            // A step with nothing to attribute it to is a line in a log. The
            // whole point of a call TREE is that every step belongs to an
            // operation, so one that does not is dropped rather than emitted
            // under a made-up id.
            return;
        };
        self.trace
            .push(protocol::Reply::Step { of, depth, what, n });
    }

    /// A versioned protocol request.
    ///
    /// The translation is deliberately shallow: the protocol's vocabulary and
    /// the core's are close because they describe the same thing, and a
    /// mapping layer that "improved" one into the other is where the two
    /// drift apart.
    fn on_protocol(&mut self, r: protocol::Request) -> Vec<Effect> {
        use protocol::Request as P;
        // ONE conversion, used by every arm that takes a range. Three arms
        // take one now; three copies of this would be three places for the
        // bounds to be read differently.
        fn bound(b: protocol::Bound) -> std::ops::Bound<Vec<u8>> {
            use std::ops::Bound as B;
            match b {
                protocol::Bound::Unbounded => B::Unbounded,
                protocol::Bound::Included(k) => B::Included(k),
                protocol::Bound::Excluded(k) => B::Excluded(k),
            }
        }
        let ev = match r {
            // v5's write (craftworks-sdk#183). Cannot arrive: decode refuses it
            // below v5, and `serve` answers every v5 frame `Unsupported` until
            // the engine implements the order rule (build step 2). NOT a panic
            // if it ever does — a panic in a delegate closes the engine.
            P::WriteFrom { .. } => {
                debug_assert!(false, "a WriteFrom reached the shell: serve must answer v5 Unsupported until step 2");
                return Vec::new();
            }
            P::Identity => {
                // Identity is also how a session BEGINS. There is no separate
                // `start` in the protocol and there should not be: answering
                // "where is your head" requires reading it, so the engine is
                // started here and the head read goes out with this call.
                //
                // The answer carries the head as known RIGHT NOW — 0 on a
                // brand-new engine that has never published, the real seq on
                // a rehydrated one, since a rehydrated engine carries its
                // published seq in its context. A client that wants the
                // freshest asks again after the read lands.
                self.identity = true;
                Event::Start {
                    key: KeySource::Provisioned(engine::Provisioned::Test),
                    epochs: vec![as_epoch(1)],
                }
            }
            P::Get { req_id, key } => Event::Get {
                client: self.speaker,
                req_id: as_req_id(req_id),
                key,
            },
            P::Write { write_id, ops } => Event::Write {
                client: self.speaker,
                write_id: as_write_id(write_id),
                ops: ops
                    .into_iter()
                    .map(|o| match o {
                        protocol::Op::Put(k, v) => (k, engine::Op::Put(v)),
                        protocol::Op::Delete(k) => (k, engine::Op::Delete),
                    })
                    .collect(),
            },
            // UNUSED BY ANY CLIENT (sdk#146): `src/`, `web/src/` and `js/` send
            // no `AskWrite` (read at all three, against 3 `Request::Write`
            // senders as the control). Served, and keyed by the asking
            // session like every request, so a reader of `on_ask`'s `known`
            // test knows it is exercised by tests alone.
            P::AskWrite { write_id } => Event::AskWrite {
                client: self.speaker,
                write_id: as_write_id(write_id),
            },
            P::Tick { now } => Event::Tick(now),
            P::Flush => Event::Flush,
            P::Range {
                req_id,
                lo,
                hi,
                reverse,
                after,
                max_entries,
            } => {
                // CLAMPED here, and the clamp is reported back rather than
                // applied silently: a caller that asked for a thousand rows
                // and got a hundred needs to know the page it holds is not
                // the page it asked for, or it will read the short answer as
                // the end of the range.
                let clamped = (max_entries as usize).clamp(1, MAX_PAGE_ENTRIES);
                self.page_clamp.insert(req_id, clamped as u32);
                Event::Scan {
                    client: self.speaker,
                    req_id: as_req_id(req_id),
                    range: Box::new(freenet_prolly::range::Range {
                        lo: bound(lo),
                        hi: bound(hi),
                        reverse,
                        after,
                        max_entries: clamped,
                        max_bytes: MAX_PAGE_BYTES,
                    }),
                }
            }
            P::Preload { roots } => Event::Preload {
                client: self.speaker,
                // Fixed-width ids off the wire. A root is 32 bytes; anything
                // else is not one, and the engine's budget bounds how many of
                // them are walked.
                roots: roots.into_iter().collect(),
            },
            P::SubscribeRange { sub_id, lo, hi } => Event::SubscribeRange {
                client: self.speaker,
                sub_id,
                range: engine::subs::SubRange {
                    lo: bound(lo),
                    hi: bound(hi),
                },
            },
            // The ENGINE's copy only. The node has no unsubscribe, so a
            // delegate goes on being woken for contracts its engine has
            // forgotten — which is ordinary, not an error, and is why nothing
            // here treats an unknown wake-up as one. Writing a release path
            // that silently does nothing at the node would be worse than
            // having none.
            P::Unsubscribe { sub_id } => Event::Unsubscribe {
                client: self.speaker,
                sub_id,
            },
            P::ChangesSince {
                req_id,
                from,
                lo,
                hi,
                max_entries,
            } => Event::ChangesSince {
                client: self.speaker,
                req_id: as_req_id(req_id),
                from,
                range: engine::subs::SubRange {
                    lo: bound(lo),
                    hi: bound(hi),
                },
                max_entries: max_entries as usize,
            },
            // Still v1 vocabulary this shell does not serve. Answered as such
            // rather than silently ignored: a client that asked and heard
            // nothing cannot tell "not implemented" from "lost".
            P::Trace { on } => {
                self.tracing = on;
                return Vec::new();
            }
            P::Subscribe { .. } => {
                self.unserved.push(0);
                return Vec::new();
            }
            P::Install {
                block_code,
                register_code,
                signing_key,
                ..
            } => {
                // FIRST WRITER WINS, and the guard is HERE rather than in any
                // page.
                //
                // `Install` overwrites the signing key, and the Register
                // instance is derived from a keyset — so a second install
                // mints a second key, moves the head's contract id, and
                // orphans everything written under the first. A polite client
                // does not prevent that: two tabs opened together on a fresh
                // node both find it unprovisioned and both install, a
                // re-issue after a stall installs again, an older page knows
                // nothing of the rule, and any web page at all can send one
                // message. The delegate is the only place that sees them all.
                //
                // So an install over a provisioned delegate changes NOTHING
                // and says so. It is the ordinary outcome of a race, not an
                // error. Replacing a key or the contract code is the hand-over
                // design in sdk#14, never a blind overwrite.
                if self.head_writable {
                    self.already_installed = true;
                    return Vec::new();
                }
                // Handled by the entry point, which is the only place with a
                // secret store. The shell records that it was asked, so a
                // caller can tell "installed" from "never arrived".
                self.installed = Some(block_code.len() + register_code.len());
                let _ = &signing_key;
                self.provisioned = true;
                return Vec::new();
            }
        };
        self.engine.step(ev)
    }

    /// A GET came back.
    ///
    /// Two jobs at once, and they are different. If the shell was waiting to
    /// READ BACK this id, its arrival is what makes the put durable enough to
    /// call confirmed. Either way the bytes are a block the core may be
    /// waiting on, so they also become `BlockArrived`.
    fn on_got(&mut self, id: Cid, bytes: Option<Vec<u8>>, s: &mut Scheduler) -> Vec<Effect> {
        let mut effects = Vec::new();
        match bytes {
            Some(b) => {
                if self.awaiting.remove(&id).is_some() {
                    // THE read-back. Only this produces PutConfirmed.
                    s.confirm(id);
                    effects.extend(self.engine.step(Event::PutConfirmed(id)));
                }
                effects.extend(self.engine.step(Event::BlockArrived { id, bytes: b }));
            }
            None => {
                // The sync read again before believing a miss: a GET that
                // came back empty and a node that does not hold it are
                // different claims, and only the second one matters.
                if self.awaiting.contains_key(&id) && self.engine.blocks().get(&id).is_some() {
                    self.awaiting.remove(&id);
                    self.read_back_hits += 1;
                    s.confirm(id);
                    return self.engine.step(Event::PutConfirmed(id));
                }
                match self.awaiting.get(&id).copied() {
                    // An empty read-back is NOT proof the bytes are absent.
                    //
                    // The node acknowledged the put, and a put is readable a
                    // short time later rather than instantly — measured at
                    // +50 ms, and asked for here at +0. Treating the first
                    // empty answer as a failure reported a write dead that
                    // had in fact landed, which is the same false statement
                    // `Failed` exists to avoid, arrived at from the other
                    // side.
                    //
                    // So it is ASKED AGAIN, a bounded number of times. The
                    // bound counts ROUNDS, not misses, for the usual reason:
                    // every round here is a completed request.
                    Some(rounds) if rounds + 1 < MAX_READ_BACK_ROUNDS => {
                        self.awaiting.insert(id, rounds + 1);
                        s.offer(vec![Effect::FetchBlock {
                            id,
                            via: Via::Direct,
                            attempt: rounds + 2,
                        }]);
                    }
                    // Out of rounds. NOW the ack is disbelieved.
                    Some(_) => {
                        self.awaiting.remove(&id);
                        effects.extend(self.engine.step(Event::PutFailed(id)));
                    }
                    None => effects.extend(self.engine.step(Event::BlockMissed(id))),
                }
            }
        }
        effects
    }

    /// A PUT was acknowledged. Read it back; do not believe the ack.
    ///
    /// The read-back is the SYNC read (F14), not a GET. It asks the node what
    /// it holds right now, costs no round trip, and is the same call the
    /// engine reads the tree through — so a block that answers it is a block
    /// the engine can actually use, which is the property `Published` is
    /// supposed to mean. A delegate-originated GET was the first attempt and
    /// came back empty twelve times for a put the node had accepted without
    /// complaint.
    ///
    /// If the sync read misses, the block may simply not be visible YET, so a
    /// GET is issued as well — it fetches, and it also wakes this delegate
    /// again, which nothing else will do once the client stops talking.
    fn on_acked(&mut self, id: Cid, ok: bool, s: &mut Scheduler) -> Vec<Effect> {
        if !ok {
            return self.engine.step(Event::PutFailed(id));
        }
        if self.engine.blocks().get(&id).is_some() {
            self.read_back_hits += 1;
            // The SCHEDULER must hear this too. It starts every call knowing
            // nothing, so an effect that depends on this block — the head
            // bump does — would be held and then lost with the call. The
            // engine emits the head in the very step this confirmation is
            // delivered, so the two must happen in that order, here.
            s.confirm(id);
            return self.engine.step(Event::PutConfirmed(id));
        }
        self.awaiting.insert(id, 0);
        s.offer(vec![Effect::FetchBlock {
            id,
            via: Via::Direct,
            attempt: 1,
        }]);
        Vec::new()
    }

    /// The head was acknowledged: now read it back.
    ///
    /// A `ReadHead` effect, so the read goes out the same way the engine's
    /// own first read does and the entry point needs no second path for it.
    fn on_head_acked(&mut self, ok: bool, s: &mut Scheduler) -> Vec<Effect> {
        if !ok {
            // The head did not even reach the node. The commit is not
            // published and the engine is told nothing it could act on: the
            // next entry re-reads the head and finds the old one.
            self.head = None;
            return Vec::new();
        }
        s.offer(vec![Effect::ReadHead { epoch: as_epoch(1) }]);
        Vec::new()
    }

    /// The head Register was read.
    ///
    /// Three different facts arrive on this one message and they must not be
    /// confused. If the shell wrote a head and this is that seq, the commit
    /// is PUBLISHED. If it wrote one and a DIFFERENT seq is there, someone
    /// else won and this is a conflict. If it wrote none, this is the fresh
    /// start reading where it left off.
    fn on_head(&mut self, got: Option<(u64, Cid)>) -> Vec<Effect> {
        match (self.head.take(), got) {
            // The head this shell WROTE is (seq, root), and only that pair
            // read back is its confirmation. The same seq under another root
            // is another writer's head at that seq: a conflict, never
            // `Published` (sdk#178: seq alone was compared, and a foreign
            // head at our seq was told Published).
            (Some((want, want_root)), Some((seq, root))) => {
                if seq == want && root == want_root {
                    self.head_exists = true;
                    self.engine.step(Event::HeadConfirmed(seq))
                } else {
                    self.engine.step(Event::HeadConflict { seq, root })
                }
            }
            // Written, and not there: the ack was not a promise. The engine
            // hears it, so a commit whose head never landed re-issues it
            // rather than waiting for a confirmation that cannot come.
            (Some(_), None) => self.engine.step(Event::HeadMissing),
            (None, Some((seq, root))) => {
                // Reading one is proof it is there.
                self.head_exists = true;
                self.engine.step(Event::HeadRead {
                    epoch: as_epoch(1),
                    seq,
                    root,
                })
            }
            (None, None) => self.engine.step(Event::HeadMissing),
        }
    }

    /// Turn what the core emitted for a CLIENT into protocol replies.
    ///
    /// The engine's states map one-to-one onto the protocol's, deliberately:
    /// a client that is told `Busy` has to know it may re-submit, and a
    /// translation that blurred that into "an error" would leave the outbox
    /// with nothing to act on.
    fn reply_from(&self, effects: &[Effect], out: &mut Outbound) {
        use protocol::WriteState as W;
        for f in effects {
            let r = match f {
                Effect::Notify {
                    client, write_id, state,
                } => {
                    let version = version_of(*client);
                    let state = match state {
                        State::Accepted => W::Accepted,
                        State::Stalled => W::Stalled,
                        State::Published => W::Published,
                        State::ParityComplete => W::ParityComplete,
                        State::Busy => W::Busy,
                        State::Failed => W::Failed,
                        State::Lost => W::Lost,
                        State::TooLarge { bound, limit, got } => W::too_large(
                            match bound {
                                engine::WriteBound::CommitBlocks => {
                                    protocol::WriteBound::CommitBlocks
                                }
                                engine::WriteBound::WriteBytes => protocol::WriteBound::WriteBytes,
                            },
                            *limit,
                            *got,
                        ),
                    }
                    // THE ONE PLACE a write state leaves the delegate, so the
                    // one place a client is told what IT can read — in the
                    // version of the write's own client, never of whichever
                    // tab happened to speak in this call.
                    .for_client(version);
                    // Named only for a writer that can read it AND has a session
                    // to name: a v4 frame that sent the legacy session is as
                    // indistinguishable as any pre-v4 page, and gains nothing.
                    if version >= protocol::SESSION_SINCE && session_of(*client) != protocol::LEGACY_SESSION {
                        protocol::Reply::SessionWriteState {
                            session: session_of(*client),
                            write_id: write_id.0,
                            state,
                        }
                    } else {
                        protocol::Reply::WriteState { write_id: write_id.0, state }
                    }
                }
                Effect::Reply { req_id, result, .. } => {
                    // WHERE THIS ENGINE STANDS, as it answers.
                    //
                    // Taken once, here, so every answer in this call reports the
                    // same head — two answers from one call describing two trees
                    // would be the very confusion `At` exists to remove.
                    let at = protocol::At {
                        seq: self.engine.published_seq(),
                        root: self.engine.published_root(),
                    };
                    match result {
                        engine::read::ReadResult::Value(v) => protocol::Reply::Value {
                            req_id: req_id.0,
                            value: v.clone(),
                        },
                        engine::read::ReadResult::Page {
                            entries, cursor, ..
                        } => protocol::Reply::Page {
                            req_id: req_id.0,
                            entries: entries.clone(),
                            cursor: cursor.clone(),
                            // What was USED, not what was asked for.
                            max_entries: self
                                .page_clamp
                                .get(&req_id.0)
                                .copied()
                                .unwrap_or(entries.len() as u32),
                            at,
                        },
                        engine::read::ReadResult::Unavailable(cid) => {
                            protocol::Reply::Unavailable {
                                req_id: req_id.0,
                                blocked_on: *cid,
                            }
                        }
                        engine::read::ReadResult::OutOfWarmSpace => protocol::Reply::Unavailable {
                            req_id: req_id.0,
                            blocked_on: [0u8; 32],
                        },
                        engine::read::ReadResult::Delta {
                            changes,
                            cursor,
                            new_root,
                        } => protocol::Reply::Delta {
                            req_id: req_id.0,
                            changes: changes.clone(),
                            cursor: cursor.clone(),
                            new_root: *new_root,
                            at,
                        },
                        engine::read::ReadResult::FullReloadRequired { new_root } => {
                            protocol::Reply::FullReloadRequired {
                                req_id: req_id.0,
                                new_root: *new_root,
                                at,
                            }
                        }
                    }
                }
                // A subscribed range moved. PUSHED — no client asked for this
                // message, which is the whole point of it.
                Effect::Changed {
                    sub_id,
                    new_root,
                    seq,
                    why,
                    ..
                } => protocol::Reply::Changed {
                    sub_id: *sub_id,
                    new_root: *new_root,
                    seq: *seq,
                    why: match why {
                        engine::subs::Why::Diffed => protocol::Why::Diffed,
                        engine::subs::Why::BlockMissing => protocol::Why::BlockMissing,
                        engine::subs::Why::Budgeted => protocol::Why::Budgeted,
                        engine::subs::Why::Stale => protocol::Why::Stale,
                    },
                },
                // A subscribe was taken or refused. Answered either way: a
                // client that believes it is subscribed and is not waits for
                // ever, and nothing it can see would tell it so.
                Effect::Subscribed {
                    sub_id, accepted, ..
                } => protocol::Reply::Subscribed {
                    sub_id: *sub_id,
                    accepted: match accepted {
                        engine::subs::Accepted::Yes => protocol::Accepted::Yes,
                        engine::subs::Accepted::Full => protocol::Accepted::Full,
                        engine::subs::Accepted::TooWide => protocol::Accepted::TooWide,
                    },
                },
                _ => continue,
            };
            out.replies.push(reply_bytes(&r));
        }
    }

    /// Which operation the steps of this call belong to.
    ///
    /// A client request names itself. A node response does not, so the
    /// previous call's operation stands until an effect names a new one —
    /// which is right, because a node response IS the continuation of the
    /// operation that issued the op it answers.
    fn attribute(&mut self, msg: &Inbound) {
        if !self.tracing {
            return;
        }
        let Inbound::Client(bytes) = msg else { return };
        let crate::serve::Served::Do(r, _, _) = crate::serve::serve(bytes) else {
            return;
        };
        let (of, began) = match &r {
            protocol::Request::Write { write_id, ops } => {
                (protocol::TraceOf::Write(*write_id), ops.len() as u64)
            }
            protocol::Request::Get { req_id, .. }
            | protocol::Request::Range { req_id, .. }
            | protocol::Request::ChangesSince { req_id, .. } => {
                (protocol::TraceOf::Read(*req_id), 0)
            }
            _ => return,
        };
        self.tracing_of = Some(of);
        self.step(0, protocol::Step::Began, began);
    }

    /// Does the head Register already exist on the node?
    ///
    /// DERIVED from the engine's published seq, which is 0 only when no head
    /// exists anywhere for this key — a fresh start re-reads the head before
    /// anything else and adopts whatever is there, so a seq above 0 means
    /// something published one.
    ///
    /// Deriving it from `HeadRead` alone was wrong and the live run caught
    /// it: the fact is stated on the call that READS the head, and a later
    /// commit's bump happens on a call where nothing read it — so the second
    /// write re-CREATED the Register and paid 152,542 B again (F38). Reading
    /// it out of state the engine already carries for its own reasons is
    /// both derived and always available.
    ///
    /// `self.head_exists` still records a head READ or CONFIRMED this call,
    /// for the case where the seq has not been adopted yet.
    pub fn head_exists(&self) -> bool {
        self.head_exists || self.engine.published_seq() > 0
    }

    /// Ids put and not yet read back.
    pub fn awaiting(&self) -> usize {
        self.awaiting.len()
    }

    /// Which block an answer naming `contract` is about: the block this
    /// shell or its engine is waiting on whose contract id it is.
    ///
    /// Derived over what is actually outstanding -- the engine's
    /// `waiting_on` and this shell's read-backs -- never looked up in a map
    /// of what went out. The map this replaced held 64 entries while one
    /// return carries 128 puts, so a commit of more than 64 blocks evicted
    /// its own pairings; each unpaired ack was taken for a block, read back
    /// as one, and the commit never published (sdk#150, executed).
    pub fn block_for_contract(&self, contract: &Cid) -> Option<Cid> {
        let Some(derive) = &self.contract_of else {
            return Some(*contract);
        };
        self.engine
            .waiting_on()
            .into_iter()
            .chain(self.awaiting.keys().copied())
            .find(|b| derive(b) == *contract)
    }
}
