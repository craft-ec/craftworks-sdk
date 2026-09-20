//! App-facing SDK (ARCHITECTURE.md §19). Grows one surface per build phase:
//! `collection` (1) · `identity` `query` `subscribe` (3) · `inbox` `edge`
//! `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).
//!
//! Every capability is written once in Rust and exposed to JavaScript from
//! [`js`], so the builder and apps get it the phase it lands.

pub mod binding;
pub mod blockid;
pub mod db;
pub mod id;
pub mod record;
pub mod schema;
pub mod store;
pub mod trace;
pub mod tree_store;

mod engine_store;
pub use binding::{Binding, LiveMode, Reloads};
pub use blockid::{BlockId, ContentHash, IdError};
pub use db::{Db, Record, Scan};
pub use engine_store::{EngineStore, Event as EngineEvent, Page, Transport};
pub use freenet_prolly::Cid;
pub use id::{Env, RKey, SystemEnv};
pub use schema::{Field, Kind, Schema};
pub use store::{Delta, Edit, MemStore, Read, Reads, Store, StoreError};
pub use trace::{Stamped, Trace, Traces};
pub use tree_store::{OwedGroup, OwedParity, Stats, TreeStore};

/// The tree format tag every node carries, from the library that writes them.
///
/// Read from `freenet_prolly` rather than written down here. A tag this crate
/// spelled itself would keep reporting `PT01` the day the library moved on,
/// which is precisely the drift a version panel exists to catch.
pub fn format_tag() -> &'static str {
    // `MAGIC` is four ASCII bytes by construction, asserted below.
    std::str::from_utf8(freenet_prolly::node::MAGIC).expect("the format tag is ASCII")
}

/// The commit this build came from, as `build.rs` stamped it. `unknown` when
/// nothing could say — which a consumer must treat as a mismatch, not as an
/// absence.
pub const BUILD_REV: &str = env!("SDK_BUILD_REV");

/// The `freenet-prolly` revision this build LINKS, from `Cargo.lock`.
pub const PROLLY_REV: &str = env!("SDK_PROLLY_REV");

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

    /// What this build IS, as JSON: `{version, rev, prollyRev, formatTag}`.
    ///
    /// The point of it is `rev`. A consumer that pins an SDK revision and copies
    /// the built wasm into its own tree has, until now, had no way to check that
    /// the bytes it copied came from the revision it asked for — a stale copy
    /// and a fresh one are indistinguishable. `rev` is baked from the SOURCE by
    /// `build.rs` (see that file for why it must not be passed in), so a
    /// consumer can compare it against the rev it recorded and find out.
    ///
    /// `formatTag` is read from the linked tree library rather than spelled
    /// here: a tag this crate wrote down could drift from the tag the trees
    /// actually carry, which is the failure it exists to make visible.
    #[wasm_bindgen(js_name = buildInfo)]
    pub fn build_info() -> String {
        let mut m = Map::new();
        m.insert("version".into(), Value::from(env!("CARGO_PKG_VERSION")));
        m.insert("rev".into(), Value::from(env!("SDK_BUILD_REV")));
        m.insert("prollyRev".into(), Value::from(env!("SDK_PROLLY_REV")));
        m.insert(
            "formatTag".into(),
            Value::from(crate::format_tag().to_string()),
        );
        Value::Object(m).to_string()
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
        pub fn schema(&mut self, domain: &str) -> Result<String, JsError> {
            json(&self.0.schema(domain).map_err(err)?)
        }
        pub fn domains(&mut self) -> Result<String, JsError> {
            json(&self.0.domains().map_err(err)?)
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
        pub fn get(&mut self, domain: &str, id: &str) -> Result<String, JsError> {
            json(&self.0.get(domain, &rkey(id)?).map_err(err)?)
        }
        pub fn delete(&mut self, domain: &str, id: &str) -> Result<bool, JsError> {
            self.0.delete(domain, &rkey(id)?).map_err(err)
        }
        /// `after` is a record id or the empty string.
        pub fn scan(
            &mut self,
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

        pub fn count(&mut self, domain: &str) -> Result<usize, JsError> {
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
