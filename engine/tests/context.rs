//! What survives a `process()` call, and what must not.
//!
//! A delegate's memory is fresh every time — measured, not assumed: fifty
//! calls and the guest's own counter reads 1 every time, while the context's
//! reads 50. So the context is the whole of what the engine carries forward,
//! and it has 400 KiB to do it in.

use engine::{ClientId, Effect, Engine, Event, Op, Params, WriteId};
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;

/// A block source a test can fill, standing in for the node's own store.
#[derive(Default, Clone)]
struct Store(MemBlocks);

impl Blocks for Store {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        self.0.get(cid)
    }
}

impl Store {
    fn put(&mut self, id: Cid, bytes: &[u8]) {
        self.0.insert(id, bytes);
    }
    /// Absorb what a commit emitted, the way a node does once the puts land.
    fn absorb(&mut self, effects: &[Effect]) {
        for f in effects {
            match f {
                Effect::PutPack { id, bytes, .. } => {
                    for (mid, mbytes) in engine::pack::members(bytes) {
                        self.put(mid, &mbytes);
                    }
                    self.put(*id, bytes);
                }
                Effect::PutBlock { id, bytes, .. } | Effect::PutParity { id, bytes, .. } => {
                    self.put(*id, bytes)
                }
                _ => {}
            }
        }
    }
}

/// The empty leaf is the ONLY block the core can produce without its source,
/// and its bytes must hash to the id the format defines.
///
/// It is a pure function of the format, not a cache — but a constant that
/// drifts from the format is worse than no constant at all: it would name a
/// root nothing can produce, on a device that has never written. So the id is
/// checked against the library's own, every build.
#[test]
fn the_empty_leaf_is_the_only_block_the_core_knows_and_it_hashes_to_its_id() {
    let leaf = freenet_prolly::chunk::empty_leaf();
    assert_eq!(
        leaf.cid,
        freenet_prolly::block_id(freenet_prolly::kind::TREE_NODE, &leaf.bytes),
        "the empty leaf's bytes do not hash to its id: the constant has drifted \
         from the format"
    );

    // An engine with an EMPTY source can still read its own empty tree, and
    // gets "no such key" rather than "unavailable".
    let mut e = Engine::new(Params::default(), Store::default());
    let out = e.step(Event::Get {
        client: ClientId(1),
        req_id: engine::read::ReqId(1),
        key: b"anything".to_vec(),
    });
    let replied = out.iter().any(|f| {
        matches!(
            f,
            Effect::Reply {
                result: engine::read::ReadResult::Value(None),
                ..
            }
        )
    });
    assert!(
        replied,
        "a device with no head could not read its own empty tree: {out:?}"
    );
    // And it asked the network for nothing to do it.
    assert!(
        !out.iter().any(|f| matches!(f, Effect::FetchBlock { .. })),
        "reading an empty tree cost a fetch"
    );
}

/// The context round-trips, and refuses what it cannot read.
#[test]
fn a_context_round_trips_and_refuses_what_it_cannot_read() {
    let mut store = Store::default();
    // ABOUT the pack path: the property is that a pack's BYTES never ride in
    // the context. Phase 3 leaves packing off the write path, so this turns
    // it on to have a pack to look for at all.
    let packed = Params {
        pack_on_write: true,
        ..Params::default()
    };
    let mut e = Engine::new(packed, store.clone());
    let out = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        // Big enough that the pack is the dominant thing in the commit. With
        // a tiny write the bookkeeping is legitimately larger than the pack,
        // and "context < pack" would be the wrong property to assert.
        ops: (0..40u32)
            .map(|i| {
                (
                    format!("k/{i:03}").into_bytes(),
                    Op::Put(vec![(i % 251) as u8; 1400]),
                )
            })
            .collect(),
        reads: Vec::new(),
    });
    store.absorb(&out);

    let bytes = e.to_context().expect("a context");
    let root = e.root();
    let back =
        Engine::from_context(&bytes, packed, store.clone()).expect("its own context reads back");
    assert_eq!(back.root(), root, "the root did not survive the round trip");

    // Garbage, truncation and a wrong version are REFUSED, never a panic:
    // these bytes come from outside the call.
    for bad in [
        vec![],
        vec![0u8; 8],
        b"not a context at all".to_vec(),
        bytes[..bytes.len() / 2].to_vec(),
    ] {
        assert!(
            Engine::from_context(&bad, packed, store.clone()).is_err(),
            "a context of {} byte(s) was accepted",
            bad.len()
        );
    }

    // A context carries NO pack. That is the budget's whole premise.
    let packed: usize = out
        .iter()
        .filter_map(|f| match f {
            Effect::PutPack { bytes, .. } => Some(bytes.len()),
            _ => None,
        })
        .sum();
    assert!(
        packed > 16 * 1024,
        "the commit shipped only {packed} B of pack, so this proves nothing"
    );
    // The property is not "smaller" — it is that the pack is NOT IN THERE.
    // A context that carried it would blow the 400 KiB budget on one commit.
    let pack_bytes: Vec<Vec<u8>> = out
        .iter()
        .filter_map(|f| match f {
            Effect::PutPack { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
        .collect();
    for p in &pack_bytes {
        let probe = &p[..64.min(p.len())];
        assert!(
            !bytes.windows(probe.len()).any(|w| w == probe),
            "the context contains a pack's bytes"
        );
    }
    assert!(
        bytes.len() * 4 < packed,
        "the context ({} B) is the same order as the pack it must not carry \
         ({packed} B); bookkeeping should not scale with payload",
        bytes.len()
    );
    println!(
        "  context {} B for a commit whose pack is {packed} B",
        bytes.len()
    );
}

/// What the context actually costs, for the shapes that can grow.
///
/// Numbers, not adjectives: the platform gives a delegate 400 KiB of context
/// (`DelegateContext::MAX_SIZE` = 4096*10*10), and every cap in `Params` is a
/// slice of that. This prints the measured size of each shape so the caps can
/// be read against something, and asserts the ones that must stay small.
#[test]
fn the_context_costs_what_it_is_budgeted() {
    const PLATFORM: usize = 4096 * 10 * 10;
    let p = Params::default();

    // Idle: a started engine with a head and nothing in flight.
    let mut store = Store::default();
    let mut e = Engine::new(p, store.clone());
    let _ = e.step(Event::Start {
        key: engine::KeySource::SecretStore,
        epochs: vec![engine::Epoch(1)],
    });
    let idle = e.to_context().expect("idle").len();

    // One commit in flight over ~1 MiB of values. This is the shape that
    // grows with the SIZE of a write: the commit's bookkeeping names every
    // block it is waiting on, one Cid each.
    //
    // The values are over `max_packed_value`, so each is PUT on its own. A
    // first attempt used 4 KiB values, and the whole megabyte rode in ONE
    // pack -- one id in the bookkeeping, and a number that said nothing about
    // per-block cost. The `blocks > 10` floor below is what caught it.
    let mut e = Engine::new(p, store.clone());
    let ops: Vec<(Vec<u8>, Op)> = (0..16u32)
        .map(|i| {
            (
                format!("k/{i:05}").into_bytes(),
                Op::Put(vec![(i % 251) as u8; p.max_packed_value + 1]),
            )
        })
        .collect();
    let out = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops,
        reads: Vec::new(),
    });
    store.absorb(&out);
    let in_flight = e.to_context().expect("in flight").len();
    let blocks = out
        .iter()
        .filter(|f| {
            matches!(
                f,
                Effect::PutPack { .. } | Effect::PutBlock { .. } | Effect::PutParity { .. }
            )
        })
        .count();

    println!("  idle:                      {idle:6} B");
    println!("  ~1 MiB commit in flight:   {in_flight:6} B  ({blocks} blocks)");
    println!(
        "  ...per block:              {:6} B",
        (in_flight - idle) / blocks.max(1)
    );
    println!("  platform limit:            {PLATFORM:6} B");
    println!("  max_context_bytes:         {:6} B", p.max_context_bytes);

    assert!(
        idle < 1024,
        "an idle engine carries {idle} B of context; it holds a head and \
         nothing else"
    );
    assert!(
        p.max_context_bytes < PLATFORM,
        "max_context_bytes ({}) leaves no headroom under the platform's {PLATFORM} B",
        p.max_context_bytes
    );
    // The point of printing `blocks`: if a 1 MiB write produced two blocks,
    // the in-flight number would be small for a reason that says nothing
    // about the shape this is measuring.
    assert!(
        blocks > 10,
        "a 1 MiB write produced only {blocks} block(s), so this does not \
         measure a commit's per-block bookkeeping"
    );
    assert!(
        in_flight < p.max_context_bytes,
        "a 1 MiB commit's context is {in_flight} B, over the {} B budget",
        p.max_context_bytes
    );
}
/// Every shape at its cap still fits the budget, and a burst past a cap is
/// answered rather than allowed to destroy the context.
///
/// The measured costs: an idle engine is 125 B, a parked read about 131 B,
/// and a commit's bookkeeping about 60 B per block in flight. Unbounded, ten
/// thousand parked reads make a 1.31 MB context — over three times the
/// platform's 400 KiB — and `to_context` then fails, taking the IN-FLIGHT
/// COMMIT with it. A read burst must not be able to destroy a write.
///
/// The shapes are measured APART and summed, because they cannot be built
/// together: a commit needs a tree path it can read, and a parked read is one
/// that could not read the tree. Summing is the conservative direction. A
/// parked write and a pending commit are genuinely exclusive — a write is
/// refused while either is set — so the worst case is reads plus the LARGER
/// of those two, and the arithmetic below says so rather than assuming it.
#[test]
fn the_budget_holds_with_every_shape_at_its_cap() {
    let p = Params::default();
    let store = Store::default();
    let idle = {
        let mut e = Engine::new(p, store.clone());
        let _ = e.step(Event::Start {
            key: engine::KeySource::SecretStore,
            epochs: vec![engine::Epoch(1)],
        });
        e.to_context().expect("idle").len()
    };

    // --- reads at their cap, and a burst well past it ---
    let mut e = Engine::new(p, store.clone());
    let _ = e.step(Event::Start {
        key: engine::KeySource::SecretStore,
        epochs: vec![engine::Epoch(1)],
    });
    // A root the store does not hold, so every read parks rather than being
    // answered from the empty tree.
    let _ = e.step(Event::HeadRead {
        epoch: engine::Epoch(1),
        seq: 1,
        root: [7u8; 32],
    });
    let burst = p.max_parked_reads * 10;
    let mut refused = 0usize;
    for i in 0..burst {
        let out = e.step(Event::Get {
            client: ClientId(1),
            req_id: engine::read::ReqId(i as u64),
            key: format!("k/{i:09}").into_bytes(),
        });
        if out.iter().any(|f| {
            matches!(
                f,
                Effect::Reply {
                    result: engine::read::ReadResult::Unavailable(_),
                    ..
                }
            )
        }) {
            refused += 1;
        }
    }
    let reads = e.to_context().expect("the cap must keep it writable").len();
    assert_eq!(
        refused,
        burst - p.max_parked_reads,
        "{refused} of {burst} reads were refused; every read past the cap \
         owes the caller an answer, and none before it may be refused"
    );

    // --- a commit at the block cap, measured on a tree that can be read ---
    // Against the empty tree, so the apply succeeds and a commit really does
    // open. An earlier version measured this on the fake root above: the
    // apply stopped on a cold block, no commit opened, and "a commit fits
    // beside the reads" was asserted over a context with no commit in it.
    let mut e2 = Engine::new(p, store.clone());
    let _ = e2.step(Event::Start {
        key: engine::KeySource::SecretStore,
        epochs: vec![engine::Epoch(1)],
    });
    // Values over max_packed_value are PUT one each, so the block count is
    // the number of keys and the cap is reachable without 8 MiB of data.
    let n = 64u32;
    let ops: Vec<(Vec<u8>, Op)> = (0..n)
        .map(|i| {
            (
                format!("w/{i:05}").into_bytes(),
                Op::Put(vec![(i % 251) as u8; p.max_packed_value + 1]),
            )
        })
        .collect();
    let out = e2.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops,
        reads: Vec::new(),
    });
    let commit_blocks = out
        .iter()
        .filter(|f| matches!(f, Effect::PutPack { .. } | Effect::PutBlock { .. }))
        .count();
    assert!(
        commit_blocks >= n as usize,
        "the commit named {commit_blocks} block(s) for {n} oversized values, \
         so this does not measure per-block bookkeeping"
    );
    let commit = e2.to_context().expect("a commit in flight").len();
    let per_block = (commit - idle) / commit_blocks;
    // What the cap costs, at the per-block rate this just measured.
    let commit_worst = idle + per_block * p.max_commit_blocks;
    let write_worst = idle + p.max_parked_write_bytes;
    let worst = reads + commit_worst.max(write_worst);

    println!("  idle:                        {idle:6} B");
    println!(
        "  {:4} parked reads (the cap):  {reads:6} B",
        p.max_parked_reads
    );
    println!("  commit of {commit_blocks:3} blocks:        {commit:6} B  ({per_block} B/block)");
    println!(
        "  commit at its cap ({:4}):    {commit_worst:6} B",
        p.max_commit_blocks
    );
    println!("  parked write at its cap:     {write_worst:6} B");
    println!("  WORST CASE:                  {worst:6} B");
    println!("  budget:                      {:6} B", p.max_context_bytes);
    println!("  platform:                    {:6} B", 4096 * 10 * 10);

    assert!(
        worst < p.max_context_bytes,
        "every shape at its cap costs {worst} B, over the {} B budget: the \
         caps do not add up and a legal state cannot be written",
        p.max_context_bytes
    );
    // ...and the test is near the thing it claims to check. Without this,
    // caps of 1 would pass it.
    assert!(
        worst > p.max_context_bytes / 2,
        "the worst case is {worst} B against a {} B budget, so this asserts \
         almost nothing about the caps",
        p.max_context_bytes
    );
}

/// The commit block cap refuses the write, and leaves no trace.
#[test]
fn a_commit_over_the_block_cap_is_refused_and_a_smaller_one_is_not() {
    let cap = 8usize;
    let p = Params {
        max_commit_blocks: cap,
        ..Params::default()
    };
    let store = Store::default();
    let big = |n: u32| -> Vec<(Vec<u8>, Op)> {
        (0..n)
            .map(|i| {
                (
                    format!("w/{i:05}").into_bytes(),
                    Op::Put(vec![(i % 251) as u8; p.max_packed_value + 1]),
                )
            })
            .collect()
    };
    let run = |ops: Vec<(Vec<u8>, Op)>| -> (Vec<Effect>, Cid, Cid) {
        let mut e = Engine::new(p, store.clone());
        let _ = e.step(Event::Start {
            key: engine::KeySource::SecretStore,
            epochs: vec![engine::Epoch(1)],
        });
        let before = e.root();
        let out = e.step(Event::Write {
            client: ClientId(1),
            write_id: WriteId(1),
            ops,
            reads: Vec::new(),
        });
        let after = e.root();
        (out, before, after)
    };

    let (out, before, after) = run(big(cap as u32 * 4));
    // Refused as NEVER acceptable, with the count -- not `Busy`, which the
    // outbox re-sends for ever (craftworks-sdk#136).
    assert!(
        out.iter().any(|f| matches!(
            f,
            Effect::Notify {
                state: engine::State::TooLarge { bound: engine::WriteBound::CommitBlocks, limit, got },
                ..
            } if *limit == cap && *got > cap
        )),
        "a write naming more than {cap} blocks was not refused TooLarge"
    );
    assert_eq!(
        before, after,
        "the refused write changed the tree, so it was applied AND refused"
    );

    // The control: a write UNDER the cap, otherwise identical, is accepted.
    let (out, before, after) = run(big(2));
    assert!(
        out.iter().any(|f| matches!(
            f,
            Effect::Notify {
                state: engine::State::Accepted,
                ..
            }
        )),
        "a write under the same cap was refused too, so the cap is not what \
         refused the larger one"
    );
    assert_ne!(before, after, "the accepted write did not reach the tree");
    println!("  over the block cap: Busy, tree untouched; under it: Accepted");
}

/// The engine keeps nothing outside its context.
///
/// A delegate gets a fresh wasm instance, and a fresh linear memory, on every
/// `inbound_app_message`. A `static mut`, a `thread_local!`, a `lazy_static`
/// or a `OnceLock` in the engine would work perfectly in every test here — one
/// process, one memory — and silently hold nothing at all in production. No
/// test can observe that, because the test harness IS the single process the
/// defect needs; so this reads the source.
///
/// The source is EMBEDDED, not read from disk. Reading it needed
/// `CARGO_MANIFEST_DIR`, which `env!` bakes in at COMPILE time — and this
/// workspace shares one `CARGO_TARGET_DIR` across git worktrees, so a cached
/// test binary can carry a path belonging to a checkout that no longer
/// exists. This gate then failed with `NotFound` on a tree whose engine was
/// perfectly clean, which for a gate is the worst outcome: one that cries
/// wolf is one people learn to re-run and then to ignore. `include_str!`
/// resolves relative to THIS file at compile time and embeds the bytes, so
/// the gate reads the exact source its own binary was built from and touches
/// no filesystem at all.
///
/// Completeness is the other half, and it is checked against the directory
/// when the directory happens to be readable: a module added and not listed
/// here would otherwise go unscanned for ever.
#[test]
fn no_global_state_in_the_engine() {
    const SOURCES: [(&str, &str); 5] = [
        ("src/lib.rs", include_str!("../src/lib.rs")),
        ("src/asks.rs", include_str!("../src/asks.rs")),
        ("src/read.rs", include_str!("../src/read.rs")),
        ("src/pack.rs", include_str!("../src/pack.rs")),
        ("src/subs.rs", include_str!("../src/subs.rs")),
    ];
    let pattern = [
        "static ",
        "thread_local!",
        "lazy_static!",
        "OnceLock",
        "OnceCell",
    ];

    let mut lines = 0usize;
    let mut found: Vec<String> = Vec::new();
    for (name, text) in SOURCES {
        for (n, line) in text.lines().enumerate() {
            lines += 1;
            let code = line.trim_start();
            // `const` is fine: it is inlined, not stored.
            if code.starts_with("//") || code.starts_with("pub const") || code.starts_with("const")
            {
                continue;
            }
            if pattern.iter().any(|p| code.contains(p)) {
                found.push(format!("{name}:{}: {code}", n + 1));
            }
        }
    }

    assert!(
        lines > 100,
        "the scan read {lines} line(s) of engine source, so it checked nothing"
    );
    assert!(
        found.is_empty(),
        "the engine holds state outside its context, which a delegate's fresh \
         linear memory throws away on every call:\n{}",
        found.join("\n")
    );

    // The pattern really matches: this file's own store has a deliberate,
    // test-only `thread_local!`, and the same scan finds it.
    let support = include_str!("common/mod.rs");
    let hits = support
        .lines()
        .filter(|l| pattern.iter().any(|p| l.trim_start().contains(p)))
        .count();
    assert!(
        hits > 0,
        "the scan found no global in tests/common/mod.rs either, which HAS \
         one on purpose — so the pattern matches nothing and the clean result \
         above means nothing"
    );

    // Completeness, when the directory can be read. Not a skip that passes:
    // if the directory IS there and holds a module this test does not list,
    // that module is unscanned and this fails.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            let on_disk: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".rs"))
                .collect();
            let listed: Vec<&str> = SOURCES
                .iter()
                .map(|(n, _)| n.trim_start_matches("src/"))
                .collect();
            for f in &on_disk {
                assert!(
                    listed.contains(&f.as_str()),
                    "engine/src/{f} exists and this gate does not scan it: add \
                     it to SOURCES"
                );
            }
            println!(
                "  {} embedded file(s), {lines} lines, no globals (pattern \
                 verified on {hits} test-only hit(s); {} file(s) on disk, all \
                 listed)",
                SOURCES.len(),
                on_disk.len()
            );
        }
        Err(e) => println!(
            "  {} embedded file(s), {lines} lines, no globals (pattern \
             verified on {hits} test-only hit(s); the src directory was not \
             readable -- {e} -- so completeness was not re-checked, but every \
             listed file WAS scanned)",
            SOURCES.len()
        ),
    }
}

/// Owed parity survives a rehydration, and is actually PUT.
///
/// The context carries owed groups as ids with no bytes, because parity is a
/// pure function of its members and the bytes can be recomputed from the
/// node's blocks. "Can be" is the claim this checks.
#[test]
fn owed_parity_survives_a_rehydration_and_is_still_put() {
    let p = Params {
        coalesce_parity: true,
        ..Params::default()
    };
    let mut store = Store::default();
    let mut e = Engine::new(p, store.clone());

    // Values by reference, so leaves carry parity over them.
    let ops: Vec<(Vec<u8>, Op)> = (0..64u32)
        .map(|i| {
            (
                format!("k/{i:05}").into_bytes(),
                Op::Put(vec![(i % 251) as u8; 1400]),
            )
        })
        .collect();
    let mut queue = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops,
        reads: Vec::new(),
    });
    store.absorb(&queue);
    let mut live: Vec<Effect> = Vec::new();
    let mut guard = 0;
    while let Some(f) = queue.pop() {
        guard += 1;
        assert!(guard < 100_000, "the commit did not settle");
        let out = match &f {
            Effect::PutPack { id, .. } | Effect::PutBlock { id, .. } => {
                e.step(Event::PutConfirmed(*id))
            }
            Effect::UpdateHead { seq, .. } => e.step(Event::HeadConfirmed(*seq)),
            _ => Vec::new(),
        };
        store.absorb(&out);
        live.extend(out.clone());
        queue.extend(out);
    }
    let owed = e.owed_groups();
    assert!(
        owed > 0,
        "the commit left no parity owed, so there is nothing for a \
         rehydration to carry"
    );
    let _ = &live;

    // Round-trip the context FIRST, so both engines start from the same owed
    // set, then drive the live one to get the oracle.
    let ctx = e.to_context().expect("a context");
    let mut live_parity: Vec<Cid> = Vec::new();
    for t in 1..=(p.parity_age * 3) {
        for f in e.step(Event::Tick(t)) {
            if let Effect::PutParity { id, .. } = f {
                live_parity.push(id);
            }
        }
    }
    live_parity.sort();
    // Nothing here confirms a parity put, so each is re-asked every
    // `reask_after` ticks (sdk#150: an ask is not a fact). The oracle is the
    // DISTINCT ids; the full lists, re-asks and all, are compared below too.
    let distinct = |v: &[Cid]| v.iter().collect::<std::collections::BTreeSet<_>>().len();
    // The oracle must exist. Guarded behind an `if !live_parity.is_empty()`,
    // the comparison below would be skipped silently whenever the live engine
    // happened to put nothing — which is the case it most needs to catch.
    assert_eq!(
        distinct(&live_parity),
        owed * 3,
        "the live engine put {} distinct parity block(s) for {owed} owed group(s), so \
         there is no oracle to compare the rehydrated one against",
        distinct(&live_parity)
    );

    // The same context, in an engine that never saw the commit.
    let mut e2 = Engine::from_context(&ctx, p, store.clone()).expect("its own context");
    assert_eq!(
        e2.owed_groups(),
        owed,
        "the rehydrated engine does not owe the same groups"
    );

    // Drive it past the parity age, the way a live engine flushes.
    let mut put: Vec<Cid> = Vec::new();
    for t in 1..=(p.parity_age * 3) {
        for f in e2.step(Event::Tick(t)) {
            if let Effect::PutParity { id, .. } = f {
                put.push(id);
            }
        }
    }
    assert!(
        !put.is_empty(),
        "a rehydrated engine owing {owed} parity group(s) put NONE of them: \
         the bytes are not in the context and nothing recomputes them, so the \
         groups are marked sent and the redundancy is silently lost"
    );
    put.sort();
    assert_eq!(
        distinct(&put),
        owed * 3,
        "{owed} group(s) owed, {} distinct parity block(s) put: a group is three \
         blocks, so this is not one per group",
        distinct(&put)
    );
    // The recomputed blocks are the SAME blocks, by id. Parity is a pure
    // function of its members, and this is the assertion that says so rather
    // than the comment.
    assert_eq!(
        put, live_parity,
        "the rehydrated engine put different parity from the live one for \
         the same groups"
    );
    println!(
        "  {owed} group(s) owed across a rehydration, {} parity block(s) put, \
         ids identical to the live engine's",
        put.len()
    );
}

/// A damaged context is REFUSED, not decoded into a plausible engine.
///
/// The context comes back from outside: a node's cache, as bytes, with no
/// guarantee beyond their length. A version check plus `bincode::deserialize`
/// is not enough, and this is the measurement that says so — core dev's probe
/// overwrote every 8-byte window of a valid context with a large integer and
/// **656 of 876 damaged contexts were ACCEPTED**. No panic and no runaway
/// allocation, which is why nothing else caught it: the engine came back in
/// whatever state the damage described, and since the context carries the
/// `(seq, root)` of the commit in flight, it could then emit `UpdateHead`
/// naming a root nobody has.
///
/// Refusal is free in this design — an engine with no context starts from its
/// head and reports its in-flight writes `Lost` — so the bar is exact: accept
/// only what this build wrote, byte for byte.
#[test]
fn a_damaged_context_is_refused_without_panicking_or_allocating_the_world() {
    let store = Store::default();
    let mut e: Engine<Store> = Engine::new(Params::default(), store.clone());
    let ops: Vec<(Vec<u8>, Op)> = (0..200)
        .map(|i| (format!("k{i:04}").into_bytes(), Op::Put(vec![7u8; 100])))
        .collect();
    let _ = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops,
        reads: Vec::new(),
    });
    let good = e.to_context().expect("context");

    let (mut refused, mut accepted) = (0usize, 0usize);
    for at in 0..good.len().saturating_sub(8) {
        for fill in [u64::MAX, 3_000_000_000u64] {
            let mut bad = good.clone();
            bad[at..at + 8].copy_from_slice(&fill.to_le_bytes());
            match Engine::from_context(&bad, Params::default(), store.clone()) {
                Ok(_) => accepted += 1,
                Err(_) => refused += 1,
            }
        }
    }
    // Truncations and the empty context: neither may panic.
    let mut short = 0usize;
    for cut in 0..good.len() {
        if Engine::from_context(&good[..cut], Params::default(), store.clone()).is_err() {
            short += 1;
        }
    }
    assert!(
        Engine::from_context(&[], Params::default(), store.clone()).is_err(),
        "an empty context was accepted"
    );

    println!("  {} B context: {refused} damaged refused, {accepted} accepted, {short} truncations refused", good.len());
    assert_eq!(
        accepted, 0,
        "{accepted} damaged context(s) were accepted and re-hydrated an \
         engine in whatever state the damage described"
    );
    assert_eq!(
        short,
        good.len(),
        "a truncated context was accepted: every prefix of a valid context is \
         a context this build did not write"
    );
    // The probe must have RUN. Without this, `accepted == 0` is also what a
    // zero-length context would report.
    assert!(
        refused > 800,
        "only {refused} damaged context(s) were tried, so this is not the \
         sweep the number above claims"
    );
    // ...and the undamaged one still round-trips, or the refusal is just a
    // decoder that says no to everything.
    assert!(
        Engine::from_context(&good, Params::default(), store.clone()).is_ok(),
        "the UNDAMAGED context was refused too"
    );
}

/// A refused context costs a restart, not correctness.
///
/// This is the other half of refusing: it is only free if what follows is
/// right. The engine starts from `Start`, re-reads its head, and gives a
/// client asking about the write that was in flight NO verdict: a fresh
/// engine cannot tell a write that died from one whose head landed before the
/// loss, so `Lost` would be a guess (WRITE-PATH.md session table; sdk#196).
/// The client's own timeout hands the write back.
#[test]
fn a_refused_context_recovers_from_the_head_and_gives_the_unknown_write_no_verdict() {
    let mut store = Store::default();
    let mut e: Engine<Store> = Engine::new(Params::default(), store.clone());
    let out = e.step(Event::Write {
        client: ClientId(1),
        write_id: WriteId(1),
        ops: vec![(b"k".to_vec(), Op::Put(vec![5u8; 40]))],
        reads: Vec::new(),
    });
    store.absorb(&out);
    let good = e.to_context().expect("context");
    let published = e.published_root();

    // One byte of the body, flipped. Nothing else about it is wrong.
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert!(
        Engine::from_context(&bad, Params::default(), store.clone()).is_err(),
        "a one-bit change to the body was accepted"
    );

    // So the delegate starts fresh, as it must.
    let mut e2: Engine<Store> = Engine::new(Params::default(), store.clone());
    let out = e2.step(Event::Start {
        key: engine::KeySource::SecretStore,
        epochs: vec![engine::Epoch(1)],
    });
    assert!(
        out.iter().any(|f| matches!(f, Effect::ReadHead { .. })),
        "a fresh engine did not re-read its head, so a refused context loses \
         the tree as well as the bookkeeping"
    );
    let _ = e2.step(Event::HeadRead {
        epoch: engine::Epoch(1),
        seq: 1,
        root: published,
    });
    assert_eq!(
        e2.published_root(),
        published,
        "the recovered engine did not take the head it was given"
    );

    let out = e2.step(Event::AskWrite {
        client: ClientId(1),
        write_id: WriteId(1),
    });
    assert_eq!(
        out.iter()
            .filter_map(|f| match f {
                Effect::Notify { state, .. } => Some(*state),
                _ => None,
            })
            .collect::<Vec<_>>(),
        Vec::<engine::State>::new(),
        "a client asking about a write this fresh engine has no record of was \
         told something. Not knowing is not evidence (sdk#196 review): the \
         write may be one that published and whose Published was lost, so \
         `Lost` would roll back a write that is in the tree. No verdict; the \
         client's own timeout decides"
    );
    println!(
        "  refused context: fresh start, head re-read, the unknown write answered with no verdict"
    );
}
