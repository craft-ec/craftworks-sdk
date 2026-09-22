//! PUTs of contracts the APP names — code, params and state it chose — and
//! what the node said about each.
//!
//! The builder publishes a web container this way (builder#104): the SDK
//! frames it and learns the outcome, and the app never touches the client API.
//!
//! **Matched by the KEY the answer names, never by position.** Acks and
//! refusals arrive on one connection with no correlation id, beside the
//! engine's own PUTs; pairing by order would let a block's ack confirm the
//! app's container. A key this does not know is NOT consumed, so the caller's
//! other paths still see it.
//!
//! **An ack is not durability** (see [`crate::Incoming::Ack`]): `Put` means the
//! node accepted it, not that anyone can read it. A publisher that must know
//! reads it back.

use std::collections::BTreeMap;

use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractWasmAPIVersion, Parameters, WrappedContract, WrappedState,
};

/// Where one PUT stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutState {
    /// Sent, and the node has not answered.
    Pending,
    /// The node acknowledged it.
    Put,
    /// The node refused it, in its own words. Display only: nothing branches
    /// on a stranger's wording.
    Refused(String),
    /// The connection dropped while it waited. An answer sent on the old
    /// socket never arrives on the new one, so this is NOT pending — but it is
    /// not refused either: the frames may still go out on the new socket and
    /// be answered there. PUT it again to be sure (a contract's PUT of the
    /// same state is idempotent).
    Unanswered,
}

/// The contract, its key as the node NAMES it, and the state to put.
pub fn contract(code: &[u8], params: &[u8], state: &[u8]) -> (String, ContractContainer, WrappedState) {
    let c = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(code.to_vec())),
        Parameters::from(params.to_vec()),
    )));
    (c.key().id().encode(), c, WrappedState::new(state.to_vec()))
}

/// The app's PUTs, by key.
#[derive(Default, Debug)]
pub struct Puts {
    by_key: BTreeMap<String, PutState>,
}

impl Puts {
    /// A PUT of `key` went out. A re-PUT of a key already answered starts
    /// over: the new answer is the one that counts.
    pub fn begin(&mut self, key: String) {
        self.by_key.insert(key, PutState::Pending);
    }

    /// The node acknowledged a PUT of `key`. `true` iff it answers one of
    /// ours that was waiting; otherwise it is not ours and not consumed.
    pub fn acked(&mut self, key: &str) -> bool {
        self.settle(key, PutState::Put)
    }

    /// The node refused a PUT of `key`. `true` iff it answers one of ours.
    pub fn refused(&mut self, key: &str, said: &str) -> bool {
        self.settle(key, PutState::Refused(said.to_string()))
    }

    /// The socket dropped: every PUT still waiting is `Unanswered`.
    pub fn connection_lost(&mut self) {
        for s in self.by_key.values_mut() {
            if *s == PutState::Pending {
                *s = PutState::Unanswered;
            }
        }
    }

    /// Where the PUT of `key` stands; `None` if none was made.
    pub fn state(&self, key: &str) -> Option<&PutState> {
        self.by_key.get(key)
    }

    /// Only a WAITING put is settled (pending, or unanswered when the socket
    /// dropped — its frames may have gone out on the new one): a second answer
    /// for one already answered is a duplicate, and must not flip it. It is
    /// still OURS — consumed, so no other path mistakes it for its own.
    fn settle(&mut self, key: &str, to: PutState) -> bool {
        match self.by_key.get_mut(key) {
            Some(s) => {
                if matches!(s, PutState::Pending | PutState::Unanswered) {
                    *s = to;
                }
                true
            }
            None => false,
        }
    }
}
