//! The JavaScript surface — and the ONLY crate that links the SDK core to the
//! framing.
//!
//! Thin: no logic lives here. Structured values cross the boundary as JSON
//! strings; `js/wrap.js` gives apps plain objects.
//!
//! # Why this is its own crate
//!
//! The core is sans-IO and testable with no node, and the boundary gate keeps
//! `freenet-stdlib` out of it. `wire` knows the client API; the core knows the
//! data. Something has to link them for a page, and that something is here —
//! so `craftworks-sdk` stays on the CHECKED side of the gate rather than
//! becoming a crate the gate has to excuse.
//!
//! Empty off wasm. This crate exists to be a browser artefact; building it for
//! the host would pull `wasm-bindgen` into a place it has no business being,
//! and the `cfg` says that rather than a comment claiming it.
//!
//! The JavaScript surface. Thin: no logic lives here. Structured values cross
//! the boundary as JSON strings; `js/wrap.js` gives apps plain objects.
#![cfg(target_arch = "wasm32")]

pub mod session;

use craftworks_sdk::{id, Scan, Schema, SystemEnv, TreeStore};
use serde_json::{Map, Value};
use wasm_bindgen::prelude::*;

/// SDK version, so an app can tell which build it loaded.
#[wasm_bindgen]
pub fn version() -> String {
    craftworks_sdk::VERSION.to_string()
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
    m.insert("version".into(), Value::from(craftworks_sdk::VERSION));
    m.insert("rev".into(), Value::from(craftworks_sdk::BUILD_REV));
    m.insert("prollyRev".into(), Value::from(craftworks_sdk::PROLLY_REV));
    m.insert(
        "formatTag".into(),
        Value::from(craftworks_sdk::format_tag().to_string()),
    );
    // What this build PROVISIONS with. Copied from the contracts build,
    // never re-hashed here — the number that matters is the one Freenet
    // keys the contract by, and only the script that built the wasm can
    // say it without drifting.
    m.insert("blockHash".into(), Value::from(craftworks_sdk::BLOCK_HASH));
    m.insert(
        "registerHash".into(),
        Value::from(craftworks_sdk::REGISTER_HASH),
    );
    m.insert(
        "contractsRev".into(),
        Value::from(craftworks_sdk::CONTRACTS_REV),
    );
    Value::Object(m).to_string()
}

/// The id of a block of opaque bytes holding `bytes` — `raw:<64 hex>`.
/// This is what addresses those bytes on the network, and the only kind of
/// id an app is given.
#[wasm_bindgen(js_name = blockId)]
pub fn block_id(bytes: &[u8]) -> String {
    craftworks_sdk::BlockId::raw(bytes).to_string()
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
    craftworks_sdk::ContentHash::of(bytes).to_string()
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
    let id: craftworks_sdk::BlockId = text.parse().map_err(err)?;
    json(&serde_json::json!({
        "tag": id.tag(),
        "hex": craftworks_sdk::hex(&id.hash()),
        "id": id.to_string(),
    }))
}

/// The slot a record copied from a source takes, as 32 hex: the same source
/// always gives the same slot (craftworks-sdk#149). `createdMs` is the SOURCE
/// record's `created` field — never a time read out of its id.
///
/// A number, not a BigInt, because that is what `created` is in JavaScript;
/// anything that is not a whole, non-negative, exactly representable
/// millisecond count is refused rather than rounded into another slot.
#[wasm_bindgen(js_name = slotFrom)]
pub fn slot_from(created_ms: f64, namespace: &str, source_id: &str) -> Result<String, JsError> {
    if !(created_ms >= 0.0 && created_ms.fract() == 0.0 && created_ms <= 9_007_199_254_740_991.0) {
        return Err(err(format!("createdMs must be a whole number of milliseconds; got {created_ms}")));
    }
    Ok(id::to_hex(&craftworks_sdk::slot_from(created_ms as u64, namespace, source_id)))
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
/// A record id as an app holds it: 32 hex, or 64 when the domain keys its
/// records under a parent. The app never takes one apart — the length says
/// which it is, so it does not have to know (craftworks-sdk#122).
fn loc(hex: &str) -> Result<id::Loc, JsError> {
    id::loc_from_hex(hex)
        .ok_or_else(|| err("record id must be 32 hex characters, or 64 under a parent"))
}
fn json<T: serde::Serialize>(v: &T) -> Result<String, JsError> {
    serde_json::to_string(v).map_err(err)
}

/// The host's clock, through the platform's own `Date.now`.
fn js_now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// The name an app wrote, checked by the SAME rule a Session applies before
/// prefixing (`app::check_name`, 1–32): the in-tab db holds one app's data
/// under the names it wrote, so a name the node would refuse is refused here
/// too, in the same words (sdk#276). The core's own bound is the STORED name.
fn name(domain: &str) -> Result<&str, JsError> {
    craftworks_sdk::app::check_name(domain).map_err(err)?;
    Ok(domain)
}

/// A database on the real prolly tree, in memory. The same surface will sit
/// over the node's engine later; what changes is where the blocks live.
#[wasm_bindgen]
pub struct Db(craftworks_sdk::Db<TreeStore, SystemEnv>);

#[wasm_bindgen]
impl Db {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Db {
        let mut device = [0u8; 4];
        let _ = getrandom::getrandom(&mut device);
        Db(craftworks_sdk::Db::new(TreeStore::new(), SystemEnv, device))
    }
    pub fn define(&mut self, domain: &str, schema: &str) -> Result<(), JsError> {
        let s: Schema = serde_json::from_str(schema).map_err(err)?;
        self.0.define(name(domain)?, &s).map_err(err)
    }
    pub fn schema(&mut self, domain: &str) -> Result<String, JsError> {
        json(&self.0.schema(name(domain)?).map_err(err)?)
    }
    pub fn domains(&mut self) -> Result<String, JsError> {
        json(&self.0.domains().map_err(err)?)
    }
    /// Errors here are ordinary JavaScript `Error`s the app can catch —
    /// not a wasm abort. With `panic = abort` a limit breach that reached
    /// the store would take the whole SDK down with it, so `Db` screens
    /// first and returns this instead.
    pub fn put(&mut self, domain: &str, f: &str) -> Result<String, JsError> {
        json(&self.0.put(name(domain)?, &fields(f)?).map_err(err)?)
    }
    /// Create at a slot the caller derived (`slotFrom`), or answer the record
    /// already there: `{ outcome: "created" | "exists", record }`. Never an
    /// overwrite (craftworks-sdk#149).
    pub fn create_at(&mut self, domain: &str, slot: &str, f: &str) -> Result<String, JsError> {
        json(&self.0.create_at(name(domain)?, rkey(slot)?, &fields(f)?).map_err(err)?)
    }
    pub fn update(&mut self, domain: &str, id: &str, patch: &str) -> Result<String, JsError> {
        json(
            &self
                .0
                .update(name(domain)?, loc(id)?, &fields(patch)?)
                .map_err(err)?,
        )
    }
    /// An id that does not parse answers `null`, the same as one that parses
    /// and is not there. See `Db::get` for why a read and a write differ here
    /// (craftworks-sdk#118).
    pub fn get(&mut self, domain: &str, id: &str) -> Result<String, JsError> {
        let Some(at) = id::loc_from_hex(id) else {
            // The DOMAIN is still checked: "no such domain" is a programming
            // error, not an outside id, and must not be hidden behind `null`.
            self.0.schema(name(domain)?).map_err(err)?;
            return Ok("null".into());
        };
        json(&self.0.get(name(domain)?, at).map_err(err)?)
    }
    pub fn delete(&mut self, domain: &str, id: &str) -> Result<bool, JsError> {
        self.0.delete(name(domain)?, loc(id)?).map_err(err)
    }
    /// The children of one parent, as a bounded read.
    ///
    /// The app names the PARENT; it never builds a key range. That is the same
    /// rule `Session::preload` keeps — a caller that built one would be
    /// encoding the key layout — and it is why this exists as a call rather
    /// than as a range an app could assemble (craftworks-sdk#122).
    pub fn children(
        &mut self,
        domain: &str,
        parent: &str,
        reverse: bool,
        limit: usize,
        after: &str,
    ) -> Result<String, JsError> {
        let after = if after.is_empty() { None } else { Some(rkey(after)?) };
        json(
            &self
                .0
                .children(name(domain)?, &rkey(parent)?, Scan { reverse, limit, after })
                .map_err(err)?,
        )
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
                    name(domain)?,
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
        craftworks_sdk::BlockId::from_parts(freenet_prolly::kind::TREE_NODE, self.0.store().root())
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
        self.0.count(name(domain)?).map_err(err)
    }
}

impl Default for Db {
    fn default() -> Self {
        Self::new()
    }
}

/// `webapp`'s params for a web container's state: its blake3 (builder#104).
/// With the `webapp` code (`pkg/web/webapp.wasm`) and the state, it is all
/// `Session.put_contract` needs to publish a container — and the key that
/// returns is the address the node serves it under.
#[wasm_bindgen]
pub fn webapp_params(state: &[u8]) -> Vec<u8> {
    wire::webapp::params(state).to_vec()
}

/// The address the node serves a web container under
/// (`/v1/contract/web/<address>/`): the `webapp` contract's key for this
/// state, derived by freenet-stdlib itself. The same key a PUT of
/// `(code, webapp_params(state), state)` is answered with.
#[wasm_bindgen]
pub fn webapp_address(code: &[u8], state: &[u8]) -> String {
    wire::webapp::address(code, state)
}

/// An APP web container (builder#104), built in the page: the files as a
/// deterministic tar (sorted, whatever order they are added in), in xz of
/// stored LZMA2 chunks (no compression: the page has no xz encoder, and the
/// real `xz` reads it — `wire/tests/webapp_xz.rs`), in the node's framing.
#[wasm_bindgen]
#[derive(Default)]
pub struct AppContainer(Vec<(String, Vec<u8>)>);

#[wasm_bindgen]
impl AppContainer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> AppContainer {
        AppContainer::default()
    }

    /// A file, at a plain relative path. A path added twice is refused: two
    /// entries for one path would unpack as whichever the tar reader kept.
    pub fn add(&mut self, path: String, bytes: Vec<u8>) -> Result<(), JsError> {
        if self.0.iter().any(|(p, _)| *p == path) {
            return Err(JsError::new(&format!("{path} was added twice")));
        }
        self.0.push((path, bytes));
        Ok(())
    }

    /// The container's bytes: the state to PUT under `webapp`.
    pub fn finish(&self) -> Result<Vec<u8>, JsError> {
        let files: Vec<(&str, &[u8])> = self.0.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
        wire::webapp::app_container(&files).map_err(|e| JsError::new(&e))
    }
}

/// This page's wasm linear memory, in bytes: what a tree reader costs is
/// MEASURED against it (`tests/js/tree.test.mjs`), and the open-tree cap is
/// set from that measurement, not from a guess.
#[wasm_bindgen]
pub fn wasm_memory_bytes() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        (core::arch::wasm32::memory_size(0) * 65_536) as f64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}
