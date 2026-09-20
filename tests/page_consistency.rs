//! A RANGE IS NEVER STITCHED FROM TWO TREES.
//!
//! A `Page` used to carry rows and nothing about where they came from, so a
//! range assembled from several pages could hold rows from before a write and
//! rows from after it, recorded as one coherent snapshot with nobody able to
//! tell.
//!
//! That is not a rarity. One delegate context serves every connection —
//! MEASURED, F47, `probe/src/bin/two-connections.rs` — so another tab writing
//! between page 1 and page 2 of this one's load is an ordinary Tuesday.
//!
//! Every page now says which head it was read at, and a load is recorded only
//! if every page of it agrees.

use craftworks_sdk::loads::Page;
use craftworks_sdk::Loads;
use protocol::At;

fn rows(n: usize, tag: u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n).map(|i| (vec![tag, i as u8], vec![tag])).collect()
}

const OLD: At = At {
    seq: 4,
    root: [0xAA; 32],
};
const NEW: At = At {
    seq: 5,
    root: [0xBB; 32],
};

/// **A WRITE BETWEEN TWO PAGES RESTARTS THE LOAD.**
///
/// Page 1 is read at the old head; before page 2 arrives another tab writes,
/// so page 2 is read at the new one. What was gathered is half a tree and is
/// thrown away.
#[test]
fn a_write_between_two_pages_restarts_the_load() {
    // NOTE: this case is caught by EITHER guard — page 2 carries a higher
    // seq, so the seq check would also fire. `two_roots_at_the_same_seq_...`
    // is the one that isolates the root check.
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");

    // Page 1, at the old head, not the end of the range.
    assert!(matches!(
        l.on_page(id, rows(2, 1), Some(b"a/k".to_vec()), OLD),
        Page::More { .. }
    ));

    // Page 2 arrives read at a DIFFERENT head.
    match l.on_page(id, rows(2, 2), None, NEW) {
        Page::Restart { lo, hi } => {
            assert_eq!((lo, hi), (b"a/".to_vec(), b"a0".to_vec()));
        }
        other => panic!(
            "a range stitched from two trees was accepted: {other:?}. Rows from \
             before a write and rows from after it would have been recorded as \
             one snapshot"
        ),
    }
    assert!(
        l.take_ended().is_empty(),
        "the load ended on a restart; it should ask again, not give up"
    );

    // Asked again and answered consistently, it completes — at the NEW head.
    assert!(matches!(
        l.on_page(id, rows(2, 2), Some(b"a/k".to_vec()), NEW),
        Page::More { .. }
    ));
    match l.on_page(id, rows(2, 3), None, NEW) {
        Page::Complete { rows, .. } => {
            assert_eq!(
                rows.len(),
                4,
                "the restarted load kept rows from the old tree"
            );
            assert!(
                rows.iter().all(|(k, _)| k[0] != 1),
                "a row from the first, abandoned attempt survived into the result"
            );
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

/// **THE CONTROL.** Without the check, the mixed range IS recorded.
///
/// Shown rather than asserted: the same two pages, both reported at the SAME
/// head, complete with four rows from two different trees. That is exactly
/// what used to happen on every page, because no page carried a head at all.
#[test]
fn control_with_one_head_on_both_pages_the_mixed_range_is_recorded() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");

    assert!(matches!(
        l.on_page(id, rows(2, 1), Some(b"a/k".to_vec()), OLD),
        Page::More { .. }
    ));
    match l.on_page(id, rows(2, 2), None, OLD) {
        Page::Complete { rows, .. } => {
            assert_eq!(rows.len(), 4);
            // Rows from BOTH pages, recorded as one range. With a real write
            // in between, these two tags would be two trees — and nothing
            // here could tell. That is the defect, reproduced.
            assert!(rows.iter().any(|(k, _)| k[0] == 1));
            assert!(rows.iter().any(|(k, _)| k[0] == 2));
        }
        other => panic!("the control did not complete: {other:?}"),
    }
}

/// **THE ROOT CHECK ON ITS OWN**: two heads at the SAME seq, different roots.
///
/// The test above passes with the root check disabled, because the SEQ check
/// catches that case too — page 2 carries a higher seq, so the completed load
/// is behind what the client now knows. Two guards defending one property
/// means neither is tested by a case both can catch.
///
/// Here the seq is identical and only the ROOT differs, so only the root
/// check can see it. That is also why the rule is stated as it is: **seq
/// orders, root identifies.** A design that compared only seq would accept
/// two different trees numbered the same.
#[test]
fn two_roots_at_the_same_seq_still_restart_the_load() {
    const A: At = At {
        seq: 7,
        root: [0xA1; 32],
    };
    const B: At = At {
        seq: 7,
        root: [0xB2; 32],
    };

    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");

    assert!(matches!(
        l.on_page(id, rows(2, 1), Some(b"a/k".to_vec()), A),
        Page::More { .. }
    ));
    match l.on_page(id, rows(2, 2), None, B) {
        Page::Restart { .. } => {}
        other => panic!(
            "two DIFFERENT trees numbered the same seq were stitched into one \
             range: {other:?}. seq orders; root identifies"
        ),
    }
}

/// A load that finishes BEHIND what this client already knows is not
/// recorded: it would move the copy backwards.
#[test]
fn a_load_finishing_behind_the_known_head_is_not_recorded() {
    let mut l = Loads::new();
    // Something else — an `Identity`, a `Delta` — already named a newer head.
    l.note_seq(NEW.seq);

    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");
    match l.on_page(id, rows(2, 1), None, OLD) {
        Page::Restart { .. } => {}
        other => panic!(
            "a load read at seq {} was recorded although this client already \
             knows seq {}: {other:?}",
            OLD.seq, NEW.seq
        ),
    }

    // THE CONTROL: at the known head it completes.
    match l.on_page(id, rows(2, 2), None, NEW) {
        Page::Complete { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("a load at the known head was refused: {other:?}"),
    }
}

/// A range under continuous write ENDS rather than restarting for ever.
#[test]
fn a_range_that_never_settles_ends_unavailable() {
    let mut l = Loads::new();
    l.max_restarts = 3;
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");

    let mut restarts = 0;
    for i in 0..20u8 {
        let at = At {
            seq: 10 + i as u64,
            root: [i; 32],
        };
        // Always a first page at a fresh head: the tree never settles.
        match l.on_page(id, rows(1, i), Some(b"a/k".to_vec()), at) {
            Page::Restart { .. } => restarts += 1,
            Page::Nothing => break,
            _ => {}
        }
    }
    assert_eq!(restarts, l.max_restarts, "it did not stop at the budget");
    assert_eq!(
        l.take_ended().len(),
        1,
        "a range nobody can read consistently must END, not spin: a page that \
         never fills and never says why is the worst of both"
    );
}

/// THE SINGLE-PAGE CASE COSTS NO EXTRA ROUND TRIP.
///
/// The check must not turn every small read into two.
#[test]
fn a_single_page_load_completes_in_one_answer() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"a/", b"a0", 0).expect("a fresh span");
    match l.on_page(id, rows(3, 1), None, OLD) {
        Page::Complete { rows, .. } => assert_eq!(rows.len(), 3),
        other => panic!("a one-page range did not complete in one answer: {other:?}"),
    }
    assert_eq!(l.in_flight(), 0);
}
