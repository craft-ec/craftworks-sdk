//! The restart gate (craftworks-sdk#29).
//!
//! This is the phase's promise: the engine is dropped at every state boundary
//! of every commit and restarted against a network that kept EXACTLY what had
//! been confirmed, and nothing published is lost, nothing is applied twice,
//! and no head ever names a root whose blocks are not all there.
//!
//! It is also the reason slice 1 is a pure state machine. A crash at a
//! precise moment is an ordinary value being dropped here; against a live
//! node it is a race nobody can schedule.

use engine::{ClientId, Effect, Engine, Epoch, Event, KeySource, Op, Params, State, WriteId};
use freenet_prolly::build::TreeBuilder;
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

/// What the network kept. Only what was CONFIRMED is here: a put that was
/// emitted and not confirmed is exactly what a crash loses.
#[derive(Default, Clone)]
struct Network {
    blocks: MemBlocks,
    head: Option<(u64, Cid)>,
}

impl Network {
    /// A block whose put was confirmed. Packs are unpacked, because that is
    /// what the far side holds once it has one.
    fn confirm(&mut self, id: Cid, bytes: &[u8]) {
        for (mid, mbytes) in engine::pack::members(bytes) {
            self.blocks.insert(mid, &mbytes);
        }
        self.blocks.insert(id, bytes);
    }
}

/// Where a commit can be cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cut {
    AfterAccept,
    AfterSomePacks,
    AfterAllPacks,
    AfterHeadEmitted,
    AfterHeadConfirmed,
    MidParity,
}

const CUTS: [Cut; 6] = [
    Cut::AfterAccept,
    Cut::AfterSomePacks,
    Cut::AfterAllPacks,
    Cut::AfterHeadEmitted,
    Cut::AfterHeadConfirmed,
    Cut::MidParity,
];

fn boot(net: &Network, params: Params) -> Engine {
    let mut e = Engine::new(params);
    let out = e.step(Event::Start {
        key: KeySource::SecretStore,
        epochs: vec![Epoch(2), Epoch(1)],
    });
    // Answer whatever head reads it asks for.
    let mut queue = out;
    while let Some(f) = queue.pop() {
        if let Effect::ReadHead { epoch } = f {
            let ev = match (epoch, net.head) {
                // The head lives under the current epoch in this harness.
                (Epoch(2), Some((seq, root))) => Event::HeadRead {
                    epoch: Epoch(2),
                    seq,
                    root,
                },
                _ => Event::HeadMissing,
            };
            queue.extend(e.step(ev));
        }
    }
    e
}

/// Give a recovered engine the blocks the network kept, as a read would.
fn warm_from(e: &mut Engine, net: &Network) {
    for (id, bytes) in net.blocks.0.iter() {
        let _ = e.step(Event::BlockArrived {
            id: *id,
            bytes: bytes.clone(),
        });
    }
}

fn rebuild(records: &BTreeMap<Vec<u8>, Vec<u8>>) -> Cid {
    let mut t = TreeBuilder::new(|_, _: &[u8]| {});
    for (k, v) in records {
        t.push_bytes(k, v).unwrap();
    }
    t.finish().unwrap()
}

/// THE GATE.
#[test]
fn the_engine_survives_being_dropped_at_every_commit_boundary() {
    let mut cut_at: BTreeMap<Cut, usize> = BTreeMap::new();
    let mut checked_lost = 0usize;

    for (batch_n, cut) in CUTS.iter().enumerate() {
        // A scripted workload: three commits, cut in the third.
        let mut net = Network::default();
        let mut published: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut e = boot(&net, Params::default());

        // One clock for the whole workload. Restarting it per commit sends
        // time BACKWARDS, and the parity timer then never fires — which is
        // how the mid-parity boundary came to be unreachable.
        let mut clock = 0u64;
        let mut next_write = 0u64;
        let mut accepted_only: BTreeSet<WriteId> = BTreeSet::new();

        for commit in 0..3u32 {
            let cutting = commit == 2;
            let will_publish: Vec<(Vec<u8>, Vec<u8>)> = (0..40u32)
                .map(|i| {
                    let k = format!("k/{commit}/{i:03}").into_bytes();
                    let v = vec![(i % 251) as u8; if i.is_multiple_of(4) { 1400 } else { 30 }];
                    (k, v)
                })
                .collect();
            let ops: Vec<(Vec<u8>, Op)> = will_publish
                .iter()
                .map(|(k, v)| (k.clone(), Op::Put(v.clone())))
                .collect();

            next_write += 1;
            let wid = WriteId(next_write);
            let mut queue = e.step(Event::Write {
                client: ClientId(1),
                write_id: wid,
                ops,
            });
            accepted_only.insert(wid);

            if cutting && *cut == Cut::AfterAccept {
                *cut_at.entry(*cut).or_default() += 1;
                break;
            }

            // Drive the commit, cutting where the case says.
            let mut packs_confirmed = 0usize;
            let mut dropped = false;
            let mut guard = 0;
            while let Some(f) = queue.pop() {
                guard += 1;
                assert!(guard < 200_000, "commit {commit} did not settle");
                match f {
                    Effect::PutPack { id, bytes, .. } | Effect::PutBlock { id, bytes, .. } => {
                        net.confirm(id, &bytes);
                        let out = e.step(Event::PutConfirmed(id));
                        packs_confirmed += 1;
                        if cutting && *cut == Cut::AfterSomePacks && packs_confirmed == 1 {
                            dropped = true;
                            break;
                        }
                        // Every pack is in and the engine has asked for the
                        // head to move — but the shell never acted on it, so
                        // the network never heard. Cut BEFORE that effect is
                        // processed, or the cut lands after the head is
                        // already confirmed and tests a different moment than
                        // it claims to.
                        let asks_for_head =
                            out.iter().any(|f| matches!(f, Effect::UpdateHead { .. }));
                        queue.extend(out);
                        if cutting && *cut == Cut::AfterAllPacks && asks_for_head {
                            dropped = true;
                            break;
                        }
                    }
                    Effect::UpdateHead { seq, root, .. } => {
                        if cutting && *cut == Cut::AfterHeadEmitted {
                            // Emitted and NOT confirmed: the network never
                            // learned of it, so recovery must not see it.
                            dropped = true;
                            break;
                        }
                        net.head = Some((seq, root));
                        queue.extend(e.step(Event::HeadConfirmed(seq)));
                        if cutting && *cut == Cut::AfterHeadConfirmed {
                            published.extend(will_publish.iter().cloned());
                            accepted_only.remove(&wid);
                            dropped = true;
                            break;
                        }
                        // Parity goes out on a tick, once a group has
                        // settled, so reaching the mid-parity boundary means
                        // ticking here. Breaking out first is why the
                        // boundary counter reported five of six.
                        if cutting && *cut == Cut::MidParity {
                            published.extend(will_publish.iter().cloned());
                            accepted_only.remove(&wid);
                            let mut parity: Vec<(Cid, Vec<u8>)> = Vec::new();
                            for _ in 0..3 {
                                clock += 1;
                                for f in e.step(Event::Tick(clock)) {
                                    if let Effect::PutParity { id, bytes, .. } = f {
                                        parity.push((id, bytes));
                                    }
                                }
                            }
                            assert!(
                                parity.len() >= 2,
                                "{cut:?}: only {} parity block(s) were offered, so \
                                 there is no mid-point to cut at",
                                parity.len()
                            );
                            // Half of it lands; the rest is lost with the
                            // engine. Redundancy is not correctness: the tree
                            // must still read.
                            for (id, bytes) in parity.iter().take(parity.len() / 2) {
                                net.confirm(*id, bytes);
                                let _ = e.step(Event::PutConfirmed(*id));
                            }
                            dropped = true;
                            break;
                        }
                    }
                    Effect::PutParity { id, bytes, .. } => {
                        net.confirm(id, &bytes);
                        queue.extend(e.step(Event::PutConfirmed(id)));
                    }
                    Effect::Notify {
                        write_id,
                        state: State::Published,
                        ..
                    } => {
                        published.extend(will_publish.iter().cloned());
                        accepted_only.remove(&write_id);
                    }
                    _ => {}
                }
            }
            // Let parity settle for the commits that are not being cut.
            if !dropped {
                for _ in 0..4 {
                    clock += 1;
                    let out = e.step(Event::Tick(clock));
                    for f in out {
                        if let Effect::PutParity { id, bytes, .. } = f {
                            net.confirm(id, &bytes);
                            let _ = e.step(Event::PutConfirmed(id));
                        }
                    }
                }
            }
            if dropped {
                *cut_at.entry(*cut).or_default() += 1;
                break;
            }
        }

        // ---- the drop ----
        drop(e);
        let mut e = boot(&net, Params::default());
        warm_from(&mut e, &net);

        // (b) the recovered root is the root of exactly the published writes.
        assert_eq!(
            e.published_root(),
            rebuild(&published),
            "{cut:?}: the recovered tree is not the tree of exactly what was published"
        );

        // (c) the head names nothing missing.
        if let Some((_, root)) = net.head {
            let mut stack = vec![root];
            let mut seen = BTreeSet::new();
            while let Some(cid) = stack.pop() {
                if !seen.insert(cid) {
                    continue;
                }
                let bytes = net
                    .blocks
                    .get(&cid)
                    .unwrap_or_else(|| panic!("{cut:?}: the head names block {cid:?}, which the network does not have"))
                    .to_vec();
                if let Ok(n) = freenet_prolly::node::Node::parse(&bytes) {
                    if !n.is_leaf() {
                        for i in 0..n.len() {
                            stack.push(n.child(i).0);
                        }
                    }
                }
            }
        }

        // (a) every published write is readable in the recovered tree.
        for (k, v) in &published {
            assert_eq!(
                freenet_prolly::read::get(&net.blocks, &e.published_root(), k)
                    .expect("the recovered tree reads")
                    .map(|val| match val {
                        freenet_prolly::node::Value::Inline(b) => b.to_vec(),
                        freenet_prolly::node::Value::Ref { cid, .. } =>
                            net.blocks.get(&cid).expect("the value block").to_vec(),
                    })
                    .as_ref(),
                Some(v),
                "{cut:?}: a published write is not in the recovered tree"
            );
        }

        // (d) an accepted-only write is answered Lost, never silence.
        for wid in &accepted_only {
            let out = e.step(Event::AskWrite {
                client: ClientId(1),
                write_id: *wid,
            });
            assert!(
                out.iter().any(|f| matches!(
                    f,
                    Effect::Notify {
                        state: State::Lost,
                        ..
                    }
                )),
                "{cut:?}: write {wid:?} was accepted and never published, and the \
                 restarted engine did not report it Lost"
            );
            checked_lost += 1;
        }
        let _ = batch_n;
    }

    // The gate counts what it cut, and fails below a floor: a run that cut
    // nowhere would otherwise be the greenest of all.
    assert_eq!(
        cut_at.len(),
        CUTS.len(),
        "only {} of {} boundaries were actually cut: {cut_at:?}",
        cut_at.len(),
        CUTS.len()
    );
    assert!(
        checked_lost > 0,
        "no accepted-only write was ever checked for Lost"
    );
    println!(
        "  cut at all {} boundaries; {checked_lost} Lost replies checked",
        CUTS.len()
    );
}

/// The control for (c): a head written before its packs leaves a root that
/// names blocks nobody has.
///
/// This is why pack read-back strictly precedes the head update, and why the
/// two cannot be parallelised for latency. Without the control, "(c) holds"
/// would be a claim about a window the test never opened.
#[test]
fn a_head_written_before_its_packs_names_blocks_nobody_has() {
    let mut net = Network::default();
    let mut e = boot(
        &net,
        Params {
            head_before_packs: true,
            // Values get their own PUT, so the commit is many blocks rather
            // than one pack. With a single pack, confirming one put confirms
            // everything and the window this control exists to open is never
            // open — which is what the assertion below caught.
            max_packed_value: 512,
            ..Params::default()
        },
    );
    let ops: Vec<(Vec<u8>, Op)> = (0..40u32)
        .map(|i| {
            (
                format!("k/{i:03}").into_bytes(),
                Op::Put(vec![(i % 251) as u8; 1400]),
            )
        })
        .collect();
    let mut queue = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops,
    });

    // Confirm ONE put, then take whatever head the engine offers.
    let mut confirmed_one = false;
    let mut head = None;
    let mut guard = 0;
    while let Some(f) = queue.pop() {
        guard += 1;
        assert!(guard < 100_000, "the commit did not settle");
        match f {
            Effect::PutPack { id, bytes, .. } | Effect::PutBlock { id, bytes, .. } => {
                if !confirmed_one {
                    net.confirm(id, &bytes);
                    confirmed_one = true;
                    queue.extend(e.step(Event::PutConfirmed(id)));
                }
            }
            Effect::UpdateHead { seq, root, .. } => {
                head = Some((seq, root));
                break;
            }
            _ => {}
        }
    }
    let (_, root) = head.expect("the control must offer a head early, or it is no control");

    // The head names a root the network cannot walk.
    let mut stack = vec![root];
    let mut missing = 0;
    let mut seen = BTreeSet::new();
    while let Some(cid) = stack.pop() {
        if !seen.insert(cid) {
            continue;
        }
        match net.blocks.get(&cid) {
            None => missing += 1,
            Some(bytes) => {
                if let Ok(n) = freenet_prolly::node::Node::parse(bytes) {
                    if !n.is_leaf() {
                        for i in 0..n.len() {
                            stack.push(n.child(i).0);
                        }
                    }
                }
            }
        }
    }
    assert!(
        missing > 0,
        "the control's head named nothing missing, so (c) in the gate above is \
         asserted over a window this suite never opens"
    );
    println!("  control: a head written early names {missing} block(s) nobody has");
}

/// (e): a write never sits merely `accepted` for ever.
///
/// `accepted` survives a refresh and not a restart, so it is exposure. With
/// one commit in flight at a time, a write arriving behind a commit that
/// cannot publish would wait for ever — and the client would hear nothing,
/// which is the one outcome it cannot recover from. At T it is told `Failed`
/// and re-submits.
#[test]
fn a_write_never_sits_accepted_for_ever() {
    let t = 8u64;
    let run = |bound: bool| -> (usize, usize) {
        let net = Network::default();
        let mut e = boot(
            &net,
            Params {
                max_accept_age: t,
                bound_accept_age: bound,
                ..Params::default()
            },
        );
        // The first write opens a commit that is never confirmed: the network
        // is not answering.
        let _ = e.step(Event::Write {
            client: ClientId(1),
            write_id: WriteId(1),
            ops: vec![(b"a".to_vec(), Op::Put(vec![1u8; 40]))],
        });
        // Writes keep arriving behind it.
        let mut failed = 0;
        let mut accepted = 0;
        for n in 2..=12u64 {
            for f in e.step(Event::Write {
                client: ClientId(1),
                write_id: WriteId(n),
                ops: vec![(format!("k{n}").into_bytes(), Op::Put(vec![2u8; 40]))],
            }) {
                if let Effect::Notify { state, .. } = f {
                    match state {
                        State::Accepted => accepted += 1,
                        State::Failed => failed += 1,
                        _ => {}
                    }
                }
            }
            for f in e.step(Event::Tick(n * 2)) {
                if let Effect::Notify {
                    state: State::Failed,
                    ..
                } = f
                {
                    failed += 1;
                }
            }
        }
        (accepted, failed)
    };

    let (accepted, failed) = run(true);
    assert_eq!(accepted, 11, "every write should have been accepted first");
    assert!(
        failed > 0,
        "writes folded behind a stuck commit for {t}+ ticks and none was ever \
         reported Failed: the client is left waiting on a write nobody will \
         finish"
    );
    let (_, failed_without) = run(false);
    assert_eq!(
        failed_without, 0,
        "the control reported {failed_without} failures, so the bound is not \
         what produces them"
    );
    println!("  bounded: {failed} write(s) aged out; control: {failed_without}");
}

/// An engine's own head may sit under the PREVIOUS code epoch after an
/// upgrade, so recovery tries them newest first.
///
/// Rule 2. A new binary that looked only under its own epoch would decide its
/// device had never written and start an empty tree beside a real one — the
/// worst possible recovery, because it looks like a successful one.
#[test]
fn recovery_finds_a_head_left_under_the_previous_epoch() {
    let mut e = Engine::default();
    let out = e.step(Event::Start {
        key: KeySource::SecretStore,
        epochs: vec![Epoch(2), Epoch(1)],
    });
    assert_eq!(
        out.iter()
            .filter_map(|f| match f {
                Effect::ReadHead { epoch } => Some(*epoch),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![Epoch(2)],
        "recovery must ask for the newest epoch first"
    );

    // Nothing under the current epoch.
    let out = e.step(Event::HeadMissing);
    assert_eq!(
        out.iter()
            .filter_map(|f| match f {
                Effect::ReadHead { epoch } => Some(*epoch),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![Epoch(1)],
        "a missing head under the current epoch must fall back to the previous"
    );

    // The head is there, under the old epoch.
    let records: BTreeMap<Vec<u8>, Vec<u8>> = (0..50u32)
        .map(|i| (format!("k{i:03}").into_bytes(), vec![7u8; 30]))
        .collect();
    let root = rebuild(&records);
    let out = e.step(Event::HeadRead {
        epoch: Epoch(1),
        seq: 41,
        root,
    });
    assert!(
        out.is_empty(),
        "recovery walks nothing: reads warm it lazily"
    );
    assert_eq!(e.published_root(), root, "the recovered root is the head's");

    // And a device that has never written gets an empty tree, not a panic.
    let mut fresh = Engine::default();
    let _ = fresh.step(Event::Start {
        key: KeySource::Reissued,
        epochs: vec![Epoch(2)],
    });
    let out = fresh.step(Event::HeadMissing);
    assert!(out.is_empty(), "a brand-new device has nothing left to ask");
    assert_eq!(
        fresh.published_root(),
        rebuild(&BTreeMap::new()),
        "a device with no head starts from the empty tree"
    );
}

/// Two engines on one device key: the loser rebases, and never forks.
///
/// Rule 6. A restart racing the old process is the ordinary case, not an
/// exotic one — the Register refuses the lower or equal seq, and the engine
/// that loses must take the winner's head rather than publish a second
/// history under the same key. Its in-flight writes are reported `Lost`,
/// because their commit is not going to publish and the client is the only
/// thing that still has them.
#[test]
fn the_loser_of_a_head_conflict_rebases_and_never_forks() {
    let net = Network::default();
    let mut e = boot(&net, Params::default());
    let before = e.published_root();

    let _ = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops: vec![(b"mine".to_vec(), Op::Put(vec![1u8; 40]))],
    });
    let _ = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(2),
        ops: vec![(b"also-mine".to_vec(), Op::Put(vec![2u8; 40]))],
    });
    assert_ne!(e.root(), before, "the writes did not reach the warm tree");

    // The other engine got there first.
    let theirs: BTreeMap<Vec<u8>, Vec<u8>> = (0..30u32)
        .map(|i| (format!("theirs{i:02}").into_bytes(), vec![9u8; 30]))
        .collect();
    let winner = rebuild(&theirs);
    let out = e.step(Event::HeadConflict {
        seq: 99,
        root: winner,
    });

    assert_eq!(
        e.published_root(),
        winner,
        "the loser did not take the winner's head"
    );
    assert_eq!(
        e.root(),
        winner,
        "the loser kept its own tree alongside the winner's: that is a fork"
    );
    let lost: Vec<WriteId> = out
        .iter()
        .filter_map(|f| match f {
            Effect::Notify {
                write_id,
                state: State::Lost,
                ..
            } => Some(*write_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        lost,
        vec![WriteId(1), WriteId(2)],
        "the in-flight writes were not reported Lost, so a client would wait \
         for ever on a commit that will never publish"
    );
    println!("  conflict: loser adopts the winner's head, both writes reported Lost");
}
