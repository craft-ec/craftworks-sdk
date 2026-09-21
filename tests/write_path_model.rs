//! THE WRITE PATH'S GATE: a model test, not a list of cases (WRITE-PATH.md,
//! revision 3, "The gate"; build order step 1 — written BEFORE the code).
//!
//! A seeded PRNG (splitmix64 — no new dependency) drives the REAL
//! `CachedStore`, two sessions of it, against a SCRIPTED node that follows the
//! engine's rule (today's, `Rule::Today`) and misbehaves the ways a real node
//! does: answers `Busy`; drops a verdict (F39); delivers one to the WRONG
//! session (F49); refuses a request with no verdict — past 101 queued (F51),
//! or past 8 queued while parked (F50); delays and reorders its answers;
//! FORGETS EVERYTHING mid-run (a context loss: it keeps its tree, nothing
//! else); and the client's clock jumps. Then the faults stop and the run is
//! driven to rest.
//!
//! Every check is on OBSERVABLE behaviour — frames on the wire, what the
//! node applied, the copy, what the person is told — never on an API the
//! fix will add, so the same file runs on the tree before the fix.
//!
//! Checked at every step: W5 (≤ 16 of a session's writes at the node); every
//! Write frame carries exactly the ops its write was made with (W1); the node
//! never applied a (session, write_id) twice — across a context loss too — (W3)
//! nor a session's writes out of order (W2). Checked at rest: nothing pending
//! and every write ended exactly once (W4); what the person was TOLD is true —
//! told rolled back ⇒ the node did not apply it, shown Published ⇒ it did
//! (W4/W6 as revised); each key a session wrote shows the node's value (W6);
//! no verdict arrived for a write the copy no longer had.
//!
//! THIS FILE IS RED ON TODAY'S CODE BY DESIGN. It is the gate the write path
//! is rebuilt against (sdk#183); the classes it finds today are pinned below
//! as TRIPWIRES — each asserts that its defect is still found, so the suite
//! stays green until a fix makes one disappear, and then that test names the
//! issue and asks to be inverted.

use craftworks_sdk::{CachedStore, Store};
use protocol::{Op, Reply, Request, WriteState};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

// ---------------------------------------------------------------- the dice

/// splitmix64: a seed is the whole run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// ------------------------------------------------------------- the config

/// The engine's rule, behind one switch so revision 3 slots in without
/// touching a single check.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rule {
    /// Today's engine: a write that arrives while a commit is pending is
    /// `Busy`; any other is taken, whatever its id — no order, no dedupe.
    Today,
    /// Revision 3's order rule (floor, ledger, `Duplicate`, `OutOfOrder`).
    /// Arrives with wire v5 in build step 2: today's client sends no floor
    /// and its ids have gaps (sdk#186), so a rev-3 node fed v4 frames would
    /// answer `OutOfOrder` for ever and every red would be an artefact.
    #[allow(dead_code)]
    Rev3,
}

/// Who drives the client.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Driver {
    /// Today's client, exactly: it re-sends a write only after `Busy`.
    Today,
    /// NOT TODAY'S CLIENT — what a RECOVERING client would do: a write at the
    /// node that has heard nothing for `RESEND_AFTER_MS` is sent again, the
    /// same frame. On the scripted node this is a SELF-CHECK of the harness:
    /// it shows the W3-across-a-context-loss check fires. It is not evidence
    /// that today's real engine applies twice — that is the architect's
    /// executed run (ledger attack §4); it becomes the gate's own at build
    /// step 4, when the real `Engine` runs under the same seeds.
    ResendOnSilence,
}

const RESEND_AFTER_MS: u64 = 20_000;

#[derive(Clone, Copy, Debug)]
struct Config {
    rule: Rule,
    driver: Driver,
    steps: usize,
    /// Misbehaviour on. Off, the node is HEALTHY: it answers every frame, in
    /// order, to the right session, and never forgets; the clock never jumps.
    faults: bool,
}

const TODAY: Config = Config { rule: Rule::Today, driver: Driver::Today, steps: 200, faults: true };
const HEALTHY: Config = Config { faults: false, ..TODAY };

/// The node's request queue per key: 100 waiting + 1 in service (F51).
const NODE_ADMITS: usize = 101;
/// Requests the node queues for a delegate that is parked, before refusing (F50).
const PARKED_ADMITS: usize = 8;
/// W5.
const WINDOW: usize = 16;
/// One node action (take a request, or move the commit one step).
const NODE_ACTION_MS: u64 = 100;
/// Drive-to-rest gives up — BY NAME — after this many rounds.
const REST_CAP: usize = 2_000;

// --------------------------------------------------------------- findings

/// A violation, by class — the class is what a pinned test asserts on.
#[derive(Clone, Debug)]
struct Finding {
    class: &'static str,
    /// What happened to the write first — the cause a pinned test names.
    tag: &'static str,
    detail: String,
}

// ------------------------------------------------------------- the model

#[derive(Clone, Debug)]
struct Commit {
    session: usize,
    write_id: u64,
    ops: Vec<Op>,
    /// The head PUT landed: the ops are in the tree, not yet answered.
    landed: bool,
}

/// How a write ended, as the PERSON was told it.
#[derive(Clone, Debug)]
enum End {
    Published { step: usize },
    RolledBack { why: String, step: usize },
    RefusedAtMake,
}

struct Model {
    rng: Rng,
    cfg: Config,
    clock: testkit::Clock,
    stores: [CachedStore; 2],
    sessions: [u64; 2],
    /// Keys each session writes: DISJOINT, so W6 needs no deltas. Its price,
    /// stated: a cross-session effect on one key is invisible here — a
    /// double apply shows as W3 only, never as W6, and two writers of one key
    /// are not modelled at all (M2's `Commit{reads,writes}`; `COVERAGE` reads 0).
    keys: [[&'static [u8]; 4]; 2],
    // --- the node ---
    tree: BTreeMap<Vec<u8>, Vec<u8>>,
    commit: Option<Commit>,
    inbound: VecDeque<(usize, Vec<u8>)>,
    parked: bool,
    queued_while_parked: usize,
    /// Verdicts on their way to a session: (addressed to, bytes, about).
    outbound: VecDeque<(usize, Vec<u8>, u64)>,
    /// (session, write_id) → the step it was APPLIED at.
    applied: BTreeMap<(usize, u64), usize>,
    last_applied: [u64; 2],
    // --- the harness's view of each write ---
    made: [BTreeMap<u64, Vec<Op>>; 2],
    ended: [BTreeMap<u64, End>; 2],
    /// Sent, and no verdict heard by that session yet: (write_id → when sent).
    at_node: [BTreeMap<u64, u64>; 2],
    /// The last frame of each write the client sent, for `ResendOnSilence`.
    last_frame: [BTreeMap<u64, Vec<u8>>; 2],
    /// What happened to each write on the way — the causes a finding names.
    notes: BTreeMap<(usize, u64), Vec<&'static str>>,
    /// Writes whose last verdict heard was `Busy`: queued at the CLIENT.
    queued: [BTreeSet<u64>; 2],
    findings: Vec<Finding>,
    trace: Vec<String>,
    /// What this run's mix actually REACHED (see `COVERAGE`).
    saw: BTreeSet<&'static str>,
    /// Fault-phase time that passed WITHOUT a jump.
    honest_ms: u64,
    /// Time the node has not yet spent on work.
    node_budget: u64,
    /// A rollback carried by a verdict, and nothing since that refills the
    /// window (the session's next verdict or tick).
    fell_at_node: [bool; 2],
    step: usize,
    faults: bool,
}

fn ops_of(edits: &[(Vec<u8>, craftworks_sdk::store::Edit)]) -> Vec<Op> {
    edits
        .iter()
        .map(|(k, e)| match e {
            craftworks_sdk::store::Edit::Put(v) => Op::Put(k.clone(), v.clone()),
            craftworks_sdk::store::Edit::Delete => Op::Delete(k.clone()),
        })
        .collect()
}

fn state_name(s: &WriteState) -> String {
    match s {
        WriteState::TooLarge { .. } => "TooLarge".into(),
        other => format!("{other:?}"),
    }
}

impl Model {
    fn new(seed: u64, cfg: Config) -> Model {
        let clock = testkit::Clock::new(1_790_000_000_000);
        let mut stores = [testkit::cached_store_on(&clock), testkit::cached_store_on(&clock)];
        for s in &mut stores {
            // Every key is LOADED and absent: a read of one answers "absent".
            s.on_page(b"", &[0xff; 8], vec![], [0u8; 32]);
        }
        let sessions = [
            stores[0].client.session().expect("session 0 has a session"),
            stores[1].client.session().expect("session 1 has a session"),
        ];
        Model {
            rng: Rng(seed),
            cfg,
            clock,
            stores,
            sessions,
            keys: [[b"a", b"b", b"c", b"d"], [b"e", b"f", b"g", b"h"]],
            tree: BTreeMap::new(),
            commit: None,
            inbound: VecDeque::new(),
            parked: false,
            queued_while_parked: 0,
            outbound: VecDeque::new(),
            applied: BTreeMap::new(),
            last_applied: [0, 0],
            made: [BTreeMap::new(), BTreeMap::new()],
            ended: [BTreeMap::new(), BTreeMap::new()],
            at_node: [BTreeMap::new(), BTreeMap::new()],
            last_frame: [BTreeMap::new(), BTreeMap::new()],
            notes: BTreeMap::new(),
            queued: [BTreeSet::new(), BTreeSet::new()],
            findings: Vec::new(),
            trace: Vec::new(),
            saw: BTreeSet::new(),
            fell_at_node: [false, false],
            honest_ms: 0,
            node_budget: 0,
            step: 0,
            faults: cfg.faults,
        }
    }

    fn find(&mut self, class: &'static str, detail: String) {
        self.find_tagged(class, "", detail);
    }

    fn find_tagged(&mut self, class: &'static str, tag: &'static str, detail: String) {
        self.trace.push(format!("  !! {class} [{tag}]: {detail}"));
        self.findings.push(Finding { class, tag, detail });
    }

    fn note(&mut self, session: usize, write_id: u64, what: &'static str) {
        self.notes.entry((session, write_id)).or_default().push(what);
    }

    /// The first cause on a write's record that explains a finding, in the
    /// order a reader should look.
    fn cause(&self, session: usize, write_id: u64) -> &'static str {
        const ORDER: &[&str] = &[
            "Busy, then applied after a later write",
            "its Published went to the other session",
            "the node lost its context after the head PUT",
            "its verdict was dropped",
            "left the client after it was rolled back",
            "the client's clock jumped while it was at the node",
            "timed out while its frame waited in the node's queue",
            "timed out while its verdict was on its way",
            "timed out while its commit was in flight",
            "the node refused its frame",
            "rolled back behind another write of its keys",
            "Busy",
        ];
        let notes = self.notes.get(&(session, write_id));
        ORDER.iter().copied().find(|c| notes.is_some_and(|n| n.contains(c))).unwrap_or("")
    }

    fn log(&mut self, s: String) {
        self.trace.push(format!("{:>4} t={:>7} {s}", self.step, self.clock.now_ms() - 1_790_000_000_000));
    }

    fn pending(&self, i: usize) -> BTreeSet<u64> {
        self.stores[i].copy.pending_ids().into_iter().collect()
    }

    /// Ids that left the pending list across `f`, and how the person was told.
    fn ends_across(&mut self, i: usize, told: impl FnOnce(&mut Model) -> BTreeMap<u64, String>) {
        let before = self.pending(i);
        let label = told(self);
        let after = self.pending(i);
        for id in before.difference(&after) {
            let why = match label.get(id) {
                Some(w) => w.clone(),
                None => {
                    self.note(i, *id, "rolled back behind another write of its keys");
                    "rolled back behind another write of its keys".into()
                }
            };
            let end = if why == "Published" || why == "ParityComplete" {
                End::Published { step: self.step }
            } else {
                End::RolledBack { why, step: self.step }
            };
            self.at_node[i].remove(id);
            // A rollback carried by a VERDICT frees the slots of every write it
            // takes, sent or still in the outbox — and today the window was
            // refilled before it (the tick refills after its rollbacks).
            if matches!(&end, End::RolledBack { why, .. } if !why.contains("(by the tick)")) {
                self.fell_at_node[i] = true;
            }
            if let Some(prev) = self.ended[i].insert(*id, end.clone()) {
                self.find("W4 ENDED TWICE", format!("session {i} w{id}: {prev:?}, then {end:?}"));
            }
        }
    }

    // ----------------------------------------------------------- the client

    fn make_write(&mut self, i: usize) {
        let n = 1 + self.rng.below(3) as usize;
        let mut ks: Vec<&[u8]> = self.keys[i].to_vec();
        let mut edits = Vec::new();
        for _ in 0..n {
            let k = ks.remove(self.rng.below(ks.len() as u64) as usize);
            let e = if self.rng.chance(20) {
                craftworks_sdk::store::Edit::Delete
            } else {
                craftworks_sdk::store::Edit::Put(format!("v{}", self.rng.below(1000)).into_bytes())
            };
            edits.push((k.to_vec(), e));
        }
        edits.sort_by(|a, b| a.0.cmp(&b.0));
        let id = self.stores[i].next_write_id();
        let refused_before = self.stores[i].refused.len();
        let _ = Store::apply_batch(&mut self.stores[i], &edits);
        self.made[i].insert(id, ops_of(&edits));
        if self.stores[i].refused.len() > refused_before {
            self.ended[i].insert(id, End::RefusedAtMake);
            self.log(format!("s{i} makes w{id} — refused at make"));
        } else {
            self.log(format!("s{i} makes w{id} {:?}", self.made[i][&id]));
        }
    }

    /// Everything a session has decided to send, onto the node's queue.
    fn pump(&mut self, i: usize) {
        for f in self.stores[i].take_outbound() {
            self.send_to_node(i, f, false);
        }
        if self.stores[i].held_count() > 0 {
            self.saw.insert("the window bound (writes held)");
            // The other half of W5: held writes leave while there is ROOM. A
            // client holding writes with fewer than the window at the node
            // has a slot it thinks is taken — the leaked-slot family (a
            // verdict that frees nothing, a timeout that frees nothing).
            let at = self.at_node[i].len();
            if at < WINDOW {
                let d = format!("session {i} holds {} writes with only {at} at the node (window {WINDOW}); queued {}", self.stores[i].held_count(), self.stores[i].queued_count());
                if self.fell_at_node[i] {
                    // Today's order in `on_write_state`: the window is refilled
                    // BEFORE `copy.failed` takes the later writes down with the
                    // failed one — their slots come free after the refill ran.
                    self.find("W5 NOT REFILLED AFTER A FALL", d);
                } else {
                    self.find("W5 HELD WITH ROOM", d);
                }
            }
        }
        let at = self.at_node[i].len();
        if at > WINDOW {
            let ids: Vec<u64> = self.at_node[i].keys().copied().collect();
            let d = format!("session {i} has {at} writes at the node (bound {WINDOW}): {ids:?}");
            self.find("W5 WINDOW", d);
        }
    }

    fn send_to_node(&mut self, i: usize, f: Vec<u8>, resent: bool) {
        if let protocol::Incoming::Ok(env) = protocol::decode_request(&f) {
            if let Request::Write { write_id, ops } = &env.body {
                // W1: the frame carries exactly the ops the write was made with.
                if let Some(made) = self.made[i].get(write_id) {
                    if made != ops {
                        let d = format!("session {i} w{write_id}: made with {made:?}, sent with {ops:?}");
                        self.find("W1 NOT WHOLE", d);
                    }
                }
                // W5 counts the writes the client still HOLDS. A frame can
                // leave for a write the client has already ended — its tick
                // rolled it back while the frame sat in the outbox — and
                // that is not a window slot; if the node then applies it, the
                // truth check at rest says so (FALSE ROLLBACK).
                if self.stores[i].copy.pending_ids().contains(write_id) {
                    self.at_node[i].insert(*write_id, self.clock.now_ms());
                } else {
                    self.log(format!("s{i} → node w{write_id}, a write it has ALREADY ENDED"));
                    self.note(i, *write_id, "left the client after it was rolled back");
                }
                self.queued[i].remove(write_id);
                self.last_frame[i].insert(*write_id, f.clone());
                self.log(format!("s{i} → node w{write_id}{}", if resent { " (RE-SENT on silence)" } else { "" }));
            }
        }
        // F51 / F50: refused with a host error — no verdict, to anyone.
        if self.inbound.len() >= NODE_ADMITS || (self.parked && self.faults && self.queued_while_parked >= PARKED_ADMITS) {
            self.log(format!("node REFUSES a frame from s{i} (queue {}, parked {})", self.inbound.len(), self.parked));
            self.saw.insert(if self.parked { "a request refused while parked (F50)" } else { "a request refused past 101 (F51)" });
            if let protocol::Incoming::Ok(env) = protocol::decode_request(&f) {
                if let Request::Write { write_id, .. } = env.body {
                    self.note(i, write_id, "the node refused its frame");
                }
            }
            return;
        }
        if self.parked {
            self.queued_while_parked += 1;
        }
        self.inbound.push_back((i, f));
    }

    fn resend_on_silence(&mut self, i: usize) {
        let now = self.clock.now_ms();
        let silent: Vec<u64> = self.at_node[i]
            .iter()
            .filter(|(_, at)| now.saturating_sub(**at) >= RESEND_AFTER_MS)
            .map(|(id, _)| *id)
            .collect();
        for id in silent {
            if let Some(f) = self.last_frame[i].get(&id).cloned() {
                self.send_to_node(i, f, true);
            }
        }
    }

    fn tick(&mut self, i: usize) {
        self.fell_at_node[i] = false; // the tick refills after its own rollbacks
        let now = self.clock.now_ms();
        self.stores[i].send_tick(now);
        let queued_before = self.queued[i].clone();
        let mut timed_out = Vec::new();
        self.ends_across(i, |m| {
            let told = m.stores[i].tick();
            timed_out = told.rolled_back.iter().filter(|(_, why)| matches!(why, craftworks_sdk::RolledBack::Unknown)).map(|(id, _)| *id).collect();
            told.rolled_back
                .iter()
                .map(|(id, why)| {
                    if matches!(why, craftworks_sdk::RolledBack::AfterFailed) {
                        m.note(i, *id, "rolled back behind another write of its keys");
                    }
                    (*id, format!("{why:?} (by the tick)"))
                })
                .collect()
        });
        for id in timed_out {
            let waiting = self.inbound.iter().any(|(s, f)| {
                *s == i && matches!(protocol::decode_request(f), protocol::Incoming::Ok(env) if matches!(env.body, Request::Write { write_id, .. } if write_id == id))
            });
            if waiting {
                self.note(i, id, "timed out while its frame waited in the node's queue");
            }
            if self.outbound.iter().any(|(to, _, about)| *to == i && *about == id) {
                self.note(i, id, "timed out while its verdict was on its way");
            }
            if self.commit.as_ref().is_some_and(|c| c.session == i && c.write_id == id) {
                self.note(i, id, "timed out while its commit was in flight");
            }
            if queued_before.contains(&id) {
                // The node ANSWERED it (`Busy`); it sat queued at the client,
                // never re-offered, and a timer called it Unknown.
                let d = format!("session {i} w{id}: answered Busy, never re-sent, rolled back Unknown by the timer at step {}", self.step);
                self.find_tagged("STALE CLOCK", "Busy", d);
            }
            self.queued[i].remove(&id);
        }
    }

    // ------------------------------------------------------------- the node

    fn reply(&mut self, session: usize, write_id: u64, state: WriteState) {
        let bytes = protocol::encode_reply(&Reply::SessionWriteState { session: self.sessions[session], write_id, state })
            .expect("a verdict encodes");
        self.log(format!("node answers s{session} w{write_id}: {}", state_name(&state)));
        self.outbound.push_back((session, bytes, write_id));
    }

    /// The node takes ONE request from its queue, as it runs one delegate
    /// call at a time.
    fn serve(&mut self) {
        if self.parked {
            return;
        }
        // A real node runs one delegate call at a time, so a request waits in
        // the node's queue until the commit in flight is done and meets an
        // idle engine (sdk#176: 0 Busy in every live L4 run). `Busy` is what a
        // request meets when it reaches the engine mid-commit anyway — a
        // FAULT here, never on a healthy node.
        if self.commit.is_some() && !(self.faults && self.rng.chance(30)) {
            return;
        }
        let Some((i, f)) = self.inbound.pop_front() else { return };
        let protocol::Incoming::Ok(env) = protocol::decode_request(&f) else { return };
        let Request::Write { write_id, ops } = env.body else { return };
        match self.cfg.rule {
            Rule::Today => {
                if self.commit.is_some() {
                    self.note(i, write_id, "Busy");
                    self.saw.insert("a Busy");
                    self.reply(i, write_id, WriteState::Busy);
                } else if self.faults && self.rng.chance(3) {
                    // The engine refuses it as invalid: nothing applied.
                    self.reply(i, write_id, WriteState::Failed);
                } else {
                    self.commit = Some(Commit { session: i, write_id, ops, landed: false });
                    self.reply(i, write_id, WriteState::Accepted);
                }
            }
            Rule::Rev3 => unimplemented!("rev 3's order rule arrives with wire v5 (build step 2)"),
        }
    }

    /// The commit in flight moves one step: its head PUT lands (the ops are
    /// APPLIED), then — in a later step — it is answered `Published`.
    fn advance_commit(&mut self) {
        let Some(c) = self.commit.clone() else { return };
        if !c.landed {
            for op in &c.ops {
                match op {
                    Op::Put(k, v) => {
                        self.tree.insert(k.clone(), v.clone());
                    }
                    Op::Delete(k) => {
                        self.tree.remove(k);
                    }
                }
            }
            let key = (c.session, c.write_id);
            if let Some(first) = self.applied.get(&key) {
                let d = format!("session {} w{} applied at step {first} and again at step {}", c.session, c.write_id, self.step);
                let tag = self.cause(c.session, c.write_id);
                self.find_tagged("W3 APPLIED TWICE", tag, d);
            }
            if c.write_id < self.last_applied[c.session] {
                let busy = self.notes.get(&(c.session, c.write_id)).is_some_and(|n| n.contains(&"Busy"));
                if busy {
                    self.note(c.session, c.write_id, "Busy, then applied after a later write");
                }
                let d = format!(
                    "session {} w{} applied after w{} — a later write of the session landed first",
                    c.session, c.write_id, self.last_applied[c.session]
                );
                let tag = self.cause(c.session, c.write_id);
                self.find_tagged("W2 OUT OF ORDER", tag, d);
            }
            self.last_applied[c.session] = self.last_applied[c.session].max(c.write_id);
            self.applied.insert(key, self.step);
            self.log(format!("node APPLIES s{} w{} (head PUT landed)", c.session, c.write_id));
            self.commit.as_mut().expect("the commit").landed = true;
        } else {
            self.commit = None;
            self.reply(c.session, c.write_id, WriteState::Published);
            // As the real engine does for every coded write: ParityComplete
            // AFTER Published — for a write the client has, by then, already
            // settled (cached_store.rs counts it as a verdict for a write it
            // no longer has).
            self.reply(c.session, c.write_id, WriteState::ParityComplete);
        }
    }

    /// Hand one verdict to a session — the right one, or not.
    fn deliver(&mut self, faults: bool) {
        if self.outbound.is_empty() {
            return;
        }
        // Out of order: any of the waiting answers, not only the oldest.
        let at = if faults { self.rng.below(self.outbound.len() as u64) as usize } else { 0 };
        let (mut to, bytes, about) = self.outbound.remove(at).expect("an answer");
        let state = match protocol::decode_reply(&bytes) {
            Ok(Reply::SessionWriteState { state, .. }) => state,
            _ => unreachable!("the node only sends write states"),
        };
        if faults && self.rng.chance(6) {
            self.log(format!("verdict for s{to} w{about} DROPPED (F39)"));
            self.saw.insert("a verdict dropped (F39)");
            self.note(to, about, "its verdict was dropped");
            return;
        }
        if faults && self.rng.chance(6) {
            if matches!(state, WriteState::Published | WriteState::ParityComplete) {
                self.note(to, about, "its Published went to the other session");
            }
            to = 1 - to;
            self.log(format!("verdict about w{about} delivered to the WRONG session s{to} (F49)"));
            self.saw.insert("a verdict misrouted (F49)");
        }
        // A session hears about its OWN writes only: a misrouted verdict names
        // the other session, and the client drops it as foreign.
        let own = match protocol::decode_reply(&bytes) {
            Ok(Reply::SessionWriteState { session, .. }) => session == self.sessions[to],
            _ => false,
        };
        if own {
            self.at_node[to].remove(&about);
            if matches!(state, WriteState::Busy) {
                self.queued[to].insert(about);
            } else {
                self.queued[to].remove(&about);
            }
        }
        let label = state_name(&state);
        // The session's OWN verdict refills the window — before whatever this
        // one falls. A misrouted one is dropped as foreign and refills nothing.
        if own {
            self.fell_at_node[to] = false;
        }
        self.ends_across(to, |m| {
            m.stores[to].on_inbound(&bytes);
            BTreeMap::from([(about, if own { label.clone() } else { format!("(foreign) {label}") })])
        });
    }

    /// The node does work in proportion to TIME: one action (take a request,
    /// or move the commit a step) per `NODE_ACTION_MS` — a write is three
    /// actions, so about 3.4 commits a second, as measured live.
    fn node_works(&mut self, dt: u64) {
        self.node_budget += dt;
        while self.node_budget >= NODE_ACTION_MS {
            self.node_budget -= NODE_ACTION_MS;
            if self.commit.is_some() {
                self.advance_commit();
            } else {
                self.serve();
            }
        }
    }

    /// The delegate FORGETS EVERYTHING — its context is gone. The tree (with
    /// whatever head PUT landed) stays; the commit in flight does not.
    fn context_loss(&mut self) {
        self.saw.insert(match &self.commit {
            None => "a context lost with no commit",
            Some(c) if !c.landed => "a context lost with a commit taken, not landed",
            Some(_) => "a context lost AFTER a head PUT",
        });
        if let Some(c) = self.commit.clone().filter(|c| c.landed) {
            self.note(c.session, c.write_id, "the node lost its context after the head PUT");
        }
        let what = match &self.commit {
            Some(c) if c.landed => format!("commit s{} w{} LANDED, not answered", c.session, c.write_id),
            Some(c) => format!("commit s{} w{} taken, not landed", c.session, c.write_id),
            None => "no commit".into(),
        };
        self.log(format!("node LOSES ITS CONTEXT ({what})"));
        self.commit = None;
        self.parked = false;
        self.queued_while_parked = 0;
    }

    // ------------------------------------------------------------- the run

    fn fault_step(&mut self) {
        self.step += 1;
        // Time passes on every step — honestly, so a write can sit at the node
        // past the timeout with no jump involved.
        let dt = self.rng.below(600);
        self.clock.advance(dt);
        self.honest_ms += dt;
        self.node_works(dt);
        let i = self.rng.below(2) as usize;
        match self.rng.below(100) {
            0..=29 => self.make_write(i),
            30..=44 => self.pump(i),
            45..=59 => self.serve(),
            60..=69 => self.advance_commit(),
            70..=81 => self.deliver(self.faults),
            82..=89 => {
                let dt = self.rng.below(3_000);
                self.clock.advance(dt);
                self.honest_ms += dt;
                self.node_works(dt);
                self.tick(i);
            }
            // The client's clock JUMPS — the machine slept. RARE (1 in 500
            // steps): the architect measured every Unknown rollback downstream
            // of a jump when it was 1 in 50, and no seed's HONEST time ever
            // crossed the 60 s timeout. Time now also passes on every step.
            90..=91 if self.faults && self.rng.chance(10) => {
                // The client's clock JUMPS: the machine slept.
                let ms = 61_000 + self.rng.below(3_600_000);
                self.log(format!("the client's clock JUMPS {ms} ms"));
                self.saw.insert("the client's clock jumped");
                for j in 0..2 {
                    let ids: Vec<u64> = self.at_node[j].keys().copied().collect();
                    for id in ids {
                        self.note(j, id, "the client's clock jumped while it was at the node");
                    }
                }
                self.clock.advance(ms);
                self.tick(i);
            }
            92..=93 if self.faults => self.context_loss(),
            94..=95 => {
                // A BURST — a publish's handoff, a paste: more writes at once
                // than the window holds, so the window is what paces them.
                let n = 17 + self.rng.below(24);
                self.log(format!("s{i} makes a BURST of {n} writes"));
                self.saw.insert("a burst");
                for _ in 0..n {
                    self.make_write(i);
                }
            }
            96 if self.faults => {
                self.parked = !self.parked;
                self.queued_while_parked = 0;
                let p = self.parked;
                self.log(format!("node {}", if p { "PARKS (a cold write)" } else { "unparks" }));
                self.saw.insert("the node parked");
            }
            _ => {
                self.pump(0);
                self.pump(1);
            }
        }
        if self.cfg.driver == Driver::ResendOnSilence {
            self.resend_on_silence(i);
        }
        // A page sends what it has at once, and answers arrive as they come:
        // both happen EVERY step. Under faults an answer may be held back a
        // step (delay; out of order), dropped or misrouted — in `deliver`.
        self.pump(0);
        self.pump(1);
        let ready = self.outbound.len();
        for _ in 0..ready {
            if self.faults && self.rng.chance(30) {
                continue; // delayed: stays for a later step
            }
            self.deliver(self.faults);
        }
    }

    /// Faults off; everything delivered, every second ticked, until nothing
    /// is pending anywhere — or FAIL BY NAME.
    fn drive_to_rest(&mut self) {
        self.faults = false;
        self.parked = false;
        self.log("—— faults stop; driving to rest ——".into());
        for round in 0..REST_CAP {
            self.step += 1;
            for i in 0..2 {
                self.pump(i);
                if self.cfg.driver == Driver::ResendOnSilence {
                    self.resend_on_silence(i);
                }
            }
            while !self.inbound.is_empty() {
                self.serve();
                while self.commit.is_some() {
                    self.advance_commit();
                }
                for i in 0..2 {
                    self.pump(i);
                }
            }
            while !self.outbound.is_empty() {
                self.deliver(false);
            }
            self.clock.advance(1_000);
            for i in 0..2 {
                self.tick(i);
                self.pump(i);
            }
            // The tick frames just pumped are not work: only a WRITE still
            // queued at the node, an answer on its way, or a commit is.
            let writes_queued = self.inbound.iter().any(|(_, f)| {
                matches!(protocol::decode_request(f), protocol::Incoming::Ok(env) if matches!(env.body, Request::Write { .. }))
            });
            let quiet = self.pending(0).is_empty()
                && self.pending(1).is_empty()
                && !writes_queued
                && self.outbound.is_empty()
                && self.commit.is_none();
            if quiet {
                self.log(format!("at rest after {round} rounds"));
                return;
            }
        }
        let d = format!(
            "still pending after {REST_CAP} rounds of delivering everything and ticking every second: s0 {:?} (held {}), s1 {:?} (held {})",
            self.pending(0),
            self.stores[0].held_count(),
            self.pending(1),
            self.stores[1].held_count()
        );
        self.find("W4 NEVER AT REST", d);
    }

    fn check_at_rest(&mut self) {
        for i in 0..2 {
            // Every write made ended exactly once (ENDED TWICE is caught as it happens).
            let unended: Vec<u64> = self.made[i].keys().filter(|id| !self.ended[i].contains_key(id)).copied().collect();
            if !unended.is_empty() {
                self.find("W4 NEVER ENDED", format!("session {i}: {unended:?} made and never ended"));
            }
            // What the person was told is TRUE.
            let ends: Vec<(u64, End)> = self.ended[i].iter().map(|(k, v)| (*k, v.clone())).collect();
            for (id, end) in ends {
                let applied = self.applied.get(&(i, id)).copied();
                match (&end, applied) {
                    (End::RolledBack { why, step }, Some(at)) => {
                        let tag = self.cause(i, id);
                        let d = format!("session {i} w{id}: told {why} at step {step}, applied at step {at}");
                        self.find_tagged("FALSE ROLLBACK", tag, d);
                    }
                    (End::Published { step }, None) => {
                        self.find("FALSE PUBLISHED", format!("session {i} w{id}: shown Published at step {step}, never applied"));
                    }
                    _ => {}
                }
            }
            // W6: each key this session wrote shows the node's value. No escape by `rolled_back`.
            for k in self.keys[i] {
                let node = self.tree.get(k).cloned();
                let copy = self.stores[i].copy.get(k).and_then(|v| v.value().map(|b| b.to_vec()));
                if node != copy {
                    let show = |v: &Option<Vec<u8>>| v.as_ref().map(|b| String::from_utf8_lossy(b).into_owned());
                    let d = format!("session {i} key {}: the copy shows {:?}, the node has {:?}", String::from_utf8_lossy(k), show(&copy), show(&node));
                    self.find("W6 COPY LIES", d);
                }
            }
            let late = self.stores[i].unknown_verdicts();
            if late > 0 {
                self.find("LATE VERDICT", format!("session {i}: {late} verdict(s) for a write the copy no longer had"));
            }
        }
    }
}

/// What the step mix can reach. A situation no run reaches is one the model
/// is BLIND to — whatever defect lives there, the sweep cannot find it — so
/// every one is counted and printed, zeros included. (908feb6 ran identically
/// to main, seed for seed, until bursts were in the mix: the window never
/// bound, so its leaked slot was invisible.)
const COVERAGE: &[&str] = &[
    "the window bound (writes held)",
    "a burst",
    "a Busy",
    "a verdict dropped (F39)",
    "a verdict misrouted (F49)",
    "a request refused past 101 (F51)",
    "a request refused while parked (F50)",
    "the node parked",
    "a context lost with no commit",
    "a context lost with a commit taken, not landed",
    "a context lost AFTER a head PUT",
    "the client's clock jumped",
    "honest time crossed the 60 s timeout (no jump)",
    // NOT MODELLED: the sessions write disjoint keys, so W6 holds without
    // deltas. Two writers of one key are M2's business (`Commit{reads,
    // writes}`); this model is blind to them, and says so.
    "two sessions writing one key",
];

/// How the writes of a run ended, over both sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Tally {
    made: usize,
    refused_at_make: usize,
    published: usize,
    rolled_back: usize,
}

/// Situations in `COVERAGE` this model does NOT reach, and why — printed, so
/// the blindness is on the page, not discovered later.
const NOT_REACHED: &[(&str, &str)] = &[
    ("two sessions writing one key", "the sessions' keys are disjoint (see `keys`); two writers of one key are M2's `Commit{reads,writes}`"),
    ("a request refused past 101 (F51)", "two windowed sessions put at most 2 x 16 writes plus their ticks in the node's queue; F51 needs the unwindowed client or more sessions (model v2)"),
];

/// One seeded run: its findings, its trace, what its mix reached, and how its
/// writes ended.
fn run_full(seed: u64, cfg: Config) -> (Vec<Finding>, Vec<String>, BTreeSet<&'static str>, Tally) {
    let mut m = Model::new(seed, cfg);
    for _ in 0..cfg.steps {
        m.fault_step();
    }
    if m.honest_ms >= 60_000 {
        m.saw.insert("honest time crossed the 60 s timeout (no jump)");
    }
    m.drive_to_rest();
    m.check_at_rest();
    let mut t = Tally::default();
    for i in 0..2 {
        t.made += m.made[i].len();
        for e in m.ended[i].values() {
            match e {
                End::Published { .. } => t.published += 1,
                End::RolledBack { .. } => t.rolled_back += 1,
                End::RefusedAtMake => t.refused_at_make += 1,
            }
        }
    }
    (m.findings, m.trace, m.saw, t)
}

fn run(seed: u64, cfg: Config) -> (Vec<Finding>, Vec<String>, BTreeSet<&'static str>) {
    let (f, trace, saw, _) = run_full(seed, cfg);
    (f, trace, saw)
}

type Found = BTreeMap<(&'static str, &'static str), (u64, String)>;

/// Every (class, cause) found across `seeds`, with the first seed that found
/// it — and how many runs reached each situation in `COVERAGE`.
struct Sweep {
    first: Found,
    /// Runs in which each (class, cause) appeared at least once.
    runs_with: BTreeMap<(&'static str, &'static str), usize>,
    reached: BTreeMap<&'static str, usize>,
}

fn sweep_all(seeds: std::ops::Range<u64>, cfg: Config) -> Sweep {
    let mut first = BTreeMap::new();
    let mut runs_with: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    let mut reached: BTreeMap<&'static str, usize> = COVERAGE.iter().map(|c| (*c, 0)).collect();
    for seed in seeds {
        let (f, _, saw) = run(seed, cfg);
        let mut here = BTreeSet::new();
        for x in f {
            here.insert((x.class, x.tag));
            first.entry((x.class, x.tag)).or_insert((seed, x.detail));
        }
        for c in here {
            *runs_with.entry(c).or_insert(0) += 1;
        }
        for c in saw {
            *reached.entry(c).or_insert(0) += 1;
        }
    }
    Sweep { first, runs_with, reached }
}

fn sweep(seeds: std::ops::Range<u64>, cfg: Config) -> Found {
    sweep_all(seeds, cfg).first
}

fn show(seed: u64, cfg: Config) -> String {
    let (f, trace, _) = run(seed, cfg);
    let keep: usize = std::env::var("WPM_TAIL").ok().and_then(|s| s.parse().ok()).unwrap_or(80);
    let tail: Vec<&String> = trace.iter().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
    format!(
        "seed {seed} ({:?}/{:?}): {:?}\n{}",
        cfg.rule,
        cfg.driver,
        f.iter().map(|x| format!("{}: {}", x.class, x.detail)).collect::<Vec<_>>(),
        tail.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    )
}

// ------------------------------------------------------------- the gate

/// What today's client and today's engine rule are KNOWN to break: each
/// (class, cause) PAIR, the number of the 1,000 fixed seeds that find it, and
/// the issue it belongs to. The runs are deterministic, so the COUNT is
/// pinned, not only the class — and per CAUSE, not only per class: a new
/// defect that lands inside a known class (a verdict that frees no slot shows
/// up as more STALE CLOCK and FALSE ROLLBACK; a frame that leaves after its
/// rollback is a new cause of a known FALSE ROLLBACK) moves a count, and a
/// fix moves one DOWN. Either way the sweep fails and says which pair —
/// update this table in the same change, and invert a tripwire whose pair
/// reaches 0.
///
/// Issues: FALSE ROLLBACK, STALE CLOCK, W2, W5 refill, W6 — sdk#183; a
/// misrouted Published — sdk#184; LATE VERDICT — sdk#183 (on a HEALTHY node
/// it is gone: `a_healthy_node_finds_nothing`).
const KNOWN_RED_TODAY: &[(&str, &str, usize, &str)] = &[
    // (class, cause, runs of 1,000 fixed seeds, issue) — rows as the sweep prints them.
    ("FALSE ROLLBACK", "Busy, then applied after a later write", 13, "sdk#183"),
    ("FALSE ROLLBACK", "its Published went to the other session", 871, "sdk#184"),
    ("FALSE ROLLBACK", "its verdict was dropped", 827, "sdk#183"),
    ("FALSE ROLLBACK", "left the client after it was rolled back", 706, "sdk#183"),
    ("FALSE ROLLBACK", "rolled back behind another write of its keys", 894, "sdk#183"),
    ("FALSE ROLLBACK", "the client's clock jumped while it was at the node", 179, "sdk#183"),
    ("FALSE ROLLBACK", "the node lost its context after the head PUT", 437, "sdk#183"),
    ("FALSE ROLLBACK", "timed out while its commit was in flight", 3, "sdk#183"),
    ("FALSE ROLLBACK", "timed out while its frame waited in the node's queue", 4, "sdk#183"),
    ("FALSE ROLLBACK", "timed out while its verdict was on its way", 20, "sdk#183"),
    // Under faults: a Published (or Failed) arriving after the copy rolled the
    // write back — the false rollbacks above, seen from the other side.
    ("LATE VERDICT", "", 1000, "sdk#183"),
    ("STALE CLOCK", "Busy", 292, "sdk#183"),
    ("W2 OUT OF ORDER", "Busy, then applied after a later write", 246, "sdk#183"),
    ("W5 NOT REFILLED AFTER A FALL", "", 297, "sdk#183"),
    ("W6 COPY LIES", "", 495, "sdk#183"),
];

#[test]
fn the_sweep_finds_exactly_the_known_classes_and_counts() {
    let sw = sweep_all(0..1_000, TODAY);
    for ((class, tag), (seed, detail)) in &sw.first {
        println!("  {class:<16} [{tag}] first at seed {seed:>4}: {detail}");
    }
    println!("  COVERAGE over 1,000 runs — a situation no run reaches is one the model is blind to:");
    for c in COVERAGE {
        println!("    {:>5}  {c}", sw.reached[c]);
    }
    // Every situation the model CLAIMS to cover is reached; the one it does
    // not model reads 0, visibly, rather than being left off the list.
    let blind: Vec<&&str> = COVERAGE.iter().filter(|c| !NOT_REACHED.iter().any(|(n, _)| n == *c) && sw.reached[*c] == 0).collect();
    for (c, why) in NOT_REACHED {
        println!("    NOT REACHED, by design: {c} — {why}");
    }
    assert!(blind.is_empty(), "the step mix never reaches {blind:?}: the model is blind there");

    println!("  RUNS PER (CLASS, CAUSE) OF 1,000 — as table rows:");
    for ((c, t), n) in &sw.runs_with {
        println!("    ({c:?}, {t:?}, {n}, \"\"),");
    }
    let want: BTreeMap<(&str, &str), usize> = KNOWN_RED_TODAY.iter().map(|(c, t, n, _)| ((*c, *t), *n)).collect();
    let mut moved = Vec::new();
    for k in want.keys().chain(sw.runs_with.keys()).collect::<BTreeSet<_>>() {
        let (was, now) = (want.get(k).copied(), sw.runs_with.get(k).copied().unwrap_or(0));
        match was {
            None => moved.push(format!("NEW {} [{}]: {now} runs — a defect nobody has named", k.0, k.1)),
            Some(w) if w != now => moved.push(format!("{} [{}]: {w} → {now} runs", k.0, k.1)),
            _ => {}
        }
    }
    assert!(
        moved.is_empty(),
        "the classes the model finds on this tree MOVED — a new defect (up, or a new class) or a fix (down; invert a tripwire whose class reaches 0):\n  {}",
        moved.join("\n  ")
    );
}

/// A pinned finding: `seed` must still find `class` with cause `tag` —
/// TODAY. When this fails, the defect it pins is gone: invert this test
/// (assert the class is absent for this seed) and take the class off
/// `KNOWN_RED_TODAY` once no seed finds it.
fn tripwire(seed: u64, cfg: Config, class: &str, tag: &str, issue: &str) {
    let (f, _, _) = run(seed, cfg);
    let hit = f.iter().any(|x| x.class == class && x.tag == tag);
    assert!(
        hit,
        "seed {seed} no longer finds {class} [{tag}] ({issue}) — if a fix for it landed, INVERT this tripwire.\n{}",
        show(seed, cfg)
    );
}

/// sdk#179 c, found unaided: a write answered Busy, re-sent after a later
/// write of the same session had landed — the older value lands last.
#[test]
fn known_red_busy_reorder_k_old_after_k_new() {
    tripwire(5, TODAY, "W2 OUT OF ORDER", "Busy, then applied after a later write", "sdk#183");
}

/// A Busy'd write, queued at the client with its original clock, never
/// re-offered while others were pending, rolled back Unknown by the timer.
#[test]
fn known_red_stale_clock_of_a_queued_write() {
    tripwire(6, TODAY, "STALE CLOCK", "Busy", "sdk#183");
}

/// Run (a): the node's context is lost after the head PUT landed and before
/// its answer. Today's client never asks again: at 60 s it is told Unknown,
/// and the node HAS it.
#[test]
fn known_red_context_loss_after_the_head_put_is_a_false_rollback() {
    tripwire(0, TODAY, "FALSE ROLLBACK", "the node lost its context after the head PUT", "sdk#183 run (a)");
}

/// sdk#184: a Published delivered to the OTHER session (F49); this one is
/// told Unknown at 60 s, and the node has it.
#[test]
fn known_red_misrouted_published_is_a_false_rollback() {
    tripwire(0, TODAY, "FALSE ROLLBACK", "its Published went to the other session", "sdk#184");
}

/// A dropped verdict (F39): the same false rollback by a different road.
#[test]
fn known_red_dropped_verdict_is_a_false_rollback() {
    tripwire(0, TODAY, "FALSE ROLLBACK", "its verdict was dropped", "sdk#183");
}

/// A frame that sat in the client's outbox leaves AFTER its write was rolled
/// back, and the node applies it.
#[test]
fn known_red_a_rolled_back_write_still_leaves_and_lands() {
    tripwire(0, TODAY, "FALSE ROLLBACK", "left the client after it was rolled back", "sdk#183");
}

/// The window refilled before the fall it should follow (`on_write_state`).
#[test]
fn known_red_the_window_is_not_refilled_after_a_fall() {
    tripwire(0, TODAY, "W5 NOT REFILLED AFTER A FALL", "", "sdk#183");
}

/// W6 at rest: the copy shows a value the node does not have.
#[test]
fn known_red_the_copy_lies_at_rest() {
    tripwire(1, TODAY, "W6 COPY LIES", "", "sdk#183");
}

/// THE HARNESS'S OWN CHECK — not a finding about today's client, which
/// never re-sends on silence. With a driver that DOES (what a recovering
/// client would do), today's rule applies a write twice across a context
/// loss, and the W3 check must see it. On the scripted node this proves the
/// CHECK works; that today's real engine applies twice is the architect's
/// executed run (ledger attack §4), and build step 4 runs the real Engine
/// under these seeds.
#[test]
fn harness_check_resend_on_silence_makes_today_apply_twice() {
    let cfg = Config { driver: Driver::ResendOnSilence, ..TODAY };
    let found = sweep(0..300, cfg);
    let twice: Vec<_> = found.iter().filter(|((c, _), _)| *c == "W3 APPLIED TWICE").collect();
    for ((c, t), (seed, d)) in &twice {
        println!("  {c} [{t}] first at seed {seed}: {d}");
    }
    // The run the design names: the context lost BETWEEN the head PUT and its
    // answer, the write re-sent, applied again.
    assert!(
        twice.iter().any(|((_, t), _)| *t == "the node lost its context after the head PUT"),
        "no seed applied a write twice ACROSS A CONTEXT LOSS: the W3 check does not see it, or the model never loses a context between the head PUT and its answer"
    );
}

/// W1 and W5 hold on today's client in every seed: a write is whole on the
/// wire, never more than the window of a session's writes is at the node, and
/// writes are not held while there is room — except right after a fall, which
/// is its own known class. (Checked every step; this pins that they are CLEAN.)
#[test]
fn today_keeps_writes_whole_and_the_window() {
    let found = sweep(0..1_000, TODAY);
    let broken: Vec<_> = found.keys().filter(|(c, _)| c.starts_with("W1") || *c == "W5 WINDOW" || *c == "W5 HELD WITH ROOM").collect();
    assert!(broken.is_empty(), "{broken:?}");
}

/// THE LIVENESS FLOOR. Every rest check is a SAFETY check: a client that
/// rolls everything back, or never sends, passes them all (executed by the
/// architect: "a node that swallows every frame: 50 writes made, applied 0,
/// findings []"). On a HEALTHY node — no fault of any kind — every write
/// made and not refused at make is PUBLISHED, and none is rolled back.
#[test]
fn liveness_on_a_healthy_node_every_write_is_published() {
    let mut total = Tally::default();
    let mut short = Vec::new();
    for seed in 0..300 {
        let (_, _, _, t) = run_full(seed, HEALTHY);
        if t.rolled_back != 0 || t.published != t.made - t.refused_at_make {
            short.push((seed, t));
        }
        total.made += t.made;
        total.refused_at_make += t.refused_at_make;
        total.published += t.published;
        total.rolled_back += t.rolled_back;
    }
    println!("  healthy, 300 seeds: {total:?}");
    assert!(short.is_empty(), "{} healthy runs did not publish every write: first {:?}", short.len(), short.first());
}

/// A HEALTHY node finds NOTHING: no fault, so no class of any kind. The
/// last one standing on today's client was LATE VERDICT — the engine's
/// `ParityComplete` after `Published`, counted as a stranger's verdict for
/// a write the client had already settled (fixed with this test).
#[test]
fn a_healthy_node_finds_nothing() {
    let mut found: BTreeMap<&str, (u64, String)> = BTreeMap::new();
    for seed in 0..300 {
        let (f, _, _) = run(seed, HEALTHY);
        for x in f {
            found.entry(x.class).or_insert((seed, x.detail));
        }
    }
    assert!(found.is_empty(), "a node with no fault still produced findings: {found:?}");
}

/// A run is a function of its seed — or no seed can be pinned.
#[test]
fn a_seed_is_the_whole_run() {
    for seed in [1u64, 77, 999] {
        let a = run(seed, TODAY);
        let b = run(seed, TODAY);
        assert_eq!(a.1, b.1, "seed {seed} ran two different ways");
    }
}

#[test]
#[ignore = "diagnostic: print one seed's trace — WPM_SEED=<n> [WPM_RESEND=1] [WPM_TAIL=<lines>] cargo test --test write_path_model trace -- --ignored --nocapture"]
fn trace() {
    let seed: u64 = std::env::var("WPM_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let driver = if std::env::var("WPM_RESEND").is_ok() { Driver::ResendOnSilence } else { Driver::Today };
    println!("{}", show(seed, Config { driver, ..TODAY }));
}

// ====================================================================
// RULE::REV3 — the scripted node's rule for wire v5, at the FRAME level
// (WRITE-PATH.md revision 3, "The rule"). Fed v5 frames by hand, no client:
// today's client speaks v4, and the client that speaks v5 is build step 3,
// which wires this node into the model above. Types from sdk#193.
// ====================================================================

/// The rev-3 node: the order rule over a tree, a HEAD (root + ledger) that
/// survives a forget, and a CONTEXT (the commit in flight, what was taken,
/// what frames were seen) that does not.
#[derive(Default)]
struct Rev3Node {
    tree: BTreeMap<Vec<u8>, Vec<u8>>,
    /// THE HEAD'S LEDGER: `published_through[S]`, written with the head PUT —
    /// the same signed record — so it is atomic with the commit and read by
    /// an engine that has just lost everything.
    ledger: BTreeMap<u64, u64>,
    // --- the context: gone on a forget ---
    commit: Option<Rev3Commit>,
    taken_through: BTreeMap<u64, u64>,
    frames_seen: BTreeMap<u64, u64>,
    /// Every (session, write_id) APPLIED, for W3. APPLIED = TAKEN under the
    /// rule AND NOT WITHDRAWN: a take whose commit died un-landed — Failed,
    /// Lost, or a forget before its head PUT — is withdrawn, so the write
    /// sent again and taken again is a correct recovery, not a double apply.
    applied: Vec<(u64, u64)>,
}

#[derive(Clone)]
struct Rev3Commit {
    session: u64,
    write_id: u64,
    ops: Vec<Op>,
    landed: bool,
}

impl Rev3Node {
    fn next(&self, s: u64) -> u64 {
        self.ledger.get(&s).copied().unwrap_or(0) + 1
    }

    fn ack(&self, s: u64, answering: Option<u64>) -> protocol::Ack {
        protocol::Ack {
            session: s,
            published_through: self.ledger.get(&s).copied().unwrap_or(0),
            taken_through: self.taken_through.get(&s).copied().unwrap_or(0),
            // The scripted node does not model parity: it does not KNOW, and
            // says so — never "0 owed".
            parity: None,
            frames_seen: self.frames_seen.get(&s).copied().unwrap_or(0),
            answering,
        }
    }

    /// A verdict to session `s`, wrapped with `s`'s Ack. `caller` is the
    /// session whose frame started this call, and `frame` that frame.
    fn verdict(&self, s: u64, write_id: u64, state: WriteState, caller: u64, frame: u64) -> Vec<u8> {
        let answering = (caller == s).then_some(frame);
        protocol::encode_reply(&Reply::Acked {
            ack: self.ack(s, answering),
            body: Box::new(Reply::SessionWriteState { session: s, write_id, state }),
        })
        .expect("a verdict encodes")
    }

    /// One call: a v5 frame in, the replies out.
    fn call(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let env = match protocol::decode_request(frame) {
            protocol::Incoming::Ok(env) if env.version >= protocol::FLOOR_SINCE => env,
            other => panic!("the rev-3 node is fed v5 frames only: {other:?}"),
        };
        let (s, f) = (env.session, env.frame.expect("a v5 frame carries its sequence"));
        let seen = self.frames_seen.entry(s).or_insert(0);
        *seen = (*seen).max(f);
        match env.body {
            Request::WriteFrom { write_id, floor, ops } => {
                let next = self.next(s);
                // 1. Below next: already PUBLISHED. Nothing applied; the Ack says so.
                if write_id < next {
                    return vec![self.verdict(s, write_id, WriteState::Duplicate, s, f)];
                }
                // 2. A commit pending: Busy, `next` untouched.
                if self.commit.is_some() {
                    return vec![self.verdict(s, write_id, WriteState::Busy, s, f)];
                }
                // 3. The expected one: take it. (`floor > next` is ordinary —
                //    ids have gaps where a write was refused at make.)
                let expected = next.max(floor);
                if write_id == expected {
                    self.applied.push((s, write_id));
                    self.taken_through.insert(s, write_id);
                    self.commit = Some(Rev3Commit { session: s, write_id, ops, landed: false });
                    return vec![self.verdict(s, write_id, WriteState::Accepted, s, f)];
                }
                // 4. Anything else: OutOfOrder, nothing applied.
                vec![self.verdict(s, write_id, WriteState::OutOfOrder { expected }, s, f)]
            }
            // A tick moves the commit one step — and its verdict goes to the
            // COMMIT's session, whoever ticked (F49's shape).
            Request::Tick { .. } => self.step(s, f),
            other => panic!("the rev-3 node's frame tests send WriteFrom and Tick only: {other:?}"),
        }
    }

    /// The commit moves one step: its head PUT lands — the tree AND the
    /// ledger, one record — then it is answered Published.
    fn step(&mut self, caller: u64, frame: u64) -> Vec<Vec<u8>> {
        let Some(c) = self.commit.clone() else { return Vec::new() };
        if !c.landed {
            for op in &c.ops {
                match op {
                    Op::Put(k, v) => {
                        self.tree.insert(k.clone(), v.clone());
                    }
                    Op::Delete(k) => {
                        self.tree.remove(k);
                    }
                }
            }
            // ONLY A PUBLISH CONSUMES A NUMBER.
            self.ledger.insert(c.session, c.write_id);
            self.commit.as_mut().expect("the commit").landed = true;
            return Vec::new();
        }
        self.commit = None;
        vec![self.verdict(c.session, c.write_id, WriteState::Published, caller, frame)]
    }

    /// The commit FAILS (the engine refused it): nothing applied, and the
    /// number is RELEASED — the ledger does not move.
    fn fail(&mut self, caller: u64, frame: u64) -> Vec<Vec<u8>> {
        let Some(c) = self.commit.take() else { return Vec::new() };
        assert!(!c.landed, "a landed commit does not fail");
        self.applied.retain(|a| *a != (c.session, c.write_id));
        vec![self.verdict(c.session, c.write_id, WriteState::Failed, caller, frame)]
    }

    /// The commit is LOST (a head conflict, settle rounds exhausted) before
    /// its head lands: nothing applied, and — like Failed — the number is
    /// RELEASED; only a publish consumes one.
    fn lose(&mut self, caller: u64, frame: u64) -> Vec<Vec<u8>> {
        let Some(c) = self.commit.take() else { return Vec::new() };
        assert!(!c.landed, "a landed commit is not lost: its head holds it");
        self.applied.retain(|a| *a != (c.session, c.write_id));
        vec![self.verdict(c.session, c.write_id, WriteState::Lost, caller, frame)]
    }

    /// The context is LOST: the commit in flight, what was taken, what frames
    /// were seen. The tree and its head's ledger stay.
    fn forget(&mut self) {
        // A take whose head never landed is WITHDRAWN: nothing of it is in the
        // tree or the ledger. (A landed one stays applied: its head holds it.)
        if let Some(c) = self.commit.as_ref().filter(|c| !c.landed) {
            let taken = (c.session, c.write_id);
            self.applied.retain(|a| *a != taken);
        }
        self.commit = None;
        self.taken_through.clear();
        self.frames_seen.clear();
    }
}

mod rev3 {
    use super::*;

    const A: u64 = 0x0000_1234_5678_9abc;
    const B: u64 = 0x0000_2345_6789_abcd;

    fn write(s: u64, frame: u64, write_id: u64, floor: u64, v: &str) -> Vec<u8> {
        protocol::encode_v5_request(s, 1_790_000_000_000, frame, &Request::WriteFrom { write_id, floor, ops: vec![Op::Put(b"k".to_vec(), v.as_bytes().to_vec())] })
            .expect("encodes")
    }
    fn tick(s: u64, frame: u64) -> Vec<u8> {
        protocol::encode_v5_request(s, 1_790_000_000_000, frame, &Request::Tick { now: 1 }).expect("encodes")
    }
    /// (whose Ack, the Ack, whose write, which write, the state) of each reply.
    fn read(replies: &[Vec<u8>]) -> Vec<(protocol::Ack, u64, u64, WriteState)> {
        replies
            .iter()
            .map(|b| match protocol::decode_reply(b).expect("decodes") {
                Reply::Acked { ack, body } => match *body {
                    Reply::SessionWriteState { session, write_id, state } => (ack, session, write_id, state),
                    other => panic!("{other:?}"),
                },
                other => panic!("a reply to a v5 client that is not Acked: {other:?}"),
            })
            .collect()
    }
    fn one(replies: Vec<Vec<u8>>) -> (protocol::Ack, WriteState) {
        let r = read(&replies);
        assert_eq!(r.len(), 1, "{r:?}");
        (r[0].0, r[0].3)
    }
    /// Take, land, publish w`id` of `s` with ticks from `s`.
    fn publish(n: &mut Rev3Node, s: u64, frame: &mut u64, id: u64, floor: u64) {
        *frame += 1;
        assert_eq!(one(n.call(&write(s, *frame, id, floor, "v"))).1, WriteState::Accepted);
        *frame += 1;
        assert!(n.call(&tick(s, *frame)).is_empty());
        *frame += 1;
        assert_eq!(one(n.call(&tick(s, *frame))).1, WriteState::Published);
    }

    /// Rule 3, then the ledger: taken in order, published, and the Ack says
    /// how far — on EVERY reply, with the frame it answers, parity unknown.
    #[test]
    fn in_order_writes_are_taken_and_the_ack_says_how_far() {
        let mut n = Rev3Node::default();
        let (ack, st) = one(n.call(&write(A, 1, 1, 1, "a")));
        assert_eq!(st, WriteState::Accepted);
        assert_eq!((ack.session, ack.published_through, ack.taken_through, ack.frames_seen, ack.answering, ack.parity), (A, 0, 1, 1, Some(1), None));
        assert!(n.call(&tick(A, 2)).is_empty());
        let (ack, st) = one(n.call(&tick(A, 3)));
        assert_eq!(st, WriteState::Published);
        assert_eq!((ack.published_through, ack.answering), (1, Some(3)));
    }

    /// Rule 1: a write at or below what is published is a DUPLICATE —
    /// nothing applied, and the Ack says it published.
    #[test]
    fn a_published_write_sent_again_is_a_duplicate_and_applied_once() {
        let mut n = Rev3Node::default();
        let mut f = 0;
        publish(&mut n, A, &mut f, 1, 1);
        let (ack, st) = one(n.call(&write(A, 10, 1, 1, "a")));
        assert_eq!(st, WriteState::Duplicate);
        assert_eq!(ack.published_through, 1, "the Duplicate's Ack must say it PUBLISHED");
        assert_eq!(n.applied, vec![(A, 1)], "applied twice");
    }

    /// Rule 2: while a commit is pending, Busy — and `next` does not move.
    #[test]
    fn a_write_while_a_commit_is_pending_is_busy() {
        let mut n = Rev3Node::default();
        assert_eq!(one(n.call(&write(A, 1, 1, 1, "a"))).1, WriteState::Accepted);
        assert_eq!(one(n.call(&write(B, 1, 1, 1, "b"))).1, WriteState::Busy);
        assert_eq!(n.next(B), 1);
    }

    /// Rule 3's floor arm: a gap (an id refused at make, never sent) is
    /// ORDINARY — `floor > next` takes the floor.
    #[test]
    fn a_gap_below_the_floor_is_ordinary() {
        let mut n = Rev3Node::default();
        let mut f = 0;
        publish(&mut n, A, &mut f, 1, 1);
        // w2 was refused at make; the client's floor is now 3.
        assert_eq!(one(n.call(&write(A, 20, 3, 3, "c"))).1, WriteState::Accepted);
    }

    /// Rule 4: anything else is OutOfOrder{expected} — nothing applied. THE
    /// sdk#179 c case: W2 Busy'd, W3 already at the node → W3 is refused, not
    /// applied ahead of W2.
    #[test]
    fn a_write_ahead_of_its_turn_is_out_of_order_and_not_applied() {
        let mut n = Rev3Node::default();
        let mut f = 0;
        publish(&mut n, A, &mut f, 1, 1);
        // The client still holds w2 un-ended (floor 2) and sends w3.
        let (_, st) = one(n.call(&write(A, 30, 3, 2, "new")));
        assert_eq!(st, WriteState::OutOfOrder { expected: 2 });
        assert_eq!(n.applied, vec![(A, 1)], "w3 was applied ahead of w2");
        // w2, then w3, in order.
        let mut f2 = 30;
        publish(&mut n, A, &mut f2, 2, 2);
        publish(&mut n, A, &mut f2, 3, 3);
        assert_eq!(n.applied, vec![(A, 1), (A, 2), (A, 3)]);
    }

    /// ONLY A PUBLISH CONSUMES A NUMBER: a Failed releases it, and the same
    /// write sent again gets a TRUE second attempt.
    #[test]
    fn a_failed_write_releases_its_number() {
        let mut n = Rev3Node::default();
        assert_eq!(one(n.call(&write(A, 1, 1, 1, "a"))).1, WriteState::Accepted);
        let (ack, st) = one(n.fail(A, 1));
        assert_eq!(st, WriteState::Failed);
        assert_eq!(ack.published_through, 0, "a failed write was counted published");
        assert_eq!(one(n.call(&write(A, 2, 1, 1, "a"))).1, WriteState::Accepted, "no second attempt after Failed");
    }

    /// Lost releases its number too: nothing published, a true second attempt.
    #[test]
    fn a_lost_write_releases_its_number() {
        let mut n = Rev3Node::default();
        assert_eq!(one(n.call(&write(A, 1, 1, 1, "a"))).1, WriteState::Accepted);
        let (ack, st) = one(n.lose(A, 1));
        assert_eq!(st, WriteState::Lost);
        assert_eq!(ack.published_through, 0, "a lost write was counted published");
        assert_eq!(one(n.call(&write(A, 2, 1, 1, "a"))).1, WriteState::Accepted, "no second attempt after Lost");
    }

    /// The ledger moves WITH the head, not before: a context lost after the
    /// take and BEFORE the head PUT published nothing, so the write sent again
    /// is TAKEN — not answered Duplicate for a write that never landed.
    #[test]
    fn a_context_lost_before_the_head_lands_publishes_nothing() {
        let mut n = Rev3Node::default();
        assert_eq!(one(n.call(&write(A, 1, 1, 1, "a"))).1, WriteState::Accepted);
        n.forget(); // before any tick: the head PUT never went
        assert_eq!(n.taken_through.get(&A), None, "taken_through survived the forget: it is the context's");
        assert!(n.applied.is_empty(), "the un-landed take was not withdrawn");
        let (ack, st) = one(n.call(&write(A, 2, 1, 1, "a")));
        assert_eq!(st, WriteState::Accepted, "a write that never landed was told Duplicate");
        assert_eq!(ack.published_through, 0);
        assert_eq!(ack.taken_through, 1, "the re-send is taken again");
        assert!(!n.tree.contains_key(b"k".as_slice()), "the tree changed without a head PUT");
        // …and that is ONE apply, not two: the forgotten take was withdrawn.
        assert!(n.call(&tick(A, 3)).is_empty());
        assert_eq!(one(n.call(&tick(A, 4))).1, WriteState::Published);
        assert_eq!(n.applied, vec![(A, 1)], "a correct recovery was counted as a double apply");
        assert_eq!(n.tree.get(b"k".as_slice()).map(|v| v.as_slice()), Some(b"a".as_slice()));
    }

    /// THE LEDGER SURVIVES A FORGET — run (a): the head PUT landed, the
    /// context died before its answer, the write is sent again. A fresh
    /// engine reads the ledger: Duplicate, published — NOT applied twice.
    #[test]
    fn the_ledger_survives_a_forget_and_nothing_is_applied_twice() {
        let mut n = Rev3Node::default();
        assert_eq!(one(n.call(&write(A, 1, 1, 1, "a"))).1, WriteState::Accepted);
        assert!(n.call(&tick(A, 2)).is_empty()); // the head PUT lands
        n.forget(); // …and the context dies before the Published
        // B writes the key in between.
        let mut fb = 0;
        publish(&mut n, B, &mut fb, 1, 1);
        let (ack, st) = one(n.call(&write(A, 3, 1, 1, "a")));
        assert_eq!(st, WriteState::Duplicate, "run (a): the write was taken again");
        assert_eq!(ack.published_through, 1);
        assert_eq!(n.applied.iter().filter(|a| **a == (A, 1)).count(), 1, "applied twice across the forget");
        // …and taken_through went BACKWARDS with the context: it says 0.
        assert_eq!(ack.taken_through, 0);
    }

    /// A verdict decided in ANOTHER session's call (F49's shape) carries its
    /// OWN session's Ack — and answers none of that session's frames.
    #[test]
    fn a_verdict_decided_in_anothers_call_answers_none_of_its_frames() {
        let mut n = Rev3Node::default();
        assert_eq!(one(n.call(&write(A, 1, 1, 1, "a"))).1, WriteState::Accepted);
        assert!(n.call(&tick(B, 1)).is_empty());
        let r = read(&n.call(&tick(B, 2)));
        assert_eq!(r.len(), 1);
        let (ack, session, _, state) = r[0];
        assert_eq!((session, state), (A, WriteState::Published));
        assert_eq!(ack.session, A, "the Ack names the WRITE's session");
        assert_eq!(ack.answering, None, "B's frame was reported as A's");
    }
}
