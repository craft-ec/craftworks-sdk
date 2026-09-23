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

//! # The node's version is not on this wire
//!
//! Checked at the deciding lines of `freenet-stdlib` 0.10.0, because the right
//! response to a version skew is to REFUSE rather than to misread — and
//! refusing requires knowing.
//!
//! * `HostResponse` (`client_api/client_events.rs:771`) — `ContractResponse`,
//!   `DelegateResponse`, `QueryResponse`, `Ok`, `StreamChunk`, `StreamHeader`.
//!   No version.
//! * `QueryResponse` (`:812`) — `ConnectedPeers`, `NetworkDebug`,
//!   `NodeDiagnostics`, `NeighborHosting`. No version.
//! * `NodeInfo` (`:856`), the richest node-describing struct the API has —
//!   `peer_id`, `is_gateway`, `location`, `listening_address`,
//!   `uptime_seconds`. **No version field.**
//! * `NodeQuery` (`:901`) — nothing that asks for one.
//!
//! **So there is nothing to detect and nothing to refuse on, and no
//! negotiation is designed here.** The binary reports its version on its own
//! command line (`freenet --version`), which a page cannot reach.
//!
//! What follows for us: the version this build was written against is pinned
//! in `Cargo.toml` and belongs in the versions panel and the diagnostic bundle
//! as ENVIRONMENT, not as something checked at runtime. And the dangerous
//! skew is named so nobody later infers safety from silence: **a bump where
//! most variants still decode and one has changed** would present as ordinary
//! traffic with one thing quietly wrong. Compatibility must never be inferred
//! from "the first message parsed".

#![forbid(unsafe_code)]

use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse};
use freenet_stdlib::prelude::*;

pub mod block;
pub mod puts;
pub mod signer;
pub mod webapp;
pub mod reassemble;
/// The delegate's identity, so a caller can hold one without depending on
/// freenet itself. Opaque everywhere outside this crate.
pub use freenet_stdlib::prelude::DelegateKey;
pub use reassemble::Reassembler;

/// The largest single frame this build will decode before looking inside.
///
/// A bound BEFORE the decoder sees anything, so a length field cannot ask for
/// an allocation the sender chooses. It is the outermost door.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// The largest CHUNK this build will hold.
///
/// stdlib's own `CHUNK_SIZE`: a chunk larger than the size the sender's own
/// chunker produces is not a chunk, it is somebody claiming to be one. The
/// first version bounded a chunk by [`MAX_FRAME`] instead, which is sixteen
/// times larger — and that, with an eviction rule that could never fire for a
/// single stream, let one sender hold about a gigabyte in a browser tab.
pub const MAX_CHUNK: usize = freenet_stdlib::client_api::streaming::CHUNK_SIZE;

/// The largest message this build will REASSEMBLE.
///
/// Chosen from the largest legitimate reply rather than from what the format
/// allows, which is the difference between a bound and a formality.
///
/// The biggest thing this client actually receives is an engine reply, and
/// those are bounded by `protocol::MAX_MESSAGE` — 4 MiB — before the engine
/// will encode one. Doubling it leaves room for the client-API envelope and
/// for an engine that grows its own ceiling once without this becoming the
/// thing that breaks.
///
/// stdlib's cap is 256 chunks ≈ 64 MiB, sized for a 50 MiB contract STATE.
/// This client never asks for one: it talks to a delegate, and contract state
/// reaches it as engine replies. So the tighter number is the true one, and
/// using theirs would be accepting a bound for a case we do not have.
pub const MAX_REASSEMBLED: usize = 8 * 1024 * 1024;

/// What arrived, once it has been understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Messages for the engine — our protocol, inside their envelope.
    ///
    /// A LIST, because one `DelegateResponse` can carry several application
    /// messages and each is its own message. The first version concatenated
    /// them into one buffer, which the exact decoder then refused as
    /// `TrailingBytes` — correctly: a run of messages is not a message, and
    /// a decoder that accepted the prefix would have read the first one and
    /// thrown the rest away.
    EngineBytes(Vec<Vec<u8>>),
    /// A subscribed contract changed.
    ///
    /// **A HINT, never an authority.** See the module docs: a root from the
    /// node means "look", not "this is where the tree is".
    HeadChanged { key: String },
    /// A contract GET answered (ENGINE-SHAPE §2: the page reads blocks and the
    /// head itself). `id` is the contract's INSTANCE id, 32 bytes, as the node
    /// names it: the page matches it against the head it subscribed to. For a
    /// BLOCK the id is not trusted either way — the block id is recomputed
    /// from the state (`block::block_of_state`) and a state nobody asked for
    /// verifies as nothing.
    ///
    /// ASSUMPTION (the cold read's tests decide it): a node that cannot find a
    /// contract answers with `ContractError::Get` ([`Incoming::GetFailed`]),
    /// not with an empty state. An empty `GetResponse` stays unusable, and to
    /// a cold read it is a GET that did not answer.
    Got { id: [u8; 32], state: Vec<u8> },
    /// A contract GET the node REFUSED, naming the contract (its
    /// `ContractError::Get { key, .. }`), so the page can tell which of its
    /// GETs this answers. A page GET of a block this node's own delegate wrote
    /// is refused on a node with a peer (F55) — that is how a cold read learns
    /// a root is local. A refusal that names nothing stays [`Incoming::Refused`].
    GetFailed { id: [u8; 32] },
    /// A contract PUT the node REFUSED, naming the contract (its
    /// `ContractError::Put { key, cause }`) as [`AckKind::Put`] names one it
    /// accepted — so a refusal is attributed to its PUT by key, never by
    /// position. `said` is the node's `cause`: display only.
    PutFailed { key: String, said: String },
    /// The node accepted a request, and WHICH.
    ///
    /// **An ack is not durability.** A write becomes published by being READ
    /// BACK, never by being acknowledged — and through a gateway that is a
    /// trust requirement and not only a timing one: an ack is unauthenticated,
    /// so a hostile node could acknowledge a write it never made.
    ///
    /// The kind is for diagnostics and for `provision()`, which needs to tell
    /// "the delegate registered" from "the contract went in" while doing both
    /// in one session.
    Ack(AckKind),
    /// The node refused. The reason is the NODE's word; see [`Refused`].
    Refused(Refused),
    /// Not something this build can use. Counted, never silently dropped.
    Unusable(Unusable),
    /// A chunk of a larger message; nothing to hand on yet.
    Partial,
}

/// Which request an [`Incoming::Ack`] answers — and WHICH ONE.
///
/// **Every ack that can name what it answers, does.** The first version had a
/// bare `Put`, so two contract installs both expected `Put` and either one's
/// ack completed the other: a duplicate acknowledgement of the FIRST contract
/// advanced the SECOND, and the run reported a contract installed that had
/// never been sent. "One step in flight" does not help, because the ack that
/// arrives need not be this step's.
///
/// The node names them: `DelegateResponse` carries a `DelegateKey`
/// (`client_events.rs:773`), `PutResponse` and `SubscribeResponse` carry a
/// `ContractKey`. Pairing by position when the answer names itself is the
/// defect; carrying the name is the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckKind {
    /// A delegate was registered, and which.
    Registered(String),
    /// A contract was put, and which.
    Put(String),
    /// A subscription was taken, and on what.
    Subscribed(String),
    /// A contract UPDATE was answered. It carries NO information about which
    /// record the register kept (F56): the page confirms by the SUBSCRIBED
    /// head's ledger, never by this (ENGINE-SHAPE §5, correction 4).
    Updated(String),
    /// Something that needed no answer succeeded. Names nothing because there
    /// is nothing to name — and a step must not rely on this one to identify
    /// itself.
    Ok,
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
    /// Decoded, but a kind this side has no use for. NAMED: a diagnostic
    /// that says only "not for us" costs a second run to find out which.
    UnknownKind(&'static str),
    /// The node's own error reply. A message, not a failure to read one.
    NodeSaidNo,
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

/// Frame an engine message to a delegate registered with empty parameters.
///
/// The parameters are part of what the delegate's key is DERIVED from, so
/// addressing it with different ones names a delegate that was never
/// registered. `delegate_from_code` registers with empty parameters and this
/// addresses with empty parameters; keeping the pair in one file is what
/// stops them drifting apart.
pub fn frame_engine_request(
    key: &DelegateKey,
    payload: Vec<u8>,
    stream_id: u32,
) -> Result<Vec<Vec<u8>>, String> {
    frame_delegate_op(key, &Parameters::from(vec![]), payload, stream_id)
}

/// A contract's instance id, from the 32 bytes that name it.
///
/// So a caller can hold one without depending on freenet itself — the same
/// reason `DelegateKey` is re-exported. The engine reports a head as 32
/// bytes; this is what turns those into something the client API will accept.
pub fn contract_id(bytes: [u8; 32]) -> ContractInstanceId {
    ContractInstanceId::new(bytes)
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

/// Frame a contract GET (the page reads blocks and the head itself: ENGINE-SHAPE
/// §2). `subscribe`: the HEAD is read with a subscription — a peered node serves
/// a page only what it is subscribed to or wrote (F55), and a subscription is
/// also how a head move reaches this page (§5 correction 4). A block is got
/// without one; `return_contract_code` never, the page has the code.
pub fn frame_get(id: ContractInstanceId, subscribe: bool, stream_id: u32) -> Result<Vec<Vec<u8>>, String> {
    let req = ClientRequest::ContractOp(freenet_stdlib::client_api::ContractRequest::Get {
        key: id,
        return_contract_code: false,
        subscribe,
        blocking_subscribe: false,
    });
    frames(&req, stream_id)
}

/// Frame a contract UPDATE with a whole new state — the head register's signed
/// record (§5: `Signed` → send UPDATE). An UPDATE names the contract by its
/// full KEY (code hash included), not by instance id. An UPDATE of an identical
/// record is the same record, so a silent one is re-sent as it is.
pub fn frame_update(key: ContractKey, state: Vec<u8>, stream_id: u32) -> Result<Vec<Vec<u8>>, String> {
    let req = ClientRequest::ContractOp(freenet_stdlib::client_api::ContractRequest::Update {
        key,
        data: UpdateData::State(State::from(state)),
    });
    frames(&req, stream_id)
}

/// The delegate, and the key the node will know it by, from its wasm.
///
/// Both come from the same bytes and the key is DERIVED — a caller that
/// carried a key alongside the code could hold one that names a different
/// build, and would then address requests to a delegate that is not the one
/// it registered.
pub fn delegate_from_code(wasm: &[u8]) -> (DelegateContainer, DelegateKey) {
    let d = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(Delegate::from((
        &DelegateCode::from(wasm.to_vec()),
        &Parameters::from(vec![]),
    ))));
    let key = d.key().clone();
    (d, key)
}

/// The Register contract's parameters for a head signed by `verifying_key`.
///
/// **One copy.** The format is `RG01`, a version byte, the 32-byte verifying
/// key, then the record's name — and the contract INSTANCE is derived from
/// it, so a caller that lays the bytes out differently addresses a different
/// contract and finds an empty head rather than an error. It was written out
/// by hand in the live driver and would have been written out again in the
/// page; a second copy is a silent fork of an id.
pub fn register_params(verifying_key: &[u8; 32], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::from(*signer_proto::head::RECORD_MAGIC);
    p.push(0u8);
    p.extend_from_slice(verifying_key);
    p.extend_from_slice(name);
    p
}

/// The name the head record is kept under.
pub const HEAD_NAME: &[u8] = b"head";

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
/// Refuses a host that is not loopback, because `ws://` is cleartext.
///
/// Reaching a node on another machine over an unencrypted socket would put
/// every request and every reply — and the traffic pattern of a person's whole
/// session — on the wire in the clear. Whether that ever happens, and over
/// what, is a decision nobody has taken; until they have, building the URL is
/// how it happens by accident. A refusal here is a compile-time-shaped
/// problem rather than a silent downgrade.
pub fn ws_url(host: &str, port: u16) -> Result<String, String> {
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]");
    if !loopback {
        return Err(format!(
            "refusing to build a cleartext ws:// URL for `{host}`: only \
             loopback is allowed until transport security for a remote node \
             has been decided"
        ));
    }
    Ok(format!(
        "ws://{host}:{port}/v1/contract/command?encodingProtocol=native"
    ))
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
    let decoded = match decode_one(bytes) {
        Ok(d) => d,
        Err(other) => return other,
    };
    let whole = match decoded {
        HostResponse::StreamChunk {
            stream_id,
            index,
            total,
            data,
        } => match r.chunk(stream_id, index, total, data.to_vec()) {
            Ok(None) => return Incoming::Partial,
            // THE SAME FUNCTION, because this is the same check at a second
            // door. The first version decoded here with a different arm, so a
            // node error arriving UNCHUNKED became `Refused` and the identical
            // error arriving CHUNKED became `Unusable` — one fact, two
            // answers, decided by a detail of transport the sender picks.
            Ok(Some(bytes)) => match decode_one(&bytes) {
                Ok(d) => d,
                Err(other) => return other,
            },
            Err(why) => return Incoming::Unusable(why),
        },
        other => other,
    };
    classify(whole)
}

/// Decode one complete message, turning a node's own error into the message
/// it is.
///
/// One function, called at BOTH doors — unchunked and reassembled — so the
/// same bytes cannot mean two different things depending on how they arrived.
fn decode_one(bytes: &[u8]) -> Result<HostResponse, Incoming> {
    match Reassembler::decode(bytes) {
        Ok(d) => Ok(d),
        // A GET refusal that names its contract: which GET it answers.
        Err(Unusable::NodeSaidNo) if get_refused(bytes).is_some() => {
            Err(Incoming::GetFailed { id: get_refused(bytes).expect("just checked") })
        }
        // A PUT refusal that names its contract: which PUT it answers.
        Err(Unusable::NodeSaidNo) if put_refused(bytes).is_some() => {
            let (key, said) = put_refused(bytes).expect("just checked");
            Err(Incoming::PutFailed { key, said })
        }
        // A node's own error reply is a MESSAGE, not a failure to read one.
        Err(Unusable::NodeSaidNo) => Err(Incoming::Refused(Refused {
            said: "the node refused the request".into(),
        })),
        Err(why) => Err(Incoming::Unusable(why)),
    }
}

fn classify(r: HostResponse) -> Incoming {
    use freenet_stdlib::client_api::ContractResponse;
    match r {
        HostResponse::DelegateResponse { key, values } => {
            let mut out: Vec<Vec<u8>> = Vec::new();
            for v in values {
                if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                    out.push(m.payload.to_vec());
                }
            }
            if out.is_empty() {
                // A delegate response carrying no application message is the
                // node ACKNOWLEDGING — RegisterDelegate answers this way. The
                // first version called it unusable, which turned the ordinary
                // reply to the first message of every session into an error.
                Incoming::Ack(AckKind::Registered(key.to_string()))
            } else {
                Incoming::EngineBytes(out)
            }
        }
        // A GET answered with NO state is not a block (no kind byte) and not a
        // head record: unusable, as every GET answer was before the page read
        // blocks itself — a run of zeroes decodes as exactly this. ASSUMPTION
        // (the live run tells): a node that cannot find a contract answers
        // with an error, not with an empty state; to a cold read an empty one
        // is a GET that did not answer, and is re-fetched.
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) if state.as_ref().is_empty() => {
            Incoming::Unusable(Unusable::UnknownKind("ContractResponse::GetResponse (no state)"))
        }
        HostResponse::ContractResponse(ContractResponse::GetResponse { key, state, .. }) => {
            let mut id = [0u8; 32];
            id.copy_from_slice(&key.id().as_bytes()[..32]);
            Incoming::Got { id, state: state.as_ref().to_vec() }
        }
        HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
            Incoming::Ack(AckKind::Updated(key.to_string()))
        }
        HostResponse::ContractResponse(ContractResponse::UpdateNotification { key, .. }) => {
            Incoming::HeadChanged {
                key: key.to_string(),
            }
        }
        HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
            Incoming::Ack(AckKind::Put(key.to_string()))
        }
        HostResponse::ContractResponse(ContractResponse::SubscribeResponse { key, .. }) => {
            Incoming::Ack(AckKind::Subscribed(key.to_string()))
        }
        // 0.2.136 answers a GET of a contract that exists nowhere with an
        // explicit NotFound ("after exhaustive search"), not an error: the
        // same fact as a `ContractError::Get` refusal, so the same message.
        // (Seen live: the head Register's first read, before its first PUT.)
        HostResponse::ContractResponse(ContractResponse::NotFound { instance_id }) => {
            let mut id = [0u8; 32];
            id.copy_from_slice(&instance_id.as_bytes()[..32]);
            Incoming::GetFailed { id }
        }
        HostResponse::Ok => Incoming::Ack(AckKind::Ok),
        // A kind this build has no use for. NAMED, so a failure says which:
        // the first run of the live driver reported `NotForUs` and could have
        // meant either "the node refused" or "a variant we do not handle",
        // and those want opposite responses.
        other => Incoming::Unusable(Unusable::UnknownKind(kind_of(&other))),
    }
}

/// The variant's own name, for a diagnostic that does not need a second run.
fn kind_of(r: &HostResponse) -> &'static str {
    use freenet_stdlib::client_api::ContractResponse as C;
    match r {
        HostResponse::ContractResponse(C::GetResponse { .. }) => "ContractResponse::GetResponse",
        HostResponse::ContractResponse(C::UpdateResponse { .. }) => {
            "ContractResponse::UpdateResponse"
        }
        HostResponse::ContractResponse(_) => "ContractResponse(other)",
        HostResponse::DelegateResponse { .. } => "DelegateResponse",
        HostResponse::Ok => "Ok",
        _ => "another kind",
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

/// The contract a node's GET refusal names, if the bytes are one.
fn get_refused(bytes: &[u8]) -> Option<[u8; 32]> {
    use freenet_stdlib::client_api::{ClientError, ContractError, ErrorKind, RequestError};
    let Ok(Err(e)) = bincode::deserialize::<Result<HostResponse, ClientError>>(bytes) else {
        return None;
    };
    match e.kind() {
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Get { key, .. })) => {
            let mut id = [0u8; 32];
            id.copy_from_slice(&key.id().as_bytes()[..32]);
            Some(id)
        }
        _ => None,
    }
}

/// The contract (named as [`AckKind::Put`] names it) and the node's cause, if
/// the bytes are a PUT refusal.
fn put_refused(bytes: &[u8]) -> Option<(String, String)> {
    use freenet_stdlib::client_api::{ClientError, ContractError, ErrorKind, RequestError};
    let Ok(Err(e)) = bincode::deserialize::<Result<HostResponse, ClientError>>(bytes) else {
        return None;
    };
    match e.kind() {
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Put { key, cause })) => {
            Some((key.to_string(), cause.to_string()))
        }
        _ => None,
    }
}

/// Handles for tests that need to build what a node would send.
///
/// Not a general surface: the alternative is a test that constructs the bytes
/// by hand and agrees with its own idea of the format, which is the drift this
/// crate exists to prevent.
#[doc(hidden)]
pub mod _test {
    pub use freenet_stdlib::client_api::ClientError as Err;
    pub use freenet_stdlib::client_api::HostResponse as Host;

    pub fn client_error(msg: &str) -> Err {
        Err::from(freenet_stdlib::client_api::ErrorKind::Unhandled {
            cause: msg.to_string().into(),
        })
    }

    /// Run a COMPLETE message through the same path a reassembled one takes.
    pub fn classify_for_test(bytes: &[u8]) -> super::Incoming {
        match super::decode_one(bytes) {
            Ok(d) => super::classify(d),
            Err(other) => other,
        }
    }
}
