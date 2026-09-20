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
/// An order, not a set: the delegate has to exist before it can be asked
/// anything, and it has to be asked before installing over it is safe.
///
/// **There is no contract-PUT step, and there was never a wire exchange for
/// one.** An earlier plan had the page put the Block and Register contracts
/// itself. It does not: the delegate is handed their CODE by `Install` and
/// mints the containers itself, at the moment it has something to store
/// (`engine-delegate/src/entry.rs`). Modelling a client-side put invented two
/// acks the node never sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The engine delegate, ~721 KB — the one that is always chunked.
    Delegate,
    /// Ask the delegate what it already has.
    ///
    /// The only step that READS, and the reason the others are safe. An
    /// unprovisioned delegate answers `Identity` with a seq of 0 and a zero
    /// root, which is byte-identical to a healthy engine nobody has written
    /// to yet — so `head_writable` is what separates them.
    Ask,
    /// Hand over the contract code, the Register's parameters, and a freshly
    /// minted signing key.
    ///
    /// **Taken only when `Ask` said the delegate cannot write a head.**
    /// `Install` overwrites `SIGNING_KEY` unconditionally, and the Register
    /// instance is derived from a keyset — so installing over a provisioned
    /// delegate mints a new key, changes the head's contract id, and orphans
    /// everything written under the old one. Running it on every page load,
    /// which was the previous plan, would have lost the data on every reload
    /// while reporting success.
    Install,
}

impl Step {
    /// Does this ack answer THIS step, for the thing this step actually sent?
    ///
    /// **Matched by NAME, never by kind alone** — a duplicate ack of one step
    /// completed the next when only the kind was compared, reporting an
    /// artefact installed that had never been sent.
    ///
    /// Only `Delegate` is answered by an ack at all. `Ask` is answered by a
    /// `Reply::Identity` ([`Provisioner::on_identity`]), and `Install`
    /// produces no reply of its own — it is confirmed by the `Ask` that
    /// follows it, which is a stronger fact than an echo would be.
    pub fn answered_by(self, named: &str, ack: &AckKind) -> bool {
        match (self, ack) {
            (Step::Delegate, AckKind::Registered(k)) => k == named,
            _ => false,
        }
    }

    /// Whether this step's completion is CONFIRMED or merely acknowledged.
    ///
    /// `Identity` proves the delegate is registered and says whether it can
    /// sign a head — an unregistered delegate cannot answer at all. So both
    /// of the steps that change anything are confirmed by asking, and
    /// provisioning never rests on a node's word about its own store.
    pub fn confirmed_by_asking(self) -> bool {
        matches!(self, Step::Delegate | Step::Install)
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
        self.did(Step::Delegate).is_some() && self.did(Step::Install).is_some()
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
    /// The delegate is registered. Set by its ack, or by [`already`].
    ///
    /// [`already`]: Provisioner::already
    registered: bool,
    /// What the last `Ask` answered, or `None` when nothing has been asked
    /// since the last thing that could have changed it. This is the whole
    /// state machine: `None` means ask, `Some(false)` means install,
    /// `Some(true)` means done.
    writable: Option<bool>,
    /// How many `Install`s have gone out.
    installs: usize,
    /// The cap on them. An `Install` produces no reply, so a delegate that
    /// accepts one and stays unwritable would otherwise loop Ask→Install for
    /// ever, each pass costing the contract code. Two attempts, then the run
    /// reports itself stuck rather than spending the uplink silently.
    pub max_installs: usize,
    /// Every step was taken that could be, and it is still not writable.
    exhausted: bool,
    /// What the step in flight PUT on the wire, so its ack can be matched to
    /// it rather than to whatever arrived next.
    in_flight: Option<(Step, String, u64)>,
    done: Provisioned,
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
            registered: false,
            writable: None,
            installs: 0,
            max_installs: 2,
            exhausted: false,
            in_flight: None,
            done: Provisioned::default(),
            unexpected: 0,
            stall_after_ms: 30_000,
            stalled: None,
            refused: None,
        }
    }

    /// Tell it the delegate is already registered — from a previous run in
    /// this same session.
    ///
    /// This is what makes a reload cheap: re-registering a 721 KB delegate on
    /// every reload would be most of the cost of opening a tab. It is only
    /// ever said of `Delegate`; whether the delegate is PROVISIONED is never
    /// asserted from memory, because the delegate itself will say.
    pub fn already_registered(&mut self) {
        if !self.registered {
            self.registered = true;
            self.done.steps.push((Step::Delegate, Did::AlreadyThere));
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
        if self.in_flight.is_some() || self.refused.is_some() || self.exhausted {
            return None;
        }
        if !self.registered {
            return Some(Step::Delegate);
        }
        match self.writable {
            // Nothing has been asked since the last change: ask.
            None => Some(Step::Ask),
            // It answered, and it cannot write a head.
            Some(false) => {
                if self.installs < self.max_installs {
                    Some(Step::Install)
                } else {
                    self.exhausted = true;
                    None
                }
            }
            // Provisioned. Nothing left to do, and nothing to re-send.
            Some(true) => None,
        }
    }

    /// Record what the page actually sent for the step it just took.
    ///
    /// Separate from `next_step` because only the caller knows the key: it
    /// framed the request.
    pub fn sent(&mut self, step: Step, named: &str, now_ms: u64) {
        if step == Step::Install {
            // `Install` is answered by nothing. Putting it in flight would
            // wait out the stall timeout on every single provisioning run.
            // It is confirmed by the `Ask` that follows — so what this
            // records is that the question must be put again.
            self.installs += 1;
            self.writable = None;
            self.stalled = None;
            return;
        }
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
        debug_assert_eq!(step, Step::Delegate, "only Delegate is answered by an ack");
        self.registered = true;
        self.done.steps.push((step, Did::Installed));
        self.in_flight = None;
    }

    /// The delegate answered `Identity`.
    ///
    /// `head_writable` is the delegate's own report that it holds everything
    /// a head write needs. It is the ONLY thing that completes provisioning,
    /// and it is a fact about the store rather than an acknowledgement that a
    /// message was received.
    ///
    /// Answering at all also proves the delegate is registered, so a session
    /// that reconnects can ask first and skip the 721 KB.
    pub fn on_identity(&mut self, head_writable: bool) {
        if matches!(self.in_flight, Some((Step::Ask, _, _))) {
            self.in_flight = None;
            self.stalled = None;
        }
        if !self.registered {
            self.registered = true;
            self.done.steps.push((Step::Delegate, Did::AlreadyThere));
        }
        self.writable = Some(head_writable);
        if head_writable && self.did(Step::Install).is_none() {
            // Installed just now, or already there before this run started.
            let did = if self.installs > 0 {
                Did::Installed
            } else {
                Did::AlreadyThere
            };
            self.done.steps.push((Step::Install, did));
        }
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
        self.exhausted = true;
    }

    /// Which step was refused, and what the node said. Display only.
    pub fn refused(&self) -> Option<(Step, &str)> {
        self.refused.as_ref().map(|(s, w)| (*s, w.as_str()))
    }

    /// What a step did, if it is done. Delegates to the result.
    pub fn did(&self, step: Step) -> Option<Did> {
        self.done.did(step)
    }

    /// Provisioning is finished and the delegate says so.
    pub fn provisioned(&self) -> bool {
        self.writable == Some(true)
    }

    /// Every step was taken and it is STILL not writable. Distinct from
    /// stalled (nothing answered) and refused (the node said no): here the
    /// node accepted everything and the delegate still cannot sign.
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }

    pub fn result(&self) -> &Provisioned {
        &self.done
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }
}
