//! An app's PUT (a published web container) goes through the PAGE's sender:
//! the same RTO deadline and re-send as every op, and an END — acknowledged,
//! refused in the node's words, or given up at `APP_PUT_BUDGET_MS`. On the
//! real network the builder's own 60 s wait with one re-PUT "on a dropped
//! socket" left a silent node pending for ever (2026-09-23 rehearsal).

use engine::Params;
use page::rto::RTO_MAX_MS;
use page::{AppPut, Answer, Ms, Op, Page, PutPath, APP_PUT_BUDGET_MS};

const RTO_MAX: u64 = RTO_MAX_MS as u64;
const KEY: &str = "8YwvenTZJgTESoiutjWRR2zYZVg1HkNBYoQ347ST66Du";

/// The app PUTs of `key` the page sent, in order, and when.
fn app_puts(ops: &[Op], key: &str) -> usize {
    ops.iter().filter(|o| matches!(o, Op::PutApp { key: k } if k == key)).count()
}

/// Drive the page's clock from `from` to `to` in 100 ms steps, answering
/// nothing; the app PUTs it sent on the way, with their times.
fn silent(p: &mut Page, from: u64, to: u64) -> Vec<u64> {
    let mut sent = Vec::new();
    let mut now = from;
    while now <= to {
        p.tick(Ms(now));
        for _ in 0..app_puts(&p.take_ops(), KEY) {
            sent.push(now);
        }
        now += 100;
    }
    sent
}

#[test]
fn a_put_nobody_answers_is_re_sent_and_ends_named_within_its_budget() {
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.tick(Ms(0));
    p.put_app(KEY.into(), Ms(0));
    assert_eq!(app_puts(&p.take_ops(), KEY), 1, "the PUT did not go out at once");
    assert_eq!(p.app_put(KEY), Some(&AppPut::Pending));

    let end = APP_PUT_BUDGET_MS + RTO_MAX;
    let resent = silent(&mut p, 100, end);
    println!("re-sent {} times, at {:?} ms; ended as {:?}", resent.len(), resent, p.app_put(KEY));
    assert!(resent.len() >= 2, "a silent node's PUT was re-sent {} time(s)", resent.len());
    match p.app_put(KEY) {
        Some(AppPut::GaveUp(why)) => {
            assert!(why.contains(KEY) && why.contains("did not acknowledge"), "the end does not say what: {why}");
        }
        other => panic!("{} ms of silence did not end the PUT: {other:?}", end),
    }
    // Ended means ENDED: nothing more goes out for it.
    assert!(silent(&mut p, end + 100, end + 3 * RTO_MAX).is_empty(), "a given-up PUT was still sent");
}

#[test]
fn a_put_answered_only_on_its_re_send_succeeds() {
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.tick(Ms(0));
    p.put_app(KEY.into(), Ms(0));
    assert_eq!(app_puts(&p.take_ops(), KEY), 1);
    // The first one is lost: nothing answers it. The page must send it again.
    let resent = silent(&mut p, 100, 30_000);
    assert!(!resent.is_empty(), "the lost PUT was never sent again: nothing could answer it");
    p.answer(Answer::AppPutOk(KEY.into()), Ms(resent[0] + 50));
    assert_eq!(p.app_put(KEY), Some(&AppPut::Put));
    assert!(silent(&mut p, 30_100, 30_100 + 3 * RTO_MAX).is_empty(), "an acknowledged PUT was sent again");
}

#[test]
fn a_refusal_ends_it_in_the_nodes_words_and_late_or_foreign_answers_change_nothing() {
    let mut p = Page::new(Params::default(), PutPath::Page);
    p.tick(Ms(0));
    p.put_app(KEY.into(), Ms(0));
    p.put_app("other".into(), Ms(0));
    let _ = p.take_ops();
    // Somebody else's answer settles nothing of ours.
    p.answer(Answer::AppPutOk("never-put".into()), Ms(10));
    assert_eq!((p.app_put(KEY), p.app_put("never-put")), (Some(&AppPut::Pending), None));
    p.answer(Answer::AppPutRefused { key: KEY.into(), said: "invalid put".into() }, Ms(20));
    assert_eq!(p.app_put(KEY), Some(&AppPut::Refused("invalid put".into())));
    assert_eq!(p.app_put("other"), Some(&AppPut::Pending), "a refusal settled the wrong PUT");
    // A late answer to the ended one does not flip it.
    p.answer(Answer::AppPutOk(KEY.into()), Ms(30));
    assert_eq!(p.app_put(KEY), Some(&AppPut::Refused("invalid put".into())));
    // Asking again after it ENDED starts it over; asking while pending does not
    // send a second copy.
    p.put_app(KEY.into(), Ms(0));
    p.put_app("other".into(), Ms(0));
    let ops = p.take_ops();
    assert_eq!((app_puts(&ops, KEY), app_puts(&ops, "other")), (1, 0));
    assert_eq!(p.app_put(KEY), Some(&AppPut::Pending));
}
