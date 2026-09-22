//! EVERY head reader reads the ONE format (signer_proto::head): the page's
//! `HeadRead`, the delegate's `head_of`, and the signer's own read agree on
//! every value, and all are TOLERANT (main's ruling: the last head change an
//! old reader cannot survive).

use contract_keys::register::{head_of, head_state};
use signer_proto::head::{value, Ledger};

fn params() -> Vec<u8> {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME)
}

fn ledgered(seq: u64, root: [u8; 32]) -> Vec<u8> {
    let prev = signer_proto::Head { seq: seq - 1, root: [1; 32] };
    head_state(&params(), &[7u8; 32], seq, &value(&root, &Ledger { prev: Some(prev), ..Ledger::default() })).expect("signs")
}

#[test]
fn the_page_and_the_delegate_read_every_head_alike() {
    for st in [
        head_state(&params(), &[7u8; 32], 1, &[3; 32]).expect("a bare head"),
        ledgered(2, [250; 32]),
        ledgered(u64::MAX - 1, [0; 32]),
    ] {
        let h = page::HeadRead::from_record(&st).expect("the page reads it");
        assert_eq!(Some((h.seq, h.root())), head_of(&st));
    }
}

/// A value whose ledger carries a field no build knows still yields its root, on
/// every reader.
#[test]
fn an_unknown_trailing_ledger_still_yields_the_root_on_every_reader() {
    let mut v = value(&[9; 32], &Ledger { prev: Some(signer_proto::Head { seq: 4, root: [2; 32] }), ..Ledger::default() });
    v.extend_from_slice(&[250, 2, 0, 7, 7]);
    let st = head_state(&params(), &[7u8; 32], 5, &v).expect("signs");
    let h = page::HeadRead::from_record(&st).expect("the page reads it");
    assert_eq!((h.seq, h.root(), h.prev()), (5, [9; 32], Some((4, [2; 32]))));
    assert_eq!(head_of(&st), Some((5, [9; 32])));
}

/// A malformed ledger is read ROOT-ONLY and gives NO prev (main's ruling (1): it
/// can never classify as "built on mine").
#[test]
fn a_malformed_ledger_reads_root_only_with_no_prev() {
    let mut v = value(&[9; 32], &Ledger { prev: Some(signer_proto::Head { seq: 4, root: [2; 32] }), ..Ledger::default() });
    v.truncate(v.len() - 3);
    let st = head_state(&params(), &[7u8; 32], 5, &v).expect("signs");
    let h = page::HeadRead::from_record(&st).expect("still a head");
    assert_eq!((h.root(), h.prev()), ([9; 32], None));
    assert_eq!(head_of(&st), Some((5, [9; 32])), "the delegate refused a head over its ledger");
}

/// THE ONE-TIME BREAK, named: the reader BEFORE this format demanded a value of
/// exactly 32 bytes, so a build from before it reads every ledgered head as NO
/// HEAD (an empty app). In development: rebuild everything on the key, then write.
#[test]
fn the_old_exact_root_reader_is_what_breaks_once() {
    fn old_head_of(state: &[u8]) -> Option<(u64, [u8; 32])> {
        let (seq, v) = signer_proto::head::record_of(state)?;
        Some((seq, v.try_into().ok()?))
    }
    let bare = head_state(&params(), &[7u8; 32], 1, &[3; 32]).expect("signs");
    assert_eq!(old_head_of(&bare), Some((1, [3; 32])), "an old head still reads on the old rule");
    assert_eq!(old_head_of(&ledgered(2, [4; 32])), None, "the old reader, on a ledgered head: 'no head'");
    assert_eq!(head_of(&ledgered(2, [4; 32])), Some((2, [4; 32])));
}

/// `sign_ledger`: PREV from the prev, omitted at the genesis.
#[test]
fn a_sign_carries_its_prev_and_the_genesis_carries_none() {
    assert!(page::sign_ledger(0, [0; 32], [5; 32]).is_empty(), "the genesis carries a PREV");
    let l = page::sign_ledger(3, [8; 32], [5; 32]);
    let h = page::HeadRead::from_value(4, &[[5u8; 32].as_slice(), &l].concat()).expect("a head");
    assert_eq!(h.prev(), Some((3, [8; 32])));
}
