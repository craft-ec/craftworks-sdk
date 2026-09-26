//! WHO WANTS A BLOCK (WANTED-LIFE, sdk#480 part 3; the owner's "Structure before code"): the three reader indexes --
//! the parked reads waiting on a block, the repairs that need it as a slot, the parked write's needs -- held in ONE
//! type whose fields are PRIVATE to this module. The only code that can change them is the four transitions below
//! (`want`, `drop_reader`, `served`, `release_write`); everything else reads them through [`Wanted::readers_of`] and
//! the read accessors. The compiler is the one-writer gate: a source scan of spellings could be walked around (the
//! architect on #504).

use crate::read::ReqId;
use crate::Readers;
use freenet_prolly::Cid;
use std::collections::{BTreeMap, BTreeSet};

/// ONE reader of a block, as [`Wanted::want`] and [`Wanted::drop_reader`] take it: a parked read, a repair (by the block
/// it rebuilds), or the one parked write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reader {
    Read(ReqId),
    Repair(Cid),
    Write,
}

#[derive(Debug, Default)]
pub(crate) struct Wanted {
    /// Who is waiting on each outstanding block. Two requests needing one block share its fetch: the second does not
    /// pay for the first's round trip, and a popular branch is fetched once however many readers want it.
    waiting: BTreeMap<Cid, BTreeSet<ReqId>>,
    /// Which repairs a group block is being fetched for.
    slots: BTreeMap<Cid, BTreeSet<Cid>>,
    /// The blocks the parked write is waiting on, so an arrival for something else does not re-run its apply.
    needs: BTreeSet<Cid>,
}

impl Wanted {
    /// WHO WANTS `id`, derived, never stored.
    pub(crate) fn readers_of(&self, id: &Cid) -> Readers {
        Readers { reads: self.waiting.contains_key(id), repairs: self.slots.contains_key(id), parked_write: self.needs.contains(id) }
    }

    /// THE WANT: `reader` wants `id`. Who wanted it BEFORE -- each caller's own ask rule reads it (one ask per block).
    pub(crate) fn want(&mut self, id: Cid, reader: Reader) -> Readers {
        let before = self.readers_of(&id);
        match reader {
            Reader::Read(req) => {
                self.waiting.entry(id).or_default().insert(req);
            }
            Reader::Repair(missing) => {
                self.slots.entry(id).or_default().insert(missing);
            }
            Reader::Write => {
                self.needs.insert(id);
            }
        }
        before
    }

    /// THE DROP: `reader` no longer wants `id`. `true` when nobody does now.
    pub(crate) fn drop_reader(&mut self, id: Cid, reader: Reader) -> bool {
        match reader {
            Reader::Read(req) => {
                if let Some(reqs) = self.waiting.get_mut(&id) {
                    reqs.remove(&req);
                    if reqs.is_empty() {
                        self.waiting.remove(&id);
                    }
                }
            }
            Reader::Repair(missing) => {
                if let Some(set) = self.slots.get_mut(&id) {
                    set.remove(&missing);
                    if set.is_empty() {
                        self.slots.remove(&id);
                    }
                }
            }
            Reader::Write => {
                self.needs.remove(&id);
            }
        }
        !self.readers_of(&id).any()
    }

    /// `id` ARRIVED: the reads waiting on it are served -- taken. Not a drop: the block is here.
    pub(crate) fn served(&mut self, id: Cid) -> Option<BTreeSet<ReqId>> {
        self.waiting.remove(&id)
    }

    /// The parked write is released: every need loses it. The needs nobody else wants now.
    pub(crate) fn release_write(&mut self) -> Vec<Cid> {
        let needs = std::mem::take(&mut self.needs);
        needs.into_iter().filter(|id| !self.readers_of(id).any()).collect()
    }

    // ---- read accessors: nothing below changes a reader ----

    /// Is a parked read waiting on `id`?
    pub(crate) fn read_waits_on(&self, id: &Cid) -> bool {
        self.waiting.contains_key(id)
    }

    /// The parked reads waiting on `id`.
    pub(crate) fn reads_on(&self, id: &Cid) -> Option<&BTreeSet<ReqId>> {
        self.waiting.get(id)
    }

    /// The blocks read `req` waits on.
    pub(crate) fn waited_by(&self, req: ReqId) -> Vec<Cid> {
        self.waiting.iter().filter(|(_, reqs)| reqs.contains(&req)).map(|(id, _)| *id).collect()
    }

    /// Every block some parked read waits on.
    pub(crate) fn read_blocks(&self) -> impl Iterator<Item = &Cid> {
        self.waiting.keys()
    }

    /// The whole read-wait index, for the state walk only (its size, never its meaning).
    pub(crate) fn waiting(&self) -> &BTreeMap<Cid, BTreeSet<ReqId>> {
        &self.waiting
    }

    /// Is `id` a slot some repair is fetching?
    pub(crate) fn is_slot(&self, id: &Cid) -> bool {
        self.slots.contains_key(id)
    }

    /// The repairs (by the block they rebuild) that need slot `id`.
    pub(crate) fn repairs_on(&self, id: &Cid) -> Vec<Cid> {
        self.slots.get(id).map_or(Vec::new(), |s| s.iter().copied().collect())
    }

    /// The slots repair `missing` is fetching.
    pub(crate) fn slots_of(&self, missing: &Cid) -> Vec<Cid> {
        self.slots.iter().filter(|(_, set)| set.contains(missing)).map(|(slot, _)| *slot).collect()
    }

    /// The parked write's needs.
    pub(crate) fn write_needs(&self) -> &BTreeSet<Cid> {
        &self.needs
    }
}
