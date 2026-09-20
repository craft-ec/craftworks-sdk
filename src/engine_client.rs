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
//! 1. Send whatever [`Client::take_outbound`] hands it.
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
        }
    }

    /// Queue a request. It goes out when the host next asks.
    pub fn send(&mut self, r: &Request) {
        self.outbound
            .push(protocol::encode_request(protocol::CURRENT, r));
    }

    /// Everything waiting to be sent, in order. Drained.
    pub fn take_outbound(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbound)
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
                self.dropped.push(why);
                return;
            }
        };
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
            // A version this build does not serve. Counted, and the client is
            // told: a silence here is a UI that waits for ever.
            Reply::Unsupported { .. } => self.dropped.push(Dropped::NotForUs),
            other => self.replies.push(other),
        }
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
