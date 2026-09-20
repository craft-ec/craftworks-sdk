//! A delegate that answers questions about the platform, and nothing else.
//!
//! Every question here is one the shell's design depends on and that no
//! amount of reading settles: whether a state this node just accepted is
//! readable synchronously, how quickly, what a call costs, and what the
//! secret store will take. It is deliberately tiny — five answers, not a
//! framework.
//!
//! It imports ONLY host functions the node actually defines. `freenet-stdlib`
//! 0.10.0 declares seven the 0.2.135 node does not, and a call to any of them
//! that survives optimisation puts an import in the module that the node
//! refuses at INSTANTIATION — so the delegate would not load at all, for
//! every message, not just the one that took that branch.

use freenet_stdlib::prelude::*;
use serde::{Deserialize, Serialize};

/// A counter in guest memory. If wasm memory persisted between `process()`
/// calls this would climb; the source says it will not, and this is the
/// cheapest possible confirmation from the other side.
static mut CALLS: u32 = 0;

#[derive(Debug, Serialize, Deserialize)]
pub enum Ask {
    /// Is this contract's state readable synchronously, right now?
    ReadState { id: [u8; 32] },
    /// Write a secret of `len` bytes under `key`.
    SetSecret { key: Vec<u8>, len: usize },
    /// Read it back.
    GetSecret { key: Vec<u8> },
    /// Nothing at all: the floor for what a call costs.
    Nothing,
    /// What the node said about the last `PutContractRequest`.
    ///
    /// The node answers a delegate PUT with a `PutContractResponse` carrying
    /// its verdict. Dropping it left "the state is not readable afterwards"
    /// with two possible causes -- refused, or accepted and not visible --
    /// and no way to tell them apart. This reads the node's own word for it.
    LastPut,
    /// Emit a PutContractRequest and see whether the engine's own writes end
    /// up hosted — the question source could not settle (Q8). The CODE comes
    /// from the driver because a delegate cannot fabricate a contract.
    PutState {
        code: Vec<u8>,
        params: Vec<u8>,
        state: Vec<u8>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Said {
    State {
        /// `None` = this node does not hold it (or does not hold its CODE).
        len: Option<usize>,
        /// First 8 bytes, so the caller can tell WHICH state it read.
        head: Vec<u8>,
    },
    Secret {
        ok: bool,
        len: Option<usize>,
    },
    Nothing {
        /// Calls seen by THIS wasm instance. Always 1 if memory is fresh per
        /// call; climbing if it is not.
        calls_in_memory: u32,
        /// Calls recorded in the context, which does persist between calls.
        calls_in_context: u32,
    },
    /// The delegate built and emitted a `PutContractRequest`.
    ///
    /// It says only that THIS side ran: whether the node accepted the put is
    /// a separate question the driver asks by reading the state back. Without
    /// it a delegate that answers nothing at all is indistinguishable from a
    /// message the node dropped, which is how Q8's first answer came to mean
    /// nothing.
    Put {
        emitted: bool,
    },
    /// The node's verdict on the last delegate PUT, as it gave it.
    PutResult {
        /// `None` = no response has arrived yet.
        ok: Option<bool>,
        err: String,
    },
}

/// The context, as this delegate lays it out.
///
/// `[0..4]` the call counter, `[4]` the last put's verdict
/// (0 none / 1 ok / 2 error), `[5..]` the error text. One layout in one place
/// because two arms write it and a disagreement between them would look like
/// a node that answered something it did not.
fn ctx_get(ctx: &mut DelegateCtx) -> (u32, u8, String) {
    let b = ctx.read().to_vec();
    let calls = u32::from_le_bytes(b.first_chunk::<4>().copied().unwrap_or([0; 4]));
    let verdict = b.get(4).copied().unwrap_or(0);
    let err = b
        .get(5..)
        .map(|e| String::from_utf8_lossy(e).into_owned())
        .unwrap_or_default();
    (calls, verdict, err)
}

fn ctx_put(ctx: &mut DelegateCtx, calls: u32, verdict: u8, err: &str) {
    let mut out = Vec::with_capacity(5 + err.len());
    out.extend_from_slice(&calls.to_le_bytes());
    out.push(verdict);
    out.extend_from_slice(err.as_bytes());
    ctx.write(&out);
}

/// Where the last delegate PUT's verdict is kept.
const PUT_VERDICT: &[u8] = b"put_verdict";

struct Probe;

#[delegate]
impl DelegateInterface for Probe {
    fn process(
        ctx: &mut DelegateCtx,
        _params: Parameters<'static>,
        _origin: Option<MessageOrigin>,
        inbound: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        // The node's verdict on a delegate PUT arrives as its own inbound
        // message. Recorded in the context, which is the only thing that
        // survives to the call that asks for it.
        if let InboundDelegateMsg::PutContractResponse(r) = &inbound {
            // Recorded in a SECRET, not the context.
            //
            // This is the whole finding of the differential against F21's
            // working probe: a context write made during THIS invocation does
            // not survive to the next client call, because the node calls the
            // delegate with a response as an INNER run whose context does not
            // come back. Secrets do survive -- measured here in (5), across a
            // node restart -- so the known-working instrument records there,
            // and reading the verdict out of the context reported `None` for
            // a response that had in fact arrived.
            let (calls, _, _) = ctx_get(ctx);
            let said = match &r.result {
                Ok(()) => {
                    ctx.set_secret(PUT_VERDICT, &[1]);
                    ctx_put(ctx, calls, 1, "");
                    Said::PutResult {
                        ok: Some(true),
                        err: String::new(),
                    }
                }
                Err(e) => {
                    let mut v = vec![2u8];
                    v.extend_from_slice(e.as_bytes());
                    ctx.set_secret(PUT_VERDICT, &v);
                    ctx_put(ctx, calls, 2, e);
                    Said::PutResult {
                        ok: Some(false),
                        err: e.clone(),
                    }
                }
            };
            // Answered OUT OF BAND as well as recorded. The context write
            // alone cannot distinguish "the node never called us with a
            // response" from "it did, and the write did not survive an inner
            // invocation" — and those are different facts about the platform.
            let payload =
                bincode::serialize(&said).map_err(|e| DelegateError::Other(e.to_string()))?;
            return Ok(vec![OutboundDelegateMsg::ApplicationMessage(
                ApplicationMessage::new(payload).processed(true),
            )]);
        }
        let InboundDelegateMsg::ApplicationMessage(msg) = inbound else {
            return Ok(vec![]);
        };
        // Anything unparseable is dropped, not a panic: the node hands a
        // delegate whatever arrives.
        let Ok(ask) = bincode::deserialize::<Ask>(msg.payload.as_slice()) else {
            return Ok(vec![]);
        };

        let in_memory = unsafe {
            CALLS += 1;
            CALLS
        };

        let said = match ask {
            Ask::ReadState { id } => match ctx.get_contract_state(&id) {
                Some(state) => Said::State {
                    len: Some(state.len()),
                    head: state.iter().take(8).copied().collect(),
                },
                None => Said::State {
                    len: None,
                    head: Vec::new(),
                },
            },
            Ask::SetSecret { key, len } => {
                let value = vec![0xABu8; len];
                Said::Secret {
                    ok: ctx.set_secret(&key, &value),
                    len: Some(len),
                }
            }
            Ask::GetSecret { key } => {
                let got = ctx.get_secret(&key);
                Said::Secret {
                    ok: got.is_some(),
                    len: got.map(|v| v.len()),
                }
            }
            Ask::PutState {
                code,
                params,
                state,
            } => {
                // The delegate has no host function that writes contract
                // state — those were removed — so a write is a MESSAGE.
                let container =
                    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
                        std::sync::Arc::new(ContractCode::from(code)),
                        // The PARAMS matter: the Block contract validates
                        // `params == blake3(state)`, so an empty-params
                        // container is a state the node correctly refuses —
                        // which looked exactly like "a delegate PUT never
                        // lands" until this probe was fixed.
                        Parameters::from(params),
                    )));
                let req = PutContractRequest::new(
                    container,
                    WrappedState::new(state),
                    RelatedContracts::default(),
                );
                // BOTH: the request for the node, and a reply for the driver.
                // A delegate that emits only the request cannot be told apart
                // from one whose message never arrived.
                let payload = bincode::serialize(&Said::Put { emitted: true })
                    .map_err(|e| DelegateError::Other(e.to_string()))?;
                return Ok(vec![
                    OutboundDelegateMsg::PutContractRequest(req),
                    OutboundDelegateMsg::ApplicationMessage(
                        ApplicationMessage::new(payload).processed(true),
                    ),
                ]);
            }
            Ask::LastPut => {
                // The secret first: it is the one that survives an inner
                // invocation. The context is read too, and the driver prints
                // both, because the DIFFERENCE between them is the platform
                // fact -- a response recorded in a context that is then thrown
                // away looks exactly like a response that never came.
                let from_secret = ctx.get_secret(PUT_VERDICT);
                let (_, ctx_verdict, ctx_err) = ctx_get(ctx);
                match from_secret {
                    Some(v) => Said::PutResult {
                        ok: Some(v.first() == Some(&1)),
                        err: String::from_utf8_lossy(v.get(1..).unwrap_or(&[])).into_owned(),
                    },
                    None => Said::PutResult {
                        ok: match ctx_verdict {
                            1 => Some(true),
                            2 => Some(false),
                            _ => None,
                        },
                        err: ctx_err,
                    },
                }
            }
            Ask::Nothing => {
                // The context is the only thing that persists between calls,
                // so the count lives there.
                let (mut n, v, e) = ctx_get(ctx);
                n += 1;
                ctx_put(ctx, n, v, &e);
                Said::Nothing {
                    calls_in_memory: in_memory,
                    calls_in_context: n,
                }
            }
        };

        let payload = bincode::serialize(&said).map_err(|e| DelegateError::Other(e.to_string()))?;
        Ok(vec![OutboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(payload).processed(true),
        )])
    }
}
