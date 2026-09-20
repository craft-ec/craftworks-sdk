//! How many round trips a range costs.
//!
//! `max_entries: 0` reads like "no limit" and is not one. The delegate shell
//! clamps a request with `clamp(1, MAX_PAGE_ENTRIES)`, so `0` becomes ONE
//! entry — and a domain of N rows then loads in N round trips of a single row
//! each. At roughly 15 ms per delegate call (F32) a thousand rows is fifteen
//! seconds. Correct, and crawling, and invisible in every test that does not
//! COUNT the requests.

use craftworks_sdk::loads::{Loads, Page};

/// What the shell does to a request, so the test measures the engine's real
/// behaviour rather than a guess about it.
fn clamped(asked: u32) -> usize {
    (asked as usize).clamp(1, protocol::MAX_PAGE_ENTRIES as usize)
}

/// How many pages a range of `rows` takes when asking for `asked` per page.
fn pages_for(rows: usize, asked: u32) -> usize {
    let per = clamped(asked);
    let mut l = Loads::new();
    let (id, _) = l
        .want(b"d\0note\0", b"d\0note\x01", 0)
        .expect("a fresh span");
    let mut sent = 1; // the first request
    let mut delivered = 0;
    while delivered < rows {
        let n = per.min(rows - delivered);
        delivered += n;
        let entries: Vec<(Vec<u8>, Vec<u8>)> =
            (0..n).map(|i| (vec![(i % 251) as u8], vec![0u8])).collect();
        let cursor = if delivered < rows {
            Some(vec![delivered as u8])
        } else {
            None
        };
        match l.on_page(id, entries, cursor) {
            Page::More { .. } => sent += 1,
            Page::Complete { .. } => break,
            Page::Nothing => panic!("the load ended early at {delivered} of {rows}"),
        }
        assert!(sent < 10_000, "it did not converge");
    }
    sent
}

/// **A 300-row domain loads in 2 pages.**
#[test]
fn a_domain_of_300_rows_loads_in_two_pages() {
    assert_eq!(
        pages_for(300, protocol::MAX_PAGE_ENTRIES),
        2,
        "a 300-row domain did not load in two pages"
    );
}

/// **THE SAME RANGE, ASKED FOR WITH `0`, TAKES 300.**
///
/// This is what the loaders were sending. The test above passes for a loader
/// that asks properly and this one shows what the old value actually cost —
/// without it, "2 pages" is a number with nothing to compare it to.
#[test]
fn asking_with_zero_costs_one_round_trip_per_row() {
    assert_eq!(
        pages_for(300, 0),
        300,
        "`0` no longer means one row per page; if the shell's clamp changed, \
         this test and the constant's documentation both need revisiting"
    );
    // And the difference is the whole point.
    assert!(
        pages_for(300, 0) > 100 * pages_for(300, protocol::MAX_PAGE_ENTRIES),
        "asking for a page is not meaningfully cheaper than asking for 0, so \
         either the clamp or this test is wrong"
    );
}

/// The constant the loaders send survives the shell's clamp unchanged.
///
/// A client asking for more than the engine allows gets silently less, and
/// the two then disagree about what a full page is. They hold one number now,
/// and this says so.
#[test]
fn the_page_constant_is_not_clamped_down_by_the_engine() {
    assert_eq!(
        clamped(protocol::MAX_PAGE_ENTRIES),
        protocol::MAX_PAGE_ENTRIES as usize,
        "the loaders ask for more than the engine will give"
    );
}

/// A range that fits in one page costs one round trip.
#[test]
fn a_small_range_costs_a_single_round_trip() {
    assert_eq!(pages_for(10, protocol::MAX_PAGE_ENTRIES), 1);
}
