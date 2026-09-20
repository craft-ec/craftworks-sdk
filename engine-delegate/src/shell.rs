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
    /// Effects still waiting at the end of the call, and therefore LOST.
    ///
    /// The scheduler does not survive the call — nothing in a delegate does
    /// — so anything it is still holding is gone, and the core will not emit
    /// it again. This must never be non-zero, and it is reported rather than
    /// asserted so the live run can see it too.
    pub stranded: usize,
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
    /// Contract id -> the block id the engine knows it by, for requests that
    /// are still out.
    ///
    /// A request is issued in one call and answered in the NEXT, and a
    /// response names the contract while the engine named the block. Nothing
    /// can recover one from the other — the contract id is a hash of the code
    /// AND the params — so the pairing is remembered. Bounded, because an
    /// unbounded map of them is a context that grows with traffic.
    outstanding: Vec<(Cid, Cid)>,
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
    }
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

/// The engine's ids, from the protocol's plain numbers.
fn as_client(n: u64) -> engine::ClientId {
    engine::ClientId(n)
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

pub struct Shell<B: Blocks> {
    pub engine: Engine<B>,
    awaiting: BTreeMap<Cid, u32>,
    head: Option<(u64, Cid)>,
    head_exists: bool,
    outstanding: Vec<(Cid, Cid)>,
    /// Bytes of contract code this call was asked to install, if any.
    pub installed: Option<usize>,
    /// Whether a signing key has been provisioned this call.
    pub provisioned: bool,
    /// Puts confirmed by a read-back this call.
    pub read_back_hits: usize,
    /// The client asked who it is talking to.
    pub identity: bool,
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
        let (engine_ctx, awaiting, head, outstanding, tracing, tracing_of) = match carried {
            Some(c) => (
                c.engine,
                c.shell.awaiting.into_iter().collect(),
                c.shell.head,
                c.shell.outstanding,
                c.shell.tracing,
                c.shell.tracing_of,
            ),
            None => (Vec::new(), BTreeMap::new(), None, Vec::new(), false, None),
        };
        let (engine, resumed) = Engine::from_context_or_new(&engine_ctx, params, blocks);
        Shell {
            engine,
            // Ids to read back belong to a commit the engine no longer has if
            // it did not resume, so they go with it.
            awaiting: if resumed { awaiting } else { BTreeMap::new() },
            head: if resumed { head } else { None },
            // Starts false every call and is set by what the node SAYS this
            // call — a read of the head, or its absence. Nothing carries it.
            head_exists: false,
            outstanding: if resumed { outstanding } else { Vec::new() },
            installed: None,
            provisioned: false,
            read_back_hits: 0,
            identity: false,
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
        ctx_opts()
            .serialize(&Carried {
                engine,
                shell: ShellState {
                    awaiting: self.awaiting.iter().map(|(c, n)| (*c, *n)).collect(),
                    head: self.head,
                    outstanding: self.outstanding.clone(),
                    tracing: self.tracing,
                    tracing_of: self.tracing_of,
                },
            })
            .ok()
    }

    /// One call: everything that arrived, everything that leaves.
    pub fn handle(&mut self, inbound: Vec<Inbound>) -> Outbound {
        let mut out = Outbound::default();
        let mut sched = Scheduler::default();

        for msg in inbound {
            // Attribution BEFORE the work, so the steps the work produces
            // have somewhere to go.
            self.attribute(&msg);
            let effects = match msg {
                Inbound::Client(bytes) => match crate::serve::serve(&bytes) {
                    crate::serve::Served::Do(r) => self.on_protocol(r),
                    // A version this build does not serve, or bytes it cannot
                    // read. Either way the client is ANSWERED: it is the one
                    // waiting, and a refusal it can act on beats a silence it
                    // cannot.
                    crate::serve::Served::Answer(reply) => {
                        out.replies.push(protocol::encode_reply(&reply));
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
            out.replies
                .push(protocol::encode_reply(&protocol::Reply::Identity {
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
                .push(protocol::encode_reply(&protocol::Reply::AlreadyInstalled));
        }
        for req_id in std::mem::take(&mut self.unserved) {
            // Named, not silent. A client that asked and heard nothing cannot
            // tell "this build does not serve that yet" from "lost".
            out.replies
                .push(protocol::encode_reply(&protocol::Reply::Unavailable {
                    req_id,
                    blocked_on: [0u8; 32],
                }));
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
            out.replies.push(protocol::encode_reply(&r));
        }
        out.stranded = sched.ready_len() + sched.held_len();
        out.awaiting = self.awaiting.len();
        out.read_back_hits = self.read_back_hits;
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
                client: as_client(1),
                req_id: as_req_id(req_id),
                key,
            },
            P::Write { write_id, ops } => Event::Write {
                client: as_client(1),
                write_id: as_write_id(write_id),
                ops: ops
                    .into_iter()
                    .map(|o| match o {
                        protocol::Op::Put(k, v) => (k, engine::Op::Put(v)),
                        protocol::Op::Delete(k) => (k, engine::Op::Delete),
                    })
                    .collect(),
            },
            P::AskWrite { write_id } => Event::AskWrite {
                client: as_client(1),
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
                    client: as_client(1),
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
                client: as_client(1),
                // Fixed-width ids off the wire. A root is 32 bytes; anything
                // else is not one, and the engine's budget bounds how many of
                // them are walked.
                roots: roots.into_iter().collect(),
            },
            P::SubscribeRange { sub_id, lo, hi } => Event::SubscribeRange {
                client: as_client(1),
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
                client: as_client(1),
                sub_id,
            },
            P::ChangesSince {
                req_id,
                from,
                lo,
                hi,
                max_entries,
            } => Event::ChangesSince {
                client: as_client(1),
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
            (Some((want, _)), Some((seq, root))) => {
                if seq == want {
                    self.head_exists = true;
                    self.engine.step(Event::HeadConfirmed(seq))
                } else {
                    self.engine.step(Event::HeadConflict { seq, root })
                }
            }
            // Written, and not there. The ack was not a promise.
            (Some(_), None) => Vec::new(),
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
                    write_id, state, ..
                } => protocol::Reply::WriteState {
                    write_id: write_id.0,
                    state: match state {
                        State::Accepted => W::Accepted,
                        State::Stalled => W::Stalled,
                        State::Published => W::Published,
                        State::ParityComplete => W::ParityComplete,
                        State::Busy => W::Busy,
                        State::Failed => W::Failed,
                        State::Lost => W::Lost,
                    },
                },
                Effect::Reply { req_id, result, .. } => match result {
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
                    },
                    engine::read::ReadResult::Unavailable(cid) => protocol::Reply::Unavailable {
                        req_id: req_id.0,
                        blocked_on: *cid,
                    },
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
                    },
                    engine::read::ReadResult::FullReloadRequired { new_root } => {
                        protocol::Reply::FullReloadRequired {
                            req_id: req_id.0,
                            new_root: *new_root,
                        }
                    }
                },
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
            out.replies.push(protocol::encode_reply(&r));
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
        let crate::serve::Served::Do(r) = crate::serve::serve(bytes) else {
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

    /// A request went out for `block` under contract id `contract`.
    ///
    /// Bounded at 64: enough for several calls' worth of outstanding
    /// requests, and a cap rather than a hope. Dropping the oldest costs a
    /// re-fetch, which the read path already handles; growing without a cap
    /// costs the context, which nothing handles.
    pub fn note_request(&mut self, contract: Cid, block: Cid) {
        if self.outstanding.iter().any(|(c, _)| *c == contract) {
            return;
        }
        if self.outstanding.len() >= 64 {
            self.outstanding.remove(0);
        }
        self.outstanding.push((contract, block));
    }

    /// Which block a response for `contract` is about, read straight out of
    /// a context without building an engine.
    ///
    /// The entry point needs this BEFORE it can build the message the shell
    /// is given, and building a shell to ask would mean building one twice.
    pub fn peek_block_for(ctx: &[u8], contract: &Cid) -> Option<Cid> {
        let c: Carried = ctx_opts().deserialize(ctx).ok()?;
        c.shell
            .outstanding
            .iter()
            .find(|(k, _)| k == contract)
            .map(|(_, b)| *b)
    }

    /// Which block a response for `contract` is about.
    pub fn block_for(&self, contract: &Cid) -> Option<Cid> {
        self.outstanding
            .iter()
            .find(|(c, _)| c == contract)
            .map(|(_, b)| *b)
    }
}
