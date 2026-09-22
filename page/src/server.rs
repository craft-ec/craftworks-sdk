//! # The page's protocol SERVER (sdk wiring, main's ruling B)
//!
//! The web Session's `Db` → `CachedStore` → `sdk::Client` speak the page ↔
//! engine protocol. Until now the engine delegate's Shell answered it; here
//! the PAGE does, over [`crate::Page`] (#215): the real engine, whose node
//! work is the client-API executor and whose head the SIGNER signs. Nothing
//! above it changes, so every client-side model still judges it.
//!
//! **Ported VERBATIM from `engine-delegate/src/shell.rs` at b0ab729** (line
//! numbers below are e17711d's; b0ab729 adds only `reads: Vec::new()` and the
//! two `State::Conflict` arms, ported as they are) (main's
//! condition 1) — the protocol half of the Shell, not its node half (the
//! scheduler, read-backs and own-key head signing, which `Page` replaced):
//! * `serve` — `engine-delegate/src/serve.rs` (v5 and above answered
//!   `Unsupported`, as there);
//! * `on_protocol` — shell.rs 796–979; the one change: the event goes to
//!   `Page::event` and its effects are read back through `Page::take_client`;
//! * `reply_from` — shell.rs 1139–1279; the one change: `At` from
//!   `Page::published`;
//! * `attribute` / `step` — shell.rs 1287–1308 / 775–788; `attribute` takes
//!   the client's bytes rather than an `Inbound`;
//! * the identity / already-installed / unserved answers and the per-effect
//!   trace steps — shell.rs `handle`, 606–646 and 671–711;
//! * `as_client` / `version_of` / `session_of` / `as_*`, `reply_bytes`,
//!   `rows_in`, `state_tag` and the page constants — shell.rs 163–296.
//!
//! The differential (`tests/server_differential.rs`) holds it to the Shell:
//! the same requests, the same replies, modulo the head's signer.

use crate::{Answer, Ms, Op, Page};
use engine::{Effect, Event, KeySource, State};
use protocol::{Incoming, Reply, Request};
use std::collections::BTreeMap;

/// shell.rs 163 / 167 / 170.
const MAX_PAGE_ENTRIES: usize = protocol::MAX_PAGE_ENTRIES as usize;
const MAX_PAGE_BYTES: usize = 512 * 1024;
const MAX_STEPS: usize = 64;

/// What leaves one call: the replies for the client.
#[derive(Debug, Default)]
struct Outbound {
    replies: Vec<Vec<u8>>,
}

/// What the page knows about its signer and head, as the Session found it
/// (the Shell's `StoreFacts`, whose secret store is now the signer's).
#[derive(Debug, Clone, Copy, Default)]
pub struct SignerFacts {
    /// The signer holds a key and the Register it signs for (`Provisioned`).
    pub head_writable: bool,
    /// The head Register's instance id, or zero when there is none.
    pub head_id: [u8; 32],
}

pub struct Server {
    pub page: Page,
    facts: SignerFacts,
    client_version: u16,
    speaker: engine::ClientId,
    identity: bool,
    already_installed: bool,
    /// An `Install` arrived at an unprovisioned page: the Session provisions
    /// the SIGNER (`wire::signer::frame_provision`) when it sees this.
    pub installed: Option<usize>,
    pub provisioned: bool,
    unserved: Vec<u64>,
    page_clamp: BTreeMap<u64, u32>,
    tracing: bool,
    trace: Vec<protocol::Reply>,
    tracing_of: Option<protocol::TraceOf>,
    out: Vec<Vec<u8>>,
    /// READS held until the page has recovered its head (sdk#175's twin): an
    /// engine that has not read its head answers from the EMPTY tree it
    /// started on — `Page { entries: [], complete: true }` for data that
    /// exists. Not a change to the ported rules: they run on each read, in
    /// order, the moment the head is read.
    held_reads: Vec<(Request, u16, u64)>,
}

impl Server {
    pub fn new(page: Page, facts: SignerFacts) -> Server {
        Server {
            page,
            facts,
            client_version: 0,
            speaker: as_client(protocol::LEGACY_SESSION, 1),
            identity: false,
            already_installed: false,
            installed: None,
            provisioned: false,
            unserved: Vec::new(),
            page_clamp: BTreeMap::new(),
            tracing: false,
            trace: Vec::new(),
            tracing_of: None,
            out: Vec::new(),
            held_reads: Vec::new(),
        }
    }

    /// The Session learnt more about its signer (after provisioning it).
    pub fn set_facts(&mut self, facts: SignerFacts) {
        self.facts = facts;
    }

    /// A client's protocol frame (shell.rs `handle`, the `Inbound::Client` arm).
    pub fn client(&mut self, bytes: &[u8]) {
        let mut out = Outbound::default();
        self.attribute(bytes);
        match serve(bytes) {
            Served::Do(r, v, session) => {
                if is_read(&r) && !self.page.recovered() {
                    self.held_reads.push((r, v, session));
                } else {
                    self.client_version = self.client_version.max(v);
                    self.speaker = as_client(session, v);
                    self.on_protocol(r);
                }
            }
            Served::Answer(reply) => {
                out.replies.push(reply_bytes(&reply));
            }
        }
        self.drain(&mut out);
        self.answer_call(&mut out);
        self.out.extend(out.replies);
    }

    /// Something the NODE (or the signer) answered, as the web layer decoded it.
    pub fn node(&mut self, a: Answer, now_ms: Ms) {
        let mut out = Outbound::default();
        self.page.answer(a, now_ms);
        self.drain(&mut out);
        self.answer_call(&mut out);
        self.out.extend(out.replies);
    }

    /// The page's clock.
    pub fn tick(&mut self, now_ms: Ms) {
        let mut out = Outbound::default();
        self.page.tick(now_ms);
        self.drain(&mut out);
        self.answer_call(&mut out);
        self.out.extend(out.replies);
    }

    /// Replies for the client, in order.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.out)
    }

    /// Client-API operations for the web layer to frame and send.
    pub fn take_ops(&mut self) -> Vec<Op> {
        self.page.take_ops()
    }

    /// Every client-facing effect the page produced, turned into replies and
    /// trace steps (shell.rs `handle`, 628–660). Reads held for the head run
    /// first, in the order they came, once the head is read.
    fn drain(&mut self, out: &mut Outbound) {
        if self.page.recovered() && !self.held_reads.is_empty() {
            for (r, v, session) in std::mem::take(&mut self.held_reads) {
                self.client_version = self.client_version.max(v);
                self.speaker = as_client(session, v);
                self.on_protocol(r);
            }
        }
        let effects = self.page.take_client();
        self.reply_from(&effects, out);
        self.step(1, protocol::Step::Effects, effects.len() as u64);
        for f in &effects {
            match f {
                Effect::Notify { write_id, state, .. } => {
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
    }

    /// The answers that are about the CALL, not an effect (shell.rs 663–700).
    fn answer_call(&mut self, out: &mut Outbound) {
        if self.identity {
            self.identity = false;
            let (head_seq, head_root) = self.page.published();
            out.replies.push(reply_bytes(&protocol::Reply::Identity {
                engine: concat!("craftworks-engine/", env!("CARGO_PKG_VERSION")).into(),
                key_source: "Provisioned(Test)".into(),
                head_seq,
                head_root,
                head_writable: self.facts.head_writable,
                head_id: self.facts.head_id,
            }));
        }
        if self.already_installed {
            self.already_installed = false;
            out.replies.push(reply_bytes(&protocol::Reply::AlreadyInstalled));
        }
        for req_id in std::mem::take(&mut self.unserved) {
            out.replies.push(reply_bytes(&protocol::Reply::Unavailable { req_id, blocked_on: [0u8; 32] }));
        }
        for r in std::mem::take(&mut self.trace) {
            out.replies.push(reply_bytes(&r));
        }
    }

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
                reads: Vec::new(),
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
                if self.facts.head_writable {
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
        // PORT: the engine is the page's; its effects are read back through
        // `Page::take_client` by the caller, as `handle` read `effects`.
        self.page.event(ev);
        Vec::new()
    }

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
                        // Unreachable here: this path sends no reads. Nothing
                        // was applied, so `Failed` is true to this client.
                        State::Conflict => W::Failed,
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
                    let (seq, root) = self.page.published();
                    let at = protocol::At { seq, root };
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

    fn attribute(&mut self, bytes: &[u8]) {
        if !self.tracing {
            return;
        }
        let Served::Do(r, _, _) = serve(bytes) else {
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
}

/// `engine-delegate/src/serve.rs`, verbatim.
enum Served {
    Do(Request, u16, u64),
    Answer(Reply),
}

fn served() -> Vec<u16> {
    protocol::KNOWN.iter().copied().filter(|v| *v <= protocol::CURRENT).collect()
}

fn serve(bytes: &[u8]) -> Served {
    match protocol::decode_request(bytes) {
        Incoming::Ok(env) if env.version > protocol::CURRENT => Served::Answer(Reply::Unsupported {
            got: env.version,
            known: served(),
        }),
        Incoming::Ok(env) => Served::Do(env.body, env.version, env.session),
        Incoming::Unsupported(got) => Served::Answer(Reply::Unsupported { got, known: served() }),
        Incoming::Dropped(reason) => Served::Answer(Reply::Dropped { reason }),
    }
}

/// shell.rs 180–296, verbatim.
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
        // Never reached on this path: the delegate's writes carry no reads
        // (`reads: Vec::new()` below), so nothing can conflict. Tagged as a
        // failure, which is what it would mean to this client.
        State::Conflict => 5,
    }
}

pub(crate) fn reply_bytes(r: &protocol::Reply) -> Vec<u8> {
    protocol::encode_reply(r).unwrap_or_else(|reason| {
        protocol::encode_reply(&protocol::Reply::Dropped { reason }).expect(
            "a Dropped reply is a handful of bytes; `a_dropped_reply_always_encodes` pins it",
        )
    })
}

fn rows_in(r: &engine::read::ReadResult) -> u64 {
    use engine::read::ReadResult as R;
    match r {
        R::Value(v) => v.is_some() as u64,
        R::Page { entries, .. } => entries.len() as u64,
        R::Delta { changes, .. } => changes.len() as u64,
        R::FullReloadRequired { .. } | R::Unavailable(_) | R::OutOfWarmSpace => 0,
    }
}

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

/// A request that READS the tree (and so must wait for the head).
fn is_read(r: &Request) -> bool {
    matches!(r, Request::Get { .. } | Request::Range { .. } | Request::ChangesSince { .. } | Request::Preload { .. })
}
