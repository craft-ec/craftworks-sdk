//! The engine, hosted in a Freenet delegate.
//!
//! This crate does TRANSLATION and SCHEDULING, and no policy. The core
//! (`engine`) decides what must happen and in what order; this decides when
//! the node is asked, and how much of it fits in one `process()` return.
//!
//! The division is not stylistic. A delegate gets fresh linear memory on
//! every call (F32), so the engine is rebuilt from its context each time and
//! nothing may be remembered here either. The platform's numbers — four GETs
//! a return, eight PUTs, 400 KiB of context, five seconds — live in
//! [`schedule::Limits`] where a node that changes them is a different value
//! and not a different design.

pub mod blocks;
pub mod schedule;
pub mod shell;
pub mod wire;
