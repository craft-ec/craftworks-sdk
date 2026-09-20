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
use engine::read::Via;
use engine::{Effect, Engine, Event, KeySource, Params};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::collections::BTreeSet;

/// What arrived this call.
#[derive(Debug)]
pub enum Inbound {
    /// An application message from a client. Opaque bytes.
    Client(Vec<u8>),
    /// A GET came back. `None` = the node does not hold it.
    GotState { id: Cid, bytes: Option<Vec<u8>> },
    /// A PUT was ACKNOWLEDGED. This is not a confirmation of anything.
    PutAcked { id: Cid, ok: bool },
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
    /// Put, acknowledged, and not yet read back.
    awaiting: Vec<Cid>,
    /// A head written and not yet read back, with the root it named.
    head: Option<(u64, Cid)>,
}

/// The whole shell context: the engine's, and the shell's own.
#[derive(serde::Serialize, serde::Deserialize)]
struct Carried {
    engine: Vec<u8>,
    shell: ShellState,
}

pub struct Shell<B: Blocks> {
    pub engine: Engine<B>,
    awaiting: BTreeSet<Cid>,
    head: Option<(u64, Cid)>,
    /// Bytes of contract code this call was asked to install, if any.
    pub installed: Option<usize>,
    /// Whether a signing key has been provisioned this call.
    pub provisioned: bool,
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
        let carried: Option<Carried> = bincode::deserialize(ctx).ok();
        // A context the shell cannot read and one the ENGINE refuses are the
        // same outcome: start fresh. Never a panic — these bytes come from
        // the node's cache, and a delegate a malformed context can take down
        // is one anybody can take down.
        let (engine_ctx, awaiting, head) = match carried {
            Some(c) => (
                c.engine,
                c.shell.awaiting.into_iter().collect(),
                c.shell.head,
            ),
            None => (Vec::new(), BTreeSet::new(), None),
        };
        let (engine, resumed) = Engine::from_context_or_new(&engine_ctx, params, blocks);
        Shell {
            engine,
            // Ids to read back belong to a commit the engine no longer has if
            // it did not resume, so they go with it.
            awaiting: if resumed { awaiting } else { BTreeSet::new() },
            head: if resumed { head } else { None },
            installed: None,
            provisioned: false,
            has_code,
            limits: Limits::default(),
        }
    }

    pub fn to_context(&self) -> Option<Vec<u8>> {
        let engine = self.engine.to_context().ok()?;
        bincode::serialize(&Carried {
            engine,
            shell: ShellState {
                awaiting: self.awaiting.iter().copied().collect(),
                head: self.head,
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
    fn on_got(&mut self, id: Cid, bytes: Option<Vec<u8>>, _s: &mut Scheduler) -> Vec<Effect> {
        let mut effects = Vec::new();
        match bytes {
            Some(b) => {
                if self.awaiting.remove(&id) {
                    // THE read-back. Only this produces PutConfirmed.
                    effects.extend(self.engine.step(Event::PutConfirmed(id)));
                }
                effects.extend(self.engine.step(Event::BlockArrived { id, bytes: b }));
            }
            None => {
                if self.awaiting.contains(&id) {
                    // Put, acknowledged, and not there. The ack was not a
                    // promise and this is the proof.
                    self.awaiting.remove(&id);
                    effects.extend(self.engine.step(Event::PutFailed(id)));
                } else {
                    effects.extend(self.engine.step(Event::BlockMissed(id)));
                }
            }
        }
        effects
    }

    /// A PUT was acknowledged. Ask for it back; do not believe it.
    fn on_acked(&mut self, id: Cid, ok: bool, s: &mut Scheduler) -> Vec<Effect> {
        if !ok {
            return self.engine.step(Event::PutFailed(id));
        }
        self.awaiting.insert(id);
        // The read-back GET is a shell operation: the core did not ask for
        // this block and must not be told it is being fetched.
        s.offer(vec![Effect::FetchBlock {
            id,
            via: Via::Direct,
            attempt: 1,
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
                    self.engine.step(Event::HeadConfirmed(seq))
                } else {
                    self.engine.step(Event::HeadConflict { seq, root })
                }
            }
            // Written, and not there. The ack was not a promise.
            (Some(_), None) => Vec::new(),
            (None, Some((seq, root))) => self.engine.step(Event::HeadRead {
                epoch: wire::epoch(1),
                seq,
                root,
            }),
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

    /// Ids put and not yet read back.
    pub fn awaiting(&self) -> usize {
        self.awaiting.len()
    }
}
