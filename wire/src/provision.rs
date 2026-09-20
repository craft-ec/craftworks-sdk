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

    /// Which ack answers this step, so a reply can be matched to what asked
    /// for it rather than to whatever arrived next.
    pub fn expects(self) -> AckKind {
        match self {
            Step::Delegate => AckKind::Registered,
            Step::BlockContract | Step::RegisterContract => AckKind::Put,
            // The engine answers a key over the delegate, not the client API.
            Step::Key => AckKind::Ok,
        }
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
    waiting: bool,
    done: Provisioned,
    /// Steps the caller said are already in place, so they are not redone.
    known: Vec<Step>,
    /// Acks that did not match what was asked for. Counted, because a node
    /// answering something nobody asked is worth seeing rather than ignoring.
    pub unexpected: usize,
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
            waiting: false,
            done: Provisioned::default(),
            known: Vec::new(),
            unexpected: 0,
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

    /// The next step to perform, or `None` when there is nothing to do.
    ///
    /// Returns `None` while a step is in flight: one at a time.
    pub fn next_step(&mut self) -> Option<Step> {
        if self.waiting {
            return None;
        }
        while self.at < Step::ALL.len() {
            let step = Step::ALL[self.at];
            if self.known.contains(&step) {
                // Recorded as ALREADY THERE rather than skipped silently —
                // a run that says nothing about a step is a run that cannot
                // be checked for completeness.
                self.done.steps.push((step, Did::AlreadyThere));
                self.at += 1;
                continue;
            }
            self.waiting = true;
            return Some(step);
        }
        None
    }

    /// An ack arrived.
    ///
    /// Matched against what the step EXPECTS. An ack of the wrong kind does
    /// not advance anything: it is counted and the step stays in flight,
    /// because believing it would record an install that did not happen.
    pub fn on_ack(&mut self, kind: AckKind) {
        if !self.waiting {
            self.unexpected += 1;
            return;
        }
        let step = Step::ALL[self.at];
        if kind != step.expects() {
            self.unexpected += 1;
            return;
        }
        self.done.steps.push((step, Did::Installed));
        self.at += 1;
        self.waiting = false;
    }

    /// The node refused the step in flight.
    ///
    /// Provisioning STOPS. The steps are ordered because each depends on the
    /// last, so carrying on would install a contract for a delegate that is
    /// not there — and the failure would surface at the first write, a long
    /// way from here.
    pub fn on_refused(&mut self) {
        self.waiting = false;
        self.at = Step::ALL.len();
    }

    /// What has been done so far.
    pub fn result(&self) -> &Provisioned {
        &self.done
    }

    pub fn in_flight(&self) -> bool {
        self.waiting
    }
}
