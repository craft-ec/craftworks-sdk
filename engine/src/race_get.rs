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
//!   match); the root is a group of ONE (sdk#335) when its head listed its parity -- the first of its `1 + m` to
//!   arrive answers -- and is fetched singly when it did not.
//!
//! Stragglers need nothing here: the page re-sends a GET on its RTO with no count (rule 7), and a block that
//! arrives after its group resolved is either used (someone still waits) or dropped (withdrawn).

use crate::{Effect, Engine};
use freenet_prolly::store::Blocks;
use freenet_prolly::Cid;
use std::collections::BTreeSet;

impl<B: Blocks> Engine<B> {
    /// A read has just asked for `id` under `root`: race the rest of its group. Nothing when racing is off, when
    /// `id` is already being raced or repaired, or when it is in no group (a root whose head listed no parity).
    pub(crate) fn race(&mut self, id: Cid, root: Cid) -> Vec<Effect> {
        if !self.params.race_get || !self.params.repair_reads || self.repairs.contains_key(&id) {
            return Vec::new();
        }
        match self.group_for(root, id) {
            Some(group) => self.start_repair(group, false),
            None => Vec::new(),
        }
    }

    /// A block rebuilt from its group. Its bytes must BE its id, checked here and not only inside the rebuild
    /// (the architect's (a)): otherwise nothing -- the page never holds bytes under a wrong id. Verified, the
    /// page KEEPS it, and PUTs it back (sdk#303) when a reader WAITED on it (a rebuild nobody asked for is not
    /// written: the architect's (c)).
    pub(crate) fn rebuilt(&self, id: Cid, body: &[u8]) -> Vec<Effect> {
        if !crate::read::matches_id(&id, body) {
            return Vec::new();
        }
        let mut out = vec![Effect::Keep { id, bytes: body.to_vec() }];
        // PUT back for whoever needed it: a read, or a parked write (sdk#405) -- the network is whole again.
        if self.reads.waiting.contains_key(&id) || self.parked_write.as_ref().is_some_and(|p| p.needs.contains(&id)) {
            out.push(Effect::PutRepaired { id, bytes: body.to_vec() });
        }
        out
    }

    /// The block a race was racing is answered NotFound: the race becomes a REPAIR, and the slots it dropped
    /// (NotFound or wrong beside a healthy read) are asked again, now until answered.
    pub(crate) fn race_missed(&mut self, id: Cid) -> Vec<Effect> {
        let Some(r) = self.repairs.get_mut(&id) else { return Vec::new() };
        if r.missed {
            return Vec::new();
        }
        r.missed = true;
        let slots: Vec<(usize, Cid)> = std::mem::take(&mut r.dropped).into_iter().map(|i| (i, r.group.slots[i])).collect();
        for (i, _) in &slots {
            r.asked.insert(*i, 0);
        }
        let mut out = Vec::new();
        for (_, slot) in slots {
            let in_flight = self.repair_slots.contains_key(&slot) || self.reads.waiting.contains_key(&slot);
            self.repair_slots.entry(slot).or_default().insert(id);
            self.withdrawn.remove(&slot);
            if !in_flight {
                self.reads.fetches += 1;
                self.step_asks.entry(slot).or_insert(false);
                out.push(Effect::FetchBlock { id: slot, via: crate::read::Via::Direct, attempt: 0 });
            }
        }
        out
    }

    /// Land every block rebuilt this step, like an arrival, in a LOOP: a landing that finishes more races queues
    /// their rebuilds behind it rather than recursing into them ([`Engine`]'s `landing`).
    ///
    /// A block lands ONCE per step. `on_arrived` clears `arrived` when it ends, and the page keeps a rebuilt block
    /// only when it applies `Keep`, after the step -- so a read re-descending through it later in the same step
    /// wants it again, races, and rebuilds it again; landed again, that loop never ends. Once is enough: the read
    /// asks the block, and the next step reads it from the page.
    pub(crate) fn land_rebuilt(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        let mut landed: BTreeSet<Cid> = BTreeSet::new();
        while !self.landing.is_empty() {
            let (id, body) = self.landing.remove(0);
            if landed.insert(id) {
                out.extend(self.on_arrived(id, body));
            }
        }
        out
    }

    /// Was `id` asked for by a race or repair that has finished, with nobody wanting it any more?
    pub fn is_withdrawn(&self, id: &Cid) -> bool {
        self.withdrawn.contains(id)
    }

    /// Every block withdrawn since the last call, handed to the page, which ENDS their GETs (queued, in flight or
    /// waiting to re-ask): the engine keeps no entry after.
    pub fn take_all_withdrawn(&mut self) -> BTreeSet<Cid> {
        std::mem::take(&mut self.withdrawn)
    }

    /// Withdrawn blocks the page has not taken yet (0 after every page call).
    pub fn withdrawn_count(&self) -> usize {
        self.withdrawn.len()
    }
}

#[cfg(test)]
mod put_back {
    use crate::read::ReqId;
    use crate::{Effect, Engine, Params};
    use freenet_prolly::store::MemBlocks;
    use freenet_prolly::{block_id, kind};

    fn put(fx: &[Effect]) -> usize {
        fx.iter().filter(|f| matches!(f, Effect::PutRepaired { .. })).count()
    }

    /// The put-back decision, each condition on its own: wanted AND verified is put; verified but unwanted is
    /// only kept; unverified is neither. (Through the engine, a rebuild never yields bytes that are not its id -- `repair::rebuild`
    /// refuses them first -- so (a) is reachable only here.)
    #[test]
    fn a_rebuilt_block_is_put_back_only_when_wanted_and_verified() {
        let mut e = Engine::new(Params::default(), MemBlocks::default());
        let body = b"a rebuilt block".to_vec();
        let id = block_id(kind::RAW, &body);
        assert_eq!(put(&e.rebuilt(id, &body)), 0, "(c) a rebuild nobody waited on was put");
        e.reads.want(id, ReqId(1), true);
        assert_eq!(put(&e.rebuilt(id, &body)), 1, "a wanted, verified rebuild was not put");
        assert_eq!(put(&e.rebuilt(id, b"not its bytes")), 0, "(a) an unverified rebuild was put");
        assert!(e.rebuilt(id, b"not its bytes").is_empty(), "(a) an unverified rebuild was KEPT: the page would hold bytes under a wrong id");
    }
}
