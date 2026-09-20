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
use crate::wire::{self, Dropped, Reply, Request};
use bincode::Options;
use engine::read::Via;
use engine::{Effect, Engine, Event, KeySource, Params};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
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

/// How many times a put is asked back for before its ack is disbelieved.
///
/// A put is readable a short time after it is acknowledged, not instantly, so
/// the first empty answer means "not yet" far more often than "never".
const MAX_READ_BACK_ROUNDS: u32 = 12;

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
    /// Whether the delegate has the contract code it writes with.
    ///
    /// Supplied by the entry point from the secret store, because only the
    /// entry point has one. Kept as a bool rather than the bytes: the shell
    /// decides WHETHER a put can be built, and the entry point builds it.
    has_code: bool,
    pub limits: Limits,
}

impl<B: Blocks> Shell<B> {
    /// Rebuild from the context, or start fresh if it cannot be read.
    ///
    /// A context this build did not write is REFUSED by the engine, and
    /// refusal is free: the engine re-reads its head and reports whatever was
    /// in flight `Lost`. So an unreadable context is a fresh start, not an
    /// error — and never a panic, because the bytes come from outside.
    pub fn resume(ctx: &[u8], params: Params, blocks: B) -> Self {
        Self::resume_with(ctx, params, blocks, true)
    }

    /// As `resume`, saying whether the contract code is on hand.
    pub fn resume_with(ctx: &[u8], params: Params, blocks: B, has_code: bool) -> Self {
        let carried: Option<Carried> = ctx_opts().deserialize(ctx).ok();
        // A context the shell cannot read and one the ENGINE refuses are the
        // same outcome: start fresh. Never a panic — these bytes come from
        // the node's cache, and a delegate a malformed context can take down
        // is one anybody can take down.
        let (engine_ctx, awaiting, head, outstanding) = match carried {
            Some(c) => (
                c.engine,
                c.shell.awaiting.into_iter().collect(),
                c.shell.head,
                c.shell.outstanding,
            ),
            None => (Vec::new(), BTreeMap::new(), None, Vec::new()),
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
            has_code,
            limits: Limits::default(),
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
                },
            })
            .ok()
    }

    /// One call: everything that arrived, everything that leaves.
    pub fn handle(&mut self, inbound: Vec<Inbound>) -> Outbound {
        let mut out = Outbound::default();
        let mut sched = Scheduler::default();

        for msg in inbound {
            let effects = match msg {
                Inbound::Client(bytes) => match wire::request(&bytes) {
                    Some(r) => self.on_request(r),
                    None => {
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
            out.effects += effects.len();
            sched.offer(effects);
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
        out.stranded = sched.ready_len() + sched.held_len();
        out.awaiting = self.awaiting.len();
        out.read_back_hits = self.read_back_hits;
        out
    }

    fn on_request(&mut self, r: Request) -> Vec<Effect> {
        let ev = match r {
            // Handled by the entry point, which is the only place with a
            // secret store. The shell records that it was asked, so a caller
            // can tell "installed" from "the message never arrived".
            Request::Install {
                block_code,
                register_code,
                signing_key,
                ..
            } => {
                self.installed = Some(block_code.len() + register_code.len());
                // Recorded, never derived from. What the engine is told is
                // WHERE its authority came from, which is a statement it
                // keeps; the key itself never reaches the core.
                let _ = &signing_key;
                self.provisioned = true;
                return Vec::new();
            }
            Request::Start { epochs } => Event::Start {
                // What the engine records is WHERE its authority came from,
                // never the key. Today the only provisioner is a test
                // harness, and the type says so.
                key: KeySource::Provisioned(engine::Provisioned::Test),
                epochs: epochs.into_iter().map(wire::epoch).collect(),
            },
            Request::Write {
                client,
                write_id,
                ops,
            } => Event::Write {
                client: wire::client(client),
                write_id: wire::write_id(write_id),
                ops,
            },
            Request::Get {
                client,
                req_id,
                key,
            } => Event::Get {
                client: wire::client(client),
                req_id: wire::req_id(req_id),
                key,
            },
            Request::Tick { now } => Event::Tick(now),
            Request::Flush => Event::Flush,
            Request::AskWrite { client, write_id } => Event::AskWrite {
                client: wire::client(client),
                write_id: wire::write_id(write_id),
            },
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
        s.offer(vec![Effect::ReadHead {
            epoch: wire::epoch(1),
        }]);
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
                    epoch: wire::epoch(1),
                    seq,
                    root,
                })
            }
            (None, None) => self.engine.step(Event::HeadMissing),
        }
    }

    fn reply_from(&self, effects: &[Effect], out: &mut Outbound) {
        for f in effects {
            let r = match f {
                Effect::Notify {
                    client,
                    write_id,
                    state,
                } => Reply::Write {
                    client: client.0,
                    write_id: write_id.0,
                    state: *state,
                },
                Effect::Reply {
                    client,
                    req_id,
                    result,
                } => Reply::Read {
                    client: client.0,
                    req_id: req_id.0,
                    result: result.clone(),
                },
                _ => continue,
            };
            out.replies.push(wire::reply(&r));
        }
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
