//! OP-LIFE.md's gate: the ONE per-tree piece. It reads the page's op record into the model's terms (a need per key,
//! the held send) and drives the page through its own doors. The model (`op_life_model`) is the same on every tree;
//! only this file changes with the record's representation. This version reads the ONE RECORD (`op_life::Ops`); the
//! known-broken trees (06ec9d6, c3a298a, 138b0e4) were gated with the flags version of this file.
// A TEST driver: its matches over the pool are not the record's (op_life's lints are for production).
#![allow(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
use crate::op_life::model::{Due, N};
use crate::op_life::Need;
use crate::*;

/// Each key's need, as the page holds it.
pub(crate) fn need(p: &Page, w: &Waiting) -> N {
    match p.ops.need(w) {
        None => N::Absent,
        Some(Need::Queued {
            lane: Lane::Interactive,
            ..
        }) => N::QI,
        Some(Need::Queued {
            lane: Lane::Background,
            ..
        }) => N::QB,
        Some(Need::OnWire {
            lane: Lane::Interactive,
            ..
        }) => N::WI,
        Some(Need::OnWire {
            lane: Lane::Background,
            ..
        }) => N::WB,
        Some(Need::Parked {
            lane: Lane::Interactive,
            ..
        }) => N::PI,
        Some(Need::Parked {
            lane: Lane::Background,
            ..
        }) => N::PB,
    }
}

/// The send the node holds that no need rides, and its key.
pub(crate) fn held(p: &Page) -> Option<Waiting> {
    p.ops.held().map(|(w, _)| w.clone())
}

/// Every send the page still counts as at the node: (key, send ordinal, due, lane).
pub(crate) fn sends(p: &Page) -> Vec<(Waiting, u32, u64, Lane)> {
    p.ops
        .at_node()
        .map(|(w, s, _, place)| {
            let lane = if place == op_life::Place::Slot {
                Lane::Background
            } else {
                Lane::Interactive
            };
            (w.clone(), s.seq, s.at, lane)
        })
        .collect()
}

/// Everything with a deadline: needs (on the wire or parked), the held send, and each stale send with its AGE RANK
/// among its key's stale sends (0 = oldest).
pub(crate) fn dues(p: &Page) -> Vec<(Waiting, u64, Due)> {
    let mut out: Vec<(Waiting, u64, Due)> = p
        .ops
        .needs()
        .filter_map(|(w, n)| n.due().map(|at| (w.clone(), at, Due::Need)))
        .chain(p.ops.held().map(|(w, h)| (w.clone(), h.send.at, Due::Held)))
        .collect();
    let mut stale: Vec<(Waiting, u32, u64)> = p
        .ops
        .at_node()
        .filter(|(_, _, h, _)| *h == op_life::Holder::Stale)
        .map(|(w, s, ..)| (w.clone(), s.seq, s.at))
        .collect();
    stale.sort();
    let mut rank: BTreeMap<Waiting, usize> = BTreeMap::new();
    for (w, _, at) in stale {
        let r = rank.entry(w.clone()).or_default();
        out.push((w, at, Due::Stale(*r)));
        *r += 1;
    }
    out
}

/// Would this on-wire GET be SILENT at `t` (its node GET not over)?
pub(crate) fn silent_at(p: &Page, w: &Waiting, t: u64) -> bool {
    matches!(w, Waiting::Get(_))
        && p.ops
            .need(w)
            .and_then(Need::send)
            .is_some_and(|s| t < node_get_over_at(s.sent_at, p.rto.rto_ms()))
}

pub(crate) fn window(p: &Page) -> usize {
    p.window.size()
}

pub(crate) fn now(p: &Page) -> u64 {
    p.now
}

pub(crate) fn send(p: &mut Page, w: Waiting, op: Op) {
    p.send(w, op);
}

pub(crate) fn join(p: &mut Page) {
    p.promote_joined();
}

pub(crate) fn withdraw(p: &mut Page, w: &Waiting) {
    p.on(w, op_life::OpEvent::Withdraw);
}

/// The node answers `w` with what it waits for.
pub(crate) fn answer(p: &mut Page, w: &Waiting) {
    p.answered(w);
}

/// The node answers `w` without what it waits for: a GET's NotFound, a PUT refused transiently.
pub(crate) fn answer_without(p: &mut Page, w: &Waiting) {
    match w {
        Waiting::Get(id) => {
            let now = p.now;
            p.answer(Answer::GetMissed(*id), Ms(now));
        }
        Waiting::Put(id) => {
            let now = p.now;
            p.answer(
                Answer::PutRefused {
                    id: *id,
                    transient: true,
                },
                Ms(now),
            );
        }
        Waiting::Held(_)
        | Waiting::Sign(_)
        | Waiting::Update(_)
        | Waiting::Warm
        | Waiting::RecoverHead
        | Waiting::Verify
        | Waiting::ReadBack(_)
        | Waiting::Hint
        | Waiting::PutApp(_)
        | Waiting::Ext(_) => {}
    }
}

/// The node answers `w` NAMING its send by the request id (SG02).
pub(crate) fn answer_named(p: &mut Page, w: &Waiting, request: u32) {
    p.on(w, op_life::OpEvent::AnswerNamed { request });
}

pub(crate) fn tick(p: &mut Page, t: u64) {
    p.tick(Ms(t));
}

pub(crate) fn reconnect(p: &mut Page) {
    let now = p.now;
    p.reconnected(Ms(now));
}

/// Stale sends per key.
pub(crate) fn stale(p: &Page) -> BTreeMap<Waiting, usize> {
    let mut out: BTreeMap<Waiting, usize> = BTreeMap::new();
    for (w, ..) in p
        .ops
        .at_node()
        .filter(|(_, _, h, _)| *h == op_life::Holder::Stale)
    {
        *out.entry(w.clone()).or_default() += 1;
    }
    out
}

/// N2's untracked sends per key.
pub(crate) fn untracked(p: &Page) -> BTreeMap<Waiting, usize> {
    p.ops.untracked().map(|(w, n)| (w.clone(), n)).collect()
}
