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
        // WHO IS ASKING (sdk#318): only a web app the node served under its token arrives as `WebApp(id)`;
        // everything else -- a tokenless client, another delegate -- is unattested.
        let caller = match origin {
            Some(MessageOrigin::WebApp(id)) => {
                let mut app = [0u8; 32];
                app.copy_from_slice(&id.as_bytes()[..32]);
                crate::Caller::WebApp(app)
            }
            _ => crate::Caller::Unattested,
        };
        match inbound {
            InboundDelegateMsg::ApplicationMessage(m) => {
                let served = crate::serve_full(&mut Ctx(ctx), caller, &m.payload);
                Ok(vec![message(crate::reply(&served))])
            }
            // The signer issues no GET, PUT, UPDATE or SUBSCRIBE, so nothing else can answer it.
            _ => Ok(Vec::new()),
        }
    }
}
