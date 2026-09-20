//! Framing for freenet's client API — the one place that knows it.
//!
//! Everything here is a pure function over freenet's OWN types. Nothing is
//! reimplemented: a format we restate is a format that drifts, and the
//! interesting failure would be the drift nobody notices because most
//! variants still decode.
//!
//! # Why a crate of its own
//!
//! The SDK core is sans-IO and testable with no node, and its boundary gate
//! keeps `freenet-stdlib` out of it on purpose. Putting the framing HERE keeps
//! that true, keeps every decision in Rust — `unframe` is a dispatch, and a
//! dispatch in JavaScript is a decision nothing tests — and means that
//! spanning two node versions later is two implementations behind one
//! interface rather than a refactor.
//!
//! # `unframe` is a trust boundary
//!
//! **A gateway is not the user's node.** Everything that arrives here was
//! chosen by something else, so:
//!
//! * sizes and counts are bounded BEFORE anything is allocated;
//! * garbage, truncation and a wrong variant are answers, never panics —
//!   this compiles to wasm with `panic = abort`, where a panic is a dead
//!   instance rather than an error an app can catch;
//! * [`Incoming::Unusable`] is COUNTED by reason, because a client that
//!   silently ignores what it cannot read waits for ever for an answer to a
//!   message nobody understood.
//!
//! And two rules that are about what the caller may DO with what comes back,
//! stated here because this is where the temptation is:
//!
//! * [`Incoming::HeadChanged`] is a HINT. A node can fabricate, replay or
//!   suppress it. Fabrication is contained because the data is
//!   content-addressed and must verify — so it is a confusion and denial
//!   vector, not corruption — but **a root supplied by the node is never
//!   authoritative until data verifies against it.**
//! * [`Refused`] carries a reason the NODE chose. It may drive at most a
//!   bounded retry, and **it may never lead to a credential prompt.**
//!   "Refused: bad key → ask the person to re-enter their passphrase" is a
//!   phishing channel delivered through the protocol.

#![forbid(unsafe_code)]

use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse};
use freenet_stdlib::prelude::*;

pub mod reassemble;
pub use reassemble::Reassembler;

/// The largest single frame this build will decode before looking inside.
///
/// A bound BEFORE the decoder sees anything, so a length field cannot ask for
/// an allocation the sender chooses. Matches the protocol crate's own ceiling
/// in spirit and is checked here because this is the outermost door.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// What arrived, once it has been understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Bytes for the engine — our protocol, inside their envelope.
    EngineBytes(Vec<u8>),
    /// A subscribed contract changed.
    ///
    /// **A HINT, never an authority.** See the module docs: a root from the
    /// node means "look", not "this is where the tree is".
    HeadChanged { key: String },
    /// The node accepted a request.
    ///
    /// **An ack is not durability.** A write becomes published by being READ
    /// BACK, never by being acknowledged — and through a gateway that is a
    /// trust requirement and not only a timing one: an ack is unauthenticated,
    /// so a hostile node could acknowledge a write it never made.
    Ack,
    /// The node refused. The reason is the NODE's word; see [`Refused`].
    Refused(Refused),
    /// Not something this build can use. Counted, never silently dropped.
    Unusable(Unusable),
    /// A chunk of a larger message; nothing to hand on yet.
    Partial,
}

/// A refusal, as the node worded it.
///
/// **What a caller may do with this is bounded, and the bound is the point.**
/// The node chose the reason, so it steers client control flow — which makes
/// it an input from a stranger, not a diagnosis. At most a bounded retry, and
/// never, under any reason, a prompt for a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The node's own words. For display and logs. Never matched on to decide
    /// anything security-relevant.
    pub said: String,
}

impl Refused {
    /// May a caller retry this at all? Bounded by the CALLER's own counter —
    /// this never says "retry for ever".
    pub fn retryable(&self) -> bool {
        true
    }

    /// Always false, and it is a function rather than a comment so that a
    /// future caller reaching for "should I ask for the passphrase again?"
    /// finds an answer instead of inventing one.
    ///
    /// No node-supplied reason may lead to a credential prompt. A node that
    /// could make a page ask for a passphrase has a phishing channel
    /// delivered through the protocol.
    pub fn may_prompt_for_credentials(&self) -> bool {
        false
    }
}

/// Why something could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// Bigger than this build will decode, before decoding it.
    TooLarge,
    /// Not decodable as a `HostResponse` at all.
    Unparseable,
    /// Decoded, but a kind this side has no use for.
    NotForUs,
    /// A chunked message whose shape this build refuses — too many chunks,
    /// an index outside its own total, or too many streams at once.
    BadStream,
}

/// Frame an engine message to the delegate.
///
/// Returns FRAMES, plural: stdlib chunks anything over its threshold, and the
/// engine delegate is ~721 KB, so `provision()` crosses it on the first
/// `RegisterDelegate` of every session. A `frame_*` that returned one buffer
/// would work for every small message and fail on the first real one.
pub fn frame_delegate_op(
    key: &DelegateKey,
    params: &Parameters<'static>,
    payload: Vec<u8>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    let req = ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: key.clone(),
        params: params.clone(),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(payload),
        )],
    });
    frames(&req, stream_id)
}

/// Frame a client-API subscription to a contract.
///
/// This is the notifier that reaches a connection which made no write: an
/// engine-originated push returns to whoever invoked the delegate, so it
/// cannot (F40).
pub fn frame_subscribe(id: ContractInstanceId, stream_id: u32) -> Result<Vec<Vec<u8>>, String> {
    let req = ClientRequest::ContractOp(freenet_stdlib::client_api::ContractRequest::Subscribe {
        key: id,
        summary: None,
    });
    frames(&req, stream_id)
}

/// Frame a contract PUT.
pub fn frame_put(
    contract: ContractContainer,
    state: WrappedState,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    let req = ClientRequest::ContractOp(freenet_stdlib::client_api::ContractRequest::Put {
        contract,
        state,
        related_contracts: RelatedContracts::default(),
        subscribe: false,
        blocking_subscribe: false,
    });
    frames(&req, stream_id)
}

/// Frame a delegate registration. This is the one that is always chunked.
pub fn frame_register_delegate(
    delegate: DelegateContainer,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    let req = ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate {
        delegate,
        cipher: [0u8; 32],
        nonce: [0u8; 24],
    });
    frames(&req, stream_id)
}

/// The websocket URL for a node's client API.
///
/// **`encodingProtocol=native` is not optional and not a default.** The node
/// falls back to Flatbuffers when the parameter is absent
/// (`client_events/websocket.rs:904` at `ea1ff5f`), while stdlib's own client
/// — and therefore everything here — speaks bincode. Built in Rust rather
/// than assembled in JavaScript for exactly that reason: it is a protocol
/// fact, and the first version of the page's socket omitted it.
pub fn ws_url(host: &str, port: u16) -> String {
    format!("ws://{host}:{port}/v1/contract/command?encodingProtocol=native")
}

/// Understand one frame from the node.
///
/// **This is the trust boundary.** Everything arriving here was chosen by
/// something else — and a gateway is not the user's node. Sizes and counts are
/// bounded before anything is allocated for them, nothing panics, and anything
/// unusable comes back said rather than swallowed.
///
/// Stateful, because replies are chunked: a frame may be part of a message and
/// not a message. `Partial` is that case, and it is not an error.
pub fn unframe(r: &mut Reassembler, bytes: &[u8]) -> Incoming {
    if bytes.len() > MAX_FRAME {
        return Incoming::Unusable(Unusable::TooLarge);
    }
    // A chunk first: a `StreamChunk` is a valid `HostResponse`, so decoding
    // has to happen before the shape is known.
    let decoded = match Reassembler::decode(bytes) {
        Ok(d) => d,
        // A node's own error reply is a MESSAGE, not a failure to read one.
        Err(Unusable::NotForUs) => {
            return Incoming::Refused(Refused {
                said: "the node refused the request".into(),
            })
        }
        Err(why) => return Incoming::Unusable(why),
    };
    let whole = match decoded {
        HostResponse::StreamChunk {
            stream_id,
            index,
            total,
            data,
        } => match r.chunk(stream_id, index, total, data.to_vec()) {
            Ok(None) => return Incoming::Partial,
            Ok(Some(bytes)) => match Reassembler::decode(&bytes) {
                Ok(d) => d,
                Err(why) => return Incoming::Unusable(why),
            },
            Err(why) => return Incoming::Unusable(why),
        },
        other => other,
    };
    classify(whole)
}

fn classify(r: HostResponse) -> Incoming {
    use freenet_stdlib::client_api::ContractResponse;
    match r {
        HostResponse::DelegateResponse { values, .. } => {
            let mut out = Vec::new();
            for v in values {
                if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                    out.extend_from_slice(m.payload.as_ref());
                }
            }
            if out.is_empty() {
                Incoming::Unusable(Unusable::NotForUs)
            } else {
                Incoming::EngineBytes(out)
            }
        }
        HostResponse::ContractResponse(ContractResponse::UpdateNotification { key, .. }) => {
            Incoming::HeadChanged {
                key: key.to_string(),
            }
        }
        HostResponse::ContractResponse(ContractResponse::PutResponse { .. })
        | HostResponse::ContractResponse(ContractResponse::SubscribeResponse { .. })
        | HostResponse::Ok => Incoming::Ack,
        _ => Incoming::Unusable(Unusable::NotForUs),
    }
}

/// Serialise and chunk, using stdlib's own rules.
fn frames(req: &ClientRequest<'static>, stream_id: u32) -> Result<Vec<Vec<u8>>, String> {
    use freenet_stdlib::client_api::streaming::{chunk_request, ensure_chunkable, CHUNK_THRESHOLD};
    let body = bincode::serialize(req).map_err(|e| e.to_string())?;
    if body.len() <= CHUNK_THRESHOLD {
        return Ok(vec![body]);
    }
    // Refuse BEFORE sending anything. stdlib's own client does this: streaming
    // an oversized payload only to have the node reject the first chunk wastes
    // the whole upload, and on a hotspot that is minutes.
    ensure_chunkable(body.len()).map_err(|e| e.to_string())?;
    chunk_request(body, stream_id)
        .iter()
        .map(|c| bincode::serialize(c).map_err(|e| e.to_string()))
        .collect()
}
