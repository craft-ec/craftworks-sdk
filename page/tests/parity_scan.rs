//! What a page says about its redundancy, from the moment it opens
//! (craftworks-sdk#119): a page that read a non-empty head never looked at
//! that tree's parity, so it says `NotScanned` — never "0 owed". Only a page
//! that started from the empty tree knows the tree in full.

use engine::{ParityScan, Params};
use page::{Answer, Ms, Op, Page, PutPath};

fn opened_on(head: Option<(u64, [u8; 32])>) -> Page {
    let mut p = Page::new(Params::default(), PutPath::Page);
    let ops = p.take_ops();
    assert!(ops.iter().any(|o| matches!(o, Op::ReadHead)), "a page reads its head first: {ops:?}");
    p.answer(Answer::Head(head), Ms(1));
    p
}

#[test]
fn a_page_opened_on_an_existing_head_says_not_scanned_never_zero_owed() {
    let p = opened_on(Some((7, [9; 32])));
    assert_eq!(p.published().0, 7, "the head was not taken, so this proves nothing");
    assert_eq!(p.parity_scan(), &ParityScan::NotScanned);
}

/// The control: a page with no head starts from the empty tree, which lists no
/// parity at all — its zero IS "all put".
#[test]
fn a_page_opened_on_no_head_knows_the_empty_tree_in_full() {
    let p = opened_on(None);
    assert!(matches!(p.parity_scan(), ParityScan::Done { .. }), "{:?}", p.parity_scan());
}
