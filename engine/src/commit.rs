//! A COMMIT'S LIFE (COMMIT-LIFE rev 5, sdk#481; the owner's "Structure before code" and "One writer, by type"): the
//! ONE commit in flight as ONE stage -- `Idle`, `Racing` (its PUTs out, the head waiting for race-ready, §P),
//! `Heading` (its `UpdateHead` sent) -- with its data in its variant, so it dies with the stage (C1); and the published
//! commits still putting toward BACKED_UP (`Backing`, with the writes carried onto the next own commit's).
//!
//! The fields are PRIVATE to this module, so the only code that can change a stage or a Backing is the transitions
//! below: `start`, `send_head`, `end` (forward only, C2: nothing here moves Heading back to Racing), and `back`,
//! `acked`, `supersede`, `join_open`, `take_carry`. The commit's CONTENT (its acks, held members, held-back parity) is
//! counted through [`CommitLife::commit_mut`], which cannot change the stage. The engine's side of an end -- the queue,
//! the warm root, the seq -- is ONE function, `Engine::end_commit` (C3).
//!
//! A machine's module: no catch-all over an enum here (CLAUDE.md), so a new stage fails to compile -- under clippy --
//! until every match handles it, a `_` over several variants and over one alike.
#![deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)]

use crate::{Backing, ClientId, Commit, CommitStage, Witness, WriteId};
use freenet_prolly::Cid;
use std::collections::BTreeSet;

/// How a commit ends (C3): every end goes through `Engine::end_commit`, with this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Its head showed (E6).
    Published,
    /// A foreign head (E8), adopted here when `head` is given; the WITNESS decides a Heading commit's group (⁵). A
    /// Racing commit's head never left: the witness is not read and no try is spent (A8).
    Dead { witness: Option<Witness>, head: Option<(u64, Cid)> },
    /// A final refusal: the head refused by the signer (E9), or a block the commit needs rejected (E4).
    Failed,
}

#[derive(Debug, Default)]
enum Stage {
    #[default]
    Idle,
    Racing {
        commit: Commit,
        /// The engine time it started: the stall clock (E11).
        started: u64,
    },
    Heading {
        commit: Commit,
        started: u64,
    },
}

#[derive(Debug, Default)]
pub(crate) struct CommitLife {
    stage: Stage,
    /// Published commits still putting toward BACKED_UP (§P), in publish order.
    backing: Vec<Backing>,
    /// Writes whose Backing a later commit superseded: they join the next own commit's Backing.
    carry: BTreeSet<(ClientId, WriteId)>,
}

impl CommitLife {
    // ---- the stage ----

    pub(crate) fn stage(&self) -> CommitStage {
        match &self.stage {
            Stage::Idle => CommitStage::Idle,
            Stage::Racing { .. } => CommitStage::Racing,
            Stage::Heading { .. } => CommitStage::Heading,
        }
    }

    /// The commit in flight.
    pub(crate) fn commit(&self) -> Option<&Commit> {
        match &self.stage {
            Stage::Idle => None,
            Stage::Racing { commit, .. } | Stage::Heading { commit, .. } => Some(commit),
        }
    }

    /// The commit in flight, to COUNT into (acks, held members, the held-back parity). Its stage is not reachable
    /// from here.
    pub(crate) fn commit_mut(&mut self) -> Option<&mut Commit> {
        match &mut self.stage {
            Stage::Idle => None,
            Stage::Racing { commit, .. } | Stage::Heading { commit, .. } => Some(commit),
        }
    }

    /// When the commit in flight started (the stall clock), if one is.
    pub(crate) fn started(&self) -> Option<u64> {
        match &self.stage {
            Stage::Idle => None,
            Stage::Racing { started, .. } | Stage::Heading { started, .. } => Some(*started),
        }
    }

    /// Re-anchor the stall clock: to `now` when the clock was reset, or when it started before any clock (`0`).
    pub(crate) fn anchor(&mut self, now: u64, reset: bool) {
        match &mut self.stage {
            Stage::Idle => {}
            Stage::Racing { started, .. } | Stage::Heading { started, .. } => {
                if reset || *started == 0 {
                    *started = now;
                }
            }
        }
    }

    /// Idle → Racing (E1). `false` if a commit is already in flight: an impossible cell, counted by the caller.
    pub(crate) fn start(&mut self, commit: Commit, now: u64) -> bool {
        match self.stage {
            Stage::Idle => {
                self.stage = Stage::Racing { commit, started: now };
                true
            }
            Stage::Racing { .. } | Stage::Heading { .. } => false,
        }
    }

    /// Racing → Heading: the head is sent. `false` in any other stage (a Heading commit re-issues without moving).
    pub(crate) fn send_head(&mut self) -> bool {
        match std::mem::take(&mut self.stage) {
            Stage::Racing { commit, started } => {
                self.stage = Stage::Heading { commit, started };
                true
            }
            other @ (Stage::Idle | Stage::Heading { .. }) => {
                self.stage = other;
                false
            }
        }
    }

    /// → Idle: the commit in flight, taken for `Engine::end_commit`, and whether its head had been sent.
    pub(crate) fn end(&mut self) -> Option<(Commit, bool)> {
        match std::mem::take(&mut self.stage) {
            Stage::Idle => None,
            Stage::Racing { commit, .. } => Some((commit, false)),
            Stage::Heading { commit, .. } => Some((commit, true)),
        }
    }

    // ---- Backing (§P) ----

    /// A published commit's blocks still out: its Backing.
    pub(crate) fn back(&mut self, writes: Vec<(ClientId, WriteId)>, remaining: BTreeSet<Cid>) {
        self.backing.push(Backing { writes, remaining });
    }

    /// `id` acked: the writes of every Backing it drained.
    pub(crate) fn acked(&mut self, id: &Cid) -> Vec<(ClientId, WriteId)> {
        let mut drained = Vec::new();
        self.backing.retain_mut(|b| {
            if b.remaining.remove(id) && b.remaining.is_empty() {
                drained.extend(b.writes.iter().copied());
                return false;
            }
            true
        });
        drained
    }

    /// Stragglers a root move made garbage (`gone`): withdrawn from every Backing, whose writes are carried onto the
    /// next own commit's. The ids withdrawn, and the writes of every Backing that drained.
    pub(crate) fn supersede(&mut self, gone: &BTreeSet<Cid>) -> (BTreeSet<Cid>, Vec<(ClientId, WriteId)>) {
        let mut withdrawn = BTreeSet::new();
        for b in self.backing.iter_mut() {
            let hit: Vec<Cid> = b.remaining.intersection(gone).copied().collect();
            if hit.is_empty() {
                continue;
            }
            for id in hit {
                b.remaining.remove(&id);
                withdrawn.insert(id);
            }
            self.carry.extend(b.writes.iter().copied());
        }
        let mut drained = Vec::new();
        self.backing.retain(|b| {
            if b.remaining.is_empty() {
                drained.extend(b.writes.iter().copied());
                false
            } else {
                true
            }
        });
        (withdrawn, drained)
    }

    /// A no-op write (#164) joins every open Backing: BACKED_UP when they drain. `false` when none is open (BACKED_UP
    /// now).
    pub(crate) fn join_open(&mut self, w: (ClientId, WriteId)) -> bool {
        if self.backing.is_empty() {
            return false;
        }
        for b in self.backing.iter_mut() {
            if !b.writes.contains(&w) {
                b.writes.push(w);
            }
        }
        true
    }

    /// The carried writes, for the commit publishing now.
    pub(crate) fn take_carry(&mut self) -> BTreeSet<(ClientId, WriteId)> {
        std::mem::take(&mut self.carry)
    }

    // ---- read accessors: nothing below changes a stage or a Backing ----

    /// Is `w` still waiting on a Backing, or carried to the next one?
    pub(crate) fn waits_for(&self, w: &(ClientId, WriteId)) -> bool {
        self.carry.contains(w) || self.backing.iter().any(|b| b.writes.contains(w))
    }

    /// Does a published commit's Backing still owe `id`?
    pub(crate) fn backing_owes(&self, id: &Cid) -> bool {
        self.backing.iter().any(|b| b.remaining.contains(id))
    }

    /// Every block the Backings still owe.
    pub(crate) fn unacked(&self) -> BTreeSet<Cid> {
        self.backing.iter().flat_map(|b| b.remaining.iter().copied()).collect()
    }

    pub(crate) fn backing_len(&self) -> usize {
        self.backing.len()
    }
}
