//! THE AUDIT IS A READER (sdk#530; the architect's H1; WANTED-LIFE W1, W7, W8, A1-A3): the assets audit's pass is one
//! more holder of a block's (block, GET) key, so no other reader's end can end the GET it waits on; only the NODE's
//! answer serves it -- never bytes the page holds or rebuilt; and it pins nothing.
use engine::{ClientId, Effect, Event, PagePins, PassId, Params};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

mod common;
use common::{cold_reader, tree};

fn records() -> BTreeMap<Vec<u8>, Vec<u8>> {
    (0..600u32).map(|i| (format!("a/{i:05}").into_bytes(), format!("value {i}").into_bytes())).collect()
}

fn fetched(fx: &[Effect]) -> Vec<Cid> {
    fx.iter().filter_map(|f| if let Effect::FetchBlock { id, .. } = f { Some(*id) } else { None }).collect()
}

fn unwanted(fx: &[Effect], id: Cid) -> bool {
    fx.iter().any(|f| matches!(f, Effect::Unwanted { id: x } if *x == id))
}

fn answered(fx: &[Effect]) -> Vec<(Cid, Vec<PassId>, bool)> {
    fx.iter()
        .filter_map(|f| if let Effect::AuditAnswered { id, passes, found } = f { Some((*id, passes.clone(), *found)) } else { None })
        .collect()
}

/// A cold reader whose read of one key is parked on the root: the block it waits on, and the read's id.
fn a_parked_read() -> (engine::Engine<common::Store>, Cid, engine::read::ReqId) {
    let (root, _) = tree(&records());
    let (mut e, _) = cold_reader(root, Params::default());
    let req = engine::read::ReqId(1);
    let fx = e.step(Event::Get { client: ClientId(1), req_id: req, key: b"a/00001".to_vec() });
    let wait = *fetched(&fx).first().expect("THE SETUP: the read asked no block");
    (e, wait, req)
}

/// W1, BOTH ORDERS: an app read and an audit on one block. Whichever leaves first, the GET survives (no `Unwanted`,
/// the other still a reader); the last to leave ends it. Mutant "the audit not counted in any()" -> red.
#[test]
fn a_read_and_an_audit_on_one_block_each_keep_the_get_until_both_leave() {
    for read_leaves_first in [true, false] {
        let (mut e, b, req) = a_parked_read();
        let joined = e.step(Event::AuditWant { id: b, pass: PassId(7) });
        assert!(fetched(&joined).is_empty(), "the audit asked a second GET of a block a read already asks (W5)");
        let first = if read_leaves_first { e.supersede_read(req).expect("the read is superseded") } else { e.step(Event::AuditDrop { id: b, pass: PassId(7) }) };
        assert!(!unwanted(&first, b), "read_leaves_first={read_leaves_first}: the GET ended while the other reader still waits");
        assert!(e.readers_of(&b).any(), "read_leaves_first={read_leaves_first}: the block lost its remaining reader");
        assert_eq!(e.readers_of(&b).audits, read_leaves_first, "read_leaves_first={read_leaves_first}: the wrong reader remains");
        let last = if read_leaves_first { e.step(Event::AuditDrop { id: b, pass: PassId(7) }) } else { e.supersede_read(req).expect("the read is superseded") };
        assert!(unwanted(&last, b), "read_leaves_first={read_leaves_first}: the last reader left and the GET was not ended");
    }
}

/// A2: the NODE's answer answers every pass waiting, found, and takes them. Mutant "served not taking the audits" ->
/// red (the pass never hears).
#[test]
fn the_nodes_answer_answers_the_audit_found() {
    let (root, all) = tree(&records());
    let (mut e, _) = cold_reader(root, Params::default());
    let fx = e.step(Event::AuditWant { id: root, pass: PassId(3) });
    assert_eq!(fetched(&fx), vec![root], "the audit's want asked no GET");
    let bytes = all.get(&root).expect("the root").to_vec();
    let fx = e.step(Event::BlockArrived { id: root, bytes });
    assert_eq!(answered(&fx), vec![(root, vec![PassId(3)], true)], "the node's answer did not answer the audit");
    assert!(!e.readers_of(&root).audits, "the answered audit is still a reader");
}

/// A3: a NotFound answers the audit ABSENT and takes it; the read waiting on the same block keeps waiting (rule 8).
#[test]
fn a_not_found_answers_the_audit_absent_and_the_read_keeps_waiting() {
    let (mut e, b, _) = a_parked_read();
    let _ = e.step(Event::AuditWant { id: b, pass: PassId(4) });
    let fx = e.step(Event::BlockMissed(b));
    assert_eq!(answered(&fx), vec![(b, vec![PassId(4)], false)], "the NotFound did not answer the audit absent");
    assert!(!e.readers_of(&b).audits);
    assert!(e.readers_of(&b).reads, "the read stopped waiting on a NotFound");
}

/// W7: a HELD block's audit still asks the node (held never suppresses an audit's ask), and a LOCAL landing -- the page
/// serving held bytes, or a rebuild -- answers NO audit; only the node's answer does. Mutant "E2r answers audits" -> red.
#[test]
fn only_the_nodes_answer_serves_an_audit_never_held_bytes() {
    let (root, all) = tree(&records());
    let (mut e, store) = cold_reader(root, Params::default());
    let bytes = all.get(&root).expect("the root").to_vec();
    store.put(root, &bytes);
    let fx = e.step(Event::AuditWant { id: root, pass: PassId(5) });
    assert_eq!(fetched(&fx), vec![root], "a held block's audit asked no GET: held suppressed it");
    let local = e.step(Event::BlockLanded { id: root, bytes: bytes.clone() });
    assert!(answered(&local).is_empty(), "a LOCAL landing answered the audit: {:?}", answered(&local));
    assert!(e.readers_of(&root).audits, "the audit stopped waiting on a local landing");
    let node = e.step(Event::BlockArrived { id: root, bytes });
    assert_eq!(answered(&node), vec![(root, vec![PassId(5)], true)], "the node's answer did not answer the audit");
}

/// W8: an audit pins nothing -- an audit of N blocks leaves the engine's pins as they were.
#[test]
fn an_audit_pins_nothing() {
    let (root, all) = tree(&records());
    let (mut e, _) = cold_reader(root, Params::default());
    let none = BTreeSet::new();
    let before = e.pins(&PagePins { held_asks: &none });
    let n = freenet_prolly::node::Node::parse(all.get(&root).expect("the root")).expect("a node");
    let blocks: Vec<Cid> = std::iter::once(root).chain((0..n.len().min(5)).filter(|_| !n.is_leaf()).map(|i| n.child(i).0)).collect();
    for (i, b) in blocks.iter().enumerate() {
        let _ = e.step(Event::AuditWant { id: *b, pass: PassId(i as u64) });
        let bytes = all.get(b).expect("held in the fixture").to_vec();
        let _ = e.step(Event::BlockArrived { id: *b, bytes });
    }
    assert_eq!(e.pins(&PagePins { held_asks: &none }), before, "an audit of {} blocks changed the pins", blocks.len());
}
