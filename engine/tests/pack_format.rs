//! Does the engine build the bytes the Block contract defines?
//!
//! `engine/src/pack.rs` is a second implementation of a format whose authority
//! lives in `freenet-contracts/block/src/pack.rs`. Two spellings of one format
//! drift, and the drift does not show up here — it shows up as a host refusing
//! a commit, in production, with nothing to point at.
//!
//! So this is a differential, not a self-test: the vectors were produced BY
//! the contract's own `build`, and the engine has to reproduce them byte for
//! byte. A test that checked the engine against its own output would pass
//! forever no matter how far the two drifted.

use engine::pack;

fn member(kind: u8, seed: u8, len: usize) -> (u8, Vec<u8>) {
    (
        kind,
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect(),
    )
}

/// A named set of members: what one vector covers.
type Case = (&'static str, Vec<(u8, Vec<u8>)>);

/// The same cases the emitter in freenet-contracts uses, in the same order.
fn cases() -> Vec<Case> {
    let raw = freenet_prolly::kind::RAW;
    vec![
        ("one-empty", vec![member(raw, 0, 0)]),
        ("one-small", vec![member(raw, 7, 13)]),
        (
            "three-unordered",
            vec![member(raw, 3, 40), member(raw, 1, 9), member(raw, 2, 77)],
        ),
        (
            "duplicates",
            vec![member(raw, 5, 20), member(raw, 5, 20), member(raw, 6, 4)],
        ),
        (
            "many",
            (0..12u8)
                .map(|i| member(raw, i, 1 + i as usize * 3))
                .collect(),
        ),
    ]
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn the_engine_builds_the_bytes_the_contract_defines() {
    let text = include_str!("pack_vectors.txt");
    let want: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|l| l.strip_prefix("VEC "))
        .filter_map(|l| l.split_once(' '))
        .collect();
    assert!(
        !want.is_empty(),
        "no vectors were read; this test would pass over nothing"
    );

    let cases = cases();
    assert_eq!(
        want.len(),
        cases.len(),
        "the vector file has {} case(s) and this test has {} — one of them \
         has been changed without the other",
        want.len(),
        cases.len()
    );

    for ((name, members), (vname, vhex)) in cases.iter().zip(&want) {
        assert_eq!(name, vname, "the cases are not in the same order");
        let body = pack::build(members).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(
            hex(&body),
            *vhex,
            "{name}: the engine's pack does not match the bytes the Block \
             contract produces for the same members"
        );
    }
    println!(
        "  {} pack vector(s) match the contract byte for byte",
        want.len()
    );
}

/// The manifest round-trips, and a truncated or over-declared one is refused.
///
/// Recovery reads this after a restart with nothing else to go on, so a
/// manifest that decodes to something plausible but wrong is a commit replayed
/// against the wrong head.
#[test]
fn a_commit_manifest_round_trips_and_refuses_a_malformed_one() {
    let m = pack::Manifest {
        prev_seq: 7,
        prev_root: [9u8; 32],
        seq: 8,
        root: [4u8; 32],
        owed: vec![
            [[1u8; 32], [2u8; 32], [3u8; 32]],
            [[5u8; 32], [6u8; 32], [7u8; 32]],
        ],
    };
    let bytes = m.encode();
    assert_eq!(pack::Manifest::decode(&bytes).as_ref(), Some(&m));

    for cut in [0, 1, 4, 40, bytes.len() - 1] {
        assert!(
            pack::Manifest::decode(&bytes[..cut]).is_none(),
            "a manifest truncated to {cut} byte(s) decoded anyway"
        );
    }
    // Trailing bytes are a second encoding of the same commit.
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(
        pack::Manifest::decode(&extra).is_none(),
        "a manifest with trailing bytes decoded anyway"
    );
    // A count that does not match what follows.
    let mut lying = bytes.clone();
    lying[84] = 9;
    assert!(
        pack::Manifest::decode(&lying).is_none(),
        "a manifest claiming 9 groups it does not carry decoded anyway"
    );
}
