//! The SDK's client half, as a state machine that does no I/O.
//!
//! The same cut the engine already made, for the same reason and with the same
//! payoff. The delegate shell is sans-IO and the entry point translates; this
//! is the client's side of the same wire, and the host owns the socket.
//!
//! # Why it had to move
//!
//! The first version called a `Transport` that sent a request and returned its
//! replies. That works wherever a caller may block — a Rust test, the live
//! driver — and **cannot exist in a browser**: there is no blocking receive on
//! the main thread, and the worker-plus-`Atomics.wait` route needs
//! cross-origin isolation a plain dev server does not send. So the engine
//! backend was unreachable from the one host it was built for, and nobody
//! noticed because every test could block.
//!
//! Moving the I/O out is not a browser workaround. It means there is ONE state
//! machine — one outbox, one `Lost` policy, one retry rule, one trace
//! assembler — driven identically by a test, by the live driver and by a
//! websocket. The alternative was an async duplicate of all of it, which is
//! where two implementations drift.
//!
//! # What a host has to do
//!
//! Three things, and no decisions:
//!
//! 1. Send whatever the client has waiting — [`Client::take_outbound`] where
//!    a send cannot fail, or [`Client::outbound`] + [`Client::sent`] where it
//!    can, which is any socket.
//! 2. Feed every message that arrives to [`Client::on_inbound`].
//! 3. Look at [`Client::drain_replies`] for answers.
//!
//! Retry, ordering, what to do with a `Lost` write, when to reload — all of it
//! stays here, where it is tested against a transport that reorders,
//! duplicates and drops.

use crate::trace::{Trace, Traces};
use protocol::{Dropped, Reply, Request};

/// What happened that an app might want to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A write from before a restart was not applied, because someone else
    /// changed a key it touched. The newer value stands.
    ///
    /// Surfaced, never a prompt: the decision is already made.
    Conflict { write_id: u64, keys: Vec<Vec<u8>> },
    /// A write will never be applied and re-submitting would not help.
    Failed { write_id: u64 },
    /// Something in a subscribed range changed. An ACCELERATOR: a binding
    /// that never heard this would still be correct, on its backstop.
    Changed {
        sub_id: u64,
        new_root: [u8; 32],
        why: protocol::Why,
    },
    /// The engine has stopped comparing a range. Reload it to retry.
    ///
    /// Surfaced rather than swallowed: a binding whose notifications quietly
    /// stopped looks exactly like one whose data quietly stopped changing.
    Stale { sub_id: u64 },
}

/// The client's side of the wire. No socket, no clock, no blocking.
pub struct Client {
    /// Requests encoded and waiting for the host to send them.
    outbound: Vec<Vec<u8>>,
    /// Replies that arrived and are not pushes.
    replies: Vec<Reply>,
    /// Things the app may want to hear about. Drained, never dropped.
    pub events: Vec<Event>,
    /// Messages this build could not use, BY REASON.
    ///
    /// Counted and not silent, exactly as the delegate counts them. A client
    /// talking to a node in a format it cannot read would otherwise wait for
    /// ever for an answer to a message nobody understood, and the only symptom
    /// would be a UI that never settles.
    pub dropped: Vec<Dropped>,
    /// The diagnostics ring. THE CLIENT HOLDS IT.
    ///
    /// Never the delegate's context — that budget belongs to the commit in
    /// flight (F31) — and never its secret store, whose quota is shared with
    /// KEY MATERIAL: a full diagnostic buffer must not be able to break key
    /// storage. The delegate reports per call and the report rides out; the
    /// recording lives here.
    ///
    /// `None` until a client attaches one, and calls that arrive meanwhile are
    /// COUNTED as a gap rather than silently missing — a bundle that looks
    /// continuous when it is not is worse than one that admits a hole.
    rec: Option<instrument::SyncRecorder>,
    /// Per-call reports that arrived with no recorder attached.
    unrecorded_calls: u64,
    /// Requests that could not be encoded, so were never sent.
    unencodable: usize,
    traces: Traces,
    /// The client's clock, as a function, because this crate compiles to wasm
    /// and to a host binary and must not reach for one of its own.
    now_ms: Option<Box<dyn Fn() -> u64>>,
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    pub fn new() -> Client {
        Client {
            outbound: Vec::new(),
            replies: Vec::new(),
            events: Vec::new(),
            dropped: Vec::new(),
            traces: Traces::default(),
            now_ms: None,
            rec: None,
            unrecorded_calls: 0,
            unencodable: 0,
        }
    }

    /// Attach the diagnostics ring.
    ///
    /// Bounded: it DROPS when full and counts the drop, never grows and never
    /// panics. An unbounded buffer is a leak in a long-lived page, and a probe
    /// that panics turns a passing test — or a working app — into a failure,
    /// which is the worst kind of behaviour change.
    pub fn record_into(&mut self, capacity: usize) {
        self.rec = Some(instrument::SyncRecorder::with_capacity(capacity));
    }

    /// Record that a read was handed an id of the wrong width (see
    /// [`Reads::wrong_width`](crate::store::Reads::wrong_width)). Two numbers
    /// from the vocabulary, 32 or 64 each; nothing from the id or the domain.
    pub fn record_wrong_width(&self, given: usize, wanted: usize) {
        use instrument::{vocab::Key, Entry, Event, OpId, Probe, Site};
        const READ: Site = Site::of("sdk::db::read::id-width");
        let Some(rec) = &self.rec else { return };
        for (key, value) in [(Key::IdWidthGiven, given as u64), (Key::IdWidthWanted, wanted as u64)] {
            rec.event(Event::Counter { site: READ, op: OpId::NONE, entry: Entry { key, value } });
        }
    }

    /// The recording, for a support bundle or a test. A SEPARATE handle: the
    /// trait the recording code holds returns unit and has no read-back, so a
    /// probe can never become an input.
    pub fn recording(&self) -> Option<instrument::SyncRecording<'_>> {
        self.rec.as_ref().map(|r| r.recording())
    }

    /// Per-call reports that arrived before a ring was attached.
    ///
    /// A bundle that omitted these would look continuous across a hole. It is
    /// better to say "there are N calls I did not see" than to present a
    /// gapless story that is not one.
    pub fn unrecorded_calls(&self) -> u64 {
        self.unrecorded_calls
    }

    /// The tail of the recording, for a failure or a support bundle.
    pub fn dump(&self, what: &'static str) -> String {
        match &self.rec {
            Some(r) => {
                let mut s = instrument::dump::render(&r.recording(), what, 40);
                if self.unrecorded_calls > 0 {
                    s.push_str(&format!(
                        "   NOTE: {} call(s) arrived before a recorder was attached and are \
                         NOT in this recording.\n",
                        self.unrecorded_calls
                    ));
                }
                s
            }
            None => format!(
                "no recording attached; {} call report(s) went unrecorded\n",
                self.unrecorded_calls
            ),
        }
    }

    /// Queue a request. It goes out when the host next asks.
    ///
    /// A request that cannot be encoded is NOT queued as an empty frame —
    /// which is what `encode_request` used to hand back, and the engine
    /// answered "Unparseable" with no id to hang it on (craftworks-sdk#136).
    /// It is counted, and `CachedStore` refuses a write that will not fit
    /// before it ever gets here.
    pub fn send(&mut self, r: &Request) {
        match protocol::encode_request(protocol::CURRENT, r) {
            Ok(bytes) => self.outbound.push(bytes),
            Err(_) => self.unencodable += 1,
        }
    }

    /// Requests that could not be encoded, so were never sent.
    pub fn unencodable(&self) -> usize {
        self.unencodable
    }

    /// Everything waiting to be sent, in order. **Drained.**
    ///
    /// For a host whose send CANNOT FAIL — the blocking driver, where
    /// `exchange` either returns replies or panics. A host whose send can fail
    /// must use [`Client::outbound`] and [`Client::sent`] instead, or it will
    /// lose whatever it could not get out: this hands the queue over, and
    /// nothing hands it back.
    pub fn take_outbound(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbound)
    }

    /// Everything waiting to be sent, WITHOUT giving it up.
    ///
    /// For a host whose send can fail — a socket. Look at the queue, send what
    /// you can, then tell the client how many left with [`Client::sent`].
    /// Nothing is lost when a send fails part-way, and the order is the order
    /// the client chose.
    ///
    /// The alternative — take the queue and hand back what did not go — needs
    /// the returned items re-queued at the FRONT, and a host that got that
    /// backwards would reorder writes in a way nothing downstream could
    /// detect. This shape has no such corner.
    pub fn outbound(&self) -> &[Vec<u8>] {
        &self.outbound
    }

    /// The first `n` of [`Client::outbound`] went out. Drop them.
    ///
    /// `n` beyond what is queued is clamped rather than refused: a host that
    /// miscounts is a bug, and panicking in a browser with `panic = abort`
    /// would take the whole SDK instance down for it.
    pub fn sent(&mut self, n: usize) {
        self.outbound.drain(..n.min(self.outbound.len()));
    }

    pub fn outbound_len(&self) -> usize {
        self.outbound.len()
    }

    /// A message arrived.
    ///
    /// Decoding is EXACT — `decode_reply` refuses trailing bytes, because a
    /// prefix that parses is not a message and one message read as another is
    /// how a wrapped value becomes its wrapper's first variant. Anything this
    /// build cannot use is counted by REASON rather than ignored.
    ///
    /// Pushes are routed here rather than returned: a `Changed` answers no
    /// request, and a caller waiting on a read would otherwise have to know to
    /// look for it.
    pub fn on_inbound(&mut self, bytes: &[u8]) {
        let reply = match protocol::decode_reply(bytes) {
            Ok(r) => r,
            Err(why) => {
                self.record_drop(why);
                self.dropped.push(why);
                return;
            }
        };
        self.record_reply(&reply);
        match reply {
            Reply::Changed {
                sub_id,
                new_root,
                why,
                ..
            } => {
                if why == protocol::Why::Stale {
                    self.events.push(Event::Stale { sub_id });
                }
                self.events.push(Event::Changed {
                    sub_id,
                    new_root,
                    why,
                });
            }
            Reply::Step { of, depth, what, n } => {
                // Stamped as it LANDS. There is no other honest moment: the
                // engine has no clock, so the only time anyone can measure is
                // the client's own wait.
                if let Some(now) = &self.now_ms {
                    let t = now();
                    self.traces.record(of, depth, what, n, t);
                }
            }
            // A diagnostic, not an answer to anything. It is RECORDED above
            // and must not reach the reply queue: a caller draining replies is
            // looking for the answer to its request, and an extra message in
            // that queue is a reply to somebody else's question.
            Reply::CallBytes { .. } => {}
            // A version this build does not serve. Counted, and the client is
            // told: a silence here is a UI that waits for ever.
            Reply::Unsupported { .. } => {
                self.record_drop(Dropped::NotForUs);
                self.dropped.push(Dropped::NotForUs)
            }
            other => self.replies.push(other),
        }
    }

    /// Record a per-call report. The counts the engine already had, and
    /// nothing else.
    ///
    /// `Reply::Call` exists because a delegate prints nothing and the node says
    /// nothing about it, so five different breaks in the write path all
    /// presented as one symptom — a write that stops at `Accepted`. Until now
    /// the engine emitted it and nothing read it, which means a page had no
    /// answer to "why did my write stop".
    ///
    /// Every field here is a NUMBER the engine computed. No key, no value, no
    /// domain name: this recording ships as the production support tool, so
    /// the rule is vocabulary, not type — `domain = "medical"` is a category
    /// and no grep for a key format would find it.
    fn record_reply(&mut self, reply: &Reply) {
        use instrument::{vocab::Key, Entry, Event, OpId, Probe, Site};
        const CALL: Site = Site::of("sdk::delegate::call");

        // The node-op counts, from their own message.
        if let Reply::CallBytes {
            put_bytes,
            puts,
            gets,
        } = reply
        {
            let Some(rec) = &self.rec else {
                self.unrecorded_calls += 1;
                return;
            };
            for (key, value) in [
                (Key::BytesOut, *put_bytes),
                (Key::Ops, (*puts as u64) + (*gets as u64)),
            ] {
                rec.event(Event::Counter {
                    site: CALL,
                    op: OpId::NONE,
                    entry: Entry { key, value },
                });
            }
            return;
        }
        let Reply::Call {
            effects,
            ops,
            awaiting,
            read_back,
            stranded,
            dropped,
            ..
        } = reply
        else {
            return;
        };
        let Some(rec) = &self.rec else {
            // No ring attached. COUNTED, so a bundle cannot look continuous
            // across the hole.
            self.unrecorded_calls += 1;
            return;
        };
        for (key, value) in [
            (Key::Effects, *effects as u64),
            (Key::Ops, *ops as u64),
            (Key::Awaiting, *awaiting as u64),
            (Key::ReadBack, *read_back as u64),
            (Key::Stranded, *stranded as u64),
            (Key::DroppedMsgs, *dropped as u64),
        ] {
            rec.event(Event::Counter {
                site: CALL,
                op: OpId::NONE,
                entry: Entry { key, value },
            });
        }
    }

    /// Record why a message was discarded, as a vocabulary value.
    fn record_drop(&mut self, why: Dropped) {
        use instrument::{
            vocab::{DropReason, Key},
            Entry, Event, OpId, Probe, Site,
        };
        const DROP: Site = Site::of("sdk::client::dropped");
        let Some(rec) = &self.rec else {
            return;
        };
        // The protocol's reason mapped onto the instrument's closed one. Both
        // are closed lists on purpose: a free-text reason is the easiest place
        // for user data to arrive by accident, and this one crosses into a
        // support bundle.
        let reason = match why {
            Dropped::Unparseable => DropReason::Unparseable,
            Dropped::TrailingBytes => DropReason::TrailingBytes,
            Dropped::TooLarge => DropReason::TooLarge,
            Dropped::Unexpected => DropReason::Unexpected,
            Dropped::NotForUs => DropReason::NotForUs,
        };
        rec.event(Event::Counter {
            site: DROP,
            op: OpId::NONE,
            entry: Entry {
                key: Key::DroppedMsgs,
                value: reason.code(),
            },
        });
    }

    /// The replies that have arrived since this was last called.
    pub fn drain_replies(&mut self) -> Vec<Reply> {
        std::mem::take(&mut self.replies)
    }

    pub fn has_replies(&self) -> bool {
        !self.replies.is_empty()
    }

    /// Take the events that have accumulated. Drained, never dropped.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Turn the call tree on, with the clock the app already uses.
    ///
    /// The clock is HANDED IN. The engine has none — it is sans-IO, which is
    /// what makes it testable — so every duration in a trace is the client's
    /// own measurement of its own wait, which is the number an app cares
    /// about anyway.
    pub fn trace_on(&mut self, now_ms: Box<dyn Fn() -> u64>) {
        self.now_ms = Some(now_ms);
        self.send(&Request::Trace { on: true });
    }

    pub fn trace_off(&mut self) {
        self.send(&Request::Trace { on: false });
        self.now_ms = None;
    }

    /// The call tree for one operation, if it was traced.
    pub fn trace(&self, of: protocol::TraceOf) -> Option<&Trace> {
        self.traces.of(of)
    }
}
