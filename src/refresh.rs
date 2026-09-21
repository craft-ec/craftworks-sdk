//! Asking what changed, and knowing which answer belongs to which domain.
//!
//! A binding that only READS can never show new data: the local copy answers
//! a loaded range from cache, so its root never moves and "has anything
//! changed" is always no. Something has to ASK — and asking is a decision
//! with state behind it: which root this client last saw, which questions
//! are outstanding, and whether the answer moved anything.
//!
//! It lives here, in the SDK, and not in the browser crate. The bookkeeping
//! for `Loads` was moved here for the same reason and it was the right call
//! both times: a test that wraps a FAKE session written in JavaScript
//! compiles the Rust and never runs it. What must be tested is this, against
//! a real engine.

/// What to do with an answer that has arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Apply these changes and stand on this root. The domain is named so a
    /// caller knows whose rows moved.
    Delta {
        domain: String,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        new_root: [u8; 32],
        /// Whether anything actually moved. An empty delta must NOT re-run a
        /// binding: every screen would re-render on every notification about
        /// anything.
        moved: bool,
    },
    /// The engine cannot compute a delta for this domain. Forget the range
    /// and load it again.
    Reload { domain: String },
    /// An answer to a question this client did not ask. Counted, never
    /// applied: applying it would move the copy's root on a stranger's word.
    NotOurs,
}

/// Which root this client last saw, per domain, and what is outstanding.
pub struct Refresh {
    /// **The client owns this, not the engine.** It is what makes a missed
    /// notification recoverable: however many were dropped, the gap closes in
    /// one call.
    seen: std::collections::BTreeMap<String, [u8; 32]>,
    in_flight: std::collections::BTreeMap<u64, String>,
    changed: std::collections::BTreeSet<String>,
    /// Answers for questions nobody asked.
    pub unasked: usize,
}

impl Default for Refresh {
    fn default() -> Self {
        Refresh::new()
    }
}

impl Refresh {
    pub fn new() -> Refresh {
        Refresh {
            seen: std::collections::BTreeMap::new(),
            in_flight: std::collections::BTreeMap::new(),
            changed: std::collections::BTreeSet::new(),
            unasked: 0,
        }
    }

    /// The request that asks what changed in `domain` since this client last
    /// looked — or `None` if a question about it is already outstanding.
    ///
    /// **One in flight per domain.** Several notifications about one domain
    /// is the ordinary case on a tree somebody is writing to, and a request
    /// each would multiply its traffic by how often it is written.
    ///
    /// `req_id` is the caller's, from the session's ONE counter
    /// (`Loads::take_id`): this used to keep its own, starting at 1 like the
    /// loads' — so a tab's first refresh and first load were the same read to
    /// the engine, and one of them was never answered (sdk#166).
    pub fn ask(&mut self, req_id: u64, domain: &str, lo: &[u8], hi: &[u8]) -> Option<protocol::Request> {
        if self.in_flight.values().any(|d| d == domain) {
            return None;
        }
        self.in_flight.insert(req_id, domain.to_string());
        Some(protocol::Request::ChangesSince {
            req_id,
            from: self.seen.get(domain).copied().unwrap_or([0u8; 32]),
            lo: protocol::Bound::Included(lo.to_vec()),
            hi: protocol::Bound::Excluded(hi.to_vec()),
            max_entries: protocol::MAX_PAGE_ENTRIES,
        })
    }

    /// A delta arrived.
    ///
    /// **A delta with a `cursor` is a FIRST PAGE, not an answer** (sdk#140).
    /// The engine pages at 256 changes; applying page 1 and recording
    /// `new_root` as seen would leave everything after it stale under a root
    /// that claims to be current — and the next question starts FROM that
    /// root, where those changes are no longer changes, so nothing would ever
    /// fetch them. So it is answered the way `binding.rs` answers the same
    /// thing: too much changed to diff, reload the domain. Nothing of the
    /// page is applied and `seen` does not advance.
    ///
    /// `cursor` is a parameter, not left in the reply, because dropping it in
    /// a `..` is exactly how this happened.
    pub fn on_delta(
        &mut self,
        req_id: u64,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        cursor: Option<Vec<u8>>,
        new_root: [u8; 32],
    ) -> Answer {
        let Some(domain) = self.in_flight.remove(&req_id) else {
            self.unasked += 1;
            return Answer::NotOurs;
        };
        if cursor.is_some() {
            return self.reload(domain);
        }
        let moved = !changes.is_empty();
        self.seen.insert(domain.clone(), new_root);
        if moved {
            self.changed.insert(domain.clone());
        }
        Answer::Delta {
            domain,
            changes,
            new_root,
            moved,
        }
    }

    pub fn on_full_reload(&mut self, req_id: u64) -> Answer {
        let Some(domain) = self.in_flight.remove(&req_id) else {
            self.unasked += 1;
            return Answer::NotOurs;
        };
        self.reload(domain)
    }

    /// The domain has to be read again in full.
    fn reload(&mut self, domain: String) -> Answer {
        // The root this client thought it stood on is no longer a root the
        // engine can reconcile from, so it is forgotten rather than kept as
        // a starting point that will fail the same way next time.
        self.seen.remove(&domain);
        self.changed.insert(domain.clone());
        Answer::Reload { domain }
    }

    /// A load of the WHOLE domain completed, every page read at `root`.
    ///
    /// **This is what makes the delta path run at all** (sdk#142). `seen` was
    /// written only by a delta, and a delta could only be asked for FROM
    /// `seen` — so every question went out from the zero root, the engine
    /// made three fetches of a block that cannot exist and answered
    /// `FullReloadRequired`, and every change notification reloaded the
    /// whole domain. A completed load is the other fact that establishes a
    /// root: the copy holds the domain as it was AT that root.
    ///
    /// The root is the one the pages were answered at — never the `new_root`
    /// a `FullReloadRequired` named, since the tree can move between that
    /// refusal and the reload finishing.
    pub fn on_loaded(&mut self, domain: &str, root: [u8; 32]) {
        self.seen.insert(domain.to_string(), root);
    }

    /// A question that will never be answered: drop it so the domain can be
    /// asked about again.
    pub fn give_up(&mut self, req_id: u64) -> Option<String> {
        self.in_flight.remove(&req_id)
    }

    /// This client changed keys itself. Any bound domain containing one of
    /// them has moved, whatever the engine says later.
    pub fn note_local<'a>(
        &mut self,
        keys: impl Iterator<Item = &'a Vec<u8>>,
        bound: impl Fn(&[u8]) -> Option<String>,
    ) {
        for k in keys {
            if let Some(d) = bound(k) {
                self.changed.insert(d);
            }
        }
    }

    /// Domains whose rows moved since this was last asked. Drains.
    pub fn take_changed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.changed).into_iter().collect()
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// The root this client last saw for a domain, if it has seen one.
    pub fn seen(&self, domain: &str) -> Option<[u8; 32]> {
        self.seen.get(domain).copied()
    }
}
