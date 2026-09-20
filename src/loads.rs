//! Ranges asked for and not yet answered.
//!
//! A read that cannot be answered from the local copy is not a failure — it
//! is a range nobody has fetched. The recovery is to fetch it, and this is
//! the bookkeeping that makes the recovery a fact rather than a hope: which
//! range each request was for, which pages have come back, when to stop
//! waiting, and which reads to wake.
//!
//! It lives here, and not in the browser crate, so it can be tested on a
//! machine rather than only in a tab. The first version of this recovery was
//! a comment in JavaScript that awaited a resolved promise and asked again —
//! before anything had been requested and before any message could have
//! arrived — and no test could see it, because the only test was a fake that
//! scripted the second answer.

/// How a load ended, for the reads parked on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The range was received in full and recorded.
    Loaded,
    /// It will not arrive. The engine could not answer, nothing answered
    /// within the bound, or it is larger than this client will hold.
    Unavailable,
}

/// What the caller should do with a page that just arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    /// Ask again from `after`, under the same ticket. The range is not
    /// exhausted, and the copy may only record `[lo, hi)` as loaded once it
    /// is — "everything here and not in this list is absent" is the fact a
    /// later read rests on.
    More {
        lo: Vec<u8>,
        hi: Vec<u8>,
        after: Vec<u8>,
    },
    /// Record these rows as the whole of `[lo, hi)`.
    Complete {
        lo: Vec<u8>,
        hi: Vec<u8>,
        rows: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// Nothing to do: the load ended, or there was no such load.
    Nothing,
}

struct Load {
    lo: Vec<u8>,
    hi: Vec<u8>,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    at_ms: u64,
}

pub struct Loads {
    open: std::collections::BTreeMap<u64, Load>,
    ended: Vec<(u64, Ended)>,
    next: u64,
    /// The most rows one load may accumulate. A range that will not fit is
    /// ENDED rather than recorded short: recorded short, later reads would
    /// be answered "empty" with confidence.
    pub max_rows: usize,
    /// How long a load may go unanswered. An unbounded wait is a spinner
    /// that never stops; the node's own tail can be ~60 s (F20), so this is
    /// a bound on the WAIT and not a claim about the network.
    pub budget_ms: u64,
    /// Spans completed and not yet asked for again.
    ///
    /// A cold read is a CHAIN, not one hop: `scan` reads its domain's schema
    /// before its rows, so the first `NotLoaded` names the schema's key and
    /// the next names the range. Re-asking must therefore be allowed to go
    /// round more than once — and the thing that must not happen is going
    /// round WITHOUT PROGRESS. Asking for a span that was just loaded is
    /// exactly that, and it is a fact this registry can see and a caller
    /// cannot.
    done: std::collections::BTreeSet<(Vec<u8>, Vec<u8>)>,
}

impl Default for Loads {
    fn default() -> Self {
        Loads::new()
    }
}

impl Loads {
    pub fn new() -> Loads {
        Loads {
            open: std::collections::BTreeMap::new(),
            ended: Vec::new(),
            next: 1,
            max_rows: 50_000,
            budget_ms: 30_000,
            done: std::collections::BTreeSet::new(),
        }
    }

    /// The ticket for `[lo, hi)`, and whether a REQUEST must be sent for it.
    ///
    /// **One request per range.** Several components reading the same
    /// unloaded range on one frame is the ordinary case, not a rarity, and a
    /// request each would multiply the first paint of every screen by the
    /// number of things on it.
    /// Returns `None` when this span was ALREADY LOADED and is being asked
    /// for again — a read that is going round without progress. Loading it a
    /// second time would answer exactly as it did the first, so the caller is
    /// told rather than sent round again.
    pub fn want(&mut self, lo: &[u8], hi: &[u8], now_ms: u64) -> Option<(u64, bool)> {
        if self.done.contains(&(lo.to_vec(), hi.to_vec())) {
            return None;
        }
        if let Some((id, _)) = self.open.iter().find(|(_, l)| l.lo == lo && l.hi == hi) {
            return Some((*id, false));
        }
        let id = self.next;
        self.next += 1;
        self.open.insert(
            id,
            Load {
                lo: lo.to_vec(),
                hi: hi.to_vec(),
                rows: Vec::new(),
                at_ms: now_ms,
            },
        );
        Some((id, true))
    }

    /// Forget which spans have been loaded.
    ///
    /// Called when a read SUCCEEDS: the chain it was walking is finished, and
    /// the next read starts its own. Without this, a span loaded for one read
    /// would make a later read of it report no-progress instead of waiting.
    pub fn read_succeeded(&mut self) {
        self.done.clear();
    }

    pub fn on_page(
        &mut self,
        id: u64,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        cursor: Option<Vec<u8>>,
    ) -> Page {
        let Some(load) = self.open.get_mut(&id) else {
            // A page for a load nobody is waiting on: a duplicate, or one
            // that already timed out. Applying it would record a range as
            // loaded on the strength of an answer whose question is gone.
            return Page::Nothing;
        };
        load.rows.extend(entries);
        if load.rows.len() > self.max_rows {
            self.open.remove(&id);
            self.ended.push((id, Ended::Unavailable));
            return Page::Nothing;
        }
        if let Some(after) = cursor {
            return Page::More {
                lo: load.lo.clone(),
                hi: load.hi.clone(),
                after,
            };
        }
        let load = self.open.remove(&id).expect("checked above");
        self.ended.push((id, Ended::Loaded));
        self.done.insert((load.lo.clone(), load.hi.clone()));
        Page::Complete {
            lo: load.lo,
            hi: load.hi,
            rows: load.rows,
        }
    }

    /// The engine said it could not answer this read.
    ///
    /// The range is NOT recorded as loaded. An empty page here would say
    /// "this range is empty", which is a wrong answer wearing the shape of a
    /// right one.
    pub fn on_unavailable(&mut self, id: u64) {
        if self.open.remove(&id).is_some() {
            self.ended.push((id, Ended::Unavailable));
        }
    }

    /// End every load that has outlived the budget. The reads parked on them
    /// get a fact instead of a wait.
    pub fn time_out(&mut self, now_ms: u64) -> Vec<u64> {
        let over: Vec<u64> = self
            .open
            .iter()
            .filter(|(_, l)| now_ms.saturating_sub(l.at_ms) > self.budget_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in &over {
            self.open.remove(id);
            self.ended.push((*id, Ended::Unavailable));
        }
        over
    }

    /// Loads that ended since this was last asked.
    pub fn take_ended(&mut self) -> Vec<(u64, Ended)> {
        std::mem::take(&mut self.ended)
    }

    pub fn in_flight(&self) -> usize {
        self.open.len()
    }

    /// The request that loads `[lo, hi)`, continuing after `after`.
    ///
    /// **The only place a loader's page size is chosen.** It used to be
    /// picked at each call site, and both sites picked `0` — which the shell
    /// clamps to ONE ENTRY, so a range of N rows loaded in N round trips.
    /// A test could assert the constant and still not notice, because
    /// asserting a parameter is not the same as counting what crossed.
    ///
    /// One function, so there is one thing to get right and one thing to
    /// test.
    pub fn range_request(
        req_id: u64,
        lo: &[u8],
        hi: &[u8],
        after: Option<Vec<u8>>,
    ) -> protocol::Request {
        protocol::Request::Range {
            req_id,
            lo: protocol::Bound::Included(lo.to_vec()),
            hi: protocol::Bound::Excluded(hi.to_vec()),
            reverse: false,
            after,
            max_entries: protocol::MAX_PAGE_ENTRIES,
        }
    }

    /// The range a ticket is for. For a caller that must re-send it.
    pub fn range_of(&self, id: u64) -> Option<(&[u8], &[u8])> {
        self.open.get(&id).map(|l| (&l.lo[..], &l.hi[..]))
    }
}
