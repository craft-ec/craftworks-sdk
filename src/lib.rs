//! App-facing SDK (ARCHITECTURE.md §19). Grows one surface per build phase:
//! `collection` (1) · `identity` `query` `subscribe` (3) · `inbox` `edge`
//! `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).
//!
//! Every capability is written once in Rust and exposed to JavaScript from
//! [`js`], so the builder and apps get it the phase it lands.

pub mod blockid;
pub mod db;
pub mod id;
pub mod record;
pub mod schema;
pub mod store;
pub mod tree_store;

pub use blockid::{BlockId, ContentHash, IdError};
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

// `cid` and `cid_hex` were here, returning `BLAKE3(bytes)` under a name that
// reads like an address. Nothing in this crate called them — the tree makes its
// own ids through `freenet_prolly::block_id` — so the only thing they did was
// offer apps a 32-byte value that looks like a block id and fetches nothing.
// They are now [`ContentHash`] and [`BlockId`], which are different types.

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

    /// The id of a block of opaque bytes holding `bytes` — `raw:<64 hex>`.
    /// This is what addresses those bytes on the network, and the only kind of
    /// id an app is given.
    #[wasm_bindgen(js_name = blockId)]
    pub fn block_id(bytes: &[u8]) -> String {
        crate::BlockId::raw(bytes).to_string()
    }

    /// `BLAKE3(bytes)` as `hash:<64 hex>` — the hash of some bytes, which
    /// addresses nothing.
    ///
    /// Deliberately NOT re-exported by `js/wrap.js`: apps get block ids. It
    /// stays on the raw module because the frozen vectors are checked from
    /// JavaScript, and a binding that mangles bytes has to fail that check in
    /// both languages.
    #[wasm_bindgen(js_name = contentHash)]
    pub fn content_hash(bytes: &[u8]) -> String {
        crate::ContentHash::of(bytes).to_string()
    }

    /// Read a block id back from its text form, split into its parts:
    /// `{ tag, hex, id }` — the kind's name, the 64 hex characters, and the
    /// canonical text.
    ///
    /// The half that makes a self-describing id worth having: an id says what
    /// it is so the RECEIVER can check it, and here the receiver is
    /// JavaScript. Without this, anything that wants the tag and the hex apart
    /// splits the string itself and quietly owns a copy of the format.
    ///
    /// Throws an ordinary `Error` for anything that is not a block id —
    /// including a content hash, which is the mistake worth naming — with the
    /// message the Rust parser produces. Never a wasm abort: with
    /// `panic = abort` a bad id from a paste box would take the SDK down.
    #[wasm_bindgen(js_name = parseBlockId)]
    pub fn parse_block_id(text: &str) -> Result<String, JsError> {
        let id: crate::BlockId = text.parse().map_err(err)?;
        json(&serde_json::json!({
            "tag": id.tag(),
            "hex": crate::hex(&id.hash()),
            "id": id.to_string(),
        }))
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
        /// The tree's root, as `node:<64 hex>`. The whole database in 32
        /// bytes: it changes with every write and is the same for any two
        /// databases holding the same records, whatever order they were
        /// written in.
        ///
        /// Tagged, because a root IS a block id — it names the tree's top node
        /// — and it is the one id this surface hands out most.
        pub fn root(&self) -> String {
            crate::BlockId::from_parts(freenet_prolly::kind::TREE_NODE, self.0.store().root())
                .to_string()
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
    use super::{BlockId, ContentHash};

    fn bytes(input: &str) -> Vec<u8> {
        (0..input.len() / 2)
            .map(|i| u8::from_str_radix(&input[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    /// The same vectors are checked from JavaScript (`tests/js/ids.test.cjs`), so
    /// a binding that mangles bytes on the way in or out cannot pass both.
    ///
    /// The file is unchanged by this rename: these are the same numbers they
    /// always were, now under the name that says what they are. The block ids
    /// beside them are new, and are what apps actually get.
    #[test]
    fn the_frozen_vectors_hold_in_rust() {
        let mut n = 0;
        for line in include_str!("../tests/vectors.txt").lines() {
            let (input, want) = line.split_once(' ').unwrap();
            assert_eq!(
                super::hex(&ContentHash::of(&bytes(input)).bytes()),
                want,
                "input {input}"
            );
            n += 1;
        }
        assert!(n >= 4, "only {n} vectors");

        let mut m = 0;
        for line in include_str!("../tests/block_vectors.txt").lines() {
            let mut it = line.split(' ');
            let (tag, input, want) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            let b = bytes(input);
            let id = match tag {
                "raw" => BlockId::raw(&b),
                "node" => BlockId::node(&b),
                t => panic!("unknown tag {t}"),
            };
            assert_eq!(id.to_string(), want, "input {input}");
            m += 1;
        }
        assert!(m >= 4, "only {m} block vectors");
    }
}
