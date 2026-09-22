//! Turning "I could not read that" into "wait for this ticket".
//!
//! # Why this is in the SDK and not in `web/src/session.rs`
//!
//! It used to be a method on `Session`, which is a `#[wasm_bindgen]` type in
//! a `cdylib`. Nothing native can construct one, so the only tests that could
//! reach it drove a FAKE session written in JavaScript — which compiles the
//! Rust and runs none of it.
//!
//! That is not a hypothetical cost. The fix for sdk#89 was reviewed by
//! putting the defect BACK at the Rust site — `define` returning a ticketless
//! `NotLoaded`, exactly as before — and the whole suite stayed green: `build`
//! succeeded, `engine-db.test.mjs` passed, rc 0. The JavaScript tests
//! exercise the retry loop and never the parking, so the defect that cost
//! five acceptance runs could be reintroduced without a single test noticing.
//!
//! `Refresh` is in this crate for the same reason and says so in its own
//! comment. This follows it.
//!
//! # What it decides
//!
//! A read or a write comes back from `Db`. If it succeeded, the chain it was
//! walking is finished. If it failed for want of a range, that range is
//! requested — ONCE, bounded by `Loads` — and the caller is handed the error
//! with a ticket to wait on. If it failed for any other reason, or for a
//! range already loaded that still cannot answer, the caller is TOLD rather
//! than sent round: asking again would answer exactly as it did the first
//! time.

use crate::db::DbError;
use crate::loads::Loads;
use crate::CachedStore;

/// What a `Db` call came back as, once the loading has been decided.
#[derive(Debug)]
pub enum Outcome<T> {
    /// It worked.
    Done(T),
    /// It could not read a range, that range has been requested, and this is
    /// the ticket whose end says to ask again.
    Wait(DbError, u64),
    /// Nothing more is coming: either no load can help, or the load that
    /// would have helped has already happened.
    Told(DbError),
}

impl<T> Outcome<T> {
    /// The ticket, if this outcome has one. For callers that only report.
    pub fn ticket(&self) -> Option<u64> {
        match self {
            Outcome::Wait(_, t) => Some(*t),
            _ => None,
        }
    }
}

/// Decide what a `Db` result means, queueing the load it needs.
///
/// **This is the whole of the recovery.** The first version of the JS wrapper
/// answered a `NotLoaded` by awaiting a resolved promise and asking again —
/// in a microtask, before any websocket message could have arrived, and
/// having requested nothing at all.
///
/// # Writes go through here too
///
/// A write can fail because it is INVALID, which no load fixes, or because it
/// had to READ something in order to apply itself. Every write in `Db` reads
/// first: `define` reads the existing schema to check the new one against it,
/// and `put`, `update` and `delete` all go through `need_schema`. So a
/// `NotLoaded` from a write means the second, always — and it is retryable.
///
/// Retrying cannot double-apply: every one of those reads happens before the
/// single `write` that mutates, and that write applies the whole edit or none
/// of it. A `NotLoaded` leaves the tree untouched, so asking again repeats the
/// attempt and not the effect.
pub fn decide<T>(
    loads: &mut Loads,
    store: &mut CachedStore,
    r: Result<T, DbError>,
    now_ms: u64,
) -> Outcome<T> {
    decide_with(loads, store, r, now_ms, None)
}

/// [`decide`], with the page's COLD READS given the first refusal of a new
/// load (`crate::cold`): a range they take is read by the page's own GETs and
/// never reaches the engine; one they leave goes to the engine exactly as
/// `decide` sends it.
/// A `Delta` or a `FullReloadRequired` whose `req_id` is a READ'S OWN ticket
/// (sdk#266): the read parked on a stale range, and this is its answer.
///
/// Returns false when the id belongs to no open load — then it is a
/// binding's refresh, and the caller's `Refresh` owns it.
///
/// * a delta that fits: applied, the range is current again, the ticket
///   completes and the read wakes;
/// * a PAGED delta (too much changed to patch) or a `FullReloadRequired`:
///   the range is loaded in full under the SAME ticket, so the read waits
///   for one answer rather than being told to start again.
pub fn delta_for_read(
    loads: &mut Loads,
    store: &mut CachedStore,
    req_id: u64,
    changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    cursor: Option<Vec<u8>>,
    new_root: [u8; 32],
) -> Option<([u8; 32], Vec<u8>, Vec<u8>)> {
    let (lo, hi) = loads.range_of(req_id).map(|(l, h)| (l.to_vec(), h.to_vec()))?;
    if cursor.is_some() {
        store.client.send(&Loads::range_request(req_id, &lo, &hi, None));
        return None;
    }
    store.on_delta(changes, new_root);
    store.copy.refreshed(&lo, &hi);
    loads.on_refreshed(req_id);
    Some((new_root, lo, hi))
}

/// The engine could not diff a stale range's re-ask: load it in full under
/// the read's own ticket (sdk#266). False when the id is not a read's.
pub fn full_reload_for_read(loads: &mut Loads, store: &mut CachedStore, req_id: u64) -> bool {
    let Some((lo, hi)) = loads.range_of(req_id).map(|(l, h)| (l.to_vec(), h.to_vec())) else {
        return false;
    };
    store.copy.forget(&lo, &hi);
    store.client.send(&Loads::range_request(req_id, &lo, &hi, None));
    true
}

pub fn decide_with<T>(
    loads: &mut Loads,
    store: &mut CachedStore,
    r: Result<T, DbError>,
    now_ms: u64,
    cold: Option<&mut crate::cold::ColdReads>,
) -> Outcome<T> {
    let e = match r {
        Ok(v) => {
            // The chain this call was walking is finished; the next one
            // starts its own.
            loads.read_succeeded();
            return Outcome::Done(v);
        }
        Err(e) => e,
    };
    let Some((lo, hi)) = e.needs() else {
        return Outcome::Told(e);
    };
    let (lo, hi) = (lo.to_vec(), hi.to_vec());
    let Some((req_id, send)) = loads.want(&lo, &hi, now_ms) else {
        // This span was loaded already and the call still cannot be answered.
        // Loading it again would answer exactly as it did the first time, so
        // the caller is told instead of sent round.
        return Outcome::Told(e);
    };
    let taken = send && cold.is_some_and(|c| c.take(req_id, &lo, &hi, now_ms));
    // ALREADY HELD, ONLY BEHIND (sdk#266). The copy has these rows and knows
    // the root it read them at, so the question is "what changed since", not
    // "send me the range": a delta of what moved, under this read's own
    // ticket, which the answer completes. A range nobody has ever loaded
    // takes the full load below, as before.
    if send && !taken && store.copy.is_stale(&lo, &hi) {
        if let Some(from) = store.copy.root() {
            store.client.send(&protocol::Request::ChangesSince {
                req_id,
                from,
                lo: protocol::Bound::Included(lo.clone()),
                hi: protocol::Bound::Excluded(hi.clone()),
                max_entries: protocol::MAX_PAGE_ENTRIES,
            });
            return Outcome::Wait(e, req_id);
        }
    }
    if send && !taken {
        // A FULL PAGE, by the shared constant. `0` reads like "no limit" and
        // is not one: the shell clamps it to ONE entry, so a range of N rows
        // would load in N round trips of a single row.
        store
            .client
            .send(&Loads::range_request(req_id, &lo, &hi, None));
    }
    Outcome::Wait(e, req_id)
}
