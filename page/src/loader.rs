//! THE LOADER'S RECORDING, handed to the page (sdk#386's instrument work, the architect's design).
//!
//! A published app's loader fetches the SDK before the SDK exists (`js/served.js`, the bootstrap exception), so it
//! records in JS, into a bounded ring of CODES. When the wasm exists the ring is handed here, once, and becomes the
//! OPENING segment of the page's recording: one dump holds the loader's fetch rounds and the page's sends.
//!
//! JS has no `&'static str` and no closed enum, which are what keep user content out of a recording in Rust. So the
//! boundary is HERE, and nothing is trusted: every event arrives as numbers, every number is looked up in a closed
//! list below, every time is re-coarsened by the vocabulary's rule, and anything unknown is REFUSED -- counted, never
//! passed through. The lists are the one owner: `js/instrument-vocab.js` is generated from them
//! (`examples/instrument_js.rs`), and a test fails when it is stale.

use instrument::vocab::{coarsen_ms, DropReason, Key, Outcome, Site, StatusClass};
use instrument::{Dir, Entry, Event, Kind, Label, OpId};

/// The loader's SITES, by code (the index): closed.
pub const SITES: [Site; 1] = [Site::of("loader::fetch")];

/// The keys a loader event may carry, by code (the index): closed. Any other key is refused.
pub const KEYS: [Key; 3] = [Key::Attempts, Key::OffsetMs, Key::StatusClass];

/// How a loader fetch round may END without an answer, by code (the index): no HTTP answer at all (Timeout), or
/// cancelled on this side (Withdrawn: a race that had enough, or a person's cancel).
pub const OUTCOMES: [Outcome; 2] = [Outcome::Timeout, Outcome::Withdrawn];

/// An event's KIND, the first number of its five.
pub const EDGE: u64 = 0;
pub const EXIT: u64 = 1;
pub const COUNTER: u64 = 2;

/// Numbers per event: `[kind, site, a, b, c]`.
/// - EDGE: `a` 0 request / 1 response, `b` the fetch's ordinal.
/// - EXIT: `a` the ordinal, `b` an OUTCOMES code.
/// - COUNTER: `a` the ordinal, `b` a KEYS code, `c` the value.
pub const STRIDE: usize = 5;

/// Where refused events and the ring's own losses are counted, as `DroppedMsgs` (the vocabulary's key for what a
/// build could not use).
pub const HANDOVER: Site = Site::of("loader::handover");
pub const RING: Site = Site::of("loader::ring");

/// The loader's segment as handed over: when the loader started (its `Date.now()`, the page's clock too), its events
/// as numbers, and how many its ring dropped.
#[derive(Clone, Debug, Default)]
pub struct Segment {
    pub start_ms: u64,
    pub events: Vec<u64>,
    pub dropped: u64,
}

/// One coded event, VALIDATED: `Some` only if every number names something in the closed lists above. Offsets are
/// re-coarsened here whatever the JS did.
pub fn decode(ev: &[u64]) -> Option<Event> {
    let [kind, site, a, b, c] = <[u64; STRIDE]>::try_from(ev).ok()?;
    let site = *SITES.get(usize::try_from(site).ok()?)?;
    // A fetch's label, or `None` (refused) for an ordinal no label carries.
    let label = |n: u64| Label::new(Kind::Fetch, u32::try_from(n).ok()?);
    match kind {
        EDGE => {
            let dir = match a {
                0 => Dir::Request,
                1 => Dir::Response,
                _ => return None,
            };
            Some(Event::Edge { site, dir, id: label(b)? })
        }
        EXIT => {
            let outcome = *OUTCOMES.get(usize::try_from(b).ok()?)?;
            Some(Event::Exit { site, op: label(a)?.op(), outcome })
        }
        COUNTER => {
            let key = *KEYS.get(usize::try_from(b).ok()?)?;
            let value = match key {
                Key::OffsetMs => coarsen_ms(c),
                Key::StatusClass if usize::try_from(c).ok()? >= StatusClass::ALL.len() => return None,
                _ => c,
            };
            Some(Event::Counter { site, op: label(a)?.op(), entry: Entry { key, value } })
        }
        _ => None,
    }
}

/// The events a segment puts into a recording, in order: each validated one, a `DroppedMsgs` (Unparseable) for each
/// refused one, and one `DroppedMsgs` with the ring's own loss count if it lost any.
pub fn events(seg: &Segment) -> Vec<Event> {
    let mut out = Vec::with_capacity(seg.events.len() / STRIDE + 1);
    for ev in seg.events.chunks(STRIDE) {
        out.push(decode(ev).unwrap_or(Event::Counter {
            site: HANDOVER,
            op: OpId::NONE,
            entry: Entry { key: Key::DroppedMsgs, value: DropReason::Unparseable.code() },
        }));
    }
    if seg.dropped > 0 {
        out.push(Event::Counter { site: RING, op: OpId::NONE, entry: Entry { key: Key::DroppedMsgs, value: seg.dropped } });
    }
    out
}
