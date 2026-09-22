//! Ranges asked for and not yet answered.
//!
//! The recovery from a `NotLoaded` read is to FETCH the range, and this is
//! the bookkeeping that makes that a fact. It is tested here, natively,
//! because the first version of the recovery lived in JavaScript as a comment
//! over an `await Promise.resolve()` — it requested nothing, woke in a
//! microtask before any message could arrive, and no test could see it.

use craftworks_sdk::loads::{Ended, Loads, Page};

/// One head for every page: these tests do not move the tree, and a
/// constant says so rather than leaving it to be inferred.
const AT: protocol::At = protocol::At {
    seq: 1,
    root: [1u8; 32],
};

/// Rows INSIDE the `a/` range these tests load: a page holding keys outside
/// its load's range is refused (sdk#166).
fn rows(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n).map(|i| (vec![b'a', b'/', i as u8], vec![0u8])).collect()
}

/// A range asked for once is requested once, and answering it ends the
/// ticket with the rows to record.
#[test]
fn a_range_is_requested_and_completed() {
    let mut l = Loads::new();
    let (id, send) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");
    assert!(send, "the first want of a range did not ask for it");
    assert_eq!(l.in_flight(), 1);

    match l.on_page(id, rows(3), None, AT) {
        Page::Complete { lo, hi, rows, .. } => {
            assert_eq!(lo, b"a/".to_vec());
            assert_eq!(hi, b"a0".to_vec());
            assert_eq!(rows.len(), 3);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
    assert_eq!(l.take_ended(), vec![(id, Ended::Loaded)]);
    assert_eq!(l.in_flight(), 0);
}

/// **CONCURRENT READS OF ONE RANGE ISSUE ONE REQUEST.**
///
/// Several components reading the same unloaded range on one frame is the
/// ordinary case. A request each would multiply the first paint of every
/// screen by the number of things on it.
#[test]
fn concurrent_reads_of_one_range_issue_one_request() {
    let mut l = Loads::new();
    let (first, send1) = l.want(b"a/", b"a0", 0).expect("a fresh span");
    let (second, send2) = l.want(b"a/", b"a0", 1).expect("a span in flight");
    let (third, send3) = l.want(b"a/", b"a0", 2).expect("a span in flight");

    assert!(send1, "the first read did not request the range");
    assert!(!send2 && !send3, "a second reader asked the node again");
    assert_eq!(
        (first, second, third),
        (first, first, first),
        "the three readers are parked on different tickets, so two of them \
         will never be woken"
    );
    assert_eq!(l.in_flight(), 1);
}

/// THE CONTROL: a DIFFERENT range is a different request.
///
/// Without this, `concurrent_reads_of_one_range_issue_one_request` passes
/// against a registry that never requests anything after the first.
#[test]
fn control_a_different_range_is_its_own_request() {
    let mut l = Loads::new();
    let (a, send_a) = l.want(b"a/", b"a0", 0).expect("a fresh span");
    let (b, send_b) = l.want(b"b/", b"b0", 0).expect("another fresh span");
    assert!(
        send_a && send_b,
        "a second, DIFFERENT range was not requested"
    );
    assert_ne!(a, b, "two ranges share a ticket");
    assert_eq!(l.in_flight(), 2);
}

/// A range that arrives in pages is followed to its end before it is
/// recorded.
///
/// The copy records `[lo, hi)` as LOADED, and "everything here and not in
/// this list is absent" is only true once the range is exhausted. Recorded
/// after the first page, every later read of the tail would be answered
/// "empty" — confidently, and wrongly.
#[test]
fn a_paged_range_is_recorded_only_once_it_is_exhausted() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");

    match l.on_page(id, rows(2), Some(b"a/k".to_vec()), AT) {
        Page::More { after, lo, hi } => {
            assert_eq!(after, b"a/k".to_vec());
            assert_eq!((lo, hi), (b"a/".to_vec(), b"a0".to_vec()));
        }
        other => panic!("a truncated page was treated as the whole range: {other:?}"),
    }
    assert!(
        l.take_ended().is_empty(),
        "the read was woken by a PART of the range it asked for"
    );

    match l.on_page(id, rows(2), None, AT) {
        Page::Complete { rows, .. } => assert_eq!(rows.len(), 4, "the earlier page was dropped"),
        other => panic!("expected Complete, got {other:?}"),
    }
    assert_eq!(l.take_ended(), vec![(id, Ended::Loaded)]);
}

/// A ticket nobody answers ends within the bound, as UNAVAILABLE.
///
/// An unbounded wait is a spinner that never stops. The read parked on it
/// gets a fact instead.
#[test]
fn a_load_nobody_answers_ends_within_the_bound() {
    let mut l = Loads::new();
    l.budget_ms = 1_000;
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");

    assert!(
        l.time_out(1_000).is_empty(),
        "it gave up inside its own budget"
    );
    assert_eq!(l.time_out(1_001), vec![id]);
    assert_eq!(l.take_ended(), vec![(id, Ended::Unavailable)]);
    assert_eq!(l.in_flight(), 0);
}

/// A load the page's own COLD READ holds is not ended by its age: it ends by
/// its blocks' deadlines, as NOT_ANSWERING. A load beside it that the cold
/// reader does not hold still times out — the control that `kept` is what
/// spared the first one.
#[test]
fn a_load_the_cold_read_holds_outlives_the_budget_the_others_do_not() {
    let mut l = Loads::new();
    l.budget_ms = 1_000;
    let (cold, _) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");
    let (plain, _) = l.want(b"b/", b"b0", 0).expect("a fresh span is wanted");
    assert_eq!(l.time_out_except(5_000, |id| id == cold), vec![plain]);
    assert_eq!(l.take_ended(), vec![(plain, Ended::Unavailable)]);
    assert_eq!(l.in_flight(), 1, "the cold load was ended by its age");
    l.on_not_answering(cold);
    assert_eq!(l.take_ended(), vec![(cold, Ended::NotAnswering)]);
}

/// THE CONTROL for the bound: a load that IS answered never times out.
#[test]
fn control_an_answered_load_never_times_out() {
    let mut l = Loads::new();
    l.budget_ms = 1_000;
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");
    l.on_page(id, rows(1), None, AT);
    assert_eq!(l.take_ended(), vec![(id, Ended::Loaded)]);
    assert!(
        l.time_out(10_000).is_empty(),
        "a completed load was reported as timed out; a page would show a \
         failure for data it already has"
    );
}

/// `Unavailable` ends the ticket and records NOTHING.
///
/// An empty page here would say "this range is empty", which is a wrong
/// answer wearing the shape of a right one.
#[test]
fn an_unavailable_reply_ends_the_ticket_and_records_nothing() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");
    l.on_unavailable(id);
    assert_eq!(l.take_ended(), vec![(id, Ended::Unavailable)]);
    assert_eq!(l.in_flight(), 0);
    // And a page arriving afterwards changes nothing.
    assert_eq!(l.on_page(id, rows(3), None, AT), Page::Nothing);
    assert!(l.take_ended().is_empty());
}

/// A range larger than this client will hold ENDS rather than being recorded
/// short.
#[test]
fn a_range_too_large_to_hold_ends_rather_than_being_recorded_short() {
    let mut l = Loads::new();
    l.max_rows = 4;
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span is wanted");
    assert_eq!(
        l.on_page(id, rows(5), Some(b"a/k".to_vec()), AT),
        Page::Nothing
    );
    assert_eq!(
        l.take_ended(),
        vec![(id, Ended::Unavailable)],
        "an oversized range was recorded short; later reads of it would be \
         answered 'empty' with confidence"
    );
}

/// A page for a load nobody is waiting on is ignored, not applied.
#[test]
fn a_page_for_an_unknown_load_is_ignored() {
    let mut l = Loads::new();
    assert_eq!(l.on_page(999, rows(3), None, AT), Page::Nothing);
    assert!(l.take_ended().is_empty());
}

/// **A READ THAT GOES ROUND WITHOUT PROGRESS IS STOPPED.**
///
/// A cold read is a chain — `scan` reads its domain's schema before its rows
/// — so re-asking has to be allowed to go round more than once. What must
/// never happen is going round having loaded the same span again: loading it
/// a second time answers exactly as the first did, and the read would spin.
///
/// The registry can see this and a caller cannot, which is why the rule lives
/// here rather than as a counter in JavaScript.
#[test]
fn a_span_already_loaded_is_not_wanted_again() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");
    l.on_page(id, rows(1), None, AT);
    let _ = l.take_ended();

    assert_eq!(
        l.want(b"a/", b"a0", 1),
        None,
        "a span that was just loaded was requested again; the read would go \
         round for ever loading data it already has"
    );
}

/// THE CONTROL: the chain keeps moving while each hop is a NEW span.
///
/// Without this, `a_span_already_loaded_is_not_wanted_again` passes against a
/// registry that refuses every second request — which would stop a cold read
/// at its schema and never reach its rows.
#[test]
fn control_a_chain_of_different_spans_keeps_going() {
    let mut l = Loads::new();
    // hop 1: the schema's key.
    let (a, _) = l
        .want(b"note/#", b"note/#\0", 0)
        .expect("the schema's span");
    // The schema's own key: a row inside the span, so hop 1 COMPLETES.
    assert!(matches!(
        l.on_page(a, vec![(b"note/#".to_vec(), vec![0])], None, AT),
        craftworks_sdk::loads::Page::Complete { .. }
    ));
    let _ = l.take_ended();
    // hop 2: the rows. A DIFFERENT span, so it proceeds.
    let b = l.want(b"note/", b"note0", 1);
    assert!(
        b.is_some(),
        "the second hop of a cold read was refused, so a scan would never \
         reach its rows"
    );
    let (b, send) = b.expect("checked");
    assert!(send && b != a);
}

/// A read that succeeds clears the chain, so the NEXT read may load the same
/// spans again.
#[test]
fn a_successful_read_clears_the_chain() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");
    l.on_page(id, rows(1), None, AT);
    let _ = l.take_ended();
    assert_eq!(l.want(b"a/", b"a0", 1), None);

    l.read_succeeded();
    assert!(
        l.want(b"a/", b"a0", 2).is_some(),
        "a later read of the same span was refused for ever; one read's \
         chain must not bind the next one's"
    );
}
