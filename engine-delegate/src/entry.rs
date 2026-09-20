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

        let code = ctx.get_secret(BLOCK_CODE);
        let (out, saved, installing) = {
            let blocks = NodeBlocks::new(ctx);
            let mut shell = Shell::resume_with(&carried, Params::default(), blocks, code.is_some());
            let out = shell.handle(vec![msg]);
            (out, shell.to_context(), shell.installed)
        };
        // The install is the one thing the shell cannot do itself: only the
        // entry point has a secret store. Done AFTER the blocks are dropped,
        // since writing a secret needs the ctx mutably.
        if installing.is_some() {
            if let InboundDelegateMsg::ApplicationMessage(m) = &inbound {
                if let Some(crate::wire::Request::Install {
                    block_code,
                    register_code,
                    register_params,
                    signing_key,
                }) = crate::wire::request(m.payload.as_ref())
                {
                    ctx.set_secret(BLOCK_CODE, &block_code);
                    ctx.set_secret(REGISTER_CODE, &register_code);
                    ctx.set_secret(REGISTER_PARAMS, &register_params);
                    // The signing key. Stored as given and never derived
                    // from; the delegate's only use for it is to sign a
                    // Register record, and today it is a key a driver made
                    // for one run and will remove afterwards.
                    ctx.set_secret(SIGNING_KEY, &signing_key.0);
                }
            }
        }

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
                    let Some(code) = code.clone() else {
                        // The shell already dropped these; belt and braces.
                        continue;
                    };
                    // The contract carrying a block: its parameters are the
                    // hash of the state, which is what makes the id the block
                    // id the engine asked for.
                    let params = Parameters::from(blake3_of(&bytes));
                    let container = ContractContainer::from(ContractWasmAPIVersion::V1(
                        WrappedContract::new(std::sync::Arc::new(ContractCode::from(code)), params),
                    ));
                    msgs.push(OutboundDelegateMsg::PutContractRequest(
                        PutContractRequest::new(
                            container,
                            WrappedState::new(bytes),
                            RelatedContracts::default(),
                        ),
                    ));
                }
                // The head is a Register record, and writing it means
                // SIGNING it. Everything needed was provisioned by `Install`
                // and none of it is derived here.
                Op::Head { seq, root } => {
                    let (Some(rc), Some(rp), Some(sk)) = (
                        ctx.get_secret(REGISTER_CODE),
                        ctx.get_secret(REGISTER_PARAMS),
                        ctx.get_secret(SIGNING_KEY),
                    ) else {
                        continue;
                    };
                    // A head this delegate cannot sign is one the contract
                    // would refuse on arrival, and the commit would then wait
                    // for a confirmation that is never coming. Dropping it
                    // here reaches the same place without the round trip.
                    let Ok(state) = crate::register::head_state(&rp, &sk, seq, &root) else {
                        continue;
                    };
                    let container =
                        ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
                            std::sync::Arc::new(ContractCode::from(rc)),
                            Parameters::from(rp),
                        )));
                    msgs.push(OutboundDelegateMsg::PutContractRequest(
                        PutContractRequest::new(
                            container,
                            WrappedState::new(state),
                            RelatedContracts::default(),
                        ),
                    ));
                }
                // The head Register is read by its own key, which the shell
                // asks for through `Op::Get` on that contract; a bare
                // ReadHead has no block to name.
                Op::ReadHead { .. } => {}
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

/// Where the Block contract's code is kept.
///
/// The SECRET store, not the context: secrets are on disk and survive a node
/// restart (measured), while the context is process memory with a ten-minute
/// TTL. Code that had to be re-sent after every restart would make the
/// delegate useless exactly when it is left alone, which is the case it
/// exists for.
const BLOCK_CODE: &[u8] = b"block_contract_code";
/// The Register contract's code, and the keyset its instance is created
/// under. Both supplied, neither derived.
const REGISTER_CODE: &[u8] = b"register_contract_code";
const REGISTER_PARAMS: &[u8] = b"register_contract_params";
/// The device signing key, as handed in. TEST ONLY today — generated per run
/// by the driver, never read from disk, removed at the end of the run.
const SIGNING_KEY: &[u8] = b"device_signing_key";

fn blake3_of(bytes: &[u8]) -> Vec<u8> {
    freenet_prolly::block_id(freenet_prolly::kind::RAW, bytes).to_vec()
}
