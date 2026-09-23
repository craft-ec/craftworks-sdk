//! RACE GET (the owner's rule 11, sdk#303): a read asks ALL `k + m` blocks of a sibling group and finishes on the
//! first `k`, repairing from parity.
//!
//! #300's repair already does the second half: given a group, it asks every other block of it at once, counts
//! what arrives (each checked by hash against its slot, parity included), rebuilds the missing member at `k`,
//! verifies it, and hands it to the page as a block to keep, where it lands like any arrival. What it did not do
//! was START until the member itself had been answered NotFound -- and a member that is SILENT is never answered,
//! so its read waited on the slowest block. Racing starts that gather the FIRST time a read wants the member:
//!
//! - the member is asked, as before, and so is the rest of its group, at once (point reads too: rule 11 as
//!   written; a "race only after an RTO" hedge would amend it, and is not built);
//! - whichever comes first ends it: the member itself (`on_arrived` ends its repair), or any `k` of the group
//!   (rebuilt and verified);
//! - each block is asked ONCE however many members race it (`start_repair` does not re-ask a slot in flight);
//! - what a finished race no longer wants is WITHDRAWN (`Engine::take_withdrawn`): the page drops its GET instead
//!   of re-asking it for ever, and does not keep it if it arrives late;
//! - the group is found in the parent held under the read's OWN pinned root (another root's parity would not
//!   match); the root itself is in no group and is fetched singly.
//!
//! Stragglers need nothing here: the page re-sends a GET on its RTO with no count (rule 7), and a block that
//! arrives after its group resolved is either used (someone still waits) or dropped (withdrawn).

use crate::{repair, Effect, Engine};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;

impl<B: Blocks> Engine<B> {
    /// A read has just asked for `id` under `root`: race the rest of its group. Nothing when racing is off, when
    /// `id` is already being raced or repaired, or when it is in no group (the root).
    pub(crate) fn race(&mut self, id: Cid, root: Cid) -> Vec<Effect> {
        if !self.params.race_get || !self.params.repair_reads || self.repairs.contains_key(&id) {
            return Vec::new();
        }
        match repair::find_group(&self.source(), root, id) {
            Some(group) => self.start_repair(group),
            None => Vec::new(),
        }
    }

    /// Was `id` asked for by a race or repair that has finished, with nobody wanting it any more?
    pub fn is_withdrawn(&self, id: &Cid) -> bool {
        self.withdrawn.contains(id)
    }

    /// Every block withdrawn since the last call, handed to the page, which ENDS their GETs (queued, in flight or
    /// waiting to re-ask): the engine keeps no entry after.
    pub fn take_all_withdrawn(&mut self) -> std::collections::BTreeSet<Cid> {
        std::mem::take(&mut self.withdrawn)
    }

    /// Withdrawn blocks the page has not taken yet (0 after every page call).
    pub fn withdrawn_count(&self) -> usize {
        self.withdrawn.len()
    }
}
