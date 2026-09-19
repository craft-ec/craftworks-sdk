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

pub use db::{Db, Record, Scan};
pub use freenet_prolly::{cid, Cid};
pub use id::{Env, RKey, SystemEnv};
pub use schema::{Field, Kind, Schema};
pub use store::{MemStore, Store};

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content id of `bytes`, as hex — the form apps and links use.
pub fn cid_hex(bytes: &[u8]) -> String {
    hex(&cid(bytes))
}

#[cfg(target_arch = "wasm32")]
pub mod js {
    //! The JavaScript surface. Thin: no logic lives here. Structured values cross
    //! the boundary as JSON strings; `js/wrap.js` gives apps plain objects.
    use crate::{id, MemStore, Scan, Schema, SystemEnv};
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

    /// An in-memory database. The same surface will sit over the tree and, later,
    /// the node's engine.
    #[wasm_bindgen]
    pub struct Db(crate::Db<MemStore, SystemEnv>);

    #[wasm_bindgen]
    impl Db {
        #[wasm_bindgen(constructor)]
        pub fn new() -> Db {
            let mut device = [0u8; 4];
            let _ = getrandom::getrandom(&mut device);
            Db(crate::Db::new(MemStore::default(), SystemEnv, device))
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
