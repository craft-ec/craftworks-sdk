//! THE ONE BACKGROUND LANE (KEEPER §7, OBSERVABILITY §3; the architect's design): one Background op on the node's
//! one queue at a time, outside the interactive GET window, and PROMOTED -- once, never back -- when an
//! interactive waiter joins it. Nothing on main is Background yet, so these tests class waits Background through
//! `test_background`, the stand-in for the audit's and the observation tree's arms of `lane_of`.
use crate::*;

/// A page whose opening head read is answered: nothing on the wire (OP-LIFE's one fixture).
fn page() -> Page {
    super::scenarios::page()
}

fn put(i: u8) -> (Waiting, Op) {
    (Waiting::Put([i; 32]), Op::Put { id: [i; 32], bytes: vec![i] })
}

fn get(i: u8) -> (Waiting, Op) {
    (Waiting::Get([i; 32]), Op::Get { id: [i; 32] })
}

fn background(p: &mut Page, (w, op): (Waiting, Op)) {
    p.test_background.insert(w.clone());
    p.send(w, op);
}

/// (a) **ONE Background op on the wire**, across a burst of ten, in order: each answer lets the next out.
#[test]
fn a_burst_of_background_ops_goes_one_at_a_time_in_order() {
    let mut p = page();
    for i in 1..=10 {
        background(&mut p, put(i));
    }
    let mut sent = Vec::new();
    for i in 1..=10u8 {
        let ops = p.take_ops();
        assert_eq!(p.background_in_flight(), 1, "{} Background ops on the wire", p.background_in_flight());
        assert_eq!(ops, vec![put(i).1], "the {i}th Background op did not go out alone and in order");
        sent.push(i);
        p.answered(&put(i).0);
    }
    assert_eq!(sent, (1..=10).collect::<Vec<_>>());
    assert_eq!((p.background_in_flight(), p.bg_queued().len()), (0, 0));
}

/// (b) **A Background GET never takes the interactive window**: with the window full and an interactive GET
/// queued behind it, a Background GET goes out on its own slot, and the window's next place goes to the
/// interactive GET.
#[test]
fn a_background_get_never_takes_an_interactive_place() {
    let mut p = page();
    let n = p.window.size();
    for i in 1..=(n as u8 + 1) {
        let (w, op) = get(i);
        p.send(w, op);
    }
    assert_eq!((p.gets_in_flight(), p.get_queued().len()), (n, 1), "THE SETUP: the window is not full with one queued");
    background(&mut p, get(200));
    assert!(p.dl().get(&get(200).0).is_some_and(|d| d.sent), "the Background GET waited on the interactive window");
    assert_eq!(p.gets_in_flight(), n, "the Background GET counts in the interactive window");
    let _ = p.take_ops();
    p.answered(&get(1).0);
    let queued = get(n as u8 + 1).0;
    assert!(p.dl().get(&queued).is_some_and(|d| d.sent && d.lane == Lane::Interactive), "the freed place did not go to the queued interactive GET");
}

/// (c) **A Background GET neither grows nor shrinks the interactive window**: answered, or timed out.
#[test]
fn a_background_gets_answer_and_loss_leave_the_window_alone() {
    let mut p = page();
    let size = p.window.size();
    background(&mut p, get(1));
    p.answered(&get(1).0);
    assert_eq!(p.window.size(), size, "a Background GET's answer grew the interactive window");
    background(&mut p, get(2));
    // Its first deadline only makes it SILENT (sdk#447: the node's own GET is not over); the moved deadline, past
    // that node GET's bound, is the LOSS.
    let due = p.dl()[&get(2).0].at;
    p.tick(Ms(due));
    assert!(p.dl()[&get(2).0].silent, "THE SETUP: the first deadline did not make it silent");
    let lost_at = p.dl()[&get(2).0].at;
    p.tick(Ms(lost_at));
    assert_eq!(p.window.size(), size, "a Background GET's loss shrank the interactive window");
    assert!(p.dl().get(&get(2).0).is_some_and(|d| d.lane == Lane::Background), "the lost Background GET's re-send left its lane");
    assert!(!p.get_queued().contains(&[2; 32]), "the lost Background GET's re-send queued for the interactive window");
    let overlap = match p.ops.need(&get(2).0) {
        Some(op_life::Entry::OnWire { send, .. }) => send.overlap,
        Some(op_life::Entry::Queued { overlap, .. }) => *overlap,
        Some(op_life::Entry::Parked { .. }) | None => false,
    };
    assert!(overlap, "the lost Background GET's re-send at its bound is not marked an overlap (F1; sdk#447's one overlap rule)");
}

/// (d) **A queued Background op that is withdrawn never goes out.**
#[test]
fn a_withdrawn_queued_background_op_is_never_sent() {
    let mut p = page();
    background(&mut p, put(1));
    background(&mut p, put(2));
    let _ = p.take_ops();
    p.on(&put(2).0, op_life::OpEvent::Withdraw);
    p.answered(&put(1).0);
    assert!(p.take_ops().is_empty(), "the withdrawn queued Background op went out");
}

/// (e) **The person is never shown background work waiting.**
#[test]
fn not_answering_never_names_background_work() {
    let mut p = page();
    background(&mut p, put(1));
    assert_eq!(p.not_answering(), None, "a Background op was shown as not answering");
    let (w, op) = put(2);
    p.send(w, op);
    assert!(p.not_answering().is_some(), "THE CONTROL: an interactive op was not shown");
}

/// (f) **PROMOTION, queued**: a Background GET of X waits behind another Background op; an interactive waiter
/// joins X (the engine may emit no new fetch for it) -- at the next engine step X goes out NOW, as interactive
/// work, counted in the interactive window.
#[test]
fn a_queued_background_get_an_interactive_waiter_joins_goes_out_now() {
    let mut p = page();
    background(&mut p, put(1));
    background(&mut p, get(7));
    assert!(p.bg_queued().iter().any(|(w, _)| *w == get(7).0), "THE SETUP: X is not queued behind the slot");
    let before = p.gets_in_flight();
    // An app read joins X: its class is now Interactive.
    p.test_background.remove(&get(7).0);
    let now = p.now;
    p.tick(Ms(now));
    let d = p.dl().get(&get(7).0).cloned().expect("X was not sent");
    assert!(d.sent && d.lane == Lane::Interactive, "X did not go out as interactive work");
    assert_eq!(p.gets_in_flight(), before + 1, "the promoted GET is not counted in the interactive window");
    assert_eq!(p.background_in_flight(), 1, "THE CONTROL: the other Background op lost its slot");
}

/// Tick the page to its next due, once.
fn next(p: &mut Page) {
    let t = p.next_due().expect("something is due").0;
    p.tick(Ms(t));
}

/// Is `w`'s key's send in flight?
fn in_flight(p: &Page, w: &Waiting) -> bool {
    p.ops.at_node().any(|(by, ..)| by == w)
}

/// (g) **A WITHDRAWN on-wire Background op KEEPS the slot** (engineer2's finding; OP-LIFE L0/L1): the node still holds
/// it, so the queued Background op does not go out until its answer comes (dropped: it serves nobody) -- and, in a
/// second run, until its node BOUND passes (silent first). An interactive withdrawn GET frees its window place at once,
/// while its send still occupies its key.
#[test]
fn a_withdrawn_background_op_on_the_wire_keeps_the_slot_until_its_answer_or_bound() {
    for by_answer in [true, false] {
        let mut p = page();
        background(&mut p, get(7));
        background(&mut p, put(2));
        let _ = p.take_ops();
        assert!(p.bg_queued().iter().any(|(w, _)| *w == put(2).0), "THE SETUP: the second op is not queued");
        p.on(&get(7).0, op_life::OpEvent::Withdraw);
        assert_eq!(p.background_in_flight(), 1, "the withdrawn on-wire op freed the slot");
        let now = p.now;
        p.tick(Ms(now));
        assert!(p.take_ops().is_empty(), "a second Background op went out beside the withdrawn one");
        if by_answer {
            assert_eq!(p.answered(&get(7).0), None, "the withdrawn op's answer was taken as an answer");
        } else {
            while in_flight(&p, &get(7).0) {
                next(&mut p);
            }
        }
        let _ = p.take_ops();
        assert!(!in_flight(&p, &get(7).0), "the withdrawn op outlived its {}", if by_answer { "answer" } else { "bound" });
        assert!(p.dl().get(&put(2).0).is_some_and(|d| d.sent), "the slot did not go to the queued op after the {}", if by_answer { "answer" } else { "bound" });
    }
    // THE CONTROL: an INTERACTIVE GET withdrawn on the wire frees its window place at once; its send stays on its key.
    let mut p = page();
    let (w, op) = get(9);
    p.send(w.clone(), op);
    p.on(&w, op_life::OpEvent::Withdraw);
    assert!(!p.ops.contains(&w) && p.gets_in_flight() == 0, "an interactive withdrawn GET still waits or holds its place");
    assert!(in_flight(&p, &w), "the withdrawn GET's send left its key before its answer or bound (L0)");
}

/// **WANTED AGAIN BY AN INTERACTIVE WAITER: IT RIDES THE SEND IN FLIGHT** (OP-LIFE L0, L6): a Background GET of X
/// withdrawn on the wire, then an app's read needs X -- the read JOINS the one send (no second send of the key); the
/// send keeps its lane and the slot until it leaves, and its answer serves the app's read; the slot then frees.
#[test]
fn a_withdrawn_background_get_an_app_read_wants_again_rides_the_send_in_flight() {
    let mut p = page();
    background(&mut p, get(7));
    background(&mut p, put(2));
    let _ = p.take_ops();
    p.on(&get(7).0, op_life::OpEvent::Withdraw);
    assert!(!p.ops.contains(&get(7).0), "THE SETUP: X is not withdrawn");
    p.test_background.remove(&get(7).0);
    let (w, op) = get(7);
    p.send(w, op);
    assert!(p.take_ops().is_empty(), "a second send of X went out beside the one in flight (L0)");
    let d = &p.dl()[&get(7).0];
    assert!(!d.withdrawn && d.lane == Lane::Background, "X did not ride its send in flight (withdrawn {}, lane {:?})", d.withdrawn, d.lane);
    assert_eq!(p.background_in_flight(), 1, "the send in flight left the slot before it left (L6)");
    assert!(p.answered(&get(7).0).is_some(), "the send's answer did not serve the app's read");
    assert!(p.dl().get(&put(2).0).is_some_and(|d| d.sent), "the freed slot did not take the next Background op");
}

/// **ONE END PER OP** (the architect): recorded when the send LEAVES -- withdrawn, wanted again, answered: exactly one
/// end, the answer; withdrawn, then its bound: exactly one end, Withdrawn (nothing is sent again).
#[test]
fn a_withdrawn_op_records_exactly_one_end() {
    use instrument::{Dir, Event, Outcome, Record};
    let ends = |p: &Page| -> Vec<Event> {
        p.recording().expect("recording").events().into_iter().filter(|e| matches!(e, Event::Exit { .. } | Event::Edge { dir: Dir::Response, .. })).collect()
    };
    // withdraw -> wanted again (Background) -> answer
    let mut p = page();
    p.record_into(256);
    background(&mut p, get(7));
    p.on(&get(7).0, op_life::OpEvent::Withdraw);
    let (w, op) = get(7);
    p.send(w, op);
    p.answered(&get(7).0);
    let e = ends(&p);
    assert_eq!(e.len(), 1, "withdrawn, wanted again, answered: {} ends recorded: {e:?}", e.len());
    assert!(matches!(e[0], Event::Edge { dir: Dir::Response, .. }), "the one end is not the answer: {e:?}");
    // withdraw -> bound
    let mut p = page();
    p.record_into(256);
    background(&mut p, get(8));
    p.on(&get(8).0, op_life::OpEvent::Withdraw);
    assert!(ends(&p).is_empty(), "an end was recorded at withdraw, before the send left");
    while in_flight(&p, &get(8).0) {
        next(&mut p);
    }
    let e = ends(&p);
    assert_eq!(e.len(), 1, "withdrawn, then its bound: {} ends: {e:?}", e.len());
    assert!(matches!(e[0], Event::Exit { outcome: Outcome::Withdrawn, .. }), "the one end is not Withdrawn: {e:?}");
}

/// **not_answering_in(lane)**: a Background wait shows ONLY through the Background lane; the plain
/// `not_answering` (what an app shows) is the Interactive lane's.
#[test]
fn a_lanes_longest_wait_is_named_only_in_that_lane() {
    let mut p = page();
    background(&mut p, put(1));
    assert_eq!(p.not_answering(), None);
    assert!(p.not_answering_in(Lane::Background).is_some_and(|(what, _)| what == "a block's save"), "the Background wait was not named in its lane");
    let (w, op) = put(2);
    p.send(w, op);
    assert!(p.not_answering().is_some() && p.not_answering_in(Lane::Interactive) == p.not_answering(), "the plain form is not the Interactive lane's");
}

/// (f) **PROMOTION, on the wire (L6):** an interactive waiter joining an on-wire Background op does not change the
/// send in flight -- it keeps its lane and the slot until it leaves; the promotion applies at the key's NEXT send. It
/// is never demoted back.
#[test]
fn an_on_wire_background_op_an_interactive_waiter_joins_is_promoted_at_its_next_send() {
    let mut p = page();
    background(&mut p, get(7));
    background(&mut p, put(2));
    let _ = p.take_ops();
    p.test_background.remove(&get(7).0);
    let now = p.now;
    p.tick(Ms(now));
    assert_eq!(p.dl()[&get(7).0].lane, Lane::Background, "the send in flight changed lane (L6)");
    assert_eq!(p.background_in_flight(), 1, "the send in flight left the slot");
    // Its bound: the NEXT send goes Interactive, and the slot frees for the queued Background op.
    let first = p.dl()[&get(7).0].seq;
    while p.dl().get(&get(7).0).is_some_and(|d| d.seq == first) {
        next(&mut p);
    }
    let _ = p.take_ops();
    assert!(p.dl().get(&get(7).0).is_none_or(|d| d.lane == Lane::Interactive), "the next send was not promoted");
    assert!(p.dl().get(&put(2).0).is_some_and(|d| d.sent), "the freed slot did not take the next Background op");
    // Never demoted: classing it Background again changes nothing.
    p.test_background.insert(get(7).0);
    let now = p.now;
    p.tick(Ms(now));
    assert!(p.dl().get(&get(7).0).is_none_or(|d| d.lane == Lane::Interactive), "a promoted op was demoted back");
}

/// (L3) **A PARKED key takes its waits' lane NOW:** a Background GET promoted while it rode the wire (an app read
/// joined it) is answered WITHOUT its block; it parks as INTERACTIVE (its re-ask then never queues behind the
/// Background op that took the slot meanwhile). The op record is the witness: the re-ask itself needs an engine reader
/// (E2 ends a parked GET nobody reads), which this fixture has not. The mutant "a parked key keeps its Background
/// lane" fails.
#[test]
fn a_promoted_get_answered_without_parks_interactive() {
    let mut p = page();
    background(&mut p, get(7));
    background(&mut p, put(2));
    assert_eq!(p.take_ops(), vec![get(7).1], "THE SETUP: X did not go out alone in the slot");
    // An app read joins X while it is on the wire: its class is now Interactive (L6: the send's lane stays).
    p.test_background.remove(&get(7).0);
    assert_eq!(p.answered_without(&get(7).0, get(7).1), Some(1), "THE SETUP: the answer without the block did not answer X");
    assert!(
        matches!(p.ops.need(&get(7).0), Some(op_life::Entry::Parked { lane: Lane::Interactive, .. })),
        "the promoted GET parked in the Background lane: {:?}",
        p.ops.need(&get(7).0)
    );
    // THE CONTROL: the slot freed on the answer, and the Background PUT took it.
    assert_eq!(p.take_ops(), vec![put(2).1], "the queued Background PUT did not take the freed slot");
}
