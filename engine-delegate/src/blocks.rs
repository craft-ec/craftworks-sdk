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
pub struct NodeBlocks<'a> {
    ctx: RefCell<&'a mut DelegateCtx>,
    seen: RefCell<BTreeMap<Cid, Option<&'static [u8]>>>,
    /// Sync reads made, so a call can report what it cost.
    reads: RefCell<usize>,
}

impl<'a> NodeBlocks<'a> {
    pub fn new(ctx: &'a mut DelegateCtx) -> Self {
        NodeBlocks {
            ctx: RefCell::new(ctx),
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
        // The Block contract's parameters are the hash of its state, so a
        // block's id IS its contract instance id. That equality is what lets
        // the engine name a block and the node find a contract.
        let got = self.ctx.borrow_mut().get_contract_state(cid);
        let leaked: Option<&'static [u8]> = got.map(|v| &*Box::leak(v.into_boxed_slice()));
        self.seen.borrow_mut().insert(*cid, leaked);
        leaked
    }
}
