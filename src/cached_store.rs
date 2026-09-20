//! A store whose reads come from the local copy and whose writes go out on the
//! pump.
//!
//! This is what makes `Db` work in a browser. `Db` needs synchronous reads; in
//! a browser a read cannot be a round trip; so reads are served from the copy
//! of the ranges the app has bound, and the only part that crosses the wire is
//! the reload.
//!
//! # The one thing it must never do
//!
//! Answer "empty" for a range it has not loaded. The two facts are one byte
//! apart in a reply and a world apart in what an app does with them: told
//! empty it draws an empty list and stops; told [`StoreError::NotLoaded`] it
//! reloads. So a read outside the loaded intervals is an ERROR, and the app's
//! recovery from it is a `reload()` — which is a Promise, which is the honest
//! shape of a round trip.

use crate::copy::{Copy, Refused};
use crate::engine_client::Client;
use crate::store::{Delta, Edit, Read, Reads, RowState, Store, StoreError};
use protocol::Request;

/// Reads from the copy; writes to the pump and, optimistically, to the copy.
pub struct CachedStore {
    pub copy: Copy,
    pub client: Client,
    next_write_id: u64,
    /// Writes refused by the copy's own bound, so a caller that did not check
    /// the return can still find out. Drained.
    pub refused: Vec<(u64, Refused)>,
    /// Keys whose last write was rolled back.
    ///
    /// Kept because the fact outlives the pending entry: the moment a write
    /// is rolled back it stops being pending, and a row asking afterwards
    /// would be told `Clean` — "saved" — about a write that never landed.
    /// That is the worst answer available, so the key is remembered until
    /// something is written to it again.
    rolled_back: std::collections::BTreeSet<Vec<u8>>,
    /// Verdicts for writes this client never issued.
    unknown_verdicts: usize,
    /// The clock, handed in for the same reason the traces' is: this crate
    /// compiles to wasm and to a host binary and must not reach for one.
    now_ms: Box<dyn Fn() -> u64>,
}

impl CachedStore {
    pub fn new(now_ms: Box<dyn Fn() -> u64>) -> CachedStore {
        CachedStore {
            copy: Copy::new(),
            client: Client::new(),
            next_write_id: 1,
            refused: Vec::new(),
            rolled_back: std::collections::BTreeSet::new(),
            unknown_verdicts: 0,
            now_ms,
        }
    }

    /// Queue the request that loads `[lo, hi)`. The host pumps; the answer
    /// arrives at [`CachedStore::on_page`].
    pub fn request_range(&mut self, req_id: u64, lo: &[u8], hi: &[u8], max_entries: u32) {
        self.client.send(&Request::Range {
            req_id,
            lo: protocol::Bound::Included(lo.to_vec()),
            hi: protocol::Bound::Excluded(hi.to_vec()),
            reverse: false,
            after: None,
            max_entries,
        });
    }

    /// A page arrived: record it, and record that the range is now LOADED.
    ///
    /// The interval is passed in rather than inferred from the rows, because
    /// "everything in this span and not in this list is absent" is the fact
    /// that makes a later read trustworthy, and a list of rows cannot state
    /// it.
    pub fn on_page(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        rows: Vec<(Vec<u8>, Vec<u8>)>,
        at_root: [u8; 32],
    ) {
        self.copy.loaded_range(lo, hi, rows, at_root);
    }

    /// A delta arrived: base moves, the pending overlay re-applies.
    pub fn on_delta(
        &mut self,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        new_root: [u8; 32],
    ) -> crate::copy::Told {
        self.copy.apply_delta(changes, new_root)
    }

    /// Everything waiting to go out.
    pub fn take_outbound(&mut self) -> Vec<Vec<u8>> {
        self.client.take_outbound()
    }

    /// A message arrived from the node.
    ///
    /// **This is what takes a write OUT of the pending list**, and for a
    /// while nothing did. A write entered the copy as pending and stayed
    /// there: at `max_pending` the copy began refusing writes and the app
    /// could make no more, every row reported `PENDING` for ever so none
    /// could say "saved", and `tick` eventually rolled them all back as
    /// `Unknown` — every write a person made turning into "not applied".
    ///
    /// Found by writing 300 records and asserting that 300 arrived: 256 did,
    /// and 44 were refused. For every state a thing can enter, there has to
    /// be named code that takes it out.
    pub fn on_inbound(&mut self, bytes: &[u8]) {
        if let Ok(protocol::Reply::WriteState { write_id, state }) = protocol::decode_reply(bytes) {
            self.on_write_state(write_id, state);
        }
        self.client.on_inbound(bytes);
    }

    /// Move a write through the copy, as the engine reports it.
    ///
    /// Exhaustive on purpose: a new `WriteState` has to be classified here
    /// rather than silently leaving a write pending for ever, which is the
    /// failure this function exists to fix.
    fn on_write_state(&mut self, write_id: u64, state: protocol::WriteState) {
        use protocol::WriteState as W;
        // A verdict for a write this client never issued. The node chose the
        // id, so believing it would let one message clear or fail somebody
        // else's write. Counted rather than applied.
        if !self.copy.pending_ids().contains(&write_id) {
            self.unknown_verdicts += 1;
            return;
        }
        match state {
            // Taken by the engine, not yet on the network. Closing the tab
            // now still loses it, and the row says so.
            W::Accepted => self.copy.submitted(write_id),
            // The engine is busy with another commit; this one has not gone.
            W::Busy => self.copy.queued(write_id),
            // On the network. THIS is what clears it.
            W::Published | W::ParityComplete => self.copy.published(write_id),
            // Terminal and not applied. The edit is not in the tree.
            W::Failed | W::Lost => {
                let told = self.copy.failed(write_id);
                self.rolled_back.extend(told.rolled_back_keys);
            }
            // Still in flight, and said so rather than silently.
            W::Stalled => {}
        }
    }

    /// Verdicts for writes this client never issued.
    ///
    /// Counted, because a node answering about a write nobody made is worth
    /// seeing — and because a silent drop here would hide the case where the
    /// copy and the engine disagree about which writes exist.
    pub fn unknown_verdicts(&self) -> usize {
        self.unknown_verdicts
    }

    /// Roll back anything that has waited too long for a verdict.
    pub fn tick(&mut self) -> crate::copy::Told {
        let now = (self.now_ms)();
        let told = self.copy.time_out(now);
        self.rolled_back
            .extend(told.rolled_back_keys.iter().cloned());
        told
    }

    fn submit(&mut self, edits: Vec<(Vec<u8>, Option<Vec<u8>>)>) {
        let write_id = self.next_write_id;
        self.next_write_id += 1;
        let now = (self.now_ms)();

        // Applied to the copy FIRST, and only sent if the copy took it. A
        // write the client cannot hold is one it must not pretend to have
        // made: sending it while showing nothing would put the row and the
        // network permanently out of step with no way to notice.
        for (k, v) in &edits {
            // A key being written again is no longer a key whose last write
            // was rolled back. Clearing it here rather than on success is
            // deliberate: from this moment the row has something in flight
            // to report, which is a truer answer than the old failure.
            self.rolled_back.remove(k);
            if let Err(why) = self.copy.write(k, v.clone(), write_id, now) {
                self.refused.push((write_id, why));
                // Anything already applied for this write comes back off, so a
                // multi-key write is all or nothing in the copy as well as on
                // the wire.
                self.copy.failed(write_id);
                return;
            }
        }
        self.client.send(&Request::Write {
            write_id,
            ops: edits
                .into_iter()
                .map(|(k, v)| match v {
                    Some(v) => protocol::Op::Put(k, v),
                    None => protocol::Op::Delete(k),
                })
                .collect(),
        });
    }

    /// The write id the next write will carry, so a caller can watch for it.
    pub fn next_write_id(&self) -> u64 {
        self.next_write_id
    }
}

impl Store for CachedStore {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.submit(vec![(key.to_vec(), Some(value.to_vec()))]);
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        // What the copy knows. A delete of a key in an unloaded range reports
        // `false` — not because it was absent, but because nothing here knows;
        // the delete still goes, which is right either way.
        let existed = matches!(self.copy.get(key), Some(v) if v.value().is_some());
        self.submit(vec![(key.to_vec(), None)]);
        existed
    }

    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) {
        // ONE write, so the engine applies them as one commit.
        self.submit(
            edits
                .iter()
                .map(|(k, e)| match e {
                    Edit::Put(v) => (k.clone(), Some(v.clone())),
                    Edit::Delete => (k.clone(), None),
                })
                .collect(),
        );
    }
}

impl Reads for CachedStore {
    /// What this key's own write is doing, from the local copy.
    ///
    /// `Unknown` when the key is not loaded — **never `Clean`**. "I have not
    /// looked" is not "there is nothing in flight", and a row that said
    /// "saved" about a write it cannot see is the exact failure `NOT_LOADED`
    /// exists to prevent, one layer up.
    fn row_state(&self, key: &[u8]) -> RowState {
        if self.rolled_back.contains(key) {
            return RowState::RolledBack;
        }
        match self.copy.get(key) {
            None => RowState::Unknown,
            Some(crate::copy::Visible::Clean(_)) => RowState::Clean,
            Some(crate::copy::Visible::Queued(_, _)) => RowState::Queued,
            Some(crate::copy::Visible::Pending(_, _)) => RowState::Pending,
        }
    }

    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        match self.copy.get(key) {
            Some(v) => Ok(v.value().map(|b| b.to_vec())),
            None => Err(StoreError::NotLoaded),
        }
    }

    fn scan(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(rows) = self.copy.range(lo, hi) else {
            return Err(StoreError::NotLoaded);
        };
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = rows
            .into_iter()
            .filter_map(|(k, v)| v.value().map(|b| (k, b.to_vec())))
            .collect();
        if reverse {
            out.reverse();
        }
        out.truncate(limit);
        Ok(out)
    }

    fn root(&mut self) -> Read<[u8; 32]> {
        self.copy.root().ok_or(StoreError::NotLoaded)
    }

    fn changes_since(
        &mut self,
        _from: [u8; 32],
        _lo: &[u8],
        _hi: &[u8],
        _max_entries: u32,
    ) -> Read<Delta> {
        // The copy holds no history, so it cannot diff two roots itself. The
        // DEFINED answer, not an error: the caller does a plain range read,
        // which is the same recovery it would do against an engine whose old
        // root has been evicted. The engine's own `ChangesSince` is what a
        // delta actually comes from; this is the local fallback.
        //
        // With NOTHING loaded there is no root to name, and the first version
        // answered with thirty-two zero bytes — a root-shaped value that is
        // not a root. A caller would have recorded it as where it stands and
        // asked for a delta against it for ever. `NotLoaded` is the same
        // answer `root()` already gives, and it is the true one.
        Ok(Delta::FullReloadRequired {
            new_root: self.copy.root().ok_or(StoreError::NotLoaded)?,
        })
    }
}
