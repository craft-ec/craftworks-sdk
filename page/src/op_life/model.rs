//! OP-LIFE.md's GATE (craftworks-docs/docs/design/OP-LIFE.md): seeded random event sequences over the REAL page, with
//! the invariants checked after every step and every key's need compared with a REFERENCE -- a small pure model that
//! IS the document's tables. The page is read and driven only through `op_life_adapter`, the one per-tree piece, so
//! the same model runs on the known-broken trees (06ec9d6, c3a298a, 138b0e4) and must go RED on each.
//!
//! What each invariant is checked against, independently of the page's own record:
//! - L1 against a NODE-SIDE LEDGER the driver keeps: every send it took, released only by the node's answer (the
//!   oldest answerable send of its key, F61) or by the deadline the page last gave it. A tree that forgets a send the
//!   node still holds is caught here.
//! - L4 against the page's recording: one end per send ordinal.
//! - L5 against a BLACK-HOLE key the node never answers: every other queued background need must reach the wire; and
//!   a fault-free TAIL after the random steps, after which nothing is held or stale.
// A TEST driver: its matches over the pool are not the record's (op_life's lints are for production).
#![allow(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
use crate::op_life::adapter as a;
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

/// A need, in the document's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum N {
    Absent,
    QI,
    QB,
    WI,
    WB,
    PI,
    PB,
}

impl N {
    fn lane(self) -> Option<Lane> {
        match self {
            N::Absent => None,
            N::QI | N::WI | N::PI => Some(Lane::Interactive),
            N::QB | N::WB | N::PB => Some(Lane::Background),
        }
    }
}

/// What a deadline belongs to, in the order a tick applies them: the key's `rank`-th oldest stale send, the held send,
/// a need.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Due {
    Stale(usize),
    Held,
    Need,
}

// ---------------------------------------------------------------- the reference: OP-LIFE.md's tables

/// A reference need; an on-wire one carries its send's place in the reference's own send order (C1 compares ages).
#[derive(Clone, Debug)]
enum RN {
    QI { att: u32, order: u64 },
    QB { att: u32, order: u64 },
    WI { seq: u64 },
    WB { seq: u64 },
    PI,
    PB,
}

impl RN {
    fn n(&self) -> N {
        match self {
            RN::QI { .. } => N::QI,
            RN::QB { .. } => N::QB,
            RN::WI { .. } => N::WI,
            RN::WB { .. } => N::WB,
            RN::PI => N::PI,
            RN::PB => N::PB,
        }
    }

    fn seq(&self) -> Option<u64> {
        match self {
            RN::WI { seq } | RN::WB { seq } => Some(*seq),
            RN::QI { .. } | RN::QB { .. } | RN::PI | RN::PB => None,
        }
    }
}

#[derive(Default)]
struct Reference {
    need: BTreeMap<Waiting, RN>,
    /// The page-level held send: its key, whether its answer can still come, its send's age.
    held: Option<(Waiting, bool, u64)>,
    /// Stale interactive sends per key: their ages, oldest first.
    stale: BTreeMap<Waiting, Vec<u64>>,
    order: u64,
    seq: u64,
    /// A NAMED key's (the Sign's) current payload: the request id its next send carries.
    payload: BTreeMap<Waiting, u32>,
    /// Each named send's request id, by the reference's send order.
    ids: BTreeMap<u64, u32>,
    /// Each send's attempt number, by the reference's send order.
    att_of: BTreeMap<u64, u32>,
    /// N2: per recurring key, the sends the page stopped counting while they could still answer, by send order.
    untracked: BTreeMap<Waiting, Vec<u64>>,
}

fn is_get(w: &Waiting) -> bool {
    matches!(w, Waiting::Get(_))
}

/// Content-addressed: the same key is the same bytes (a GET, a PUT). The Update key is not.
fn same_bytes(w: &Waiting) -> bool {
    matches!(w, Waiting::Get(_) | Waiting::Put(_))
}

impl Reference {
    /// N2: a send stops being counted while it can still answer.
    fn untrack(&mut self, w: &Waiting, seq: u64) {
        if op_life::recurs(w) {
            let v = self.untracked.entry(w.clone()).or_default();
            let at = v.partition_point(|s| *s < seq);
            v.insert(at, seq);
        }
    }

    fn remove_untracked(&mut self, w: &Waiting, seq: u64) {
        if let Some(v) = self.untracked.get_mut(w) {
            v.retain(|s| *s != seq);
            if v.is_empty() {
                self.untracked.remove(w);
            }
        }
    }

    /// N2: the need's own send on the wire becomes an ORPHAN when a lost send's late answer serves the need.
    fn orphan_the_needs_send(&mut self, w: &Waiting) {
        match self.need.remove(w) {
            Some(RN::WB { seq }) => self.held = Some((w.clone(), true, seq)),
            // An interactive need simply TAKES it; its own send, still at the node, is REMEMBERED (untracked).
            Some(RN::WI { seq }) => self.untrack(w, seq),
            other => {
                if let Some(n) = other {
                    self.need.insert(w.clone(), n);
                }
            }
        }
    }

    fn next_order(&mut self) -> u64 {
        self.order += 1;
        self.order
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// A NEW send of `w`: its place in the send order, with the request id it carries if `w` is named.
    fn new_send(&mut self, w: &Waiting, att: u32) -> u64 {
        let seq = self.next_seq();
        self.att_of.insert(seq, att);
        if let Some(id) = self.payload.get(w) {
            self.ids.insert(seq, *id);
        }
        seq
    }

    /// A NAMED answer (the architect's row): among the key's sends carrying the id, the OLDEST (F61) -- a lost one's
    /// serves the need (N2), the need's own answers it, the held or a stale one is released -- or nothing (dropped).
    fn answer_named(&mut self, w: &Waiting, id: u32, window: usize) -> bool {
        let carries = |r: &Reference, seq: u64| r.ids.get(&seq) == Some(&id);
        let need_seq = self
            .need
            .get(w)
            .and_then(RN::seq)
            .filter(|s| carries(self, *s));
        let lost = self
            .untracked
            .get(w)
            .and_then(|v| v.iter().copied().find(|s| carries(self, *s)));
        if let Some(l) = lost.filter(|l| need_seq.is_none_or(|n| *l < n)) {
            self.remove_untracked(w, l);
            if need_seq.is_some() {
                self.orphan_the_needs_send(w);
                self.place_frees(window);
                return true;
            }
            return false;
        }
        if need_seq.is_some() {
            self.need.remove(w);
            self.place_frees(window);
            return true;
        }
        if self
            .held
            .as_ref()
            .is_some_and(|(h, _, s)| h == w && carries(self, *s))
        {
            self.held = None;
            self.place_frees(window);
            return false;
        }
        if let Some(i) = self
            .stale
            .get(w)
            .and_then(|v| v.iter().position(|s| carries(self, *s)))
        {
            let v = self.stale.get_mut(w).expect("stale");
            v.remove(i);
            if v.is_empty() {
                self.stale.remove(w);
            }
        }
        false
    }

    fn slot_held(&self) -> bool {
        self.held.is_some() || self.need.values().any(|n| matches!(n, RN::WB { .. }))
    }

    fn window_gets(&self) -> usize {
        self.need
            .iter()
            .filter(|(w, n)| is_get(w) && matches!(n, RN::WI { .. }))
            .count()
    }

    /// A send becomes STALE: kept in AGE order (its place in the send order), oldest first -- the order C1, adoption
    /// and the deadline ranks read.
    fn add_stale(&mut self, w: &Waiting, seq: u64) {
        let v = self.stale.entry(w.clone()).or_default();
        let at = v.partition_point(|s| *s < seq);
        v.insert(at, seq);
    }

    /// The oldest stale send of `w` an identical need may ADOPT, removed.
    fn take_stale(&mut self, w: &Waiting) -> Option<u64> {
        if !same_bytes(w) {
            return None;
        }
        let v = self.stale.get_mut(w)?;
        let seq = v.remove(0);
        if v.is_empty() {
            self.stale.remove(w);
        }
        Some(seq)
    }

    /// The need goes on the wire as its `att`-th send -- or ADOPTS an identical stale send.
    fn on_wire(&mut self, w: &Waiting, lane: Lane, att: u32) {
        let seq = match self.take_stale(w) {
            Some(seq) => seq,
            None => self.new_send(w, att),
        };
        self.need.insert(
            w.clone(),
            if lane == Lane::Interactive {
                RN::WI { seq }
            } else {
                RN::WB { seq }
            },
        );
    }

    /// PLACE-FREES, after every event (L2): the slot to the oldest queued background need (a successor has no
    /// priority: round-robin); the window's queue in order.
    fn place_frees(&mut self, window: usize) {
        if !self.slot_held() {
            let pick = self
                .need
                .iter()
                .filter_map(|(w, n)| {
                    if let RN::QB { order, att } = n {
                        Some((*order, w.clone(), *att))
                    } else {
                        None
                    }
                })
                .min();
            if let Some((_, w, att)) = pick {
                self.on_wire(&w, Lane::Background, att + 1);
            }
        }
        while self.window_gets() < window {
            let pick = self
                .need
                .iter()
                .filter_map(|(w, n)| {
                    if let RN::QI { order, att } = n {
                        Some((*order, w.clone(), *att))
                    } else {
                        None
                    }
                })
                .min();
            let Some((_, w, att)) = pick else { break };
            self.on_wire(&w, Lane::Interactive, att + 1);
        }
    }

    /// A need sent as from Absent (adoption of a stale send included).
    fn send_new(&mut self, w: &Waiting, lane: Lane, window: usize) {
        self.send_new_after(w, lane, window, 0);
    }

    /// A need sent as from Absent after `att` earlier sends (adoption of a stale send included).
    fn send_new_after(&mut self, w: &Waiting, lane: Lane, window: usize, att: u32) {
        match lane {
            Lane::Interactive => {
                let adopt = same_bytes(w) && self.stale.contains_key(w);
                if is_get(w) && self.window_gets() >= window && !adopt {
                    let order = self.next_order();
                    self.need.insert(w.clone(), RN::QI { att, order });
                } else {
                    self.on_wire(w, lane, att + 1);
                }
            }
            Lane::Background => {
                if self.slot_held() {
                    let order = self.next_order();
                    self.need.insert(w.clone(), RN::QB { att, order });
                } else {
                    self.on_wire(w, lane, att + 1);
                }
            }
        }
    }

    fn send(&mut self, w: &Waiting, lane: Lane, window: usize) {
        let identical = same_bytes(w);
        match (self.need.get(w).cloned(), lane) {
            (None, _) => {
                if identical && self.held.as_ref().is_some_and(|(h, ans, _)| h == w && *ans) {
                    // ADOPTED: the need rides the held send.
                    let (_, _, seq) = self.held.take().expect("held");
                    self.need.insert(
                        w.clone(),
                        if lane == Lane::Interactive {
                            RN::WI { seq }
                        } else {
                            RN::WB { seq }
                        },
                    );
                } else {
                    self.send_new(w, lane, window);
                }
            }
            (Some(RN::QI { .. }), _) => {}
            (Some(RN::QB { att, .. }), Lane::Interactive) => {
                self.need.remove(w);
                self.send_new_after(w, Lane::Interactive, window, att);
            }
            (Some(RN::QB { .. }), Lane::Background) => {}
            (Some(RN::WI { seq }), _) => {
                // A GET joins; any other op supersedes, and the old send is STALE.
                if !is_get(w) {
                    self.add_stale(w, seq);
                    let seq = self.new_send(w, 1);
                    self.need.insert(w.clone(), RN::WI { seq });
                }
            }
            (Some(RN::WB { seq }), Lane::Interactive) => {
                if identical {
                    self.need.insert(w.clone(), RN::WI { seq });
                } else {
                    self.held = Some((w.clone(), true, seq));
                    self.need.remove(w);
                    self.send_new(w, Lane::Interactive, window);
                }
            }
            (Some(RN::WB { seq }), Lane::Background) => {
                if !identical {
                    self.held = Some((w.clone(), true, seq));
                    let order = self.next_order();
                    self.need.insert(w.clone(), RN::QB { att: 0, order });
                }
            }
            (Some(RN::PI), _) => {}
            (Some(RN::PB), Lane::Interactive) => {
                self.need.insert(w.clone(), RN::PI);
            }
            (Some(RN::PB), Lane::Background) => {}
        }
        self.place_frees(window);
    }

    fn join(&mut self, w: &Waiting, window: usize) {
        match self.need.get(w).cloned() {
            Some(RN::QB { att, .. }) => {
                self.need.remove(w);
                self.send_new_after(w, Lane::Interactive, window, att);
            }
            Some(RN::WB { seq }) => {
                self.need.insert(w.clone(), RN::WI { seq });
            }
            Some(RN::PB) => {
                self.need.insert(w.clone(), RN::PI);
            }
            _ => {}
        }
        self.place_frees(window);
    }

    /// C1: the answer belongs to the key's OLDEST send the node may still answer. `Some(true)`: an orphan (the held
    /// send, a stale one) took it; `Some(false)`: a LOST (untracked) send's -- N2, it serves the need; `None`: the
    /// need's cell decides.
    fn c1(&mut self, w: &Waiting, window: usize) -> Option<bool> {
        let need_seq = self.need.get(w).and_then(RN::seq);
        let held_seq = self
            .held
            .as_ref()
            .filter(|(h, ans, _)| h == w && *ans)
            .map(|(_, _, s)| *s);
        let stale_seq = self.stale.get(w).and_then(|v| v.first().copied());
        let lost_seq = self.untracked.get(w).and_then(|v| v.first().copied());
        let oldest = [need_seq, held_seq, stale_seq, lost_seq]
            .into_iter()
            .flatten()
            .min();
        if oldest.is_some() && oldest == held_seq {
            self.held = None;
            self.place_frees(window);
            return Some(true);
        }
        if oldest.is_some() && oldest == stale_seq {
            let v = self.stale.get_mut(w).expect("stale");
            v.remove(0);
            if v.is_empty() {
                self.stale.remove(w);
            }
            return Some(true);
        }
        if let Some(l) = lost_seq.filter(|_| oldest == lost_seq) {
            self.remove_untracked(w, l);
            return Some(false);
        }
        None
    }

    /// Returns whether the NEED was served (its waiters leave).
    fn answer(&mut self, w: &Waiting, window: usize) -> bool {
        let c1 = self.c1(w, window);
        if c1 == Some(true) {
            return false;
        }
        let late = c1 == Some(false);
        let served = match self.need.get(w).cloned() {
            None => false,
            Some(RN::QB { att: 0, .. }) | Some(RN::QI { att: 0, .. }) => false,
            // N2: a lost send's late answer serves the need; its own send on the wire is an orphan (held if background,
            // untracked if interactive).
            Some(RN::WB { .. }) | Some(RN::WI { .. }) if late => {
                self.orphan_the_needs_send(w);
                true
            }
            Some(_) => {
                self.need.remove(w);
                true
            }
        };
        self.place_frees(window);
        served
    }

    fn answer_without(&mut self, w: &Waiting, window: usize) {
        let c1 = self.c1(w, window);
        if c1 == Some(true) {
            return;
        }
        let late = c1 == Some(false);
        if late && matches!(self.need.get(w), Some(RN::WB { .. }) | Some(RN::WI { .. })) {
            let bg = matches!(self.need.get(w), Some(RN::WB { .. }));
            self.orphan_the_needs_send(w);
            self.need
                .insert(w.clone(), if bg { RN::PB } else { RN::PI });
            self.place_frees(window);
            return;
        }
        match self.need.get(w).cloned() {
            Some(RN::QI { att, .. }) if att >= 1 => {
                self.need.insert(w.clone(), RN::PI);
            }
            Some(RN::WI { .. }) | Some(RN::PI) => {
                self.need.insert(w.clone(), RN::PI);
            }
            Some(RN::QB { att, .. }) if att >= 1 => {
                self.need.insert(w.clone(), RN::PB);
            }
            Some(RN::WB { .. }) => {
                self.need.insert(w.clone(), RN::PB);
            }
            Some(RN::PB) => {
                self.need.insert(w.clone(), RN::PB);
            }
            _ => {}
        }
        self.place_frees(window);
    }

    fn withdraw(&mut self, w: &Waiting, window: usize) {
        match self.need.remove(w) {
            Some(RN::WB { seq }) => self.held = Some((w.clone(), true, seq)),
            Some(RN::WI { seq }) => self.add_stale(w, seq),
            _ => {}
        }
        self.place_frees(window);
    }

    /// A NEED's deadline. `silent`: an on-wire GET whose node GET is not over.
    fn deadline(&mut self, w: &Waiting, silent: bool, window: usize) {
        match self.need.get(w).cloned() {
            Some(RN::WI { .. }) if silent => {}
            Some(RN::WI { seq }) if is_get(w) => {
                self.untrack(w, seq);
                let order = self.next_order();
                let att = self.att_of[&seq];
                self.need.insert(w.clone(), RN::QI { att, order });
            }
            // Re-sent by its owner: a send of the need, so it adopts an identical stale send.
            Some(RN::WI { seq }) => {
                self.untrack(w, seq);
                self.need.remove(w);
                let att = self.att_of[&seq];
                self.on_wire(w, Lane::Interactive, att + 1);
            }
            Some(RN::WB { .. }) if silent => {}
            Some(RN::WB { seq }) => {
                self.untrack(w, seq);
                let order = self.next_order();
                let att = self.att_of[&seq];
                self.need.insert(w.clone(), RN::QB { att, order });
            }
            // A parked GET nobody wants (the engine awaits no model id): engine-withdraw.
            Some(RN::PI) | Some(RN::PB) if is_get(w) => {
                self.need.remove(w);
            }
            Some(RN::PI) => {
                self.need.remove(w);
                self.send_new_after(w, Lane::Interactive, window, 1);
            }
            Some(RN::PB) => {
                self.need.remove(w);
                self.send_new_after(w, Lane::Background, window, 1);
            }
            _ => {}
        }
        self.place_frees(window);
    }

    fn held_deadline(&mut self, w: &Waiting, window: usize) {
        if let Some((h, ans, seq)) = self.held.clone().filter(|(h, ..)| h == w) {
            self.held = None;
            if ans {
                self.untrack(&h, seq);
            }
        }
        self.place_frees(window);
    }

    fn stale_deadline(&mut self, w: &Waiting, rank: usize) {
        if let Some(v) = self.stale.get_mut(w) {
            if rank < v.len() {
                let seq = v.remove(rank);
                if v.is_empty() {
                    self.stale.remove(w);
                }
                self.untrack(w, seq);
                return;
            }
            if v.is_empty() {
                self.stale.remove(w);
            }
        }
    }

    fn reconnect(&mut self, window: usize) {
        self.stale.clear();
        self.untracked.clear();
        if let Some((_, ans, _)) = self.held.as_mut() {
            *ans = false;
        }
        let on_wire: Vec<(Waiting, RN)> = self
            .need
            .iter()
            .filter(|(_, n)| n.seq().is_some())
            .map(|(w, n)| (w.clone(), n.clone()))
            .collect();
        for (w, n) in on_wire {
            match n {
                RN::WI { seq } => {
                    // Re-sent at once on the new socket, its attempt not counted up (DEADLINES).
                    let att = self.att_of[&seq];
                    let seq = self.new_send(&w, att);
                    self.need.insert(w, RN::WI { seq });
                }
                RN::WB { seq } => {
                    self.held = Some((w.clone(), false, seq));
                    let order = self.next_order();
                    let att = self.att_of[&seq];
                    self.need.insert(w, RN::QB { att, order });
                }
                _ => {}
            }
        }
        self.place_frees(window);
    }
}

// ---------------------------------------------------------------- the node-side ledger (L1's witness)

struct NodeSend {
    w: Waiting,
    /// The request id the send carries (a named key's), if any.
    request: Option<u32>,
    /// When the node took it.
    sent: u64,
    seq: u32,
    due: u64,
    lane: Lane,
    answerable: bool,
    /// The page still counts it (else: LOST to the page at its deadline, which the node may still answer, late).
    counted: bool,
}

/// THE NODE's side: every send it holds for this client, answered in ONE sequence (F61) -- a key's oldest first,
/// whether the page still counts it or not. A send the page gives up on at its deadline is KEPT by the node (answered
/// late, in order) or DROPPED (never answered), decided per send.
#[derive(Default)]
struct Ledger {
    sends: Vec<NodeSend>,
    /// Lost non-GET sends the node DROPPED, per key: never answered, so the page's late-answer memory (N2) of them
    /// stays until the cap evicts it.
    dropped: BTreeMap<Waiting, usize>,
    /// Does this node drop lost sends at all (odd seeds), or answer every one late (even seeds)?
    drops: bool,
}

impl Ledger {
    /// Follow the page's view of each send it still counts (by ordinal): its due and its lane. A send the page stopped
    /// counting keeps the last due it was given.
    fn follow(&mut self, page_sends: &[(Waiting, u32, u64, Lane)]) {
        for h in self.sends.iter_mut().filter(|h| h.counted) {
            if let Some((_, _, at, lane)) = page_sends
                .iter()
                .find(|(w, s, _, _)| *w == h.w && *s == h.seq)
            {
                h.due = *at;
                h.lane = *lane;
            }
        }
    }

    /// Time `now`: a counted send past the page's deadline is LOST to the page -- the node keeps it (answered late)
    /// or drops it; an unanswerable one is simply gone. A lost GET's node GET is over after its bound.
    fn release_due(&mut self, now: u64) {
        let mut dropped: Vec<Waiting> = Vec::new();
        self.sends.retain_mut(|h| {
            if h.counted && now >= h.due {
                h.counted = false;
                // Kept or dropped, per send (deterministic): the node answers two thirds of its lost sends, late.
                let kept = h.answerable
                    && (!self.drops || (u64::from(h.seq).wrapping_mul(0x9E37_79B9) >> 7) % 3 != 0);
                if !kept && h.answerable && !is_get(&h.w) {
                    dropped.push(h.w.clone());
                }
                return kept;
            }
            h.counted || !is_get(&h.w) || now < h.sent + GET_OVER_MS
        });
        for w in dropped {
            *self.dropped.entry(w).or_default() += 1;
        }
    }

    fn answerable(&self, w: &Waiting) -> bool {
        self.sends.iter().any(|h| h.w == *w && h.answerable)
    }

    /// The node answers `w`: its OLDEST answerable send (F61: one sequence per client) -- with the request id that
    /// send carries, if it carries one.
    fn answer(&mut self, w: &Waiting) -> Option<u32> {
        let i = self
            .sends
            .iter()
            .enumerate()
            .filter(|(_, h)| h.w == *w && h.answerable)
            .min_by_key(|(_, h)| h.seq)
            .map(|(i, _)| i)?;
        self.sends.remove(i).request
    }

    /// Background sends the PAGE still counts (L1's own terms: a lost send is no longer counted).
    fn background(&self) -> usize {
        self.sends
            .iter()
            .filter(|h| h.counted && h.lane == Lane::Background)
            .count()
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

fn key_of(op: &Op) -> Option<Waiting> {
    match op {
        Op::Get { id } => Some(Waiting::Get(*id)),
        Op::Put { id, .. } => Some(Waiting::Put(*id)),
        Op::Update { label, .. } => Some(Waiting::Update(label.clone())),
        Op::Sign { label, .. } => Some(Waiting::Sign(label.clone())),
        _ => None,
    }
}

fn pool() -> Vec<Waiting> {
    let mut keys: Vec<Waiting> = (0..4u8).map(|i| Waiting::Get([0x10 + i; 32])).collect();
    keys.extend((0..3u8).map(|i| Waiting::Put([0x20 + i; 32])));
    keys.push(Waiting::Update(Label::Site("model".into())));
    // A NAMED key: its answers name their send by the request id (SG02).
    keys.push(Waiting::Sign(Label::Site("model".into())));
    keys
}

fn key_short(w: &Waiting) -> String {
    match w {
        Waiting::Get(id) => format!("G{:x}", id[0]),
        Waiting::Put(id) => format!("P{:x}", id[0]),
        Waiting::Update(_) => "U".into(),
        Waiting::Sign(_) => "S".into(),
        other => format!("{other:?}"),
    }
}

/// The key the node NEVER answers (L5's witness).
fn black_hole() -> Waiting {
    Waiting::Put([0x22; 32])
}

/// The longest one send can hold the slot: a re-send's back-off (at most RTO_MAX), or a silent GET's node bound plus
/// one RTO.
const HOLD_MS: u64 = rto::NODE_GET_BOUND_MS as u64 + rto::RTO_MAX_MS as u64;

/// When a node GET is certainly over after its send: its bound plus the RTO's ceiling (the page's decay of an untracked
/// GET, `node_get_over_at(sent, RTO_MAX)`).
const GET_OVER_MS: u64 = rto::NODE_GET_BOUND_MS as u64 + rto::RTO_MAX_MS as u64;

struct Run {
    p: Page,
    r: Reference,
    ledger: Ledger,
    waiters: BTreeMap<Waiting, (bool, bool)>,
    found: BTreeMap<&'static str, String>,
    /// Each queued background need: when it queued, and how many holds of the slot can come before its turn (the
    /// needs ahead of it then, and the slot's occupant) -- round-robin gives each at most one.
    queued_since: BTreeMap<Waiting, (u64, u64)>,
    last_lane: BTreeMap<Waiting, Lane>,
    log: Vec<String>,
    seed: u64,
}

impl Run {
    fn hook(&mut self, w: &Waiting) {
        if self.waiters.get(w).is_some_and(|(i, b)| !*i && *b) {
            self.p.test_background.insert(w.clone());
        } else {
            self.p.test_background.remove(w);
        }
    }

    fn fail(&mut self, step: usize, inv: &'static str, why: String) {
        let last = self.log[self.log.len().saturating_sub(10)..].join(" | ");
        let seed = self.seed;
        self.found
            .entry(inv)
            .or_insert_with(|| format!("seed {seed} step {step}: {why}\n    last steps: {last}"));
    }

    /// TIME: to `t`, the deadlines due there applied to the reference in the document's order (the stale sends, the
    /// held send, then the needs in key order).
    fn tick_to(&mut self, t: u64) {
        let dues = a::dues(&self.p);
        let mut due: Vec<(Due, Waiting, bool)> = dues
            .iter()
            .filter(|(_, at, _)| *at <= t)
            .map(|(w, _, d)| (*d, w.clone(), a::silent_at(&self.p, w, t)))
            .collect();
        // Stale sends of one key expire highest rank first, so the ranks read before the tick stay true.
        due.sort_by(|(d1, w1, _), (d2, w2, _)| match (d1, d2) {
            (Due::Stale(r1), Due::Stale(r2)) => w1.cmp(w2).then(r2.cmp(r1)),
            _ => d1.cmp(d2).then(w1.cmp(w2)),
        });
        a::tick(&mut self.p, t);
        self.ledger.release_due(t);
        // THE EARLY LEAVE ENDS NOTHING (the architect): an N2 orphan that left early by its bound has NO End recorded.
        let early: Vec<u32> = self.p.early_left_seqs().to_vec();
        if !early.is_empty() {
            use instrument::{Dir, Event as IE, Record};
            let ended: Vec<u32> = self
                .p
                .recording()
                .map(|r| {
                    r.events()
                        .into_iter()
                        .filter_map(|e| match e {
                            IE::Edge {
                                dir: Dir::Response,
                                id,
                                ..
                            } => Some(id.ordinal()),
                            IE::Exit { op, .. } => (1..=self.p.sends).find(|n| {
                                instrument::Label::new(instrument::Kind::Request, *n)
                                    .is_some_and(|l| l.op() == op)
                            }),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Some(seq) = early.iter().find(|s| ended.contains(s)) {
                let step = self.log.len();
                self.fail(
                    step,
                    "N2 an early leave ends nothing",
                    format!("send #{seq} left early and has an End recorded"),
                );
            }
        }
        for (d, w, silent) in due {
            let window = a::window(&self.p);
            match d {
                Due::Held => self.r.held_deadline(&w, window),
                Due::Stale(rank) => self.r.stale_deadline(&w, rank),
                Due::Need => self.r.deadline(&w, silent, window),
            }
            // A parked GET the engine does not want is withdrawn by it (the model's ids are wanted by nobody).
            if is_get(&w) && a::need(&self.p, &w) == N::Absent && !self.r.need.contains_key(&w) {
                self.waiters.remove(&w);
                self.hook(&w);
            }
        }
        // N2's GET entries DECAY with time (the node's GET is over): read from the page, the one timed input.
        let page_untracked = a::untracked(&self.p);
        for (k, v) in self.r.untracked.iter_mut() {
            if is_get(k) {
                // The oldest decay first.
                let keep = page_untracked.get(k).copied().unwrap_or(0);
                while v.len() > keep {
                    v.remove(0);
                }
            }
        }
        self.r.untracked.retain(|_, v| !v.is_empty());
    }

    /// The node answers the oldest answerable send of `w` -- NAMED when that send carries a request id.
    fn answer(&mut self, w: &Waiting) {
        let request = self.ledger.answer(w);
        let window = a::window(&self.p);
        let served = match request {
            Some(id) => {
                a::answer_named(&mut self.p, w, id);
                let window = a::window(&self.p);
                self.r.answer_named(w, id, window)
            }
            None => {
                a::answer(&mut self.p, w);
                let window = a::window(&self.p);
                self.r.answer(w, window)
            }
        };
        let _ = window;
        if served {
            self.waiters.remove(w);
            self.hook(w);
        }
    }

    /// After a step: the sends it made, then every invariant and the reference.
    fn check(&mut self, step: usize, keys: &[Waiting]) {
        let page_sends = a::sends(&self.p);
        let mut sent_now: BTreeSet<Waiting> = BTreeSet::new();
        for op in self.p.take_ops() {
            let Some(k) = key_of(&op) else { continue };
            if !keys.contains(&k) {
                continue;
            }
            sent_now.insert(k.clone());
            let Some((_, seq, at, lane)) = page_sends
                .iter()
                .filter(|(w, ..)| *w == k)
                .max_by_key(|(_, s, ..)| *s)
                .cloned()
            else {
                continue;
            };
            let request = op_life::request_of(&op);
            let sent = a::now(&self.p);
            self.ledger.sends.push(NodeSend {
                w: k,
                request,
                sent,
                seq,
                due: at,
                lane,
                answerable: true,
                counted: true,
            });
        }
        self.ledger.follow(&page_sends);
        let now = a::now(&self.p);
        // L1: one background send at the node.
        if self.ledger.background() > 1 {
            let bg: Vec<String> = self
                .ledger
                .sends
                .iter()
                .filter(|h| h.counted && h.lane == Lane::Background)
                .map(|h| {
                    format!(
                        "{}#{} due {} ans {}",
                        key_short(&h.w),
                        h.seq,
                        h.due,
                        h.answerable
                    )
                })
                .collect();
            self.fail(
                step,
                "L1 one background send at the node",
                format!(
                    "{} background sends held: {bg:?}; now {now}",
                    self.ledger.background()
                ),
            );
        }
        let needs: BTreeMap<Waiting, N> = keys
            .iter()
            .map(|k| (k.clone(), a::need(&self.p, k)))
            .collect();
        let held = a::held(&self.p);
        if held.is_some() && needs.values().any(|n| *n == N::WB) {
            self.fail(
                step,
                "L1 impossible pair (held + OnWire·B)",
                format!("held {held:?} beside an OnWire·B need"),
            );
        }
        // L2: work-conserving.
        let slot_free = held.is_none() && !needs.values().any(|n| *n == N::WB);
        if slot_free && needs.values().any(|n| *n == N::QB) {
            self.fail(
                step,
                "L2 work-conserving",
                "the slot is free and a background need waits".into(),
            );
        }
        let waiters = self.waiters.clone();
        for (k, (i, b)) in &waiters {
            // L3: an app never waits behind background work.
            if *i && matches!(needs[k], N::QB | N::WB | N::PB) {
                self.fail(
                    step,
                    "L3 app behind background",
                    format!(
                        "{} has an interactive waiter and is {:?}",
                        key_short(k),
                        needs[k]
                    ),
                );
            }
            // L0: a need with a waiter keeps a record (GETs: the engine may withdraw a block it does not want).
            if (*i || *b) && !is_get(k) && needs[k] == N::Absent {
                self.fail(
                    step,
                    "L0 a need with a waiter is lost",
                    format!("{} has a waiter and no record", key_short(k)),
                );
            }
        }
        // L4: one end per send.
        if let Some(rec) = self.p.recording() {
            use instrument::{Dir, Event as IE, Record};
            let mut ends: BTreeMap<instrument::OpId, u32> = BTreeMap::new();
            for e in rec.events() {
                match e {
                    IE::Edge {
                        dir: Dir::Response,
                        id,
                        ..
                    } => *ends.entry(id.op()).or_default() += 1,
                    IE::Exit { op, .. } => *ends.entry(op).or_default() += 1,
                    _ => {}
                }
            }
            if let Some((op, n)) = ends.iter().find(|(_, n)| **n > 1) {
                let why = format!("{op:?} recorded {n} ends");
                self.fail(step, "L4 one end per send", why);
            }
        }
        // L5: a queued background need reaches the wire within its bound. A need that went out this step (and yielded
        // again inside it) waited no longer than this step.
        let qb: BTreeSet<Waiting> = needs
            .iter()
            .filter(|(_, n)| **n == N::QB)
            .map(|(k, _)| k.clone())
            .collect();
        self.queued_since
            .retain(|k, _| qb.contains(k) && !sent_now.contains(k));
        let occupied = u64::from(!slot_free);
        for k in &qb {
            let ahead = self.queued_since.len() as u64;
            let (since, turns) = *self
                .queued_since
                .entry(k.clone())
                .or_insert((now, ahead + occupied));
            // The bound is the WAIT's, fixed when it queued: the queue can shorten later without shortening it.
            let bound = (turns + 1) * HOLD_MS;
            if now.saturating_sub(since) > bound {
                self.fail(
                    step,
                    "L5 no need waits for ever",
                    format!(
                        "{} queued for {} ms (bound {bound}: {turns} holds ahead when it queued)",
                        key_short(k),
                        now - since
                    ),
                );
            }
        }
        // L6: never demoted.
        for (k, n) in &needs {
            match n.lane() {
                Some(l) => {
                    if self.last_lane.get(k) == Some(&Lane::Interactive) && l == Lane::Background {
                        self.fail(
                            step,
                            "L6 never demoted",
                            format!("{} went Interactive → Background ({n:?})", key_short(k)),
                        );
                    }
                    self.last_lane.insert(k.clone(), l);
                }
                None => {
                    self.last_lane.remove(k);
                }
            }
        }
        // THE REFERENCE: every key's need, the held key, and the stale sends per key.
        for (k, n) in &needs {
            let want = self.r.need.get(k).map_or(N::Absent, RN::n);
            if *n != want {
                self.fail(
                    step,
                    "REF the tables",
                    format!(
                        "{}: the page has {n:?}, the tables say {want:?}",
                        key_short(k)
                    ),
                );
            }
        }
        let want_held = self.r.held.as_ref().map(|(h, ..)| h.clone());
        if held != want_held {
            self.fail(
                step,
                "REF the tables (held)",
                format!("the page holds {held:?}, the tables say {want_held:?}"),
            );
        }
        let untracked = a::untracked(&self.p);
        let want_untracked: BTreeMap<Waiting, usize> = self
            .r
            .untracked
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect();
        if untracked != want_untracked {
            let show = |m: &BTreeMap<Waiting, usize>| {
                m.iter()
                    .map(|(k, n)| format!("{}×{n}", key_short(k)))
                    .collect::<Vec<_>>()
            };
            self.fail(
                step,
                "REF the tables (untracked)",
                format!(
                    "the page has untracked {:?}, the tables say {:?}",
                    show(&untracked),
                    show(&want_untracked)
                ),
            );
        }
        let stale = a::stale(&self.p);
        let want_stale: BTreeMap<Waiting, usize> = self
            .r
            .stale
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect();
        if stale != want_stale {
            let show = |m: &BTreeMap<Waiting, usize>| {
                m.iter()
                    .map(|(k, n)| format!("{}×{n}", key_short(k)))
                    .collect::<Vec<_>>()
            };
            self.fail(
                step,
                "REF the tables (stale)",
                format!(
                    "the page has stale {:?}, the tables say {:?}",
                    show(&stale),
                    show(&want_stale)
                ),
            );
        }
    }
}

/// One seed's run: the violations it found, by invariant (the first of each, with its step).
/// A seed's run: the invariants it broke (each with its first failure), and N2's memory -- the most keys it held at
/// once, the keys evicted at the cap, and whether this seed's node drops lost sends.
struct Ran {
    found: BTreeMap<&'static str, String>,
    peak: usize,
    evicted: u64,
    drops: bool,
}

fn run(seed: u64, steps: usize) -> Ran {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut p = Page::new_at(Params::default(), PutPath::Page, Ms(1_790_253_181_367));
    p.answered(&Waiting::RecoverHead);
    let _ = p.take_ops();
    p.record_into(1 << 16);
    let keys = pool();
    let ledger = Ledger {
        drops: seed % 2 == 1,
        ..Ledger::default()
    };
    let mut m = Run {
        p,
        r: Reference::default(),
        ledger,
        waiters: BTreeMap::new(),
        found: BTreeMap::new(),
        queued_since: BTreeMap::new(),
        last_lane: BTreeMap::new(),
        log: Vec::new(),
        seed,
    };
    let mut upd = 0u8;
    let mut req = 0u32;
    for step in 0..steps {
        let w = keys[rng.below(keys.len())].clone();
        let ks = key_short(&w);
        let op = match &w {
            Waiting::Get(id) => Op::Get { id: *id },
            Waiting::Put(id) => Op::Put {
                id: *id,
                bytes: vec![id[0]],
            },
            Waiting::Update(label) => {
                upd = upd.wrapping_add(1);
                Op::Update {
                    label: label.clone(),
                    state: vec![upd],
                }
            }
            Waiting::Sign(label) => {
                req += 1;
                Op::Sign {
                    id: req,
                    prev_seq: 0,
                    prev_root: [0; 32],
                    seq: 1,
                    root: [0; 32],
                    ledger: Vec::new(),
                    label: label.clone(),
                }
            }
            _ => unreachable!("the pool"),
        };
        // A send's payload is the named key's newest request id (the reference's record of what its sends carry).
        let sends_payload = |m: &mut Run, w: &Waiting, op: &Op| {
            if let Some(id) = op_life::request_of(op) {
                m.r.payload.insert(w.clone(), id);
            }
        };
        let roll = rng.below(100);
        let what: String;
        // The black hole is BACKGROUND work only: it takes the slot and never answers, so a lane that does not yield
        // starves everything behind it (L5's witness).
        let roll = if w == black_hole() && roll < 14 {
            20
        } else {
            roll
        };
        if roll < 14 {
            what = format!("send·I {ks}");
            m.waiters.entry(w.clone()).or_default().0 = true;
            m.hook(&w);
            sends_payload(&mut m, &w, &op);
            let window = a::window(&m.p);
            m.r.send(&w, Lane::Interactive, window);
            a::send(&mut m.p, w.clone(), op);
        } else if roll < 38 {
            m.waiters.entry(w.clone()).or_default().1 = true;
            let lane = if m.waiters.get(&w).is_some_and(|(i, _)| *i) {
                Lane::Interactive
            } else {
                Lane::Background
            };
            what = format!(
                "send·{} {ks}",
                if lane == Lane::Interactive { "I" } else { "B" }
            );
            m.hook(&w);
            sends_payload(&mut m, &w, &op);
            let window = a::window(&m.p);
            m.r.send(&w, lane, window);
            a::send(&mut m.p, w.clone(), op);
        } else if roll < 41 {
            // An INTERACTIVE waiter LEAVES while a background one stays: the need is background work now, but a need is
            // never demoted (L6) -- the next background send of it must not put it in the slot's count.
            if !m.waiters.get(&w).is_some_and(|(i, b)| *i && *b) {
                continue;
            }
            what = format!("leave·I {ks}");
            m.waiters.entry(w.clone()).or_default().0 = false;
            m.hook(&w);
        } else if roll < 43 {
            if w == black_hole() || a::need(&m.p, &w) == N::Absent || !m.r.need.contains_key(&w) {
                continue;
            }
            what = format!("join·I {ks}");
            m.waiters.entry(w.clone()).or_default().0 = true;
            m.hook(&w);
            a::join(&mut m.p);
            let window = a::window(&m.p);
            m.r.join(&w, window);
        } else if roll < 63 {
            // The node answers the key's OLDEST send it holds (F61) -- one the page counts, or one it lost (late).
            if w == black_hole() || !m.ledger.answerable(&w) {
                continue;
            }
            what = format!("answer {ks}");
            m.answer(&w);
        } else if roll < 72 {
            if matches!(w, Waiting::Sign(_)) {
                // A NAMED answer carrying an id nothing holds: dropped and counted, nothing changes.
                what = format!("answer-named nothing {ks}");
                a::answer_named(&mut m.p, &w, u32::MAX);
                m.log.push(format!("{step}: {what}"));
                m.check(step, &keys);
                continue;
            }
            if w == black_hole() || !m.ledger.answerable(&w) || matches!(w, Waiting::Update(_)) {
                continue;
            }
            what = format!("answer-without {ks}");
            m.ledger.answer(&w);
            a::answer_without(&mut m.p, &w);
            let window = a::window(&m.p);
            m.r.answer_without(&w, window);
        } else if roll < 79 {
            what = format!("withdraw {ks}");
            m.waiters.remove(&w);
            m.hook(&w);
            a::withdraw(&mut m.p, &w);
            let window = a::window(&m.p);
            m.r.withdraw(&w, window);
        } else if roll < 97 {
            // TIME: to the next due (a need's, the held send's, a stale send's), or a little.
            let now = a::now(&m.p);
            let t = if roll < 94 {
                a::dues(&m.p)
                    .iter()
                    .map(|(_, at, _)| *at)
                    .filter(|at| *at > now)
                    .min()
                    .unwrap_or(now + 1_000)
            } else {
                now + rng.below(500) as u64
            };
            what = format!("tick +{} ms", t - now);
            m.tick_to(t);
        } else {
            what = "reconnect".into();
            for h in &mut m.ledger.sends {
                h.answerable = false;
            }
            a::reconnect(&mut m.p);
            let window = a::window(&m.p);
            m.r.reconnect(window);
        }
        m.log.push(format!("{step}: {what}"));
        m.check(step, &keys);
        if std::env::var("OPLIFE_TRACE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            == Some(seed)
        {
            let needs: Vec<String> = keys
                .iter()
                .filter(|k| a::need(&m.p, k) != N::Absent)
                .map(|k| format!("{}={:?}", key_short(k), a::need(&m.p, k)))
                .collect();
            let rneeds: Vec<String> =
                m.r.need
                    .iter()
                    .map(|(k, n)| format!("{}={:?}", key_short(k), n))
                    .collect();
            let st: Vec<String> = a::stale(&m.p)
                .iter()
                .map(|(k, n)| format!("{}×{n}", key_short(k)))
                .collect();
            let rst: Vec<String> =
                m.r.stale
                    .iter()
                    .map(|(k, v)| format!("{}{:?}", key_short(k), v))
                    .collect();
            let sends: Vec<String> = a::sends(&m.p)
                .iter()
                .map(|(k, s, _, l)| {
                    format!(
                        "{}#{s}{}",
                        key_short(k),
                        if *l == Lane::Background { "B" } else { "" }
                    )
                })
                .collect();
            let un: Vec<String> = a::untracked(&m.p)
                .iter()
                .map(|(k, n)| format!("{}×{n}", key_short(k)))
                .collect();
            let late: Vec<String> = m
                .ledger
                .sends
                .iter()
                .filter(|h| !h.counted)
                .map(|h| format!("{}#{}", key_short(&h.w), h.seq))
                .collect();
            let node: Vec<String> = m
                .ledger
                .sends
                .iter()
                .filter(|h| h.counted)
                .map(|h| {
                    format!(
                        "{}#{}{}",
                        key_short(&h.w),
                        h.seq,
                        if h.lane == Lane::Background { "B" } else { "" }
                    )
                })
                .collect();
            println!("{step} {what}\n   page {needs:?} stale {st:?} held {:?} sends {sends:?} untracked {un:?}\n   ref  {rneeds:?} stale {rst:?} held {:?}\n   node {node:?} late {late:?} dropped {:?}", a::held(&m.p).map(|k| key_short(&k)), m.r.held.as_ref().map(|(k, a, s)| format!("{}{a}#{s}", key_short(k))), m.ledger.dropped.iter().map(|(k, n)| format!("{}×{n}", key_short(k))).collect::<Vec<_>>());
        }
    }
    // THE FAULT-FREE TAIL: the black hole withdrawn, every send answered as it comes, time to each due -- after which
    // nothing is held and nothing is stale (the architect: `stale` is bounded by time; L5's tail).
    m.waiters.remove(&black_hole());
    m.hook(&black_hole());
    a::withdraw(&mut m.p, &black_hole());
    let window = a::window(&m.p);
    m.r.withdraw(&black_hole(), window);
    m.log.push("tail".into());
    m.check(steps, &keys);
    for round in 0..2_000 {
        for k in &keys {
            while *k != black_hole() && m.ledger.answerable(k) {
                m.answer(k);
            }
        }
        m.check(steps + 1 + round, &keys);
        let now = a::now(&m.p);
        let Some(t) = a::dues(&m.p)
            .iter()
            .map(|(_, at, _)| *at)
            .filter(|at| *at > now)
            .min()
        else {
            break;
        };
        m.tick_to(t);
        m.check(steps + 1 + round, &keys);
    }
    if a::held(&m.p).is_some() || !a::stale(&m.p).is_empty() {
        let why = format!(
            "after the fault-free tail: held {:?}, stale {:?}",
            a::held(&m.p),
            a::stale(&m.p)
        );
        m.fail(
            steps,
            "L5 the fault-free tail ends with nothing held or stale",
            why,
        );
    }
    // N2's memory after the tail -- past every GET's decay -- is exactly the lost sends the node DROPPED (never
    // answered), and none of the black hole's (never answered by design) counts against it.
    let now = a::now(&m.p);
    m.tick_to(now + GET_OVER_MS + 1);
    let mut untracked = a::untracked(&m.p);
    untracked.remove(&black_hole());
    let show = |m: &BTreeMap<Waiting, usize>| {
        m.iter()
            .map(|(k, n)| format!("{}×{n}", key_short(k)))
            .collect::<Vec<_>>()
    };
    if !m.ledger.drops && !untracked.is_empty() {
        // A node that answers every lost send, late: the memory of them returns to NOTHING.
        let why = format!(
            "after the fault-free tail, with a node that answers every send: untracked {:?}",
            show(&untracked)
        );
        m.fail(
            steps,
            "N2 the late-answer memory returns to 0 when the node answers",
            why,
        );
    }
    if untracked.len() > op_life::UNTRACKED_KEYS_MAX {
        let why = format!(
            "{} keys remembered, over the cap {}",
            untracked.len(),
            op_life::UNTRACKED_KEYS_MAX
        );
        m.fail(steps, "N2 the late-answer memory is bounded", why);
    }
    Ran {
        peak: m.p.untracked_peak(),
        evicted: m.p.untracked_evicted(),
        drops: m.ledger.drops,
        found: m.found,
    }
}

#[test]
fn op_life_model() {
    let seeds: u64 = std::env::var("CRAFTWORKS_MODEL_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let steps = 1500;
    let mut by_inv: BTreeMap<&'static str, (u64, String)> = BTreeMap::new();
    // N2's memory, per node kind [answers every lost send, drops some]: the peak keys held at once, the keys evicted.
    let (mut peak, mut evicted) = ([0usize; 2], [0u64; 2]);
    for seed in 1..=seeds {
        let r = run(seed, steps);
        let k = usize::from(r.drops);
        peak[k] = peak[k].max(r.peak);
        evicted[k] += r.evicted;
        for (inv, first) in r.found {
            let e = by_inv.entry(inv).or_insert((0, first));
            e.0 += 1;
        }
    }
    println!("OP-LIFE model: {seeds} seeds × {steps} steps + the fault-free tail");
    println!(
        "  N2 memory (cap {}): node answers every lost send -- peak {} keys, {} evicted; node drops some -- peak {} keys, {} evicted",
        op_life::UNTRACKED_KEYS_MAX,
        peak[0],
        evicted[0],
        peak[1],
        evicted[1]
    );
    for (inv, (n, first)) in &by_inv {
        println!("  {inv}: {n} seeds\n    first: {first}");
    }
    assert!(
        by_inv.is_empty(),
        "OP-LIFE invariants violated: {:?}",
        by_inv.keys().collect::<Vec<_>>()
    );
}
