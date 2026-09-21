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
        /// The head EVERY page was read at — a restart throws away a load
        /// whose pages disagree. What a delta from this range may start FROM
        /// (sdk#142).
        at: protocol::At,
    },
    /// The tree moved under this load, or it finished behind what this
    /// client already knows. Ask again from the top of the range.
    Restart { lo: Vec<u8>, hi: Vec<u8> },
    /// Nothing to do: the load ended, or there was no such load.
    Nothing,
}

struct Load {
    lo: Vec<u8>,
    hi: Vec<u8>,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    at_ms: u64,
    /// The head the FIRST page of this load was read at. Every later page
    /// must match it, or the range would be stitched from two trees.
    at: Option<protocol::At>,
    /// How many times this load has been restarted because the tree moved
    /// under it. Bounded: a range somebody is writing to continuously would
    /// otherwise restart for ever.
    restarts: u32,
}

pub struct Loads {
    open: std::collections::BTreeMap<u64, Load>,
    ended: Vec<(u64, Ended)>,
    next: u64,
    /// The most rows one load may accumulate. A range that will not fit is
    /// ENDED rather than recorded short: recorded short, later reads would
    /// be answered "empty" with confidence.
    pub max_rows: usize,
    /// How many times a load may restart because the tree moved under it.
    ///
    /// One delegate context serves every tab (F47), so another tab writing
    /// between two pages is ordinary. Restarting is right; restarting for
    /// ever is not, so a range under continuous write ends `Unavailable`
    /// rather than spinning.
    pub max_restarts: u32,
    /// The highest head seq this client has seen anywhere.
    ///
    /// A completed load read at an OLDER seq is not recorded: it would move
    /// the copy backwards, and a reader would be answered confidently from a
    /// tree that has already been superseded.
    known_seq: u64,
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
    /// Pages refused because they held keys outside their load's range.
    pub foreign_pages: usize,
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
            max_restarts: 8,
            known_seq: 0,
            budget_ms: 30_000,
            done: std::collections::BTreeSet::new(),
            foreign_pages: 0,
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
        let id = self.take_id();
        self.open.insert(
            id,
            Load {
                lo: lo.to_vec(),
                hi: hi.to_vec(),
                rows: Vec::new(),
                at_ms: now_ms,
                at: None,
                restarts: 0,
            },
        );
        Some((id, true))
    }

    /// A request id from THIS session's one counter (sdk#166).
    ///
    /// Every kind of read shares it — a range load and a `ChangesSince` are
    /// both just `req_id` on the wire, and the engine parks every read in one
    /// map keyed by the id alone. Two counters each starting at 1 made one
    /// tab's first load and first refresh the same read: one was answered,
    /// the other waited out its budget. `Refresh` takes its ids from here.
    pub fn take_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// Forget which spans have been loaded.
    ///
    /// Called when a read SUCCEEDS: the chain it was walking is finished, and
    /// the next read starts its own. Without this, a span loaded for one read
    /// would make a later read of it report no-progress instead of waiting.
    pub fn read_succeeded(&mut self) {
        self.done.clear();
    }

    /// The highest head seq this client knows about, from anywhere.
    ///
    /// Told, not inferred: `Identity` and every `Delta` name one, and a load
    /// is judged against the newest thing this client has heard rather than
    /// against its own history.
    pub fn note_seq(&mut self, seq: u64) {
        self.known_seq = self.known_seq.max(seq);
    }

    pub fn known_seq(&self) -> u64 {
        self.known_seq
    }

    pub fn on_page(
        &mut self,
        id: u64,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        cursor: Option<Vec<u8>>,
        at: protocol::At,
    ) -> Page {
        self.note_seq(at.seq);
        let max_restarts = self.max_restarts;
        let known_seq = self.known_seq;
        let Some(load) = self.open.get_mut(&id) else {
            // A page for a load nobody is waiting on: a duplicate, or one
            // that already timed out. Applying it would record a range as
            // loaded on the strength of an answer whose question is gone.
            return Page::Nothing;
        };
        // A PAGE OF SOMEBODY ELSE'S RANGE (sdk#166). Reads are keyed by the id
        // alone, and another tab's first load is also id 1: this load can be
        // handed THAT range's rows. Accepted, the range was recorded as loaded
        // with none of its own rows — "everything here not in this list is
        // absent", said of a list that holds nothing of here: a wrong empty,
        // shown as right. A range is never recorded on rows that are not in
        // it; the load ends Unavailable, which is honest, and is re-asked.
        let (lo, hi) = (load.lo.clone(), load.hi.clone());
        let outside = |k: &[u8]| k < lo.as_slice() || k >= hi.as_slice();
        if entries.iter().any(|(k, _)| outside(k)) || cursor.as_deref().is_some_and(outside) {
            self.open.remove(&id);
            self.ended.push((id, Ended::Unavailable));
            self.foreign_pages += 1;
            return Page::Nothing;
        }
        let Some(load) = self.open.get_mut(&id) else {
            unreachable!("checked above");
        };
        // THE TREE MOVED UNDER THIS LOAD.
        //
        // Every page must have been read at the same head, or what is
        // assembled is a range stitched from two trees — rows from before a
        // write and rows from after it, recorded as one coherent snapshot.
        // One delegate context serves every tab (F47), so this is ordinary.
        match load.at {
            None => load.at = Some(at),
            Some(first) if first.root != at.root => {
                load.restarts += 1;
                if load.restarts > max_restarts {
                    // Under continuous write. Ending is honest; restarting
                    // for ever is a page that never fills and never says why.
                    self.open.remove(&id);
                    self.ended.push((id, Ended::Unavailable));
                    return Page::Nothing;
                }
                // Throw away what was gathered: half of it is from the old
                // tree. Start again from the top of the range.
                let (lo, hi) = (load.lo.clone(), load.hi.clone());
                load.rows.clear();
                load.at = None;
                return Page::Restart { lo, hi };
            }
            Some(_) => {}
        }
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
        // A load that finished at an OLDER head than this client already
        // knows about would move the copy backwards. Not recorded; asked
        // again, bounded by the same restart budget.
        let finished_at = load.at.unwrap_or(at);
        if finished_at.seq < known_seq {
            let load = self.open.get_mut(&id).expect("checked above");
            load.restarts += 1;
            if load.restarts > max_restarts {
                self.open.remove(&id);
                self.ended.push((id, Ended::Unavailable));
                return Page::Nothing;
            }
            let (lo, hi) = (load.lo.clone(), load.hi.clone());
            load.rows.clear();
            load.at = None;
            return Page::Restart { lo, hi };
        }

        let load = self.open.remove(&id).expect("checked above");
        self.ended.push((id, Ended::Loaded));
        self.done.insert((load.lo.clone(), load.hi.clone()));
        Page::Complete {
            lo: load.lo,
            hi: load.hi,
            rows: load.rows,
            at: finished_at,
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
