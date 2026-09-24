//! The live notifier: a CLIENT subscription to a tree's head contract.
//!
//! # Why this exists, measured rather than argued
//!
//! Slice 6 shipped an engine-side subscription that pushes `Changed`. The live
//! run then showed what it is worth: **a delegate's output returns to whoever
//! INVOKED it**, so an engine-originated push lands on the exchange in flight —
//! the writer's — and never on a watcher's connection (F40). The watcher was
//! correct only because of its tick backstop.
//!
//! The platform's own push path is the client API: a CLIENT subscription to a
//! contract receives `UpdateNotification`s, and it is also the only kind of
//! subscription that registers interest (F39). So the notifier for a live
//! binding is a client subscription to the HEAD contract of the tree it reads —
//! the Register whose value is `(seq, root)`. When that moves, the tree moved.
//!
//! Three mechanisms now, and they are in this order on purpose:
//!
//! 1. **The head subscription** — reaches a connection that made no write, so
//!    it is the one that makes another tab work.
//! 2. **The engine's `Changed`** — same connection only, but it costs nothing
//!    extra and arrives first for the writer's own tabs.
//! 3. **The tick** — the backstop, and the only one that is not lossy. The
//!    node drops notifications when its channel is full and can evict a
//!    subscription without telling anyone, so nothing above is a guarantee.
//!
//! All three end in the same `Binding::reload()`. That is deliberate: the path
//! everything funnels through is the path that is always exercised.
//!
//! # One subscription per TREE, never per binding
//!
//! A tree has ONE head. Five live bindings over five ranges of one tree are
//! five reasons to watch the same contract, not five subscriptions — and on
//! the node they would be five, against a per-delegate cap of 256 that evicts
//! the least-recently-notified without telling it (F39). A feed across many
//! identities is the case that makes this matter: N foreign trees must not
//! become N × (their bindings) subscriptions.
//!
//! # The head contract id is GIVEN, not discovered
//!
//! The SDK is told which contract is a tree's head rather than asking the
//! engine for it. For its own tree the app provisioned it; for a foreign tree
//! the app learnt it from wherever it learnt of the tree at all. Asking the
//! engine would put a round trip in front of every subscription and would add
//! a field to a reply that is already on the wire in v1.

use crate::binding::{Binding, LiveMode};
use crate::store::{Read, Reads};
use protocol::Step;

/// A tree's head contract, as the client API names it.
pub type HeadId = [u8; 32];

/// The host's ability to subscribe to a contract and hear when it changes.
///
/// Implemented by whatever owns the connection — a websocket in a browser, the
/// live driver in a test. It is a TRAIT because this crate must not know what
/// a connection is: that is what has let the whole write path be tested with
/// no node at all, and the notifier is no different.
pub trait HeadWatch {
    /// Ask the node to push updates for this contract.
    ///
    /// Returns whether it was accepted. A refusal is not an error — the tick
    /// backstop is still correct — but it IS a downgrade, and one that is not
    /// reported is indistinguishable from a tree that simply stopped changing.
    ///
    /// **Idempotent, and it has to be.** Subscriptions are client state; the
    /// SDK re-asserts them on every reconnect, and re-subscribing is free at
    /// the node (F39).
    fn watch(&mut self, head: HeadId) -> bool;

    /// Heads the node has said moved since this was last asked.
    ///
    /// Drained. A head that moved twice before anyone looked is one wake-up:
    /// what a binding does with it is compare roots and reload, which is
    /// idempotent, so collapsing them loses nothing.
    fn moved(&mut self) -> Vec<HeadId>;
}

/// A recorder for the probe. Always on; dumped when something fails.
///
/// The same vocabulary the engine uses on the wire, deliberately — one set of
/// words for a test's dump, the harness's tables, `db.trace` and the builder's
/// viewer. A probe written for one bug belongs here with its test rather than
/// in a scratch branch.
#[derive(Default)]
pub struct Recorder {
    events: Vec<(Step, u64)>,
    max: usize,
}

impl Recorder {
    pub fn new(max: usize) -> Recorder {
        Recorder {
            events: Vec::new(),
            max,
        }
    }

    pub fn note(&mut self, what: Step, n: u64) {
        if self.max == 0 {
            // Recording off is a PARAMETER, not a rebuild: a probe whose cost
            // can only be removed by recompiling is one nobody measures.
            return;
        }
        if self.events.len() == self.max {
            self.events.remove(0);
        }
        self.events.push((what, n));
    }

    pub fn events(&self) -> &[(Step, u64)] {
        &self.events
    }

    /// Everything recorded, for a failure message.
    pub fn dump(&self) -> String {
        let mut s = String::from("probe:\n");
        for (what, n) in &self.events {
            s.push_str(&format!("  {what:?}({n})\n"));
        }
        s
    }

    pub fn count(&self, what: Step) -> usize {
        self.events.iter().filter(|(w, _)| *w == what).count()
    }
}

/// Every live binding, grouped by the tree it reads.
///
/// Owns the head subscriptions: one per tree, taken when a tree's first live
/// binding appears and never again.
pub struct Trees {
    trees: Vec<Tree>,
    pub probe: Recorder,
}

struct Tree {
    head: HeadId,
    /// Whether the node accepted a subscription to this head.
    watched: bool,
    bindings: Vec<Binding>,
}

impl Default for Trees {
    fn default() -> Self {
        Trees::new()
    }
}

impl Trees {
    pub fn new() -> Trees {
        Trees {
            trees: Vec::new(),
            probe: Recorder::new(256),
        }
    }

    /// Add a binding over a tree, taking a head subscription if it is the
    /// tree's first LIVE one.
    ///
    /// A non-live binding takes nothing — not an engine subscription and not a
    /// head subscription — so a tree read only by plain bindings is watched by
    /// nobody, which is the whole point of the flag.
    pub fn add<W: HeadWatch>(&mut self, head: HeadId, binding: Binding, w: &mut W) -> usize {
        let live = binding.is_live();
        let i = match self.trees.iter().position(|t| t.head == head) {
            Some(i) => i,
            None => {
                self.trees.push(Tree {
                    head,
                    watched: false,
                    bindings: Vec::new(),
                });
                self.trees.len() - 1
            }
        };
        self.trees[i].bindings.push(binding);
        if live && !self.trees[i].watched {
            let ok = w.watch(head);
            self.probe.note(Step::HeadWatch, ok as u64);
            self.trees[i].watched = ok;
            let mode = if ok {
                LiveMode::HeadSubscribed
            } else {
                LiveMode::Polled
            };
            if !ok {
                self.probe.note(Step::Downgraded, 0);
            }
            for b in &mut self.trees[i].bindings {
                if b.is_live() {
                    b.note_mode(mode);
                }
            }
        } else if live {
            // The tree is already watched; this binding inherits it. No second
            // subscription — the node would take one, and a per-delegate cap
            // that evicts the least-recently-notified is not a budget to spend
            // on the same contract twice.
            let last = self.trees[i].bindings.len() - 1;
            self.trees[i].bindings[last].note_mode(LiveMode::HeadSubscribed);
        }
        self.trees[i].bindings.len() - 1
    }

    /// Heads the node has pushed: reload the live bindings on those trees.
    ///
    /// Returns how many bindings actually changed. A push is a TRIGGER for
    /// the same reload the tick does — one path, so the one that matters is
    /// the one that is always exercised.
    pub fn on_pushed<W: HeadWatch, S: Reads>(&mut self, w: &mut W, store: &mut S) -> Read<usize> {
        let moved = w.moved();
        let mut changed = 0;
        for head in moved {
            let Some(t) = self.trees.iter_mut().find(|t| t.head == head) else {
                // A push for a tree this client no longer watches. ORDINARY:
                // the platform has no unsubscribe, so the node goes on waking
                // us for contracts we have forgotten (F39). Never an error.
                continue;
            };
            let live = t.bindings.iter().filter(|b| b.is_live()).count();
            self.probe.note(Step::HeadMoved, live as u64);
            for b in &mut t.bindings {
                if !b.is_live() {
                    continue;
                }
                let before = b.snapshot();
                b.reload(store)?;
                if !std::rc::Rc::ptr_eq(&before, &b.snapshot()) {
                    changed += 1;
                    self.probe.note(Step::Reloaded, b.snapshot().len() as u64);
                } else {
                    self.probe.note(Step::Quiet, 0);
                }
            }
        }
        Ok(changed)
    }

    /// The backstop: reload everything, live or not, on the client's tick.
    ///
    /// Cheap when nothing moved — each binding compares roots first — and it
    /// is the only mechanism here that is not lossy.
    pub fn tick<S: Reads>(&mut self, store: &mut S) -> Read<usize> {
        let mut changed = 0;
        for t in &mut self.trees {
            for b in &mut t.bindings {
                let before = b.snapshot();
                b.reload(store)?;
                if !std::rc::Rc::ptr_eq(&before, &b.snapshot()) {
                    changed += 1;
                    self.probe.note(Step::Reloaded, b.snapshot().len() as u64);
                } else {
                    self.probe.note(Step::Quiet, 0);
                }
            }
        }
        Ok(changed)
    }

    /// How many head subscriptions are held. One per watched tree.
    pub fn subscriptions(&self) -> usize {
        self.trees.iter().filter(|t| t.watched).count()
    }

    /// Read a binding. There is deliberately no `&mut` counterpart.
    ///
    /// Every way a binding's rows can change goes through `on_pushed` or
    /// `tick`, which are the probed boundaries. Handing out a `&mut Binding`
    /// would let a caller — a test most of all — reload outside the recording,
    /// and a test that builds the system bare is exactly what the probe rule
    /// exists to prevent: the failure then says nothing and somebody re-runs
    /// it with prints added.
    pub fn binding(&self, tree: usize, i: usize) -> &Binding {
        &self.trees[tree].bindings[i]
    }

    /// Change how a binding decides between a delta and a full re-read.
    ///
    /// Exposed by name rather than by handing out the binding, for the reason
    /// above: this cannot reload anything.
    pub fn delta_above(&mut self, tree: usize, i: usize, rows: usize) {
        self.trees[tree].bindings[i].delta_above(rows);
    }

    pub fn trees(&self) -> usize {
        self.trees.len()
    }
}
