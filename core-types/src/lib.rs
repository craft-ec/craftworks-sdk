//! LAYER 0: the basic facts every crate in the SDK uses, each stated ONCE.
//!
//! No dependencies, so every crate above can use it. A crate that needs hex or
//! a name rule calls this; it never writes its own. Anything added here must be
//! a fact at least two crates need.

pub mod hex;
pub mod name;
