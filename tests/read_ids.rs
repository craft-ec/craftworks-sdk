//! Reads are keyed by `req_id` alone in the engine, so a read's id must be
//! unique across every kind of read this session sends — and a load must
//! never be recorded on rows that are not in its range (craftworks-sdk#166,
//! pieces 1 and 2; the architect's trace and probe).
//!
//! Piece 3 — reads keyed by `(client, req_id)` and their replies naming the
//! session, as write states do — is engine work and not here. Until it lands
//! two TABS can still share a read id; piece 2 is what stops that from
//! recording a wrong empty.

use craftworks_sdk::loads::Page;
use craftworks_sdk::{Loads, Refresh};
use protocol::{Bound, Op, Reply, Request};
use testkit::page_node::{PageNode, Served};

const AT: protocol::At = protocol::At { seq: 1, root: [1u8; 32] };

fn row(k: &str) -> (Vec<u8>, Vec<u8>) {
    (k.as_bytes().to_vec(), vec![0u8])
}

/// **A load handed ANOTHER range's page records nothing.** Before: the load
/// took it, asked for "the rest of d/notes after d/tasks/0004" (past its own
/// end, so empty), and recorded d/notes as LOADED with none of its rows.
#[test]
fn a_page_of_rows_outside_the_load_is_refused_and_the_load_ends_unavailable() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"d/notes/", b"d/notes0", 0).expect("a ticket");
    let tasks: Vec<_> = (0..5).map(|i| row(&format!("d/tasks/{i:04}"))).collect();
    let outcome = l.on_page(id, tasks, Some(b"d/tasks/0004".to_vec()), AT);
    assert!(matches!(outcome, Page::Nothing), "another range's page was taken: {outcome:?}");
    assert_eq!(l.foreign_pages, 1);
    let ended = l.take_ended();
    assert_eq!(ended.len(), 1);
    assert!(matches!(ended[0].1, craftworks_sdk::loads::Ended::Unavailable), "{ended:?}");
    // And the continuation of that wrong page can complete nothing.
    assert!(matches!(l.on_page(id, Vec::new(), None, AT), Page::Nothing));
}

/// A page whose ROWS are in range but whose CURSOR points outside it is the
/// same stranger's page, and is refused too.
#[test]
fn a_cursor_outside_the_load_is_refused() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"d/notes/", b"d/notes0", 0).expect("a ticket");
    let outcome = l.on_page(id, vec![row("d/notes/0000")], Some(b"d/tasks/0004".to_vec()), AT);
    assert!(matches!(outcome, Page::Nothing), "{outcome:?}");
}

/// THE CONTROL: the load's OWN rows page and complete as before — both
/// bounds inclusive/exclusive exactly as the request is.
#[test]
fn control_a_page_inside_the_range_completes() {
    let mut l = Loads::new();
    let (id, _) = l.want(b"d/notes/", b"d/notes0", 0).expect("a ticket");
    match l.on_page(id, vec![row("d/notes/"), row("d/notes/0001")], Some(b"d/notes/0001".to_vec()), AT) {
        Page::More { .. } => {}
        other => panic!("{other:?}"),
    }
    match l.on_page(id, vec![row("d/notes/0002")], None, AT) {
        Page::Complete { rows, .. } => assert_eq!(rows.len(), 3),
        other => panic!("{other:?}"),
    }
    assert_eq!(l.foreign_pages, 0);
}

/// The architect's tree: 120 notes and 120 tasks, published, read COLD.
fn cold_tree() -> PageNode {
    let writer = PageNode::new();
    let mut w = writer.connect();
    w.client(&Request::Identity);
    let mut ops: Vec<Op> = (0..120u32).map(|i| Op::Put(format!("d/notes/{i:04}").into_bytes(), vec![1u8; 900])).collect();
    ops.extend((0..120u32).map(|i| Op::Put(format!("d/tasks/{i:04}").into_bytes(), vec![2u8; 900])));
    w.client(&Request::Write { write_id: 1, ops });
    PageNode::cold_over(&writer)
}

fn range(req_id: u64, domain: &str) -> Request {
    Request::Range {
        req_id,
        lo: Bound::Included(format!("d/{domain}/").into_bytes()),
        hi: Bound::Excluded(format!("d/{domain}0").into_bytes()),
        reverse: false,
        after: None,
        max_entries: 5,
    }
}

/// **The architect's (b), end to end: two tabs' FIRST loads, both id 1, cold
/// and in flight together.** One Page comes back — tab B's d/tasks — and tab
/// A's real `Loads` is handed it. It must never end `Complete`.
#[test]
fn two_tabs_first_loads_sharing_an_id_never_record_a_wrong_empty() {
    let node = cold_tree();
    let mut c = node.connect();
    c.client(&Request::Identity);
    c.hold_answers();
    let mut all = c.client(&range(1, "notes")); // tab A's first load
    all.extend(c.client(&range(1, "tasks"))); // tab B's first load
    while c.held() > 0 {
        all.extend(c.release_one());
    }
    assert!(c.served(Served::Get) > 0, "PRECONDITION: nothing was cold, so nothing parked");
    let mut a = Loads::new();
    let (id, _) = a.want(b"d/notes/", b"d/notes0", 0).expect("a ticket");
    // Hand tab A every page that comes back under its id — FOLLOWING a
    // `More` the way the session does (the same ticket, the rest of the
    // range). The wrong `Complete` is reached only on the continuation.
    let mut seen = 0;
    let mut replies = all;
    for _ in 0..10 {
        let mut next = Vec::new();
        for x in &replies {
            if let Ok(Reply::Page { req_id, entries, cursor, at, .. }) = protocol::decode_reply(x) {
                if req_id != id {
                    continue;
                }
                seen += 1;
                match a.on_page(req_id, entries, cursor, at) {
                    Page::Complete { lo, rows, .. } => panic!(
                        "{:?} recorded as LOADED with {} rows, none of them its own",
                        String::from_utf8_lossy(&lo),
                        rows.len()
                    ),
                    Page::More { lo, hi, after } => {
                        let mut more = c.client(&Request::Range {
                            req_id,
                            lo: Bound::Included(lo),
                            hi: Bound::Excluded(hi),
                            reverse: false,
                            after: Some(after),
                            max_entries: 5,
                        });
                        while c.held() > 0 {
                            more.extend(c.release_one());
                        }
                        next.extend(more);
                    }
                    _ => {}
                }
            }
        }
        if next.is_empty() {
            break;
        }
        replies = next;
    }
    assert!(seen > 0, "no page came back for id {id}, so the check above saw nothing");
}

/// **Piece 1, the one-tab variant (the architect's (c)): a load and a
/// refresh from ONE counter are two reads, and both are answered.** With two
/// counters each starting at 1 they were the same read: one reply, and the
/// range load waited out its budget.
#[test]
fn one_tabs_load_and_refresh_are_two_reads_and_both_are_answered() {
    let node = cold_tree();
    let mut c = node.connect();
    c.client(&Request::Identity);
    let mut loads = Loads::new();
    let mut refresh = Refresh::new();
    let (load_id, _) = loads.want(b"d/notes/", b"d/notes0", 0).expect("a ticket");
    let ask = refresh
        .ask(loads.take_id(), "tasks", b"d/tasks/", b"d/tasks0")
        .expect("an ask");
    let Request::ChangesSince { req_id: refresh_id, .. } = ask else { unreachable!() };
    assert_ne!(load_id, refresh_id, "one tab's load and refresh were given one id");

    c.hold_answers();
    // Five entries, as the architect's probe: a full page of a cold range is
    // the read overflow (sdk#150, the matrix's W2), a different defect.
    let mut all = c.client(&range(load_id, "notes"));
    all.extend(c.client(&ask));
    while c.held() > 0 {
        all.extend(c.release_one());
    }
    for k in 1..=5u64 {
        all.extend(c.tick_at(1_790_000_000_000 + k * 1000));
    }
    let answered: std::collections::BTreeSet<u64> = all
        .iter()
        .filter_map(|x| protocol::decode_reply(x).ok())
        .filter_map(|r| match r {
            Reply::Page { req_id, .. }
            | Reply::Delta { req_id, .. }
            | Reply::FullReloadRequired { req_id, .. }
            | Reply::Unavailable { req_id, .. } => Some(req_id),
            _ => None,
        })
        .collect();
    assert!(answered.contains(&load_id), "the range load was never answered: {answered:?}");
    assert!(answered.contains(&refresh_id), "the refresh was never answered: {answered:?}");
}
