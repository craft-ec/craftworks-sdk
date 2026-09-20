//! Acceptance 4 and 5: the read-back rule, and what the shell does with
//! things it does not understand. No wasm, no node.

use engine::{Op, Params, State};
use engine_delegate::shell::{Inbound, Shell};
use engine_delegate::wire::{Dropped, Reply, Request};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// The node's store, as the shell sees it.
#[derive(Clone, Default)]
struct Store(Rc<RefCell<BTreeMap<Cid, &'static [u8]>>>);

impl Blocks for Store {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.borrow().get(cid).copied()
    }
}

impl Store {
    fn put(&self, id: Cid, bytes: &[u8]) {
        if self.0.borrow().contains_key(&id) {
            return;
        }
        let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        self.0.borrow_mut().insert(id, leaked);
    }
}

fn states(out: &[Vec<u8>]) -> Vec<State> {
    out.iter()
        .filter_map(|b| bincode::deserialize::<Reply>(b).ok())
        .filter_map(|r| match r {
            Reply::Write { state, .. } => Some(state),
            _ => None,
        })
        .collect()
}

fn write_req() -> Vec<u8> {
    bincode::serialize(&Request::Write {
        client: 1,
        write_id: 1,
        ops: vec![(b"k".to_vec(), Op::Put(vec![3u8; 40]))],
    })
    .unwrap()
}

/// A `PutContractResponse` is NOT a confirmation; the read-back is.
///
/// Acceptance 4. The control is a shell told to treat the ack as
/// confirmation — which is the same code path with one substitution — and the
/// test distinguishes them by what the client is told. Without the control,
/// "the write published" would read identically in both.
#[test]
fn an_ack_is_not_a_confirmation_and_the_read_back_is() {
    let store = Store::default();
    let mut s: Shell<Store> = Shell::resume(&[], Params::default(), store.clone());

    let out = s.handle(vec![Inbound::Client(write_req())]);
    assert_eq!(
        states(&out.replies).first(),
        Some(&State::Accepted),
        "the write was not accepted"
    );
    let puts: Vec<Cid> = out
        .ops
        .iter()
        .filter_map(|o| match o {
            engine_delegate::schedule::Op::Put { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert!(!puts.is_empty(), "the write shipped nothing to acknowledge");

    // Every put is ACKNOWLEDGED. Nothing may publish on that alone.
    let acks: Vec<Inbound> = puts
        .iter()
        .map(|id| Inbound::PutAcked { id: *id, ok: true })
        .collect();
    let out = s.handle(acks);
    assert!(
        !states(&out.replies).contains(&State::Published),
        "the write PUBLISHED on acknowledgements alone: the node has said it \
         accepted the requests, not that the bytes are anywhere"
    );
    assert_eq!(
        s.awaiting(),
        puts.len(),
        "the acks did not queue a read-back for every put"
    );
    // ...and the shell asked for each of them back.
    let asked: Vec<Cid> = out
        .ops
        .iter()
        .filter_map(|o| match o {
            engine_delegate::schedule::Op::Get { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    for p in &puts {
        assert!(asked.contains(p), "no read-back was issued for {p:?}");
    }
    println!(
        "  {} put(s) acknowledged, {} read-back(s) issued, nothing published",
        puts.len(),
        asked.len()
    );
}

/// A put that is acknowledged and then NOT there fails, rather than hanging.
///
/// This is the case the read-back exists for. The ack said yes; the GET says
/// the bytes are not on the node. A shell that believed the ack would have
/// told the client `Published` and the data would simply be missing.
#[test]
fn a_put_acknowledged_but_not_readable_back_is_a_failure_not_a_publish() {
    let store = Store::default();
    let mut s: Shell<Store> = Shell::resume(&[], Params::default(), store.clone());
    let out = s.handle(vec![Inbound::Client(write_req())]);
    let put = out
        .ops
        .iter()
        .find_map(|o| match o {
            engine_delegate::schedule::Op::Put { id, .. } => Some(*id),
            _ => None,
        })
        .expect("a put");

    let _ = s.handle(vec![Inbound::PutAcked { id: put, ok: true }]);
    assert_eq!(s.awaiting(), 1);
    // The read-back comes back empty.
    let out = s.handle(vec![Inbound::GotState {
        id: put,
        bytes: None,
    }]);
    assert!(
        !states(&out.replies).contains(&State::Published),
        "a put whose read-back found nothing was treated as published"
    );
    assert_eq!(s.awaiting(), 0, "the read-back is still outstanding");
    // The core re-emitted it, which is what a failed put must produce.
    assert!(
        !out.ops.is_empty(),
        "a failed read-back produced no retry, so the commit waits for ever"
    );
    println!(
        "  ack + empty read-back: not published, {} op(s) re-emitted",
        out.ops.len()
    );
}

/// Anything the shell does not understand is dropped and COUNTED.
///
/// Acceptance 5: garbage, an empty message, and a response for a contract
/// nobody is waiting on. None of them may panic, and none may pass silently —
/// a client talking in a format the shell cannot read would otherwise wait
/// for ever for a reply to a message that was never read.
#[test]
fn what_the_shell_cannot_understand_is_dropped_and_counted() {
    let store = Store::default();
    let mut s: Shell<Store> = Shell::resume(&[], Params::default(), store.clone());

    let out = s.handle(vec![
        Inbound::Client(vec![]),
        Inbound::Client(vec![0xFF; 64]),
        Inbound::Client(b"not bincode at all".to_vec()),
        Inbound::Other,
    ]);
    assert_eq!(
        out.dropped.len(),
        4,
        "{} of 4 unusable messages were counted",
        out.dropped.len()
    );
    assert_eq!(
        out.dropped
            .iter()
            .filter(|d| **d == Dropped::Unparseable)
            .count(),
        3
    );
    assert_eq!(
        out.dropped
            .iter()
            .filter(|d| **d == Dropped::NotForUs)
            .count(),
        1
    );
    assert!(out.ops.is_empty(), "garbage produced a node operation");

    // A state for a contract nobody asked about: not a panic, and not a
    // confirmation of anything.
    let out = s.handle(vec![Inbound::GotState {
        id: [9u8; 32],
        bytes: Some(vec![1, 2, 3]),
    }]);
    assert!(
        states(&out.replies).is_empty(),
        "an unrelated state told a client something"
    );

    // The control: a well-formed request in the same shell IS understood, so
    // the counts above are not simply a shell that drops everything.
    let out = s.handle(vec![Inbound::Client(write_req())]);
    assert_eq!(
        states(&out.replies).first(),
        Some(&State::Accepted),
        "a well-formed write was dropped too, so this test shows only that \
         the shell rejects everything"
    );
    println!("  4 unusable messages dropped and counted; a good one still works");
}

/// The shell survives being rebuilt from its context between every call.
///
/// Which is not a stress test: it is what a delegate DOES. Anything the shell
/// keeps in memory is gone by the next message.
#[test]
fn the_shell_works_when_rebuilt_from_its_context_between_every_call() {
    let store = Store::default();
    let mut ctx: Vec<u8> = Vec::new();
    let mut every_state: Vec<State> = Vec::new();

    let mut inbound = vec![Inbound::Client(write_req())];
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 1000, "the write never settled");
        let mut s: Shell<Store> = Shell::resume(&ctx, Params::default(), store.clone());
        let out = s.handle(std::mem::take(&mut inbound));
        ctx = s.to_context().expect("a context after every call");
        every_state.extend(states(&out.replies));
        // Nothing may be left holding at the end of a call. The scheduler
        // does not survive one, so anything still queued is GONE and the core
        // will not emit it again — a silently lost write.
        assert_eq!(
            out.stranded, 0,
            "call {guard} ended with {} effect(s) still queued; the scheduler \
             does not survive a call, so they are lost",
            out.stranded
        );
        println!(
            "    call {guard}: {} op(s) {:?}, stranded {}, states {:?}",
            out.ops.len(),
            out.ops
                .iter()
                .map(|o| match o {
                    engine_delegate::schedule::Op::Put { .. } => "put",
                    engine_delegate::schedule::Op::Get { .. } => "get",
                    engine_delegate::schedule::Op::Head { .. } => "head",
                    engine_delegate::schedule::Op::ReadHead { .. } => "readhead",
                })
                .collect::<Vec<_>>(),
            out.stranded,
            states(&out.replies)
        );
        if every_state.contains(&State::Published) {
            break;
        }
        // The node does what the ops ask, and answers.
        let mut next = Vec::new();
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    store.put(id, &bytes);
                    next.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = store.get(&id).map(|b| b.to_vec());
                    next.push(Inbound::GotState { id, bytes: held });
                }
                // The head is written to the Register and read back by the
                // same rule as a block: the node acknowledges the write, and
                // only reading the expected seq back publishes the commit.
                engine_delegate::schedule::Op::Head { seq, root } => {
                    next.push(Inbound::GotHead { seq, root });
                }
                engine_delegate::schedule::Op::ReadHead { .. } => {
                    next.push(Inbound::NoHead);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        inbound = next;
    }
    assert!(
        every_state.contains(&State::Accepted),
        "the write was never accepted across {guard} calls: {every_state:?}"
    );
    assert!(
        every_state.contains(&State::Published),
        "the write never PUBLISHED across {guard} calls: {every_state:?}. \
         Reaching only Accepted means the head was written and never \
         confirmed, and a shell that stops there has told a client its data \
         is saved when the head nobody read back may not be there."
    );
    println!("  rebuilt from context on every call: {guard} calls, states {every_state:?}");
}
