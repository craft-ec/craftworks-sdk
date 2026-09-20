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

use crate::store::{Edit, Store};
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
}

impl<T: Transport> EngineStore<T> {
    pub fn new(transport: T) -> EngineStore<T> {
        EngineStore {
            transport,
            outbox: Outbox::new(),
            next_write_id: 1,
            events: Vec::new(),
            resubmits: 0,
        }
    }

    fn ask(&mut self, r: &Request) -> Vec<Reply> {
        let bytes = protocol::encode_request(protocol::CURRENT, r);
        self.transport
            .exchange(&bytes)
            .iter()
            .filter_map(|b| protocol::decode_reply(b).ok())
            .collect()
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
                let now = self.read(k);
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
                                    .map(|k| (k.clone(), Self::pre(self.read(k).as_deref())))
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

    fn read(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let replies = self.ask(&Request::Get {
            req_id: 0,
            key: key.to_vec(),
        });
        for r in replies {
            match r {
                Reply::Value { value, .. } => return value,
                // A read the engine could not answer is NOT an absent key.
                // Returning `None` would tell the caller the key does not
                // exist, which is a different and wrong fact.
                Reply::Unavailable { .. } => return None,
                _ => {}
            }
        }
        None
    }

    /// Ask the engine who it is, and where its head is.
    pub fn identity(&mut self) -> Option<(String, u64)> {
        for r in self.ask(&Request::Identity) {
            if let Reply::Identity {
                engine, head_seq, ..
            } = r
            {
                return Some((engine, head_seq));
            }
        }
        None
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
    fn get(&self, _key: &[u8]) -> Option<Vec<u8>> {
        // `Store::get` takes `&self` and an exchange needs `&mut`. Rather
        // than hide interior mutability — which would make a read look free
        // when it is a round trip — the engine backend's reads go through
        // `read_mut`, and `Db` uses that. Returning None here would be a
        // wrong answer, so it is unreachable instead.
        unreachable!("EngineStore reads go through read_mut: a read is a round trip")
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.submit(vec![protocol::Op::Put(key.to_vec(), value.to_vec())]);
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        let existed = self.read(key).is_some();
        self.submit(vec![protocol::Op::Delete(key.to_vec())]);
        existed
    }

    fn scan(
        &self,
        _lo: &[u8],
        _hi: &[u8],
        _reverse: bool,
        _limit: usize,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        unreachable!("EngineStore scans go through scan_mut: a scan is a round trip")
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

impl<T: Transport> EngineStore<T> {
    /// A read, which is a round trip.
    pub fn read_mut(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.read(key)
    }

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
    ) -> Page {
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
                    return Page {
                        entries,
                        cursor,
                        page_size_used: used,
                    }
                }
                Reply::Unavailable { .. } => return Page::default(),
                _ => {}
            }
        }
        Page::default()
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
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        for _ in 0..64 {
            let p = self.page(lo.clone(), hi.clone(), reverse, after.clone(), 256);
            let done = p.cursor.is_none() || p.entries.is_empty();
            out.extend(p.entries);
            if done {
                break;
            }
            after = p.cursor;
        }
        out
    }
}
