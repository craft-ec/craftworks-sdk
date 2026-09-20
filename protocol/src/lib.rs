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

pub mod outbox;
pub mod record;
pub mod session;

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
    /// Give the engine what it cannot produce for itself.
    ///
    /// A delegate cannot fabricate a contract — it has no way to produce wasm
    /// — and it cannot mint authority. These arrive once and are kept in the
    /// node's secret store, which survives a restart where the context does
    /// not.
    ///
    /// APPENDED, at the end of v1's vocabulary. A variant's position is its
    /// wire tag, so this is the only place a new request can go without
    /// renumbering the ones before it and silently turning old messages into
    /// different new ones. The recorded v1 session is what proves it did not.
    Install {
        block_code: Vec<u8>,
        register_code: Vec<u8>,
        register_params: Vec<u8>,
        /// TEST ONLY today: generated per run by a driver, never read from
        /// disk. A named type rather than bare bytes, because the danger is
        /// not that a test key leaks — it is that a REAL one arrives here
        /// and nothing notices.
        signing_key: TestKey,
    },
    /// Tell me when anything in this KEY RANGE changes.
    ///
    /// **For live data only.** A subscription costs context, a diff on every
    /// root move and a push the client must handle, continuously, and it is
    /// worth that only where the data changes while someone is looking and
    /// the change is small against what it changed. Ordinary data is read
    /// when it is wanted and is correct then; subscribing to it spends a slot
    /// the live data needed. Nothing above this layer takes a subscription
    /// out on a client's behalf.
    ///
    /// A separate request from [`Request::Subscribe`] rather than a field
    /// added to it. A variant's position is its wire tag and its FIELDS are
    /// its body, so adding `lo`/`hi` to `Subscribe` would leave the tag alone
    /// and change what follows it — a v1 client's `Subscribe` would decode as
    /// a range subscription over whatever the next bytes happened to be. The
    /// recorded v1 session is what proves this did not happen.
    SubscribeRange {
        /// CLIENT-chosen, so `Unsubscribe` and every `Changed` name the same
        /// subscription without the client having to learn an id the engine
        /// invented.
        sub_id: u64,
        lo: Bound,
        hi: Bound,
    },
    /// Stop telling me. Unknown ids are not an error: a client dropping a
    /// subscription it already lost should not have to find out first.
    Unsubscribe {
        sub_id: u64,
    },
    /// Emit a call tree as operations happen, or stop.
    ///
    /// **Asked for in advance, never afterwards.** A delegate gets a fresh
    /// linear memory on every call (F32), so there is nothing to ask about
    /// once an operation is over — a trace held until it finished is a trace
    /// that does not survive the thing it describes. So a client turns
    /// emission ON and the steps arrive as they happen; assembling them into
    /// a tree, and timing them, is the client's job.
    Trace {
        on: bool,
    },
    /// What changed in this range since the root I last saw?
    ///
    /// **No subscription is involved.** A delta is a capability of the tree,
    /// available to any reader holding a root: an ordinary reload uses it, and
    /// a binding that is not live uses it too. Being TOLD is the separate,
    /// declared thing, and all it does is trigger this sooner.
    ///
    /// **Per TREE.** A view assembled over several device trees is NOT the
    /// union of their per-tree deltas — a tombstone in one tree can REVEAL an
    /// older value in another, which no per-tree diff mentions. Phase 3 has
    /// one tree per identity; this is stated so nothing later assumes the
    /// union is valid.
    ChangesSince {
        req_id: u64,
        /// The root the CLIENT last saw. The client owns this, not the
        /// engine — which is what makes a missed notification recoverable:
        /// the gap closes in one call however many were dropped.
        from: [u8; 32],
        lo: Bound,
        hi: Bound,
        max_entries: u32,
    },
}

/// Which id space a trace question is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceOf {
    Write(u64),
    Read(u64),
}

/// A signing key that is not a real one, and says so in its own type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestKey(pub Vec<u8>);

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
        /// Whether this delegate can sign and publish a head — i.e. whether
        /// it has been provisioned.
        ///
        /// PRESENCE ONLY: it says that a key is there, never which, and
        /// nothing derived from it. A page reads this to decide whether to
        /// install; `false` with a non-zero `head_seq` cannot happen and
        /// would mean the store lost a secret the engine had already used.
        head_writable: bool,
    },
    /// `Install` arrived at a delegate that is already provisioned, and
    /// NOTHING was changed.
    ///
    /// The ordinary outcome of a race, not an error. Two tabs opened together
    /// on a fresh node both find it unprovisioned and both install; the first
    /// one to arrive wins and the second is told this. Were the second to
    /// overwrite, it would mint a second signing key, move the head's
    /// contract id, and orphan everything the first had already written.
    ///
    /// Replacing a key or the contract code is the hand-over design in
    /// sdk#14, and is never a blind overwrite.
    AlreadyInstalled,
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
    /// What one engine call DID. Appended to v1.
    ///
    /// Part of the protocol rather than a side channel, because an engine
    /// hosted in a delegate has no log anyone can read and the node prints
    /// nothing about it — so without this, five different breaks in the
    /// write path all present as the same thing: a write that stops at
    /// `Accepted`. Every field is a count the engine already had, and it
    /// found all five.
    Call {
        saw: Saw,
        /// Effects the core returned.
        effects: u32,
        /// Node operations issued.
        ops: u32,
        /// Blocks put and not yet read back.
        awaiting: u32,
        /// Puts confirmed by reading them back.
        read_back: u32,
        /// Effects still queued at the end of the call, and therefore LOST.
        /// Must be zero.
        stranded: u32,
        /// Inbound messages this build could not use.
        dropped: u32,
        /// What the head bump cost, by route: a PUT carries the contract's
        /// CODE, an UPDATE names it by id.
        head_put: u32,
        head_update: u32,
        /// What the node said, when it said anything.
        note: String,
    },
    /// Something in a subscribed range changed. Appended to v1.
    ///
    /// PUSHED, not answered: no client asked for this message, which is the
    /// whole point — a list that refreshes itself is a list nobody polls for.
    Changed {
        sub_id: u64,
        /// Where the tree is NOW. That is all.
        ///
        /// No key list and no previous root. The CLIENT owns "the last root I
        /// saw" and passes it to [`Request::ChangesSince`], which is what
        /// makes a dropped notification self-healing: the next one, or the
        /// client's own backstop, diffs across the whole gap in one call.
        ///
        /// It matters because delivery is lossy by construction — the node's
        /// notification channel drops when full, and a subscription can be
        /// evicted without anyone being told — so a client that missed some is
        /// the ORDINARY case. A payload here would also raise the drop rate
        /// for every other subscriber, to save a round trip that is local.
        new_root: [u8; 32],
        seq: u64,
        /// How the engine knows. See [`Why`].
        why: Why,
    },
    /// What changed since the root the client named. Appended to v1.
    Delta {
        req_id: u64,
        /// `None` as a value is a REMOVAL, not an empty value. A client told
        /// an empty value keeps a key the writer deleted.
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        cursor: Option<Vec<u8>>,
        /// Where the client now stands, if it applies these.
        new_root: [u8; 32],
    },
    /// The delta could not be computed; read the range in full instead.
    ///
    /// **A defined answer, not an error.** The old root's blocks may be gone
    /// — superseded, holding no demand, evicted — and for a client that was
    /// away a while that is ordinary rather than a failure. Naming the
    /// fallback here means the degraded path is one thing that gets tested,
    /// instead of something every caller re-invents.
    FullReloadRequired {
        req_id: u64,
        new_root: [u8; 32],
    },
    /// A subscribe was taken, or refused and why. Appended to v1.
    Subscribed {
        sub_id: u64,
        accepted: Accepted,
    },
    /// One step of an operation's call tree. Appended to v1.
    ///
    /// Emitted INCREMENTALLY, as it happens — never accumulated and sent at
    /// the end. A delegate gets a fresh linear memory every call (F32), so a
    /// tree held in memory until an operation finishes is a tree that does
    /// not survive the operation it describes.
    ///
    /// There is no duration here. The engine has no clock — it is sans-IO by
    /// construction — and a number it invented would be worse than none.
    /// The CLIENT stamps each step as it arrives.
    Step {
        /// Which operation this belongs to.
        of: TraceOf,
        /// Depth in the tree: 0 is the operation itself.
        depth: u8,
        /// What this step is. A short fixed vocabulary, not free text: a
        /// trace a client has to parse English out of is a log.
        what: Step,
        /// A count whose meaning depends on `what` — blocks, bytes, rows.
        n: u64,
    },
}

/// Why a subscriber was told something changed.
///
/// Distinct facts, never merged. Re-reading is correct on any of them, but a
/// client deciding whether to back off has to know which it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    /// The trees were compared over this range and they differ. A finding.
    Diffed,
    /// A block the comparison needed was not held. DURABLE — the old root's
    /// blocks may be gone for good — so repetition means the range goes
    /// `Stale` rather than notifying for ever.
    BlockMissing,
    /// The comparison was not attempted within this call's budget.
    /// TRANSIENT: retry promptly, do not back off.
    Budgeted,
    /// The engine has STOPPED comparing this range and is saying so once.
    /// Reload the range to retry; a declared degradation beats a silent one.
    Stale,
}

/// What came of a subscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accepted {
    Yes,
    /// The engine is at its own cap. The client still holds the intention and
    /// may try again once it drops one.
    Full,
    /// A bound longer than the engine will hold. Refused rather than trimmed:
    /// a silently narrowed range watches something the client did not ask for.
    TooWide,
}

/// One kind of step in an operation's call tree.
///
/// APPEND ONLY, like everything else on this wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// The operation began. `n` = how many edits, or 0 for a read.
    Began,
    /// The core returned effects. `n` = how many.
    Effects,
    /// Blocks were put. `n` = how many.
    Put,
    /// A put was read back and found. `n` = how many.
    ReadBack,
    /// The head was written. `n` = the seq.
    Head,
    /// The head was read back at the seq it was written at.
    HeadConfirmed,
    /// The operation reached a state a client can see. `n` = the
    /// [`WriteState`] as its wire tag, or rows for a read.
    Reached,
    /// Blocks were fetched. `n` = how many.
    Fetch,

    // ---- the SDK's own boundaries ----
    //
    // ONE event vocabulary, so a test's dump, the harness's tables,
    // `db.trace` and the builder's viewer all read the same words. These are
    // recorded client-side rather than sent by the engine, and they are here
    // rather than in a second enum for exactly that reason: a trace that
    // stops at the wire is a trace that cannot show why a binding did not
    // update.
    /// A client subscription to a tree's HEAD contract was asked for.
    /// `n` = 1 if it was accepted.
    HeadWatch,
    /// The node pushed "this head moved". `n` = how many bindings it woke.
    HeadMoved,
    /// A binding reloaded. `n` = rows it now holds.
    Reloaded,
    /// A binding looked and nothing had moved. `n` = 0.
    Quiet,
    /// A live binding lost its notifier and is on the backstop alone.
    Downgraded,
}

/// What kind of message woke an engine call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Saw {
    Client,
    GetResponse,
    PutResponse,
    UpdateResponse,
    Other,
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
    /// A message kind this side has no use for. Appended, like everything
    /// else: a variant's position is its wire tag.
    NotForUs,
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
