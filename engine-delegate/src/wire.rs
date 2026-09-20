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
    /// Give the delegate everything it cannot produce for itself.
    ///
    /// A delegate cannot fabricate a contract — it has no way to produce
    /// wasm — and it cannot mint authority. Three things therefore arrive
    /// from outside, once, and are kept in the SECRET store: secrets are on
    /// disk and survive a node restart (measured), while the context is
    /// process memory with a ten-minute TTL. Anything that had to be re-sent
    /// after every restart would make the delegate useless exactly when it is
    /// left alone, which is the case it exists for.
    ///
    /// The delegate DERIVES NOTHING from any of it. A Register record must be
    /// signed, so writing a head at all needs a signing key; where a real
    /// device key comes from is sdk#14's question, and `KeySource` is the
    /// parameter that lets its answer slot in without the shell changing.
    Install {
        /// The Block contract, which carries the engine's blocks.
        block_code: Vec<u8>,
        /// The Register contract, which carries the head.
        register_code: Vec<u8>,
        /// The Register's parameters: the writer keyset it is created under.
        /// Supplied, never derived.
        register_params: Vec<u8>,
        /// The device signing key. TEST ONLY today: generated per run by the
        /// driver, never read from disk, removed at the end of the run.
        signing_key: TestKey,
    },
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

/// What kind of message a call was woken by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Saw {
    Client,
    GetResponse,
    PutResponse,
    UpdateResponse,
    Other,
}

/// A signing key that is not a real one, and says so in its own type.
///
/// A plain `Vec<u8>` would let a real key be passed to this path by mistake
/// and nothing would notice. Naming it makes the log line, the wire format
/// and every function signature that touches it say TEST — which is the
/// point: the danger is not that a test key leaks, it is that a real one
/// arrives here.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TestKey(pub Vec<u8>);

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
    /// What one `process()` call did.
    ///
    /// Not decoration: a delegate is invisible from outside — it has no log a
    /// client can read and the node prints nothing about it — so without this
    /// the only observable is whether the thing a caller wanted happened, and
    /// a break anywhere in the chain looks the same from the outside. Every
    /// field is a count the shell already had.
    Call {
        /// What the shell was asked to do this call.
        saw: Saw,
        /// Node operations issued.
        ops: usize,
        /// Effects still queued at the end, and therefore LOST.
        stranded: usize,
        /// Puts dropped for want of contract code.
        no_code: usize,
        /// Inbound messages the shell could not use.
        dropped: usize,
        /// Blocks put and not yet read back.
        awaiting: usize,
        /// Bytes the head bump cost, by route. A PUT carries the Register's
        /// CODE (~157 KiB) as well as the record; an UPDATE carries only the
        /// record, because the node already has the contract.
        head_put: usize,
        head_update: usize,
        /// Puts confirmed by reading them back this call.
        read_back: usize,
        /// Effects the CORE returned this call.
        effects: usize,
        /// What the node said, when it said anything; otherwise what this
        /// call did that is worth a word. Labelled at the point it is
        /// printed, because "the node said" and "the shell noticed" are
        /// different claims and only one of them is evidence about the node.
        note: String,
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
