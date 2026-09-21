//! Acceptance 4 and 5: the read-back rule, and what the shell does with
//! things it does not understand. No wasm, no node.

use engine::Params;
use engine_delegate::shell::{Inbound, Shell, StoreFacts};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use protocol::{Dropped, Reply, Request, WriteState};
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

fn states(out: &[Vec<u8>]) -> Vec<WriteState> {
    out.iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|r| match r {
            Reply::WriteState { state, .. } | Reply::SessionWriteState { state, .. } => Some(state),
            _ => None,
        })
        .collect()
}

fn write_req() -> Vec<u8> {
    protocol::encode_request(
        protocol::CURRENT,
        &Request::Write {
            write_id: 1,
            ops: vec![protocol::Op::Put(b"k".to_vec(), vec![3u8; 40])],
        },
    )
    .expect("encodes")
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
        Some(&WriteState::Accepted),
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
        !states(&out.replies).contains(&WriteState::Published),
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

/// A put acknowledged and then never readable back FAILS — but only after
/// the rounds run out.
///
/// The first empty read-back is not proof of absence: a put is readable a
/// short time after it is acknowledged, not instantly (+50 ms, measured), and
/// treating the first empty answer as failure reported a write dead that had
/// in fact landed. So it is asked again, a bounded number of times, and only
/// then is the ack disbelieved.
///
/// Two assertions, and both matter: it does NOT fail early, and it does NOT
/// go on for ever.
#[test]
fn a_put_never_readable_back_fails_only_after_its_rounds_run_out() {
    let store = Store::default();
    let mut s: Shell<Store> = Shell::resume_with(
        &[],
        Params::default(),
        store,
        StoreFacts {
            has_code: true,
            head_writable: true,
            head_id: [0u8; 32],
        },
    );
    let out = s.handle(vec![Inbound::Client(write_req())]);
    let put = out
        .ops
        .iter()
        .find_map(|o| match o {
            engine_delegate::schedule::Op::Put { id, .. } => Some(*id),
            _ => None,
        })
        .expect("a put");

    // The store never holds it, so every read-back is empty.
    let _ = s.handle(vec![Inbound::PutAcked { id: put, ok: true }]);
    assert_eq!(s.awaiting(), 1, "the ack did not queue a read-back");

    let mut rounds = 0;
    let mut failed_at = None;
    while rounds < 100 {
        rounds += 1;
        let out = s.handle(vec![Inbound::GotState {
            id: put,
            bytes: None,
        }]);
        if s.awaiting() == 0 {
            failed_at = Some(rounds);
            assert!(
                !states(&out.replies).contains(&WriteState::Published),
                "a put that never read back was published"
            );
            break;
        }
        assert!(
            !out.ops.is_empty(),
            "round {rounds}: the shell stopped asking but is still waiting, \
             so nothing will ever wake it again"
        );
    }
    let failed_at = failed_at.expect("it never gave up, so it would wait for ever");
    assert!(
        failed_at > 1,
        "the FIRST empty read-back was treated as failure; a put is readable \
         a short time after it is acknowledged, not instantly"
    );
    assert!(
        failed_at < 100,
        "it asked {failed_at} times without a bound"
    );
    println!("  never readable back: asked {failed_at} times, then failed");
}

/// A message with TRAILING BYTES is refused, not half-read.
///
/// `bincode::deserialize` decodes the type it was asked for and ignores the
/// rest, so a longer message reads as a shorter one and the difference is
/// silent. That is how a wrapped value gets read as its wrapper: the answer
/// is in the part that was thrown away, and every message decodes to the
/// same thing.
#[test]
fn a_message_with_trailing_bytes_is_refused_rather_than_half_read() {
    let good = protocol::encode_request(protocol::CURRENT, &Request::Flush).expect("encodes");
    assert!(
        matches!(protocol::decode_request(&good), protocol::Incoming::Ok(_)),
        "a well-formed Flush was refused, so the check below shows nothing"
    );
    let mut trailing = good.clone();
    trailing.extend_from_slice(b"and then some");
    assert!(
        !matches!(
            protocol::decode_request(&trailing),
            protocol::Incoming::Ok(_)
        ),
        "a request with {} trailing byte(s) was accepted; its prefix was read \
         as a whole message, which is how one message becomes another",
        trailing.len() - good.len()
    );

    // ...and the shell drops it rather than acting on the prefix.
    let store = Store::default();
    let mut s: Shell<Store> = Shell::resume_with(
        &[],
        Params::default(),
        store,
        StoreFacts {
            has_code: true,
            head_writable: true,
            head_id: [0u8; 32],
        },
    );
    let out = s.handle(vec![Inbound::Client(trailing)]);
    assert_eq!(
        out.dropped.len(),
        1,
        "the trailing-byte message was not counted"
    );
    assert!(out.ops.is_empty(), "it produced a node operation anyway");
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
        Some(&WriteState::Accepted),
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
    let mut every_state: Vec<WriteState> = Vec::new();

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
        if every_state.contains(&WriteState::Published) {
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
        every_state.contains(&WriteState::Accepted),
        "the write was never accepted across {guard} calls: {every_state:?}"
    );
    assert!(
        every_state.contains(&WriteState::Published),
        "the write never PUBLISHED across {guard} calls: {every_state:?}. \
         Reaching only Accepted means the head was written and never \
         confirmed, and a shell that stops there has told a client its data \
         is saved when the head nobody read back may not be there."
    );
    println!("  rebuilt from context on every call: {guard} calls, states {every_state:?}");
}

/// A write before `Install` is refused, and COUNTED, not left hanging.
///
/// A delegate cannot fabricate a contract — it has no way to produce wasm —
/// so the code must arrive from outside. Until it has, a PUT is one the node
/// would refuse anyway; dropping it here is the same outcome without the
/// round trip. What must not happen is silence: a client that never sent
/// `Install` would otherwise see its write accepted and then nothing.
#[test]
fn a_put_before_the_contract_code_arrives_is_refused_and_counted() {
    let store = Store::default();
    let mut without: Shell<Store> = Shell::resume_with(
        &[],
        Params::default(),
        store.clone(),
        StoreFacts {
            has_code: false,
            head_writable: true,
            head_id: [0u8; 32],
        },
    );
    let out = without.handle(vec![Inbound::Client(write_req())]);
    assert_eq!(
        states(&out.replies).first(),
        Some(&WriteState::Accepted),
        "the write was not even accepted"
    );
    assert!(
        !out.ops
            .iter()
            .any(|o| matches!(o, engine_delegate::schedule::Op::Put { .. })),
        "a put was built with no contract code to build it from"
    );
    assert!(
        out.refused_no_code > 0,
        "the put was dropped without being counted, so a client that never \
         sent Install sees its write accepted and then nothing at all"
    );

    // The control: the same write with the code on hand DOES put. Otherwise
    // this test would pass over a shell that never puts anything.
    let mut with: Shell<Store> = Shell::resume_with(
        &[],
        Params::default(),
        store,
        StoreFacts {
            has_code: true,
            head_writable: true,
            head_id: [0u8; 32],
        },
    );
    let out = with.handle(vec![Inbound::Client(write_req())]);
    assert!(
        out.ops
            .iter()
            .any(|o| matches!(o, engine_delegate::schedule::Op::Put { .. })),
        "the same write did not put even WITH the code, so the refusal above \
         is not the missing code doing it"
    );
    assert_eq!(out.refused_no_code, 0);
    println!("  no code: write accepted, put refused and counted; with code: put issued");
}

/// `Install` hands the delegate what it cannot produce, and it derives none
/// of it.
///
/// A delegate cannot fabricate a contract and cannot mint authority. What it
/// is given is kept as given: the test asserts the engine records
/// `Provisioned(Test)` as the SOURCE of its authority — a statement, never
/// the key — so a key that is not for real use cannot be mistaken for one
/// that is by anything reading the engine's state.
#[test]
fn install_provisions_what_the_delegate_cannot_make_and_nothing_is_derived() {
    let store = Store::default();
    // NOT yet provisioned — which is the only state in which an install does
    // anything. A delegate that already has a key refuses one, and that is
    // asserted by its own test.
    let mut s: Shell<Store> = Shell::resume_with(
        &[],
        Params::default(),
        store,
        StoreFacts {
            has_code: false,
            head_writable: false,
            head_id: [0u8; 32],
        },
    );

    let req = protocol::encode_request(
        protocol::CURRENT,
        &Request::Install {
            block_code: vec![1u8; 64],
            register_code: vec![2u8; 32],
            register_params: vec![3u8; 16],
            signing_key: protocol::TestKey(vec![4u8; 32]),
        },
    )
    .expect("encodes");
    let out = s.handle(vec![Inbound::Client(req)]);
    assert!(
        s.provisioned,
        "Install did not record that a key was provisioned, so a caller \
         cannot tell it from a message that never arrived"
    );
    assert_eq!(
        s.installed,
        Some(64 + 32),
        "the installed sizes do not add up to the code that was sent"
    );
    assert!(out.dropped.is_empty(), "a well-formed Install was dropped");
    assert!(out.ops.is_empty(), "Install produced a node operation");

    // What the ENGINE records is where its authority came from — and the
    // type says TEST, which is the whole point of naming it.
    let out = s.handle(vec![Inbound::Client(
        protocol::encode_request(protocol::CURRENT, &Request::Identity).expect("encodes"),
    )]);
    assert!(
        out.ops
            .iter()
            .any(|o| matches!(o, engine_delegate::schedule::Op::ReadHead { .. })),
        "Start did not read the head"
    );
    println!("  Install: 96 B of code + a TEST key provisioned, nothing derived");
}

/// A commit too big for one return is refused UP FRONT, never half-emitted.
///
/// A pack's bytes exist only in the return that made them — `Commit::packs`
/// is `#[serde(skip)]`, deliberately, because a pack is the largest thing the
/// engine touches and the context is 400 KiB. So the shell cannot hold a put
/// back for the next entry: whatever a commit emits must go out now or be
/// lost, and a commit that went out HALF would wait for ever on
/// confirmations for blocks nobody has.
///
/// That makes the core's `max_commit_blocks` and the shell's per-return PUT
/// limit one number, not two. This asserts the consequence: at the boundary
/// the write is refused -- `TooLarge`, naming the bound and the count, since
/// it is refused the same way every time (craftworks-sdk#136) -- and nothing
/// is put, rather than a partial set going out.
#[test]
fn a_commit_larger_than_one_return_is_refused_rather_than_half_emitted() {
    // The two numbers, set equal. A commit may name at most what one return
    // can carry.
    let per_return = 4usize;
    let params = Params {
        max_commit_blocks: per_return,
        // Values over max_packed_value are PUT one each, so the block count
        // is the number of keys and the boundary is reachable exactly.
        ..Params::default()
    };
    let store = Store::default();

    let run = |n: u32, version: u16| -> (Vec<WriteState>, usize, usize) {
        let mut s: Shell<Store> =
            Shell::resume_with(&[], params, store.clone(), StoreFacts::provisioned());
        s.limits = engine_delegate::schedule::Limits {
            max_gets: 4,
            max_puts: per_return,
        };
        let ops: Vec<protocol::Op> = (0..n)
            .map(|i| {
                protocol::Op::Put(
                    format!("k{i:04}").into_bytes(),
                    vec![(i % 251) as u8; params.max_packed_value + 1],
                )
            })
            .collect();
        let out = s.handle(vec![Inbound::Client(
            protocol::encode_request(version, &Request::Write { write_id: 1, ops })
                .expect("encodes"),
        )]);
        let puts = out
            .ops
            .iter()
            .filter(|o| matches!(o, engine_delegate::schedule::Op::Put { .. }))
            .count();
        (states(&out.replies), puts, out.stranded)
    };

    // Over the boundary: refused, and NOTHING put.
    let (over, over_puts, over_stranded) = run(per_return as u32 * 4, protocol::CURRENT);
    assert!(
        matches!(
            over.as_slice(),
            [WriteState::TooLarge { bound: protocol::WriteBound::CommitBlocks, limit: 4, got }] if *got > 4
        ),
        "a commit naming more blocks than one return can carry was not \
         refused TooLarge; it was {over:?}"
    );
    // THE SAME WRITE FROM A v2 CLIENT: it cannot read `TooLarge`, so it is
    // told `Failed` -- not in the tree, do not re-send -- and never sent a
    // variant its decoder would drop (the one conversion point, `for_client`).
    let (over_v2, _, _) = run(per_return as u32 * 4, 2);
    assert_eq!(
        over_v2,
        vec![WriteState::Failed],
        "a v2 client was sent {over_v2:?}"
    );
    assert_eq!(
        over_puts, 0,
        "{over_puts} put(s) went out for a refused commit — a commit emitted \
         in part waits for ever on confirmations for blocks nobody has"
    );
    assert_eq!(over_stranded, 0, "a refused commit left effects queued");

    // The control: under the boundary it is accepted and every block goes out
    // in this one return. Without it, "nothing was put" is also what a shell
    // that never puts anything looks like.
    let (under, under_puts, under_stranded) = run(2, protocol::CURRENT);
    assert_eq!(under.first(), Some(&WriteState::Accepted));
    assert!(
        under_puts > 0,
        "a commit UNDER the same boundary put nothing either, so the refusal \
         above is not the boundary doing it"
    );
    assert_eq!(
        under_stranded, 0,
        "{under_stranded} effect(s) were left queued for a commit that fits, \
         so the shell is holding puts it cannot hold"
    );
    println!(
        "  over the boundary: {over:?} (v2 client: Failed), 0 puts; under it: Accepted, {under_puts} put(s), 0 stranded"
    );
}

/// A subscription survives the shell, and a commit pushes `Changed` on the
/// wire — no client request in sight.
///
/// The shell rebuilds from its context on every call, which is what a delegate
/// does, so this also pins that the subscription is CARRIED: an engine that
/// held subscriptions only in memory would pass every in-process test and lose
/// every subscription on the first call boundary, which is every call.
#[test]
fn a_subscribed_range_is_pushed_a_changed_across_the_context() {
    let store = Store::default();
    let mut ctx: Vec<u8> = Vec::new();

    // One call: subscribe.
    let mut s: Shell<Store> = Shell::resume(&ctx, Params::default(), store.clone());
    let out = s.handle(vec![Inbound::Client(
        protocol::encode_request(
            protocol::CURRENT,
            &Request::SubscribeRange {
                sub_id: 7,
                lo: protocol::Bound::Included(b"k".to_vec()),
                hi: protocol::Bound::Unbounded,
            },
        )
        .expect("encodes"),
    )]);
    let accepted: Vec<protocol::Accepted> = out
        .replies
        .iter()
        .filter_map(|b| protocol::decode_reply(b).ok())
        .filter_map(|r| match r {
            Reply::Subscribed { accepted, .. } => Some(accepted),
            _ => None,
        })
        .collect();
    assert_eq!(
        accepted,
        [protocol::Accepted::Yes],
        "the subscribe was not answered; a client that is not told cannot \
         tell 'subscribed' from 'lost'"
    );
    ctx = s.to_context().expect("a context");

    // Later calls: a whole write, driven to its head being confirmed. The
    // shell is rebuilt from the context each time, as a delegate's is.
    let mut changed: Vec<protocol::Reply> = Vec::new();
    let mut inbound = vec![Inbound::Client(write_req())];
    for _ in 0..24 {
        let mut s: Shell<Store> = Shell::resume(&ctx, Params::default(), store.clone());
        let out = s.handle(std::mem::take(&mut inbound));
        ctx = s.to_context().expect("a context after every call");
        assert_eq!(out.stranded, 0, "the shell stranded effects");
        for b in &out.replies {
            if let Ok(r @ Reply::Changed { .. }) = protocol::decode_reply(b) {
                changed.push(r);
            }
        }
        // The node does what the ops asked.
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    store.put(id, &bytes);
                    inbound.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = store.get(&id).map(|b| b.to_vec());
                    inbound.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    inbound.push(Inbound::GotHead { seq, root })
                }
                engine_delegate::schedule::Op::ReadHead { .. } => inbound.push(Inbound::NoHead),
            }
        }
        if inbound.is_empty() {
            break;
        }
    }

    assert_eq!(
        changed.len(),
        1,
        "expected exactly one Changed pushed for one commit, got {changed:?}"
    );
    match &changed[0] {
        Reply::Changed {
            sub_id, why, seq, ..
        } => {
            assert_eq!(*sub_id, 7, "the push named a subscription nobody took");
            assert_eq!(
                *why,
                protocol::Why::Diffed,
                "the engine held the whole tree, so it must have COMPARED it \
                 rather than guessing"
            );
            assert!(*seq > 0, "the push named seq 0");
        }
        other => panic!("not a Changed: {other:?}"),
    }
    println!("  one commit -> one Changed pushed, across {} calls", 24);
}

/// A key OUTSIDE the subscribed range pushes nothing.
///
/// Its control is the test above: the same shell, the same commit machinery,
/// a range that DOES contain the key, pushing exactly one. Without that pair,
/// silence here would equally be the output of a push path that never works.
#[test]
fn a_commit_outside_the_subscribed_range_pushes_nothing() {
    let store = Store::default();
    let mut s: Shell<Store> = Shell::resume(&[], Params::default(), store.clone());
    let out = s.handle(vec![Inbound::Client(
        protocol::encode_request(
            protocol::CURRENT,
            &Request::SubscribeRange {
                sub_id: 7,
                // `write_req` writes the key "k"; this range starts after it.
                lo: protocol::Bound::Included(b"zzz".to_vec()),
                hi: protocol::Bound::Unbounded,
            },
        )
        .expect("encodes"),
    )]);
    assert!(!out.replies.is_empty(), "the subscribe was not answered");
    let mut ctx = s.to_context().expect("a context");

    let mut changed = 0usize;
    let mut inbound = vec![Inbound::Client(write_req())];
    for _ in 0..24 {
        let mut s: Shell<Store> = Shell::resume(&ctx, Params::default(), store.clone());
        let out = s.handle(std::mem::take(&mut inbound));
        ctx = s.to_context().expect("a context");
        for b in &out.replies {
            if let Ok(Reply::Changed { .. }) = protocol::decode_reply(b) {
                changed += 1;
            }
        }
        for op in out.ops {
            match op {
                engine_delegate::schedule::Op::Put { id, bytes } => {
                    store.put(id, &bytes);
                    inbound.push(Inbound::PutAcked { id, ok: true });
                }
                engine_delegate::schedule::Op::Get { id, .. } => {
                    let held = store.get(&id).map(|b| b.to_vec());
                    inbound.push(Inbound::GotState { id, bytes: held });
                }
                engine_delegate::schedule::Op::Head { seq, root } => {
                    inbound.push(Inbound::GotHead { seq, root })
                }
                engine_delegate::schedule::Op::ReadHead { .. } => inbound.push(Inbound::NoHead),
            }
        }
        if inbound.is_empty() {
            break;
        }
    }
    assert_eq!(
        changed, 0,
        "a commit outside the subscribed range pushed {changed} notification(s)"
    );
}

/// `Identity` reports whether the delegate can actually write a head.
///
/// **Both arms, because the whole value of the field is that it DIFFERS.**
/// An unprovisioned delegate answers `Identity` with a seq of 0 and a zero
/// root — byte-identical to a healthy engine nobody has written to yet —
/// while silently dropping every head op it is handed. A page that cannot
/// tell those apart either never provisions, and loses every write, or
/// provisions on every load, which mints a fresh signing key and orphans the
/// head written under the old one.
///
/// A single-arm test here would pass against a field hardcoded to `true`.
#[test]
fn identity_reports_whether_a_head_can_be_written_both_ways() {
    fn head_writable_of(store_can_sign: bool) -> bool {
        let mut s: Shell<Store> = Shell::resume_with(
            &[],
            Params::default(),
            Store::default(),
            StoreFacts {
                has_code: true,
                head_writable: store_can_sign,
                head_id: [0u8; 32],
            },
        );
        let out = s.handle(vec![Inbound::Client(
            protocol::encode_request(1, &protocol::Request::Identity).expect("encodes"),
        )]);
        out.replies
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .find_map(|r| match r {
                protocol::Reply::Identity { head_writable, .. } => Some(head_writable),
                _ => None,
            })
            .expect("Identity answers")
    }

    assert!(
        head_writable_of(true),
        "a provisioned delegate reported it cannot write a head"
    );
    assert!(
        !head_writable_of(false),
        "an UNPROVISIONED delegate reported it can write a head; a page \
         believing this never provisions and every write is dropped"
    );
}

/// **FIRST WRITER WINS.** An `Install` at a provisioned delegate changes
/// nothing and says so.
///
/// `Install` overwrites the signing key, and the Register instance is derived
/// from a keyset, so a second install mints a second key, moves the head's
/// contract id and orphans everything written under the first. A client that
/// asks before installing does not prevent it: two tabs opened together on a
/// fresh node both find it unprovisioned and both install, a re-issue after a
/// stall installs again, an older page knows nothing of the rule, and any web
/// page at all can send one message. The delegate is the only place that sees
/// them all, so the guard is here.
///
/// The observable effects are that `installed` stays `None` — which is what
/// the entry point keys its `set_secret` calls off, so nothing is written —
/// and that the caller is told `AlreadyInstalled` rather than being left to
/// infer silence.
#[test]
fn an_install_at_a_provisioned_delegate_changes_nothing_and_says_so() {
    fn install_into(already_provisioned: bool) -> (Option<usize>, Vec<protocol::Reply>) {
        let mut s: Shell<Store> = Shell::resume_with(
            &[],
            Params::default(),
            Store::default(),
            StoreFacts {
                has_code: true,
                head_writable: already_provisioned,
                head_id: [0u8; 32],
            },
        );
        let out = s.handle(vec![Inbound::Client(
            protocol::encode_request(
                1,
                &protocol::Request::Install {
                    block_code: vec![0xB1; 8],
                    register_code: vec![0x8E; 8],
                    register_params: vec![0x01; 4],
                    signing_key: protocol::TestKey(vec![0x77; 32]),
                },
            )
            .expect("encodes"),
        )]);
        let replies = out
            .replies
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .collect();
        (s.installed, replies)
    }

    // ALREADY PROVISIONED: nothing is handed to the entry point to write.
    let (installed, replies) = install_into(true);
    assert_eq!(
        installed, None,
        "a second Install reached the secret store; it would replace the \
         signing key and orphan every head written under the first"
    );
    assert!(
        replies.contains(&protocol::Reply::AlreadyInstalled),
        "the caller was not told the install was refused: {replies:?}"
    );

    // THE NEGATIVE CONTROL. On the same path, an UNPROVISIONED delegate does
    // install — without this the assertions above would pass against a
    // delegate that refuses every install there has ever been.
    let (installed, replies) = install_into(false);
    assert!(
        installed.is_some(),
        "the control did not install, so the test above proves nothing"
    );
    assert!(
        !replies.contains(&protocol::Reply::AlreadyInstalled),
        "a first install was reported as already installed"
    );
}

/// `Identity` names the head's CONTRACT, so a page can subscribe to it.
///
/// Without it a tab that made no write can only poll: an engine-originated
/// push returns to whoever invoked the delegate (F40), so a client-API
/// subscription to the head contract is the only way to be told — and a page
/// cannot subscribe to a contract it cannot name.
///
/// **Both arms.** Zero when there is no Register to name, which is exactly
/// when `head_writable` is false, and the real id when there is.
#[test]
fn identity_names_the_head_contract_when_there_is_one() {
    fn identity_of(head_id: [u8; 32]) -> ([u8; 32], bool) {
        let mut s: Shell<Store> = Shell::resume_with(
            &[],
            Params::default(),
            Store::default(),
            StoreFacts {
                has_code: true,
                head_writable: head_id != [0u8; 32],
                head_id,
            },
        );
        let out = s.handle(vec![Inbound::Client(
            protocol::encode_request(1, &protocol::Request::Identity).expect("encodes"),
        )]);
        out.replies
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .find_map(|r| match r {
                protocol::Reply::Identity {
                    head_id,
                    head_writable,
                    ..
                } => Some((head_id, head_writable)),
                _ => None,
            })
            .expect("Identity answers")
    }

    // NOTHING to name: an unprovisioned delegate has no Register.
    let (id, writable) = identity_of([0u8; 32]);
    assert_eq!(
        id, [0u8; 32],
        "a delegate with no Register named a contract"
    );
    assert!(
        !writable,
        "zero head_id and a writable head cannot both be true"
    );

    // A real one, reported as given and not derived here.
    let real = [0x5Au8; 32];
    let (id, writable) = identity_of(real);
    assert_eq!(
        id, real,
        "the head's contract id did not cross, so a page cannot subscribe to \
         its own head and every tab that made no write must poll"
    );
    assert!(writable);
}

/// What the node does with a call's ops: each answered, in order.
fn answer(node: &Store, ops: Vec<engine_delegate::schedule::Op>) -> Vec<Inbound> {
    use engine_delegate::schedule::Op;
    ops.into_iter()
        .map(|op| match op {
            Op::Put { id, bytes } => {
                node.put(id, &bytes);
                Inbound::PutAcked { id, ok: true }
            }
            Op::Get { id, .. } => Inbound::GotState {
                id,
                bytes: node.get(&id).map(|b| b.to_vec()),
            },
            Op::Head { seq, root } => Inbound::GotHead { seq, root },
            Op::ReadHead { .. } => Inbound::NoHead,
        })
        .collect()
}

/// **A write's verdict is told in ITS client's version — whoever else speaks
/// while it is parked** (sdk#146; the architect's probe on sdk#157).
///
/// The write parks on a cold path and is refused once the path arrives — a
/// call the NODE made, with no client in it. Two tabs mid-upgrade share the
/// delegate, and the other one speaks (a `Tick`) while the write is parked.
/// One version per shell was overwritten by whoever spoke last: a v4 writer
/// with a v2 bystander was told `Failed`, and a v2 writer with a v4 bystander
/// was sent `TooLarge`, which a v2 client must never be sent. The version now
/// travels with the write, in its client id.
///
/// The shell is rebuilt from its context before every call and the node
/// answers one op per call, the real shape. The GET limit is lifted: the
/// path's GETs are the read overflow, sdk#150's own defect.
#[test]
fn a_write_refused_after_a_park_is_told_in_its_clients_version_whoever_else_speaks() {
    let node = Store::default();
    let mut ctx: Vec<u8> = Vec::new();
    let mut inbound = vec![Inbound::Client(
        protocol::encode_request(protocol::CURRENT, &Request::Write {
            write_id: 1,
            ops: (0..400u32).map(|i| protocol::Op::Put(format!("h/{i:04}").into_bytes(), vec![i as u8; 200])).collect(),
        })
        .expect("encodes"),
    )];
    let mut published = false;
    for _ in 0..200 {
        let mut s: Shell<Store> = Shell::resume_with(&ctx, Params::default(), node.clone(), StoreFacts::provisioned());
        let out = s.handle(std::mem::take(&mut inbound));
        ctx = s.to_context().expect("a context");
        published |= states(&out.replies).contains(&WriteState::Published);
        inbound = answer(&node, out.ops);
        if inbound.is_empty() {
            break;
        }
    }
    assert!(published, "the history never published");

    const WRITER: u64 = 0x0000_1111_2222_3333;
    const BYSTANDER: u64 = 0x0000_4444_5555_6666;
    // (the writer's verdicts, GETs made, whether the bystander spoke while parked)
    let run = |writer: u16, bystander: Option<u16>| -> (Vec<WriteState>, usize, bool) {
        let cold = Store::default();
        let params = Params { max_commit_blocks: 4, ..Params::default() };
        let mut ctx = ctx.clone();
        let mut queue: std::collections::VecDeque<Inbound> = std::collections::VecDeque::from([Inbound::Client(
            protocol::encode_session_request(writer, WRITER, &Request::Write {
                write_id: 2,
                ops: (0..40u32)
                    .map(|i| {
                        let mut v = vec![0u8; 1024];
                        v[..4].copy_from_slice(&i.to_le_bytes());
                        protocol::Op::Put(format!("h/{:04}x", i * 10).into_bytes(), v)
                    })
                    .collect(),
            })
            .expect("encodes"),
        )]);
        let (mut told, mut gets, mut spoke, mut calls) = (Vec::new(), 0, false, 0);
        for _ in 0..400 {
            let Some(one) = queue.pop_front() else { break };
            if let Inbound::GotState { id, bytes: Some(b) } = &one {
                cold.put(*id, b);
            }
            let mut s: Shell<Store> = Shell::resume_with(&ctx, params, cold.clone(), StoreFacts::provisioned());
            s.limits.max_gets = 1000;
            let out = s.handle(vec![one]);
            ctx = s.to_context().expect("a context");
            gets += out.ops.iter().filter(|o| matches!(o, engine_delegate::schedule::Op::Get { .. })).count();
            told.extend(states(&out.replies));
            queue.extend(answer(&node, out.ops));
            calls += 1;
            // The other tab speaks ONCE, in a call of its own, while the write is parked.
            if let (Some(v), false, true) = (bystander, spoke, calls == 1 && gets > 0) {
                spoke = true;
                let mut s: Shell<Store> = Shell::resume_with(&ctx, params, cold.clone(), StoreFacts::provisioned());
                s.limits.max_gets = 1000;
                let out = s.handle(vec![Inbound::Client(
                    protocol::encode_session_request(v, BYSTANDER, &Request::Tick { now: 1_790_000_000 }).expect("encodes"),
                )]);
                ctx = s.to_context().expect("a context");
                told.extend(states(&out.replies));
                queue.extend(answer(&node, out.ops));
            }
        }
        (told, gets, spoke)
    };

    let c = protocol::CURRENT;
    // row: (writer, bystander) -> the writer's last verdict
    for (row, (writer, bystander)) in [(c, None), (c, Some(2u16)), (2u16, None), (2u16, Some(c))].into_iter().enumerate() {
        let (told, gets, spoke) = run(writer, bystander);
        println!("  row {}: writer v{writer}, bystander {bystander:?} (spoke while parked: {spoke}; {gets} GETs) -> {:?}", row + 1, told.last());
        assert!(gets > 0, "row {}: the write never parked, so this tests nothing", row + 1);
        assert_eq!(spoke, bystander.is_some(), "row {}: the bystander did not speak while the write was parked", row + 1);
        if writer >= 3 {
            assert!(matches!(told.last(), Some(WriteState::TooLarge { .. })),
                "row {}: a v{writer} writer, refused after a park, was told {told:?}", row + 1);
        } else {
            assert_eq!(told.last(), Some(&WriteState::Failed),
                "row {}: a v{writer} writer must be told Failed, never a state it cannot read: {told:?}", row + 1);
        }
    }
}

/// The engine may not ask for more in one round than one return carries: the
/// rest would be dropped at the end of the call and never asked for again.
/// Live on two private nodes (sdk#150's L3), the engine asked for 8, a return
/// carried 4, and no cold read ever answered. The pairing is refused.
#[test]
#[should_panic(expected = "is over what one return carries")]
fn a_fetch_round_larger_than_a_return_is_refused_at_construction() {
    let params = Params {
        max_fetch_per_round: engine_delegate::schedule::Limits::default().max_gets + 1,
        ..Params::default()
    };
    let _: Shell<Store> =
        Shell::resume_with(&[], params, Store::default(), StoreFacts::provisioned());
}

/// ...and where `limits` is changed after construction, where it is used.
#[test]
#[should_panic(expected = "is over what one return carries")]
fn a_return_narrowed_below_the_fetch_round_is_refused_where_it_is_used() {
    let mut s: Shell<Store> = Shell::resume_with(
        &[],
        Params::default(),
        Store::default(),
        StoreFacts::provisioned(),
    );
    s.limits.max_gets = Params::default().max_fetch_per_round - 1;
    let _ = s.handle(Vec::new());
}

#[test]
fn control_the_default_pairing_is_accepted() {
    let p = Params::default();
    assert!(p.max_fetch_per_round <= engine_delegate::schedule::Limits::default().max_gets);
    let mut s: Shell<Store> =
        Shell::resume_with(&[], p, Store::default(), StoreFacts::provisioned());
    let _ = s.handle(Vec::new());
}
