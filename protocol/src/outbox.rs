//! The client's outbox: where a write waits when the engine says `Busy`.
//!
//! The engine takes ONE COMMIT AT A TIME, and a write arriving while one is
//! in flight is refused with `Busy` — nothing applied, no trace left. That is
//! the right answer from a component with no memory: buffering needs somewhere
//! to keep the bytes, and a delegate's memory is gone when the call returns.
//!
//! So the buffer lives HERE, on the side that has a page and can hold them.
//! Three properties, and each is a way the obvious implementation goes wrong:
//!
//! 1. **In order.** Writes are applied in the order the app made them, or two
//!    edits to one key settle the wrong way round.
//! 2. **Exactly once.** A write is re-submitted until it is accepted and not
//!    after — and `Busy` is the only state that may be re-submitted, because
//!    it is the only one that applied nothing.
//! 3. **Nothing is dropped.** A write leaves the outbox when it reaches a
//!    state that says it is safe to forget, and never because the queue was
//!    full or the connection blinked.

use crate::{Request, WriteState};

/// What a key looked like when a write was made against it.
///
/// A HASH, not the bytes. The outbox may hold many writes and a page has a
/// budget; keeping every pre-image would make the outbox as large as the
/// data it is waiting to send. A hash is enough to answer the only question
/// asked of it — did this change underneath us — and cannot be mistaken for
/// a value to write back.
pub type PreImage = [u8; 32];

/// What the outbox decided about a `Lost` write, with no one asked.
///
/// `Lost` means the commit carrying a write will not publish and the engine
/// no longer has it. Something has to happen next, and "the app decides" is
/// not an answer: these run unattended, and an app that must decide either
/// prompts a person or picks blindly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lost {
    /// Every key is as the write expected, so it is sent again — silently,
    /// in order, exactly once, exactly like `Busy`.
    Resubmitted,
    /// At least one key moved underneath it. The NEWER value stands and
    /// nothing is re-applied.
    ///
    /// The app MAY surface this ("your edit from before the restart was not
    /// applied"). It must never be a retry prompt: the decision is already
    /// made, and this is a notification of it.
    Conflict { write_id: u64, keys: Vec<Vec<u8>> },
}

/// A write the app made, waiting its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub write_id: u64,
    pub ops: Vec<crate::Op>,
    /// What each key the write touches looked like when it was submitted.
    ///
    /// `None` for a key that did not exist. Recorded at SUBMIT time, because
    /// that is the state the app made its edit against — reading it later
    /// would compare against whatever happened since, which is the thing
    /// being tested for.
    pub pre: Vec<(Vec<u8>, Option<PreImage>)>,
    /// What it READ (M2, sdk#148): sent with it as a [`Request::Commit`], so
    /// the engine checks it where it lands — on every send, the same reads.
    pub reads: Vec<(Vec<u8>, crate::Expect)>,
    /// How many times it has been sent. For reporting, never for giving up:
    /// the outbox does not decide a write is hopeless, the engine does.
    pub attempts: u32,
}

/// What happened when the outbox was told a write's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// It is on its way and no longer the outbox's problem.
    Accepted,
    /// It will be sent again.
    Requeued,
    /// It ended, and not well. The APP is told; the outbox does not retry it,
    /// because nothing it can do would change the answer.
    Ended(WriteState),
    /// Nothing in the outbox has that id.
    Unknown,
}

#[derive(Debug, Default)]
pub struct Outbox {
    queue: Vec<Pending>,
    /// The write currently with the engine, if any.
    in_flight: Option<u64>,
    /// Ids that have been accepted, so a late duplicate reply cannot put one
    /// back in the queue.
    done: Vec<u64>,
    pub sent: u64,
    pub requeued: u64,
}

impl Outbox {
    pub fn new() -> Outbox {
        Outbox::default()
    }

    /// The app made a write, recording what each key looked like.
    pub fn push_against(
        &mut self,
        write_id: u64,
        ops: Vec<crate::Op>,
        pre: Vec<(Vec<u8>, Option<PreImage>)>,
    ) {
        if self.queue.iter().any(|p| p.write_id == write_id) || self.done.contains(&write_id) {
            return;
        }
        self.queue.push(Pending {
            write_id,
            ops,
            pre,
            reads: Vec::new(),
            attempts: 0,
        });
    }

    /// The app made a write that says what it READ (M2).
    pub fn push_reading(
        &mut self,
        write_id: u64,
        ops: Vec<crate::Op>,
        pre: Vec<(Vec<u8>, Option<PreImage>)>,
        reads: Vec<(Vec<u8>, crate::Expect)>,
    ) {
        self.push_against(write_id, ops, pre);
        if let Some(p) = self.queue.iter_mut().find(|p| p.write_id == write_id) {
            p.reads = reads;
        }
    }

    /// The app made a write.
    pub fn push(&mut self, write_id: u64, ops: Vec<crate::Op>) {
        // An id already queued is the same write, not a second one: the app
        // chose the id precisely so a re-submission is recognisable.
        if self.queue.iter().any(|p| p.write_id == write_id) || self.done.contains(&write_id) {
            return;
        }
        self.queue.push(Pending {
            write_id,
            ops,
            pre: Vec::new(),
            reads: Vec::new(),
            attempts: 0,
        });
    }

    /// A write came back `Lost`. Decide what happens, without asking anyone.
    ///
    /// ALL-OR-NOTHING per write: if ANY key the write touches has moved, none
    /// of it is re-applied. A write is one edit the app made, and applying
    /// half of it produces a state the app never asked for and cannot reason
    /// about — worse than applying none, because none is a state it already
    /// knows how to describe.
    ///
    /// `current` answers "what is this key's hash now?", `None` for absent.
    pub fn settle_lost(
        &mut self,
        write_id: u64,
        current: impl Fn(&[u8]) -> Option<PreImage>,
    ) -> Lost {
        if self.in_flight == Some(write_id) {
            self.in_flight = None;
        }
        let Some(at) = self.queue.iter().position(|p| p.write_id == write_id) else {
            return Lost::Conflict {
                write_id,
                keys: Vec::new(),
            };
        };
        let moved: Vec<Vec<u8>> = self.queue[at]
            .pre
            .iter()
            .filter(|(k, was)| current(k) != *was)
            .map(|(k, _)| k.clone())
            .collect();
        if moved.is_empty() {
            // Nothing changed underneath it, so re-applying it lands exactly
            // the edit the app made against exactly the state it made it
            // against. Silent, because there is nothing for anyone to decide.
            self.requeued += 1;
            return Lost::Resubmitted;
        }
        // Someone else's newer value stands. Dropping this write is the
        // decision — not a step towards one.
        let p = self.queue.remove(at);
        self.done.push(p.write_id);
        Lost::Conflict {
            write_id,
            keys: moved,
        }
    }

    /// The next write to send, if the engine is free.
    ///
    /// Named `next_to_send` rather than `next`: an outbox is not an iterator
    /// and reading it as one would suggest draining it, which is exactly the
    /// mistake — one write goes at a time and the rest wait for an answer.
    ///
    /// ONE at a time, because the engine takes one at a time: sending the
    /// next before the first is answered earns a `Busy` that teaches nothing
    /// and costs a round trip.
    pub fn next_to_send(&mut self) -> Option<Request> {
        if self.in_flight.is_some() {
            return None;
        }
        let p = self.queue.first_mut()?;
        p.attempts += 1;
        self.in_flight = Some(p.write_id);
        self.sent += 1;
        Some(if p.reads.is_empty() {
            Request::Write { write_id: p.write_id, ops: p.ops.clone() }
        } else {
            Request::Commit { write_id: p.write_id, reads: p.reads.clone(), ops: p.ops.clone() }
        })
    }

    /// The engine said something about a write.
    pub fn settle(&mut self, write_id: u64, state: WriteState) -> Settled {
        if self.in_flight == Some(write_id) {
            self.in_flight = None;
        }
        let at = self.queue.iter().position(|p| p.write_id == write_id);
        let Some(at) = at else {
            return Settled::Unknown;
        };
        // `Lost` is not decided here: it needs to know what the keys look
        // like NOW, which this cannot see. `settle_lost` is where it goes,
        // and routing it here instead would drop it as terminal.
        debug_assert!(
            state != WriteState::Lost,
            "a Lost write goes to settle_lost, which can compare pre-images"
        );
        if state.should_resubmit() {
            // Stays where it is, at the FRONT of its order. Moving it to the
            // back would let a later write overtake it, and two edits to one
            // key would settle the wrong way round.
            self.requeued += 1;
            return Settled::Requeued;
        }
        let p = self.queue.remove(at);
        self.done.push(p.write_id);
        if state.terminal() {
            Settled::Ended(state)
        } else {
            // Accepted and beyond: the engine has it, and it will publish on
            // its own confirmations. The outbox's job is done.
            Settled::Accepted
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Ids still waiting, in order.
    pub fn waiting(&self) -> Vec<u64> {
        self.queue.iter().map(|p| p.write_id).collect()
    }
}
