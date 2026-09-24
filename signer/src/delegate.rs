//! The entry the node calls: `serve_full` over the real delegate context. Behind `freenet-main-delegate` (OFF
//! natively).

use freenet_stdlib::prelude::*;

struct Ctx<'a>(&'a mut DelegateCtx);

impl crate::Host for Ctx<'_> {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get_secret(key)
    }
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool {
        self.0.set_secret(key, value)
    }
    fn contract_state(&self, instance_id: &[u8; 32]) -> Option<Vec<u8>> {
        self.0.get_contract_state(instance_id)
    }
}

fn message(bytes: Vec<u8>) -> OutboundDelegateMsg {
    OutboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(bytes).processed(true))
}

pub struct Signer;

#[delegate]
impl DelegateInterface for Signer {
    fn process(
        ctx: &mut DelegateCtx,
        _params: Parameters<'static>,
        origin: Option<MessageOrigin>,
        inbound: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        match inbound {
            InboundDelegateMsg::ApplicationMessage(m) => {
                // WHO ASKED, as the node attests it (builder#117): nothing attested is the person's own tools;
                // a served web app or a delegate (possibly relaying one) is `Served`, and signs no site.
                let who = if origin.is_some() { crate::Origin::Served } else { crate::Origin::Local };
                let served = crate::serve_full(&mut Ctx(ctx), &m.payload, who);
                let mut out = vec![message(crate::reply(&served))];
                if !served.puts.is_empty() {
                    // `serve_full` checked the code is provisioned before it named a single contract.
                    let code = ctx.get_secret(crate::BLOCK_CODE).unwrap_or_default();
                    let code = std::sync::Arc::new(ContractCode::from(code));
                    for (id, state) in served.puts {
                        let contract = ContractContainer::from(ContractWasmAPIVersion::V1(
                            WrappedContract::new(code.clone(), Parameters::from(id.to_vec())),
                        ));
                        out.push(OutboundDelegateMsg::PutContractRequest(
                            PutContractRequest::new(
                                contract,
                                WrappedState::new(state),
                                RelatedContracts::default(),
                            ),
                        ));
                    }
                }
                Ok(out)
            }
            // The node's answer to one of PUT-WITH-CODE's PUTs, relayed as it comes: nothing is remembered, so it
            // cannot name the request that caused it -- it is UNATTRIBUTED and names its contract instead.
            InboundDelegateMsg::PutContractResponse(r) => {
                let mut contract = [0u8; 32];
                contract.copy_from_slice(&r.contract_id.as_bytes()[..32]);
                let (ok, note) = match &r.result {
                    Ok(_) => (true, String::new()),
                    Err(e) => (false, e.chars().take(200).collect()),
                };
                Ok(vec![message(crate::encode_answer(
                    crate::UNATTRIBUTED,
                    &crate::Answer::Put { contract, ok, note },
                ))])
            }
            // The signer issues no GET, UPDATE or SUBSCRIBE, so nothing else can answer it.
            _ => Ok(Vec::new()),
        }
    }
}
