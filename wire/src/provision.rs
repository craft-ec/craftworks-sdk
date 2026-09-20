//! Setting a node up so a page can use it: register the delegate, install the
//! contracts, mint a key once.
//!
//! The SDK owns this so an app does not have to know what a delegate or a
//! contract is. **This is the DEVELOPMENT path.** The long-term shape is both
//! fetched from the network by hash (§19, sdk#5); shipping them beside the
//! wasm is how a page can do it today.
//!
//! # Idempotent, because re-running is the normal case
//!
//! A page reloads. A tab is opened twice. A connection drops and comes back.
//! Every one of those runs this again, so "already registered" and "already
//! installed" are ORDINARY outcomes and not errors — and re-creating what
//! exists is the mistake F38 records, where a head re-PUT cost 152,542 B
//! instead of 112.
//!
//! # The key is minted once and then forgotten
//!
//! In development the page generates a signing key when the engine reports it
//! has none, hands it to the engine's secret store, and **keeps no copy**. On
//! every later open the page asks `Identity` and learns the head from the
//! engine — which is what makes "close the tab, reopen, the data is there"
//! true without a browser holding a key at all.
//!
//! It is a [`TestKey`](protocol::TestKey) in the type, on the wire and in
//! every log line. Real keys are sdk#14 and phase 6, a passkey-derived device
//! key that never leaves keycraft, and **nothing here may be reused as that
//! path.**
//!
//! # LOOPBACK ONLY
//!
//! Provisioning installs code and hands over a signing key. It is only ever
//! done against a node on this machine — [`crate::ws_url`] refuses anything
//! else — and the acks it rests on are that node's own word about its own
//! store. Against a node somebody else runs, an ack is unauthenticated and
//! "the contract is installed" would be a stranger's claim.
//!
//! Which steps are CONFIRMED and which merely acknowledged is stated per step
//! by [`Step::confirmed_by_asking`], because the two are different promises
//! and a caller deserves to know which one it has.

use crate::AckKind;

/// What a provisioning step did. Reported per artefact, because "it worked"
/// and "it was already there" are different facts an operator wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Did {
    /// It was not there, and now it is.
    Installed,
    /// It was already there. ORDINARY — see the module docs.
    AlreadyThere,
}

/// The steps, in the order they must happen.
///
/// An order, not a set: the delegate has to exist before it can be told about
/// contracts, and the engine has to have a key before it can sign a head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The engine delegate, ~721 KB — the one that is always chunked.
    Delegate,
    /// The Block contract, which the tree's nodes are stored under.
    BlockContract,
    /// The Register contract, which holds the head.
    RegisterContract,
    /// A signing key, minted only if the engine says it has none.
    Key,
}

impl Step {
    /// Every step, in order.
    pub const ALL: [Step; 4] = [
        Step::Delegate,
        Step::BlockContract,
        Step::RegisterContract,
        Step::Key,
    ];

    /// Does this ack answer THIS step, for the thing this step actually sent?
    ///
    /// **Matched by NAME, never by kind alone.** Both contract steps send a
    /// `Put` and get a `Put` back, so kind alone let a duplicate ack of the
    /// first contract complete the second — a run reporting a contract
    /// installed that had never been sent. The node names what it is
    /// answering; this compares it.
    ///
    /// `named` is what this step put on the wire: the delegate's key, or the
    /// contract instance id.
    pub fn answered_by(self, named: &str, ack: &AckKind) -> bool {
        match (self, ack) {
            (Step::Delegate, AckKind::Registered(k)) => k == named,
            (Step::BlockContract | Step::RegisterContract, AckKind::Put(k)) => k == named,
            // The key is proved by asking, not by an ack — see `Confirmed`.
            (Step::Key, AckKind::Ok) => true,
            _ => false,
        }
    }

    /// Whether this step's completion is CONFIRMED or merely acknowledged.
    ///
    /// Stated per step, because the two are different promises and a caller
    /// deserves to know which it has. `Identity` proves the delegate is
    /// registered and the engine has a key — it answers, which an unregistered
    /// delegate cannot. The contract puts rest on the node's ack, which on
    /// loopback is the node's own word about its own store.
    pub fn confirmed_by_asking(self) -> bool {
        matches!(self, Step::Delegate | Step::Key)
    }
}

/// What a whole provisioning run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provisioned {
    pub steps: Vec<(Step, Did)>,
}

impl Provisioned {
    pub fn did(&self, step: Step) -> Option<Did> {
        self.steps.iter().find(|(s, _)| *s == step).map(|(_, d)| *d)
    }

    /// Did anything actually change? A page can skip telling the person when
    /// nothing did.
    pub fn changed_anything(&self) -> bool {
        self.steps.iter().any(|(_, d)| *d == Did::Installed)
    }

    /// Every step accounted for. A run that quietly skipped one would leave a
    /// node that half works, and the failure would appear at the first write.
    pub fn complete(&self) -> bool {
        Step::ALL.iter().all(|s| self.did(*s).is_some())
    }
}

/// Drives provisioning, one step at a time.
///
/// A state machine, like everything else that decides: the page sends what
/// this hands out and feeds back what arrives. Nothing here opens a socket.
///
/// **One step in flight at a time.** The node answers acks that carry no
/// correlation id, so two steps outstanding cannot be told apart — and
/// matching the wrong ack to the wrong step would report a delegate
/// registered because a contract went in.
pub struct Provisioner {
    at: usize,
    /// What the step in flight PUT on the wire, so its ack can be matched to
    /// it rather than to whatever arrived next.
    in_flight: Option<(Step, String, u64)>,
    done: Provisioned,
    known: Vec<Step>,
    /// Acks that did not answer the step in flight. Counted: a node answering
    /// something nobody asked for is worth seeing.
    pub unexpected: usize,
    /// How long a step may go unanswered before it is reported stalled.
    ///
    /// Acks can take a minute or never arrive at all (F20), so a provisioner
    /// with no clock waits for ever on the first one that goes missing — and
    /// the page shows nothing while it does.
    pub stall_after_ms: u64,
    /// The step that stalled, if one has.
    stalled: Option<Step>,
    /// Why the run stopped, in the node's own words. Display only.
    refused: Option<(Step, String)>,
}

impl Default for Provisioner {
    fn default() -> Self {
        Provisioner::new()
    }
}

impl Provisioner {
    pub fn new() -> Provisioner {
        Provisioner {
            at: 0,
            in_flight: None,
            done: Provisioned::default(),
            known: Vec::new(),
            unexpected: 0,
            stall_after_ms: 30_000,
            stalled: None,
            refused: None,
        }
    }

    /// Tell it what is already in place — from `Identity`, or from a previous
    /// run in the same session.
    ///
    /// This is what makes a reload cheap: a page that reconnects has usually
    /// provisioned everything already, and re-registering a 721 KB delegate
    /// on every reload would be the whole cost of opening a tab.
    pub fn already(&mut self, step: Step) {
        if !self.known.contains(&step) {
            self.known.push(step);
        }
    }

    /// The next step to perform.
    ///
    /// Takes no clock: the moment a step went out is recorded by [`sent`], by
    /// the caller that actually sent it. A time passed here would be the time
    /// the step was CHOSEN, and a stall is measured from when it left.
    ///
    /// [`sent`]: Provisioner::sent
    ///
    /// Returns `None` while a step is in flight: one at a time, because the
    /// node answers on one connection and a second outstanding step would make
    /// the two acks tell-apart-able only by name — which is why the name is
    /// carried, but one at a time is still the simpler guarantee.
    pub fn next_step(&mut self) -> Option<Step> {
        if self.in_flight.is_some() {
            return None;
        }
        while self.at < Step::ALL.len() {
            let step = Step::ALL[self.at];
            if self.known.contains(&step) {
                self.done.steps.push((step, Did::AlreadyThere));
                self.at += 1;
                continue;
            }
            return Some(step);
        }
        None
    }

    /// Record what the page actually sent for the step it just took.
    ///
    /// Separate from `next_step` because only the caller knows the key: it
    /// framed the request.
    pub fn sent(&mut self, step: Step, named: &str, now_ms: u64) {
        self.in_flight = Some((step, named.to_string(), now_ms));
        self.stalled = None;
    }

    /// An ack arrived.
    ///
    /// It completes the step in flight only if it ANSWERS it — same step, same
    /// name. Anything else is counted and changes nothing, because believing
    /// it records an install that did not happen.
    pub fn on_ack(&mut self, ack: &AckKind) {
        let Some((step, named, _)) = self.in_flight.clone() else {
            self.unexpected += 1;
            return;
        };
        if !step.answered_by(&named, ack) {
            self.unexpected += 1;
            return;
        }
        self.done.steps.push((step, Did::Installed));
        self.at += 1;
        self.in_flight = None;
    }

    /// Nothing has answered for too long.
    ///
    /// Returns the stalled step the first time it notices. The step stays in
    /// flight and may be RE-ISSUED — every step here is idempotent, which is
    /// exactly the property that makes a re-issue safe and exactly the
    /// property that made matching acks by kind dangerous.
    pub fn tick(&mut self, now_ms: u64) -> Option<Step> {
        let (step, _, at) = self.in_flight.as_ref()?;
        if now_ms.saturating_sub(*at) < self.stall_after_ms {
            return None;
        }
        if self.stalled == Some(*step) {
            return None;
        }
        self.stalled = Some(*step);
        Some(*step)
    }

    /// Send the stalled step again. Safe because every step is idempotent.
    pub fn reissue(&mut self) -> Option<Step> {
        let (step, _, _) = self.in_flight.as_ref()?;
        let step = *step;
        self.in_flight = None;
        self.stalled = None;
        Some(step)
    }

    pub fn stalled(&self) -> Option<Step> {
        self.stalled
    }

    /// The node refused the step in flight, in its own words.
    ///
    /// Provisioning STOPS. The steps are ordered because each depends on the
    /// last, so carrying on would install a contract for a delegate that is
    /// not there — and the failure would surface at the first write.
    ///
    /// `said` is kept for DISPLAY only. It is the node's wording, so nothing
    /// branches on it: see [`crate::Refused`].
    pub fn on_refused(&mut self, said: &str) {
        let step = self.in_flight.as_ref().map(|(s, _, _)| *s);
        if let Some(step) = step {
            self.refused = Some((step, said.to_string()));
        }
        self.in_flight = None;
        self.at = Step::ALL.len();
    }

    /// Which step was refused, and what the node said. Display only.
    pub fn refused(&self) -> Option<(Step, &str)> {
        self.refused.as_ref().map(|(s, w)| (*s, w.as_str()))
    }

    pub fn result(&self) -> &Provisioned {
        &self.done
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}
