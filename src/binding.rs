//! A BINDING: one range of data, as a component consumes it.
//!
//! This is the shape every reactive framework already wants — React's
//! `useSyncExternalStore`, a Svelte store, a Vue `ref` — and it is only three
//! things:
//!
//! * `subscribe(callback) -> unsubscribe`
//! * `snapshot()`, **referentially stable** between changes
//! * `reload()`, which fetches the DELTA when it can
//!
//! # Why the snapshot's identity is load-bearing
//!
//! `useSyncExternalStore` calls `getSnapshot()` on every render and re-renders
//! when the value is not the SAME OBJECT as last time. A binding that rebuilt
//! its rows on each call would therefore re-render on every render, for ever,
//! whatever the data did. So the rows live behind an `Rc` that is replaced
//! only when the content actually changed — and "actually changed" is decided
//! by the tree's root, not by comparing rows.
//!
//! # `live` is a declaration, and it is the ONLY difference
//!
//! A live binding wants to be TOLD when its range changes; a plain one finds
//! out when it is reloaded. That is the whole of it. Both have the same
//! `snapshot()`, the same `reload()`, the same `subscribe()`. A non-live
//! binding's `subscribe` is a real subscription to THIS object — the callback
//! fires when the rows change — and takes out no engine subscription at all,
//! so flipping `live` never changes the component that consumes it.
//!
//! Subscribe only where the data is genuinely live and a delta is worth
//! having: a feed, a presence list, a document two people have open. Ordinary
//! data is read when it is wanted and is correct then; a subscription on it
//! spends a slot the live data needed.
//!
//! # A notification is an accelerator, never a guarantee
//!
//! The node's delivery is lossy: its notification channel drops when full,
//! and a subscription can be evicted at the cap without anyone being told
//! (F39). So a live binding ALSO reconciles on the client's tick — re-read
//! the root, compare, reload if it moved — and that backstop is the thing
//! that makes it correct. Being notified only makes it sooner.

use crate::store::{Delta, Read, Reads};

/// How a binding is actually being kept up to date.
///
/// Reported rather than assumed. The engine may decline to notify — it has a
/// cap of its own, and the node beneath it has one it does not control — and
/// a binding that silently fell back to polling while the app believed it was
/// notified is the failure this exists to make impossible. A declared
/// degradation is one an app can show, log, or ignore on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMode {
    /// Not a live binding. It updates when reloaded.
    Manual,
    /// The engine accepted a subscription and is expected to notify.
    Notified,
    /// Wanted notifications, did not get them. The backstop is doing the work.
    Polled,
}

/// What a reload did. Reported so "it updated" and "it updated cheaply" are
/// distinguishable — a binding that has silently been doing full reloads for
/// a week looks exactly like one that has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reloads {
    /// Reloads served by a delta.
    pub by_delta: u64,
    /// Reloads that had to read the whole range.
    pub full: u64,
    /// Reloads that found nothing had moved and did no work at all.
    pub unchanged: u64,
}

type Rows = std::rc::Rc<Vec<(Vec<u8>, Vec<u8>)>>;

/// One range, as a component sees it.
pub struct Binding {
    lo: Vec<u8>,
    hi: Vec<u8>,
    live: bool,
    /// The client-chosen subscription id, if this binding asked for one.
    sub_id: Option<u64>,
    /// The root the rows were read at. The client owns this, not the engine —
    /// which is what makes a missed notification recoverable: whatever was
    /// dropped, the next reload diffs across the whole gap in one call.
    at: Option<[u8; 32]>,
    rows: Rows,
    mode: LiveMode,
    /// Called when `rows` is replaced. Framework-facing.
    listeners: Vec<(u64, Box<dyn Fn()>)>,
    next_listener: u64,
    pub reloads: Reloads,
}

impl Binding {
    /// A binding over `[lo, hi)`.
    ///
    /// `live` is the app author's declaration that this data changes while
    /// someone is looking at it. Default false everywhere above this.
    pub fn new(lo: &[u8], hi: &[u8], live: bool) -> Binding {
        Binding {
            lo: lo.to_vec(),
            hi: hi.to_vec(),
            live,
            sub_id: None,
            at: None,
            rows: std::rc::Rc::new(Vec::new()),
            mode: if live {
                // Not `Notified`: nothing has accepted a subscription yet.
                // Claiming it here would be the silent-downgrade failure in
                // its first and simplest form.
                LiveMode::Polled
            } else {
                LiveMode::Manual
            },
            listeners: Vec::new(),
            next_listener: 1,
            reloads: Reloads::default(),
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn mode(&self) -> LiveMode {
        self.mode
    }

    pub fn range(&self) -> (&[u8], &[u8]) {
        (&self.lo, &self.hi)
    }

    pub fn sub_id(&self) -> Option<u64> {
        self.sub_id
    }

    /// The rows, as the same object until they change.
    pub fn snapshot(&self) -> Rows {
        self.rows.clone()
    }

    /// Register a listener; the returned id cancels it.
    ///
    /// A binding that is not live takes one just the same. The callback fires
    /// when THESE rows change, which is what the component needs to know, and
    /// it involves no engine subscription — so a component does not change
    /// when `live` is flipped.
    pub fn subscribe(&mut self, f: Box<dyn Fn()>) -> u64 {
        let id = self.next_listener;
        self.next_listener += 1;
        self.listeners.push((id, f));
        id
    }

    pub fn unsubscribe(&mut self, id: u64) {
        self.listeners.retain(|(i, _)| *i != id);
    }

    /// Record that the engine accepted (or refused) a subscription.
    ///
    /// The engine decides the MECHANISM; the binding only declared that it
    /// wants notifications. A refusal is a downgrade to the backstop, and it
    /// is recorded rather than swallowed.
    pub fn note_subscribed(&mut self, sub_id: u64, accepted: bool) {
        if !self.live {
            return;
        }
        self.sub_id = accepted.then_some(sub_id);
        self.mode = if accepted {
            LiveMode::Notified
        } else {
            LiveMode::Polled
        };
    }

    /// The engine could not, or would not, keep notifying this range.
    pub fn note_downgraded(&mut self) {
        if self.live {
            self.mode = LiveMode::Polled;
            self.sub_id = None;
        }
    }

    /// Bring the rows up to date, by delta where possible.
    ///
    /// The backstop AND the notification path both land here, deliberately:
    /// one code path means the rarely-exercised one is the one that runs all
    /// the time. It begins by comparing roots, so a reload with nothing to do
    /// costs one round trip and no reading.
    pub fn reload<S: Reads>(&mut self, store: &mut S) -> Read<()> {
        let now = store.root()?;
        if self.at == Some(now) {
            self.reloads.unchanged += 1;
            return Ok(());
        }
        let Some(from) = self.at else {
            // Nothing seen yet: there is no delta to take, only the range.
            return self.full(store, now);
        };
        match store.changes_since(from, &self.lo, &self.hi, 1024)? {
            Delta::Changes {
                changes,
                cursor,
                new_root,
            } => {
                if cursor.is_some() {
                    // More changes than one page holds. Applying a PARTIAL
                    // delta and recording the new root would leave the
                    // binding claiming to be at a root it is not at — which
                    // is worse than the full read it is trying to avoid,
                    // because nothing afterwards would ever notice.
                    return self.full(store, new_root);
                }
                let mut rows = (*self.rows).clone();
                for (k, v) in changes {
                    match rows.binary_search_by(|(x, _)| x.as_slice().cmp(&k)) {
                        Ok(i) => match v {
                            Some(v) => rows[i].1 = v,
                            None => {
                                rows.remove(i);
                            }
                        },
                        Err(i) => {
                            if let Some(v) = v {
                                rows.insert(i, (k, v));
                            }
                        }
                    }
                }
                self.reloads.by_delta += 1;
                self.set(rows, new_root);
                Ok(())
            }
            Delta::FullReloadRequired { new_root } => self.full(store, new_root),
        }
    }

    fn full<S: Reads>(&mut self, store: &mut S, at: [u8; 32]) -> Read<()> {
        let rows = store.scan(&self.lo, &self.hi, false, usize::MAX)?;
        self.reloads.full += 1;
        self.set(rows, at);
        Ok(())
    }

    /// Replace the rows, and tell the listeners — but only if they CHANGED.
    ///
    /// The identity check is the whole contract with the frameworks: a
    /// snapshot that is a new object every time makes `useSyncExternalStore`
    /// re-render on every render. So the root is recorded either way, and the
    /// `Rc` is replaced only when the content differs.
    fn set(&mut self, rows: Vec<(Vec<u8>, Vec<u8>)>, at: [u8; 32]) {
        self.at = Some(at);
        if *self.rows == rows {
            return;
        }
        self.rows = std::rc::Rc::new(rows);
        for (_, f) in &self.listeners {
            f();
        }
    }
}
