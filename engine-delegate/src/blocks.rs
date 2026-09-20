//! The node's contract store, as the engine reads it.
//!
//! F14: `get_contract_state` is a SYNCHRONOUS host call returning whatever
//! the node happens to hold at that instant. It is the warm set — the engine
//! owns no block bytes — and it is allowed to answer `None` for something it
//! answered a moment ago, because F33 says a sync read does not refresh
//! hosting. Everything above this treats a miss as a fetch, never as an
//! error.

use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use freenet_stdlib::prelude::DelegateCtx;
use std::cell::RefCell;
use std::collections::BTreeMap;

/// Reads through to the node, and keeps what it read for the rest of the call.
///
/// `Blocks::get` hands out `Option<&[u8]>` borrowed from `&self`, while the
/// host call returns an owned `Vec`. The bytes are therefore leaked, which is
/// correct HERE and nowhere else: a delegate's linear memory is discarded
/// when `process()` returns (F32), so the arena is the call and the leak is
/// freed by the platform. A long-lived process doing this would be a bug.
///
/// The cache is also what keeps a descent honest: without it, reading one
/// node twice in a call could give two different answers, and the tree would
/// appear to change under the apply.
/// The contract instance a block lives in.
///
/// A block's cid is the Block contract's PARAMS — `blake3(kind ‖ body)` — and
/// the instance id is derived by the node from the code AND the params, so
/// the two are different 32-byte values.
pub fn contract_for(code: &[u8], cid: &Cid) -> [u8; 32] {
    use freenet_stdlib::prelude::*;
    let c = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(code.to_vec())),
        Parameters::from(cid.to_vec()),
    )));
    let mut out = [0u8; 32];
    out.copy_from_slice(&c.key().id().as_bytes()[..32]);
    out
}

pub struct NodeBlocks<'a> {
    /// Borrowed immutably: `get_contract_state` takes `&self`, so the ctx is
    /// still free to be written at the end of the call. A `&mut` here would
    /// hold the borrow for the shell's whole lifetime and leave nowhere to
    /// save the context from.
    ctx: &'a DelegateCtx,
    /// The Block contract's code, needed to name the contract a block is in.
    code: Option<Vec<u8>>,
    seen: RefCell<BTreeMap<Cid, Option<&'static [u8]>>>,
    /// Sync reads made, so a call can report what it cost.
    reads: RefCell<usize>,
}

impl<'a> NodeBlocks<'a> {
    pub fn new(ctx: &'a DelegateCtx) -> Self {
        Self::with_code(ctx, None)
    }

    pub fn with_code(ctx: &'a DelegateCtx, code: Option<Vec<u8>>) -> Self {
        NodeBlocks {
            ctx,
            code,
            seen: RefCell::new(BTreeMap::new()),
            reads: RefCell::new(0),
        }
    }

    pub fn reads(&self) -> usize {
        *self.reads.borrow()
    }
}

impl Blocks for NodeBlocks<'_> {
    fn get(&self, cid: &Cid) -> Option<&[u8]> {
        if let Some(hit) = self.seen.borrow().get(cid) {
            return *hit;
        }
        *self.reads.borrow_mut() += 1;
        // The engine names a BLOCK; the node names a CONTRACT. A block's cid
        // is that contract's PARAMS, so the instance id is derived from the
        // cid and the Block code together — never the cid itself. Passing the
        // cid straight in asks for a contract that does not exist, and the
        // node answers None to every block for ever: the engine then sees a
        // tree it cannot read, which is indistinguishable from a node that
        // holds nothing.
        let Some(code) = self.code.as_deref() else {
            self.seen.borrow_mut().insert(*cid, None);
            return None;
        };
        let got = self.ctx.get_contract_state(&contract_for(code, cid));
        // The contract's state is `kind ‖ body`; the engine wants the body.
        let leaked: Option<&'static [u8]> = got.and_then(|v| {
            let body = v.get(1..)?.to_vec();
            Some(&*Box::leak(body.into_boxed_slice()) as &'static [u8])
        });
        self.seen.borrow_mut().insert(*cid, leaked);
        leaked
    }
}
