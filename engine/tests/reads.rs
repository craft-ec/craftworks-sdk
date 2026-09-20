//! Acceptance for the read path (craftworks-sdk#27).
//!
//! The gate is the differential: whatever order blocks arrive in, whatever
//! strangers and duplicates and misses arrive with them, a read answers what
//! a direct lookup in a fully-held tree answers. Everything else here is a
//! property that differential cannot see.

use engine::read::{ReadResult, ReqId, Via};
use engine::{ClientId, Effect, Engine, Event, Params};
use freenet_prolly::range::Range;
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::collections::BTreeMap;
use std::ops::Bound;

mod common;
use common::{tree, Store};

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// A reader that has published the same root but holds no blocks.
fn reader(root: Cid, params: Params) -> (Engine<Store>, Store) {
    // A store of its OWN. Sharing one between a test's cases means warming
    // for the first leaves the second unable to be cold — which is how
    // "leaf cold" stopped being cold and three tests started passing for the
    // wrong reason.
    let store = Store::fresh();
    let mut e = Engine::new(params, store.clone());
    e.adopt_root_for_test(root);
    (e, store)
}

fn replies(effects: &[Effect]) -> Vec<(ReqId, ReadResult)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Reply { req_id, result, .. } => Some((*req_id, result.clone())),
            _ => None,
        })
        .collect()
}

fn fetch_ids(effects: &[Effect]) -> Vec<(Cid, Via, u32)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::FetchBlock { id, via, attempt } => Some((*id, *via, *attempt)),
            _ => None,
        })
        .collect()
}

fn get(client: u64, req: u64, key: &[u8]) -> Event {
    Event::Get {
        client: ClientId(client),
        req_id: ReqId(req),
        key: key.to_vec(),
    }
}

/// The oracle: what the tree actually holds, read with everything present.
fn direct(all: &MemBlocks, root: &Cid, key: &[u8]) -> Option<Vec<u8>> {
    match freenet_prolly::read::get(all, root, key) {
        Ok(Some(freenet_prolly::node::Value::Inline(b))) => Some(b.to_vec()),
        Ok(Some(freenet_prolly::node::Value::Ref { cid, .. })) => all.get(&cid).map(<[u8]>::to_vec),
        Ok(None) => None,
        Err(e) => panic!("the oracle could not read its own tree: {e:?}"),
    }
}

/// THE GATE. Cold reads answer what a fully-held tree answers, through
/// arbitrary arrival orders, duplicates, strangers and misses.
///
/// A read path has many chances to answer confidently and wrongly: resume at
/// the wrong level, cache a block under the wrong id, treat a miss as an
/// absence. All of them show up here as an answer that differs from the
/// oracle — including "absent", which is an answer and not a non-answer.
#[test]
fn cold_reads_answer_what_a_fully_held_tree_answers() {
    let mut cold_lookups = 0usize;
    for seed in 1..=16u64 {
        let mut r = rng(seed);
        let mut records: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for i in 0..600u32 {
            let v = if r().is_multiple_of(4) {
                // Over MAX_INLINE, so the value is a block of its own and the
                // read has to fetch it separately from the leaf.
                vec![(i % 251) as u8; 1400]
            } else {
                vec![(i % 251) as u8; 20]
            };
            records.insert(format!("k/{i:05}").into_bytes(), v);
        }
        let (root, all) = tree(&records);
        let (mut e, store) = reader(root, Params::default());

        // A mix of keys that exist and keys that do not.
        let keys: Vec<Vec<u8>> = records.keys().cloned().collect();
        let mut asked: Vec<(ReqId, Vec<u8>)> = Vec::new();
        let mut queue: Vec<Effect> = Vec::new();
        for n in 0..12u64 {
            let key = if r().is_multiple_of(5) {
                format!("absent/{}", r() % 1000).into_bytes()
            } else {
                keys[(r() % keys.len() as u64) as usize].clone()
            };
            let req = ReqId(n);
            asked.push((req, key.clone()));
            queue.extend(stepped!(e, get(1, n, &key)));
        }

        // Answer fetches in a shuffled order, with duplicates, strangers and
        // the occasional miss.
        let mut answered: BTreeMap<ReqId, ReadResult> = BTreeMap::new();
        let mut guard = 0;
        while !queue.is_empty() {
            guard += 1;
            assert!(guard < 200_000, "seed {seed}: the read did not settle");
            let i = (r() % queue.len() as u64) as usize;
            let f = queue.remove(i);
            match f {
                Effect::FetchBlock { id, .. } => {
                    let out = if r().is_multiple_of(7) {
                        // A bounded attempt that did not answer. The core must
                        // re-issue, not give up and not hang.
                        stepped!(e, Event::BlockMissed(id))
                    } else if let Some(bytes) = all.get(&id) {
                        let bytes = bytes.to_vec();
                        // Put, THEN tell: the engine keeps no block bytes of
                        // its own, so an arrival it cannot read is not one.
                        store.put(id, &bytes);
                        let mut out = stepped!(
                            e,
                            Event::BlockArrived {
                                id,
                                bytes: bytes.clone(),
                            }
                        );
                        if r().is_multiple_of(6) {
                            // The same block again: normal on a network.
                            out.extend(stepped!(e, Event::BlockArrived { id, bytes }));
                        }
                        out
                    } else {
                        stepped!(e, Event::BlockMissed(id))
                    };
                    queue.extend(out);
                }
                Effect::Reply { req_id, result, .. } => {
                    answered.insert(req_id, result);
                }
                _ => {}
            }
            if r().is_multiple_of(9) {
                // A block nobody asked for.
                let junk = vec![(r() % 251) as u8; 40];
                queue.extend(stepped!(
                    e,
                    Event::BlockArrived {
                        id: [(r() % 251) as u8; 32],
                        bytes: junk,
                    }
                ));
            }
        }

        for (req, key) in &asked {
            let got = answered
                .get(req)
                .unwrap_or_else(|| panic!("seed {seed}: {req:?} was never answered"));
            match got {
                ReadResult::Value(v) => {
                    assert_eq!(
                        v,
                        &direct(&all, &root, key),
                        "seed {seed}: a cold read answered differently from a \
                         fully-held tree"
                    );
                    cold_lookups += 1;
                }
                // Allowed: the harness injects misses, and exhausting the
                // attempt budget is a REPLY. What is not allowed is silence,
                // and that is asserted above.
                ReadResult::Unavailable(_) => {}
                other => panic!("seed {seed}: a Get was answered with {other:?}"),
            }
        }
    }
    // A floor, so a run where every read happened to be answered Unavailable
    // cannot read as a pass.
    assert!(
        cold_lookups >= 100,
        "only {cold_lookups} cold lookups actually produced a value; the \
         differential compared almost nothing"
    );
    println!("  {cold_lookups} cold lookups matched a fully-held tree");
}

/// A fixture with a named key and its tree.
fn fixture(n: u32) -> (BTreeMap<Vec<u8>, Vec<u8>>, Cid, MemBlocks) {
    let records: BTreeMap<Vec<u8>, Vec<u8>> = (0..n)
        .map(|i| {
            (
                format!("k/{i:05}").into_bytes(),
                vec![(i % 251) as u8; if i.is_multiple_of(3) { 1400 } else { 24 }],
            )
        })
        .collect();
    let (root, all) = tree(&records);
    (records, root, all)
}

/// Feed a reader every block, so it is fully warm.
fn warm_all(store: &Store, all: &MemBlocks) {
    // Warming means the NODE holds the blocks. Telling the engine
    // `BlockArrived` does not: the engine stores nothing, so the only thing
    // that makes a block readable is putting it where the engine reads from.
    // Seven read tests failed on this — "a warm hit asked the network for
    // something" — because feeding the engine used to be the same as filling
    // its store, and is not any more.
    for (id, bytes) in all.0.iter() {
        store.put(*id, bytes);
    }
}

/// Drive a reader until it answers, serving from `all`.
fn settle(
    e: &mut Engine<Store>,
    store: &Store,
    all: &MemBlocks,
    first: Vec<Effect>,
) -> Vec<(ReqId, ReadResult)> {
    let mut queue = first;
    let mut out = Vec::new();
    let mut guard = 0;
    while let Some(f) = queue.pop() {
        guard += 1;
        assert!(guard < 100_000, "the read did not settle");
        match f {
            Effect::FetchBlock { id, .. } => {
                // What a node does: the GET populates its store, and only
                // then is the engine told. The engine keeps nothing itself.
                let ev = match all.get(&id) {
                    Some(b) => {
                        store.put(id, b);
                        Event::BlockArrived {
                            id,
                            bytes: b.to_vec(),
                        }
                    }
                    None => Event::BlockMissed(id),
                };
                queue.extend(stepped!(e, ev));
            }
            Effect::Reply { req_id, result, .. } => out.push((req_id, result)),
            _ => {}
        }
    }
    out
}

/// Each state a reader can be in is named and tested, not left to a sweep to
/// wander into.
///
/// A sweep reaches these eventually and says nothing about which it reached;
/// a named test says which one broke.
#[test]
fn every_warmth_state_answers_the_same() {
    let (records, root, all) = fixture(400);
    let key = b"k/00123".to_vec();
    let want = direct(&all, &root, &key);
    assert!(want.is_some(), "the fixture key must exist");

    // 1. Fully warm: rule 1 — the reply is in the same step, with no fetch.
    let (mut e, store) = reader(root, Params::default());
    warm_all(&store, &all);
    let out = stepped!(e, get(1, 1, &key));
    assert!(
        fetch_ids(&out).is_empty(),
        "a warm hit asked the network for something"
    );
    assert_eq!(
        replies(&out),
        vec![(ReqId(1), ReadResult::Value(want.clone()))]
    );

    // 2. Fully cold.
    let (mut e, store) = reader(root, Params::default());
    let first = stepped!(e, get(1, 2, &key));
    let got = settle(&mut e, &store, &all, first);
    assert_eq!(got, vec![(ReqId(2), ReadResult::Value(want.clone()))]);

    // 3. Path warm, leaf cold: everything but the leaf holding the key.
    let (mut e, store) = reader(root, Params::default());
    let leaf = leaf_for(&all, &root, &key);
    // Withholding the leaf is not enough: a PACK containing it warms it just
    // as well, which is the whole point of packs. A fixture that fed the pack
    // would call this state "leaf cold" while the leaf was warm, and the
    // "exactly one fetch" assertion below is what caught that.
    for (id, bytes) in all.0.iter() {
        let carries_leaf =
            *id == leaf || engine::pack::members(bytes).iter().any(|(m, _)| *m == leaf);
        if !carries_leaf {
            // Into the STORE. Telling the engine `BlockArrived` no longer
            // makes a block readable — it keeps none — so warming means
            // putting blocks where it reads from.
            store.put(*id, bytes);
        }
    }
    let first = stepped!(e, get(1, 3, &key));
    assert_eq!(
        fetch_ids(&first).len(),
        1,
        "with only the leaf missing, exactly one block should be wanted"
    );
    assert_eq!(fetch_ids(&first)[0].0, leaf);
    let got = settle(&mut e, &store, &all, first);
    assert_eq!(got, vec![(ReqId(3), ReadResult::Value(want.clone()))]);

    // 4. Leaf warm via a PACK, branch cold. The pack carries the leaf, so one
    //    fetch of it answers what several reads were parked on.
    let (mut e, store) = reader(root, Params::default());
    let pack = all
        .0
        .iter()
        .find(|(id, bytes)| {
            freenet_prolly::block_id(engine::pack::PACK_KIND, bytes) == **id
                && engine::pack::members(bytes).iter().any(|(m, _)| *m == leaf)
        })
        .map(|(id, bytes)| (*id, bytes.clone()));
    if let Some((pid, pbytes)) = pack {
        // The node unpacks what it receives; the store does the same.
        for (mid, mbytes) in engine::pack::members(&pbytes) {
            store.put(mid, &mbytes);
        }
        store.put(pid, &pbytes);
        let first = stepped!(e, get(1, 4, &key));
        let got = settle(&mut e, &store, &all, first);
        assert_eq!(got, vec![(ReqId(4), ReadResult::Value(want.clone()))]);
    }
    let _ = records;
    println!("  warm / cold / leaf-cold / pack-warm all answer alike");
}

/// The leaf that holds `key`, found with everything present.
fn leaf_for(all: &MemBlocks, root: &Cid, key: &[u8]) -> Cid {
    let mut cur = *root;
    loop {
        let bytes = all.get(&cur).expect("held");
        let node = freenet_prolly::node::Node::parse(bytes).expect("a node");
        if node.is_leaf() {
            return cur;
        }
        let i = match node.search(key) {
            Ok(i) => i,
            Err(0) => return cur,
            Err(i) => i - 1,
        };
        cur = node.child(i).0;
    }
}

/// A block whose content is not what was asked for is refused, not cached.
///
/// This is the one that decides whether a content-addressed store is
/// content-addressed. If a wrong block is kept under the id that was wanted,
/// every later read of that key is answered wrongly, for as long as it stays
/// warm — and it was a stranger on the network who decided so.
#[test]
fn a_block_that_is_not_what_was_asked_for_is_refused() {
    let (_, root, all) = fixture(400);
    let key = b"k/00123".to_vec();
    let want = direct(&all, &root, &key);

    // Right id, wrong bytes; right bytes under a different id; and a leaf
    // where the descent wants a branch.
    let leaf = leaf_for(&all, &root, &key);
    let leaf_bytes = all.get(&leaf).expect("held").to_vec();

    for (name, make) in [
        ("wrong bytes under the right id", 0u8),
        ("right bytes under a different id", 1),
        ("a leaf where a branch is expected", 2),
    ] {
        let (mut e, store) = reader(root, Params::default());
        let first = stepped!(e, get(1, 7, &key));
        let asked = fetch_ids(&first);
        assert_eq!(asked.len(), 1, "{name}: expected one fetch to poison");
        let id = asked[0].0;

        let bytes = match make {
            0 => vec![0xEEu8; 64],
            1 => all
                .0
                .values()
                .find(|b| **b != leaf_bytes)
                .cloned()
                .expect("another block"),
            _ => leaf_bytes.clone(),
        };
        let out = stepped!(e, Event::BlockArrived { id, bytes });

        // The read must still SUCCEED once the network serves the truth.
        //
        // Answering `Unavailable` here would be a poisoned block costing a
        // reader its answer: one node sending rubbish is not a reason to stop
        // asking. And it is what separates the engine's check from the
        // library's — freenet-prolly refuses a mismatched block when it
        // PARSES one, which ends the read; the engine refuses it on ARRIVAL,
        // which turns it into another attempt. Allowing `Unavailable` here
        // let a mutant that skips the check survive this test.
        let got = settle(&mut e, &store, &all, out);
        let answer = got
            .iter()
            .find(|(r, _)| *r == ReqId(7))
            .map(|(_, res)| res.clone())
            .unwrap_or_else(|| panic!("{name}: the read was never answered"));
        assert_eq!(
            answer,
            ReadResult::Value(want.clone()),
            "{name}: a poisoned block cost the reader its answer"
        );
    }
    println!("  three hostile blocks refused, and none of them stuck");
}

/// A point lookup on a cold tree costs one block per level, not a scan.
#[test]
fn a_cold_point_lookup_costs_one_block_per_level() {
    let (_, root, all) = fixture(10_000);
    let key = b"k/05000".to_vec();

    let depth = {
        let mut d = 0;
        let mut cur = root;
        loop {
            let bytes = all.get(&cur).expect("held");
            let n = freenet_prolly::node::Node::parse(bytes).expect("node");
            d += 1;
            if n.is_leaf() {
                break d;
            }
            let i = match n.search(&key) {
                Ok(i) => i,
                Err(0) => break d,
                Err(i) => i - 1,
            };
            cur = n.child(i).0;
        }
    };

    let (mut e, store) = reader(
        root,
        Params {
            count_descent: true,
            ..Params::default()
        },
    );
    e.reset_cost();
    let first = stepped!(e, get(1, 9, &key));
    let got = settle(&mut e, &store, &all, first);
    assert!(
        matches!(got.first(), Some((_, ReadResult::Value(Some(_))))),
        "the lookup did not find the key: {got:?}"
    );
    // One fetch per level, plus at most one for a value stored by reference.
    assert!(
        e.fetches() <= depth + 1,
        "a cold point lookup on a 10,000-key tree fetched {} blocks for a \
         tree {depth} levels deep",
        e.fetches()
    );
    // Parses are per attempt, and there is one attempt per level, so the
    // bound is quadratic in depth at worst — still nothing like a scan.
    assert!(
        e.nodes_parsed() <= depth * (depth + 2),
        "{} nodes parsed for a {depth}-level lookup",
        e.nodes_parsed()
    );
    // CONTROL 1: a reader that fetches ahead. It asks for a whole level
    // where the descent needs one block, which is what keeps "≤ depth" from
    // being true of an implementation that fetched nothing useful.
    let (mut greedy, gstore) = reader(
        root,
        Params {
            fetch_ahead: true,
            ..Params::default()
        },
    );

    let first = stepped!(greedy, get(1, 10, &key));
    let _ = settle(&mut greedy, &gstore, &all, first);
    assert!(
        greedy.fetches() > depth + 1,
        "the greedy control fetched only {} blocks, so it does not blow the \
         bound and proves nothing about it",
        greedy.fetches()
    );

    println!(
        "  cold lookup: {} fetches, {} node parses, tree {depth} levels deep \
         (greedy control: {} fetches)",
        e.fetches(),
        e.nodes_parsed(),
        greedy.fetches()
    );
}

/// Two reads waiting on one block pay for one fetch.
#[test]
fn requests_waiting_on_one_block_share_its_fetch() {
    let (_, root, _all) = fixture(400);
    let key = b"k/00123".to_vec();

    // No block is ever delivered here: the count is of the fetches the first
    // step asks for, so the store stays empty on purpose.
    let count = |share: bool| -> usize {
        let (mut e, _store) = reader(
            root,
            Params {
                share_fetches: share,
                ..Params::default()
            },
        );
        let mut n = 0;
        for req in 0..5u64 {
            n += fetch_ids(&stepped!(e, get(1, req, &key))).len();
        }
        n
    };

    let shared = count(true);
    let each = count(false);
    assert_eq!(
        shared, 1,
        "five reads of one cold key emitted {shared} fetches"
    );
    assert_eq!(
        each, 5,
        "with sharing off the control emitted {each} fetches, so it is not a \
         control for sharing"
    );
    println!("  five readers, one fetch (control: {each})");
}

/// A preload manifest naming a million roots costs the budget, not the
/// manifest.
#[test]
fn a_hostile_preload_costs_the_budget() {
    let (_, root, all) = fixture(2_000);
    let (mut e, store) = reader(root, Params::default());
    // The session holds this root and nothing else -- in the STORE, which is
    // where the engine reads. Telling it about a block it cannot then read
    // makes the preload start by fetching the root, and the budget assertion
    // below would pass over a preload that never descended at all.
    let root_bytes = all.get(&root).expect("held").to_vec();
    store.put(root, &root_bytes);
    stepped!(
        e,
        Event::BlockArrived {
            id: root,
            bytes: root_bytes,
        }
    );
    e.reset_cost();

    let mut roots: Vec<Cid> = (0..1_000_000u32).map(|i| [(i % 251) as u8; 32]).collect();
    roots.insert(0, root);
    let out = stepped!(
        e,
        Event::Preload {
            client: ClientId(1),
            roots,
        }
    );
    let p = Params::default();
    assert!(
        fetch_ids(&out).len() <= p.preload_blocks,
        "a preload of a million roots asked for {} blocks",
        fetch_ids(&out).len()
    );
    assert!(
        e.nodes_parsed() <= p.preload_blocks * 2,
        "a preload of a million roots parsed {} nodes",
        e.nodes_parsed()
    );
    println!(
        "  preload of 10^6 roots: {} fetches, {} parses",
        fetch_ids(&out).len(),
        e.nodes_parsed()
    );
}

/// A range pages in both directions, and every feed pages newest-first.
#[test]
fn a_range_pages_in_both_directions() {
    let (records, root, all) = fixture(300);
    let keys: Vec<Vec<u8>> = records.keys().cloned().collect();

    for reverse in [false, true] {
        let (mut e, store) = reader(root, Params::default());
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        for page in 0..20 {
            let r = Range {
                lo: Bound::Unbounded,
                hi: Bound::Unbounded,
                reverse,
                after: after.clone(),
                max_entries: 25,
                max_bytes: 1 << 20,
            };
            let first = stepped!(
                e,
                Event::Scan {
                    client: ClientId(1),
                    req_id: ReqId(page),
                    range: Box::new(r),
                }
            );
            let got = settle(&mut e, &store, &all, first);
            let Some((
                _,
                ReadResult::Page {
                    entries,
                    cursor,
                    complete,
                },
            )) = got.first()
            else {
                panic!("reverse={reverse}: page {page} was answered {got:?}");
            };
            seen.extend(entries.iter().map(|(k, _)| k.clone()));
            after = cursor.clone();
            if *complete || after.is_none() {
                break;
            }
        }
        let mut want = keys.clone();
        if reverse {
            want.reverse();
        }
        assert_eq!(
            seen, want,
            "reverse={reverse}: paging did not visit every key in scan order"
        );
    }
    println!("  both directions page every key in order");
}

/// No sequence of read events makes the core panic — AND every read that was
/// issued is answered.
///
/// The second half is not decoration. A read that never answers does not
/// panic, so a sweep asking only "did it crash" is green while the delegate
/// spends its whole 5 s slice on one key and reports nothing. That is exactly
/// how the warm-set livelock got past this sweep.
#[test]
fn no_read_sequence_panics_and_every_read_answers() {
    let (_, root, all) = fixture(500);
    let ids: Vec<Cid> = all.0.keys().copied().collect();
    let mut cases = 0;
    let mut answered_seeds = 0;
    for seed in 1..=20u64 {
        let mut outstanding: std::collections::BTreeSet<ReqId> = Default::default();
        let mut pending: Vec<Effect> = Vec::new();
        let mut r = rng(seed);
        let (mut e, store) = reader(
            root,
            Params {
                max_attempts: 2,
                max_context_bytes: 32 * 1024,
                ..Params::default()
            },
        );
        for _ in 0..80 {
            let ev = match r() % 6 {
                0 => get(
                    1 + r() % 3,
                    r() % 10,
                    format!("k/{:05}", r() % 600).as_bytes(),
                ),
                1 => Event::Scan {
                    client: ClientId(1),
                    req_id: ReqId(r() % 10),
                    range: Box::new(Range {
                        lo: Bound::Unbounded,
                        hi: Bound::Unbounded,
                        reverse: r().is_multiple_of(2),
                        after: None,
                        max_entries: (r() % 40) as usize,
                        max_bytes: 1 << 16,
                    }),
                },
                2 => {
                    let id = ids[(r() % ids.len() as u64) as usize];
                    Event::BlockArrived {
                        id,
                        bytes: all.get(&id).expect("held").to_vec(),
                    }
                }
                3 => Event::BlockArrived {
                    id: [(r() % 251) as u8; 32],
                    bytes: vec![(r() % 251) as u8; (r() % 200) as usize],
                },
                4 => Event::BlockMissed(ids[(r() % ids.len() as u64) as usize]),
                _ => Event::Preload {
                    client: ClientId(1),
                    roots: vec![root, [(r() % 251) as u8; 32]],
                },
            };
            let issued = matches!(ev, Event::Get { .. } | Event::Scan { .. });
            let req = match &ev {
                Event::Get { req_id, .. } | Event::Scan { req_id, .. } => Some(*req_id),
                _ => None,
            };
            let out = stepped!(e, ev);
            for (r, _) in replies(&out) {
                outstanding.remove(&r);
            }
            if issued {
                if let Some(r) = req {
                    outstanding.insert(r);
                }
            }
            // The fetches a read asked for are kept, and served in the drain.
            // Dropping them would leave every parked read waiting on an answer
            // the harness never gave — which is the harness failing to answer,
            // not the engine failing to.
            pending.extend(out);
            cases += 1;
        }

        // Drain: serve what is still wanted, within a step budget. What is
        // left outstanding after that is a read that never answers.
        let mut queue: Vec<Effect> = std::mem::take(&mut pending);
        let mut steps = 0;
        while let Some(f) = queue.pop() {
            steps += 1;
            assert!(steps < 50_000, "seed {seed}: the drain did not finish");
            match f {
                Effect::FetchBlock { id, .. } => {
                    // The node puts it, THEN tells the engine. Telling alone
                    // makes no block readable — the engine keeps none — so a
                    // drain that only tells re-asks for ever.
                    let ev = match all.get(&id) {
                        Some(b) => {
                            store.put(id, b);
                            Event::BlockArrived {
                                id,
                                bytes: b.to_vec(),
                            }
                        }
                        None => Event::BlockMissed(id),
                    };
                    queue.extend(stepped!(e, ev));
                }
                Effect::Reply { req_id, .. } => {
                    outstanding.remove(&req_id);
                }
                _ => {}
            }
        }
        assert!(
            outstanding.is_empty(),
            "seed {seed}: {} read(s) were never answered: {outstanding:?}",
            outstanding.len()
        );
        answered_seeds += 1;
    }
    assert!(cases >= 1500, "only {cases} events were driven");
    assert_eq!(
        answered_seeds, 20,
        "not every seed reached the answered check"
    );
    println!("  {cases} read events across 20 seeds, no panic and no read left unanswered");
}

/// A value over `MAX_INLINE` lives in its own block, so reading it needs the
/// LEAF (to learn the reference) and the VALUE BLOCK — at the same moment.
///
/// The fixture stores 1400 bytes on every third key, so these are the sampled
/// keys whose value is a reference.
fn value_is_a_reference(key: &[u8]) -> bool {
    let n: u32 = std::str::from_utf8(&key[2..])
        .expect("k/NNNNN")
        .parse()
        .expect("a number");
    n.is_multiple_of(3)
}

#[test]
fn a_forgetful_store_still_answers_every_read() {
    forgetful(2_000, false);
}

/// The same, with the ROOT evicted too.
///
/// The case above pre-seeds the root and never takes it away, so a re-descent
/// costs almost nothing: the root is always there, and only the level that
/// just arrived is needed. That is not "a node that keeps NOTHING" — it is a
/// node that keeps the root. This one keeps nothing at all, which is what
/// makes the difference between starting over and continuing observable.
#[test]
fn a_store_that_keeps_nothing_at_all_still_answers() {
    forgetful(2_000, true);
}

fn forgetful(records_in_tree: u32, evict_root: bool) {
    use common::{Harness, Mode};

    let (records, root, all) = fixture(records_in_tree);
    let keys: Vec<Vec<u8>> = records.keys().cloned().collect();
    const BUDGET: usize = 20_000;

    for forget_every in [0usize, 1, 3] {
        let store = Store::fresh();
        let mut h = Harness::new(Mode::Rehydrate, Params::default(), store.clone());
        store.put(root, all.get(&root).expect("the root"));
        let _ = h.step(Event::Start {
            key: engine::KeySource::SecretStore,
            epochs: vec![engine::Epoch(1)],
        });
        let _ = h.step(Event::HeadRead {
            epoch: engine::Epoch(1),
            seq: 1,
            root,
        });

        if evict_root {
            // A node that retains nothing does not retain the root either.
            store.forget(root);
        }

        let (mut answered, mut unavailable) = (0, 0);
        let mut unanswered_keys: Vec<Vec<u8>> = Vec::new();
        for (n, key) in keys.iter().step_by(400).enumerate() {
            let mut queue = h.step(Event::Get {
                client: ClientId(1),
                req_id: ReqId(n as u64),
                key: key.clone(),
            });
            let mut steps = 0;
            let mut served = 0usize;
            while let Some(f) = queue.pop() {
                steps += 1;
                assert!(
                    steps < BUDGET,
                    "forget_every={forget_every}: a read took {steps} steps and \
                     had not answered"
                );
                match f {
                    Effect::FetchBlock { id, .. } => {
                        let ev = match all.get(&id) {
                            Some(b) => {
                                store.put(id, b);
                                served += 1;
                                // The node evicts what it just served, as it
                                // may: using a block through the sync read
                                // does not refresh its hosting.
                                if forget_every > 0 && served.is_multiple_of(forget_every) {
                                    store.forget(id);
                                }
                                Event::BlockArrived {
                                    id,
                                    bytes: b.to_vec(),
                                }
                            }
                            None => Event::BlockMissed(id),
                        };
                        queue.extend(h.step(ev));
                    }
                    Effect::Reply { result, .. } => match result {
                        ReadResult::Value(_) => answered += 1,
                        ReadResult::Unavailable(_) | ReadResult::OutOfWarmSpace => {
                            unavailable += 1;
                            unanswered_keys.push(key.clone());
                        }
                        other => panic!("a Get was answered with {other:?}"),
                    },
                    _ => {}
                }
            }
        }
        assert_eq!(
            answered + unavailable,
            keys.iter().step_by(400).count(),
            "forget_every={forget_every}: not every read was answered"
        );
        println!(
            "  {records_in_tree} records, forget every {forget_every}: \
             {answered} answered, {unavailable} unavailable"
        );

        // THE ACCEPTANCE ROW (sdk#34). Printed since #32; asserted now.
        //
        // `forget every 1` is a node that drops a block the instant after
        // serving it, which it may: a sync read does not refresh hosting
        // (F33), and there has been no TTL floor since 2026-07-08. Every read
        // used to be Unavailable, because a read that re-descends from the
        // root needs the whole path again and the path is gone. Continuing
        // from the block that just arrived uses each block at the moment it
        // arrives, so the same node answers.
        //
        // EXCEPT for a value stored BY REFERENCE. That read needs the leaf and
        // the value block AT THE SAME MOMENT — the leaf to learn the
        // reference, the block to have the bytes — and a node that retains
        // nothing serves one of them per call while the engine holds no block
        // bytes across calls (F32). One cid of carried state cannot express
        // it, so it is NAMED here rather than quietly tolerated.
        for key in &unanswered_keys {
            assert!(
                value_is_a_reference(key),
                "forget_every={forget_every}: {} went unanswered, and its value \
                 is INLINE — so this is not the by-reference limit, it is a \
                 regression in the frontier",
                String::from_utf8_lossy(key)
            );
        }
        if forget_every != 1 {
            assert_eq!(
                unavailable, 0,
                "forget_every={forget_every}: a node that retains anything at \
                 all must answer every read"
            );
        } else {
            let inline = keys
                .iter()
                .step_by(400)
                .filter(|k| !value_is_a_reference(k))
                .count();
            assert_eq!(
                answered, inline,
                "the acceptance row: every INLINE read must be answered by a \
                 node that keeps nothing"
            );
            assert!(
                answered > 0,
                "a row where nothing is answered passes the check above \
                 vacuously if `inline` is ever zero"
            );
        }
    }
}

#[test]
fn a_waiting_read_does_not_re_ask_however_often_the_engine_is_entered() {
    use common::{Harness, Mode};

    let (_, root, all) = fixture(2_000);
    let key = b"k/01000".to_vec();

    // TWO mechanisms protect this bound, and each MASKS the other when
    // tested alone: re-descending on every entry emits nothing because the
    // in-flight set suppresses it, and dropping the in-flight set costs
    // nothing because nothing re-descends. Measured separately each looked
    // inert — the same shape as a new guard hiding the guards behind it, seen
    // from the other side. So the control turns both off.
    let count = |mode: Mode, broken: bool, unrelated: usize| -> usize {
        let store = Store::fresh();
        let mut h = Harness::new(
            mode,
            Params {
                redescend_on_entry: broken,
                dedupe_in_flight: !broken,
                ..Params::default()
            },
            store.clone(),
        );
        // A recovered engine learns its root from its head, which is also the
        // only way to give the harness a root: it re-hydrates every step, so
        // nothing set on a value in memory would survive.
        store.put(root, all.get(&root).expect("the root"));
        let _ = h.step(Event::Start {
            key: engine::KeySource::SecretStore,
            epochs: vec![engine::Epoch(1)],
        });
        let _ = h.step(Event::HeadRead {
            epoch: engine::Epoch(1),
            seq: 1,
            root,
        });
        let mut e_root = h.step(Event::Get {
            client: ClientId(1),
            req_id: ReqId(1),
            key: key.clone(),
        });
        let mut fetches = fetch_ids(&e_root).len();
        // Now drive unrelated events while the fetch is outstanding. None of
        // them has anything to do with this read.
        for i in 0..unrelated {
            let out = h.step(Event::Tick(i as u64 + 1));
            fetches += fetch_ids(&out).len();
            let out = h.step(Event::BlockMissed([0xAAu8; 32]));
            fetches += fetch_ids(&out).len();
        }
        e_root.clear();
        fetches
    };

    // BOTH MODES, and they must agree. The bookkeeping that makes this bound
    // hold — which reads are parked and which blocks are already asked for —
    // has to come out of the CONTEXT. A version living in memory would pass
    // in `Live` and fail only here, which is the one place this design is
    // weak, so the numbers are taken in both and compared.
    let quiet = count(Mode::Rehydrate, false, 0);
    let busy = count(Mode::Rehydrate, false, 20);
    let quiet_live = count(Mode::Live, false, 0);
    let busy_live = count(Mode::Live, false, 20);
    assert_eq!(
        (quiet, busy),
        (quiet_live, busy_live),
        "the two modes disagree: re-hydrating changed what a read costs, so \
         some of what makes this bound hold is living in memory rather than \
         in the context"
    );
    assert_eq!(
        quiet, busy,
        "a read cost {busy} fetches on a busy node and {quiet} on a quiet one: \
         the cost depends on how often the engine is entered, not on the tree"
    );

    // The control, and it RUNS: an engine that re-drives every parked read on
    // every entry pays for the node being busy.
    let control = count(Mode::Rehydrate, true, 20);
    let control_live = count(Mode::Live, true, 20);
    assert_eq!(
        control, control_live,
        "the control behaves differently in the two modes"
    );
    assert!(
        control > quiet,
        "the control still cost {control} fetches against {quiet}: it is not \
         re-asking, and this bound is not being tested against anything"
    );
    println!(
        "  {quiet} fetch(es) on a quiet node, {busy} on a busy one, \
         {control} with both off (identical in Live and Rehydrate)"
    );
}
