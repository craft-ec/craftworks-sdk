//! Owed parity over a LARGE tree, and a zero that can be believed
//! (craftworks-sdk#181, and #119's first half).
//!
//! #181: `recompute_owed` restarted at the root on every call and gave up
//! after `max_parity_scan_blocks` nodes of a rightmost-first walk, so a group
//! listed deep on the LEFT of a big tree — an OLD record, since ids sort by
//! creation time — was never recovered, never put, and its writer never told
//! `ParityComplete`. Executed on 12,000 keys: the leftmost edit ended at
//! `Published`. The walk now RESUMES where it stopped.
//!
//! #119: `owed_groups() == 0` meant both "every group is put" and "this engine
//! never looked". `parity_scan()` says which.

use engine::{ClientId, Effect, Engine, Epoch, Event, KeySource, Op, ParityScan, Params, State, WriteId};
use freenet_prolly::store::MemBlocks;
use freenet_prolly::Cid;
use std::collections::BTreeMap;

mod common;
use common::Store;

/// What the network kept: every confirmed put, and the head.
#[derive(Default)]
struct Network {
    blocks: MemBlocks,
    head: Option<(u64, Cid)>,
}

fn boot(net: &Network, params: Params) -> Engine<Store> {
    let mut e = Engine::new(params, Store::default());
    let mut queue = stepped!(
        e,
        Event::Start {
            key: KeySource::SecretStore,
            epochs: vec![Epoch(1)],
        }
    );
    while let Some(f) = queue.pop() {
        if let Effect::ReadHead { .. } = f {
            let ev = match net.head {
                Some((seq, root)) => Event::HeadRead { epoch: Epoch(1), seq, root },
                None => Event::HeadMissing,
            };
            queue.extend(stepped!(e, ev));
        }
    }
    e
}

/// Answer every effect as a node that holds everything would, and record
/// every state each write is told.
fn drive(e: &mut Engine<Store>, net: &mut Network, first: Vec<Effect>, told: &mut BTreeMap<u64, Vec<State>>) {
    drive_with(e, net, first, told, false)
}

/// `withhold_parity`: parity puts are dropped unanswered, as a page that
/// closed before they went out would leave them.
fn drive_with(e: &mut Engine<Store>, net: &mut Network, first: Vec<Effect>, told: &mut BTreeMap<u64, Vec<State>>, withhold_parity: bool) {
    let mut queue = first;
    let mut guard = 0usize;
    while let Some(f) = queue.pop() {
        guard += 1;
        assert!(guard < 5_000_000, "the engine never settled");
        match f {
            Effect::PutParity { .. } if withhold_parity => {}
            Effect::PutPack { id, bytes, .. } | Effect::PutBlock { id, bytes, .. } | Effect::PutParity { id, bytes, .. } => {
                for (mid, mbytes) in engine::pack::members(&bytes) {
                    net.blocks.insert(mid, &mbytes);
                }
                net.blocks.insert(id, &bytes);
                queue.extend(stepped!(e, Event::PutConfirmed(id)));
            }
            Effect::UpdateHead { seq, root, .. } => {
                net.head = Some((seq, root));
                queue.extend(stepped!(e, Event::HeadConfirmed(seq)));
            }
            Effect::FetchBlock { id, .. } => {
                if let Some(bytes) = freenet_prolly::store::Blocks::get(&net.blocks, &id) {
                    let bytes = bytes.to_vec();
                    queue.extend(stepped!(e, Event::BlockArrived { id, bytes }));
                }
            }
            Effect::Notify { write_id, state, .. } => told.entry(write_id.0).or_default().push(state),
            _ => {}
        }
    }
}

fn write(e: &mut Engine<Store>, net: &mut Network, id: u64, ops: Vec<(Vec<u8>, Op)>, told: &mut BTreeMap<u64, Vec<State>>) {
    let out = stepped!(e, Event::forced_write(ClientId(1), WriteId(id), ops));
    drive(e, net, out, told);
}

fn ticks(e: &mut Engine<Store>, net: &mut Network, from: u64, n: u64, told: &mut BTreeMap<u64, Vec<State>>) {
    ticks_with(e, net, from, n, told, false)
}

fn ticks_with(e: &mut Engine<Store>, net: &mut Network, from: u64, n: u64, told: &mut BTreeMap<u64, Vec<State>>, withhold_parity: bool) {
    for t in from..from + n {
        let out = stepped!(e, Event::Tick(t));
        drive_with(e, net, out, told, withhold_parity);
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("d/notes/{i:06}").into_bytes()
}

/// **#181: editing the OLDEST record of a big tree gets its redundancy.**
/// The edit's group is listed far down the LEFT of the tree — beyond the first
/// `max_parity_scan_blocks` nodes of the walk — and it is reached because the
/// walk resumes. Control: the RIGHTMOST edit, which the old walk reached too.
#[test]
fn editing_the_leftmost_key_of_a_big_tree_reaches_parity_complete() {
    let params = Params { coalesce_parity: true, ..Params::default() };
    let mut net = Network::default();
    let mut e = boot(&net, params);
    let mut told = BTreeMap::new();
    // Inline values of 1,000 B make LEAVES, which is what the walk counts: a
    // tree of many small values fits in a few hundred nodes, under the cap.
    let n: u32 = 24_000;
    let mut id = 1u64;
    for chunk in (0..n).collect::<Vec<_>>().chunks(100) {
        let ops = chunk.iter().map(|i| (key(*i), Op::Put(vec![(*i % 251) as u8; 1_000]))).collect();
        write(&mut e, &mut net, id, ops, &mut told);
        id += 1;
    }
    ticks(&mut e, &mut net, 1, 200, &mut told);
    let blocks = net.blocks.0.len();
    assert!(
        blocks > params.max_parity_scan_blocks * 4,
        "a tree of {blocks} blocks does not exceed the scan cap ({}) enough to test resuming \
         (the first write was told {:?})",
        params.max_parity_scan_blocks,
        told.get(&1)
    );
    // THE STATE THE WALK IS FOR: an engine that owes a group by its IDS and
    // holds none of its bytes. An engine that coded the parity keeps the bytes
    // in memory and never walks; one that learnt the group some other way —
    // a rehydrated context today, sdk#119's seeding from the head next — must
    // find the node that lists it. So: commit the edit with its parity
    // withheld, rehydrate ONCE (the context carries owed groups as ids only),
    // and keep THAT engine, ticking, as a page keeps its engine.
    let mut t = 1_000u64;
    for (label, k) in [("RIGHTMOST", n - 1), ("LEFTMOST", 0)] {
        let w = id;
        id += 1;
        let out = stepped!(e, Event::forced_write(ClientId(1), WriteId(w), vec![(key(k), Op::Put(b"edited".to_vec()))]));
        drive_with(&mut e, &mut net, out, &mut told, true);
        ticks_with(&mut e, &mut net, t, 5, &mut told, true);
        assert!(e.owed_groups() > 0, "{label}: the edit owes no parity, so nothing here needs recovering");
        let ctx = e.to_context().expect("a context");
        e = Engine::from_context(&ctx, params, Store::default()).expect("its own context");
        ticks(&mut e, &mut net, t + 100, 120, &mut told);
        t += 1_000;
        let states = told.get(&w).cloned().unwrap_or_default();
        println!("  {blocks} blocks | edit {label} key: {states:?}");
        assert!(
            states.contains(&State::ParityComplete),
            "editing the {label} key of a {n}-key tree ({blocks} blocks) was told {states:?}: \
             its group was never recovered, so its redundancy was never put"
        );
    }
}

/// **#119: a zero that can be believed.** An engine that tracked the tree from
/// empty says `Done`; a second engine booting on that non-empty head knows
/// nothing about its parity and says `NotScanned` — while `owed_groups()` is 0
/// in BOTH, which is exactly why the count alone must never be reported.
#[test]
fn owed_zero_is_done_only_for_an_engine_that_tracked_the_tree() {
    let params = Params { coalesce_parity: true, ..Params::default() };
    let mut net = Network::default();
    let mut e = boot(&net, params);
    let mut told = BTreeMap::new();
    assert!(matches!(e.parity_scan(), ParityScan::Done { .. }), "an empty tree is known in full: {:?}", e.parity_scan());
    let ops = (0..300u32).map(|i| (key(i), Op::Put(vec![7u8; 1400]))).collect();
    write(&mut e, &mut net, 1, ops, &mut told);
    ticks(&mut e, &mut net, 1, 200, &mut told);
    let root = net.head.expect("a head").1;
    assert_eq!(e.parity_scan(), &ParityScan::Done { root }, "its own commits keep it Done, at the new root");
    assert_eq!(e.owed_groups(), 0, "the control: this engine's zero is 'all put'");

    let fresh = boot(&net, params);
    assert_eq!(fresh.owed_groups(), 0);
    assert_eq!(
        fresh.parity_scan(),
        &ParityScan::NotScanned,
        "a fresh engine on a non-empty head reports 0 owed without having looked — it must say NotScanned"
    );
}
