//! The SIGNER's wire types (sdk#209), shared by the delegate (`signer`) and the page (`wire::signer`).
//!
//! A crate of its own so the page can speak to the signer without linking the signer: the delegate needs
//! freenet-stdlib's guest side, and the client-API boundary gate keeps freenet-stdlib out of the SDK core. This is
//! serde and bincode, nothing else.
//!
//! Every message starts with [`MAGIC`], `SG01`: a node's delegate reply does not say WHICH delegate sent it once
//! `wire::unframe` has classified it, so the page tells a signer answer from an engine reply by its first four bytes.
//! The `01` is the version; a different one is not read.

use serde::{Deserialize, Serialize};

/// `SG01`: the signer's messages, version 1.
pub const MAGIC: [u8; 4] = *b"SG01";

/// Most blocks one `PutBlocks` may carry: one return's worth of PUTs (the node's per-return limit, schedule.rs).
pub const MAX_PUT_BLOCKS: usize = 128;

/// A head: `(seq, root)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub seq: u64,
    pub root: [u8; 32],
}

/// The head that follows `prev`: `seq` MUST be `prev.seq + 1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Next {
    pub seq: u64,
    pub root: [u8; 32],
    /// WRITE-PATH's ledger (opaque until step 2 fixes its format). Signed as part of the value: `root` alone when
    /// empty -- exactly what every existing head reader accepts -- else `root ‖ ledger`.
    pub ledger: Vec<u8>,
}

impl Next {
    pub fn head(&self) -> Head {
        Head {
            seq: self.seq,
            root: self.root,
        }
    }
    /// The Register value this head is signed as.
    pub fn value(&self) -> Vec<u8> {
        let mut v = self.root.to_vec();
        v.extend_from_slice(&self.ledger);
        v
    }
}

/// Why a request is refused outright, rather than answered with a fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    NotSuccessor,
    NotProvisioned,
    HeadUnknown,
    RootNotHeld,
    RecordNotSaved,
    KeyAlreadyProvisioned,
    CannotSign,
    Unreadable,
    Forked {
        mine: Head,
        read: Head,
    },
    RegisterChanged,
    /// A `PutBlocks` with none, or more than [`MAX_PUT_BLOCKS`].
    BlockCount {
        max: u32,
        got: u32,
    },
    /// A `PutBlocks` state that is not a block (empty).
    NotABlock {
        index: u32,
    },
}

/// What the signer answers. See `signer::decide` for the rule behind `Signed` / `NotNext` / `AlreadySigned`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    /// The Register state bytes to UPDATE; the record is saved.
    Signed(Vec<u8>),
    /// `prev` is not the current head: rebase onto `current`.
    NotNext {
        current: Head,
    },
    /// `prev` was already signed from: the FIRST record's bytes.
    AlreadySigned(Vec<u8>),
    Provisioned,
    /// PUT-WITH-CODE accepted: one PUT is on its way per block, in order, to these Block contracts (the page names a
    /// block's contract the same way, `contract_for(block_code, block_id)`). One `Put` answer follows per PUT.
    Putting {
        contracts: Vec<[u8; 32]>,
    },
    /// The node's answer to one of those PUTs.
    Put {
        contract: [u8; 32],
        ok: bool,
        note: String,
    },
    Refused(Why),
}

/// What the page asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Sign {
        prev: Head,
        next: Next,
    },
    Provision {
        signing_key: Vec<u8>,
        register_code: Vec<u8>,
        register_params: Vec<u8>,
        block_code: Vec<u8>,
    },
    /// PUT-WITH-CODE (ENGINE-SHAPE, for a remote gateway): the page sends only block STATES (`kind ‖ body`); the
    /// signer, which holds the Block contract's code, names each contract and PUTs it from inside the node, so the
    /// ≈100 KB of code never crosses the page's socket (arm A, measured: 98.4 % of a client PUT is code).
    PutBlocks {
        states: Vec<Vec<u8>>,
    },
}

fn encode<T: Serialize>(t: &T) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend(bincode::serialize(t).expect("a signer message encodes"));
    out
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8], limit: u64) -> Option<T> {
    use bincode::Options;
    let rest = bytes.strip_prefix(&MAGIC)?;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .with_limit(limit)
        .deserialize(rest)
        .ok()
}

pub fn encode_request(r: &Request) -> Vec<u8> {
    encode(r)
}
pub fn encode_answer(a: &Answer) -> Vec<u8> {
    encode(a)
}
/// `None` for anything that is not a signer request of this version, or does not decode EXACTLY.
pub fn decode_request(bytes: &[u8]) -> Option<Request> {
    decode(bytes, 8 * 1024 * 1024)
}
/// `None` for anything that is not a signer answer of this version -- an engine reply included -- or does not decode
/// EXACTLY.
pub fn decode_answer(bytes: &[u8]) -> Option<Answer> {
    decode(bytes, 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips_and_carries_the_magic() {
        let reqs = [
            Request::Sign {
                prev: Head {
                    seq: 3,
                    root: [1; 32],
                },
                next: Next {
                    seq: 4,
                    root: [2; 32],
                    ledger: vec![9],
                },
            },
            Request::Provision {
                signing_key: vec![1],
                register_code: vec![2],
                register_params: vec![3],
                block_code: vec![4],
            },
            Request::PutBlocks {
                states: vec![vec![1, 2], vec![3]],
            },
        ];
        for r in reqs {
            let b = encode_request(&r);
            assert_eq!(&b[..4], b"SG01");
            assert_eq!(decode_request(&b), Some(r));
        }
        let a = Answer::Put {
            contract: [5; 32],
            ok: false,
            note: "no".into(),
        };
        assert_eq!(decode_answer(&encode_answer(&a)), Some(a));
    }

    #[test]
    fn what_is_not_a_signer_message_is_not_read() {
        let good = encode_answer(&Answer::Provisioned);
        assert_eq!(
            decode_answer(&good[1..]),
            None,
            "a message without its magic was read"
        );
        let mut v2 = good.clone();
        v2[3] = b'2';
        assert_eq!(decode_answer(&v2), None, "another version was read");
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            decode_answer(&trailing),
            None,
            "trailing bytes were accepted"
        );
        assert_eq!(decode_answer(&[]), None);
        assert_eq!(
            decode_answer(&[0x04, 0, 0, 0, 1]),
            None,
            "an engine reply was read as a signer answer"
        );
    }
}
