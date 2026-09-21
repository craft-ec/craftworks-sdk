//! The probed fixtures: the only supported way to build the thing under test.
//!
//! # Why this crate exists
//!
//! Every test that built the system itself built it a little differently, and
//! the differences decided what the tests could SEE.
//!
//! - **A clock that never advanced, written out eleven times.**
//!   `CachedStore::new(Box::new(|| 0))` appears in six files. Nothing in the
//!   SDK's tests could observe time passing, so a whole class of behaviour was
//!   untestable by construction rather than by decision.
//! - **The cross-call driver, duplicated verbatim in two files, and absent
//!   from the third.** `tests/what_tick_causes.rs` and
//!   `tests/what_a_flush_causes.rs` each carry their own loop that rebuilds
//!   `Shell` from its context on every call. Those two files found four
//!   defects (`in_flight_since`, `told_stalled`, `in_flight_parity`, the
//!   delegate scheduler's confirmed set). `engine/tests/common/mod.rs` drives
//!   the engine straight through in ONE process and structurally cannot see
//!   any of them.
//!
//! That second point decides the shape of this crate. **Every call is a
//! rehydration**: state that only matters across calls has to be in the
//! context or it does not exist (F32). A fixture that offers only the
//! one-process driver keeps that class of defect invisible, so here the
//! cross-call [`Node`] is the DEFAULT and the straight-through driver is the
//! variant you have to ask for by name.
//!
//! # What "probed" buys
//!
//! Each fixture carries a recorder and a panic guard. A test that fails prints
//! the tail of what actually happened, with no re-run and no one-off `dbg!`.
//! The recording is read through a separate handle — the trait the code under
//! test holds returns unit and has no read-back, so a probe can never become
//! an input.

pub mod full_node;
pub use full_node::{Conn, DumpConnOnPanic, FullNode, Served};

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use instrument::{
    vocab::{Dir, Key, Outcome, Site},
    Entry, Event, OpId, Probe, Record, Recorder,
};

pub const NODE: Site = Site::of("testkit::node");
pub const CLOCK: Site = Site::of("testkit::clock");

/// A clock a test can DRIVE.
///
/// The thing it replaces is `Box::new(|| 0)` — a closure returning zero,
/// written by hand in six files. It is not that those tests chose a frozen
/// clock; it is that constructing one that moves was extra work in every
/// single test, so nobody did, and "does this behaviour need time?" became a
/// question the suite could not ask.
#[derive(Clone, Default)]
pub struct Clock(Rc<RefCell<u64>>);

impl Clock {
    pub fn new(start_ms: u64) -> Clock {
        Clock(Rc::new(RefCell::new(start_ms)))
    }
    /// Move time forward. The whole point of the type.
    pub fn advance(&self, ms: u64) {
        *self.0.borrow_mut() += ms;
    }
    pub fn now_ms(&self) -> u64 {
        *self.0.borrow()
    }
    /// The closure the SDK's constructors take.
    pub fn as_fn(&self) -> Box<dyn Fn() -> u64> {
        let c = self.0.clone();
        Box::new(move || *c.borrow())
    }
}

/// The block store the delegate reads through.
#[derive(Clone, Default)]
pub struct Store(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for Store {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl Store {
    pub fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }
}

/// A node that rebuilds the shell from its context on EVERY call.
///
/// This is the default fixture, and the reason is structural rather than
/// stylistic. A delegate has no clock and no memory between calls (F32); every
/// call rehydrates it from bytes. So a defect in what the context carries is
/// invisible to any driver that keeps the engine alive in one process, and
/// visible — immediately — to this one. The two files in this repo that used
/// this shape found four such defects; the file that drives straight through
/// found none, and could not have.
///
/// If a test wants the straight-through driver it asks for
/// [`step_in_one_process`](Node::step_in_one_process) by name. Making the
/// blind shape the one you have to spell out is the enforcement; a comment
/// recommending the other would not be.
pub struct Node {
    pub store: Store,
    pub clock: Clock,
    ctx: Vec<u8>,
    /// While true, a put is never acknowledged: the commit sits, which is the
    /// state `Stalled` exists to report.
    deaf: bool,
    puts: usize,
    rec: Recorder,
    calls: u32,
    /// Ids put on the last call, so the fixture can acknowledge them.
    last_put_ids: Vec<Cid>,
    /// The protocol version the shell DETECTED on the last call.
    ///
    /// The input to the gate that decides whether a v2-only message may be
    /// sent. A reply carries no version of its own, so this — read from the
    /// envelope the client sent on the way in — is the only thing that knows
    /// what the far side can read.
    client_version: u16,
    /// Every reply the shell produced, in order.
    ///
    /// `step` returns the BLOCKS to feed back as acknowledgements, so the
    /// replies had nowhere to go and were dropped. A fixture that silently
    /// discards half of what the thing under test produces can only test the
    /// other half.
    replies: Vec<Vec<u8>>,
    /// Every block this node actually took from the shell, in order.
    ///
    /// Kept so a test can compute what the call SHOULD have counted from a
    /// different path than the shell's own tally. A cost counter checked only
    /// against another view of itself is unfalsifiable: zero equals zero.
    handed: Vec<Vec<u8>>,
}

impl Default for Node {
    fn default() -> Self {
        Node::new()
    }
}

impl Node {
    /// The probed fixture. There is no unprobed constructor that is easy to
    /// reach — see [`new_unprobed_for_benchmark`](Node::new_unprobed_for_benchmark).
    pub fn new() -> Node {
        Node {
            store: Store::default(),
            clock: Clock::new(0),
            ctx: Vec::new(),
            deaf: false,
            puts: 0,
            rec: Recorder::with_capacity(1 << 14),
            calls: 0,
            last_put_ids: Vec::new(),
            client_version: 0,
            replies: Vec::new(),
            handed: Vec::new(),
        }
    }

    /// A node whose recording is a stub, for a benchmark measuring the engine
    /// itself.
    ///
    /// Named to be noticed and to be awkward. Its job is to be the ONLY bare
    /// path, so that choosing it is a deliberate act a reviewer can see — the
    /// architect ranked removing the bare path above any rule about
    /// remembering to use the fixture.
    pub fn new_unprobed_for_benchmark() -> Node {
        Node {
            rec: Recorder::with_capacity(1),
            ..Node::new()
        }
    }

    /// Stop acknowledging puts, so a commit cannot publish.
    pub fn go_deaf(&mut self) {
        self.deaf = true;
    }
    pub fn hear_again(&mut self) {
        self.deaf = false;
    }
    pub fn puts(&self) -> usize {
        self.puts
    }

    /// One call: rehydrate, handle, serialise the context back.
    ///
    /// The rehydration is not an implementation detail of the fixture — it IS
    /// the fixture. Anything the engine wanted to remember and did not put in
    /// the context is gone by the next line.
    pub fn step(&mut self, inbound: Vec<Inbound>) -> Vec<Vec<u8>> {
        self.calls += 1;
        let op = OpId(self.calls);
        self.rec.event(Event::Enter { site: NODE, op });
        self.rec.event(Event::Counter {
            site: CLOCK,
            op,
            entry: Entry {
                key: Key::OffsetMs,
                value: self.clock.now_ms(),
            },
        });

        let mut shell: Shell<Store> = Shell::resume_with(
            &self.ctx,
            engine::Params::default(),
            self.store.clone(),
            StoreFacts::provisioned(),
        );
        let out = shell.handle(inbound);
        self.ctx = shell.to_context().expect("a context after every call");
        self.replies.extend(out.replies.iter().cloned());
        self.client_version = out.client_version;

        // What this call handed to the node, counted by the shell itself and
        // recorded HERE — the client side holds the ring, never the delegate
        // (its context budget belongs to the commit in flight, and its secret
        // store's quota is shared with key material).
        for (key, value) in [
            (Key::BytesOut, out.put_bytes as u64),
            (Key::Ops, (out.puts + out.gets) as u64),
            (Key::Effects, out.effects as u64),
            (Key::Awaiting, out.awaiting as u64),
            (Key::ReadBack, out.read_back_hits as u64),
            (Key::Stranded, out.stranded as u64),
        ] {
            self.rec.event(Event::Counter {
                site: NODE,
                op,
                entry: Entry { key, value },
            });
        }

        // What the rehydration cost is NOT recorded as BytesOut.
        //
        // It was, and that was a defect of exactly the kind this crate exists
        // to prevent: the context's length and the bytes handed to the node
        // are different quantities, and putting both under one key made
        // `put_bytes()` the sum of two unrelated things. It read 191 for a
        // call that handed the node nothing. Two meanings under one key is the
        // same hazard as two methods a letter apart, and it is invisible until
        // something asks the number to equal an independently computed one.
        //
        // The context size is available as `context_len()`, which cannot be
        // confused with a byte count on the wire.

        let mut next = Vec::new();
        for o in out.ops {
            if let engine_delegate::schedule::Op::Put { id, bytes } = o {
                self.puts += 1;
                self.rec.event(Event::Edge {
                    site: NODE,
                    dir: Dir::Request,
                    id: instrument::Label {
                        kind: instrument::label::Kind::Block,
                        ordinal: self.puts as u32,
                    },
                });
                if self.deaf {
                    continue;
                }
                self.last_put_ids.push(id);
                self.handed.push(bytes.clone());
                self.store.put(id, &bytes);
                next.push(bytes);
            }
        }
        self.rec.event(Event::Exit {
            site: NODE,
            op,
            outcome: Outcome::Ok,
        });
        next
    }

    /// Send a client request and drive the node's answers back in until it
    /// settles.
    ///
    /// The multi-call part is the point. A write is not one call: the shell
    /// issues puts, the node acknowledges them on a LATER call, and the
    /// read-back lands later still. That span is exactly what a one-process
    /// driver cannot see.
    pub fn client(&mut self, r: &protocol::Request) -> Vec<Vec<u8>> {
        let frame = protocol::encode_request(protocol::CURRENT, r).expect("encodes");
        let mut out = self.step(vec![Inbound::Client(frame)]);
        for _ in 0..6 {
            let acks: Vec<Inbound> = std::mem::take(&mut self.last_put_ids)
                .into_iter()
                .map(|id| Inbound::PutAcked { id, ok: true })
                .collect();
            if acks.is_empty() {
                break;
            }
            out.extend(self.step(acks));
        }
        out
    }

    /// The straight-through driver, kept and named so that using it is a
    /// choice.
    ///
    /// It does NOT rehydrate, so it cannot see a defect in what the context
    /// carries — which is four of the defects this repo has actually had. Use
    /// it only when the question is genuinely about one call.
    pub fn step_in_one_process(&mut self, shell: &mut Shell<Store>, inbound: Vec<Inbound>) {
        let _ = shell.handle(inbound);
    }

    /// The recording, for a test to read. Separate handle on purpose: the
    /// trait the code under test holds has no read-back.
    pub fn recording(&self) -> instrument::Recording<'_> {
        self.rec.recording()
    }

    /// Bytes this node handed to the delegate's node ops, and the ops counted,
    /// read back from the RECORDING rather than from a tally kept beside it.
    pub fn put_bytes(&self) -> u64 {
        self.recording()
            .events()
            .iter()
            .filter_map(|e| match e {
                Event::Counter { entry, .. } if entry.key == Key::BytesOut => Some(entry.value),
                _ => None,
            })
            .sum()
    }

    /// The version the shell detected on the last call. Zero when no client
    /// message arrived — a tick, or a node answer.
    pub fn detected_client_version(&self) -> u16 {
        self.client_version
    }

    /// Every reply the shell has produced, decoded.
    pub fn replies(&self) -> Vec<protocol::Reply> {
        self.replies
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .collect()
    }

    /// What this node actually received, summed independently of the shell's
    /// own count. The expectation a cost counter needs in order to be
    /// falsifiable.
    pub fn bytes_handed_to_this_node(&self) -> u64 {
        self.handed.iter().map(|b| b.len() as u64).sum()
    }

    /// How many blocks this node received.
    pub fn blocks_handed_to_this_node(&self) -> usize {
        self.handed.len()
    }

    /// The context size after the last call — the budget most likely to be
    /// quietly exceeded.
    pub fn context_len(&self) -> usize {
        self.ctx.len()
    }

    /// One line, a PROJECTION of the recording rather than counters kept
    /// beside it.
    pub fn line(&self) -> String {
        let r = self.recording();
        format!(
            "calls {} puts {} context {} B unfinished {}",
            self.calls,
            self.puts,
            self.ctx.len(),
            r.unfinished().len()
        )
    }

    /// Print the tail of the stream when a test is failing.
    pub fn dump(&self, what: &'static str) -> String {
        instrument::dump::render(&self.recording(), what, 40)
    }
}

/// Prints the node's recording if the thread is panicking.
///
/// This is why the probed path is the EASY path: a test that uses the fixture
/// gets its failure explained for free. Enforcement that relies on people
/// wanting the better tool outlasts enforcement that nags.
pub struct DumpOnPanic<'a> {
    pub node: &'a Node,
    pub what: &'static str,
}

impl Drop for DumpOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", self.node.dump(self.what));
        }
    }
}

/// A `CachedStore` whose clock can be DRIVEN, with the clock handed back.
///
/// Replaces `CachedStore::new(Box::new(|| 0))` — a clock frozen at zero,
/// written out by hand in six files. Nothing in those tests could observe time
/// passing, so "does this behaviour need time?" was a question the suite could
/// not ask. Handing the clock back is the point: a fixture that hid it would
/// be the same frozen clock with better manners.
pub fn cached_store() -> (craftworks_sdk::CachedStore, Clock) {
    let clock = Clock::new(0);
    (craftworks_sdk::CachedStore::new(clock.as_fn()), clock)
}

/// A `CachedStore` on a clock the caller already holds — so several
/// sessions share ONE clock, and advancing it moves them all (the write
/// path's model test runs two).
pub fn cached_store_on(clock: &Clock) -> craftworks_sdk::CachedStore {
    craftworks_sdk::CachedStore::new(clock.as_fn())
}

/// The same, started at a given time.
pub fn cached_store_at(start_ms: u64) -> (craftworks_sdk::CachedStore, Clock) {
    let clock = Clock::new(start_ms);
    (craftworks_sdk::CachedStore::new(clock.as_fn()), clock)
}
