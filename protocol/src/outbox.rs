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

/// A write the app made, waiting its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub write_id: u64,
    pub ops: Vec<crate::Op>,
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
            attempts: 0,
        });
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
        Some(Request::Write {
            write_id: p.write_id,
            ops: p.ops.clone(),
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
