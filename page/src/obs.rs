//! THE OBSERVATION TREE's page side (craftworks-docs OBSERVABILITY §1-§2, sdk#399): the per-(app, minute) DETAIL
//! record's key, and the page's recording split so each window's record holds THAT window's events only.
//!
//! The record's value is the ONE publish filter's output (`instrument::publish`), nothing the page adds (rule 3). The
//! page never serializes its recording any other way: a source scan (page/tests/obs_one_way_out.rs) holds it.

use instrument::{Probe, Recorder, Recording};

/// The detail record's key in the observation tree: `w/ ‖ the app's SITE contract id ‖ the window's minute (u64 BE)`.
///
/// The app is named by its site contract id (the architect on #399): the stable site key (#117), the one owner of
/// "which app", fixed-width, and what support already needs to read the app's container at that version -- never the
/// app's name, a second and mutable form. Big-endian minutes: one app's windows sort by time, and a range scan over
/// `w/ ‖ site` reads that app alone.
pub fn detail_key(site: &[u8; 32], minute: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + 32 + 8);
    k.extend_from_slice(b"w/");
    k.extend_from_slice(site);
    k.extend_from_slice(&minute.to_be_bytes());
    k
}

/// The page's recording, WHOLE for the person's own dump, and beside it the CURRENT WINDOW's (sdk#399): every event
/// goes to both. A window's record is published from the window recorder alone, which is then replaced by a fresh one,
/// so the next record holds none of this one's events -- the "per window" the filter's `dropped` and ordinals rely on
/// (the architect's carry-over from instrument#18: an Exit carries no offset, so only the drain keeps an old one out).
/// The ring refuses events once full (it never overwrites), so a per-window recorder is also what keeps a long-lived
/// page publishing at all.
pub(crate) struct Rec {
    local: Recorder,
    window: std::cell::RefCell<Recorder>,
    window_capacity: usize,
}

impl Rec {
    pub(crate) fn new(capacity: usize) -> Rec {
        Rec { local: Recorder::with_capacity(capacity), window: std::cell::RefCell::new(Recorder::with_capacity(capacity)), window_capacity: capacity }
    }

    /// The whole recording (a separate read handle; the page itself only writes).
    pub(crate) fn recording(&self) -> Recording<'_> {
        self.local.recording()
    }

    /// The window now closing, taken whole; a fresh recorder takes its place.
    pub(crate) fn take_window(&self) -> Recorder {
        self.window.replace(Recorder::with_capacity(self.window_capacity))
    }
}

impl Probe for Rec {
    fn event(&self, e: instrument::Event) {
        self.local.event(e);
        // A probe never panics: a window being taken at this instant (it can't be, single-threaded) loses the event.
        if let Ok(w) = self.window.try_borrow() {
            w.event(e);
        }
    }
}

/// W_ride (OBSERVABILITY §3): at most ONE observation commit per this long of saving. A save lands its data head;
/// the pending window records ride after it only if the last observation commit was at least this long ago.
pub const W_RIDE_MS: u64 = 60_000;

/// The window records a page keeps waiting for a save (rule 9: never an unbounded queue): one roll-up hour's worth.
/// Past it the OLDEST is dropped, and what it held is counted into the next record's drop count
/// (`Window::lost_before`). To be derived from the roll-up's constants when step 5 makes them.
pub const PENDING_MAX: usize = 60;

/// The observation tree's own block store, in bytes: past it, the tree is re-opened fresh (its next commit re-reads
/// its head) once idle. Its store is its OWN, so an observation never evicts a data block (the architect); this bounds
/// the page's memory for diagnostics.
pub const BLOCK_BUDGET: usize = 4 << 20;

/// The engine client the observation tree's writes are made under: no person's (nothing shows its states).
pub const CLIENT: engine::ClientId = engine::ClientId(u64::MAX);

/// A closed window's record, waiting for a save to ride: its key and its value (the filter's output).
pub(crate) struct Pending {
    pub(crate) key: Vec<u8>,
    pub(crate) value: Vec<u8>,
}

impl Pending {
    /// What dropping this record loses, as the next record's `lost_before`: its encoded event count plus its drop
    /// bucket's LOWER bound (the architect) -- a lower bound, never exact, as a bucket is all the record keeps.
    pub(crate) fn lost_if_dropped(&self) -> u64 {
        instrument::publish::Published::decode(&self.value, &[]).map_or(0, |p| p.events.len() as u64 + lower_bound(p.dropped))
    }
}

/// A bucket's LOWER bound: the least count [`instrument::vocab::Bucket::of`] puts in it -- derived from `of` itself
/// (it is monotone), so the bucket's edges have one owner, the instrument.
pub(crate) fn lower_bound(b: instrument::vocab::Bucket) -> u64 {
    let (mut lo, mut hi) = (0u64, u64::MAX);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if instrument::vocab::Bucket::of(mid) >= b {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

#[cfg(test)]
mod bounds {
    use instrument::vocab::Bucket;

    /// Each bucket's lower bound is in it, and the count below it is not (the edges come from `Bucket::of` alone).
    #[test]
    fn a_buckets_lower_bound_is_its_least_count() {
        for b in [Bucket::Zero, Bucket::One, Bucket::UpTo9, Bucket::UpTo99, Bucket::UpTo999, Bucket::Over999] {
            let lo = super::lower_bound(b);
            assert_eq!(Bucket::of(lo), b, "{b:?}'s lower bound {lo} is not in it");
            assert!(lo == 0 || Bucket::of(lo - 1) < b, "{b:?}'s lower bound {lo} is not its least count");
        }
    }
}
