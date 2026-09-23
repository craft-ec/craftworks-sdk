//! A [`Store`](crate::Store) whose data lives in the engine delegate.
//!
//! The in-memory [`TreeStore`](crate::TreeStore) stays: it is the offline and
//! test backend, and it is what an app uses before it has a node. This is the
//! same four operations against an engine on the other side of a connection.
//!
//! Two things make that harder than it sounds, and both are handled HERE so
//! that nothing above the `Store` trait has to know:
//!
//! 1. **The engine answers `Busy`.** It takes one commit at a time, and a
//!    write arriving while one is in flight applies nothing. That is not an
//!    error to report upwards — it is a "not yet", and the
//!    [`Outbox`](protocol::outbox::Outbox) is what turns it into an eventual
//!    yes without anybody being asked.
//! 2. **A write is not done when it is accepted.** `Accepted` means the edit
//!    is in the tree; `Published` means the head moved. The store returns
//!    when the engine has the write, because that is when a subsequent read
//!    through the same engine will see it.

use crate::engine_client::Client;
pub use crate::engine_client::Event;
use crate::store::{Delta, Edit, IdWidth, Read, Reads, Store, StoreError};
use crate::trace::Trace;
use protocol::outbox::{Lost, Outbox, PreImage};
use protocol::{Reply, Request, WriteState};

/// A host that can send a request and wait for its replies.
///
/// **A blocking host, and only a blocking host.** A Rust test, the live
/// driver, a native client — anywhere a caller may sit still until an answer
/// comes back. A browser is NOT one of these: there is no blocking receive on
/// its main thread, which is why the state machine lives in
/// [`Client`](crate::engine_client::Client) and this is only one way to drive
/// it. [`EngineStore`] is the blocking driver; a browser drives the same
/// `Client` from its own event loop.
pub trait Transport {
    /// Send one encoded request; return whatever came back.
    ///
    /// Replies for OTHER requests, and pushes that answer nothing, may come
    /// back here too — the client routes by content, not by position, so a
    /// host that returns everything it has is doing the right thing.
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>>;

    /// Receive without sending.
    ///
    /// **The chaos transport is what made this necessary**, and it was a real
    /// gap rather than a test artefact. `exchange` alone encodes an assumption
    /// a connection does not keep: that the answer to a request arrives during
    /// that request's own round trip. Hold one reply back — which is all
    /// reordering IS — and a driver with no way to simply receive concludes
    /// the engine never answered. The first browser connection would have hit
    /// it, and the symptom would have been `NoAnswer` from a node that had
    /// replied.
    ///
    /// Defaulted to nothing, so a transport whose exchange really is a
    /// complete round trip needs no ceremony.
    fn poll(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

/// Rounds the blocking driver will move bytes in one call.
///
/// A bound on the DRIVER, not on the protocol: a transport that answered every
/// request with another request would spin here for ever, and this runs on a
/// thread somebody is waiting on.
const MAX_PUMP_ROUNDS: usize = 64;

/// One page of a range, and how to continue it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// Pass back as `after`. `None` means the range is exhausted.
    pub cursor: Option<Vec<u8>>,
    /// What the engine actually used, which may be less than was asked for.
    pub page_size_used: u32,
}

/// The blocking driver: a [`Client`] plus a [`Transport`] that can wait.
///
/// Holds no protocol knowledge of its own. Every byte in or out goes through
/// the client, so what a test exercises here is exactly what a browser runs.
pub struct EngineStore<T: Transport> {
    transport: T,
    client: Client,
    outbox: Outbox,
    next_write_id: u64,
    /// How many times the outbox re-sent something. Reported, so "it worked"
    /// and "it worked first time" are distinguishable.
    pub resubmits: u64,
    /// Writes sent with an `Expect::Any` read: forced (sdk#235). A `Lost` one
    /// falls instead of going again.
    forced: std::collections::BTreeSet<u64>,
}

impl<T: Transport> EngineStore<T> {
    pub fn new(transport: T) -> EngineStore<T> {
        EngineStore {
            transport,
            client: Client::new(),
            outbox: Outbox::new(),
            next_write_id: 1,
            resubmits: 0,
            forced: std::collections::BTreeSet::new(),
        }
    }

    /// The state machine underneath. Exposed so a host can reach the pump.
    pub fn client(&mut self) -> &mut Client {
        &mut self.client
    }

    /// Events the app may want. Drained, never dropped.
    pub fn take_events(&mut self) -> Vec<Event> {
        self.client.take_events()
    }

    /// Messages this build could not use, by reason.
    pub fn dropped(&self) -> &[protocol::Dropped] {
        &self.client.dropped
    }

    /// The transport, so a test can ask what it did to the stream.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Turn the call tree on, with the clock the app already uses.
    ///
    /// The clock is HANDED IN. The engine has none — it is sans-IO, which is
    /// what makes it testable — so every duration in a trace is the client's
    /// own measurement of its own wait, which is the number an app cares
    /// about anyway.
    pub fn trace_on(&mut self, now_ms: Box<dyn Fn() -> u64>) {
        self.client.trace_on(now_ms);
        self.pump();
    }

    pub fn trace_off(&mut self) {
        self.client.trace_off();
        self.pump();
    }

    /// The call tree for one operation, if it was traced.
    pub fn trace(&self, of: protocol::TraceOf) -> Option<&Trace> {
        self.client.trace(of)
    }

    /// Send one request and wait for what came of it.
    ///
    /// The whole of this driver: queue it on the client, move the bytes, hand
    /// back what the client decoded. Nothing about the protocol happens here —
    /// pushes were routed to events and traces inside the client, on the way
    /// past, so a caller waiting on a read never has to know a notification
    /// exists.
    fn ask(&mut self, r: &Request) -> Vec<Reply> {
        self.client.send(r);
        for _ in 0..MAX_PUMP_ROUNDS {
            self.pump();
            if self.client.has_replies() {
                break;
            }
            // Nothing yet. A connection may answer a request on a LATER
            // round — that is what a reordered stream is — so receive again
            // rather than concluding silence. Bounded: a node that never
            // answers must produce an answer here anyway, because a caller
            // waiting for ever is a UI that never settles.
            let more = self.transport.poll();
            if more.is_empty() {
                break;
            }
            for b in more {
                self.client.on_inbound(&b);
            }
        }
        self.client.drain_replies()
    }

    /// Move whatever is waiting, in both directions.
    ///
    /// BOUNDED. A transport that answers a request with another request would
    /// otherwise spin here, and this is called from a UI thread: "eventually"
    /// there means "never, visibly". What is not moved this round is still
    /// queued for the next call.
    fn pump(&mut self) {
        for _ in 0..MAX_PUMP_ROUNDS {
            let out = self.client.take_outbound();
            if out.is_empty() {
                return;
            }
            for bytes in out {
                for reply in self.transport.exchange(&bytes) {
                    self.client.on_inbound(&reply);
                }
            }
        }
    }

    /// Ask to be told when a range changes.
    ///
    /// The engine may refuse — it has a cap, and the node beneath it has one
    /// it does not control. The answer says which, because a client that
    /// believes it is subscribed and is not waits for ever, and nothing it
    /// can see would tell it so.
    ///
    /// There is no release path and that is deliberate: the platform has no
    /// unsubscribe, so an `unsubscribe` here would drop the ENGINE's copy
    /// while the node went on waking the delegate. `forget_subscription` says
    /// what it actually does.
    pub fn subscribe_range(&mut self, sub_id: u64, lo: &[u8], hi: &[u8]) -> Option<bool> {
        for r in self.ask(&Request::SubscribeRange {
            sub_id,
            lo: protocol::Bound::Included(lo.to_vec()),
            hi: protocol::Bound::Excluded(hi.to_vec()),
        }) {
            if let Reply::Subscribed { accepted, .. } = r {
                return Some(accepted == protocol::Accepted::Yes);
            }
        }
        None
    }

    /// Drop the ENGINE's copy of a subscription.
    ///
    /// Named for what it does. It does not and cannot release the node's: the
    /// platform has no unsubscribe, so the delegate goes on being woken for
    /// contracts its engine has forgotten. That is ordinary, and calling this
    /// `unsubscribe` would promise something nothing here can deliver.
    pub fn forget_subscription(&mut self, sub_id: u64) {
        let _ = self.ask(&Request::Unsubscribe { sub_id });
    }

    /// Hash a value as the outbox compares them.
    fn pre(v: Option<&[u8]>) -> Option<PreImage> {
        v.map(|b| *blake3::hash(b).as_bytes())
    }

    /// Submit edits and drive the outbox until they are with the engine.
    ///
    /// Bounded. An outbox that cannot make progress must return rather than
    /// spin: the caller is a UI thread in a browser, and "eventually" there
    /// means "never, visibly".
    ///
    /// It says what it READ (M2): the outbox sends it as a Commit.
    fn submit_reading(&mut self, reads: Vec<(Vec<u8>, protocol::Expect)>, edits: Vec<protocol::Op>) {
        let write_id = self.next_write_id;
        self.next_write_id += 1;

        // What each key looks like NOW, so a `Lost` write can be compared
        // against the state the app actually made its edit against.
        let keys: Vec<Vec<u8>> = edits
            .iter()
            .map(|o| match o {
                protocol::Op::Put(k, _) => k.clone(),
                protocol::Op::Delete(k) => k.clone(),
            })
            .collect();
        let pre: Vec<(Vec<u8>, Option<PreImage>)> = keys
            .iter()
            .map(|k| {
                // A read that could not be answered gives NO pre-image, which
                // the outbox treats as "I cannot compare" rather than as "it
                // was absent" — so a Lost write over an unreadable key is a
                // reported conflict, never a silent overwrite.
                let now = self.read(k).ok().flatten();
                (k.clone(), Self::pre(now.as_deref()))
            })
            .collect();

        if reads.iter().any(|(_, e)| *e == protocol::Expect::Any) {
            self.forced.insert(write_id);
        }
        self.outbox.push_reading(write_id, edits, pre, reads);
        self.drive_outbox(&keys);
    }

    /// Send what is QUEUED, with no new edit of its own.
    ///
    /// The page does this on its tick. It is what a write refused `Busy`, or
    /// one the engine has not answered yet — parked until the head is
    /// recovered (sdk#223) — waits for: without it, a write made at open sits
    /// in the outbox until the app happens to write again.
    pub fn drain_queued(&mut self) {
        self.drive_outbox(&[]);
    }

    /// One bounded pass of the outbox. `keys` are the keys of the write just
    /// submitted, for comparing a `Lost` one against what is there now.
    fn drive_outbox(&mut self, keys: &[Vec<u8>]) {
        let mut rounds = 0;
        while !self.outbox.is_empty() {
            rounds += 1;
            if rounds > 64 {
                // Not a panic and not a silent drop: the write stays in the
                // outbox and the next call carries on with it.
                return;
            }
            let Some(req) = self.outbox.next_to_send() else {
                return;
            };
            let id = match &req {
                Request::Write { write_id, .. } | Request::Commit { write_id, .. } => *write_id,
                _ => unreachable!("the outbox only sends writes"),
            };
            let replies = self.ask(&req);
            let mut answered = false;
            for r in replies {
                if let Some((write_id, state)) = self.client.own_write_state(&r) {
                    if write_id != id {
                        continue;
                    }
                    answered = true;
                    match state {
                        // A FORCED write told Lost falls, NAMED, and is not
                        // re-sent (sdk#235): no premise, so a re-send is blind.
                        WriteState::Lost if self.forced.contains(&id) => {
                            self.forced.remove(&id);
                            self.outbox.settle(id, WriteState::Failed);
                            self.client.events.push(Event::ForcedLost { write_id: id });
                        }
                        // Refused at the door (sdk#235): never re-sent.
                        WriteState::Unread => {
                            self.forced.remove(&id);
                            self.outbox.settle(id, state);
                            self.client.events.push(Event::Unread { write_id: id });
                        }
                        WriteState::Lost => {
                            let current: std::collections::BTreeMap<Vec<u8>, Option<PreImage>> =
                                keys.iter()
                                    .map(|k| {
                                        let v = self.read(k).ok().flatten();
                                        (k.clone(), Self::pre(v.as_deref()))
                                    })
                                    .collect();
                            match self
                                .outbox
                                .settle_lost(id, |k| current.get(k).copied().flatten())
                            {
                                Lost::Resubmitted => self.resubmits += 1,
                                Lost::Conflict { write_id, keys } => {
                                    self.client.events.push(Event::Conflict { write_id, keys })
                                }
                            }
                        }
                        WriteState::Failed => {
                            self.outbox.settle(id, state);
                            self.client.events.push(Event::Failed { write_id: id });
                        }
                        // M2: a read no longer held, nothing applied; terminal,
                        // and never re-sent (the outbox ends it).
                        WriteState::Conflict => {
                            self.outbox.settle(id, state);
                            self.client.events.push(Event::Conflict { write_id: id, keys: keys.to_vec() });
                        }
                        other => {
                            if other == WriteState::Busy {
                                self.resubmits += 1;
                            }
                            self.outbox.settle(id, other);
                        }
                    }
                }
            }
            if !answered {
                // The engine said nothing about this write. Leave it queued
                // rather than assume either way — the next call retries it,
                // and a write assumed accepted is a write silently lost.
                return;
            }
        }
    }

    fn read(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        let replies = self.ask(&Request::Get {
            req_id: 0,
            key: key.to_vec(),
        });
        for r in replies {
            match r {
                Reply::Value { value, .. } => return Ok(value),
                // A read the engine could not answer is NOT an absent key.
                // `Ok(None)` would tell the caller the key does not exist,
                // which is a different and wrong fact, and the caller would
                // act on it — showing an empty list, or writing over
                // something. The error says "ask again", which is the truth.
                Reply::Unavailable { .. } => return Err(StoreError::Unavailable),
                _ => {}
            }
        }
        Err(StoreError::NoAnswer)
    }

    /// Ask the engine who it is, where its head is, and what root it stands on.
    pub fn identity(&mut self) -> Read<(String, u64, [u8; 32])> {
        for r in self.ask(&Request::Identity) {
            if let Reply::Identity {
                engine,
                head_seq,
                head_root,
                ..
            } = r
            {
                return Ok((engine, head_seq, head_root));
            }
        }
        Err(StoreError::NoAnswer)
    }

    /// The tab is closing.
    pub fn flush(&mut self) {
        let _ = self.ask(&Request::Flush);
    }

    /// Writes still waiting to reach the engine.
    pub fn waiting(&self) -> usize {
        self.outbox.len()
    }
}

impl<T: Transport> Store for EngineStore<T> {
    /// A store-level batch that cannot read first: it says so, key by key, as
    /// `Expect::Any` (sdk#235, W8) — counted by the engine and shown.
    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) -> Result<(), crate::copy::Refused> {
        let reads: Vec<(Vec<u8>, protocol::Expect)> = edits.iter().map(|(k, _)| (k.clone(), protocol::Expect::Any)).collect();
        self.apply_commit(&reads, edits)
    }

    fn apply_commit(&mut self, reads: &[(Vec<u8>, protocol::Expect)], edits: &[(Vec<u8>, Edit)]) -> Result<(), crate::copy::Refused> {
        // ONE write, so the engine applies them as one commit — which is what
        // makes a record and its index entries one fact rather than several —
        // with what it READ, checked where it lands (M2).
        self.submit_reading(
            reads.to_vec(),
            edits
                .iter()
                .map(|(k, e)| match e {
                    Edit::Put(v) => protocol::Op::Put(k.clone(), v.clone()),
                    Edit::Delete => protocol::Op::Delete(k.clone()),
                })
                .collect(),
        );
        // Its outbox holds every write it is given; it refuses none.
        Ok(())
    }
}

/// Reads, which here are round trips — and say so by taking `&mut self`.
///
/// There is no `&self` read on this type to get wrong. The earlier shape had
/// one that refused by name, which was better than panicking and still worse
/// than this: a door that exists only to turn callers away is a door, and
/// someone eventually walks through it.
impl<T: Transport> Reads for EngineStore<T> {
    fn wrong_width(&self, given: IdWidth, wanted: IdWidth) {
        self.client.record_wrong_width(given, wanted);
    }

    fn get(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        self.read(key)
    }

    fn scan(
        &mut self,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        limit: usize,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        if lo >= hi || limit == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        // Bounded by PAGES, not by rows: a range that keeps producing pages
        // is a tree being written to faster than it is read, and returning
        // what has been collected beats never returning.
        for _ in 0..64 {
            let p = self.page(
                protocol::Bound::Included(lo.to_vec()),
                protocol::Bound::Excluded(hi.to_vec()),
                reverse,
                after.clone(),
                256,
            )?;
            let done = p.cursor.is_none() || p.entries.is_empty();
            for e in p.entries {
                if out.len() == limit {
                    return Ok(out);
                }
                out.push(e);
            }
            if done {
                break;
            }
            after = p.cursor;
        }
        Ok(out)
    }

    fn root(&mut self) -> Read<[u8; 32]> {
        self.identity().map(|(_, _, root)| root)
    }

    fn changes_since(
        &mut self,
        from: [u8; 32],
        lo: &[u8],
        hi: &[u8],
        max_entries: u32,
    ) -> Read<Delta> {
        let replies = self.ask(&Request::ChangesSince {
            req_id: 0,
            from,
            lo: protocol::Bound::Included(lo.to_vec()),
            hi: protocol::Bound::Excluded(hi.to_vec()),
            max_entries,
        });
        for r in replies {
            match r {
                Reply::Delta {
                    changes,
                    cursor,
                    new_root,
                    ..
                } => {
                    return Ok(Delta::Changes {
                        changes,
                        cursor,
                        new_root,
                    })
                }
                Reply::FullReloadRequired { new_root, .. } => {
                    return Ok(Delta::FullReloadRequired { new_root })
                }
                // A delta is never answered `Unavailable` by an engine that
                // understands the request — `FullReloadRequired` is its
                // degraded answer. One from an OLDER engine is treated the
                // same way a client should treat any delta it cannot get: do
                // the full read.
                Reply::Unavailable { .. } => {
                    return Ok(Delta::FullReloadRequired {
                        new_root: self.root()?,
                    })
                }
                _ => {}
            }
        }
        Err(StoreError::NoAnswer)
    }
}

impl<T: Transport> EngineStore<T> {
    /// One page of a range.
    ///
    /// Returns the rows and a cursor. The cursor is a KEY, not an offset: an
    /// offset into a tree being written to gives a page that skips or repeats
    /// rows and no way for the caller to know which.
    ///
    /// The engine CLAMPS the page size and reports what it used, so a caller
    /// that asked for more than a page can bear learns that the short answer
    /// is a short page and not the end of the range.
    pub fn page(
        &mut self,
        lo: protocol::Bound,
        hi: protocol::Bound,
        reverse: bool,
        after: Option<Vec<u8>>,
        max_entries: u32,
    ) -> Read<Page> {
        let replies = self.ask(&Request::Range {
            req_id: 0,
            lo,
            hi,
            reverse,
            after,
            max_entries,
        });
        for r in replies {
            match r {
                Reply::Page {
                    entries,
                    cursor,
                    max_entries: used,
                    ..
                } => {
                    return Ok(Page {
                        entries,
                        cursor,
                        page_size_used: used,
                    })
                }
                // An empty page here would say "the range is empty", which is
                // a wrong answer wearing the shape of a right one.
                Reply::Unavailable { .. } => return Err(StoreError::Unavailable),
                _ => {}
            }
        }
        Err(StoreError::NoAnswer)
    }

    /// Every row of a range, a page at a time.
    ///
    /// Bounded by pages, not by rows: a range that keeps producing pages is a
    /// tree being written to faster than it is read, and returning what has
    /// been collected beats never returning.
    pub fn list(
        &mut self,
        lo: protocol::Bound,
        hi: protocol::Bound,
        reverse: bool,
    ) -> Read<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        for _ in 0..64 {
            let p = self.page(lo.clone(), hi.clone(), reverse, after.clone(), 256)?;
            let done = p.cursor.is_none() || p.entries.is_empty();
            out.extend(p.entries);
            if done {
                break;
            }
            after = p.cursor;
        }
        Ok(out)
    }

    /// A read, which is a round trip. Kept as an inherent method because the
    /// live driver and the tests reach for it by name.
    pub fn read_mut(&mut self, key: &[u8]) -> Read<Option<Vec<u8>>> {
        self.read(key)
    }
}
