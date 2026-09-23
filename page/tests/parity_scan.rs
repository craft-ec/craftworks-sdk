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
    p.answer(Answer::Head(head.map(Into::into)), Ms(1));
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

/// RACE PUT's MARK (COMMIT-LIFE §P): a head this build signs carries an EMPTY
/// `TAG_PARITY` in a v2 ledger, meaning every group it lists was recoverable
/// when it was signed. A page opened on such a head knows that without a
/// scan: `Done`. The same root with NO mark (the control above) is
/// `NotScanned`, and so is a v1 ledger's parity field (#119's scan front, a
/// different meaning, dropped on read).
#[test]
fn a_page_opened_on_a_marked_head_says_done_and_a_v1_parity_field_is_not_the_mark() {
    let root = [9u8; 32];
    let marked: Vec<u8> = root.iter().copied().chain(page::sign_ledger(6, [8; 32], root)).collect();
    let head = page::HeadRead::from_value(7, &marked).expect("a head");
    assert!(head.parity_marked(), "this build's signed head carries no §P mark");
    let mut p = Page::new(Params::default(), PutPath::Page);
    let _ = p.take_ops();
    p.answer(Answer::Head(Some(head)), Ms(1));
    assert_eq!(p.published().0, 7, "the head was not taken, so this proves nothing");
    assert_eq!(p.parity_scan(), &ParityScan::Done { root }, "a marked head was not taken as scanned");

    // A v1 ledger whose parity field is the OLD scan front (here empty too):
    // never the mark.
    use signer_proto::head::{LEDGER_VERSION_V1, TAG_PARITY};
    let mut v1 = root.to_vec();
    v1.extend_from_slice(&[LEDGER_VERSION_V1, TAG_PARITY, 0, 0]);
    let old = page::HeadRead::from_value(7, &v1).expect("a head");
    assert!(!old.parity_marked(), "a v1 parity field was read as the §P mark");
    let mut q = Page::new(Params::default(), PutPath::Page);
    let _ = q.take_ops();
    q.answer(Answer::Head(Some(old)), Ms(1));
    assert_eq!(q.parity_scan(), &ParityScan::NotScanned);
}
