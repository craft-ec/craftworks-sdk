//! The page's side of the SIGNER (sdk#209): sans-IO framing of its requests and reading of its answers.
//!
//! No socket, no state: the page's Effects executor calls these and sends / reads the bytes itself, as with every
//! other frame in this crate. The signer's messages start with `SG01` (`signer_proto::MAGIC`), so [`read_answer`] picks
//! a signer answer out of a delegate response and returns `None` for anything else -- an engine reply included --
//! because `unframe` does not say which delegate a response came from.

use crate::{frame_delegate_op, DelegateKey, Parameters};
pub use signer_proto::{
    Answer as SignerAnswer, Head, Next, Request as SignerRequest, Why, MAX_PUT_BLOCKS,
};

fn frame(key: &DelegateKey, r: &SignerRequest, stream_id: u32) -> Result<Vec<Vec<u8>>, String> {
    frame_delegate_op(
        key,
        &Parameters::from(vec![]),
        signer_proto::encode_request(r),
        stream_id,
    )
}

/// `sign(prev, next)`: the signer signs `next` only if `prev` is the current head. Answered `Signed`, `NotNext`,
/// `AlreadySigned` or `Refused` (see `signer::decide`). A node refusal ("queue full") is safe to ask again: a re-ask
/// is answered the first record.
pub fn frame_sign(
    key: &DelegateKey,
    prev: Head,
    next: Next,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    frame(key, &SignerRequest::Sign { prev, next }, stream_id)
}

/// Provision the signer: the head's key, the Register it signs for, and the Block contract's code (for its root check
/// and PUT-WITH-CODE). Carries two contracts' code, so it is chunked like any large request.
pub fn frame_provision(
    key: &DelegateKey,
    signing_key: Vec<u8>,
    register_code: Vec<u8>,
    register_params: Vec<u8>,
    block_code: Vec<u8>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    frame(
        key,
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
    states: Vec<Vec<u8>>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    if states.is_empty() || states.len() > MAX_PUT_BLOCKS {
        return Err(format!(
            "PUT-WITH-CODE carries 1..={MAX_PUT_BLOCKS} blocks, not {}",
            states.len()
        ));
    }
    frame(key, &SignerRequest::PutBlocks { states }, stream_id)
}

/// A signer answer, from one application message of a delegate response; `None` for anything that is not one.
pub fn read_answer(payload: &[u8]) -> Option<SignerAnswer> {
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
        let p = payload(&frame_sign(&key(), prev, next.clone(), 1).unwrap());
        assert_eq!(
            signer_proto::decode_request(&p),
            Some(SignerRequest::Sign { prev, next })
        );
    }

    #[test]
    fn put_blocks_is_framed_and_its_count_is_refused_before_framing() {
        let p = payload(&frame_put_blocks(&key(), vec![vec![1, 2, 3]], 1).unwrap());
        assert_eq!(
            signer_proto::decode_request(&p),
            Some(SignerRequest::PutBlocks {
                states: vec![vec![1, 2, 3]]
            })
        );
        assert!(frame_put_blocks(&key(), vec![], 1).is_err());
        assert!(frame_put_blocks(&key(), vec![vec![1]; MAX_PUT_BLOCKS + 1], 1).is_err());
        assert!(frame_put_blocks(&key(), vec![vec![1]; MAX_PUT_BLOCKS], 1).is_ok());
    }

    #[test]
    fn a_provision_over_the_chunk_threshold_is_chunked_like_any_large_request() {
        use freenet_stdlib::client_api::streaming::CHUNK_THRESHOLD;
        // Sized from stdlib's OWN threshold, not a guess at it: two contracts' code that together pass it.
        let half = CHUNK_THRESHOLD / 2 + 1024;
        let frames = frame_provision(
            &key(),
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
        assert_eq!(read_answer(&signer_proto::encode_answer(&a)), Some(a));
        assert_eq!(read_answer(b"\x04not a signer answer"), None);
    }
}
