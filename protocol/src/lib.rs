//! What the SDK and the engine say to each other.
//!
//! **Every message starts with a version, from the very first one.** Not
//! because v2 is planned, but because a protocol that has to be versioned
//! LATER cannot be: the messages already in flight have nowhere to put the
//! number, and the only way to tell them apart is to guess from their shape.
//! ARCHITECTURE §19 — every published version keeps working — is a promise
//! that can only be kept by something that could always tell versions apart.
//!
//! Three rules follow, and each has a test:
//!
//! 1. A version this build does not know is answered [`Reply::Unsupported`],
//!    listing what it does know. Never silence, and never a guess at what the
//!    sender meant.
//! 2. Decoding is EXACT. `bincode::deserialize` accepts trailing bytes — it
//!    decodes the type it was asked for and ignores the rest — which is how
//!    one message is read as another. A wrapped value decoded as its wrapper
//!    returns the wrapper's first variant and throws the answer away.
//! 3. Records are append-only and unknown fields are PRESERVED byte for byte
//!    (see [`Record`]). An SDK that rewrites a record it did not fully author
//!    must not silently delete what a newer build put there.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub mod record;

/// The versions this build can serve.
///
/// A list, not a number: "the current version" is what a protocol says right
/// before it drops an old client. Serving several is the normal state.
pub const KNOWN: &[u16] = &[1];

/// The version this build SPEAKS when it starts a conversation.
pub const CURRENT: u16 = 1;

/// A client's message, with its version on the front.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Which protocol version the body is in. First field, always.
    pub version: u16,
    pub body: Request,
}

/// What a client asks the engine to do.
///
/// APPEND ONLY. A variant's position is its wire tag, so inserting one
/// renumbers every variant after it and silently turns old messages into
/// different new ones. New requests go at the END, and a version that needs
/// to change an existing one is a new VERSION.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Who am I talking to?
    ///
    /// The first thing a client should send, and the only one whose answer
    /// is about the engine rather than the data: which build, where its
    /// authority came from, and where its head is.
    Identity,
    Get {
        req_id: u64,
        key: Vec<u8>,
    },
    /// A page of a range, as a KEY continuation.
    ///
    /// The cursor is a key and not an offset: an offset into a tree that is
    /// being written to means a page that skips or repeats rows, and the
    /// caller has no way to know which.
    Range {
        req_id: u64,
        lo: Bound,
        hi: Bound,
        reverse: bool,
        /// Continue AFTER this key. `None` starts at the end `reverse` picks.
        after: Option<Vec<u8>>,
        /// Clamped by the engine, which reports what it used.
        max_entries: u32,
    },
    Write {
        /// CLIENT-chosen, so a re-submission is recognisable as the same
        /// write rather than a second one.
        write_id: u64,
        ops: Vec<Op>,
    },
    /// What happened to write N?
    AskWrite {
        write_id: u64,
    },
    /// Advisory: these roots are about to be read. Budgeted, never promised.
    Preload {
        roots: Vec<[u8; 32]>,
    },
    /// Tell me when these writes change state.
    Subscribe {
        write_ids: Vec<u64>,
    },
    /// The client is going away. Ship what is waiting.
    Flush,
    /// Time, from a connected client. It may decide WHEN, never WHAT.
    Tick {
        now: u64,
    },
}

/// One end of a range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bound {
    Unbounded,
    Included(Vec<u8>),
    Excluded(Vec<u8>),
}

/// What a client asked to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// What the engine says back. APPEND ONLY, for the same reason as `Request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    /// This build cannot serve that version, and here is what it can.
    ///
    /// The whole point of versioning from message one: a client that hears
    /// this knows to fall back, where silence would leave it waiting and a
    /// guess would leave it wrong.
    Unsupported {
        got: u16,
        known: Vec<u16>,
    },
    Identity {
        /// What this engine build calls itself.
        engine: String,
        /// Where its authority came from — never the key.
        key_source: String,
        head_seq: u64,
        head_root: [u8; 32],
    },
    Value {
        req_id: u64,
        value: Option<Vec<u8>>,
    },
    Page {
        req_id: u64,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        /// Pass back as `after`. `None` means the range is exhausted.
        cursor: Option<Vec<u8>>,
        /// What the engine actually used, which may be less than asked.
        max_entries: u32,
    },
    /// How far along a write is.
    WriteState {
        write_id: u64,
        state: WriteState,
    },
    /// A read could not be answered. A reply, not a silence.
    Unavailable {
        req_id: u64,
        blocked_on: [u8; 32],
    },
    /// Something arrived that this build could not use. Counted, never
    /// silent: a client talking in a format the engine cannot read would
    /// otherwise wait for ever for an answer to a message never understood.
    Dropped {
        reason: Dropped,
    },
}

/// The states a write moves through, as a client sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteState {
    Accepted,
    Stalled,
    Published,
    ParityComplete,
    /// TERMINAL, nothing applied. The client still holds the write and may
    /// re-submit it — which is what the OUTBOX is for.
    Busy,
    /// TERMINAL. The edit is not in the tree and never will be.
    Failed,
    /// TERMINAL. Its commit will not publish and the client is the only
    /// thing that still has it.
    Lost,
}

impl WriteState {
    /// Is there nothing more coming for this write?
    pub fn terminal(self) -> bool {
        matches!(
            self,
            WriteState::Busy | WriteState::Failed | WriteState::Lost
        )
    }

    /// Should the client send it again?
    ///
    /// Only `Busy`: nothing was applied, so a re-submission applies it once.
    /// `Failed` is also "not in the tree", but it means the write itself was
    /// refused and sending it again refuses it again. `Lost` needs the
    /// client's judgement, since another writer may have moved the tree
    /// underneath it.
    pub fn should_resubmit(self) -> bool {
        matches!(self, WriteState::Busy)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dropped {
    /// Not a message this build can read at all.
    Unparseable,
    /// Trailing bytes: the prefix parsed, and a prefix is not a message.
    TrailingBytes,
    /// Bigger than this build will decode.
    TooLarge,
    /// A response about something nobody asked for.
    Unexpected,
}

/// The largest message this build will decode.
///
/// A bound BEFORE the decoder sees anything, so a length field cannot ask for
/// an allocation the sender chooses.
pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;

/// Encoding options for everything on this wire.
///
/// `with_fixint_encoding` to match `bincode::serialize`; NO
/// `allow_trailing_bytes`, so a message with anything after it is refused
/// rather than half-read; and a limit, so a damaged length cannot ask for
/// more than the cap.
fn opts() -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_MESSAGE as u64)
}

pub fn encode_request(version: u16, body: &Request) -> Vec<u8> {
    use bincode::Options;
    opts()
        .serialize(&Envelope {
            version,
            body: body.clone(),
        })
        .unwrap_or_default()
}

pub fn encode_reply(r: &Reply) -> Vec<u8> {
    use bincode::Options;
    opts().serialize(r).unwrap_or_default()
}

/// What came of trying to read a client's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Ok(Envelope),
    /// A version this build does not serve. The VERSION was readable, which
    /// is the whole reason it is on the front.
    Unsupported(u16),
    Dropped(Dropped),
}

/// Read a client's message.
///
/// The version is checked BEFORE the body is trusted: a v2 body decoded by a
/// v1 decoder is not an error, it is a different message, and the version is
/// the only thing that can say so.
pub fn decode_request(bytes: &[u8]) -> Incoming {
    use bincode::Options;
    if bytes.len() > MAX_MESSAGE {
        return Incoming::Dropped(Dropped::TooLarge);
    }
    // The version alone, first. A body this build cannot read must still
    // produce `Unsupported` rather than `Unparseable`, or a newer client
    // learns nothing about why it was refused.
    let version = match bytes.get(..2) {
        Some(v) => u16::from_le_bytes([v[0], v[1]]),
        None => return Incoming::Dropped(Dropped::Unparseable),
    };
    if !KNOWN.contains(&version) {
        return Incoming::Unsupported(version);
    }
    match opts().deserialize::<Envelope>(bytes) {
        Ok(e) => Incoming::Ok(e),
        Err(e) => Incoming::Dropped(classify(&e)),
    }
}

pub fn decode_reply(bytes: &[u8]) -> Result<Reply, Dropped> {
    use bincode::Options;
    if bytes.len() > MAX_MESSAGE {
        return Err(Dropped::TooLarge);
    }
    opts().deserialize::<Reply>(bytes).map_err(|e| classify(&e))
}

fn classify(e: &bincode::Error) -> Dropped {
    match **e {
        bincode::ErrorKind::SizeLimit => Dropped::TooLarge,
        // bincode reports leftover input this way when trailing bytes are
        // not allowed.
        _ if e.to_string().contains("Slice had bytes remaining") => Dropped::TrailingBytes,
        _ => Dropped::Unparseable,
    }
}
