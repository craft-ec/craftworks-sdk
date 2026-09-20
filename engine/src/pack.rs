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

/// What a commit says about itself.
///
/// The network is the journal: after a restart the engine holds only its
/// device key, reads its head, finds its packs the head does not yet name, and
/// replays these in `seq` order. So everything needed to resume — where the
/// commit came FROM, where it goes, and what redundancy it still owes — is in
/// the pack, not in any local file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub prev_seq: u64,
    pub prev_root: Cid,
    pub seq: u64,
    pub root: Cid,
    /// The parity groups this commit owes, by their three ids.
    pub owed: Vec<[Cid; 3]>,
}

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::from(&MANIFEST_MAGIC[..]);
        out.extend_from_slice(&self.prev_seq.to_le_bytes());
        out.extend_from_slice(&self.prev_root);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.root);
        // u32, and refused above it rather than truncated: a count that wraps
        // writes a small number in front of a long body, and the reader then
        // stops early and silently resumes an incomplete commit.
        let n = u32::try_from(self.owed.len()).expect("owed groups fit in u32");
        out.extend_from_slice(&n.to_le_bytes());
        for g in &self.owed {
            for id in g {
                out.extend_from_slice(id);
            }
        }
        out
    }

    pub fn decode(body: &[u8]) -> Option<Manifest> {
        let (head, rest) = body.split_at_checked(4 + 8 + 32 + 8 + 32 + 4)?;
        if &head[..4] != MANIFEST_MAGIC {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(head[o..o + 8].try_into().unwrap());
        let cid_at = |o: usize| -> Cid { head[o..o + 32].try_into().unwrap() };
        let n = u32::from_le_bytes(head[84..88].try_into().unwrap()) as usize;
        // The declared count must fit what follows before anything is read.
        if rest.len() != n.checked_mul(96)? {
            return None;
        }
        let mut owed = Vec::with_capacity(n);
        for g in rest.chunks_exact(96) {
            owed.push([
                g[0..32].try_into().unwrap(),
                g[32..64].try_into().unwrap(),
                g[64..96].try_into().unwrap(),
            ]);
        }
        Some(Manifest {
            prev_seq: u64_at(4),
            prev_root: cid_at(12),
            seq: u64_at(44),
            root: cid_at(52),
            owed,
        })
    }
}

/// Why a set of members cannot be packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
    Empty,
    TooManyMembers(usize),
    TooLarge(usize),
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
