//! The page's side of the SIGNER (sdk#209): sans-IO framing of its requests and reading of its answers.
//!
//! No socket, no state: the page's Effects executor calls these and sends / reads the bytes itself, as with every
//! other frame in this crate. The signer's messages start with `SG02` (`signer_proto::MAGIC`), so [`read_answer`] picks
//! a signer answer out of a delegate response and returns `None` for anything else -- an engine reply included --
//! because `unframe` does not say which delegate a response came from.
//!
//! Every request carries the page's own `id` (never [`UNATTRIBUTED`], refused here), and [`read_answer`] returns it
//! with the answer: THE way to attribute an answer when several requests are in flight (see `signer_proto`).

use crate::{frame_delegate_op, DelegateKey, Parameters};
pub use signer_proto::{
    Answer as SignerAnswer, Head, Next, Request as SignerRequest, Why, MAX_PUT_BLOCKS, UNATTRIBUTED,
};

fn frame(
    key: &DelegateKey,
    id: u32,
    r: &SignerRequest,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    if id == UNATTRIBUTED {
        return Err(format!(
            "request id {UNATTRIBUTED} is the signer's UNATTRIBUTED; a page's ids are its own and never it"
        ));
    }
    frame_delegate_op(
        key,
        &Parameters::from(vec![]),
        signer_proto::encode_request(id, r),
        stream_id,
    )
}

/// `sign(prev, next)`: the signer signs `next` only if `prev` is the current head. Answered `Signed`, `NotNext`,
/// `AlreadySigned` or `Refused` (see `signer::decide`). A node refusal ("queue full") is safe to ask again: a re-ask
/// is answered the first record.
pub fn frame_sign(
    key: &DelegateKey,
    id: u32,
    prev: Head,
    next: Next,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    frame(key, id, &SignerRequest::Sign { prev, next }, stream_id)
}

/// Ask the signer to sign version `version` of app `app`'s site, naming the web bundle whose blake3 is `bundle`
/// (builder#117).
pub fn frame_sign_site(
    key: &DelegateKey,
    id: u32,
    app: &str,
    version: u64,
    bundle: [u8; 32],
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    frame(key, id, &SignerRequest::SignSite { app: app.into(), version, bundle }, stream_id)
}

/// Provision the signer: the head's key, the Register it signs for, and the Block contract's code (for its root check
/// and PUT-WITH-CODE). Carries two contracts' code, so it is chunked like any large request.
pub fn frame_provision(
    key: &DelegateKey,
    id: u32,
    signing_key: Vec<u8>,
    register_code: Vec<u8>,
    register_params: Vec<u8>,
    block_code: Vec<u8>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    frame(
        key,
        id,
        &SignerRequest::Provision {
            signing_key,
            register_code,
            register_params,
            block_code,
        },
        stream_id,
    )
}

/// PUT-WITH-CODE, for a node that is not the page's own: block STATES only (`kind ‖ body`), at most
/// [`MAX_PUT_BLOCKS`]; the signer names each contract and PUTs it inside the node. Answered `Putting{contracts}` at
/// once, then one `Put{contract, ok, note}` per PUT. Refused HERE, before anything is framed, when the count is out
/// of range -- the signer would refuse it whole anyway.
pub fn frame_put_blocks(
    key: &DelegateKey,
    id: u32,
    states: Vec<Vec<u8>>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    if states.is_empty() || states.len() > MAX_PUT_BLOCKS {
        return Err(format!(
            "PUT-WITH-CODE carries 1..={MAX_PUT_BLOCKS} blocks, not {}",
            states.len()
        ));
    }
    frame(key, id, &SignerRequest::PutBlocks { states }, stream_id)
}

/// READ-LOCAL: ask whether the signer's node holds these contracts (1..=[`MAX_PUT_BLOCKS`]). Answered
/// `Held{present}`, in order. How the page confirms PUT-WITH-CODE blocks: a client GET of one is answered NotFound on a
/// node with a peer (F55). Refused HERE when the count is out of range.
pub fn frame_held(
    key: &DelegateKey,
    id: u32,
    contracts: Vec<[u8; 32]>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    if contracts.is_empty() || contracts.len() > MAX_PUT_BLOCKS {
        return Err(format!(
            "Held asks about 1..={MAX_PUT_BLOCKS} contracts, not {}",
            contracts.len()
        ));
    }
    frame(key, id, &SignerRequest::Held { contracts }, stream_id)
}

/// Ask the signer which Register it signs for ([`SignerRequest::Register`]): how a page reopens the person's own tree
/// instead of minting a new identity on every load.
pub fn frame_register_query(key: &DelegateKey, id: u32, stream_id: u32) -> Result<Vec<Vec<u8>>, String> {
    frame(key, id, &SignerRequest::Register, stream_id)
}

/// A signer answer and the id of the request it answers, from one application message of a delegate response;
/// `None` for anything that is not one. An id of [`UNATTRIBUTED`] answers no page request (a `Put`, which names its
/// contract, or a request whose id could not be read).
pub fn read_answer(payload: &[u8]) -> Option<(u32, SignerAnswer)> {
    signer_proto::decode_answer(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_stdlib::client_api::{ClientRequest, DelegateRequest};
    use freenet_stdlib::prelude::InboundDelegateMsg;

    fn key() -> DelegateKey {
        crate::delegate_from_code(b"not a real delegate, a name to frame to").1
    }

    /// The one message a small frame carries, decoded back the way the node reads it.
    fn payload(frames: &[Vec<u8>]) -> Vec<u8> {
        assert_eq!(frames.len(), 1, "a small request is one frame");
        let req: ClientRequest = bincode::deserialize(&frames[0]).expect("a client request");
        match req {
            ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) => {
                match &inbound[..] {
                    [InboundDelegateMsg::ApplicationMessage(m)] => m.payload.to_vec(),
                    other => panic!("not one application message: {other:?}"),
                }
            }
            other => panic!("not a delegate op: {other:?}"),
        }
    }

    #[test]
    fn a_sign_request_is_framed_to_the_signer_as_the_signer_reads_it() {
        let prev = Head {
            seq: 4,
            root: [1; 32],
        };
        let next = Next {
            seq: 5,
            root: [2; 32],
            ledger: vec![],
        };
        let p = payload(&frame_sign(&key(), 41, prev, next.clone(), 1).unwrap());
        assert_eq!(
            signer_proto::decode_request(&p),
            Some((
                41,
                SignerRequest::Sign {
                    prev,
                    next: next.clone()
                }
            ))
        );
        assert!(
            frame_sign(&key(), UNATTRIBUTED, prev, next, 1).is_err(),
            "a request framed under UNATTRIBUTED"
        );
    }

    #[test]
    fn put_blocks_is_framed_and_its_count_is_refused_before_framing() {
        let p = payload(&frame_put_blocks(&key(), 2, vec![vec![1, 2, 3]], 1).unwrap());
        assert_eq!(
            signer_proto::decode_request(&p),
            Some((
                2,
                SignerRequest::PutBlocks {
                    states: vec![vec![1, 2, 3]]
                }
            ))
        );
        assert!(frame_put_blocks(&key(), 2, vec![], 1).is_err());
        assert!(frame_put_blocks(&key(), 2, vec![vec![1]; MAX_PUT_BLOCKS + 1], 1).is_err());
        assert!(frame_put_blocks(&key(), 2, vec![vec![1]; MAX_PUT_BLOCKS], 1).is_ok());
    }

    #[test]
    fn held_is_framed_and_its_count_is_refused_before_framing() {
        let p = payload(&frame_held(&key(), 3, vec![[4; 32], [5; 32]], 1).unwrap());
        assert_eq!(
            signer_proto::decode_request(&p),
            Some((
                3,
                SignerRequest::Held {
                    contracts: vec![[4; 32], [5; 32]]
                }
            ))
        );
        assert!(frame_held(&key(), 3, vec![], 1).is_err());
        assert!(frame_held(&key(), 3, vec![[1; 32]; MAX_PUT_BLOCKS + 1], 1).is_err());
        assert!(frame_held(&key(), 3, vec![[1; 32]; MAX_PUT_BLOCKS], 1).is_ok());
    }

    #[test]
    fn a_provision_over_the_chunk_threshold_is_chunked_like_any_large_request() {
        use freenet_stdlib::client_api::streaming::CHUNK_THRESHOLD;
        // Sized from stdlib's OWN threshold, not a guess at it: two contracts' code that together pass it.
        let half = CHUNK_THRESHOLD / 2 + 1024;
        let frames = frame_provision(
            &key(),
            4,
            vec![7; 32],
            vec![0; half],
            vec![1; 40],
            vec![0; half],
            3,
        )
        .unwrap();
        assert!(
            frames.len() > 1,
            "{} frame(s) for {} B of code: not chunked",
            frames.len(),
            2 * half
        );
        let small = frame_provision(
            &key(),
            4,
            vec![7; 32],
            vec![0; 1000],
            vec![1; 40],
            vec![0; 1000],
            3,
        )
        .unwrap();
        assert_eq!(small.len(), 1, "a small provision was chunked");
    }

    #[test]
    fn only_a_signer_answer_is_read_as_one() {
        let a = SignerAnswer::NotNext {
            current: Head {
                seq: 9,
                root: [3; 32],
            },
        };
        assert_eq!(
            read_answer(&signer_proto::encode_answer(5, &a)),
            Some((5, a))
        );
        assert_eq!(read_answer(b"\x04not a signer answer"), None);
    }
}
