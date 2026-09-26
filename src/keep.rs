//! THE `keep` RECORD (KEEPER §3): one per asset, in the identity's OWN tree, under the tree's SYSTEM tag beside the
//! domains' schemas -- the SDK's record, in no app's namespace. Its KEY is the asset's Register id (one record per
//! asset, so two devices of one identity merge it per record: rule 15); its VALUE is a fixed binary form, versioned:
//!
//! `version(1) ‖ repair(1) ‖ warn_below(1) ‖ audited_at(u64 LE, whole seconds, 0 = never) ‖ health(4 x u32 LE)`
//!
//! `repair`: `off` = 0, `always` = 1, `below N` = 2 + N. The pass's damaged ids stay in its report; only the counts
//! are stored. A value of another version or length is not read as this one (a newer build's record).

/// The key prefix: the SYSTEM tag (0x00), then `keep\0`.
pub const PREFIX: &[u8] = b"\x00keep\x00";
/// The value's version byte. Pinned by a round-trip test: changing it changes what stored records mean.
pub const VERSION: u8 = 1;
/// The value's length: version, repair, warn_below, audited_at, four counts.
pub const LEN: usize = 1 + 1 + 1 + 8 + 4 * 4;

/// What a pass may repair (KEEPER §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repair {
    /// Watch only.
    Off,
    /// Keep BACKED_UP: repair anything missing.
    Always,
    /// Repair a group once its margin is below N.
    Below(u8),
}

/// A FULL pass's result, as counts (KEEPER §4, §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub groups: u32,
    pub whole: u32,
    pub degraded: u32,
    pub damaged: u32,
}

/// One asset's policy and last full pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keep {
    pub repair: Repair,
    /// Warn once any group's margin is below this (0 ..= m).
    pub warn_below: u8,
    /// The last FULL pass, whole seconds; 0 = never.
    pub audited_at: u64,
    pub health: Health,
}

/// m, the parity blocks per group: a group's margin is at most m, so `warn_below` and `below N` are 0 ..= m (KEEPER §3).
pub const M: u8 = freenet_prolly::parity::PARITY as u8;

impl Keep {
    /// The person's OWN tree, with no record until they change it: keep BACKED_UP, warn below 2 (KEEPER §3).
    pub const OWN_DEFAULT: Keep = Keep { repair: Repair::Always, warn_below: 2, audited_at: 0, health: Health { groups: 0, whole: 0, degraded: 0, damaged: 0 } };
    /// An app's record as its FIRST publish writes it (KEEPER §2: the one way an app enters the list), until the person
    /// changes it: the own tree's defaults, never audited.
    pub const APP_DEFAULT: Keep = Keep::OWN_DEFAULT;

    /// A policy this record can hold, or why not, by name: `warn_below` and `below N` are 0 ..= [`M`] (a margin is
    /// never above m). The one check -- a policy edit is refused here, never stored and read back as another.
    pub fn check(&self) -> Result<(), String> {
        if self.warn_below > M {
            return Err(format!("warn_below {} is above m = {M}", self.warn_below));
        }
        if let Repair::Below(n) = self.repair {
            if n > M {
                return Err(format!("repair below {n} is above m = {M}"));
            }
        }
        Ok(())
    }
}

/// The record's key for the asset whose Register is `target`.
pub fn key(target: &[u8; 32]) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(target);
    k
}

/// The asset a record key names, if it is one.
pub fn target_of(key: &[u8]) -> Option<[u8; 32]> {
    key.strip_prefix(PREFIX)?.try_into().ok()
}

/// `k` must pass [`Keep::check`]: `below N` is stored as 2 + N in one byte.
pub fn encode(k: &Keep) -> Vec<u8> {
    debug_assert!(k.check().is_ok(), "an unchecked policy was stored: {k:?}");
    let mut v = Vec::with_capacity(LEN);
    v.push(VERSION);
    v.push(match k.repair {
        Repair::Off => 0,
        Repair::Always => 1,
        Repair::Below(n) => 2u8.saturating_add(n),
    });
    v.push(k.warn_below);
    v.extend_from_slice(&k.audited_at.to_le_bytes());
    for n in [k.health.groups, k.health.whole, k.health.degraded, k.health.damaged] {
        v.extend_from_slice(&n.to_le_bytes());
    }
    v
}

/// `None`: not a version-1 record (a newer build's, or damaged) -- never read as one.
pub fn decode(v: &[u8]) -> Option<Keep> {
    if v.len() != LEN || v[0] != VERSION {
        return None;
    }
    let repair = match v[1] {
        0 => Repair::Off,
        1 => Repair::Always,
        n => Repair::Below(n - 2),
    };
    let u32_at = |i: usize| u32::from_le_bytes(v[i..i + 4].try_into().expect("4"));
    Some(Keep {
        repair,
        warn_below: v[2],
        audited_at: u64::from_le_bytes(v[3..11].try_into().expect("8")),
        health: Health { groups: u32_at(11), whole: u32_at(15), degraded: u32_at(19), damaged: u32_at(23) },
    })
}

/// How many groups are below `warn_below` blocks to spare (KEEPER §3: warn once any group's margin is below N), from a
/// pass's `margins` (margin -> groups). The one place the warning is derived; 0 = no warning.
pub fn warning(margins: &std::collections::BTreeMap<i64, usize>, warn_below: u8) -> usize {
    margins.range(..i64::from(warn_below)).map(|(_, n)| *n).sum()
}
