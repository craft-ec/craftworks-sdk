//! THE PAGE'S STORE (READ-STATE, design B): reads WALK the tree from the one
//! owner's root over the blocks the page holds. No rows are copied, no range is
//! recorded, no root is kept here.
//!
//! # Why a walk and not a copy
//!
//! The row copy this replaces kept a second form of three facts the page
//! already owns: the head (the engine's), the tree (its blocks) and which
//! parts of it had been read (`loaded`/`stale`). Every defect of 2026-09-23
//! was two of those disagreeing (READ-STATE, § The measured case). Blocks are
//! content-addressed, so a cached block is never stale and nothing here is
//! ever invalidated.
//!
//! # A read that needs a block the page does not hold
//!
//! The walk stops, and the engine is asked to read the same range AT THE SAME
//! ROOT ([`Server::fetch`]): its parked read fetches through page-io on the
//! RTO estimator, and it always ENDS — answered, or given up within its rounds
//! and GETs. That read is the call's TICKET. The caller (`Db` through the
//! session) returns `NOT_LOADED` with it, the page waits for it to end
//! ([`PageStore::take_ended`]), and then RESUMES it ([`PageStore::resume`]):
//! the next call's walks are pinned to the ticket's root. So a chain of hops
//! (a schema, then its rows) converges on ONE immutable tree and never chases
//! a head that keeps moving (READ-STATE inv. 2; the architect's chase).
//!
//! # Writes
//!
//! Through [`CachedStore`], the outbox, handed to the engine AT ONCE
//! ([`PageStore::sync`]): the door is synchronous in the page. What the
//! engine refuses `Busy` (a commit is in flight) and what the window holds is
//! in no root yet — the INTERIM overlay ([`Copy::unaccepted`]), removed by R-b
//! when the engine owns the queue.
//!
//! [`Server::fetch`]: page::server::Server::fetch
//! [`Copy::unaccepted`]: crate::copy::Copy::unaccepted

use crate::cached_store::CachedStore;
use crate::store::{Delta, Edit, IdWidth, Read, Reads, RowState, Store, StoreError};
use engine::read::{DeltaSpec, ReadResult, ScanSpec, Walk, Walked};
use freenet_prolly::Cid;
use page::server::{Fetched, Host};
use std::collections::BTreeMap;
use std::ops::Bound;

/// Rows in key order, as a walk answers them.
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// How a ticket ended: the code a parked read is woken with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The engine's read at the ticket's root finished: walk again, pinned.
    Loaded,
    /// A block could not be had: not "absent", and never answered as empty.
    Unavailable,
    /// Nothing ended it within [`TICKET_LIFE_MS`].
    NotAnswering,
}

impl Ended {
    /// The stable code that crosses to JavaScript.
    pub fn code(self) -> &'static str {
        match self {
            Ended::Loaded => "LOADED",
            Ended::Unavailable => "UNAVAILABLE",
            Ended::NotAnswering => "NOT_ANSWERING",
        }
    }
}

/// What a `Db` call came back as, once a refusal for want of blocks has been
/// given its ticket ([`PageStore::decide`]).
#[derive(Debug)]
pub enum Outcome<T> {
    /// It worked.
    Done(T),
    /// A walk stopped on blocks the page does not hold; the engine is reading
    /// them at the walk's root, and this ticket ends when it has.
    Wait(crate::db::DbError, u64),
    /// Nothing more is coming.
    Told(crate::db::DbError),
}

/// How long a ticket may stay open, and how long an ENDED one keeps its root
/// pinned for a resume that may never come (a read the app abandoned). A
/// ticket's root is a pin (READ-STATE § The block cache's bound), so it
/// expires.
pub const TICKET_LIFE_MS: u64 = 60_000;

/// Entries a walk asks for per page. A walk loops pages until its limit or the
/// range's end; this bounds only one step's working set.
const WALK_PAGE: usize = 4096;

/// A read that stopped on missing blocks, waiting for the engine's fetch.
#[derive(Debug, Clone)]
struct Ticket {
    /// The root its walk stopped at; `None` before the head was recovered
    /// (the engine reads at the head once it is).
    root: Option<Cid>,
    /// When it was made, or when it ended.
    at_ms: u64,
    ended: Option<Ended>,
}

/// A fetch asked for before there was a page to ask (the store exists from
/// the session's first line; the page from provisioning on).
type Deferred = (u64, Option<Cid>, freenet_prolly::range::Range);

/// The page's store: `Db` reads and writes through it, over a [`Host`].
pub struct PageStore<H: Host> {
    host: Option<H>,
    /// The write half (INTERIM in part: see the module docs).
    pub writes: CachedStore,
    now_ms: Box<dyn Fn() -> u64>,
    tickets: BTreeMap<u64, Ticket>,
    next_ticket: u64,
    /// Tickets that ended since the page last asked, in order.
    ended: Vec<(u64, Ended)>,
    /// The ticket the last failed read made, for the caller to hand out.
    last_ticket: Option<u64>,
    /// The root this call's walks are pinned to ([`PageStore::resume`]).
    pinned: Option<Cid>,
    deferred: Vec<Deferred>,
    /// Walks made — what a test and the cost measurement read.
    pub walks: u64,
    /// Why a ticket ended UNAVAILABLE, in the engine's words, until it is
    /// taken with its end ([`PageStore::why`]).
    why: BTreeMap<u64, String>,
    /// INTERIM (R-b): whether a read lays the writes the engine has not taken
    /// over its walk. Always on; off only as the model test's control, which
    /// must then see a read that misses its own write.
    pub interim_overlay: bool,
}

impl<H: Host> PageStore<H> {
    pub fn new(now_ms: Box<dyn Fn() -> u64>, writes_clock: Box<dyn Fn() -> u64>) -> Self {
        PageStore {
            host: None,
            writes: CachedStore::new(writes_clock),
            now_ms,
            tickets: BTreeMap::new(),
            next_ticket: 1,
            ended: Vec::new(),
            last_ticket: None,
            pinned: None,
            deferred: Vec::new(),
            walks: 0,
            why: BTreeMap::new(),
            interim_overlay: true,
        }
    }

    /// The page exists: reads walk its tree from now on, and any fetch asked
    /// for before it goes now.
    pub fn set_host(&mut self, host: H) {
        self.host = Some(host);
        for (ticket, root, range) in std::mem::take(&mut self.deferred) {
            self.host.as_mut().expect("just set").with_server(|s| s.fetch(ticket, root, range));
        }
        self.sync();
    }

    pub fn host(&self) -> Option<&H> {
        self.host.as_ref()
    }

    pub fn host_mut(&mut self) -> Option<&mut H> {
        self.host.as_mut()
    }

    /// Carry what the client produced to the page and what the page answered
    /// back: every write frame is handed to the engine NOW (the door is
    /// synchronous), every reply reaches the write half, and every fetch that
    /// ended ends its ticket.
    pub fn sync(&mut self) {
        let Some(h) = self.host.as_mut() else { return };
        for _ in 0..64 {
            let frames = self.writes.take_outbound();
            for f in &frames {
                h.client(f);
            }
            let replies = h.take_replies();
            for r in &replies {
                self.writes.on_inbound(r);
            }
            if frames.is_empty() && replies.is_empty() {
                break;
            }
        }
        let fetched = h.with_server(|s| s.take_fetched());
        let now = (self.now_ms)();
        for (id, how) in fetched {
            let how = match how {
                Fetched::Loaded => Ended::Loaded,
                Fetched::Unavailable(why) => {
                    self.why.insert(id, why);
                    Ended::Unavailable
                }
            };
            self.end(id, how, now);
        }
    }

    fn end(&mut self, id: u64, how: Ended, now: u64) {
        if let Some(t) = self.tickets.get_mut(&id) {
            if t.ended.is_none() {
                t.ended = Some(how);
                t.at_ms = now;
                self.ended.push((id, how));
            }
        }
    }

    /// Tickets that ended since the last call, in order. Drains.
    pub fn take_ended(&mut self) -> Vec<(u64, Ended)> {
        self.sync();
        std::mem::take(&mut self.ended)
    }

    /// Why this ended ticket did not deliver, if the engine said. Drains.
    pub fn why(&mut self, ticket: u64) -> Option<String> {
        self.why.remove(&ticket)
    }

    /// The ticket the last refused read made. Drains.
    pub fn take_ticket(&mut self) -> Option<u64> {
        self.last_ticket.take()
    }

    /// A parked read woke on `ticket`: the NEXT call's walks read at that
    /// ticket's root, so the chain ends on one tree. A ticket unknown, not
    /// ended, or made before the head was recovered pins nothing (the walk
    /// reads the head).
    pub fn resume(&mut self, ticket: u64) {
        self.pinned = match self.tickets.get(&ticket) {
            Some(Ticket { ended: Some(Ended::Loaded), root, .. }) => *root,
            _ => None,
        };
        self.tickets.remove(&ticket);
    }

    /// Pin this call's walks to `root` — what `resume` does for a ticket's
    /// root, for a caller that holds a root of its own (a control's copy).
    pub fn pin(&mut self, root: Cid) {
        self.pinned = Some(root);
    }

    /// THE DECISION, for every `Db` call a page makes: a `NotLoaded` is a walk
    /// that stopped on blocks this page does not hold, and this store has
    /// already asked the engine to read them at the walk's root — that read
    /// is the TICKET. A `NotLoaded` with no ticket, and anything else, is
    /// TOLD. The call's pin ends here, answered or not: the next call walks
    /// the head unless it is resumed.
    ///
    /// Here, not in the web `Session`, so something native can run it: the
    /// session is `#[wasm_bindgen]` in a `cdylib`, and a decision inline there
    /// is one only a fake session in JavaScript could reach (sdk#89).
    ///
    /// A write that had to READ before it could apply comes through here too:
    /// every read in `Db` happens before the single write that mutates, so a
    /// `NotLoaded` leaves the tree untouched and asking again repeats the
    /// attempt, not the effect.
    pub fn decide<T>(&mut self, r: Result<T, crate::db::DbError>) -> Outcome<T> {
        let ticket = self.take_ticket();
        self.unpin();
        match r {
            Ok(v) => Outcome::Done(v),
            Err(e) if e.code() == "NOT_LOADED" => match ticket {
                Some(t) => Outcome::Wait(e, t),
                None => Outcome::Told(e),
            },
            Err(e) => Outcome::Told(e),
        }
    }

    /// The call ended: its pin goes.
    pub fn unpin(&mut self) {
        self.pinned = None;
    }

    /// Tickets not yet ended, and ended ones never resumed: both expire at
    /// [`TICKET_LIFE_MS`] — an open one ENDS, named, and an ended one lets
    /// its root go.
    pub fn tick(&mut self, now: u64) {
        let open: Vec<u64> = self
            .tickets
            .iter()
            .filter(|(_, t)| t.ended.is_none() && now.saturating_sub(t.at_ms) >= TICKET_LIFE_MS)
            .map(|(id, _)| *id)
            .collect();
        for id in open {
            self.end(id, Ended::NotAnswering, now);
        }
        self.tickets.retain(|_, t| t.ended.is_none() || now.saturating_sub(t.at_ms) < TICKET_LIFE_MS);
    }

    /// Tickets still open (not ended). What the model asserts is zero at rest.
    pub fn open_tickets(&self) -> usize {
        self.tickets.values().filter(|t| t.ended.is_none()).count()
    }

    /// The root this page's walks read now: the pinned one, or the owner's
    /// (`Server::read_root`, the warm root). `None` before the head is
    /// recovered, or before there is a page.
    pub fn head(&mut self) -> Option<Cid> {
        if let Some(p) = self.pinned {
            return Some(p);
        }
        self.host.as_mut()?.with_server(|s| s.read_root())
    }

    /// A walk stopped: the engine reads `range` at `root`, and the read is
    /// this call's ticket.
    fn park(&mut self, root: Option<Cid>, range: freenet_prolly::range::Range) -> StoreError {
        let id = self.next_ticket;
        self.next_ticket += 1;
        let now = (self.now_ms)();
        self.tickets.insert(id, Ticket { root, at_ms: now, ended: None });
        self.last_ticket = Some(id);
        match self.host.as_mut() {
            Some(h) => h.with_server(|s| s.fetch(id, root, range)),
            None => self.deferred.push((id, root, range)),
        }
        self.sync();
        StoreError::NotLoaded
    }

    fn walk(&mut self, root: &Cid, w: &Walk) -> Walked {
        self.walks += 1;
        match self.host.as_mut() {
            Some(h) => h.with_server(|s| s.walk(root, w)),
            None => Walked::Need(Vec::new()),
        }
    }

    /// Every entry of `[lo, hi)` at `root`, in order, at most `limit`: pages
    /// looped. `Err` is the scan to fetch from where the walk stopped.
    fn walk_scan(&mut self, root: Cid, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Result<Rows, WalkStop> {
        let mut out: Rows = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        while out.len() < limit {
            let spec = ScanSpec {
                lo: Bound::Included(lo.to_vec()),
                hi: Bound::Excluded(hi.to_vec()),
                reverse,
                after: after.clone(),
                max_entries: (limit - out.len()).min(WALK_PAGE),
                max_bytes: usize::MAX,
            };
            match self.walk(&root, &Walk::Scan(Box::new(spec.clone()))) {
                Walked::Done(ReadResult::Page { entries, cursor, .. }) => {
                    out.extend(entries);
                    match cursor {
                        Some(c) => after = Some(c),
                        None => break,
                    }
                }
                Walked::Need(_) => return Err(WalkStop::Need((&spec).into())),
                Walked::Done(_) | Walked::Broken(_) => return Err(WalkStop::Unavailable),
            }
        }
        Ok(out)
    }

    /// The root to read, or the ticket to wait on when there is none yet.
    fn root_or_park(&mut self, lo: &[u8], hi: &[u8]) -> Result<Cid, StoreError> {
        match self.head() {
            Some(r) => Ok(r),
            None => Err(self.park(None, scan_range(lo, hi, 1))),
        }
    }

    /// What this key's OWN write is doing (the write half's answer).
    pub fn row_state_of(&self, key: &[u8]) -> RowState {
        self.writes.row_state(key)
    }
}

/// Why a walk did not answer.
enum WalkStop {
    Need(freenet_prolly::range::Range),
    Unavailable,
}

fn scan_range(lo: &[u8], hi: &[u8], max_entries: usize) -> freenet_prolly::range::Range {
    freenet_prolly::range::Range {
        lo: Bound::Included(lo.to_vec()),
        hi: Bound::Excluded(hi.to_vec()),
        reverse: false,
        after: None,
        max_entries,
        max_bytes: usize::MAX,
    }
}

/// The key just after `key`: `[key, next(key))` holds exactly `key`.
fn next_key(key: &[u8]) -> Vec<u8> {
    let mut k = key.to_vec();
    k.push(0);
    k
}

impl<H: Host> Reads for PageStore<H> {
    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        self.sync();
        // INTERIM: removed by R-b (READ-STATE § queue) — a write the engine
        // has not taken shows over the tree.
        if self.interim_overlay {
            if let Some((_, v)) = self.writes.copy.unaccepted(key, &next_key(key)).pop() {
                return Ok(v);
            }
        }
        let root = self.root_or_park(key, &next_key(key))?;
        match self.walk(&root, &Walk::Get(key.to_vec())) {
            Walked::Done(ReadResult::Value(v)) => Ok(v),
            Walked::Need(_) => Err(self.park(Some(root), scan_range(key, &next_key(key), 1))),
            Walked::Done(_) | Walked::Broken(_) => Err(StoreError::Unavailable),
        }
    }

    fn row_state(&self, key: &[u8]) -> RowState {
        self.writes.row_state(key)
    }

    fn wrong_width(&self, given: IdWidth, wanted: IdWidth) {
        self.writes.client.record_wrong_width(given, wanted);
    }

    fn scan(&mut self, lo: &[u8], hi: &[u8], reverse: bool, limit: usize) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        self.sync();
        if lo >= hi {
            return Ok(Vec::new());
        }
        let root = self.root_or_park(lo, hi)?;
        // INTERIM: removed by R-b (READ-STATE § queue). With writes the engine
        // has not taken in range, the walk reads the whole range, so a delete
        // among them cannot leave the page short.
        let overlay = if self.interim_overlay { self.writes.copy.unaccepted(lo, hi) } else { Vec::new() };
        let walk_limit = if overlay.is_empty() { limit } else { usize::MAX };
        let rows = match self.walk_scan(root, lo, hi, reverse, walk_limit) {
            Ok(rows) => rows,
            Err(WalkStop::Need(range)) => return Err(self.park(Some(root), range)),
            Err(WalkStop::Unavailable) => return Err(StoreError::Unavailable),
        };
        if overlay.is_empty() {
            return Ok(rows);
        }
        let mut all: BTreeMap<Vec<u8>, Vec<u8>> = rows.into_iter().collect();
        for (k, v) in overlay {
            match v {
                Some(v) => all.insert(k, v),
                None => all.remove(&k),
            };
        }
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = all.into_iter().collect();
        if reverse {
            out.reverse();
        }
        out.truncate(limit);
        Ok(out)
    }

    fn root(&mut self) -> Read<[u8; 32]> {
        self.head().ok_or(StoreError::NotLoaded)
    }

    /// The tree's own diff, walked here: what changed in `[lo, hi)` from
    /// `from` to this store's root. Blocks missing (from `from`'s side or the
    /// root's) is a full read, the defined answer — never a shorter list.
    fn changes_since(&mut self, from: [u8; 32], lo: &[u8], hi: &[u8], max_entries: u32) -> Read<Delta> {
        let root = self.head().ok_or(StoreError::NotLoaded)?;
        let spec = DeltaSpec { from, lo: Bound::Included(lo.to_vec()), hi: Bound::Excluded(hi.to_vec()), max_entries: max_entries.max(1) as usize };
        match self.walk(&root, &Walk::Delta(Box::new(spec))) {
            Walked::Done(ReadResult::Delta { changes, cursor, new_root }) => Ok(Delta::Changes { changes, cursor, new_root }),
            _ => Ok(Delta::FullReloadRequired { new_root: root }),
        }
    }
}

impl<H: Host> Store for PageStore<H> {
    /// A store-level batch: the write half builds it (FORCED, sdk#235), and
    /// the engine has it when this returns.
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) -> Result<(), crate::copy::Refused> {
        let r = self.writes.apply_batch(edits);
        self.sync();
        r
    }

    fn last_write_id(&self) -> Option<u64> {
        self.writes.last_write_id()
    }

    fn is_pending_write(&self, write_id: u64) -> bool {
        self.writes.is_pending_write(write_id)
    }

    fn take_conflict_chains(&mut self) -> Vec<crate::store::ConflictChain> {
        self.writes.take_conflict_chains()
    }

    fn carry_tries(&mut self, write_id: u64, tries: u8) {
        self.writes.carry_tries(write_id, tries);
    }

    /// Made, then handed to the engine in the same call: the door's verdict
    /// (taken, `Busy`, refused) is known when this returns.
    fn apply_commit(&mut self, reads: &[(Vec<u8>, protocol::Expect)], edits: &[(Vec<u8>, Edit)]) -> Result<(), crate::copy::Refused> {
        let r = self.writes.apply_commit(reads, edits);
        self.sync();
        r
    }
}
