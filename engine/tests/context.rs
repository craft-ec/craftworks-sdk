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
    let mut e = Engine::new(Params::default(), store.clone());
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
    });
    store.absorb(&out);

    let bytes = e.to_context().expect("a context");
    let root = e.root();
    let back = Engine::from_context(&bytes, Params::default(), store.clone())
        .expect("its own context reads back");
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
            Engine::from_context(&bad, Params::default(), store.clone()).is_err(),
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
