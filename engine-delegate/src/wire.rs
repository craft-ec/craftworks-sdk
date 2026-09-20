//! What a client says to the shell, and what it hears back.
//!
//! A delegate's only channel to a client is an opaque application message, so
//! this is a wire format and is treated as one: everything arriving is
//! untrusted bytes, anything that does not parse is DROPPED and COUNTED, and
//! nothing here can panic on input.

use engine::read::{ReadResult, ReqId};
use engine::{ClientId, Epoch, Op, State, WriteId};
use freenet_prolly::Cid;

/// What a client asks the engine to do.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum Request {
    /// Give the delegate the contract code it will write with.
    ///
    /// A delegate CANNOT fabricate a contract — it has no way to produce
    /// wasm — so the code must arrive from outside, once, and be kept. It
    /// goes in the SECRET store rather than the context: secrets are on disk
    /// and survive a node restart (measured), while the context is process
    /// memory with a ten-minute TTL. Code that had to be re-sent after every
    /// restart would make the delegate useless exactly when it is left alone.
    Install { block_code: Vec<u8> },
    /// Begin. The shell answers by reading the head.
    Start { epochs: Vec<u32> },
    Write {
        client: u64,
        write_id: u64,
        ops: Vec<(Vec<u8>, Op)>,
    },
    Get {
        client: u64,
        req_id: u64,
        key: Vec<u8>,
    },
    /// A clock beat from a connected tab. The only source of time there is
    /// (W4), and it may only decide WHEN, never WHAT.
    Tick { now: u64 },
    /// The tab is closing. Ship what is waiting; no tick is ever coming.
    Flush,
    /// What happened to this write? Answered `Lost` if the engine has no
    /// record, which is the word that lets a client safely re-submit.
    AskWrite { client: u64, write_id: u64 },
}

/// What the shell tells a client.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum Reply {
    Write {
        client: u64,
        write_id: u64,
        state: State,
    },
    Read {
        client: u64,
        req_id: u64,
        result: ReadResult,
    },
    /// Counted, not silent: what arrived that the shell could not use.
    ///
    /// A shell that dropped these quietly would be indistinguishable from one
    /// whose client is talking to it in a format it does not understand, and
    /// the client would wait for ever for a reply to a message that was never
    /// read.
    Dropped { reason: Dropped, count: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Dropped {
    /// Not a `Request` this build can read.
    Unparseable,
    /// A response naming a contract the shell is not waiting on.
    UnknownContract,
    /// An inbound message kind the shell has no use for.
    NotForUs,
}

/// Decode a client request. `None` is a drop, never a panic.
pub fn request(bytes: &[u8]) -> Option<Request> {
    bincode::deserialize(bytes).ok()
}

pub fn reply(r: &Reply) -> Vec<u8> {
    bincode::serialize(r).unwrap_or_default()
}

/// The engine's ids, from the wire's plain numbers.
pub fn client(n: u64) -> ClientId {
    ClientId(n)
}
pub fn write_id(n: u64) -> WriteId {
    WriteId(n)
}
pub fn req_id(n: u64) -> ReqId {
    ReqId(n)
}
pub fn epoch(n: u32) -> Epoch {
    Epoch(n)
}

/// A contract instance id, as the node names it, from a block id.
///
/// They are both 32 bytes and they are NOT the same thing: a block id is the
/// hash of the block, and a contract id is derived from code and parameters.
/// The Block contract's parameters are the hash of its state, which is what
/// makes the two line up for the engine's blocks — an equality this shell
/// relies on and therefore states.
pub fn contract_id_of_block(cid: &Cid) -> [u8; 32] {
    *cid
}
