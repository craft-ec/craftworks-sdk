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
    if send {
        // A FULL PAGE, by the shared constant. `0` reads like "no limit" and
        // is not one: the shell clamps it to ONE entry, so a range of N rows
        // would load in N round trips of a single row.
        store
            .client
            .send(&Loads::range_request(req_id, &lo, &hi, None));
    }
    Outcome::Wait(e, req_id)
}
