//! The probed fixtures: the only supported way to build the thing under test.
//!
//! # Why this crate exists
//!
//! Every test that built the system itself built it a little differently, and
//! the differences decided what the tests could SEE.
//!
//! - **A clock that never advanced, written out eleven times.**
//!   `CachedStore::new(Box::new(|| 0))` appeared in six files. Nothing in the
//!   SDK's tests could observe time passing, so a whole class of behaviour was
//!   untestable by construction rather than by decision. [`Clock`] is one a
//!   test can drive.
//! - **The node, hand-rolled per file.** A tab after the switch-over is the
//!   in-page engine's `page::Server` over a node; [`page_node`] is that, with
//!   the REAL signer and the REAL Register merge on the far side, shared by
//!   every tab on it. (The delegate-era cross-call `Node` and `FullNode`, which
//!   rebuilt the Shell from its context on every call, went with the Shell.)

pub mod page_node;
pub use page_node::{PageConn, PageNode};

use std::cell::RefCell;
use std::rc::Rc;

/// A clock a test can DRIVE.
///
/// The thing it replaces is `Box::new(|| 0)` — a closure returning zero,
/// written by hand in six files. It is not that those tests chose a frozen
/// clock; it is that constructing one that moves was extra work in every
/// single test, so nobody did, and "does this behaviour need time?" became a
/// question the suite could not ask.
#[derive(Clone, Default)]
pub struct Clock(Rc<RefCell<u64>>);

impl Clock {
    pub fn new(start_ms: u64) -> Clock {
        Clock(Rc::new(RefCell::new(start_ms)))
    }
    /// Move time forward. The whole point of the type.
    pub fn advance(&self, ms: u64) {
        *self.0.borrow_mut() += ms;
    }
    pub fn now_ms(&self) -> u64 {
        *self.0.borrow()
    }
    /// The closure the SDK's constructors take.
    pub fn as_fn(&self) -> Box<dyn Fn() -> u64> {
        let c = self.0.clone();
        Box::new(move || *c.borrow())
    }
}

/// A `CachedStore` whose clock can be DRIVEN, with the clock handed back.
///
/// Replaces `CachedStore::new(Box::new(|| 0))` — a clock frozen at zero,
/// written out by hand in six files. Nothing in those tests could observe time
/// passing, so "does this behaviour need time?" was a question the suite could
/// not ask. Handing the clock back is the point: a fixture that hid it would
/// be the same frozen clock with better manners.
pub fn cached_store() -> (craftworks_sdk::CachedStore, Clock) {
    let clock = Clock::new(0);
    (craftworks_sdk::CachedStore::new(clock.as_fn()), clock)
}

/// A `CachedStore` on a clock the caller already holds — so several
/// sessions share ONE clock, and advancing it moves them all (the write
/// path's model test runs two).
pub fn cached_store_on(clock: &Clock) -> craftworks_sdk::CachedStore {
    craftworks_sdk::CachedStore::new(clock.as_fn())
}

/// The same, started at a given time.
pub fn cached_store_at(start_ms: u64) -> (craftworks_sdk::CachedStore, Clock) {
    let clock = Clock::new(start_ms);
    (craftworks_sdk::CachedStore::new(clock.as_fn()), clock)
}
