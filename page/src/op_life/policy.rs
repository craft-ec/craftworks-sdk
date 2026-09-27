//! OP-LIFE.md draft 3.1's type, pinned cell by cell: `resend(&Op)` states every op's answer, in both directions (a
//! missing row fails, a changed answer fails), and `in_flight` states its six cells. A classifier whose live range
//! were one value would be a constant: the controls assert each type takes every one of its values.
use crate::op_life::{in_flight, resend, InFlight, Resend};
use crate::*;

fn every_op() -> Vec<(&'static str, Op, Resend)> {
    let sign = Op::Sign { id: 1, prev_seq: 0, prev_root: [0; 32], seq: 1, root: [0; 32], ledger: Vec::new(), label: Label::Head };
    vec![
        ("a block PUT", Op::Put { id: [1; 32], bytes: vec![1] }, Resend::Duplicates),
        ("an app container PUT", Op::PutApp { key: "k".into() }, Resend::Duplicates),
        ("the head register's UPDATE", Op::Update { label: Label::Head, state: vec![1] }, Resend::Duplicates),
        ("a site's create PUT", Op::Update { label: Label::Site("a".into()), state: vec![1] }, Resend::OnlyOne),
        ("a block GET", Op::Get { id: [1; 32] }, Resend::AfterBound),
        ("the head register's read", Op::ReadHead { label: Label::Head }, Resend::AfterBound),
        ("a site's read", Op::ReadHead { label: Label::Site("a".into()) }, Resend::AfterBound),
        ("a Sign", sign, Resend::OnlyOne),
        ("a Held batch", Op::AskHeld { batch: 1, ids: vec![[1; 32]] }, Resend::OnlyOne),
        ("the signer's registration", Op::Ext(Ext::RegisterSigner), Resend::OnlyOne),
        ("the signer's first request", Op::Ext(Ext::SignerFirst), Resend::OnlyOne),
        ("the signer's record ask", Op::Ext(Ext::AskRecord), Resend::OnlyOne),
    ]
}

#[test]
fn every_op_has_its_stated_resend_and_each_value_is_reached() {
    for (what, op, want) in every_op() {
        assert_eq!(resend(&op), want, "{what}: resend changed");
    }
    // THE CONTROL: the classifier is not a constant -- all three answers are reached.
    let seen: std::collections::BTreeSet<String> = every_op().iter().map(|(_, op, _)| format!("{:?}", resend(op))).collect();
    assert_eq!(seen.len(), 3, "resend reaches only {seen:?}");
}

#[test]
fn the_only_rto_resend_cell_is_duplicates_interactive() {
    for r in [Resend::Duplicates, Resend::AfterBound, Resend::OnlyOne] {
        for lane in [Lane::Interactive, Lane::Background] {
            let want = if (r, lane) == (Resend::Duplicates, Lane::Interactive) { InFlight::RtoResend } else { InFlight::OneInFlight };
            assert_eq!(in_flight(r, lane), want, "in_flight({r:?}, {lane:?})");
        }
    }
}
