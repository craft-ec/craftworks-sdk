//! A transport that does what a real connection does: reorder, duplicate and
//! drop.
//!
//! Wrapping a working loopback rather than faking answers, so what is under
//! test is the CLIENT's tolerance and not a mock's imagination. Every
//! perturbation is seeded and printed, so a failure is reproducible from its
//! own output rather than from a lucky re-run.
//!
//! # Why each of the three, specifically
//!
//! * **Reorder** — replies arrive interleaved with pushes and with answers to
//!   other requests. A client that matched replies by POSITION would pass
//!   every ordered test and fail here.
//! * **Duplicate** — a node may re-send, and a client that counted a write's
//!   states rather than recording them would double-count. This is also what
//!   an idempotent `on_inbound` is FOR, and an idempotence nothing tests is a
//!   claim.
//! * **Drop** — the node's notification channel drops when full (F39), and a
//!   reply to a request can be lost the same way. A client that waits for
//!   something that is never coming hangs a UI, so every wait must be bounded.

#![allow(dead_code)]

use craftworks_sdk::Transport;

/// What was done to the stream, for the failure message.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perturbed {
    pub reordered: usize,
    pub duplicated: usize,
    pub dropped: usize,
    pub delivered: usize,
}

pub struct Chaos<T: Transport> {
    inner: T,
    seed: u64,
    /// Replies held back from an earlier exchange, delivered later — which is
    /// what reordering actually is on a connection.
    held: Vec<Vec<u8>>,
    /// 1 in N replies is held, duplicated, dropped. 0 disables that arm.
    pub hold_1_in: u64,
    pub dup_1_in: u64,
    pub drop_1_in: u64,
    /// Exchanges passed through untouched before any arm applies: an engine
    /// must be STARTED before a read reaches it (sdk#223), and a connection
    /// that drops every reply would drop the start's too.
    pub calm_for: usize,
    pub seen: Perturbed,
}

impl<T: Transport> Chaos<T> {
    pub fn new(inner: T, seed: u64) -> Chaos<T> {
        Chaos {
            inner,
            seed: seed | 1,
            held: Vec::new(),
            hold_1_in: 3,
            dup_1_in: 4,
            drop_1_in: 0,
            calm_for: 0,
            seen: Perturbed::default(),
        }
    }

    /// The control: a chaos transport that perturbs NOTHING.
    ///
    /// Same type, same code path, every arm off. A suite that only ever runs
    /// the perturbed arm cannot tell "the client tolerates this" from "the
    /// wrapper does nothing".
    pub fn calm(inner: T) -> Chaos<T> {
        Chaos {
            hold_1_in: 0,
            dup_1_in: 0,
            drop_1_in: 0,
            ..Chaos::new(inner, 1)
        }
    }

    pub fn dropping(inner: T, seed: u64, one_in: u64) -> Chaos<T> {
        Chaos {
            drop_1_in: one_in,
            ..Chaos::new(inner, seed)
        }
    }

    fn roll(&mut self, one_in: u64) -> bool {
        if one_in == 0 {
            return false;
        }
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed.is_multiple_of(one_in)
    }
}

impl<T: Transport> Transport for Chaos<T> {
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>> {
        let fresh = self.inner.exchange(request);
        if self.calm_for > 0 {
            self.calm_for -= 1;
            return fresh;
        }
        // Anything held from before goes out FIRST, which is what makes this
        // a reorder rather than a delay: a reply to an older request arrives
        // among the answers to a newer one.
        let mut out: Vec<Vec<u8>> = std::mem::take(&mut self.held);
        self.seen.reordered += out.len();
        for r in fresh {
            if self.roll(self.drop_1_in) {
                self.seen.dropped += 1;
                continue;
            }
            if self.roll(self.hold_1_in) {
                self.held.push(r);
                continue;
            }
            if self.roll(self.dup_1_in) {
                self.seen.duplicated += 1;
                out.push(r.clone());
            }
            out.push(r);
        }
        self.seen.delivered += out.len();
        out
    }

    /// Flush what was held back, without sending anything.
    ///
    /// This is the other half of reordering: a reply kept back from an earlier
    /// exchange has to be able to ARRIVE, or "reordered" would only ever mean
    /// "delayed until the next request", which is a weaker thing than a
    /// connection does.
    fn poll(&mut self) -> Vec<Vec<u8>> {
        let out = std::mem::take(&mut self.held);
        self.seen.reordered += out.len();
        self.seen.delivered += out.len();
        out
    }
}
