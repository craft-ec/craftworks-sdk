//! The ids and records the page, the signer and the probes share with the
//! contracts: a Block contract's instance id, and the head as a Register
//! record. Moved out of `engine-delegate` so they outlive it (sdk#209's Q8).
//!
//! Sans-IO and off the client-API allowlist: no `freenet-stdlib` in its
//! normal dependencies (only as a dev-dependency, to pin the derivation).

pub mod block;
pub mod register;
pub mod site;
