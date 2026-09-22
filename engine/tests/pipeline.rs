//! Acceptance for the write pipeline (craftworks-sdk#24).
//!
//! Everything here is deterministic: no node, no clock, no randomness that is
//! not seeded and printed. That is the whole point of a sans-IO core — an
//! interleaving a live network produces once a week is an ordinary test here.

use engine::{ClientId, Effect, Engine, Event, Op, Params, State, WriteId};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

mod common;
use common::{rebuild, Harness, Mode, Store};

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
        reads: Vec::new(),
    }
}

fn put(k: &str, v: &[u8]) -> (Vec<u8>, Op) {
    (k.as_bytes().to_vec(), Op::Put(v.to_vec()))
}

/// The states reported for each write, in the order they were reported.
#[derive(Default, PartialEq, Eq, Debug)]
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

/// Is this a sequence a write can legally report?
///
/// A prefix of `accepted (stalled)? published parity-complete`, or a terminal
/// (`failed` / `lost` / `busy`) with nothing after it. Each state at most
/// once: a caller that is told `Published` twice cannot tell a retry from
/// progress.
///
/// Written out rather than derived from the enum's declaration order, because
/// the legal ORDER is a rule about the protocol and the enum's order is an
/// implementation detail that happens to agree with it today. It is also what
/// catches the shape that was wrong here before: `Failed` followed by
/// `Published`, which is two contradictory answers to one question.
pub fn valid_sequence(states: &[State]) -> bool {
    const HAPPY: [State; 4] = [
        State::Accepted,
        State::Stalled,
        State::Published,
        State::ParityComplete,
    ];
    let mut at = 0usize;
    for (i, s) in states.iter().enumerate() {
        if matches!(s, State::Failed | State::Lost | State::Busy) {
            return i + 1 == states.len();
        }
        match HAPPY[at..].iter().position(|h| h == s) {
            Some(k) => at += k + 1,
            None => return false,
        }
    }
    true
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

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// Drive a commit from its puts to `published`, confirming in the given order.
fn settle(
    e: &mut Engine<Store>,
    seen: &mut Seen,
    effects: Vec<Effect>,
    order: &[usize],
) -> Vec<Effect> {
    // The caller has already absorbed `effects`; absorbing them again would
    // record every notification in them twice and make "once per state" read
    // as a duplicate.
    let mut all = effects.clone();
    let pending = ids(&effects);
    for i in order {
        let out = stepped!(e, Event::PutConfirmed(pending[*i]));
        seen.absorb(&out);
        all.extend(out.clone());
        if let Some((seq, _, _)) = head_of(&out) {
            let out = stepped!(e, Event::HeadConfirmed(seq));
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
    let mut e = common::new_store_params(Params {
        max_packed_value: 1024,
        ..Params::default()
    });
    let mut seen = Seen::default();

    let out = stepped!(
        e,
        write(
            1,
            1,
            vec![
                (b"a".to_vec(), Op::Put(vec![1u8; 4000])),
                (b"b".to_vec(), Op::Put(vec![2u8; 4000])),
            ],
        )
    );
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
        let out = stepped!(e, Event::PutConfirmed(*id));
        seen.absorb(&out);
        assert!(
            head_of(&out).is_none(),
            "the head moved while a pack was still unconfirmed"
        );
    }
    let out = stepped!(e, Event::PutConfirmed(packs[packs.len() - 1]));
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

    let out = stepped!(e, Event::HeadConfirmed(seq));
    seen.absorb(&out);
    assert_eq!(
        seen.of(1, 1),
        &[State::Accepted, State::Published],
        "states must arrive once each, in order"
    );
}

#[test]
fn a_write_during_a_commit_is_refused_and_leaves_no_trace() {
    // One commit at a time. Folding needed the folded write's blocks to
    // survive until the next commit, and the core keeps no blocks — so a
    // write that arrives mid-commit is REFUSED rather than quietly carried.
    // Buffering is the client's job: it has a page and an outbox, and the
    // delegate has neither.
    let mut e = common::new_store_params(Params::default());
    let mut seen = Seen::default();

    let first = stepped!(e, write(1, 1, vec![put("k/1", b"one")]));
    seen.absorb(&first);
    assert!(!ids(&first).is_empty(), "the first write shipped nothing");
    let root_after_first = e.root();

    for (client, id, key) in [(1u64, 2u64, "k/2"), (2, 3, "k/3")] {
        let out = stepped!(e, write(client, id, vec![put(key, b"v")]));
        seen.absorb(&out);
        assert_eq!(
            seen.of(client, id),
            &[State::Busy],
            "a write during a commit was not refused"
        );
        assert!(
            ids(&out).is_empty(),
            "a refused write put blocks on the network"
        );
        assert_eq!(
            e.root(),
            root_after_first,
            "a refused write changed the tree anyway: it left a trace"
        );
    }

    // The first write still completes normally.
    let n = ids(&first).len();
    let _ = settle(&mut e, &mut seen, first, &(0..n).collect::<Vec<_>>());
    assert!(
        seen.of(1, 1).contains(&State::Published),
        "the in-flight write did not publish: {:?}",
        seen.of(1, 1)
    );
    // And once it has, the next write is accepted.
    let out = stepped!(e, write(1, 4, vec![put("k/4", b"after")]));
    seen.absorb(&out);
    assert_eq!(
        seen.of(1, 4).first(),
        Some(&State::Accepted),
        "a write after the commit published was still refused"
    );
}

/// One seed of the interleaving sweep, driven in one mode.
///
/// Split out of the test so the SAME interleaving runs both ways. The rng is
/// seeded per call, so the two modes see an identical sequence of writes,
/// failures, duplicates and strangers -- the only difference is whether the
/// engine survives between steps.
struct SweepSeed {
    published: Cid,
    expected: Cid,
    seen: Seen,
    live: Vec<(u64, u64)>,
    pack_failures: usize,
    direct_failures: usize,
    directs: usize,
    retries: usize,
    max_context: usize,
}

fn sweep_seed(mode: Mode, params: Params, seed: u64) -> SweepSeed {
    let mut r = rng(seed);
    // A store of its own: two modes over one store would let the second read
    // blocks the first put, and a rehydrate that only works because the OTHER
    // run left its blocks behind has proved nothing.
    let mut h = Harness::new(mode, params, Store::fresh());
    let mut seen = Seen::default();
    let mut records: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut live: Vec<(u64, u64)> = Vec::new();
    let mut next_id = 0u64;
    let mut pack_failures = 0usize;
    let mut direct_failures = 0usize;
    let mut directs = 0usize;
    let mut retries_seen = 0usize;

    for _round in 0..6 {
        // A batch of writes, sometimes overwriting earlier keys.
        let n = 1 + (r() % 4) as usize;
        let mut ops = Vec::new();
        for _ in 0..n {
            let k = format!("k/{:04}", r() % 40).into_bytes();
            // Most values ride in the pack; one in eight is over
            // `max_packed_value` and is PUT on its own. Every value used to be
            // under 2 KiB against a 64 KiB threshold, so the direct branch --
            // the one whose failure leaves a commit with an outstanding put
            // that is not in any pack -- was never entered by this sweep at
            // all, and `direct_failures` below is the floor that says so.
            let big = r().is_multiple_of(8);
            let len = if big {
                params.max_packed_value + 1 + (r() % 4096) as usize
            } else {
                1 + (r() % 2000) as usize
            };
            let v = vec![(r() % 251) as u8; len];
            records.insert(k.clone(), v.clone());
            ops.push((k, Op::Put(v)));
        }
        next_id += 1;
        let client = 1 + (r() % 2);
        let out = h.step(write(client, next_id, ops));
        seen.absorb(&out);
        live.push((client, next_id));

        // Answer whatever is outstanding, in a shuffled order, with
        // duplicates and strangers mixed in.
        let mut pending = ids(&out);
        for i in (1..pending.len()).rev() {
            pending.swap(i, (r() % (i as u64 + 1)) as usize);
        }
        let packs: BTreeSet<Cid> = pack_ids(&out).into_iter().collect();
        directs += ids(&out).iter().filter(|i| !packs.contains(*i)).count();
        let mut queue: Vec<Cid> = Vec::new();
        for id in &pending {
            // A failure first, at a position the seed chooses.
            if r().is_multiple_of(3) {
                let again = h.step(Event::PutFailed(*id));
                seen.absorb(&again);
                // A failed put MUST produce a retry, or the commit waits
                // for ever on something that already failed.
                assert!(
                    !again.is_empty(),
                    "seed {seed} ({mode:?}): PutFailed re-emitted nothing, so \
                     the commit can never complete"
                );
                if packs.contains(id) {
                    pack_failures += 1;
                } else {
                    direct_failures += 1;
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
            let out = h.step(Event::PutConfirmed(id));
            seen.absorb(&out);
            if let Some((seq, _, _)) = head_of(&out) {
                let out = h.step(Event::HeadConfirmed(seq));
                seen.absorb(&out);
                // A published commit can open the next one.
                let mut more = ids(&out);
                for i in (1..more.len()).rev() {
                    more.swap(i, (r() % (i as u64 + 1)) as usize);
                }
                for id in more {
                    let out = h.step(Event::PutConfirmed(id));
                    seen.absorb(&out);
                    if let Some((seq, _, _)) = head_of(&out) {
                        let out = h.step(Event::HeadConfirmed(seq));
                        seen.absorb(&out);
                    }
                }
            }
        }
    }

    // Drain anything still in flight so every write can settle.
    for _ in 0..8 {
        let out = h.step(Event::Tick(1000));
        seen.absorb(&out);
    }

    SweepSeed {
        published: h.published_root(),
        expected: rebuild(&records),
        seen,
        live,
        pack_failures,
        direct_failures,
        directs,
        retries: retries_seen,
        max_context: h.max_context,
    }
}

/// Every interleaving publishes the root a rebuild produces -- and it does so
/// whether the engine survives between steps or is rebuilt from its context
/// each time.
///
/// The delegate is the rehydrating case: a fresh wasm instance, and a fresh
/// linear memory, on every message. Running the sweep only in `Live` tested
/// the one mode production never uses. Both modes run the same interleaving
/// and must agree with each other AND with a rebuild -- three-way, because
/// two runs that agree on the wrong answer agree just as loudly.
#[test]
fn any_interleaving_publishes_the_root_a_rebuild_produces() {
    // Counted across the whole sweep and asserted at the end. A failure
    // injected into a value block exercises a different branch from one
    // injected into a PACK, and only the pack branch could leave a commit with
    // nothing outstanding and no retry. This sweep missed that defect the
    // first time because it confirmed every id it had failed, regardless of
    // whether the retry produced anything.
    let mut pack_failures = 0usize;
    let mut direct_failures = 0usize;
    let mut directs = 0usize;
    let mut retries_seen = 0usize;
    let mut max_context = 0usize;
    // BOTH write paths. Phase 3 ships members only; Phase 4 (#39) turns the
    // pack back on, and the format has to keep working until then — a sweep
    // over one of them would let the other rot with nothing to say so.
    for (arm, params) in [
        ("members", Params::default()),
        (
            "packed",
            Params {
                pack_on_write: true,
                ..Params::default()
            },
        ),
    ] {
        for seed in 1..=24u64 {
            let l = sweep_seed(Mode::Live, params, seed);
            let d = sweep_seed(Mode::Rehydrate, params, seed);

            assert_eq!(
                l.published, d.published,
                "seed {seed}: an engine rebuilt from its context between every \
             step published a different root from one that survived, so \
             something the pipeline needs is not in the context"
            );
            for s in [&l, &d] {
                assert_eq!(
                    s.published, s.expected,
                    "seed {seed}: the published root is not the root a rebuild \
                 produces"
                );
            }
            // And no write was silently dropped, or reported an impossible life.
            for s in [&l, &d] {
                for (client, id) in &s.live {
                    let states = s.seen.of(*client, *id);
                    assert!(
                        !states.is_empty(),
                        "seed {seed}: write {id} was never reported at all"
                    );
                    assert!(
                        valid_sequence(states),
                        "seed {seed}: write {id} reported an impossible sequence: \
                     {states:?}"
                    );
                }
            }
            // The states themselves must agree too: a rehydrate that reaches the
            // right root while telling a client something different is still a
            // bug the root comparison cannot see.
            assert_eq!(
                l.seen, d.seen,
                "seed {seed}: the two modes reported different write states"
            );
            pack_failures += l.pack_failures + d.pack_failures;
            direct_failures += l.direct_failures + d.direct_failures;
            directs += l.directs + d.directs;
            retries_seen += l.retries + d.retries;
            max_context = max_context.max(d.max_context);
            println!("  {arm} seed {seed:2}: both modes match a rebuild");
        }
    }
    // Without this the sweep can inject failures that never land on a pack and
    // report green over a branch it never entered.
    // The pack arm must have hit the pack path. With packing off the write
    // path this counts zero for the members arm, which is correct and is why
    // the sweep runs both.
    assert!(
        pack_failures > 0,
        "{retries_seen} put failures were injected and NONE hit a pack across \
         BOTH arms: the pack retry path was not exercised, and Phase 4 is \
         where it comes back"
    );
    // The other half of the same floor. A pack retry and a direct-block retry
    // are different branches, and a sweep whose values are all small emits no
    // direct block to fail -- which is what this one did until the fixture
    // above was widened.
    assert!(
        directs > 0,
        "the sweep emitted no direct PutBlock at all, so no value exceeded \
         max_packed_value and the pack/direct split was never taken"
    );
    assert!(
        direct_failures > 0,
        "{directs} direct block puts were emitted and NONE was failed: the \
         direct retry path was not exercised"
    );
    println!(
        "  {retries_seen} injected failures: {pack_failures} on packs, \
         {direct_failures} on direct blocks (of {directs} emitted); largest \
         context {max_context} B"
    );
}

/// The control for the sweep above: leave one field out of the context, and
/// the two modes must stop agreeing.
///
/// `context_carries_pending: false` drops the commit in flight. Everything
/// else is identical. Without this, "Live and Rehydrate agree" would hold
/// just as well over an engine that kept its state in a global, or a Harness
/// whose rehydrate mode quietly did nothing -- the assertion would be
/// measuring the harness, not the context.
#[test]
fn dropping_one_context_field_makes_the_two_modes_disagree() {
    let blind = Params {
        context_carries_pending: false,
        ..Params::default()
    };
    let mut diverged = 0usize;
    for seed in 1..=24u64 {
        let good = sweep_seed(Mode::Live, Params::default(), seed);
        let blinded = std::panic::catch_unwind(move || sweep_seed(Mode::Rehydrate, blind, seed));
        // Either the run falls over (a retry that re-emits nothing is itself
        // the forgotten commit showing) or it finishes at the wrong root.
        // Both are divergence; neither is agreement.
        match blinded {
            Err(_) => diverged += 1,
            Ok(b) if b.published != good.published => diverged += 1,
            Ok(_) => {}
        }
    }
    assert_eq!(
        diverged, 24,
        "only {diverged} of 24 seeds noticed that the in-flight commit was \
         left out of the context, so the both-modes comparison does not see \
         a field going missing"
    );
    println!("  control: all 24 seeds diverged with one context field dropped");
}

/// Drive one write all the way to published, confirming in order.
fn commit(e: &mut Engine<Store>, seen: &mut Seen, ev: Event) -> Vec<Effect> {
    let out = stepped!(e, ev);
    seen.absorb(&out);
    let mut all = out.clone();
    for id in ids(&out) {
        let o = stepped!(e, Event::PutConfirmed(id));
        seen.absorb(&o);
        all.extend(o.clone());
        if let Some((seq, _, _)) = head_of(&o) {
            let o = stepped!(e, Event::HeadConfirmed(seq));
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
        let mut e = common::new_store_params(Params {
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
            let out = stepped!(e, Event::Tick(t));
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
    let mut e = common::new_store_params(Params {
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
        let out = stepped!(e, Event::Tick(t));
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
    let mut e = common::new_store_params(Params {
        // Small enough that the large value takes its own PUT, so both paths
        // are exercised in one test — which needs the pack path ON, since
        // Phase 3 leaves it off and there would otherwise be only one path.
        max_packed_value: 4096,
        pack_on_write: true,
        ..Params::default()
    });
    let mut seen = Seen::default();
    let sealed: Vec<u8> = (0..9000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let small: Vec<u8> = (0..2000u32).map(|i| (i ^ 0x5a) as u8).collect();

    let out = stepped!(
        e,
        write(
            1,
            1,
            vec![
                (b"sealed".to_vec(), Op::Put(sealed.clone())),
                (b"small".to_vec(), Op::Put(small.clone())),
            ],
        )
    );
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
        let mut e = common::new_store_params(Params {
            max_write_bytes: 4096,
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
            let out = stepped!(e, ev);
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
    let mut e = common::new_store_params(Params {
        max_write_bytes: 4000,
        ..Params::default()
    });
    let mut seen = Seen::default();
    let out = stepped!(
        e,
        write(1, 1, vec![(b"a".to_vec(), Op::Put(vec![1u8; 3000]))])
    );
    seen.absorb(&out);
    let root_before = e.root();

    let out = stepped!(
        e,
        write(1, 2, vec![(b"b".to_vec(), Op::Put(vec![2u8; 3000]))])
    );
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
        let mut e = common::new_store_params(Params {
            whole_tree_supersede_scan: whole,
            // The 10,000-key SEED below is a fixture, not a live commit. The
            // commit cap exists because a pack's bytes do not survive the
            // `process()` return that made them, and nothing here returns;
            // capping the seed would mean this test could only ever measure
            // a tree one commit deep, which is the opposite of what it is
            // for. The single-key write it then measures IS under the cap.
            max_commit_blocks: usize::MAX,
            // And it never saves a context, so it has no bound to keep.
            max_context_bytes: usize::MAX,
            ..Params::default()
        });
        let mut seen = Seen::default();
        commit(&mut e, &mut seen, write(1, 1, seed.clone()));
        // Measure ONE key changing, on a tree that is already large.
        e.reset_cost();
        let _ = stepped!(e, write(1, 2, vec![put("k/005000", b"changed")]));
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
    let panicked = std::panic::catch_unwind(move || Engine::new(bad, Store::default()));
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
    let _ = Engine::new(ok, Store::default());
}

/// The limit that bites a packed member is its KIND's, not the pack's.
///
/// The check above bounded `max_packed_value` against `max_pack` — the
/// container — and packed members are `RAW`, whose ceiling is 262,208. So
/// every setting from 262,209 up to `max_pack - 11` satisfied it: with a
/// 1 MiB pack that is a **786,357-wide band** the engine accepted and that
/// builds packs the contract refuses PER MEMBER.
///
/// That band is worse than a panic. The refusal happens at the NODE, remotely,
/// where nothing local says why (F48) — the diagnosable local failure existed
/// and was unreachable, so what survived was the least diagnosable one.
///
/// Driven at the boundary rather than deep in the band: an off-by-one here
/// either refuses a setting the network accepts or admits one it does not, and
/// a value picked from the middle cannot tell those apart.
#[test]
fn a_packed_value_over_its_kinds_limit_is_refused_where_it_is_set() {
    let raw_limit = engine::pack::max_body(freenet_prolly::kind::RAW);
    assert_eq!(raw_limit, 262_208, "the contract's RAW ceiling");

    // Inside the old band: it fits a 1 MiB pack, so the pack-size check passes
    // and ONLY the per-member one can catch it.
    let bad = Params {
        max_pack: 1024 * 1024,
        max_packed_value: raw_limit + 1,
        ..Params::default()
    };
    assert!(
        std::panic::catch_unwind(move || Engine::new(bad, Store::default())).is_err(),
        "max_packed_value {} fits the pack but not a RAW member, and was accepted",
        raw_limit + 1,
    );

    // And exactly at the limit is accepted, so this is the relationship and
    // not a blanket refusal of large values.
    let ok = Params {
        max_pack: 1024 * 1024,
        max_packed_value: raw_limit,
        ..Params::default()
    };
    let _ = Engine::new(ok, Store::default());
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
        let mut e = common::new_store_params(Params {
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
            let out = stepped!(e, Event::Tick(t));
            seen.absorb(&out);
            queue.extend(out);
        }
        let mut guard = 0;
        while let Some(f) = queue.pop() {
            guard += 1;
            assert!(guard < 5_000, "the run did not settle");
            if let Effect::PutParity { id, .. } = f {
                let out = stepped!(e, Event::PutConfirmed(id));
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

/// The page-closed promise, with no clock at all.
///
/// `Tick` arrives only from a connected client (W4: the released node fires
/// no wake-ups), so once the tab closes nothing will ever ask the engine to
/// get on with it. Everything the promise covers must therefore be
/// event-driven — a commit in flight advances on its own confirmations — or
/// must happen on `Flush`, which the shell sends on disconnect.
///
/// So this drives writes, sends `Flush`, and then NEVER sends a tick. Every
/// write must still reach `Published` and `ParityComplete`.
///
/// The control is the same run with no `Flush`: owed parity stays owed for
/// ever, and the test sees it. Without that, "parity was complete" would
/// read identically in a build where `Flush` did nothing.
#[test]
fn after_a_disconnect_every_write_publishes_and_its_parity_is_put_without_a_tick() {
    let run = |flush: bool| -> (Seen, usize, usize) {
        let store = Store::fresh();
        let mut e = Engine::new(
            Params {
                coalesce_parity: true,
                ..Params::default()
            },
            store.clone(),
        );
        // A NEW tree, and the engine is told so: a write before the head is
        // recovered waits for it (sdk#223). This test is about a DISCONNECT
        // after that, not about recovery.
        let _ = e.step(Event::HeadMissing);
        let mut seen = Seen::default();
        // Values by reference, so the leaves carry parity over them.
        let mut queue = Vec::new();
        for n in 1..=3u64 {
            let ops: Vec<(Vec<u8>, Op)> = (0..16u32)
                .map(|i| {
                    (
                        format!("k/{n}/{i:04}").into_bytes(),
                        Op::Put(vec![(i % 251) as u8; 1400]),
                    )
                })
                .collect();
            let out = stepped!(e, write(1, n, ops));
            seen.absorb(&out);
            queue.extend(out.clone());
            // Drive this commit to published before the next write, since one
            // commit at a time means the next would otherwise be refused.
            let mut guard = 0;
            while let Some(f) = queue.pop() {
                guard += 1;
                assert!(guard < 100_000, "the commit did not settle");
                let o = match &f {
                    Effect::PutPack { id, .. }
                    | Effect::PutBlock { id, .. }
                    | Effect::PutParity { id, .. } => stepped!(e, Event::PutConfirmed(*id)),
                    Effect::UpdateHead { seq, .. } => stepped!(e, Event::HeadConfirmed(*seq)),
                    _ => Vec::new(),
                };
                seen.absorb(&o);
                queue.extend(o);
            }
        }

        // The client goes away. No tick is sent here, or ever.
        if flush {
            let out = stepped!(e, Event::Flush);
            seen.absorb(&out);
            queue.extend(out);
        }
        let mut guard = 0;
        let mut parity_put = 0usize;
        while let Some(f) = queue.pop() {
            guard += 1;
            assert!(guard < 100_000, "the flush did not settle");
            let o = match &f {
                Effect::PutParity { id, .. } => {
                    parity_put += 1;
                    stepped!(e, Event::PutConfirmed(*id))
                }
                Effect::PutPack { id, .. } | Effect::PutBlock { id, .. } => {
                    stepped!(e, Event::PutConfirmed(*id))
                }
                Effect::UpdateHead { seq, .. } => stepped!(e, Event::HeadConfirmed(*seq)),
                _ => Vec::new(),
            };
            seen.absorb(&o);
            queue.extend(o);
        }
        (seen, parity_put, e.owed_groups())
    };

    let (seen, put, _) = run(true);
    for n in 1..=3u64 {
        let states = seen.of(1, n);
        assert!(
            states.contains(&State::Published),
            "write {n} did not publish after a disconnect: {states:?}"
        );
        assert!(
            states.contains(&State::ParityComplete),
            "write {n} published but its parity was never complete, and no \
             tick is ever coming: {states:?}"
        );
        assert!(
            valid_sequence(states),
            "write {n} reported an impossible sequence: {states:?}"
        );
    }
    assert!(
        put > 0,
        "Flush put no parity at all, so ParityComplete above says nothing"
    );

    // The control: no Flush, no tick, and the parity stays owed.
    let (control, control_put, _) = run(false);
    let complete = (1..=3u64)
        .filter(|n| control.of(1, *n).contains(&State::ParityComplete))
        .count();
    assert_eq!(
        control_put, 0,
        "the control put {control_put} parity block(s) without a Flush or a \
         tick, so Flush is not what puts them"
    );
    assert_eq!(
        complete, 0,
        "{complete} write(s) reached ParityComplete with no Flush and no \
         tick, so the test cannot tell a working Flush from a no-op"
    );
    println!("  disconnect, no ticks ever: 3 writes published, {put} parity block(s) put (control without Flush: {control_put})");
}
