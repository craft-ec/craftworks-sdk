//! The SIGNER's wire types (sdk#209), shared by the delegate (`signer`) and the page (`wire::signer`).
//!
//! A crate of its own so the page can speak to the signer without linking the signer: the delegate needs
//! freenet-stdlib's guest side, and the client-API boundary gate keeps freenet-stdlib out of the SDK core. This is
//! serde and bincode, nothing else.
//!
//! Every message starts with [`MAGIC`], `SG02`: a node's delegate reply does not say WHICH delegate sent it once
//! `wire::unframe` has classified it, so the page tells a signer answer from an engine reply by its first four bytes.
//! The `02` is the version; a different one is not read.
//!
//! # Every answer names its request (SG02)
//!
//! After the magic comes the request's `id` (u32), and the signer ECHOES it on the answer. An answer's variant does
//! not say which request it answers: `Held{present}` names no contract, `Refused(NotProvisioned)` answers a sign or a
//! put, `Refused(BlockCount)` a put or a held. With two requests in flight only the id attributes an answer -- a pacing
//! rule (one request of each kind at a time) would stand in for an identity. The page's ids are its own; `0` is
//! [`UNATTRIBUTED`], never a page's: the answer to a request whose id could not be read, and the `Put` answers, which
//! arrive after the request that caused them and name their contract instead.

use serde::{Deserialize, Serialize};

pub mod head;

/// `SG02`: the signer's messages, version 2 (an echoed request id).
pub const MAGIC: [u8; 4] = *b"SG02";

/// The id no page request carries: an answer that cannot be attributed to one (see the crate docs).
pub const UNATTRIBUTED: u32 = 0;

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
    /// A `PutBlocks` or `Held` with none, or more than [`MAX_PUT_BLOCKS`].
    BlockCount {
        max: u32,
        got: u32,
    },
    /// A `PutBlocks` state that is not a block (empty).
    NotABlock {
        index: u32,
    },
    /// The value to sign carries a ledger that is not the format ([`head::check`]): nothing is signed, because a
    /// malformed ledger, once signed, degrades every reader's merge for that head's life and nobody is told.
    BadLedger,
    /// A GATED verb (sign, which Register, re-provision, approve) from an origin the owner has not approved and
    /// without the owner's capability (sdk#318). Nothing changed. Appended, as every variant after `BadLedger`.
    NotApproved,
    /// A verb this signer no longer serves (`PutBlocks`, sdk#318): its slot is kept so every later variant keeps
    /// its encoding.
    Removed,
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
    /// block's contract the same way, `contract_for(block_code, block_id)`). Confirm them with `Held`: the node's
    /// answer to each PUT is relayed as `Put`, but no `Put` reached the ASKING connection in the live run (#214).
    Putting {
        contracts: Vec<[u8; 32]>,
    },
    /// The node's answer to one of those PUTs.
    Put {
        contract: [u8; 32],
        ok: bool,
        note: String,
    },
    /// READ-LOCAL: for each contract asked, in order, whether THIS node holds its state.
    Held {
        present: Vec<bool>,
    },
    Refused(Why),
    /// The Register this signer signs for — its params — or `None` when it holds no key (never provisioned). Appended
    /// LAST, so every earlier answer keeps its encoding.
    Register {
        params: Option<Vec<u8>>,
    },
    /// Provisioned (or the held key re-stated), and the OWNER CAPABILITY (sdk#318): what the owner's own session
    /// keeps and wraps its gated requests in ([`Request::WithCap`]). Derived from the signing key, so re-stating
    /// the key yields the same one; it is never the key.
    Capability([u8; 32]),
    /// The web apps (contract instance ids) the owner has approved to sign, after an `Approve` / `Unapprove`.
    Approved {
        apps: Vec<[u8; 32]>,
    },
    /// MEMBERSHIP (sdk#318): whether this signer signs for the Register these params name. What "is the viewer
    /// the publisher?" needs, without telling a stranger's site whose node this is.
    Owns(bool),
    /// Whether this signer has signed anything for its Register: it holds a record, or can read a head of it
    /// ([`Request::HasRecord`]).
    HasRecord(bool),
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
    /// READ-LOCAL: does this node hold these contracts' state? The signer's synchronous local read, per contract --
    /// never a network fetch, so it cannot park. How the page confirms a PUT-WITH-CODE block: on a node WITH A PEER a
    /// client GET of a delegate-put block is answered NotFound (F55), and the `Put` answers do not reach the page.
    /// 1..=[`MAX_PUT_BLOCKS`] contracts.
    Held {
        contracts: Vec<[u8; 32]>,
    },
    /// WHICH REGISTER DO YOU SIGN FOR? A page that opens must reopen the person's own tree, not mint a new identity
    /// on every load: this is how it learns whether the signer already holds a key, and for which Register. Needs
    /// nothing; answers [`Answer::Register`]. The key itself never leaves the signer. Appended LAST, so every earlier
    /// request keeps its encoding.
    Register,
    /// A GATED request carrying the OWNER CAPABILITY ([`Answer::Capability`]) (sdk#318). Only the owner's own
    /// session holds it; an app the owner approved signs without it (its origin is `WebApp(id)`). One level only.
    WithCap {
        cap: [u8; 32],
        inner: Box<Request>,
    },
    /// Approve a web app (its contract instance id) to use the gated verbs. Capability only: approval happens in
    /// the owner's own session, never by an app's request.
    Approve {
        app: [u8; 32],
    },
    /// Withdraw an approval. Capability only.
    Unapprove {
        app: [u8; 32],
    },
    /// Does this signer sign for the Register these params name? Open to all; answers only yes or no
    /// ([`Answer::Owns`]).
    Owns {
        register_params: Vec<u8>,
    },
    /// Has this signer signed anything for its Register? Open to all ([`Answer::HasRecord`]), and nothing is signed:
    /// the question a page asks at open, which it used to ask with an unsignable `Sign` (sdk#318 gates `Sign`).
    HasRecord,
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

/// Request `id` (a page's own, never [`UNATTRIBUTED`]); the answer carries it back.
pub fn encode_request(id: u32, r: &Request) -> Vec<u8> {
    encode(&(id, r))
}
/// An answer to request `id`.
pub fn encode_answer(id: u32, a: &Answer) -> Vec<u8> {
    encode(&(id, a))
}
/// `(id, request)`; `None` for anything that is not a signer request of this version, or does not decode EXACTLY.
pub fn decode_request(bytes: &[u8]) -> Option<(u32, Request)> {
    decode(bytes, 8 * 1024 * 1024)
}
/// The id of a request that does not decode, so its `Unreadable` still names it: the four bytes after the magic,
/// [`UNATTRIBUTED`] when there are not four, or no magic.
pub fn request_id(bytes: &[u8]) -> u32 {
    match bytes.strip_prefix(&MAGIC).and_then(|r| r.get(..4)) {
        Some(id) => u32::from_le_bytes([id[0], id[1], id[2], id[3]]),
        None => UNATTRIBUTED,
    }
}
/// `(id, answer)`; `None` for anything that is not a signer answer of this version -- an engine reply included -- or does not decode
/// EXACTLY.
pub fn decode_answer(bytes: &[u8]) -> Option<(u32, Answer)> {
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
            Request::Held {
                contracts: vec![[6; 32], [7; 32]],
            },
        ];
        for (id, r) in (1..).zip(reqs) {
            let b = encode_request(id, &r);
            assert_eq!(&b[..4], b"SG02");
            assert_eq!(request_id(&b), id);
            assert_eq!(decode_request(&b), Some((id, r)));
        }
        let a = Answer::Put {
            contract: [5; 32],
            ok: false,
            note: "no".into(),
        };
        assert_eq!(decode_answer(&encode_answer(7, &a)), Some((7, a)));
        let h = Answer::Held {
            present: vec![true, false],
        };
        assert_eq!(
            decode_answer(&encode_answer(u32::MAX, &h)),
            Some((u32::MAX, h))
        );
    }

    #[test]
    fn what_is_not_a_signer_message_is_not_read() {
        let good = encode_answer(3, &Answer::Provisioned);
        assert_eq!(
            decode_answer(&good[1..]),
            None,
            "a message without its magic was read"
        );
        let mut v2 = good.clone();
        v2[3] = b'1';
        assert_eq!(decode_answer(&v2), None, "version 1 was read as version 2");
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            decode_answer(&trailing),
            None,
            "trailing bytes were accepted"
        );
        assert_eq!(decode_answer(&[]), None);
        assert_eq!(
            request_id(b"SG02\x05\x00"),
            UNATTRIBUTED,
            "a short id was read"
        );
        assert_eq!(
            request_id(b"SG01\x05\x00\x00\x00"),
            UNATTRIBUTED,
            "another version's id was read"
        );
        assert_eq!(request_id(b"SG02\x05\x00\x00\x00garbage"), 5);
        assert_eq!(
            decode_answer(&[0x04, 0, 0, 0, 1]),
            None,
            "an engine reply was read as a signer answer"
        );
    }
}
