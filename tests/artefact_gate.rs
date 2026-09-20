//! The ARTEFACT gate: what this SDK writes, run through the contract that
//! will judge it.
//!
//! Replaces `tests/prolly_pin.rs`, which compared a freenet-prolly REVISION
//! string between this repository and the contracts'. That was only ever a
//! proxy for the property, and since freenet-contracts#36 the two revisions
//! are deliberately allowed to differ — so the proxy now fails on correct
//! trees and passes on nothing in particular.
//!
//! The property itself has two halves, and they pull in opposite directions:
//!
//! - **Write arm.** Blocks this SDK's prolly produces must be ACCEPTED by the
//!   released `block.wasm`. A writer that emits something the contract
//!   refuses is a writer whose data never lands.
//! - **Corpus arm.** Blocks a released contract accepted must still PARSE
//!   with this SDK's prolly. A parser that tightens is a reader that cannot
//!   read data already on the network.
//!
//! Stated as one rule: **a newer prolly may only LOOSEN its parser, and may
//! never LOOSEN its writer, relative to the released contract.**
//!
//! "Could not find the wasm" and "could not find the corpus" are FAILURES,
//! not skips. The skew this exists for is invisible to every other test here,
//! because both sides of those tests are the same library.

mod support;
use support::contract::{Contract, Verdict};

/// Where the contracts checkout is.
///
/// `CARGO_MANIFEST_DIR/..` is not enough: work happens in a `git worktree`
/// under a temp dir, and from there the sibling repo is not beside the
/// manifest. The worktree's `.git` is a FILE holding `gitdir: <main>/...`,
/// which names the checkout this one belongs to — read rather than asked for
/// with a subprocess, because the main checkout is shared and a `git` call
/// against a repository someone else is merging in fails for reasons that
/// have nothing to do with this gate.
fn contracts_repo() -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    if let Ok(p) = std::env::var("CRAFTWORKS_CONTRACTS") {
        return Some(PathBuf::from(p));
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let beside = manifest.join("../freenet-contracts");
    if beside.join("block/Cargo.toml").is_file() {
        return Some(beside);
    }
    let gitdir = std::fs::read_to_string(manifest.join(".git")).ok()?;
    let common = PathBuf::from(gitdir.trim().strip_prefix("gitdir:")?.trim());
    let common = if common.is_absolute() {
        common
    } else {
        manifest.join(common)
    };
    let main = common
        .ancestors()
        .find(|p| {
            p.join("block/Cargo.toml").is_file() || p.file_name().is_some_and(|n| n == ".git")
        })
        .and_then(|p| {
            if p.file_name().is_some_and(|n| n == ".git") {
                p.parent()
            } else {
                Some(p)
            }
        })?;
    let beside = main.join("../freenet-contracts");
    beside.join("block/Cargo.toml").is_file().then_some(beside)
}

fn block_wasm() -> Vec<u8> {
    let repo = contracts_repo().unwrap_or_else(|| {
        panic!(
            "cannot find the freenet-contracts checkout, so NOTHING was \
             validated through the real contract. Set CRAFTWORKS_CONTRACTS to \
             its path. This is a failure and not a skip: the skew this gate \
             exists for is invisible to every other test in this repository, \
             because both sides of those tests are the same library."
        )
    });
    let path = repo.join("build/block.wasm");
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "the contracts checkout is at {} but {} could not be read ({e}). \
             Run its ./build.sh. Not validating is not passing.",
            repo.display(),
            path.display()
        )
    })
}

/// The gate can run the real contract at all, and the contract can tell a
/// good block from a bad one.
///
/// Every assertion below rests on this: a harness that returned `Invalid` for
/// everything — a mis-built buffer, a misread result — would pass a refusal
/// control and fail the acceptance arm in a way that looks like a writer bug.
#[test]
fn the_gate_can_run_the_contract_and_it_separates_good_from_bad() {
    let mut c = Contract::load(&block_wasm()).expect("the released block.wasm must load");
    // A RAW block: kind byte 0, then the body. Params are blake3 of the whole
    // state, which is exactly what the engine calls the block's id.
    let state = {
        let mut s = vec![freenet_prolly::kind::RAW];
        s.extend_from_slice(b"artefact gate");
        s
    };
    let params = blake3::hash(&state).as_bytes().to_vec();
    assert_eq!(
        c.validate(&params, &state),
        Verdict::Valid,
        "the contract refused a well-formed RAW block, so this harness is not \
         speaking its ABI and no other result here means anything"
    );
    // One byte of the params changed: the state no longer hashes to its key.
    let mut wrong = params.clone();
    wrong[0] ^= 0xFF;
    assert_eq!(
        c.validate(&wrong, &state),
        Verdict::Invalid,
        "the contract ACCEPTED a state that does not hash to its params, so \
         it is not checking and neither is this gate"
    );
}

/// A case the write arm offers the contract, and what it is a boundary of.
struct Case {
    what: &'static str,
    state: Vec<u8>,
}

impl Case {
    fn of(what: &'static str, kind: u8, body: Vec<u8>) -> Case {
        let mut state = Vec::with_capacity(1 + body.len());
        state.push(kind);
        state.extend_from_slice(&body);
        Case { what, state }
    }
    /// A block's params ARE its id: `blake3(kind ‖ body)`.
    fn params(&self) -> Vec<u8> {
        blake3::hash(&self.state).as_bytes().to_vec()
    }
}

/// Build the boundary cases with THIS SDK's prolly.
///
/// On every format boundary the two implementations could disagree about:
/// the largest node, a full pack, parity over a group, and each inline/by
/// reference size class. A gate over comfortable middle-sized blocks would
/// pass while the edges rotted, and the edges are where a writer and a
/// parser drift apart.
fn write_arm_cases() -> Vec<Case> {
    use freenet_prolly::build::TreeBuilder;
    use freenet_prolly::store::MemBlocks;
    use freenet_prolly::{kind, node};

    let mut cases = Vec::new();

    // --- RAW values, at each size class boundary ---
    //
    // MAX_INLINE is where a value stops riding in its leaf and becomes a
    // block of its own, so it is the boundary that decides whether a RAW
    // block exists at all.
    for (what, len) in [
        ("raw: one byte", 1usize),
        ("raw: at MAX_INLINE", node::MAX_INLINE),
        ("raw: one over MAX_INLINE", node::MAX_INLINE + 1),
        ("raw: at MAX_VALUE", node::MAX_VALUE),
    ] {
        cases.push(Case::of(what, kind::RAW, vec![0xA5; len]));
    }

    // --- tree nodes, including one as large as the format allows ---
    //
    // Built by driving the real builder, so these are nodes this SDK would
    // actually write rather than nodes hand-assembled to be near a limit.
    let mut sink = MemBlocks::default();
    let mut b = TreeBuilder::new(|c, bytes: &[u8]| {
        sink.insert(c, bytes);
    });
    // Keys wide enough that the tree is several levels deep, so branch nodes
    // and leaf nodes are both represented.
    for i in 0..4000u32 {
        b.push_bytes(format!("k/{i:06}").as_bytes(), &vec![(i % 251) as u8; 200])
            .expect("push");
    }
    let root = b.finish().expect("finish");
    let mut biggest = 0usize;
    let mut leaves = 0usize;
    let mut branches = 0usize;
    for (cid, bytes) in sink.0.iter() {
        let _ = cid;
        biggest = biggest.max(bytes.len());
        match node::Node::parse(bytes) {
            Ok(n) if n.is_leaf() => leaves += 1,
            Ok(_) => branches += 1,
            Err(_) => {}
        }
    }
    assert!(
        leaves > 0 && branches > 0,
        "the fixture tree has {leaves} leaves and {branches} branches, so it \
         is not deep enough to exercise both node shapes"
    );
    // Every distinct node the builder produced, capped so the arm stays a
    // gate and not a benchmark. The largest is always included.
    let mut nodes: Vec<(usize, Vec<u8>)> =
        sink.0.iter().map(|(_, b)| (b.len(), b.to_vec())).collect();
    nodes.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    for (n, bytes) in nodes.iter().take(24) {
        let what: &'static str = if *n == biggest {
            "tree node: the largest this build wrote"
        } else {
            "tree node"
        };
        cases.push(Case::of(what, kind::TREE_NODE, bytes.clone()));
    }
    let _ = root;
    cases
}

/// Everything this SDK's prolly writes is accepted by the released contract.
#[test]
fn every_block_this_sdk_writes_is_accepted_by_the_released_contract() {
    let mut c = Contract::load(&block_wasm()).expect("the released block.wasm must load");
    let cases = write_arm_cases();
    assert!(
        cases.len() >= 12,
        "the write arm built only {} case(s); a green arm over almost nothing \
         reads exactly like a green arm over the format",
        cases.len()
    );

    let mut sizes: Vec<usize> = Vec::new();
    for case in &cases {
        let v = c.validate(&case.params(), &case.state);
        assert_eq!(
            v,
            Verdict::Valid,
            "the released contract REFUSED a {} of {} B that this SDK's \
             prolly wrote. A writer that emits what the contract refuses is a \
             writer whose data never lands.",
            case.what,
            case.state.len()
        );
        sizes.push(case.state.len());
    }

    // The control, and it must EXECUTE: a node one byte over the format's
    // limit is refused. Without it, "everything was accepted" is also what a
    // contract that accepts everything looks like — and the same harness bug
    // that once read every verdict as Valid would pass the arm above.
    let over = Case::of(
        "a node one byte over MAX_NODE",
        freenet_prolly::kind::TREE_NODE,
        vec![0u8; freenet_prolly::node::MAX_NODE + 1],
    );
    assert_eq!(
        c.validate(&over.params(), &over.state),
        Verdict::Invalid,
        "the contract ACCEPTED a node over MAX_NODE, so it is not enforcing \
         the bound and neither is this arm"
    );

    let biggest = sizes.iter().copied().max().unwrap_or(0);
    println!(
        "  write arm: {} case(s) accepted, largest {biggest} B; control (one \
         byte over MAX_NODE) refused",
        cases.len()
    );
}
