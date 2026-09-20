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

use crate::store::{Delta, Edit, Read, Reads, Store, StoreError};
use crate::trace::{Trace, Traces};
use protocol::outbox::{Lost, Outbox, PreImage};
use protocol::{Reply, Request, WriteState};

/// How bytes get to the engine and back.
///
/// One call, one exchange: send a request, receive whatever replies came of
/// it. A websocket in a browser, a direct call in a test. The SDK does not
/// care which, and keeping it a trait is what lets the whole write path be
/// tested with no node at all.
pub trait Transport {
    /// Send one encoded request; return the encoded replies it produced.
    fn exchange(&mut self, request: &[u8]) -> Vec<Vec<u8>>;
}

/// One page of a range, and how to continue it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// Pass back as `after`. `None` means the range is exhausted.
    pub cursor: Option<Vec<u8>>,
    /// What the engine actually used, which may be less than was asked for.
    pub page_size_used: u32,
}

/// What happened that an app might want to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A write from before a restart was not applied, because someone else
    /// changed a key it touched. The newer value stands.
    ///
    /// Surfaced, never a prompt: the decision is already made.
    Conflict { write_id: u64, keys: Vec<Vec<u8>> },
    /// A write will never be applied and re-submitting would not help.
    Failed { write_id: u64 },
    /// Something in a subscribed range changed. An ACCELERATOR: a binding
    /// that never heard this would still be correct, on its backstop.
    Changed {
        sub_id: u64,
        new_root: [u8; 32],
        why: protocol::Why,
    },
    /// The engine has stopped comparing a range. Reload it to retry.
    ///
    /// Surfaced rather than swallowed: a binding whose notifications quietly
    /// stopped looks exactly like one whose data quietly stopped changing.
    Stale { sub_id: u64 },
}

pub struct EngineStore<T: Transport> {
    transport: T,
    outbox: Outbox,
    next_write_id: u64,
    /// Things the app may want to hear about. Drained, never dropped.
    pub events: Vec<Event>,
    /// How many times the outbox re-sent something. Reported, so "it worked"
    /// and "it worked first time" are distinguishable.
    pub resubmits: u64,
    /// Call trees, when tracing is on.
    traces: Traces,
    /// The client's clock, as a function, because this crate compiles to wasm
    /// and to a host binary and must not reach for one of its own.
    now_ms: Option<Box<dyn Fn() -> u64>>,
}

impl<T: Transport> EngineStore<T> {
    pub fn new(transport: T) -> EngineStore<T> {
        EngineStore {
            transport,
            outbox: Outbox::new(),
            next_write_id: 1,
            events: Vec::new(),
            resubmits: 0,
            traces: Traces::default(),
            now_ms: None,
        }
    }

    /// Turn the call tree on, with the clock the app already uses.
    ///
    /// The clock is HANDED IN. The engine has none — it is sans-IO, which is
    /// what makes it testable — so every duration in a trace is the client's
    /// own measurement of its own wait, which is the number an app cares
    /// about anyway.
    pub fn trace_on(&mut self, now_ms: Box<dyn Fn() -> u64>) {
        self.now_ms = Some(now_ms);
        let _ = self.ask(&Request::Trace { on: true });
    }

    pub fn trace_off(&mut self) {
        let _ = self.ask(&Request::Trace { on: false });
        self.now_ms = None;
    }

    /// The call tree for one operation, if it was traced.
    pub fn trace(&self, of: protocol::TraceOf) -> Option<&Trace> {
        self.traces.of(of)
    }

    /// Send one request and take its replies, harvesting anything PUSHED.
    ///
    /// A `Changed` arrives on whatever exchange happens to be in flight — no
    /// client asked for it, so it belongs to no particular request. Harvesting
    /// here means every call drains them, rather than only a call that thought
    /// to look; a notification that arrives on the exchange for an unrelated
    /// read is the ordinary case, not a special one.
    fn ask(&mut self, r: &Request) -> Vec<Reply> {
        let bytes = protocol::encode_request(protocol::CURRENT, r);
        let replies: Vec<Reply> = self
            .transport
            .exchange(&bytes)
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .collect();
        let mut out = Vec::with_capacity(replies.len());
        for reply in replies {
            match reply {
                Reply::Changed {
                    sub_id,
                    new_root,
                    why,
                    ..
                } => {
                    if why == protocol::Why::Stale {
                        self.events.push(Event::Stale { sub_id });
                    }
                    self.events.push(Event::Changed {
                        sub_id,
                        new_root,
                        why,
                    });
                }
                Reply::Step { of, depth, what, n } => {
                    // Stamped as it LANDS. There is no other honest moment:
                    // the engine has no clock, so the only time anyone can
                    // measure is the client's own wait.
                    if let Some(now) = &self.now_ms {
                        let t = now();
                        self.traces.record(of, depth, what, n, t);
                    }
                }
                other => out.push(other),
            }
        }
        out
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

    /// Take the events that have accumulated. Drained, never dropped.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
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
    fn submit(&mut self, edits: Vec<protocol::Op>) {
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

        self.outbox.push_against(write_id, edits, pre);

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
                Request::Write { write_id, .. } => *write_id,
                _ => unreachable!("the outbox only sends writes"),
            };
            let replies = self.ask(&req);
            let mut answered = false;
            for r in replies {
                if let Reply::WriteState { write_id, state } = r {
                    if write_id != id {
                        continue;
                    }
                    answered = true;
                    match state {
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
                                    self.events.push(Event::Conflict { write_id, keys })
                                }
                            }
                        }
                        WriteState::Failed => {
                            self.outbox.settle(id, state);
                            self.events.push(Event::Failed { write_id: id });
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
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.submit(vec![protocol::Op::Put(key.to_vec(), value.to_vec())]);
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        // A read that FAILED is not a key that was absent. Reported as "did
        // not exist" here would be a lie the caller acts on; the write still
        // goes, because deleting a key is right whether or not it was there.
        let existed = matches!(self.read(key), Ok(Some(_)));
        self.submit(vec![protocol::Op::Delete(key.to_vec())]);
        existed
    }

    fn apply_batch(&mut self, edits: &[(Vec<u8>, Edit)]) {
        // ONE write, so the engine applies them as one commit — which is what
        // makes a record and its index entries one fact rather than several.
        self.submit(
            edits
                .iter()
                .map(|(k, e)| match e {
                    Edit::Put(v) => protocol::Op::Put(k.clone(), v.clone()),
                    Edit::Delete => protocol::Op::Delete(k.clone()),
                })
                .collect(),
        );
    }
}

/// Reads, which here are round trips — and say so by taking `&mut self`.
///
/// There is no `&self` read on this type to get wrong. The earlier shape had
/// one that refused by name, which was better than panicking and still worse
/// than this: a door that exists only to turn callers away is a door, and
/// someone eventually walks through it.
impl<T: Transport> Reads for EngineStore<T> {
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
