//! Acceptance for the write pipeline (craftworks-sdk#24).
//!
//! Everything here is deterministic: no node, no clock, no randomness that is
//! not seeded and printed. That is the whole point of a sans-IO core — an
//! interleaving a live network produces once a week is an ordinary test here.

use engine::{ClientId, Effect, Engine, Event, Op, Params, State, WriteId};
use freenet_prolly::build::TreeBuilder;
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

fn w(n: u64) -> WriteId {
    WriteId(n)
}
fn c(n: u64) -> ClientId {
    ClientId(n)
}

fn write(client: u64, id: u64, ops: Vec<(Vec<u8>, Op)>) -> Event {
    Event::Write {
        client: c(client),
        write_id: w(id),
        ops,
    }
}

fn put(k: &str, v: &[u8]) -> (Vec<u8>, Op) {
    (k.as_bytes().to_vec(), Op::Put(v.to_vec()))
}

/// The states reported for each write, in the order they were reported.
#[derive(Default)]
struct Seen(BTreeMap<(ClientId, WriteId), Vec<State>>);

impl Seen {
    fn absorb(&mut self, effects: &[Effect]) {
        for e in effects {
            if let Effect::Notify {
                client,
                write_id,
                state,
            } = e
            {
                self.0.entry((*client, *write_id)).or_default().push(*state);
            }
        }
    }
    fn of(&self, client: u64, id: u64) -> &[State] {
        self.0
            .get(&(c(client), w(id)))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn ids(effects: &[Effect]) -> Vec<Cid> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::PutPack { id, .. } | Effect::PutBlock { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn pack_ids(effects: &[Effect]) -> Vec<Cid> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::PutPack { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn head_of(effects: &[Effect]) -> Option<(u64, Cid, Vec<Cid>)> {
    effects.iter().find_map(|e| match e {
        Effect::UpdateHead { seq, root, after } => Some((*seq, *root, after.clone())),
        _ => None,
    })
}

/// The root a from-scratch build of the same records produces.
///
/// This is the oracle that matters: the pipeline may reorder, fold, retry and
/// coalesce as much as it likes, and the tree it ends up naming must be the
/// one the library would have built from the final records in one pass.
fn rebuild(records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Cid {
    let mut t = TreeBuilder::new(|_, _: &[u8]| {});
    for (k, v) in records {
        t.push_bytes(k, v).unwrap();
    }
    t.finish().unwrap()
}

/// Drive a commit from its puts to `published`, confirming in the given order.
fn settle(e: &mut Engine, seen: &mut Seen, effects: Vec<Effect>, order: &[usize]) -> Vec<Effect> {
    // The caller has already absorbed `effects`; absorbing them again would
    // record every notification in them twice and make "once per state" read
    // as a duplicate.
    let mut all = effects.clone();
    let pending = ids(&effects);
    for i in order {
        let out = e.step(Event::PutConfirmed(pending[*i]));
        seen.absorb(&out);
        all.extend(out.clone());
        if let Some((seq, _, _)) = head_of(&out) {
            let out = e.step(Event::HeadConfirmed(seq));
            seen.absorb(&out);
            all.extend(out);
        }
    }
    all
}

#[test]
fn one_write_reaches_published_and_the_head_waits_for_its_packs() {
    // Two values too large to ride in a pack, so the commit has SEVERAL
    // outstanding puts. With a single put the "all but the last" loop below
    // runs zero times and the test cannot tell a head that waits from one
    // that does not -- measured: a mutant emitting the head on the first
    // confirmation survived this test until the fixture was widened.
    let mut e = Engine::new(Params {
        max_packed_value: 1024,
        ..Params::default()
    });
    let mut seen = Seen::default();

    let out = e.step(write(
        1,
        1,
        vec![
            (b"a".to_vec(), Op::Put(vec![1u8; 4000])),
            (b"b".to_vec(), Op::Put(vec![2u8; 4000])),
        ],
    ));
    seen.absorb(&out);
    assert_eq!(
        seen.of(1, 1),
        &[State::Accepted],
        "a write is accepted before anything is on the network"
    );
    let packs = ids(&out);
    assert!(
        packs.len() >= 2,
        "this fixture produced {} put(s); with fewer than two the loop below \
         is empty and proves nothing about waiting",
        packs.len()
    );
    assert!(
        head_of(&out).is_none(),
        "the head moved before a single pack was confirmed"
    );

    // Confirm all but one: still no head.
    for id in &packs[..packs.len() - 1] {
        let out = e.step(Event::PutConfirmed(*id));
        seen.absorb(&out);
        assert!(
            head_of(&out).is_none(),
            "the head moved while a pack was still unconfirmed"
        );
    }
    let out = e.step(Event::PutConfirmed(packs[packs.len() - 1]));
    seen.absorb(&out);
    let (seq, root, after) = head_of(&out).expect("the head must move once every pack is in");
    assert_eq!(seq, 1);
    assert_eq!(root, e.root());
    // The dependency is stated as well as obeyed, so a shell that batches
    // aggressively cannot get it wrong either.
    let named: BTreeSet<Cid> = after.into_iter().collect();
    assert_eq!(
        named,
        packs.iter().copied().collect::<BTreeSet<_>>(),
        "UpdateHead does not name every block it depends on"
    );
    // No state between Accepted and Published: what the head does not name
    // was never published, so nothing before the head moves can be promised
    // to survive a restart.
    assert_eq!(seen.of(1, 1), &[State::Accepted]);

    let out = e.step(Event::HeadConfirmed(seq));
    seen.absorb(&out);
    assert_eq!(
        seen.of(1, 1),
        &[State::Accepted, State::Published],
        "states must arrive once each, in order"
    );
}

#[test]
fn writes_from_two_clients_fold_into_one_commit_and_all_reach_published() {
    let mut e = Engine::default();
    let mut seen = Seen::default();

    // The first write opens a commit; the next three arrive while it is in
    // flight and must fold into the NEXT one, each keeping its own id.
    let first = e.step(write(1, 1, vec![put("k/1", b"one")]));
    seen.absorb(&first);
    for (client, id, key) in [(1u64, 2u64, "k/2"), (2, 3, "k/3"), (2, 4, "k/4")] {
        let out = e.step(write(client, id, vec![put(key, b"v")]));
        seen.absorb(&out);
        assert_eq!(
            ids(&out).len(),
            0,
            "a write during a commit opened a second commit"
        );
    }

    let n = ids(&first).len();
    let all = settle(&mut e, &mut seen, first, &(0..n).collect::<Vec<_>>());
    // The second commit was opened when the first was published.
    let second = ids(&all);
    assert!(!second.is_empty(), "the folded writes never shipped");
    let out: Vec<Effect> = all
        .into_iter()
        .filter(|e| matches!(e, Effect::PutPack { .. } | Effect::PutBlock { .. }))
        .collect();
    let n2 = ids(&out).len();
    let _ = settle(&mut e, &mut seen, out, &(0..n2).collect::<Vec<_>>());

    for (client, id) in [(1, 1), (1, 2), (2, 3), (2, 4)] {
        let states = seen.of(client, id);
        assert!(
            states.contains(&State::Published),
            "client {client} write {id} did not reach published: {states:?}"
        );
        // Each state at most once, and never backwards. A write that reports
        // Durable twice is a write whose caller cannot tell a retry from
        // progress.
        let mut sorted = states.to_vec();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            states.len(),
            "client {client} write {id} repeated a state: {states:?}"
        );
        assert!(
            states.windows(2).all(|w| w[0] < w[1]),
            "client {client} write {id} went backwards: {states:?}"
        );
    }
}

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// The differential: whatever order the network answers in, the root the
/// pipeline publishes is the root a from-scratch build produces.
///
/// This is the gate. Everything else here checks that the engine says the
/// right things; this checks that the tree it names is the right tree. A
/// pipeline that folds, retries and coalesces has many chances to lose or
/// double-apply a write, and all of them show up here as a different root.
///
/// Confirmations arrive shuffled, twice, and for ids nothing is waiting on;
/// some puts fail first and must be re-emitted. The seed is printed, so a
/// failure is reproducible from the output alone.
#[test]
fn any_interleaving_publishes_the_root_a_rebuild_produces() {
    // Counted across the whole sweep and asserted at the end. A failure
    // injected into a value block exercises a different branch from one
    // injected into a PACK, and only the pack branch could leave a commit with
    // nothing outstanding and no retry. This sweep missed that defect the
    // first time because it confirmed every id it had failed, regardless of
    // whether the retry produced anything.
    let mut pack_failures = 0usize;
    let mut retries_seen = 0usize;
    for seed in 1..=24u64 {
        let mut r = rng(seed);
        let mut e = Engine::default();
        let mut seen = Seen::default();
        let mut records: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut next_id = 0u64;

        for round in 0..6 {
            // A batch of writes, sometimes overwriting earlier keys.
            let n = 1 + (r() % 4) as usize;
            let mut ops = Vec::new();
            for _ in 0..n {
                let k = format!("k/{:04}", r() % 40).into_bytes();
                let v = vec![(r() % 251) as u8; 1 + (r() % 2000) as usize];
                records.insert(k.clone(), v.clone());
                ops.push((k, Op::Put(v)));
            }
            next_id += 1;
            let client = 1 + (r() % 2);
            let out = e.step(write(client, next_id, ops));
            seen.absorb(&out);
            live.push((client, next_id));

            // Answer whatever is outstanding, in a shuffled order, with
            // duplicates and strangers mixed in.
            let mut pending = ids(&out);
            for i in (1..pending.len()).rev() {
                pending.swap(i, (r() % (i as u64 + 1)) as usize);
            }
            let packs: BTreeSet<Cid> = pack_ids(&out).into_iter().collect();
            let mut queue: Vec<Cid> = Vec::new();
            for id in &pending {
                // A failure first, at a position the seed chooses.
                if r().is_multiple_of(3) {
                    let again = e.step(Event::PutFailed(*id));
                    seen.absorb(&again);
                    // A failed put MUST produce a retry, or the commit waits
                    // for ever on something that already failed.
                    assert!(
                        !again.is_empty(),
                        "seed {seed}: PutFailed re-emitted nothing, so the \
                         commit can never complete"
                    );
                    if packs.contains(id) {
                        pack_failures += 1;
                    }
                    retries_seen += 1;
                    queue.extend(ids(&again));
                }
                queue.push(*id);
                if r().is_multiple_of(4) {
                    queue.push(*id); // a duplicate confirmation
                }
                if r().is_multiple_of(5) {
                    // A confirmation for a block nothing is waiting on.
                    queue.push([(r() % 251) as u8; 32]);
                }
            }
            for id in queue {
                let out = e.step(Event::PutConfirmed(id));
                seen.absorb(&out);
                if let Some((seq, _, _)) = head_of(&out) {
                    let out = e.step(Event::HeadConfirmed(seq));
                    seen.absorb(&out);
                    // A published commit can open the next one.
                    let mut more = ids(&out);
                    for i in (1..more.len()).rev() {
                        more.swap(i, (r() % (i as u64 + 1)) as usize);
                    }
                    for id in more {
                        let out = e.step(Event::PutConfirmed(id));
                        seen.absorb(&out);
                        if let Some((seq, _, _)) = head_of(&out) {
                            let out = e.step(Event::HeadConfirmed(seq));
                            seen.absorb(&out);
                        }
                    }
                }
            }
            let _ = round;
        }

        // Drain anything still in flight so every write can settle.
        for _ in 0..8 {
            let out = e.step(Event::Tick(1000));
            seen.absorb(&out);
        }

        if e.published_root() != rebuild(&records) {
            println!(
                "seed {seed}: warm root matches rebuild? {}  published==warm? {}",
                e.root() == rebuild(&records),
                e.published_root() == e.root()
            );
        }
        assert_eq!(
            e.published_root(),
            rebuild(&records),
            "seed {seed}: the published root is not the root a rebuild produces"
        );
        // And no write was silently dropped.
        for (client, id) in &live {
            let states = seen.of(*client, *id);
            assert!(
                !states.is_empty(),
                "seed {seed}: write {id} was never reported at all"
            );
            // Monotone: a state never goes backwards.
            let mut last = None;
            for s in states {
                if let Some(p) = last {
                    assert!(
                        *s >= p,
                        "seed {seed}: write {id} went {p:?} -> {s:?}, backwards"
                    );
                }
                last = Some(*s);
            }
        }
        println!(
            "  seed {seed:2}: {} records, root matches a rebuild",
            records.len()
        );
    }
    // Without this the sweep can inject failures that never land on a pack and
    // report green over a branch it never entered.
    assert!(
        pack_failures > 0,
        "{retries_seen} put failures were injected and NONE hit a pack: the \
         pack retry path was not exercised"
    );
    println!("  {retries_seen} injected failures, {pack_failures} of them on packs");
}

/// Drive one write all the way to published, confirming in order.
fn commit(e: &mut Engine, seen: &mut Seen, ev: Event) -> Vec<Effect> {
    let out = e.step(ev);
    seen.absorb(&out);
    let mut all = out.clone();
    for id in ids(&out) {
        let o = e.step(Event::PutConfirmed(id));
        seen.absorb(&o);
        all.extend(o.clone());
        if let Some((seq, _, _)) = head_of(&o) {
            let o = e.step(Event::HeadConfirmed(seq));
            seen.absorb(&o);
            all.extend(o);
        }
    }
    all
}

fn parity_puts(effects: &[Effect]) -> Vec<(engine::ParityIds, Cid)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::PutParity { group, id, .. } => Some((*group, *id)),
            _ => None,
        })
        .collect()
}

/// Two commits touching one group pay for its redundancy ONCE.
///
/// Parity is a pure function of a group's members, so parity coded for
/// members that have already moved on protects bytes no reader will ask for.
/// Putting it is a PUT bought for nothing — and on a hot key that is every
/// commit, for ever.
///
/// The control is not a description: `coalesce_parity: false` puts each
/// commit's parity as it is published, and the test asserts it really does
/// pay twice. Without that, "one put" could be one because nothing was
/// emitted at all.
#[test]
fn two_commits_touching_one_group_put_its_parity_once() {
    // Values over 1 KiB are stored by reference, so the leaf carries parity
    // members and a rewrite re-codes a group.
    let big = |b: u8| vec![b; 1500];
    let seed: Vec<(Vec<u8>, Op)> = (0..24u32)
        .map(|i| (format!("k/{i:03}").into_bytes(), Op::Put(big(i as u8))))
        .collect();

    let count = |coalesce: bool| -> usize {
        let mut e = Engine::new(Params {
            coalesce_parity: coalesce,
            ..Params::default()
        });
        let mut seen = Seen::default();
        // Counted from the FIRST commit, not from the rewrites. The two modes
        // put a group's parity at different moments -- one when its commit is
        // published, the other on a later tick -- so a window that starts
        // after the seed commit counts one mode's work and not the other's.
        // That is not a comparison; it is two different questions. (It is also
        // what this test asserted at first, and it reported coalescing as
        // being more expensive.)
        let mut n = parity_puts(&commit(&mut e, &mut seen, write(1, 1, seed.clone()))).len();
        // Two further commits, each rewriting the SAME key, so the same group
        // is re-coded twice.
        n += parity_puts(&commit(
            &mut e,
            &mut seen,
            write(1, 2, vec![put("k/005", &big(0xAA))]),
        ))
        .len();
        n += parity_puts(&commit(
            &mut e,
            &mut seen,
            write(1, 3, vec![put("k/005", &big(0xBB))]),
        ))
        .len();
        // With coalescing on, the parity goes out on a tick once the group has
        // settled — which is the point: it waits to see whether the group is
        // still moving. Both modes are given the same ticks, so neither is
        // measured over a shorter window than the other.
        for t in 1..=3 {
            let out = e.step(Event::Tick(t));
            seen.absorb(&out);
            n += parity_puts(&out).len();
        }
        n
    };

    let coalesced = count(true);
    let every_time = count(false);
    assert!(
        coalesced > 0,
        "no parity was put at all, so this measures nothing"
    );
    assert!(
        every_time > coalesced,
        "the control put {every_time} blocks and coalescing put {coalesced}: \
         coalescing bought nothing, so either it is not working or this \
         fixture never re-codes a group"
    );
    println!(
        "  coalesced {coalesced} parity blocks, every-commit control {every_time} \
         ({} saved)",
        every_time - coalesced
    );
}

/// A group rewritten on every tick still gets its redundancy.
///
/// Coalescing defers a group that is still moving. Left at that, the hottest
/// key in the store — the one most worth protecting — would be the one that
/// never is. The age bound is what stops "wait until it settles" from meaning
/// "wait for ever".
#[test]
fn a_group_written_continuously_is_still_protected_within_the_age_bound() {
    let age = 5u64;
    let mut e = Engine::new(Params {
        parity_age: age,
        ..Params::default()
    });
    let mut seen = Seen::default();
    let big = |b: u8| vec![b; 1500];
    let seed: Vec<(Vec<u8>, Op)> = (0..24u32)
        .map(|i| (format!("k/{i:03}").into_bytes(), Op::Put(big(i as u8))))
        .collect();
    commit(&mut e, &mut seen, write(1, 1, seed));

    // Rewrite the same key on every tick, so the group is never still.
    let mut emitted = 0;
    for t in 1..=(age * 3) {
        let out = commit(
            &mut e,
            &mut seen,
            write(1, 100 + t, vec![put("k/005", &big(t as u8))]),
        );
        emitted += parity_puts(&out).len();
        let out = e.step(Event::Tick(t));
        seen.absorb(&out);
        emitted += parity_puts(&out).len();
    }
    assert!(
        emitted > 0,
        "a group rewritten on every one of {} ticks was never protected: \
         the age bound does not fire under continuous writes",
        age * 3
    );
    println!("  {emitted} parity block(s) under continuous rewriting");
}

/// The core never looks inside a value.
///
/// Sealed values are coming (§11), and the engine must not be the thing that
/// has to change: it moves bytes it cannot read. Opaque bytes go in and the
/// exact same bytes come out, whether they ride in a pack or get their own
/// PUT.
#[test]
fn an_opaque_value_passes_through_untouched() {
    let mut e = Engine::new(Params {
        // Small enough that the large value takes its own PUT, so both paths
        // are exercised in one test.
        max_packed_value: 4096,
        ..Params::default()
    });
    let mut seen = Seen::default();
    let sealed: Vec<u8> = (0..9000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let small: Vec<u8> = (0..2000u32).map(|i| (i ^ 0x5a) as u8).collect();

    let out = e.step(write(
        1,
        1,
        vec![
            (b"sealed".to_vec(), Op::Put(sealed.clone())),
            (b"small".to_vec(), Op::Put(small.clone())),
        ],
    ));
    seen.absorb(&out);

    let direct: Vec<&Vec<u8>> = out
        .iter()
        .filter_map(|e| match e {
            Effect::PutBlock { bytes, .. } => Some(bytes),
            _ => None,
        })
        .collect();
    assert!(
        direct.iter().any(|b| **b == sealed),
        "the large opaque value is not among the blocks put, byte for byte"
    );
    // And the small one is inside a pack, unaltered.
    let packed: Vec<u8> = out
        .iter()
        .filter_map(|e| match e {
            Effect::PutPack { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        packed.windows(small.len()).any(|w| w == small.as_slice()),
        "the small opaque value was altered on its way into a pack"
    );
}

/// No sequence of events makes the core panic.
///
/// A delegate is re-entered with whatever the network hands it, including
/// answers to questions nobody asked. The count is printed and asserted so a
/// run that explored nothing cannot read as a pass.
#[test]
fn no_event_sequence_panics() {
    let mut cases = 0;
    for seed in 1..=40u64 {
        let mut r = rng(seed);
        let mut e = Engine::new(Params {
            max_backlog: 4096,
            parity_age: 2,
            ..Params::default()
        });
        let mut known: Vec<Cid> = Vec::new();
        for _ in 0..60 {
            let ev = match r() % 6 {
                0 => write(
                    1 + r() % 3,
                    r() % 20,
                    vec![(
                        format!("k/{}", r() % 30).into_bytes(),
                        if r().is_multiple_of(5) {
                            Op::Delete
                        } else {
                            Op::Put(vec![(r() % 251) as u8; (r() % 3000) as usize])
                        },
                    )],
                ),
                1 => Event::PutConfirmed(pick(&mut r, &known)),
                2 => Event::PutFailed(pick(&mut r, &known)),
                3 => Event::HeadConfirmed(r() % 8),
                _ => Event::Tick(r() % 16),
            };
            let out = e.step(ev);
            known.extend(ids(&out));
            for (_, id) in parity_puts(&out) {
                known.push(id);
            }
            if known.len() > 200 {
                known.drain(..100);
            }
            cases += 1;
        }
    }
    assert!(cases >= 2000, "only {cases} events were driven");
    println!("  {cases} events across 40 seeds, no panic");
}

fn pick(r: &mut impl FnMut() -> u64, known: &[Cid]) -> Cid {
    if known.is_empty() || r().is_multiple_of(4) {
        [(r() % 251) as u8; 32]
    } else {
        known[(r() % known.len() as u64) as usize]
    }
}

/// Beyond the backlog bound a write is refused, and refused CLEANLY.
///
/// A queue with no bound eventually eats the node. The refusal must leave no
/// trace: the client will send it again, and a write that was both applied
/// and refused is the worst of both.
#[test]
fn a_full_backlog_refuses_a_write_without_applying_it() {
    let mut e = Engine::new(Params {
        max_backlog: 4000,
        ..Params::default()
    });
    let mut seen = Seen::default();
    let out = e.step(write(1, 1, vec![(b"a".to_vec(), Op::Put(vec![1u8; 3000]))]));
    seen.absorb(&out);
    let root_before = e.root();

    let out = e.step(write(1, 2, vec![(b"b".to_vec(), Op::Put(vec![2u8; 3000]))]));
    seen.absorb(&out);
    assert_eq!(
        seen.of(1, 2),
        &[State::Busy],
        "an over-budget write was accepted"
    );
    assert_eq!(
        e.root(),
        root_before,
        "a refused write changed the tree anyway"
    );
    assert!(
        ids(&out).is_empty(),
        "a refused write put blocks on the network"
    );
}

/// The write path is proportional to the tree's DEPTH, not to its size.
///
/// This is a correctness property of the delegate, not a nicety: a delegate
/// call is bounded at 5 s, and the version of this engine that scanned the
/// whole tree for superseded groups would have parsed on the order of 10^5
/// nodes per keystroke at a million keys. It was correct, and it passed every
/// other test in this file.
///
/// The bound is stated against the tree's depth with room to spare, and the
/// whole-tree scan is kept as a parameter so the control can blow it. A bound
/// nothing can exceed is not a bound.
#[test]
fn a_single_key_write_parses_nodes_in_proportion_to_depth() {
    let seed: Vec<(Vec<u8>, Op)> = (0..10_000u32)
        .map(|i| {
            (
                format!("k/{i:06}").into_bytes(),
                Op::Put(vec![(i % 251) as u8; 60]),
            )
        })
        .collect();

    let measure = |whole: bool| -> usize {
        let mut e = Engine::new(Params {
            whole_tree_supersede_scan: whole,
            ..Params::default()
        });
        let mut seen = Seen::default();
        commit(&mut e, &mut seen, write(1, 1, seed.clone()));
        // Measure ONE key changing, on a tree that is already large.
        e.reset_cost();
        let _ = e.step(write(1, 2, vec![put("k/005000", b"changed")]));
        e.nodes_parsed()
    };

    let diff_walk = measure(false);
    let whole_tree = measure(true);

    // A 10k-key tree is 3 levels; 8 per level is generous and still nowhere
    // near a scan.
    const DEPTH: usize = 3;
    const PER_LEVEL: usize = 8;
    assert!(
        diff_walk <= DEPTH * PER_LEVEL,
        "a one-key write parsed {diff_walk} nodes on a 10,000-key tree; the \
         walk is not following only what changed"
    );
    assert!(
        whole_tree > DEPTH * PER_LEVEL * 4,
        "the control parsed only {whole_tree} nodes, so it is not the \
         whole-tree scan and this bound is not being tested against anything"
    );
    println!("  one-key write: {diff_walk} nodes parsed (whole-tree control: {whole_tree})");
}

/// Settings a caller can choose must not panic the core three steps later.
///
/// `max_packed_value` above `max_pack` describes a value that must ride in a
/// pack and cannot fit in one. The planner met that as an `unreachable!`,
/// far from the setting that caused it and with nothing in the message
/// pointing back at it.
#[test]
fn impossible_params_are_refused_where_they_are_set() {
    let bad = Params {
        max_pack: 4096,
        max_packed_value: 8192,
        ..Params::default()
    };
    let panicked = std::panic::catch_unwind(move || Engine::new(bad));
    assert!(
        panicked.is_err(),
        "a pack size smaller than the values that must fit in it was accepted"
    );
    // And the pair that only just fits is accepted, so the refusal is the
    // relationship and not a blanket rejection of small packs.
    let ok = Params {
        max_pack: 4096,
        max_packed_value: 4096 - 11,
        ..Params::default()
    };
    let _ = Engine::new(ok);
}

/// A write whose group is re-coded before its parity goes out waits for the
/// NEW coding, not for the one that was abandoned.
///
/// `ParityComplete` means *every group that currently covers what this write
/// changed has its parity on the network*. It must never mean *nothing is
/// outstanding under this write's name*: when a later write re-codes a group,
/// the earlier write's data has not gone anywhere — it now sits in the newer
/// coding, whose parity is not out either. Reporting completion there tells a
/// client that redundancy exists for its data when none does, and a client
/// that believes it has no reason ever to check again.
///
/// The control is the old behaviour, and it runs: with the transfer off, A is
/// reported complete while B's parity is still owed.
#[test]
fn a_write_whose_group_is_re_coded_waits_for_the_new_coding() {
    let big = |b: u8| vec![b; 1500];
    let seed: Vec<(Vec<u8>, Op)> = (0..24u32)
        .map(|i| (format!("k/{i:03}").into_bytes(), Op::Put(big(i as u8))))
        .collect();

    // Returns: was A complete before B's parity was confirmed, and how many
    // ParityComplete notifications A got in total.
    let run = |transfer: bool| -> (bool, usize) {
        let mut e = Engine::new(Params {
            transfer_superseded_waiters: transfer,
            ..Params::default()
        });
        let mut seen = Seen::default();
        commit(&mut e, &mut seen, write(1, 1, seed.clone()));

        // A: touches one key. Its commit codes the group holding it.
        let a = commit(
            &mut e,
            &mut seen,
            write(1, 2, vec![put("k/005", &big(0xAA))]),
        );
        assert!(
            parity_puts(&a).is_empty(),
            "A's parity went out immediately, so it was never superseded and \
             this test is about nothing"
        );
        // B: re-codes the SAME group, before any parity has been put.
        let b = commit(
            &mut e,
            &mut seen,
            write(1, 3, vec![put("k/005", &big(0xBB))]),
        );
        let _ = b;

        let a_done_early = seen.of(1, 2).contains(&State::ParityComplete);

        // Now let the parity go out and be confirmed.
        let mut queue: Vec<Effect> = Vec::new();
        for t in 1..=4 {
            let out = e.step(Event::Tick(t));
            seen.absorb(&out);
            queue.extend(out);
        }
        let mut guard = 0;
        while let Some(f) = queue.pop() {
            guard += 1;
            assert!(guard < 5_000, "the run did not settle");
            if let Effect::PutParity { id, .. } = f {
                let out = e.step(Event::PutConfirmed(id));
                seen.absorb(&out);
                queue.extend(out);
            }
        }
        let total = seen
            .of(1, 2)
            .iter()
            .filter(|s| **s == State::ParityComplete)
            .count();
        (a_done_early, total)
    };

    let (early_with_transfer, total_with_transfer) = run(true);
    let (early_without, _) = run(false);

    assert!(
        !early_with_transfer,
        "A was reported ParityComplete while the group covering its data had \
         no parity on the network"
    );
    assert_eq!(
        total_with_transfer, 1,
        "A must reach ParityComplete exactly once, after the coding that now \
         covers it is confirmed"
    );
    // The control: without the transfer, A completes early. If this does not
    // happen the two runs are the same run and the assertion above is idle.
    assert!(
        early_without,
        "the control did not complete A early, so it is not the old behaviour \
         and proves nothing about the new one"
    );
    println!("  with transfer: A completes once, after the new coding; control completes it early");
}
