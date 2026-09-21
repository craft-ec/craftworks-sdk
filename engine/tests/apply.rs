//! A write onto a tree path the node does not hold (craftworks-sdk#32).
//!
//! A delegate reads the tree through a SYNC host call that returns what the
//! node happens to hold at that instant (F14), and that call registers no
//! demand (F33). So an apply can stop on a cold block with nothing wrong: the
//! engine must fetch it and run the apply again, not tell the client its write
//! failed.

use engine::read::{ReadResult, ReqId};
use engine::{ClientId, Effect, Event, Op, Params, State, WriteId};
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::collections::BTreeMap;

mod common;
use common::{rebuild, tree, Harness, Mode, Store};

/// A tree of `n` keys, and every block in it.
fn fixture(n: u32) -> (BTreeMap<Vec<u8>, Vec<u8>>, Cid, MemBlocks) {
    let records: BTreeMap<Vec<u8>, Vec<u8>> = (0..n)
        .map(|i| (format!("k/{i:05}").into_bytes(), vec![(i % 251) as u8; 40]))
        .collect();
    let (root, all) = tree(&records);
    (records, root, all)
}

fn started(mode: Mode, params: Params, store: Store, root: Cid) -> Harness {
    let mut h = Harness::new(mode, params, store);
    let _ = h.step(Event::Start {
        key: engine::KeySource::SecretStore,
        epochs: vec![engine::Epoch(1)],
    });
    let _ = h.step(Event::HeadRead {
        epoch: engine::Epoch(1),
        seq: 1,
        root,
    });
    h
}

fn fetches(fx: &[Effect]) -> Vec<Cid> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::FetchBlock { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn states(fx: &[Effect]) -> Vec<State> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notify { state, .. } => Some(*state),
            _ => None,
        })
        .collect()
}

fn write_one(key: &str, val: &[u8]) -> Event {
    Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops: vec![(key.as_bytes().to_vec(), Op::Put(val.to_vec()))],
    }
}

/// A write onto a cold path is parked, fetched for, and applies.
///
/// The regression: this reported `Failed`, which says *this edit is not in
/// the tree and never will be*. It was a transient miss, and a client acting
/// on that word gives up on a write one fetch away from landing.
///
/// The control is the SAME write against a warm store: it must ask for
/// nothing and be accepted in one step. Without it, "the cold write was
/// parked" would read identically if every write were parked.
#[test]
fn a_write_onto_a_cold_path_is_parked_and_then_applies() {
    for mode in [Mode::Live, Mode::Rehydrate] {
        let (mut records, root, all) = fixture(400);

        // --- the control: a warm store ---
        let warm = Store::fresh();
        for (id, bytes) in all.0.iter() {
            warm.put(*id, bytes);
        }
        let mut hw = started(mode, Params::default(), warm, root);
        let out = hw.step(write_one("k/00100", b"warm"));
        assert!(
            fetches(&out).is_empty(),
            "{mode:?}: a write against a store holding the whole tree asked \
             for {} block(s), so parking is not what the cold case shows",
            fetches(&out).len()
        );
        assert_eq!(
            states(&out).first(),
            Some(&State::Accepted),
            "{mode:?}: a warm write was not accepted in one step"
        );

        // --- the case: only the root is held ---
        let cold = Store::fresh();
        cold.put(root, all.get(&root).expect("the root"));
        let mut h = started(mode, Params::default(), cold.clone(), root);

        let out = h.step(write_one("k/00100", b"cold"));
        assert!(
            !fetches(&out).is_empty(),
            "{mode:?}: a write onto a path the node does not hold asked for \
             nothing"
        );
        assert!(
            states(&out).is_empty(),
            "{mode:?}: a parked write was told {:?} before it had applied",
            states(&out)
        );

        // Feed it what it asks for, one block at a time, until it applies.
        let mut queue = fetches(&out);
        let mut rounds = 0;
        let mut accepted = false;
        let mut delivered = 0usize;
        while let Some(id) = queue.pop() {
            rounds += 1;
            assert!(rounds < 1000, "{mode:?}: the parked write never settled");
            let bytes = all.get(&id).expect("the fixture holds every block");
            cold.put(id, bytes);
            delivered += 1;
            let out = h.step(Event::BlockArrived {
                id,
                bytes: bytes.to_vec(),
            });
            queue.extend(fetches(&out));
            if states(&out).contains(&State::Accepted) {
                accepted = true;
                break;
            }
        }
        assert!(
            accepted,
            "{mode:?}: the parked write was fed every block it asked for and \
             was never accepted"
        );
        // It read a PATH, not the tree: 400 keys is several levels, and a
        // write that pulled the whole thing would be hundreds of blocks.
        assert!(
            delivered < 20,
            "{mode:?}: the parked write pulled {delivered} blocks to apply one \
             key, which is a whole-tree walk rather than a path"
        );

        records.insert(b"k/00100".to_vec(), b"cold".to_vec());
        assert_eq!(
            h.root(),
            rebuild(&records),
            "{mode:?}: the parked write applied to the wrong tree"
        );
        println!("  {mode:?}: cold write parked, {delivered} blocks, then applied");
    }
}

/// A write whose blocks never arrive is refused, not left waiting.
///
/// The bound is on ROUNDS, not on misses: this drives it with misses, and the
/// sibling assertion is that nothing was applied — `Failed` is only honest
/// when the edit really is not in the tree.
#[test]
fn a_write_whose_blocks_never_arrive_is_refused_and_applies_nothing() {
    // Two different bounds. One run stopping at its bound could be a
    // coincidence of the fixture's depth; two runs each stopping at their own
    // shows the bound is the thing doing it.
    for (mode, rounds_allowed) in [(Mode::Live, 8u32), (Mode::Rehydrate, 3)] {
        let params = Params {
            max_apply_rounds: rounds_allowed,
            ..Params::default()
        };
        let (_, root, all) = fixture(400);
        let cold = Store::fresh();
        cold.put(root, all.get(&root).expect("the root"));
        let mut h = started(mode, params, cold, root);
        let before = h.root();

        let mut queue = fetches(&h.step(write_one("k/00100", b"never")));
        assert!(!queue.is_empty(), "{mode:?}: the write asked for nothing");
        let mut asked = 1u32;
        let mut failed = 0usize;
        while let Some(id) = queue.pop() {
            let out = h.step(Event::BlockMissed(id));
            failed += states(&out).iter().filter(|s| **s == State::Failed).count();
            let next = fetches(&out);
            if next.is_empty() {
                break;
            }
            asked += 1;
            assert!(
                asked <= rounds_allowed,
                "{mode:?}: the write has been re-asked {asked} times against a \
                 bound of {rounds_allowed}"
            );
            queue = next;
        }
        assert_eq!(
            failed, 1,
            "{mode:?}: a write that could never apply was reported Failed \
             {failed} times; a client needs exactly one answer"
        );
        assert_eq!(
            h.root(),
            before,
            "{mode:?}: the write was reported Failed and changed the tree \
             anyway, so a client that re-submits applies it twice"
        );
        // The control for the bound: it stopped where the bound put it, not
        // on the first miss and not at some fixed depth of its own.
        assert_eq!(
            asked, rounds_allowed,
            "{mode:?}: the write was refused after {asked} round(s) against a \
             bound of {rounds_allowed}, so the bound is not what stops it"
        );
        println!("  {mode:?}: refused after {asked} rounds, tree unchanged");
    }
}

/// A second write while one is parked is refused, and leaves no trace.
#[test]
fn a_second_write_while_one_is_parked_is_refused() {
    let (records, root, all) = fixture(400);
    let cold = Store::fresh();
    cold.put(root, all.get(&root).expect("the root"));
    let mut h = started(Mode::Rehydrate, Params::default(), cold, root);

    let out = h.step(write_one("k/00100", b"first"));
    assert!(!fetches(&out).is_empty(), "the first write did not park");

    let out = h.step(Event::Write {
        client: ClientId(2),
        write_id: WriteId(2),
        ops: vec![(b"k/00200".to_vec(), Op::Put(b"second".to_vec()))],
    });
    assert_eq!(
        states(&out),
        vec![State::Busy],
        "a write arriving while one is parked was not refused"
    );
    assert_eq!(
        h.root(),
        rebuild(&records),
        "the refused write changed the tree, so it was applied AND refused"
    );
    println!("  a second write while one is parked: Busy, tree untouched");
}

/// A write too big to carry in the context is not parked -- but its blocks
/// ARE fetched, so the re-send finds a warm path and applies.
///
/// The parked write's ops ride in the context, which the platform caps at
/// 400 KiB, so one that does not fit is not parked. It used to be refused
/// with NO fetches: the one thing that would make it acceptable was the thing
/// the refusal skipped, and a cold write over the cap was `Busy` for ever
/// (craftworks-sdk#136, measured by the architect: 5 x 40 KiB, cold, four
/// re-sends, four `Busy`, zero fetches). Now `Busy` is TRUE: re-sent after the
/// fetched blocks arrive, the same write applies.
#[test]
fn a_write_too_big_to_park_fetches_its_path_and_applies_when_re_sent() {
    let (_, root, all) = fixture(400);
    let cap = 4096usize;
    let params = Params {
        max_parked_write_bytes: cap,
        ..Params::default()
    };
    // THE CONTROL: under the cap, the same cold path PARKS -- so the cap, not
    // the cold path, is what the larger write meets.
    {
        let cold = Store::fresh();
        cold.put(root, all.get(&root).expect("the root"));
        let mut h = started(Mode::Rehydrate, params, cold, root);
        let out = h.step(write_one("k/00100", &vec![7u8; cap / 2]));
        assert!(
            states(&out).is_empty() && !fetches(&out).is_empty(),
            "a write UNDER the cap onto the same cold path was not parked: {:?}",
            states(&out)
        );
    }
    let cold = Store::fresh();
    cold.put(root, all.get(&root).expect("the root"));
    let mut h = started(Mode::Rehydrate, params, cold.clone(), root);
    let big = vec![7u8; cap + 1];
    let mut rounds = 0;
    let accepted = loop {
        rounds += 1;
        assert!(
            rounds <= 8,
            "still not accepted after {rounds} re-sends: Busy for ever"
        );
        let out = h.step(write_one("k/00100", &big));
        let st = states(&out);
        if st.contains(&State::Accepted) {
            break true;
        }
        assert_eq!(
            st,
            vec![State::Busy],
            "round {rounds}: an over-cap write was not refused Busy"
        );
        let wanted = fetches(&out);
        assert!(
            !wanted.is_empty(),
            "round {rounds}: refused with NO fetches -- the path stays cold and the write is Busy for ever"
        );
        // The node delivers what was asked for.
        for id in wanted {
            let bytes = all.get(&id).expect("held");
            cold.put(id, bytes);
            let _ = h.step(Event::BlockArrived {
                id,
                bytes: bytes.to_vec(),
            });
        }
    };
    assert!(accepted);
    println!("  over the cap: Busy with fetches; accepted on re-send {rounds}");
}

/// A parked write and a parked read wait on the same block, and both are
/// answered by its one arrival.
#[test]
fn one_arrival_answers_both_a_parked_read_and_a_parked_write() {
    let (_, root, all) = fixture(400);
    let cold = Store::fresh();
    cold.put(root, all.get(&root).expect("the root"));
    let mut h = started(Mode::Rehydrate, Params::default(), cold.clone(), root);

    // A read of the key the write is about to touch: both descend the same
    // path, so they stop on the same block.
    let r = h.step(Event::Get {
        client: ClientId(1),
        req_id: ReqId(1),
        key: b"k/00100".to_vec(),
    });
    let w = h.step(write_one("k/00100", b"both"));
    let shared: Vec<Cid> = fetches(&r)
        .into_iter()
        .filter(|i| fetches(&w).contains(i))
        .collect();
    assert!(
        !shared.is_empty(),
        "the read and the write stopped on different blocks, so this test is \
         not about one arrival answering two waiters"
    );

    let mut queue = shared;
    let (mut answered, mut accepted) = (false, false);
    let mut guard = 0;
    while let Some(id) = queue.pop() {
        guard += 1;
        assert!(guard < 1000, "neither waiter settled");
        let bytes = all.get(&id).expect("held");
        cold.put(id, bytes);
        let out = h.step(Event::BlockArrived {
            id,
            bytes: bytes.to_vec(),
        });
        queue.extend(fetches(&out));
        if states(&out).contains(&State::Accepted) {
            accepted = true;
        }
        if out.iter().any(|f| {
            matches!(
                f,
                Effect::Reply {
                    result: ReadResult::Value(_),
                    ..
                }
            )
        }) {
            answered = true;
        }
        if answered && accepted {
            break;
        }
    }
    assert!(
        answered,
        "the read was never answered: the write's arrivals swallowed it"
    );
    assert!(
        accepted,
        "the write never applied: the read's arrivals swallowed it"
    );
    println!("  one block stream answered a parked read and a parked write");
}

/// A parked write is capped on what it COSTS the context, not on its payload.
///
/// The cap was on payload bytes — keys plus values — and a batch of many tiny
/// ops has a small payload and a large serialized cost. Under a 128 KiB
/// payload cap, 130,000 one-byte keys weigh 128 KiB of payload and megabytes
/// of context, which is exactly the state the cap exists to refuse.
#[test]
fn a_parked_write_of_many_tiny_ops_is_capped_on_what_it_costs() {
    let cap = 16 * 1024usize;
    let params = Params {
        max_parked_write_bytes: cap,
        ..Params::default()
    };
    let (_, root, all) = fixture(400);
    let cold = Store::fresh();
    cold.put(root, all.get(&root).expect("the root"));
    let mut h = started(Mode::Rehydrate, params, cold, root);

    // Payload well under the cap; serialized cost well over it. Each op is a
    // 6-byte key and a 1-byte value: 7 B of payload, ~25 B encoded.
    let n = cap / 8;
    let ops: Vec<(Vec<u8>, Op)> = (0..n)
        .map(|i| (format!("t{i:05}").into_bytes(), Op::Put(vec![1u8])))
        .collect();
    let payload: usize = ops
        .iter()
        .map(|(k, o)| k.len() + if let Op::Put(v) = o { v.len() } else { 0 })
        .sum();
    assert!(
        payload < cap,
        "the fixture's payload is {payload} B, over the {cap} B cap: it would \
         be refused by a payload cap too, and would not show the difference"
    );

    let out = h.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops,
    });
    assert_eq!(
        states(&out),
        vec![State::Busy],
        "a write whose payload is {payload} B (under the {cap} B cap) but \
         whose context cost is far over it was parked anyway"
    );
    // Declined, not parked -- and its path IS fetched, so the re-send finds it
    // warm (craftworks-sdk#136: a refusal with no fetches was `Busy` for ever).
    assert!(
        !fetches(&out).is_empty(),
        "a write declined on its cost fetched nothing -- it stays cold for ever"
    );
    println!(
        "  {n} tiny ops: {payload} B of payload, declined on context cost, {} fetch(es)",
        fetches(&out).len()
    );
}
