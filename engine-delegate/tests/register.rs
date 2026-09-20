//! Known-answer vectors for the Register's wire format.
//!
//! The format is implemented in `engine_delegate::register` rather than
//! imported, because sdk#35 deliberately decouples this repository's revision
//! from the contracts' and a path dependency would re-couple them. A second
//! copy of a wire format drifts, and care is not what stops it — THESE are.
//!
//! Every byte below was produced by the AUTHORITATIVE implementation
//! (`craftec-register-contract`, mode 0, single writer) and pasted here. They
//! do not descend from the code under test, which is the whole point: a
//! differential whose two sides share an ancestor proves translation
//! mechanics and not semantics. The authority also re-parsed its own output,
//! so these are bytes a real Register accepts and not merely bytes it emits.
//!
//! If one of these fails, this copy of the format has drifted and a head it
//! signs will be refused by the contract — silently, from the engine's point
//! of view, as a commit that never publishes.

use engine_delegate::register::{head_state, HeadError};

/// The fixed key the vectors were generated with.
const SIGNING_KEY: [u8; 32] = [7u8; 32];

/// Its public half, as the Register names the writer.
const VERIFYING_KEY: [u8; 32] = [
    234, 74, 108, 99, 226, 156, 82, 10, 190, 245, 80, 123, 19, 46, 197, 249, 149, 71, 118, 174,
    190, 190, 123, 146, 66, 30, 234, 105, 20, 70, 210, 44,
];

/// `RG01 | mode 0 | writer key | label "head"`.
fn params() -> Vec<u8> {
    let mut p = Vec::from(*b"RG01");
    p.push(0u8);
    p.extend_from_slice(&VERIFYING_KEY);
    p.extend_from_slice(b"head");
    p
}

/// The state the authority produced for seq 7 over a 32-byte value of 0xAB.
const EXPECTED: [u8; 112] = [
    82, 71, 48, 49, 1, 0, 7, 0, 0, 0, 0, 0, 0, 0, 32, 0, 171, 171, 171, 171, 171, 171, 171, 171,
    171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171,
    171, 171, 171, 171, 171, 102, 159, 114, 240, 136, 190, 155, 15, 5, 70, 46, 50, 175, 198, 93,
    208, 40, 176, 108, 225, 1, 25, 187, 145, 189, 234, 179, 165, 249, 215, 69, 96, 165, 85, 237,
    77, 23, 181, 211, 251, 245, 214, 52, 21, 98, 242, 131, 76, 251, 73, 236, 216, 162, 173, 187,
    224, 155, 18, 236, 76, 104, 78, 212, 15,
];

#[test]
fn a_signed_head_matches_the_authoritys_own_bytes() {
    let got = head_state(&params(), &SIGNING_KEY, 7, &[0xABu8; 32]).expect("a head");
    assert_eq!(
        got.len(),
        EXPECTED.len(),
        "the encoding is {} B where the authority produced {}",
        got.len(),
        EXPECTED.len()
    );
    assert_eq!(
        got,
        EXPECTED.to_vec(),
        "this copy of the Register's format has DRIFTED. A head it signs will \
         be refused by the contract, which the engine sees only as a commit \
         that never publishes."
    );
}

/// Ed25519 signing is deterministic (RFC 8032), so the same inputs must give
/// the same bytes every time — which is what makes the vector above a vector
/// and not a sample.
#[test]
fn signing_is_deterministic() {
    let a = head_state(&params(), &SIGNING_KEY, 7, &[0xABu8; 32]).unwrap();
    let b = head_state(&params(), &SIGNING_KEY, 7, &[0xABu8; 32]).unwrap();
    assert_eq!(a, b);
}

/// The seq and the value are both SIGNED, so changing either changes the
/// bytes. Without this, the vector above would also pass for an
/// implementation that ignored its arguments.
#[test]
fn the_seq_and_the_value_are_both_covered_by_the_signature() {
    let base = head_state(&params(), &SIGNING_KEY, 7, &[0xABu8; 32]).unwrap();
    let other_seq = head_state(&params(), &SIGNING_KEY, 8, &[0xABu8; 32]).unwrap();
    let other_value = head_state(&params(), &SIGNING_KEY, 7, &[0xACu8; 32]).unwrap();
    assert_ne!(base, other_seq, "the seq is not covered");
    assert_ne!(base, other_value, "the value is not covered");
    // ...and not merely in the plaintext: the SIGNATURES differ too.
    assert_ne!(base[base.len() - 64..], other_seq[other_seq.len() - 64..]);
    assert_ne!(
        base[base.len() - 64..],
        other_value[other_value.len() - 64..]
    );
}

/// A key that is not the Register's writer is refused HERE.
///
/// The node would refuse it anyway, but a commit whose head is refused on
/// arrival waits for a confirmation that is never coming — so this is caught
/// where it can still be reported.
#[test]
fn a_key_that_is_not_the_writer_is_refused() {
    let err = head_state(&params(), &[9u8; 32], 7, &[0xABu8; 32]).unwrap_err();
    assert_eq!(err, HeadError::WrongWriter);
    // The control: the RIGHT key against the same params succeeds, so the
    // refusal is the key and not the params.
    assert!(head_state(&params(), &SIGNING_KEY, 7, &[0xABu8; 32]).is_ok());
}

/// A k-of-n keyset head is refused rather than guessed at.
#[test]
fn a_quorum_register_is_refused_not_signed_as_if_it_were_one_writer() {
    let mut quorum = Vec::from(*b"RG01");
    quorum.push(1u8); // mode 1: k-of-n
    quorum.push(2u8);
    quorum.push(3u8);
    quorum.extend_from_slice(&[0u8; 96]);
    assert_eq!(
        head_state(&quorum, &SIGNING_KEY, 7, &[0xABu8; 32]).unwrap_err(),
        HeadError::NotSingleWriter,
        "a quorum Register was signed as though one key decided it, which \
         produces a record that looks valid and decides nothing"
    );
}
