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
    /// sdk#225b: each write handed to the engine, by (client, write id), as the
    /// FINAL value it leaves at each key (`None`: deleted), until it ends.
    sent: BTreeMap<WriteKey, Sent>,
    /// The writes of this page's LATEST Published commit and the head it was
    /// built on: what a same-identity displacement can take (only the tip can
    /// be displaced at its own seq; a higher seq built on it keeps it).
    tip: Option<Tip>,
    /// The published head last seen, to tell an ADOPTION (it moved with no
    /// Published of ours at the new head) from a commit of ours.
    seen_head: (u64, freenet_prolly::Cid),
    /// A displaced tip being judged key by key at the winner.
    probe: Option<Probe>,
    next_probe: u64,
    /// A same-seq race being MERGED (cell B).
    merge: Option<Merge>,
}

/// THE MERGE of a same-seq race (sdk#225b part 2, cell B): the winner was
/// signed from the SAME base P as this page's tip. Their changes P→winner are
/// read from the engine (`ChangesSince`, resumed until COMPLETE — a cut delta
/// is unknown, never "not theirs"); per key of the tip:
/// * changed only here, and every read its write declared unchanged there →
///   KEPT, re-applied on the winner;
/// * changed there to exactly this page's value → neither (already there);
/// * changed there otherwise, or a read of its write changed there →
///   SUPERSEDED, the winner's value stands, told.
///
/// The kept keys go as ONE write WITH READS — the kept writes' own declared
/// reads, still true at the winner since it did not change them — so the
/// engine compares them where the write lands and a winner that moved on
/// refuses it (R0): never blind. `Lost` (the head moved again) re-sends, up to
/// [`MERGE_ROUNDS`]; then, or on `Conflict`/`Failed`, the kept keys are
/// superseded too.
struct Merge {
    winner: (u64, freenet_prolly::Cid),
    writes: TipWrites,
    left: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    from: freenet_prolly::Cid,
    /// The delta read in flight, and what has come back of it.
    req: u64,
    theirs: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Decided: what is kept (the merge write's ops and reads) and what not.
    kept: Vec<Vec<u8>>,
    reads: Reads,
    superseded: std::collections::BTreeSet<Vec<u8>>,
    /// The merge write, once sent: its id, and rounds sent.
    write: Option<u64>,
    rounds: u8,
    resend: bool,
}

/// How many times a merge write is sent against a head that keeps moving.
const MERGE_ROUNDS: u8 = 3;

/// The engine client the merge commit is written under: session 0, never a
/// real session — so its verdicts are the Server's, never a client's.
const MERGE_CLIENT: engine::ClientId = engine::ClientId(1);

/// What a write leaves at each key, in key order (`None`: deleted).
type Finals = Vec<(Vec<u8>, Option<Vec<u8>>)>;
/// A write by (engine client, write id).
type WriteKey = (u64, u64);
/// What a write declared it READ.
type Reads = Vec<(Vec<u8>, engine::Expect)>;
/// A write as the Server keeps it: what it left at each key, and what it read.
#[derive(Clone)]
struct Sent {
    finals: Finals,
    reads: Reads,
}
/// A commit's writes.
type TipWrites = Vec<(WriteKey, Sent)>;

/// A Published commit's writes (sdk#225b).
struct Tip {
    head: (u64, freenet_prolly::Cid),
    /// The head this commit was signed from (its ledger's PREV): the base a
    /// same-seq race is merged on.
    base: Option<(u64, freenet_prolly::Cid)>,
    writes: TipWrites,
}

/// The tip's keys, read at the head that displaced it (sdk#225b, cell C —
/// and cell B until the merge commit exists): a key that holds exactly what
/// this page's write left is KEPT (they built on it, or wrote the same); any
/// other value, or a read that could not be answered, is SUPERSEDED, told.
struct Probe {
    winner: (u64, freenet_prolly::Cid),
    writes: TipWrites,
    /// What the tip left at each key: ONE value per key, because every tip
    /// write agrees there. A commit carries ONE write (the engine refuses a
    /// second with `Busy`; `folded` is a handoff, never a queue), and the
    /// tip's other writes are no-op writes Published at the same head
    /// (sdk#160), whose value at every key IS the tree's there. Debug-asserted.
    left: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    pending: BTreeMap<u64, Vec<u8>>,
    superseded: std::collections::BTreeSet<Vec<u8>>,
}

/// The engine client the Server reads under for itself: session 0 is never a
/// real session (`protocol::session_is_valid`), so no client's reply is ever
/// taken for a probe's.
const PROBE_CLIENT: engine::ClientId = engine::ClientId(0);

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
            sent: BTreeMap::new(),
            tip: None,
            seen_head: (0, [0; 32]),
            probe: None,
            next_probe: 1,
            merge: None,
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
                // A read before the head is recovered is PARKED by the engine
                // itself (sdk#223) and answered in order once it is; this
                // Server held it until then, and no longer needs to.
                self.client_version = self.client_version.max(v);
                self.speaker = as_client(session, v);
                self.on_protocol(r);
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
    /// The node's `HeadChanged` for the head register: a hint the page acts
    /// on by READING the register (sdk#225's reload trigger).
    pub fn head_hint(&mut self) {
        let mut out = Outbound::default();
        self.page.head_hint();
        self.drain(&mut out);
        self.answer_call(&mut out);
        self.out.extend(out.replies);
    }

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
    /// trace steps (shell.rs `handle`, 628–660).
    fn drain(&mut self, out: &mut Outbound) {
        let mut effects = self.page.take_client();
        // A merge write the engine was too busy for goes again NOW — at the
        // start of a later call, never inside the one that was refused, where
        // nothing can have freed the engine (it would only be refused again).
        if self.merge.as_ref().is_some_and(|m| m.resend) {
            self.send_merge();
            effects.extend(self.page.take_client());
        }
        self.same_identity(&mut effects, out);
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

    /// THE SAME IDENTITY, DISPLACED (sdk#225b), before the effects become
    /// replies: the Server's own probe answers are taken out; a Published
    /// write joins the tip; and when the published head moved to a head that
    /// is NOT one of this page's commits, that ADOPTION is classified:
    ///
    /// * the winner names the tip as its `prev` → they built on it: nothing
    ///   was displaced;
    /// * anything else (a same-seq race, another prev, no prev, a refused
    ///   ledger) → each of the tip's keys is READ at the winner: what holds
    ///   exactly this page's value stands (they built on it deeper, or wrote
    ///   the same), everything else is SUPERSEDED and told to its write's
    ///   session. The per-key MERGE (re-applying one-sided keys on the
    ///   winner) replaces the second arm for a same-seq race next.
    fn same_identity(&mut self, effects: &mut Vec<Effect>, out: &mut Outbound) {
        let now = self.page.published();
        let mut published_here = false;
        let mut merge_done = false;
        // The Server's own reads and the merge write's verdicts: never a
        // client's, never replied.
        let mut own = Vec::new();
        effects.retain(|f| match f {
            Effect::Reply { client, .. } if *client == PROBE_CLIENT => {
                own.push(f.clone());
                false
            }
            Effect::Notify { client, .. } if *client == MERGE_CLIENT => {
                own.push(f.clone());
                false
            }
            _ => true,
        });
        for f in own {
            match f {
                Effect::Reply { req_id, result, .. } => self.on_own_read(req_id.0, result),
                Effect::Notify { write_id, state, .. } if self.on_merge_verdict(write_id.0, state, now) => {
                    published_here = true;
                    merge_done = true;
                }
                _ => {}
            }
        }
        for f in effects.iter() {
            if let Effect::Notify { client, write_id, state } = f {
                let id = (client.0, write_id.0);
                match state {
                    State::Published => {
                        if let Some(sent) = self.sent.remove(&id) {
                            published_here = true;
                            match self.tip.as_mut() {
                                Some(t) if t.head == now => t.writes.push((id, sent)),
                                _ => {
                                    let base = self.page.last_read().filter(|r| (r.seq, r.root()) == now).and_then(|r| r.prev());
                                    self.tip = Some(Tip { head: now, base, writes: vec![(id, sent)] });
                                }
                            }
                        }
                    }
                    State::Failed | State::Lost | State::Conflict | State::TooLarge { .. } => {
                        self.sent.remove(&id);
                    }
                    _ => {}
                }
            }
        }
        if now != self.seen_head && !published_here {
            if let Some(tip) = self.tip.take() {
                if now.0 >= tip.head.0 && now != tip.head {
                    let prev = self.page.last_read().filter(|r| (r.seq, r.root()) == now).and_then(|r| r.prev());
                    match (prev, tip.base) {
                        // They built on the tip: nothing displaced.
                        (Some(p), _) if p == tip.head => {}
                        // A same-seq race from the SAME base: merge.
                        (Some(p), Some(b)) if p == b && now.0 == tip.head.0 => self.start_merge(now, tip.writes, b.1),
                        // Anything else: judge each key at the winner.
                        _ => self.start_probe(now, tip.writes),
                    }
                } else {
                    self.tip = Some(tip);
                }
            }
        }
        self.seen_head = now;
        if merge_done {
            self.finish_merge(out);
        }
        // The Server's own reads (or the merge write) may have answered at once.
        let more = self.page.take_client();
        if !more.is_empty() {
            let mut more = more;
            self.same_identity(&mut more, out);
            effects.extend(more);
        }
        self.finish_probe(out);
    }

    /// One of the Server's own reads answered: a probe's `Get`, or a page of
    /// the merge's delta.
    fn on_own_read(&mut self, req: u64, result: engine::read::ReadResult) {
        if let Some(p) = self.probe.as_mut() {
            if let Some(key) = p.pending.remove(&req) {
                let holds = matches!(&result, engine::read::ReadResult::Value(v) if Some(v) == p.left.get(&key));
                if !holds {
                    p.superseded.insert(key);
                }
                return;
            }
        }
        let Some(m) = self.merge.as_mut().filter(|m| m.req == req && m.write.is_none()) else { return };
        match result {
            engine::read::ReadResult::Delta { changes, cursor, .. } => {
                m.theirs.extend(changes);
                match cursor {
                    // CUT: not "absent in theirs" — resume after it.
                    Some(c) => {
                        let rid = self.next_probe;
                        self.next_probe += 1;
                        m.req = rid;
                        let from = m.from;
                        self.page.event(Event::ChangesSince {
                            client: PROBE_CLIENT,
                            req_id: as_req_id(rid),
                            from,
                            range: engine::subs::SubRange { lo: std::ops::Bound::Excluded(c), hi: std::ops::Bound::Unbounded },
                            max_entries: 256,
                        });
                    }
                    None => self.decide_merge(),
                }
            }
            // The base's blocks are gone, or a block could not be had: the
            // delta cannot be known, so the keys are judged one by one at the
            // winner instead (the conservative cell).
            _ => {
                let m = self.merge.take().expect("checked");
                self.start_probe(m.winner, m.writes);
            }
        }
    }

    fn start_merge(&mut self, winner: (u64, freenet_prolly::Cid), writes: TipWrites, from: freenet_prolly::Cid) {
        let rid = self.next_probe;
        self.next_probe += 1;
        self.merge = Some(Merge {
            winner,
            left: left_of(&writes),
            writes,
            from,
            req: rid,
            theirs: BTreeMap::new(),
            kept: Vec::new(),
            reads: Vec::new(),
            superseded: Default::default(),
            write: None,
            rounds: 0,
            resend: false,
        });
        self.page.event(Event::ChangesSince {
            client: PROBE_CLIENT,
            req_id: as_req_id(rid),
            from,
            range: engine::subs::SubRange { lo: std::ops::Bound::Unbounded, hi: std::ops::Bound::Unbounded },
            max_entries: 256,
        });
    }

    /// Their COMPLETE delta is in: decide per key, then send the kept keys.
    fn decide_merge(&mut self) {
        let Some(m) = self.merge.as_mut() else { return };
        // A write whose declared read moved on their side had a stale
        // premise: ALL its keys are superseded (main's rule).
        let stale: Vec<bool> = m.writes.iter().map(|(_, w)| w.reads.iter().any(|(k, _)| m.theirs.contains_key(k))).collect();
        let mut kept = Vec::new();
        for (k, v) in &m.left {
            let premise_ok = m.writes.iter().zip(&stale).filter(|((_, w), _)| w.finals.iter().any(|(wk, _)| wk == k)).all(|(_, s)| !*s);
            match m.theirs.get(k) {
                Some(t) if t == v => {}
                Some(_) => {
                    m.superseded.insert(k.clone());
                }
                None if premise_ok => kept.push(k.clone()),
                None => {
                    m.superseded.insert(k.clone());
                }
            }
        }
        let mut reads: BTreeMap<Vec<u8>, engine::Expect> = BTreeMap::new();
        for ((_, w), s) in m.writes.iter().zip(&stale) {
            if !*s && w.finals.iter().any(|(k, _)| kept.contains(k)) {
                for (k, e) in &w.reads {
                    reads.insert(k.clone(), e.clone());
                }
            }
        }
        m.kept = kept;
        m.reads = reads.into_iter().collect();
        if m.kept.is_empty() {
            // Nothing to re-apply: told now.
            m.write = Some(0);
            let mut out = Outbound::default();
            self.finish_merge(&mut out);
            self.out.extend(out.replies);
        } else {
            self.send_merge();
        }
    }

    /// The kept keys as ONE engine write, with the reads the engine checks
    /// where it lands.
    fn send_merge(&mut self) {
        let wid = self.next_probe;
        self.next_probe += 1;
        let Some(m) = self.merge.as_mut() else { return };
        m.resend = false;
        m.write = Some(wid);
        let ops: Vec<(Vec<u8>, engine::Op)> = m
            .kept
            .iter()
            .map(|k| (k.clone(), match m.left.get(k).cloned().flatten() {
                Some(v) => engine::Op::Put(v),
                None => engine::Op::Delete,
            }))
            .collect();
        let reads = m.reads.clone();
        self.page.event(Event::Write { client: MERGE_CLIENT, write_id: as_write_id(wid), ops, reads });
    }

    /// A verdict on the merge write. True when it PUBLISHED.
    fn on_merge_verdict(&mut self, write_id: u64, state: State, now: (u64, freenet_prolly::Cid)) -> bool {
        let Some(m) = self.merge.as_mut().filter(|m| m.write == Some(write_id)) else { return false };
        match state {
            State::Published => {
                // The merge commit is this page's tip now: its kept keys.
                let kept = m.kept.clone();
                let writes: TipWrites = m
                    .writes
                    .iter()
                    .filter_map(|(id, w)| {
                        let finals: Finals = w.finals.iter().filter(|(k, _)| kept.contains(k)).cloned().collect();
                        (!finals.is_empty()).then(|| (*id, Sent { finals, reads: w.reads.clone() }))
                    })
                    .collect();
                self.tip = Some(Tip { head: now, base: Some(m.winner), writes });
                true
            }
            // The engine had another commit: the same write again, later —
            // not a round (only a head that moved costs one).
            State::Busy => {
                m.resend = true;
                false
            }
            State::Lost if m.rounds + 1 < MERGE_ROUNDS => {
                m.rounds += 1;
                m.resend = true;
                false
            }
            State::Lost | State::Conflict | State::Failed | State::TooLarge { .. } => {
                // It could not land where it was judged: the kept keys are
                // superseded too — told, never blind.
                let kept = std::mem::take(&mut m.kept);
                m.superseded.extend(kept);
                let mut out = Outbound::default();
                self.finish_merge(&mut out);
                self.out.extend(out.replies);
                false
            }
            _ => false,
        }
    }

    /// The merge is over: tell each write's session its superseded keys.
    fn finish_merge(&mut self, out: &mut Outbound) {
        let Some(m) = self.merge.take() else { return };
        tell_superseded(out, m.winner, &m.writes, &m.superseded);
    }

    fn start_probe(&mut self, winner: (u64, freenet_prolly::Cid), writes: TipWrites) {
        let left = left_of(&writes);
        let keys: Vec<Vec<u8>> = left.keys().cloned().collect();
        let mut pending = BTreeMap::new();
        let mut reqs = Vec::new();
        for key in keys {
            let rid = self.next_probe;
            self.next_probe += 1;
            pending.insert(rid, key.clone());
            reqs.push((rid, key));
        }
        self.probe = Some(Probe { winner, writes, left, pending, superseded: Default::default() });
        for (rid, key) in reqs {
            self.page.event(Event::Get { client: PROBE_CLIENT, req_id: as_req_id(rid), key });
        }
    }

    /// Every probe answered: tell each write's session which of its keys the
    /// winner replaced.
    fn finish_probe(&mut self, out: &mut Outbound) {
        if !self.probe.as_ref().is_some_and(|p| p.pending.is_empty()) {
            return;
        }
        let p = self.probe.take().expect("checked");
        tell_superseded(out, p.winner, &p.writes, &p.superseded);
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
            // M2 (sdk#148): a write that says what it READ. The reads go to the
            // engine, which checks them where the ops land.
            P::Commit { write_id, reads, ops } => Event::Write {
                client: self.speaker,
                write_id: as_write_id(write_id),
                ops: ops
                    .into_iter()
                    .map(|o| match o {
                        protocol::Op::Put(k, v) => (k, engine::Op::Put(v)),
                        protocol::Op::Delete(k) => (k, engine::Op::Delete),
                    })
                    .collect(),
                reads: reads
                    .into_iter()
                    .map(|(k, e)| {
                        (
                            k,
                            match e {
                                protocol::Expect::Absent => engine::Expect::Absent,
                                protocol::Expect::Present => engine::Expect::Present,
                                protocol::Expect::Value(h) => engine::Expect::Value(h),
                            },
                        )
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
        // sdk#225b: what each write leaves at each key, in case its commit is
        // displaced by another device's.
        if let Event::Write { client, write_id, ops, reads } = &ev {
            let mut finals: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
            for (k, o) in ops {
                finals.insert(k.clone(), match o {
                    engine::Op::Put(v) => Some(v.clone()),
                    engine::Op::Delete => None,
                });
            }
            self.sent.insert((client.0, write_id.0), Sent { finals: finals.into_iter().collect(), reads: reads.clone() });
        }
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
                        // M2: nothing applied, a read no longer held. `for_client`
                        // tells a pre-v4 client `Failed`, which is true to it.
                        State::Conflict => W::Conflict,
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
                // WHICH read no longer held, and what the tree holds there now
                // (M2) — to a v4 writer with a session (the same bundle: see
                // `protocol::Request::Commit`). An older one has its `Failed`.
                Effect::Conflicted { client, write_id, key, current } => {
                    let version = version_of(*client);
                    if version < protocol::SESSION_SINCE || session_of(*client) == protocol::LEGACY_SESSION {
                        continue;
                    }
                    protocol::Reply::Conflicted {
                        session: session_of(*client),
                        write_id: write_id.0,
                        key: key.clone(),
                        current: current.as_ref().map(|f| f.hash()),
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

/// What a tip left at each key: ONE value per key, because every tip write
/// agrees there (see [`Probe::left`]). Debug-asserted.
fn left_of(writes: &TipWrites) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
    let mut left: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
    for (_, w) in writes {
        for (k, v) in &w.finals {
            let was = left.insert(k.clone(), v.clone());
            debug_assert!(was.is_none_or(|x| x == *v), "two writes of one tip disagree at a key: a commit carries one write, the rest are no-ops");
        }
    }
    left
}

/// Each write's session is told the keys of ITS that the winner replaced.
fn tell_superseded(out: &mut Outbound, winner: (u64, freenet_prolly::Cid), writes: &TipWrites, superseded: &std::collections::BTreeSet<Vec<u8>>) {
    for ((client, write_id), w) in writes {
        let keys: Vec<Vec<u8>> = w.finals.iter().map(|(k, _)| k.clone()).filter(|k| superseded.contains(k)).collect();
        let c = engine::ClientId(*client);
        if keys.is_empty() || version_of(c) < protocol::SESSION_SINCE || session_of(c) == protocol::LEGACY_SESSION {
            continue;
        }
        out.replies.push(reply_bytes(&protocol::Reply::Superseded { session: session_of(c), write_id: *write_id, seq: winner.0, root: winner.1, keys }));
    }
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

