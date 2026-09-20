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
    /// Emit a PutContractRequest and see whether the engine's own writes end
    /// up hosted — the question source could not settle (Q8). The CODE comes
    /// from the driver because a delegate cannot fabricate a contract.
    PutState { code: Vec<u8>, state: Vec<u8> },
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
}

struct Probe;

#[delegate]
impl DelegateInterface for Probe {
    fn process(
        ctx: &mut DelegateCtx,
        _params: Parameters<'static>,
        _origin: Option<MessageOrigin>,
        inbound: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
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
            Ask::PutState { code, state } => {
                // The delegate has no host function that writes contract
                // state — those were removed — so a write is a MESSAGE.
                let container =
                    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
                        std::sync::Arc::new(ContractCode::from(code)),
                        vec![].into(),
                    )));
                let req = PutContractRequest::new(
                    container,
                    WrappedState::new(state),
                    RelatedContracts::default(),
                );
                return Ok(vec![OutboundDelegateMsg::PutContractRequest(req)]);
            }
            Ask::Nothing => {
                // The context is the only thing that persists between calls,
                // so the count lives there.
                let mut n =
                    u32::from_le_bytes(ctx.read().first_chunk::<4>().copied().unwrap_or([0; 4]));
                n += 1;
                ctx.write(&n.to_le_bytes());
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
