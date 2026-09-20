//! The delegate the node loads.
//!
//! Nothing but conversion: the platform's types into [`crate::shell`]'s and
//! back. Every rule worth testing lives in the shell, where it is tested
//! without wasm and without a node — what remains here is the part a test
//! could only check by running the thing, and it is kept small for that
//! reason.

use crate::blocks::NodeBlocks;
use crate::schedule::Op;
use crate::shell::{Inbound, Shell};
use engine::Params;
use freenet_stdlib::prelude::*;

pub struct EngineDelegate;

/// A block id IS a contract instance id for the Block contract, whose
/// parameters are the hash of its state. The engine names blocks; the node
/// names contracts; this is where the two are the same 32 bytes.
fn instance(id: &[u8; 32]) -> ContractInstanceId {
    ContractInstanceId::new(*id)
}

#[delegate]
impl DelegateInterface for EngineDelegate {
    fn process(
        ctx: &mut DelegateCtx,
        _params: Parameters<'static>,
        _origin: Option<MessageOrigin>,
        inbound: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        // The context is read BEFORE the blocks borrow the ctx, and written
        // after they are dropped.
        let carried = ctx.read();

        let msg = match &inbound {
            InboundDelegateMsg::ApplicationMessage(m) => Inbound::Client(m.payload.to_vec()),
            InboundDelegateMsg::GetContractResponse(r) => {
                let mut id32 = [0u8; 32];
                id32.copy_from_slice(&r.contract_id.as_bytes()[..32]);
                Inbound::GotState {
                    id: id32,
                    bytes: r.state.as_ref().map(|s| s.as_ref().to_vec()),
                }
            }
            InboundDelegateMsg::PutContractResponse(r) => {
                let mut id32 = [0u8; 32];
                id32.copy_from_slice(&r.contract_id.as_bytes()[..32]);
                Inbound::PutAcked {
                    id: id32,
                    ok: r.result.is_ok(),
                }
            }
            _ => Inbound::Other,
        };

        let (out, saved) = {
            let blocks = NodeBlocks::new(ctx);
            let mut shell = Shell::resume(&carried, Params::default(), blocks);
            let out = shell.handle(vec![msg]);
            (out, shell.to_context())
        };

        // A context that cannot be written is not an error to report: the
        // next call starts fresh, re-reads the head, and reports whatever was
        // in flight `Lost`. Failing the call instead would turn a recoverable
        // state into a refused message.
        if let Some(c) = saved {
            ctx.write(&c);
        }

        let mut msgs: Vec<OutboundDelegateMsg> = Vec::new();
        for op in out.ops {
            match op {
                Op::Get { id, .. } => {
                    msgs.push(OutboundDelegateMsg::GetContractRequest(
                        GetContractRequest::new(instance(&id)),
                    ));
                }
                Op::Put { id, bytes } => {
                    let _ = id;
                    // The contract carrying a block: its parameters are the
                    // hash of the state, which is what makes the id the block
                    // id the engine asked for.
                    let params = Parameters::from(blake3_of(&bytes));
                    let container =
                        ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
                            std::sync::Arc::new(ContractCode::from(block_code())),
                            params,
                        )));
                    msgs.push(OutboundDelegateMsg::PutContractRequest(
                        PutContractRequest::new(
                            container,
                            WrappedState::new(bytes),
                            RelatedContracts::default(),
                        ),
                    ));
                }
                // The head lives in the Register contract, which slice 4's
                // live run wires up; until then it is not emitted here.
                Op::Head { .. } | Op::ReadHead { .. } => {}
            }
        }
        for r in out.replies {
            msgs.push(OutboundDelegateMsg::ApplicationMessage(
                ApplicationMessage::new(r).processed(true),
            ));
        }
        Ok(msgs)
    }
}

/// The Block contract's code, supplied by whoever registers the delegate.
///
/// A delegate cannot fabricate a contract, so the code must come from
/// outside. Until the registration path carries it (slice 4's live run), this
/// is empty and a PUT built on it is refused by the node — which is the
/// honest failure: better a refusal than a contract nobody can validate.
fn block_code() -> Vec<u8> {
    Vec::new()
}

fn blake3_of(bytes: &[u8]) -> Vec<u8> {
    freenet_prolly::block_id(freenet_prolly::kind::RAW, bytes).to_vec()
}
