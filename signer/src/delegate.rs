//! The entry the node calls: `serve` over the real delegate context. Behind `freenet-main-delegate` (OFF natively).

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

pub struct Signer;

#[delegate]
impl DelegateInterface for Signer {
    fn process(
        ctx: &mut DelegateCtx,
        _params: Parameters<'static>,
        _origin: Option<MessageOrigin>,
        inbound: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        // Only a client's message is served. The signer issues no contract op, so no node answer can arrive; anything
        // else is ignored rather than guessed at.
        let InboundDelegateMsg::ApplicationMessage(m) = inbound else {
            return Ok(Vec::new());
        };
        let answer = crate::serve(&mut Ctx(ctx), &m.payload);
        Ok(vec![OutboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(crate::encode_answer(&answer)).processed(true),
        )])
    }
}
