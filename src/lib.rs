//! App-facing SDK (ARCHITECTURE.md §19). Grows one surface per build phase:
//! `collection` (1) · `identity` `query` `subscribe` (3) · `inbox` `edge`
//! `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).
//!
//! Every capability is written once in Rust and exposed to JavaScript from
//! [`js`], so the builder and apps get it the phase it lands.

pub mod db;
pub mod id;
pub mod record;
pub mod schema;
pub mod store;
pub mod tree_store;

pub use db::{Db, Record, Scan};
pub use freenet_prolly::Cid;
pub use id::{Env, RKey, SystemEnv};
pub use schema::{Field, Kind, Schema};
pub use store::{Edit, MemStore, Store};
pub use tree_store::{Stats, TreeStore};

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content id of `bytes`: `BLAKE3(bytes)`, the form apps and links use.
///
/// NOT a Block's network key. A block is addressed by `BLAKE3(kind ‖ body)`
/// (`freenet_prolly::block_id`), so the same bytes have a different id as a
/// stored block than they do here. Frozen in `tests/vectors.txt` and shared
/// with the JS surface, so this is what it has always meant — but the two being
/// different things with one name is worth settling before apps put either in a
/// link.
pub fn cid(bytes: &[u8]) -> Cid {
    *blake3::hash(bytes).as_bytes()
}

/// [`cid`] as hex.
pub fn cid_hex(bytes: &[u8]) -> String {
    hex(&cid(bytes))
}

#[cfg(target_arch = "wasm32")]
pub mod js {
    //! The JavaScript surface. Thin: no logic lives here. Structured values cross
    //! the boundary as JSON strings; `js/wrap.js` gives apps plain objects.
    use crate::{id, Scan, Schema, SystemEnv, TreeStore};
    use serde_json::{Map, Value};
    use wasm_bindgen::prelude::*;

    /// SDK version, so an app can tell which build it loaded.
    #[wasm_bindgen]
    pub fn version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Content id of `bytes`, as hex.
    #[wasm_bindgen(js_name = cidHex)]
    pub fn cid_hex(bytes: &[u8]) -> String {
        crate::cid_hex(bytes)
    }

    fn err(e: impl std::fmt::Display) -> JsError {
        JsError::new(&e.to_string())
    }
    fn fields(json: &str) -> Result<Map<String, Value>, JsError> {
        match serde_json::from_str(json).map_err(err)? {
            Value::Object(m) => Ok(m),
            _ => Err(err("fields must be an object")),
        }
    }
    fn rkey(hex: &str) -> Result<id::RKey, JsError> {
        id::from_hex(hex).ok_or_else(|| err("record id must be 32 hex characters"))
    }
    fn json<T: serde::Serialize>(v: &T) -> Result<String, JsError> {
        serde_json::to_string(v).map_err(err)
    }

    /// A database on the real prolly tree, in memory. The same surface will sit
    /// over the node's engine later; what changes is where the blocks live.
    #[wasm_bindgen]
    pub struct Db(crate::Db<TreeStore, SystemEnv>);

    #[wasm_bindgen]
    impl Db {
        #[wasm_bindgen(constructor)]
        pub fn new() -> Db {
            let mut device = [0u8; 4];
            let _ = getrandom::getrandom(&mut device);
            Db(crate::Db::new(TreeStore::new(), SystemEnv, device))
        }
        pub fn define(&mut self, domain: &str, schema: &str) -> Result<(), JsError> {
            let s: Schema = serde_json::from_str(schema).map_err(err)?;
            self.0.define(domain, &s).map_err(err)
        }
        pub fn schema(&self, domain: &str) -> Result<String, JsError> {
            json(&self.0.schema(domain))
        }
        pub fn domains(&self) -> Result<String, JsError> {
            json(&self.0.domains())
        }
        /// Errors here are ordinary JavaScript `Error`s the app can catch —
        /// not a wasm abort. With `panic = abort` a limit breach that reached
        /// the store would take the whole SDK down with it, so `Db` screens
        /// first and returns this instead.
        pub fn put(&mut self, domain: &str, f: &str) -> Result<String, JsError> {
            json(&self.0.put(domain, &fields(f)?).map_err(err)?)
        }
        pub fn update(&mut self, domain: &str, id: &str, patch: &str) -> Result<String, JsError> {
            json(
                &self
                    .0
                    .update(domain, &rkey(id)?, &fields(patch)?)
                    .map_err(err)?,
            )
        }
        pub fn get(&self, domain: &str, id: &str) -> Result<String, JsError> {
            json(&self.0.get(domain, &rkey(id)?).map_err(err)?)
        }
        pub fn delete(&mut self, domain: &str, id: &str) -> Result<bool, JsError> {
            self.0.delete(domain, &rkey(id)?).map_err(err)
        }
        /// `after` is a record id or the empty string.
        pub fn scan(
            &self,
            domain: &str,
            reverse: bool,
            limit: usize,
            after: &str,
        ) -> Result<String, JsError> {
            let after = if after.is_empty() {
                None
            } else {
                Some(rkey(after)?)
            };
            json(
                &self
                    .0
                    .scan(
                        domain,
                        Scan {
                            reverse,
                            limit,
                            after,
                        },
                    )
                    .map_err(err)?,
            )
        }
        /// The tree's root hash, as hex. The whole database in 32 bytes: it
        /// changes with every write and is the same for any two databases
        /// holding the same records, whatever order they were written in.
        pub fn root(&self) -> String {
            crate::hex(&self.0.store().root())
        }

        /// Blocks held, bytes held, and the tree's height.
        pub fn stats(&self) -> Result<String, JsError> {
            let s = self.0.store().stats();
            json(&serde_json::json!({
                "blocks": s.blocks, "bytes": s.bytes, "height": s.height
            }))
        }

        pub fn count(&self, domain: &str) -> Result<usize, JsError> {
            self.0.count(domain).map_err(err)
        }
    }

    impl Default for Db {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    /// The same vectors are checked from JavaScript (`tests/js/cid.test.cjs`), so a
    /// binding that mangles bytes on the way in or out cannot pass both.
    #[test]
    fn cid_vectors_hold_in_rust() {
        let mut n = 0;
        for line in include_str!("../tests/vectors.txt").lines() {
            let (input, want) = line.split_once(' ').unwrap();
            let bytes: Vec<u8> = (0..input.len() / 2)
                .map(|i| u8::from_str_radix(&input[i * 2..i * 2 + 2], 16).unwrap())
                .collect();
            assert_eq!(super::cid_hex(&bytes), want, "input {input}");
            n += 1;
        }
        assert!(n >= 4, "only {n} vectors");
    }
}
