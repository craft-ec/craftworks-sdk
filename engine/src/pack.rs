//! A commit's blocks in one PUT, and the manifest that makes the network the
//! journal.
//!
//! **The pack format is defined by the Block contract** (freenet-contracts,
//! `block/src/pack.rs`), which is what actually refuses a malformed one. This
//! is a second implementation of the same bytes, and that is a real hazard:
//! two spellings of one format drift, and the drift shows up as a host
//! refusing a commit. It is here because the engine cannot depend on the
//! contracts repo, and it is pinned by a vector test against the format as
//! documented. A shared crate is the eventual answer.
//!
//! ```text
//! pack     = "PK01" ‖ count:u16 ‖ member*
//! member   = kind:u8 ‖ len:u32 ‖ bytes
//! manifest = "CM01" ‖ prev_seq:u64 ‖ prev_root:32 ‖ seq:u64 ‖ root:32
//!                   ‖ groups:u32 ‖ (parity ids: 3 × 32)*
//! ```
//!
//! Members are strictly ascending BY BLOCK ID, so a set has one encoding and
//! two writers packing the same commit produce the same pack.

use freenet_prolly::{block_id, kind, Cid};

pub const PACK_MAGIC: &[u8; 4] = b"PK01";
pub const MANIFEST_MAGIC: &[u8; 4] = b"CM01";

/// The kind byte a pack is stored under. Not a kind this crate may invent:
/// it is the value the Block contract assigns to PACK.
pub const PACK_KIND: u8 = 6;

// ---------------------------------------------------------------------------
// What the network will accept, PER MEMBER — mirrored, not imported.
//
// These are the Block contract's numbers. They are copied rather than depended
// on for the reason `PACK_KIND` above is: the SDK and the contracts
// deliberately do not share a revision, because a contract's wasm hash depends
// on its dependencies' IDENTITY, so pinning them together would make every SDK
// change a network epoch. What ties the two is tested against the released
// artefact, never rev equality.
//
// Until this existed, `max_body` appeared NOWHERE in the SDK. Nothing compared
// a member to its kind's limit, so `PackError::TooLarge` was not merely
// unfired but UNFIREABLE, and an oversized pack was built in silence and
// refused at the NODE — remotely, where a node can log nothing at the moment it
// refuses (F48). The legible local failure was designed and unreachable; what
// survived was the least diagnosable one.
// ---------------------------------------------------------------------------

/// The largest ordinary block body the contract accepts.
pub const MAX_BODY: usize = 256 * 1024 + 64;

/// A parity symbol is as long as the longest member it codes, plus its framing.
pub const MAX_PARITY: usize = 4 + 1 + MAX_BODY;

/// The largest pack, as a container. Agrees with the contract.
pub const MAX_PACK: usize = 1024 * 1024;

/// The largest body the contract accepts for a block of `kind`.
///
/// The limit that bites a packed member is THIS, not the pack's: packed members
/// are `RAW`, so the ceiling is 262,208 and not the pack's 1 MiB. Confusing the
/// two is the error this function exists to stop being made twice.
pub const fn max_body(kind: u8) -> usize {
    match kind {
        PACK_KIND => MAX_PACK,
        kind::PARITY => MAX_PARITY,
        _ => MAX_BODY,
    }
}

/// What a commit says about itself: where it came FROM and where it goes.
///
/// **It used to claim more, and that claim is withdrawn.** The doc here said a
/// restarted engine "finds its packs the head does not yet name" and replays
/// them, so the manifest also carried the redundancy the commit still owed.
/// That recovery cannot be performed: a pack's key is its content hash, so an
/// engine holding only its device key cannot enumerate packs its head does not
/// name — only the HEAD makes anything findable (`lib.rs`, on the same point).
/// The `owed` set is gone with it (craftworks-sdk#117).
///
/// Two further reasons it should not come back as carried state. The set was
/// WRONG: a commit-time snapshot can name a group superseded since, and
/// `record_owed` spends its whole diff walk dropping exactly those, so a
/// recovering engine trusting it would undo that coalescing. And what recovery
/// needs is DERIVED, not carried: parity ids live in the node, so a walk from
/// the published root names every group (craftworks-sdk#119).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub prev_seq: u64,
    pub prev_root: Cid,
    pub seq: u64,
    pub root: Cid,
}

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::from(&MANIFEST_MAGIC[..]);
        out.extend_from_slice(&self.prev_seq.to_le_bytes());
        out.extend_from_slice(&self.prev_root);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.root);
        out
    }

    pub fn decode(body: &[u8]) -> Option<Manifest> {
        // Fixed width now that the owed set is gone, and TRAILING BYTES ARE
        // REFUSED: a manifest is exactly this long, so anything after it is
        // either a different format or something appended, and reading past a
        // length nobody declared is how a parser starts trusting its input.
        let head: &[u8; 4 + 8 + 32 + 8 + 32] = body.try_into().ok()?;
        if &head[..4] != MANIFEST_MAGIC {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(head[o..o + 8].try_into().unwrap());
        let cid_at = |o: usize| -> Cid { head[o..o + 32].try_into().unwrap() };
        Some(Manifest {
            prev_seq: u64_at(4),
            prev_root: cid_at(12),
            seq: u64_at(44),
            root: cid_at(52),
        })
    }
}

/// Why a set of members cannot be packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
    Empty,
    TooManyMembers(usize),
    /// A member is longer than the contract accepts for ITS kind.
    ///
    /// Carries the limit and the kind, not just the length: this error is
    /// rendered into a panic message by the one caller, so what it says is
    /// the entire diagnosis available at the moment it fires.
    TooLarge { kind: u8, len: usize, limit: usize },
}

/// Build one pack body. Members are sorted and de-duplicated by block id,
/// because a pack is a SET and the same block offered twice is one member.
pub fn build(members: &[(u8, Vec<u8>)]) -> Result<Vec<u8>, PackError> {
    let mut ordered: Vec<&(u8, Vec<u8>)> = members.iter().collect();
    ordered.sort_by_key(|(k, b)| block_id(*k, b));
    ordered.dedup_by_key(|(k, b)| block_id(*k, b));
    if ordered.is_empty() {
        return Err(PackError::Empty);
    }
    if ordered.len() > u16::MAX as usize {
        return Err(PackError::TooManyMembers(ordered.len()));
    }
    // THE MISSING COMPARISON. Checked per MEMBER against its own kind's limit,
    // because that is what the contract applies — a pack whose total fits can
    // still carry a member the network refuses, and that refusal happens
    // remotely where it is hardest to see.
    if let Some((kind, body)) = ordered.iter().find(|(k, b)| b.len() > max_body(*k)) {
        return Err(PackError::TooLarge { kind: *kind, len: body.len(), limit: max_body(*kind) });
    }
    let mut out = Vec::from(&PACK_MAGIC[..]);
    out.extend_from_slice(&(ordered.len() as u16).to_le_bytes());
    for (k, b) in ordered {
        out.push(*k);
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }
    Ok(out)
}

/// What one member costs inside a pack: its bytes plus the header the format
/// puts in front of it. Used to plan a pack without building it first.
pub fn member_cost(bytes: usize) -> usize {
    1 + 4 + bytes
}

/// The header every pack carries, whatever is in it.
pub const PACK_HEADER: usize = 4 + 2;

/// The id a pack body will have.
pub fn pack_id(body: &[u8]) -> Cid {
    block_id(PACK_KIND, body)
}

/// The kind byte for a member: a node is a TREE_NODE, anything else is RAW.
///
/// Decided from the BYTES rather than from where they came from, because that
/// is how the contract decides: it parses each member and refuses one whose
/// kind does not match its content.
pub fn member_kind(bytes: &[u8]) -> u8 {
    if freenet_prolly::node::Node::parse(bytes).is_ok() {
        kind::TREE_NODE
    } else {
        kind::RAW
    }
}

/// The members of a pack body, as `(id, bytes)`.
///
/// Returns what it can read and stops at the first thing that does not parse:
/// a pack from a stranger is bytes, not a promise, and the caller
/// hash-checks every member anyway.
pub fn members(body: &[u8]) -> Vec<(Cid, Vec<u8>)> {
    let mut out = Vec::new();
    let Some((head, mut rest)) = body.split_at_checked(PACK_HEADER) else {
        return out;
    };
    if &head[..4] != PACK_MAGIC {
        return out;
    }
    let count = u16::from_le_bytes([head[4], head[5]]) as usize;
    for _ in 0..count {
        let Some((h, tail)) = rest.split_at_checked(5) else {
            return out;
        };
        let k = h[0];
        let len = u32::from_le_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let Some((bytes, tail)) = tail.split_at_checked(len) else {
            return out;
        };
        rest = tail;
        out.push((freenet_prolly::block_id(k, bytes), bytes.to_vec()));
    }
    out
}
