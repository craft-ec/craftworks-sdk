//! The derivation is checked against what the library actually did, never
//! against a hand-written number.
//!
//! **One honesty note, because it decides what these tests prove.** The
//! derivation now RUNS the library's own descent (by design — a private copy of
//! "which child holds this key" drifts exactly at a boundary). So a test that
//! ran `read::get` and compared it to `expected(Op::Get)` would be comparing
//! the library with itself and would pass no matter what this module did. It is
//! not written.
//!
//! What is checked instead is everything that is NOT the descent:
//!
//! - zero fetches, AND non-zero derivations — an empty answer passes the first
//!   on its own;
//! - an unheld block makes the answer an UPPER bound that says so;
//! - the commit figure against `apply`'s own emitted set, which is a genuinely
//!   independent observation;
//! - that a wasteful access pattern reads well above 1 and names the ratio;
//! - that `rounds` separates operations that `bytes` cannot tell apart.

use craftworks_sdk::expected::{
    commit, expected, node_bytes_bound, write_estimate, Basis, Expected, Op, DEFAULT_BATCH,
    HEAD_UPDATE,
};
use freenet_prolly::apply::{apply, apply_into, Edit};
use freenet_prolly::build::init;
use freenet_prolly::read::get;
use freenet_prolly::store::{Blocks, MemBlocks};
use freenet_prolly::Cid;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Bound;

/// A store that remembers every block it handed out and every one it was asked
/// for and did not have.
///
/// The second number is the control. `Blocks::get` is synchronous and returns a
/// borrow, so a miss is exactly where a fetch WOULD have to happen. Asserting
/// it is zero asserts that no fetch was even possible.
struct Counting {
    inner: MemBlocks,
    handed: RefCell<HashMap<Cid, usize>>,
    misses: RefCell<usize>,
    calls: RefCell<usize>,
}

impl Counting {
    fn new(inner: MemBlocks) -> Counting {
        Counting {
            inner,
            handed: RefCell::new(HashMap::new()),
            misses: RefCell::new(0),
            calls: RefCell::new(0),
        }
    }

    fn reset(&self) {
        self.handed.borrow_mut().clear();
        *self.misses.borrow_mut() = 0;
        *self.calls.borrow_mut() = 0;
    }

    fn observed(&self) -> (u64, u64) {
        let h = self.handed.borrow();
        (h.len() as u64, h.values().map(|n| *n as u64).sum())
    }
}

impl Blocks for Counting {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        *self.calls.borrow_mut() += 1;
        match self.inner.get(cid) {
            Some(b) => {
                self.handed.borrow_mut().insert(*cid, b.len());
                Some(b)
            }
            None => {
                *self.misses.borrow_mut() += 1;
                None
            }
        }
    }
}

/// A tree with branches, and inline values so a lookup touches the path only.
fn tree(n: usize) -> (Counting, Cid, Vec<Vec<u8>>) {
    let mut blocks = MemBlocks::default();
    let mut root = init(&mut blocks);
    let keys: Vec<Vec<u8>> = (0..n).map(|i| format!("k{i:06}").into_bytes()).collect();
    for chunk in keys.chunks(64) {
        let edits: Vec<(Vec<u8>, Edit)> = chunk
            .iter()
            .map(|k| (k.clone(), Edit::Put(vec![b'v'; 200])))
            .collect();
        root = apply_into(&mut blocks, &root, &edits).unwrap().root;
    }
    (Counting::new(blocks), root, keys)
}

fn scan_all<'a>(lo: Bound<&'a [u8]>, hi: Bound<&'a [u8]>) -> Op<'a> {
    Op::Scan {
        lo,
        hi,
        batch: DEFAULT_BATCH,
    }
}

/// THE control, with the line that the architect added.
///
/// "Zero fetches" alone is passed by a derivation that silently returns
/// nothing — the standing failure mode of every gate that only checks for the
/// absence of something. So the non-zero half is asserted in the same test,
/// against every operation, and it is the half that fails first if a derivation
/// is quietly doing nothing.
#[test]
fn zero_fetches_and_non_zero_derivations() {
    let (store, root, keys) = tree(2000);
    let lo = keys[100].clone();
    let hi = keys[300].clone();

    let ops: Vec<(&str, Op)> = vec![
        ("get", Op::Get(&keys[913])),
        ("get_value", Op::GetValue(&keys[913])),
        ("proof", Op::Proof(&keys[913])),
        ("scan", scan_all(Bound::Included(&lo), Bound::Included(&hi))),
    ];

    for (name, op) in ops {
        store.reset();
        let e = expected(&store, &root, op);

        assert_eq!(
            *store.misses.borrow(),
            0,
            "{name}: the derivation asked for a block it did not hold. That is \
             where a fetch would go, and a derivation that fetches is a \
             measurement wearing a derivation's name."
        );
        assert!(
            *store.calls.borrow() > 0,
            "{name}: the derivation read nothing at all"
        );
        assert!(
            e.bytes > 0 && e.blocks > 0 && e.rounds > 0,
            "{name}: an EMPTY expectation passes a zero-fetch check too — \
             {e:?}"
        );
        assert_eq!(e.basis, Basis::Exact, "{name}: everything was held");
    }
}

#[test]
fn an_unheld_block_yields_an_upper_bound_that_says_so() {
    let (mut store, root, keys) = tree(2000);
    let key = keys[913].clone();

    let exact = expected(&store, &root, Op::Get(&key));
    assert_eq!(exact.basis, Basis::Exact);
    assert!(exact.bytes > 0);

    // Forget the node one level below the root, on this key's path.
    let victim = {
        let node = freenet_prolly::store::load(&store, &root).unwrap();
        let i = match node.search(&key) {
            Ok(i) => i,
            Err(0) => unreachable!("the key is in the tree"),
            Err(i) => i - 1,
        };
        node.child(i).0
    };
    store.inner.0.remove(&victim);

    let partial = expected(&store, &root, Op::Get(&key));
    assert_eq!(partial.basis, Basis::AtMost, "it must SAY it is a bound");
    assert!(
        partial.bytes >= exact.bytes,
        "an upper bound is never smaller than the truth ({} < {}). An \
         UNDER-stated expectation inflates every ratio computed from it, which \
         is the failure this rule exists to prevent.",
        partial.bytes,
        exact.bytes
    );
    assert_eq!(partial.phrase(8.0), "at least 8.0x");
    assert_eq!(exact.phrase(8.0), "8.0x");
}

#[test]
fn a_commit_costs_what_apply_actually_emitted() {
    let (store, root, keys) = tree(2000);

    // The write reports its own rewritten set, so there is nothing to
    // hand-write: this is an independent observation of the same quantity.
    let edits = vec![(keys[913].clone(), Edit::Put(vec![b'w'; 200]))];
    let mut emitted: Vec<(Cid, usize)> = Vec::new();
    let applied = apply(&store.inner, &root, &edits, |c, b| {
        emitted.push((c, b.len()));
    })
    .unwrap();
    assert!(!emitted.is_empty(), "a write must rewrite something");

    let e = commit(&emitted, &applied.parity);

    let node_bytes: u64 = emitted.iter().map(|(_, n)| *n as u64).sum();
    let parity_bytes: u64 = applied.parity.iter().map(|(_, b)| b.len() as u64).sum();
    assert_eq!(e.bytes, node_bytes + parity_bytes + HEAD_UPDATE);
    assert_eq!(
        e.blocks,
        emitted.len() as u64 + applied.parity.len() as u64 + 1,
        "the nodes it emitted, the parity it coded, and the head bump"
    );
    assert_eq!(e.basis, Basis::Exact, "the writer needs no second walk");
}

#[test]
fn a_reader_catching_up_has_an_expectation_at_all() {
    let (mut store, root, keys) = tree(2000);
    let edits = vec![(keys[913].clone(), Edit::Put(vec![b'w'; 200]))];
    let applied = apply_into(&mut store.inner, &root, &edits).unwrap();

    store.reset();
    let e = expected(
        &store,
        &applied.root,
        Op::ChangesSince {
            from: root,
            batch: DEFAULT_BATCH,
        },
    );

    assert_eq!(*store.misses.borrow(), 0, "both trees are warm");
    assert!(
        e.bytes > 0 && e.blocks > 0,
        "changes_since had no expectation at all before this: {e:?}"
    );
    assert_eq!(e.basis, Basis::Exact);
}

#[test]
fn a_proof_costs_about_what_locating_the_key_costs() {
    let (store, root, keys) = tree(2000);
    let key = &keys[913];

    let locate = expected(&store, &root, Op::Get(key));
    let proof = expected(&store, &root, Op::Proof(key));

    // A proof IS the root-to-leaf path, so it touches the same nodes.
    assert_eq!(proof.blocks, locate.blocks);
    assert_eq!(proof.bytes, locate.bytes);
}

#[test]
fn locating_a_large_value_does_not_pay_for_it() {
    let mut blocks = MemBlocks::default();
    let mut root = init(&mut blocks);
    let key = b"big".to_vec();
    // Over MAX_INLINE, so it lives in its own block behind a reference.
    let big = vec![b'x'; 200 * 1024];
    root = apply_into(&mut blocks, &root, &[(key.clone(), Edit::Put(big.clone()))])
        .unwrap()
        .root;
    let store = Counting::new(blocks);

    let locate = expected(&store, &root, Op::Get(&key));
    let with_value = expected(&store, &root, Op::GetValue(&key));

    assert!(
        locate.bytes < 16 * 1024,
        "locating should be a few KiB, not the value: {}",
        locate.bytes
    );
    assert_eq!(
        with_value.bytes,
        locate.bytes + big.len() as u64,
        "fetching the value adds exactly the referenced length, which the leaf \
         already records"
    );
    assert_eq!(with_value.blocks, locate.blocks + 1);
    assert!(with_value.rounds > locate.rounds);
}

#[test]
fn rounds_separate_what_bytes_cannot() {
    let (store, root, keys) = tree(2000);
    let lo = keys[400].clone();
    let hi = keys[600].clone();

    let one_scan = expected(
        &store,
        &root,
        Op::Scan {
            lo: Bound::Included(&lo),
            hi: Bound::Included(&hi),
            batch: DEFAULT_BATCH,
        },
    );
    // The same range, one block per round trip.
    let unbatched = expected(
        &store,
        &root,
        Op::Scan {
            lo: Bound::Included(&lo),
            hi: Bound::Included(&hi),
            batch: 1,
        },
    );

    assert_eq!(one_scan.bytes, unbatched.bytes, "the same bytes move");
    assert!(
        unbatched.rounds > one_scan.rounds,
        "per-operation cost and round trips dominate bytes here (F30): an \
         expectation without `rounds` calls these two the same"
    );
}

#[test]
fn a_wasteful_read_reads_well_above_one_and_names_the_ratio() {
    let (store, root, keys) = tree(2000);
    let lo = keys[400].clone();
    let hi = keys[600].clone();

    let want = expected(
        &store,
        &root,
        Op::Scan {
            lo: Bound::Included(&lo),
            hi: Bound::Included(&hi),
            batch: DEFAULT_BATCH,
        },
    );
    assert_eq!(want.basis, Basis::Exact);

    // The waste: one point read per key instead of one scan. Each read looks
    // cheap on its own, which is why this shape survives review.
    let mut actual_bytes = 0u64;
    let mut actual_rounds = 0u64;
    for key in &keys[400..=600] {
        store.reset();
        let _ = get(&store, &root, key).unwrap();
        let (b, by) = store.observed();
        actual_bytes += by;
        actual_rounds += b;
    }

    let ratio = actual_bytes as f64 / want.bytes as f64;
    assert!(
        ratio > 5.0,
        "201 point reads should cost well above one scan, got {ratio}"
    );
    assert_eq!(want.phrase(ratio), format!("{ratio:.1}x"));
    assert!(actual_rounds > want.rounds * 5);
    println!(
        "201 keys one at a time vs one scan: {} of the bytes, {} round trips against {}",
        want.phrase(ratio),
        actual_rounds,
        want.rounds
    );
}

#[test]
fn the_bound_from_a_parents_word_includes_the_parity_region() {
    // The aggregate is the fold of a node's ENTRIES; parity ids sit outside it
    // (`entries_end = len - pcount * REF_LEN`). Leaving them out understates
    // the expectation, which OVERSTATES the ratio — the one direction this
    // design exists to avoid.
    let without = node_bytes_bound(10, 4000, 0);
    let with = node_bytes_bound(10, 4000, 3);
    assert_eq!(with - without, 3 * 32);

    // And it never exceeds what a node can legally be.
    assert_eq!(node_bytes_bound(100_000, 10_000_000, 128), 16 * 1024);
}

#[test]
fn an_estimate_for_a_write_that_has_not_happened_is_always_a_bound() {
    let e = write_estimate(3);
    assert_eq!(
        e.basis,
        Basis::AtMost,
        "a distribution is not a reading: p99 is a bound and must say so"
    );
    assert!(e.blocks > 3);

    // The measured figure is a point in a parameter space, not a constant: a
    // taller tree must not silently reuse it.
    assert!(write_estimate(6).blocks > write_estimate(3).blocks);
}

#[test]
fn the_weaker_basis_wins_when_two_expectations_combine() {
    let exact = Expected {
        bytes: 10,
        blocks: 1,
        rounds: 1,
        basis: Basis::Exact,
    };
    let claimed = Expected {
        basis: Basis::Claimed,
        ..exact
    };
    let at_most = Expected {
        basis: Basis::AtMost,
        ..exact
    };

    assert_eq!(exact.and(claimed).basis, Basis::Claimed);
    assert_eq!(claimed.and(at_most).basis, Basis::AtMost);
    assert_eq!(exact.and(exact).basis, Basis::Exact);
    assert_eq!(exact.and(claimed).bytes, 20);
}
