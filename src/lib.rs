//! App-facing SDK (ARCHITECTURE.md §19). Grows one surface per build phase:
//! `collection` (1) · `identity` `query` `subscribe` (3) · `inbox` `edge`
//! `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).

pub use freenet_prolly::{cid, Cid};
