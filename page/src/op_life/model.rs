//! OP-LIFE.md draft 3.1's GATE: seeded random event sequences over the REAL page, with a NODE MODEL beside it and the
//! invariants checked after EVERY step against the model's own bookkeeping -- never against the page's record:
//! - (a) L0, the page's side: a send LEAVES its key only by an answer, its bound, a reconnect, or (a `RtoResend` key)
//!   its last wait's withdraw -- so the page never makes a second send of a key while it still holds the first;
//! - (b) L1: a Background send never goes out while the page still holds another Background send or owes the slot to
//!   a reconnect's orphan; overlaps at the node beyond that are F1's, begun at a bound, and counted;
//! - (d) a LATE answer is INERT: delivered when the page holds no send of its key, it changes nothing but counters;
//! - (c) a head answer never ends a `ReadBack` whose wait began after the read it answers was sent;
//! - L2-L7, and "who waits": each wait the driver added and nobody served or withdrew still waits, and no other;
//! - L4 from the recording: one end per send.
//!
//! The node model TAKES each op after a queue delay (F61), works on it within its kind's bound, then answers it --
//! delivered, or lost at one of the three real drop points (the result router full; the session actor full; a
//! delegate response's notification channel full) -- and a reconnect sends every undelivered answer to the closed
//! socket while the node runs the op to its end (the disconnect cancels nothing).
// A TEST driver: its matches over the pool are not the record's (op_life's lints are for production).
#![allow(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]
use crate::op_life::{self, bound_at, in_flight, key_of, resend, Entry, InFlight, Key, OpEvent};
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------- the node model

/// One op the node took from this client.
#[derive(Debug, Clone)]
struct NodeOp {
    key: Key,
    w: Waiting,
    op: Op,
    seq: u32,
    lane: Lane,
    /// The page's bound for it (probably over after this, F1).
    bound: u64,
    /// When the node takes it (after its one queue's delay) and when its work ends.
    done_at: u64,
    /// The socket it came on (a reconnect closes it: its answer never arrives).
    socket: u32,
    /// Its answer's fate: `None` until the node answered; then delivered (to the page) or lost at a drop point.
    answered: Option<Fate>,
    /// A Background op a reconnect orphaned whose key was PROMOTED: F3 exempts it from the slot's debt (it no longer
    /// needs the slot), so L1 does not count it.
    exempt: bool,
    /// The page ENDED this send UNANSWERED on a live socket (its recording's End: Timeout at its bound, or Withdrawn by
    /// anything but a reconnect -- at its bound, or an RtoResend key's last withdraw) -- read from the page's
    /// recording, never judged by the model. Its work may go on, and its answer may still come (F1).
    bound_ended: bool,
    /// A send of a key some EARLIER send of which was `bound_ended` (the architect: any earlier, not only the last --
    /// an RtoResend key can leave several): such a send's late answer may be taken as this one's (F1, inert and
    /// uncounted), ending it in the page while the node still works it.
    after_bound_end: bool,
    /// Sent AT its predecessor's bound (the page's `overlap` tag).
    overlap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Delivered,
    /// The node produced it and lost it: 1 the result router full, 2 the session actor full, 3 a delegate response's
    /// notification channel full.
    Dropped(u8),
    /// Its socket was closed before it could be delivered.
    Closed,
}

struct Node {
    ops: Vec<NodeOp>,
    socket: u32,
}

impl Node {
    /// Background ops the node still WORKS at `now` (taken or queued, their work not ended), on ANY socket: a
    /// reconnect closes the socket, never the node's work. F3's promoted orphans are exempt.
    fn background_alive(&self, now: u64) -> Vec<&NodeOp> {
        self.ops.iter().filter(|o| o.lane == Lane::Background && !o.exempt && now < o.done_at).collect()
    }
}

// ---------------------------------------------------------------- the driver

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn op_for(w: &Waiting, sign_id: u32) -> Op {
    match w {
        Waiting::Get(id) => Op::Get { id: *id },
        Waiting::Put(id) | Waiting::Repair(id) => Op::Put { id: *id, bytes: vec![id[0]] },
        Waiting::PutApp(k, _) => Op::PutApp { key: k.clone() },
        Waiting::Update(l) => Op::Update { label: l.clone(), state: vec![1] },
        Waiting::Sign(l) => Op::Sign { id: sign_id, prev_seq: 0, prev_root: [0; 32], seq: 1, root: [0x5a; 32], ledger: Vec::new(), label: l.clone() },
        Waiting::ReadBack(l) => Op::ReadHead { label: l.clone() },
        Waiting::Warm | Waiting::RecoverHead | Waiting::Verify | Waiting::Hint => Op::ReadHead { label: Label::Head },
        other => panic!("not in the pool: {other:?}"),
    }
}

/// THE POOL: GET and PUT keys (both lanes, by the waiter hook), the head register's five reads, the head's UPDATE,
/// and one named key (a site's Sign).
fn pool() -> Vec<Waiting> {
    let mut v = Vec::new();
    for i in 0..3u8 {
        v.push(Waiting::Get([0x10 + i; 32]));
        v.push(Waiting::Put([0x20 + i; 32]));
    }
    v.extend([Waiting::ReadBack(Label::Head), Waiting::Verify, Waiting::Hint, Waiting::Warm, Waiting::RecoverHead]);
    v.push(Waiting::Update(Label::Head));
    v.push(Waiting::Sign(Label::Site("model".into())));
    // TWO WAITERS PER SHARED KEY (sdk#531): two publishes' asks on one container, and a read repair beside the engine's
    // own waiter on a block's PUT (the engine's withdraw leaves the repair's PUT going). The register's read key has five.
    v.push(Waiting::PutApp("container".into(), Ask::Publish("a".into())));
    v.push(Waiting::PutApp("container".into(), Ask::Publish("b".into())));
    v.push(Waiting::Repair([0x20; 32]));
    v
}

/// A wait's short name for the log.
fn short(w: &Waiting) -> String {
    match w {
        Waiting::Get(id) => format!("G{:x}", id[0]),
        Waiting::Put(id) => format!("P{:x}", id[0]),
        Waiting::Update(_) => "U".into(),
        Waiting::Sign(_) => "S".into(),
        Waiting::ReadBack(_) => "RB".into(),
        Waiting::Verify => "Ver".into(),
        Waiting::Hint => "Hint".into(),
        Waiting::Warm => "Warm".into(),
        Waiting::RecoverHead => "Rec".into(),
        Waiting::PutApp(_, Ask::Publish(a)) => format!("App{a}"),
        Waiting::Repair(id) => format!("R{:x}", id[0]),
        other => format!("{other:?}"),
    }
}

/// The key the node NEVER answers (L5's witness): Background only.
fn black_hole() -> Waiting {
    Waiting::Put([0x22; 32])
}

struct Run {
    p: Page,
    /// THE SHADOW (d): a second REAL page driven with every event but the late answers. A late answer is inert iff the
    /// two stay identical -- state and effects, counters aside -- after every step.
    shadow: Page,
    node: Node,
    /// WHO WAITS, by the driver's own book: each wait added by a send and not yet served or withdrawn.
    waits: BTreeSet<Waiting>,
    /// When each ReadBack wait began (c).
    read_back_since: Option<u64>,
    /// The sends the page held after the last step, with their bound as the page set it (a).
    held: BTreeMap<u32, (Key, u64, bool)>,
    /// Keys an answer was delivered for this step, and whether this step reconnected (a).
    answered_keys: BTreeSet<Key>,
    reconnected: bool,
    /// Sends the page made, by seq, with the socket they went on.
    sent: BTreeMap<u32, (Key, u64, u32, bool)>,
    /// When each Background key started waiting for the slot (L5).
    queued_since: BTreeMap<Key, u64>,
    sign_id: u32,
    /// Late answers delivered to the page and withheld from the shadow: (d)'s reach.
    late_delivered: u64,
    prev_in_window: usize,
    new_window_send: bool,
    found: BTreeMap<&'static str, String>,
    log: Vec<String>,
    step: usize,
    /// Each key's last send, by seq (the NEXT send after a bound-end is F1's overlap).
    last_sent: BTreeMap<Key, u32>,
    /// How far the page's recording has been read.
    rec_read: usize,
    /// L1's DETECTOR (the architect: an UPPER bound on overlaps, derived from the recording): the sends ended
    /// unanswered on a live socket before a Background send of the same key. And the extra Background ops (b) saw at
    /// the node, each counted once.
    detected: BTreeSet<u32>,
    extras_seen: BTreeSet<u32>,
    /// Every send whose ONE end the recording holds (L4): an answer's Response edge or an Exit.
    ended: BTreeSet<u32>,
    /// Keys whose next send must be tagged overlap: a send of theirs timed out at its bound (the recording's Timeout).
    overlap_owed: BTreeSet<Key>,
    /// Overlap sends (iii) compared tagged on both sides: its floor.
    overlaps_tagged: u64,
}

impl Run {
    /// The page's recording since the last read: every send it ENDED AT ITS BOUND unanswered (Timeout; or Withdrawn,
    /// no reconnect this step, at or past its bound) marks that node op `bound_ended`. The page's own record; the model
    /// judges nothing.
    fn read_ends(&mut self) {
        use instrument::{Event, Outcome, Record};
        let r = self.p.recording().expect("the model's page records");
        if r.dropped() > 0 {
            let d = r.dropped();
            self.fail("L4 one End per send (the recording)", format!("{d} events dropped: the ring is too small for the run"));
            return;
        }
        let events: Vec<Event> = r.events();
        for e in &events[self.rec_read.min(events.len())..] {
            if let Event::Edge { dir: instrument::Dir::Response, id, .. } = e {
                self.ended.insert(id.ordinal());
            }
            let Event::Exit { op, outcome, .. } = e else { continue };
            let Some(seq) = instrument::Label::of_op(*op).filter(|l| l.kind() == instrument::Kind::Request).map(|l| l.ordinal()) else { continue };
            self.ended.insert(seq);
            let reconnected = self.reconnected;
            if let Some(o) = self.node.ops.iter_mut().find(|o| o.seq == seq) {
                // ENDED UNANSWERED on a live socket: at its bound (Timeout, or Withdrawn at it) or by an RtoResend key's
                // last withdraw (Withdrawn at once) -- either way its work may go on and its answer may still come. A
                // reconnect's Withdrawn is not one: the closed socket never answers (its slot is the debt's, Q4).
                let answered = matches!(o.answered, Some(Fate::Delivered));
                let at_bound = match outcome {
                    Outcome::Timeout => true,
                    Outcome::Withdrawn => !reconnected && !answered,
                    _ => false,
                };
                if at_bound {
                    o.bound_ended = true;
                }
                // (iii): a send TIMED OUT at its bound makes the key's next send an overlap -- unless the key re-sends
                // under a fresh id (a Sign) or re-sends on its RTO by design (RtoResend).
                if matches!(outcome, Outcome::Timeout) && !matches!(o.op, Op::Sign { .. }) && in_flight(resend(&o.op), o.lane) != InFlight::RtoResend {
                    self.overlap_owed.insert(o.key.clone());
                }
            }
        }
        self.rec_read = events.len();
    }

    /// An event to BOTH pages (the shadow misses only late answers).
    fn both(&mut self, f: impl Fn(&mut Page)) {
        f(&mut self.p);
        f(&mut self.shadow);
    }

    fn fail(&mut self, inv: &'static str, why: String) {
        if !self.found.contains_key(inv) {
            let tail: Vec<String> = self.log.iter().rev().take(8).rev().cloned().collect();
            self.found.insert(inv, format!("step {}: {why}\n    last steps: {}", self.step, tail.join(" | ")));
        }
    }

    /// Everything the page put on the wire since the last look: each becomes a node op (checked against (a) first).
    fn collect(&mut self, rng: &mut Rng) {
        // (d): the shadow emits exactly what the page does (a late answer has no EFFECT).
        let (ops, shadow_ops) = (self.p.take_ops(), self.shadow.take_ops());
        if ops != shadow_ops {
            self.fail("(d) a late answer is INERT", format!("the page sent {ops:?}, the page without the late answers {shadow_ops:?}"));
        }
        // THE PAGE'S OWN BOUND for each send: a silent send's `at` is its bound, set with the RTO at the moment it went
        // silent (the model's first guess used the RTO at the send).
        let silent: Vec<(u32, u64)> = self.p.ops.at_node().filter(|(_, s, _)| s.silent).map(|(_, s, _)| (s.seq, s.at)).collect();
        for (seq, at) in silent {
            if let Some(o) = self.node.ops.iter_mut().find(|o| o.seq == seq) {
                o.bound = at;
            }
        }
        self.read_ends();
        let rto = self.p.rto.rto_ms();
        // Every send in flight the node has not seen (a send no wait wants any more included), with its key's lane.
        let all: Vec<(Waiting, op_life::Send, Lane)> = self
            .p
            .ops
            .at_node()
            .filter(|(_, s, _)| !self.sent.contains_key(&s.seq))
            .map(|(w, s, _)| (w.clone(), s.clone(), self.p.ops.entry_of(&key_of(w)).map_or(Lane::Interactive, Entry::lane)))
            .collect();
        for (w, s, lane) in all {
            let k = key_of(&w);
            let next_after = self.node.ops.iter().any(|o| o.key == k && o.bound_ended);
            // (b) L1: a Background send never goes out beside another the page still holds, nor while the slot is owed.
            if lane == Lane::Background {
                let others = self.p.ops.at_node().filter(|(ow, os, _)| os.seq != s.seq && self.p.ops.entry_of(&key_of(ow)).is_some_and(|e| e.lane() == Lane::Background && e.send().is_some_and(|x| x.seq == os.seq))).count();
                if others > 0 {
                    self.fail("(b) L1 one background send", format!("background #{} went out beside {others} other the page still holds", s.seq));
                }
                // ... nor beside one the NODE still works that the page no longer holds (a reconnect's orphan: its slot is
                // owed until its bound, Q4) -- the node's work is the witness, not the page's debt. ONE allowance, F1's
                // overlaps (the architect's (B)), keyed on what the page RECORDED: an extra that ended at its bound, the
                // next send of a key whose previous send did, or a send tagged overlap. Every such extra is an upper-bounded
                // overlap: the detector (Background bound-ends) must cover it, asserted per seed.
                let extras: Vec<(u32, bool)> = self
                    .node
                    .background_alive(self.p.now)
                    .into_iter()
                    .filter(|o| o.seq != s.seq && o.answered.is_none())
                    .map(|o| {
                        // A reconnect's orphan past its bound: its debt (Q4) held the slot until then, and past it F1 holds
                        // (its work only probably over). Its End is the page's recorded reconnect Withdrawn.
                        let orphan_past_debt = o.socket != self.node.socket && self.p.now >= o.bound;
                        (o.seq, o.bound_ended || o.after_bound_end || o.overlap || s.overlap || next_after || orphan_past_debt)
                    })
                    .collect();
                if extras.iter().any(|(_, f1)| !f1) {
                    self.fail("(b) L1 one background send", format!("background #{} went out while the node still works background {extras:?} (seq, an F1 overlap)", s.seq));
                }
                self.extras_seen.extend(extras.iter().map(|(q, _)| *q));
                // THE DETECTOR's reach: each send of this key that ended unanswered on a live socket, BEFORE this
                // Background send of the key, is counted once.
                let ends: Vec<u32> = self.node.ops.iter().filter(|o| o.key == k && o.bound_ended && !self.detected.contains(&o.seq)).map(|o| o.seq).collect();
                self.detected.extend(ends);
                // ... and each reconnect's orphan whose debt the page let expire at its bound (its End recorded, its bound
                // the page's own) before this Background send.
                let now = self.p.now;
                let socket = self.node.socket;
                let orphans: Vec<u32> = self.node.ops.iter().filter(|o| o.lane == Lane::Background && o.socket != socket && now >= o.bound && !self.detected.contains(&o.seq)).map(|o| o.seq).collect();
                self.detected.extend(orphans);
            }
            let delay = rng.below(3) as u64 * 20_000; // F61: the node's queue, 0-40 s
            let bound = bound_at(&s.op, s.sent_at, rto);
            // The node's work ends within the bound (the node's own bound: the model never breaks it); the BLACK HOLE's
            // answer is simply never delivered.
            let work = (rng.below(4) as u64) * (bound - s.sent_at) / 4;
            self.node.ops.push(NodeOp {
                key: k.clone(),
                w: w.clone(),
                op: s.op.clone(),
                seq: s.seq,
                lane,
                bound,
                done_at: s.sent_at.saturating_add(delay).saturating_add(work),
                socket: self.node.socket,
                answered: None,
                exempt: false,
                bound_ended: false,
                after_bound_end: next_after,
                overlap: s.overlap,
            });
            if lane == Lane::Interactive && matches!(k, Key::Get(_)) {
                self.new_window_send = true;
            }
            // (iii) THE TAG CANNOT DRIFT (the architect): the page tags a send overlap exactly when the model, from the
            // recording's Timeout ends, says it goes at its predecessor's bound.
            let owed = self.overlap_owed.remove(&k);
            self.overlaps_tagged += u64::from(owed && s.overlap);
            if owed != s.overlap {
                self.fail("(iii) the overlap tag", format!("#{} ({k:?}) went out tagged overlap={} where the recording says {owed}", s.seq, s.overlap));
            }
            self.last_sent.insert(k.clone(), s.seq);
            self.sent.insert(s.seq, (k, s.sent_at, self.node.socket, s.overlap));
        }
    }

    /// The node answers ONE op whose work has ended: delivered, or lost at a drop point.
    fn node_answers(&mut self, rng: &mut Rng) {
        let now = self.p.now;
        // The node answers in the order its work ENDS (F61: one sequence per client): the oldest finished op first.
        let Some(i) = self.node.ops.iter().enumerate().filter(|(_, o)| o.answered.is_none() && now >= o.done_at).min_by_key(|(_, o)| o.done_at).map(|(i, _)| i) else {
            return;
        };
        let o = self.node.ops[i].clone();
        if o.socket != self.node.socket {
            self.node.ops[i].answered = Some(Fate::Closed);
            return;
        }
        let roll = if o.w == black_hole() { 0 } else { rng.below(20) };
        let fate = match roll {
            0 => Fate::Dropped(1),
            1 => Fate::Dropped(2),
            2 if matches!(o.op, Op::Sign { .. }) => Fate::Dropped(3),
            _ => Fate::Delivered,
        };
        self.node.ops[i].answered = Some(fate);
        if fate != Fate::Delivered {
            self.log.push(format!("drop#{} {:?}", o.seq, fate));
            return;
        }
        self.log.push(format!("answer#{} {}", o.seq, short(&o.w)));
        self.answered_keys.insert(o.key.clone());
        // (d): LATE (the page holds no send of this key) -> the shadow never hears it. The one exception is documented:
        // an AfterBound key's late answer verifies itself and SERVES a queued or parked wait -- both pages hear that.
        let entry = self.p.ops.entry_of(&o.key).cloned();
        let serves = matches!(&entry, Some(Entry::Queued { attempt, .. } | Entry::Parked { attempt, .. }) if *attempt >= 1 && resend(&o.op) == op_life::Resend::AfterBound);
        let late = entry.as_ref().is_none_or(|e| e.send().is_none()) && !serves;
        let without = rng.below(6) == 0;
        // (c): which send the page's in-flight read is, before the answer lands.
        let in_flight_read = self.p.ops.at_node().find(|(w, ..)| key_of(w) == Key::Read(Label::Head)).map(|(_, s, _)| s.sent_at);
        let ev = match &o.op {
            Op::Sign { id, .. } => OpEvent::AnswerNamed { request: *id },
            Op::ReadHead { .. } => OpEvent::AnswerRead { park: BTreeSet::new() },
            Op::Get { .. } if without => OpEvent::AnswerWithout { op: o.op.clone() },
            _ => OpEvent::Answer,
        };
        let did = self.p.on(&o.w, ev.clone());
        if late {
            self.late_delivered += 1;
        } else {
            self.shadow.on(&o.w, ev);
        }
        if let op_life::Did::Answered { served, .. } = did {
            if served.contains(&Waiting::ReadBack(Label::Head)) {
                if let (Some(since), Some(sent)) = (self.read_back_since, in_flight_read) {
                    if sent < since {
                        let why = format!("a head read sent at {sent} ended a ReadBack that began at {since}");
                        self.fail("(c) a joined head answer predating its need never ends a ReadBack", why);
                    }
                }
                self.read_back_since = None;
            }
            for w in served {
                self.waits.remove(&w);
            }
        }
        // An answered-without GET parks: its wait still waits.
    }

    /// What a late answer must leave unchanged: every wait and its entry, every send in flight, what the page has to
    /// send, and its publications -- counters aside.
    fn projection(p: &Page) -> String {
        let waits: Vec<String> = p.ops.needs().map(|(w, e)| format!("{}:{:?}/{}/{:?}", short(w), std::mem::discriminant(e), e.lane() == Lane::Background, e.send().map(|x| x.seq))).collect();
        let wire: Vec<u32> = p.ops.at_node().map(|(_, s, _)| s.seq).collect();
        format!("waits {waits:?} wire {wire:?} out {} unusable {} head {:?} slot {}", p.out.len(), p.unusable.len(), p.pubs.head(), p.ops.in_slot())
    }

    fn check(&mut self) {
        let now = self.p.now;
        self.read_ends();
        // (iii): a key the page no longer has an entry for starts fresh -- its next send overlaps nothing.
        let gone: Vec<Key> = self.overlap_owed.iter().filter(|k| self.p.ops.entry_of(k).is_none()).cloned().collect();
        for k in gone {
            self.overlap_owed.remove(&k);
        }
        // (d): the page and the page without the late answers are identical, counters aside.
        let (a, b) = (Run::projection(&self.p), Run::projection(&self.shadow));
        if a != b {
            self.fail("(d) a late answer is INERT", format!("with the late answers {a}\n      without them         {b}"));
        }
        // (a) L0, the page's side: a send the page held last step and holds no more LEFT only by an answer to its key,
        // its bound, a reconnect, or its RtoResend key's last withdraw.
        let holding: BTreeMap<u32, (Key, u64, bool)> = self
            .p
            .ops
            .at_node()
            .map(|(w, s, _)| {
                let rto_resend = self.p.ops.entry_of(&key_of(w)).is_some_and(|e| in_flight(resend(&s.op), e.lane()) == InFlight::RtoResend);
                (s.seq, (key_of(w), if s.silent { s.at } else { u64::MAX }, rto_resend))
            })
            .collect();
        let left: Vec<(u32, (Key, u64, bool))> = self.held.iter().filter(|(q, _)| !holding.contains_key(q)).map(|(q, v)| (*q, v.clone())).collect();
        for (seq, (k, bound, rto_resend)) in left {
            let by_answer = self.answered_keys.contains(&k);
            let by_bound = now >= bound;
            let by_withdraw = rto_resend && !self.waits.iter().any(|w| key_of(w) == k);
            // A RtoResend key re-sent on its RTO replaces its send (the same bytes), and so does a Sign re-asked.
            let replaced = holding.values().any(|(hk, ..)| *hk == k);
            if !(by_answer || by_bound || self.reconnected || by_withdraw || replaced) {
                self.fail("(a) L0 a send leaves only by its answer, its bound or a reconnect", format!("#{seq} ({k:?}) left the page with none of them"));
            }
        }
        self.held = holding;
        self.answered_keys.clear();
        self.reconnected = false;
        // WHO WAITS: the driver's book against the page.
        for w in pool() {
            let page = self.p.ops.contains(&w);
            let book = self.waits.contains(&w);
            if page != book {
                self.fail("who waits (the driver's book)", format!("{w:?}: the page waits {page}, the book {book}"));
            }
        }
        // Q4: while a reconnect's debt holds the slot, no Background key is on the wire (one Option is enough).
        if self.p.slot_owed_until().is_some_and(|at| now < at) && self.p.ops.at_node().any(|(w, ..)| self.p.ops.entry_of(&key_of(w)).is_some_and(|e| e.lane() == Lane::Background)) {
            self.fail("Q4 the slot's debt holds the slot", "a Background key went on the wire while a reconnect's debt was owed".into());
        }
        // L2: work-conserving.
        let entries: Vec<(Waiting, Entry)> = self.p.ops.needs().map(|(w, e)| (w.clone(), e.clone())).collect();
        let queued_b = entries.iter().any(|(_, e)| matches!(e, Entry::Queued { lane: Lane::Background, .. }));
        if self.p.ops.in_slot() == 0 && queued_b {
            self.fail("L2 work-conserving", "the slot is free and a Background key is queued".into());
        }
        let queued_get = entries.iter().any(|(w, e)| matches!(w, Waiting::Get(_)) && matches!(e, Entry::Queued { lane: Lane::Interactive, .. }));
        if self.p.ops.in_window() < self.p.window.size() && queued_get {
            self.fail("L2 work-conserving", "the window has room and an interactive GET is queued".into());
        }
        // L3: an app never waits behind background work.
        for (w, e) in &entries {
            if self.p.lane_of(w) == Lane::Interactive && matches!(e, Entry::Queued { lane: Lane::Background, .. } | Entry::Parked { lane: Lane::Background, .. }) {
                self.fail("L3 app behind background", format!("{w:?} has an interactive waiter and is {:?}", std::mem::discriminant(e)));
            }
        }
        // L5: a queued Background key (not the black hole) reaches the wire within its bound.
        let longest = bound_at(&Op::Put { id: [0; 32], bytes: vec![] }, 0, rto::RTO_MAX_MS as u64);
        let keys = pool().len() as u64 + 1;
        for (w, e) in &entries {
            let k = key_of(w);
            match e {
                Entry::Queued { lane: Lane::Background, .. } => {
                    let since = *self.queued_since.entry(k.clone()).or_insert(now);
                    if now.saturating_sub(since) > keys * longest {
                        self.fail("L5 no need waits for ever", format!("{w:?} queued for the slot since {since}"));
                    }
                }
                _ => {
                    self.queued_since.remove(&k);
                }
            }
        }
        // L7: the window counts keys -- no NEW interactive GET send goes out while the window is full (a loss halves the
        // window under the GETs already out, and a withdrawn GET in flight that a wait rides again takes no new place).
        if self.new_window_send && self.p.ops.in_window() > self.p.window.size() {
            self.fail("L7 the window counts keys", format!("a new interactive GET went out with {} places held, window {}", self.p.ops.in_window(), self.p.window.size()));
        }
        self.new_window_send = false;
        self.prev_in_window = self.p.ops.in_window();
        // L4: one end per send, from the recording.
        use instrument::{Dir, Event as IE, Record};
        if let Some(r) = self.p.recording() {
            let mut ends: BTreeMap<u32, usize> = BTreeMap::new();
            for e in r.events() {
                let n = match e {
                    IE::Edge { dir: Dir::Response, id, .. } => Some(id.ordinal()),
                    IE::Exit { op, .. } => (1..=self.p.sends).find(|n| instrument::Label::new(instrument::Kind::Request, *n).is_some_and(|l| l.op() == op)),
                    _ => None,
                };
                if let Some(n) = n {
                    *ends.entry(n).or_insert(0) += 1;
                }
            }
            if let Some((n, c)) = ends.iter().find(|(_, c)| **c > 1) {
                let why = format!("send #{n} recorded {c} ends");
                self.fail("L4 one end per send", why);
            }
        }
    }
}

/// A seed's run: the invariants it broke, and the Background answers counted after their bound.
struct Ran {
    found: BTreeMap<&'static str, String>,
    late_background: u64,
    late_delivered: u64,
    bound_ends_b: u64,
    overlaps_seen: u64,
    overlaps_tagged: u64,
}

fn run(seed: u64, steps: usize) -> Ran {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let page = || {
        let mut p = Page::new_at(Params::default(), PutPath::Page, Ms(1_790_253_181_367));
        p.answered(&Waiting::RecoverHead);
        let _ = p.take_ops();
        p.record_into(1 << 20);
        // The model's Sign key has an OWNER, as every Sign does (PUBLISH-LIFE): a publication SIGNING, so a lost sign
        // is asked again (SignLost, under a fresh id) by its Life. The model answers the record directly: it stays
        // Signing.
        use crate::publication::{Life, Owed};
        p.pubs.set_site_for_test("model", Life::Signing { owed: Owed { seq: 1, root: [0x5a; 32], base: [0u8; 32] }, refusals: 0, why: None });
        p
    };
    let mut m = Run { p: page(), shadow: page(), node: Node { ops: Vec::new(), socket: 0 }, waits: BTreeSet::new(), read_back_since: None, held: BTreeMap::new(), answered_keys: BTreeSet::new(), reconnected: false, sent: BTreeMap::new(), queued_since: BTreeMap::new(), sign_id: 1000, late_delivered: 0, prev_in_window: 0, new_window_send: false, found: BTreeMap::new(), log: Vec::new(), step: 0, last_sent: BTreeMap::new(), rec_read: 0, detected: BTreeSet::new(), extras_seen: BTreeSet::new(), ended: BTreeSet::new(), overlap_owed: BTreeSet::new(), overlaps_tagged: 0 };
    let keys = pool();
    let trace: Option<u64> = std::env::var("OPLIFE_TRACE").ok().and_then(|v| v.parse().ok());
    for step in 0..steps {
        m.step = step;
        let w = keys[rng.below(keys.len())].clone();
        let roll = rng.below(100);
        if roll < 30 {
            // A need for the key, in the lane its waiter hook gives it. The class is chosen when the key is FRESH (no
            // entry): a waiter's class moves only B -> I (L6), so the driver never demotes a key it has.
            if rng.below(3) == 0 && m.p.ops.entry_of(&key_of(&w)).is_none() {
                m.both(|p| {
                    p.test_background.insert(w.clone());
                });
            }
            if w == black_hole() {
                m.both(|p| {
                    p.test_background.insert(w.clone());
                });
            }
            m.sign_id += 1;
            let op = op_for(&w, m.sign_id);
            m.log.push(format!("send{} {}", if m.p.test_background.contains(&w) { "B" } else { "I" }, short(&w)));
            if w == Waiting::ReadBack(Label::Head) && !m.waits.contains(&w) {
                m.read_back_since = Some(m.p.now);
            }
            m.waits.insert(w.clone());
            m.both(|p| p.send(w.clone(), op.clone()));
        } else if roll < 40 {
            // join·I: its waiters class it Interactive now.
            if w != black_hole() {
                m.both(|p| {
                    p.test_background.remove(&w);
                    p.promote_joined();
                });
                m.log.push(format!("join {}", short(&w)));
            }
        } else if roll < 48 {
            m.log.push(format!("withdraw {}", short(&w)));
            m.waits.remove(&w);
            if w == Waiting::ReadBack(Label::Head) {
                m.read_back_since = None;
            }
            m.both(|p| {
                p.on(&w, OpEvent::Withdraw);
            });
        } else if roll < 78 {
            m.node_answers(&mut rng);
        } else if roll < 97 {
            // Time: to the page's next due, or a short step.
            let now = m.p.now;
            let t = match (rng.below(2), m.p.next_due()) {
                (0, Some(d)) => d.0.max(now + 1),
                _ => now + 1 + rng.below(4) as u64 * 1_000,
            };
            m.log.push(format!("tick +{}", t - now));
            // A parked GET nobody reads is ENDED at its due (engine-withdraw's due-time door: the model's ids have no
            // engine reader), so its wait leaves the book with it.
            let parked_gets: Vec<Waiting> = m.p.ops.needs().filter(|(w, e)| matches!(w, Waiting::Get(_)) && matches!(e, Entry::Parked { .. })).map(|(w, _)| w.clone()).collect();
            m.both(|p| p.tick(Ms(t)));
            // An answer is emitted when its work ends and is delivered (or lost) soon after: every op finished more
            // than 5 s ago is settled now, in order.
            while m.node.ops.iter().any(|o| o.answered.is_none() && o.done_at.saturating_add(5_000) <= m.p.now) {
                m.node_answers(&mut rng);
                m.collect(&mut rng);
            }
            for w in parked_gets {
                if !m.p.ops.contains(&w) {
                    m.waits.remove(&w);
                }
            }
            // A Sign re-asked at its bound is a new send under a fresh id; its wait is the same.
        } else {
            m.log.push("reconnect".into());
            m.reconnected = true;
            // F3: an orphaned Background op whose key is Interactive NOW skips the slot's debt (the architect).
            for o in m.node.ops.iter_mut().filter(|o| o.socket == m.node.socket && o.lane == Lane::Background && o.answered.is_none()) {
                if m.p.ops.needs().any(|(w, _)| key_of(w) == o.key && m.p.lane_of(w) == Lane::Interactive) {
                    o.exempt = true;
                }
            }
            // Each orphan holds the slot until ITS bound as the page sets it (a silent send's `at`; else its bound at
            // the RTO now): the debt a reconnect must leave (Q4), derived here from the sends, not read from the page.
            let rto_now = m.p.rto.rto_ms();
            let page_bound: BTreeMap<u32, u64> = m.p.ops.at_node().map(|(_, s, _)| (s.seq, if s.silent { s.at } else { bound_at(&s.op, s.sent_at, rto_now) })).collect();
            for o in m.node.ops.iter_mut().filter(|o| o.socket == m.node.socket && o.answered.is_none()) {
                if let Some(b) = page_bound.get(&o.seq) {
                    o.bound = *b;
                }
            }
            m.node.socket += 1;
            let now = m.p.now;
            m.both(|p| p.reconnected(Ms(now)));
            // A reconnect re-sends the GETs already out on the new socket: no NEW window place (L7 skips it).
            m.collect(&mut rng);
            m.new_window_send = false;
            m.prev_in_window = m.p.ops.in_window();
        }
        m.collect(&mut rng);
        m.check();
        if trace == Some(seed) {
            let node: Vec<String> = m.node.ops.iter().filter(|o| o.answered.is_none()).map(|o| format!("#{}{}{}", o.seq, short(&o.w), if o.lane == Lane::Background { "B" } else { "" })).collect();
            let page: Vec<String> = m.p.ops.at_node().map(|(w, s, _)| format!("#{}{}{}", s.seq, short(w), if s.silent { "s" } else { "" })).collect();
            println!("{step} {} | page {page:?} | node {node:?}", m.log.last().cloned().unwrap_or_default());
        }
    }
    let late_background = m.p.background_answers_after_bound();
    m.read_ends();
    // L1's DETECTOR bounds what (b) allowed: the node never held more extra Background ops than the page recorded sends
    // ended unanswered before a Background send of their key (the node holds at most 1 + that many).
    let (detected, extras) = (m.detected.len() as u64, m.extras_seen.len() as u64);
    if detected < extras {
        m.fail("L1 the detector bounds the overlaps", format!("{extras} extra Background ops at the node, {detected} sends recorded ended unanswered before a Background send of their key"));
    }
    Ran { found: m.found, late_background, late_delivered: m.late_delivered, bound_ends_b: detected, overlaps_seen: extras, overlaps_tagged: m.overlaps_tagged }
}

/// **SEED 39, PINNED** (the architect): an Interactive PUT's RTO duplicate answering after a Background send of the same
/// key went out -- the overlap F1 makes, COUNTED, and INERT (no invariant broken).
#[test]
fn seed_39_is_a_counted_inert_overlap() {
    let r = run(39, 1500);
    assert!(r.found.is_empty(), "seed 39 broke: {:?}", r.found);
    assert!(r.late_background > 0, "seed 39's overlap was not counted");
}

/// **SEED 17, PINNED** ((B'), the architect via core dev): an Interactive PutApp's send ENDS UNANSWERED (its RtoResend
/// last withdraw); its late answer is taken as the key's later Background send's (F1, inert, uncounted), ending it in
/// the page while the node still works it -- one extra Background op at the node, ALLOWED because the recording holds
/// an earlier unanswered end of the key, and covered by L1's detector.
#[test]
fn seed_17_an_unanswered_ends_late_answer_is_an_allowed_f1_overlap() {
    let r = run(17, 1500);
    assert!(r.found.is_empty(), "seed 17 broke: {:?}", r.found);
    assert!(r.overlaps_seen > 0, "seed 17's extra Background op was not seen: the allowance was not exercised");
    assert!(r.bound_ends_b >= r.overlaps_seen, "seed 17's overlap is not covered by the detector");
}

#[test]
fn op_life_model() {
    let seeds: u64 = std::env::var("CRAFTWORKS_MODEL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let steps = 1500;
    let mut by_inv: BTreeMap<&'static str, (u64, String)> = BTreeMap::new();
    let mut late = 0;
    let mut withheld = 0;
    let (mut bound_ends, mut overlaps, mut tagged) = (0, 0, 0);
    for seed in 1..=seeds {
        let r = run(seed, steps);
        late += r.late_background;
        withheld += r.late_delivered;
        bound_ends += r.bound_ends_b;
        overlaps += r.overlaps_seen;
        tagged += r.overlaps_tagged;
        for (inv, first) in r.found {
            let e = by_inv.entry(inv).or_insert((0, format!("seed {seed} {first}")));
            e.0 += 1;
        }
    }
    println!("OP-LIFE model (draft 3.1): {seeds} seeds x {steps} steps; sends ended unanswered before a Background send of their key (L1's detector, an upper bound): {bound_ends} covering {overlaps} extra Background ops seen at the node; background late answers observed (AnsweredAfterReask): {late}; late answers withheld from the shadow (d's reach): {withheld}");
    println!("  (iii) overlap sends whose tag the recording confirmed: {tagged}");
    // (d) is not vacuous: late answers reached it.
    assert!(withheld > 0, "no late answer was ever delivered: (d) checked nothing");
    for (inv, (n, first)) in &by_inv {
        println!("  {inv}: {n} seeds\n    first: {first}");
    }
    assert!(by_inv.is_empty(), "OP-LIFE invariants violated: {:?}", by_inv.keys().collect::<Vec<_>>());
    assert!(tagged > 0, "(iii) compared no overlap send: the tag check checked nothing");
}
