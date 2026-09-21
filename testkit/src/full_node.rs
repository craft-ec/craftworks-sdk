//! A node that answers the WHOLE reply set, not just puts.
//!
//! [`Node`](crate::Node) answers `Op::Put` and nothing else. That is right for
//! counting puts across calls and wrong for anything that READS: no `Get`, no
//! `Head`, no `ReadHead`, and it does not feed its own answers back. So three
//! test files — `cold_write_native`, `refresh_native` and `two_clients` —
//! each hand-rolled the same forty lines, and `cold_write_native` had to be
//! allowed bare in `fixture-gate.sh` because a cold write is DEFINED by having
//! nothing loaded to read.
//!
//! This is built BESIDE the put-only fixture rather than by widening it. A
//! wider `Node` would change what its existing callers mean without their
//! authors looking; a test that says "this many puts crossed" must keep saying
//! exactly that.
//!
//! # It feeds its own answers back
//!
//! A write is not one call and a read is not one call. The shell emits ops,
//! the node answers them, and those answers cause the next ops. [`Conn::step`]
//! runs that to a standstill and returns the replies, so a test states what it
//! wants and reads what came back.
//!
//! The recursion is BOUNDED, unlike the hand-rolled copies: a fixture that
//! spins forever costs a whole run and reports nothing, whereas one that stops
//! and says how deep it got names the loop.
//!
//! # One node, several connections
//!
//! The node's blocks and head Register are shared; each [`Conn`] keeps its own
//! delegate context, because that is what two tabs actually are. `Conn` is
//! `Clone` and shares one context when cloned — two handles onto ONE engine
//! state — which is the shape `refresh_native` needs, while
//! [`FullNode::connect`] gives a genuinely separate client, which is the shape
//! `two_clients` needs.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use instrument::{
    vocab::{Key, Outcome, Site},
    Entry, Event, OpId, Probe, Recorder,
};

use crate::Store;

pub const FULL_NODE: Site = Site::of("testkit::full_node");

/// How deep the answer-feeding may go before it is called a loop.
///
/// A cold write settles in a handful of rounds. Anything approaching this is a
/// cycle, and the panic names the depth rather than hanging the run.
const MAX_ROUNDS: usize = 32;

/// Which op the node served.
///
/// Counted per kind so the fixture can PROVE it answers more than one. A
/// full-node fixture that only ever served puts would be the put-only fixture
/// wearing a new name, and nothing but a per-kind count can tell the
/// difference.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Served {
    Put,
    Get,
    Head,
    ReadHead,
}

/// The node's durable state: the blocks it holds and the head Register.
///
/// Shared by every connection to it, because that is what one node IS.
#[derive(Clone, Default)]
pub struct FullNode {
    /// What the delegate reads DIRECTLY: the node's local cache.
    store: Store,
    /// Where a GET is answered from, when it is not the local cache: the
    /// network behind a COLD node (sdk#150). `None` is the default warm node,
    /// whose every block is local.
    network: Option<Store>,
    /// An EVICTING node answers a GET and does not keep the block (F33,
    /// populate-not-retain), so a block fetched in one call is gone in the
    /// next. Only meaningful with `network`.
    evicting: bool,
    head: Rc<RefCell<Option<(u64, Cid)>>>,
}

impl FullNode {
    pub fn new() -> FullNode {
        FullNode::default()
    }

    /// A COLD node over `writer`'s network: it holds no block locally, sees
    /// the same head Register, and answers every GET from the network --
    /// populating its cache, unless it is also [`evicting`](Self::evicting).
    /// What a second device, or a node that restarted, is (sdk#150).
    pub fn cold_over(writer: &FullNode) -> FullNode {
        FullNode {
            store: Store::default(),
            network: Some(
                writer
                    .network
                    .clone()
                    .unwrap_or_else(|| writer.store.clone()),
            ),
            evicting: false,
            head: writer.head.clone(),
        }
    }

    /// Answer GETs without keeping the block (F33). See `FullNode::evicting`.
    pub fn evicting(mut self) -> FullNode {
        self.evicting = true;
        self
    }

    /// A NEW client on this node: its own delegate context, the same blocks.
    pub fn connect(&self) -> Conn {
        Conn(Rc::new(RefCell::new(ConnState {
            node: self.clone(),
            ctx: Vec::new(),
            rec: Recorder::with_capacity(1 << 14),
            calls: 0,
            served: BTreeMap::new(),
            replies: Vec::new(),
            handed: Vec::new(),
            one_answer_per_call: false,
            hold: false,
            held: VecDeque::new(),
            max_stranded: 0,
            params: engine::Params::default(),
            asked: BTreeMap::new(),
            fetched: BTreeMap::new(),
        })))
    }

    /// The blocks this node holds, to read or to seed.
    pub fn store(&self) -> Store {
        self.store.clone()
    }

    /// What the head Register holds, if anything.
    pub fn head(&self) -> Option<(u64, Cid)> {
        *self.head.borrow()
    }

    /// Put a head in place, for a test that starts from an existing tree.
    pub fn set_head(&self, seq: u64, root: Cid) {
        *self.head.borrow_mut() = Some((seq, root));
    }

    pub fn blocks(&self) -> usize {
        self.store.len()
    }
}

struct ConnState {
    node: FullNode,
    ctx: Vec<u8>,
    rec: Recorder,
    calls: u32,
    served: BTreeMap<Served, usize>,
    replies: Vec<Vec<u8>>,
    handed: Vec<Vec<u8>>,
    /// Deliver the node's answers ONE PER CALL, as a real node does (each
    /// `PutResponse` / `GetResponse` is its own delegate call). Off by
    /// default here, so no existing test changes meaning; the cross-call
    /// matrix turns it on (sdk#150).
    one_answer_per_call: bool,
    /// HOLD the node's answers instead of delivering them: they queue until
    /// the test releases them, one per call, so a tick can land while a
    /// commit is in flight.
    hold: bool,
    held: VecDeque<Inbound>,
    /// The largest `stranded` any call reported. Must stay 0.
    max_stranded: usize,
    /// The engine's parameters for this connection's calls.
    params: engine::Params,
    /// Every request this connection sent, and whether it was ANSWERED:
    /// `w<id>` for a write (a terminal write state), `r<id>` for a read.
    asked: BTreeMap<String, bool>,
    /// Block fetches, by block id: what `fetches_of` counts.
    fetched: BTreeMap<Cid, usize>,
}

/// One client's connection: its own context over a shared node.
///
/// Cloning shares the context — two handles onto one engine state.
#[derive(Clone)]
pub struct Conn(Rc<RefCell<ConnState>>);

impl Conn {
    /// One exchange, run until the node has nothing left to answer.
    ///
    /// Returns every reply the shell produced along the way.
    pub fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        self.step_bounded(inbound, 0)
    }

    /// Deliver the node's answers one per call, as a real node does.
    pub fn one_answer_per_call(&self) {
        self.0.borrow_mut().one_answer_per_call = true;
    }

    /// Run this connection's calls with these engine parameters.
    pub fn set_params(&self, params: engine::Params) {
        self.0.borrow_mut().params = params;
    }

    /// Queue the node's answers instead of delivering them; see `release`.
    pub fn hold_answers(&self) {
        self.0.borrow_mut().hold = true;
    }

    /// How many node answers are queued.
    pub fn held(&self) -> usize {
        self.0.borrow().held.len()
    }

    /// Deliver ONE queued node answer as its own call. What that call makes
    /// the node answer is queued again (the connection is still holding).
    pub fn release_one(&mut self) -> Vec<Vec<u8>> {
        let next = self.0.borrow_mut().held.pop_front();
        match next {
            Some(a) => self.step_bounded(vec![a], 0),
            None => Vec::new(),
        }
    }

    /// Send the page's clock: a `Tick` at `now_ms`, quantised exactly as a
    /// page does (`protocol::tick_of`) -- seconds since the epoch, not the
    /// small integers native tests used to send.
    pub fn tick_at(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        self.client(&protocol::Request::Tick {
            now: protocol::tick_of(now_ms),
        })
    }

    /// The largest number of effects any call left stranded -- lost at the
    /// end of that call. The shell's own doc says it must never be non-zero.
    pub fn max_stranded(&self) -> usize {
        self.0.borrow().max_stranded
    }

    /// Requests sent on this connection that have had no answer yet.
    pub fn unanswered(&self) -> Vec<String> {
        self.0
            .borrow()
            .asked
            .iter()
            .filter(|(_, a)| !**a)
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn step_bounded(&mut self, inbound: Vec<Inbound>, depth: usize) -> Vec<Vec<u8>> {
        assert!(
            depth < MAX_ROUNDS,
            "the node and the shell answered each other {MAX_ROUNDS} times \
             without settling: that is a cycle, not a slow write"
        );

        let (ctx, store, params) = {
            let s = self.0.borrow();
            (s.ctx.clone(), s.node.store.clone(), s.params)
        };

        let op = {
            let mut s = self.0.borrow_mut();
            s.calls += 1;
            let op = OpId(s.calls);
            s.rec.event(Event::Enter {
                site: FULL_NODE,
                op,
            });
            op
        };

        // Every call rehydrates. Anything the engine wanted to remember and
        // did not put in the context is gone by the next line (F32).
        let mut shell: Shell<Store> =
            Shell::resume_with(&ctx, params, store, StoreFacts::provisioned());
        let out = shell.handle(inbound);

        {
            let mut s = self.0.borrow_mut();
            s.ctx = shell.to_context().expect("a context after every call");
            s.max_stranded = s.max_stranded.max(out.stranded);
            for r in out
                .replies
                .iter()
                .filter_map(|b| protocol::decode_reply(b).ok())
            {
                use protocol::{Reply as R, WriteState as W};
                let key = match r {
                    R::WriteState { write_id, state }
                        if state.terminal() || matches!(state, W::Published) =>
                    {
                        Some(format!("w{write_id}"))
                    }
                    R::Value { req_id, .. }
                    | R::Page { req_id, .. }
                    | R::Unavailable { req_id, .. }
                    | R::Delta { req_id, .. }
                    | R::FullReloadRequired { req_id, .. } => Some(format!("r{req_id}")),
                    _ => None,
                };
                if let Some(k) = key {
                    if let Some(a) = s.asked.get_mut(&k) {
                        *a = true;
                    }
                }
            }
            s.replies.extend(out.replies.iter().cloned());
            for (key, value) in [
                (Key::BytesOut, out.put_bytes as u64),
                (Key::Ops, (out.puts + out.gets) as u64),
                (Key::Effects, out.effects as u64),
                (Key::Awaiting, out.awaiting as u64),
                (Key::ReadBack, out.read_back_hits as u64),
                (Key::Stranded, out.stranded as u64),
            ] {
                s.rec.event(Event::Counter {
                    site: FULL_NODE,
                    op,
                    entry: Entry { key, value },
                });
            }
        }

        let mut next = Vec::new();
        for o in out.ops {
            match o {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    self.record(Served::Put);
                    {
                        let s = self.0.borrow();
                        s.node.store.put(id, &bytes);
                        if let Some(net) = &s.node.network {
                            net.put(id, &bytes);
                        }
                    }
                    self.0.borrow_mut().handed.push(bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    self.record(Served::Get);
                    *self.0.borrow_mut().fetched.entry(id).or_default() += 1;
                    let held = {
                        let s = self.0.borrow();
                        let n = &s.node;
                        match &n.network {
                            // A cold node answers from the network, and keeps
                            // what it fetched unless it is evicting.
                            Some(net) => {
                                let b = net.get(&id).map(|b| b.to_vec());
                                if let (Some(b), false) = (&b, n.evicting) {
                                    n.store.put(id, b);
                                }
                                b
                            }
                            None => n.store.get(&id).map(|b| b.to_vec()),
                        }
                    };
                    next.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    self.record(Served::Head);
                    // A newer head wins; an older one does not overwrite it,
                    // which is what makes a second client's stale write a
                    // conflict rather than a silent rollback.
                    let answer = {
                        let s = self.0.borrow();
                        let mut h = s.node.head.borrow_mut();
                        if h.is_none_or(|(have, _)| seq > have) {
                            *h = Some((seq, root));
                        }
                        h.expect("just written")
                    };
                    next.push(Inbound::GotHead {
                        seq: answer.0,
                        root: answer.1,
                    });
                }
                engine_delegate::schedule::Op::ReadHead { .. } => {
                    self.record(Served::ReadHead);
                    let have = {
                        let s = self.0.borrow();
                        let h = *s.node.head.borrow();
                        h
                    };
                    next.push(match have {
                        Some((seq, root)) => Inbound::GotHead { seq, root },
                        None => Inbound::NoHead,
                    });
                }
            }
        }

        self.0.borrow_mut().rec.event(Event::Exit {
            site: FULL_NODE,
            op,
            outcome: Outcome::Ok,
        });

        let mut replies = out.replies;
        let (hold, one) = {
            let s = self.0.borrow();
            (s.hold, s.one_answer_per_call)
        };
        if hold {
            self.0.borrow_mut().held.extend(next);
        } else if one {
            for answer in next {
                replies.extend(self.step_bounded(vec![answer], depth + 1));
            }
        } else if !next.is_empty() {
            replies.extend(self.step_bounded(next, depth + 1));
        }
        replies
    }

    fn record(&self, what: Served) {
        *self.0.borrow_mut().served.entry(what).or_insert(0) += 1;
    }

    /// Send a client request and run it to a standstill.
    pub fn client(&mut self, r: &protocol::Request) -> Vec<Vec<u8>> {
        {
            use protocol::Request as Q;
            let key = match r {
                Q::Write { write_id, .. } => Some(format!("w{write_id}")),
                Q::Get { req_id, .. }
                | Q::Range { req_id, .. }
                | Q::ChangesSince { req_id, .. } => Some(format!("r{req_id}")),
                _ => None,
            };
            if let Some(k) = key {
                self.0.borrow_mut().asked.insert(k, false);
            }
        }
        let frame = protocol::encode_request(protocol::CURRENT, r).expect("encodes");
        self.step(vec![Inbound::Client(frame)])
    }

    /// How many of this op kind the node served.
    /// How many times the engine fetched THIS block. The all-zero id is the
    /// one worth asking about: a fetch of it is a question with no answer
    /// (sdk#142).
    pub fn fetches_of(&self, id: &Cid) -> usize {
        self.0.borrow().fetched.get(id).copied().unwrap_or(0)
    }

    pub fn served(&self, what: Served) -> usize {
        self.0.borrow().served.get(&what).copied().unwrap_or(0)
    }

    /// How many DISTINCT op kinds the node has served.
    ///
    /// The number that tells a full-node fixture from a put-only one. A test
    /// asserting only that some op was served passes against a fixture that
    /// answers puts and drops the rest.
    pub fn kinds_served(&self) -> usize {
        self.0.borrow().served.len()
    }

    /// Every reply the shell has produced, decoded.
    pub fn replies(&self) -> Vec<protocol::Reply> {
        self.0
            .borrow()
            .replies
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .collect()
    }

    /// What this node actually received, summed independently of the shell's
    /// own count.
    pub fn bytes_handed_to_this_node(&self) -> u64 {
        self.0.borrow().handed.iter().map(|b| b.len() as u64).sum()
    }

    pub fn blocks_handed_to_this_node(&self) -> usize {
        self.0.borrow().handed.len()
    }

    /// The context size after the last call.
    pub fn context_len(&self) -> usize {
        self.0.borrow().ctx.len()
    }

    /// The tail of what happened, for a failing test.
    pub fn dump(&self, what: &'static str) -> String {
        let s = self.0.borrow();
        instrument::dump::render(&s.rec.recording(), what, 40)
    }

    pub fn line(&self) -> String {
        let s = self.0.borrow();
        format!(
            "calls {} served {:?} context {} B",
            s.calls,
            s.served,
            s.ctx.len()
        )
    }
}

impl craftworks_sdk::Transport for Conn {
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
        self.step(vec![Inbound::Client(request.to_vec())])
    }
}

/// Prints the connection's recording if the thread is panicking.
pub struct DumpConnOnPanic<'a> {
    pub conn: &'a Conn,
    pub what: &'static str,
}

impl Drop for DumpConnOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", self.conn.dump(self.what));
        }
    }
}
