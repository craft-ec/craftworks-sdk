//! sdk#209's acceptance, natively: `serve` over an in-memory Host (a secret map + the node's local contract states).
use engine_delegate::register::head_of;
use signer::*;
use std::collections::BTreeMap;

const RCODE: &[u8] = b"a register contract's code, as provisioned";
const BCODE: &[u8] = b"a block contract's code, as provisioned";

#[derive(Default, Clone)]
struct Mem {
    secrets: BTreeMap<Vec<u8>, Vec<u8>>,
    states: BTreeMap<[u8; 32], Vec<u8>>,
    /// A `set_secret` of the RECORD fails (the store is full, or the node refuses).
    record_write_fails: bool,
}
impl Host for Mem {
    fn get_secret(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.secrets.get(k).cloned()
    }
    fn set_secret(&mut self, k: &[u8], v: &[u8]) -> bool {
        if self.record_write_fails && k == RECORD {
            return false;
        }
        self.secrets.insert(k.to_vec(), v.to_vec());
        true
    }
    fn contract_state(&self, id: &[u8; 32]) -> Option<Vec<u8>> {
        self.states.get(id).cloned()
    }
}

struct World {
    host: Mem,
    params: Vec<u8>,
}

impl World {
    fn new() -> World {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        let mut host = Mem::default();
        let a = serve(
            &mut host,
            &encode_request(&Request::Provision {
                signing_key: sk.to_bytes().to_vec(),
                register_code: RCODE.to_vec(),
                register_params: params.clone(),
                block_code: BCODE.to_vec(),
            }),
        );
        assert_eq!(a, Answer::Provisioned);
        World { host, params }
    }
    /// The node holds this tree's root block.
    fn hold_root(&mut self, root: [u8; 32]) {
        self.host.states.insert(
            engine_delegate::blocks::contract_for(BCODE, &root),
            vec![1, 2, 3],
        );
    }
    /// The page's UPDATE of the Register landed: the node's local state is now this signed record.
    fn land(&mut self, signed: &[u8]) {
        self.host
            .states
            .insert(register_id(RCODE, &self.params), signed.to_vec());
    }
    fn sign(&mut self, prev: Head, next: &Next) -> Answer {
        serve(
            &mut self.host,
            &encode_request(&Request::Sign {
                prev,
                next: next.clone(),
            }),
        )
    }
}

fn root(n: u8) -> [u8; 32] {
    [n; 32]
}
fn genesis() -> Head {
    Head {
        seq: 0,
        root: root(0),
    }
}
fn next(seq: u64, r: u8) -> Next {
    Next {
        seq,
        root: root(r),
        ledger: Vec::new(),
    }
}
fn signed(a: &Answer) -> Vec<u8> {
    match a {
        Answer::Signed(b) | Answer::AlreadySigned(b) => b.clone(),
        other => panic!("not a signature: {other:?}"),
    }
}

#[test]
fn a_head_is_signed_from_genesis_and_reads_back_as_that_head() {
    let mut w = World::new();
    w.hold_root(root(1));
    let a = w.sign(genesis(), &next(1, 1));
    assert!(matches!(a, Answer::Signed(_)), "{a:?}");
    assert_eq!(
        head_of(&signed(&a)),
        Some((1, root(1))),
        "the signed state is not the head it was asked for"
    );
}

#[test]
fn an_identical_re_ask_gets_the_same_bytes_back() {
    let mut w = World::new();
    w.hold_root(root(1));
    let first = w.sign(genesis(), &next(1, 1));
    let again = w.sign(genesis(), &next(1, 1));
    assert_eq!(again, Answer::AlreadySigned(signed(&first)));
}

/// THE RACE: two connections ask from the same prev; the loser is told what won, BEFORE any UPDATE leaves (nothing
/// has landed here), and never gets a signature of its own.
#[test]
fn two_tabs_racing_from_one_prev_get_one_signature() {
    let mut w = World::new();
    w.hold_root(root(1));
    w.hold_root(root(2));
    let a = w.sign(genesis(), &next(1, 1));
    let b = w.sign(genesis(), &next(1, 2));
    assert!(matches!(a, Answer::Signed(_)));
    assert_eq!(
        b,
        Answer::AlreadySigned(signed(&a)),
        "the loser was not handed the winner's record"
    );
    assert_eq!(
        head_of(&signed(&b)),
        Some((1, root(1))),
        "the loser learns the WINNER's root"
    );
}

#[test]
fn the_head_moves_on_once_the_signed_record_has_landed() {
    let mut w = World::new();
    w.hold_root(root(1));
    w.hold_root(root(2));
    let a = w.sign(genesis(), &next(1, 1));
    w.land(&signed(&a));
    let b = w.sign(
        Head {
            seq: 1,
            root: root(1),
        },
        &next(2, 2),
    );
    assert!(matches!(b, Answer::Signed(_)), "{b:?}");
    // And the old prev is now stale: told the current head, not signed again.
    let c = w.sign(genesis(), &next(1, 2));
    assert_eq!(
        c,
        Answer::NotNext {
            current: Head {
                seq: 2,
                root: root(2)
            }
        }
    );
}

/// The signer does not need the page's UPDATE to have landed to move on: its own record is truth.
#[test]
fn the_record_alone_is_enough_to_move_on() {
    let mut w = World::new();
    w.hold_root(root(1));
    w.hold_root(root(2));
    let _ = w.sign(genesis(), &next(1, 1));
    let b = w.sign(
        Head {
            seq: 1,
            root: root(1),
        },
        &next(2, 2),
    );
    assert!(matches!(b, Answer::Signed(_)), "{b:?}");
}

#[test]
fn a_head_read_ahead_of_the_record_is_the_truth() {
    let mut w = World::new();
    w.hold_root(root(1));
    w.hold_root(root(9));
    let _ = w.sign(genesis(), &next(1, 1));
    // Another holder of the key signed further (seq 5): the Register shows it.
    let other = engine_delegate::register::head_state(&w.params, &[7u8; 32], 5, &root(5)).unwrap();
    w.land(&other);
    assert_eq!(
        w.sign(
            Head {
                seq: 1,
                root: root(1)
            },
            &next(2, 9)
        ),
        Answer::NotNext {
            current: Head {
                seq: 5,
                root: root(5)
            }
        }
    );
    assert!(matches!(
        w.sign(
            Head {
                seq: 5,
                root: root(5)
            },
            &next(6, 9)
        ),
        Answer::Signed(_)
    ));
}

/// Q4: the Register at the SAME seq as the record, a different root. The record (what THIS key signed) is the truth.
#[test]
fn at_equal_seq_the_record_wins_over_the_read() {
    let mut w = World::new();
    w.hold_root(root(1));
    w.hold_root(root(3));
    let _ = w.sign(genesis(), &next(1, 1));
    let other = engine_delegate::register::head_state(&w.params, &[7u8; 32], 1, &root(4)).unwrap();
    w.land(&other);
    assert_eq!(
        w.sign(
            Head {
                seq: 1,
                root: root(4)
            },
            &next(2, 3)
        ),
        Answer::NotNext {
            current: Head {
                seq: 1,
                root: root(1)
            }
        }
    );
}

#[test]
fn every_refusal_is_named() {
    let mut w = World::new();
    assert_eq!(
        w.sign(genesis(), &next(1, 1)),
        Answer::Refused(Why::RootNotHeld)
    );
    w.hold_root(root(1));
    assert_eq!(
        w.sign(genesis(), &next(2, 1)),
        Answer::Refused(Why::NotSuccessor)
    );
    assert_eq!(
        w.sign(
            Head {
                seq: 4,
                root: root(4)
            },
            &next(5, 1)
        ),
        Answer::Refused(Why::HeadUnknown)
    );
    assert_eq!(
        serve(&mut w.host, b"\x09junk"),
        Answer::Refused(Why::Unreadable)
    );
    assert_eq!(serve(&mut w.host, &[]), Answer::Refused(Why::Unreadable));
    let mut bare = Mem::default();
    assert_eq!(
        serve(
            &mut bare,
            &encode_request(&Request::Sign {
                prev: genesis(),
                next: next(1, 1)
            })
        ),
        Answer::Refused(Why::NotProvisioned)
    );
    let other = Request::Provision {
        signing_key: vec![8u8; 32],
        register_code: RCODE.to_vec(),
        register_params: w.params.clone(),
        block_code: BCODE.to_vec(),
    };
    assert_eq!(
        serve(&mut w.host, &encode_request(&other)),
        Answer::Refused(Why::KeyAlreadyProvisioned)
    );
}

/// NO SIGNATURE WITHOUT ITS RECORD: a record that cannot be written leaves no signature behind, and the same request
/// is signed once the write can succeed.
#[test]
fn a_record_that_cannot_be_written_returns_no_signature() {
    let mut w = World::new();
    w.hold_root(root(1));
    w.host.record_write_fails = true;
    assert_eq!(
        w.sign(genesis(), &next(1, 1)),
        Answer::Refused(Why::RecordNotSaved)
    );
    w.host.record_write_fails = false;
    assert!(matches!(w.sign(genesis(), &next(1, 1)), Answer::Signed(_)));
}

/// THE CRASH ORDERINGS. After a signature at seq 1 the node goes away (the record kept or lost; the head landed or
/// not) and the page re-asks from the same prev with a DIFFERENT next. Counted: the distinct signatures at seq 1.
/// Three orderings yield ONE. The fourth -- the record LOST after it was written and the head never landed -- yields
/// two: it is exactly the platform fact sdk#209's live test measures (is set_secret durable before the reply), pinned
/// here so the dependency is stated rather than assumed.
#[test]
fn only_a_lost_record_with_an_unlanded_head_can_sign_twice_at_one_seq() {
    for (record_kept, head_landed) in [(true, true), (true, false), (false, true), (false, false)] {
        let mut w = World::new();
        w.hold_root(root(1));
        w.hold_root(root(2));
        let first = signed(&w.sign(genesis(), &next(1, 1)));
        if head_landed {
            w.land(&first);
        }
        if !record_kept {
            w.host.secrets.remove(RECORD);
        }
        let again = w.sign(genesis(), &next(1, 2));
        let second = match &again {
            Answer::Signed(b) | Answer::AlreadySigned(b) => Some(b.clone()),
            _ => None,
        };
        let distinct: std::collections::BTreeSet<Vec<u8>> = [Some(first.clone()), second]
            .into_iter()
            .flatten()
            .filter(|b| head_of(b).map(|(s, _)| s) == Some(1))
            .collect();
        println!(
            "  record kept {record_kept}, head landed {head_landed}: re-ask told {}; distinct signatures at seq 1: {}",
            match &again {
                Answer::Signed(_) => "Signed".to_string(),
                a => format!("{a:?}").chars().take(40).collect(),
            },
            distinct.len()
        );
        let expect = if !record_kept && !head_landed { 2 } else { 1 };
        assert_eq!(
            distinct.len(),
            expect,
            "record kept {record_kept}, head landed {head_landed}"
        );
    }
}
