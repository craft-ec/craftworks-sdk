//! THE SDK AS DATA (ARCHITECTURE §19, "The SDK is data too"; craftworks-docs 288f935): every SDK version but the
//! site's fixed bootstrap is published in a PLATFORM TREE, under the platform app [`PLATFORM_APP`], as ONE record
//! `f/sdk/<rev>`: its bytes are that version's piece set (the build's pieces.json entry: SDK + runtime modules, one
//! set), its body `{ rev, name?, format_tag? }`. An app's `meta.sdk` = `{ tree, rev }` names the platform tree (its
//! Register id) and the version its loader runs. Publishing a version = its pieces PUT + that one record; no site is
//! rewritten.
//!
//! This module is the ONE reader of both: the version record ([`sdk_version`]) and the piece-set JSON inside it
//! ([`parse_piece_set`], which `publish_app` also reads), and the rule for `meta.sdk` ([`check_meta_sdk`]).

use crate::db::{DbError, DefRecord};
use crate::definition::DefKey;
use serde_json::Value;

/// The platform app: the app of the platform tree whose published definition holds the SDK versions.
pub const PLATFORM_APP: &str = "craftworks-platform";

/// An SDK rev: the craftworks-sdk commit a build is made from, 40 lowercase hex.
pub fn rev_ok(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The record key of version `rev`: `f/sdk/<rev>`, refused by name for anything that is not a rev.
pub fn sdk_key(rev: &str) -> Result<DefKey, DbError> {
    if !rev_ok(rev) {
        return Err(DbError::Refused(format!("`{rev}` is not an SDK rev: 40 lowercase hex (a craftworks-sdk commit)")));
    }
    DefKey::parse(&format!("f/sdk/{rev}"))
}

/// A piece set as the build's pieces.json names it: `{ name, k, m, pieces: [{ address, sha256 }] }`, a sha256 in hex.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PieceSetSpec {
    pub name: String,
    pub k: usize,
    pub m: usize,
    pub pieces: Vec<PieceSpec>,
}

/// One piece of a [`PieceSetSpec`]: the address its web container is served at, and the sha256 of its bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PieceSpec {
    pub address: String,
    #[serde(serialize_with = "hex_32")]
    pub sha256: [u8; 32],
}

fn hex_32<S: serde::Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&core_types::hex::encode(v))
}

/// Parse ONE piece set from its JSON (the build's pieces.json entry), refused by name when a field is missing or it
/// is not exactly `k + m` pieces with `k` at least 1. `what` names the caller in the refusal (`publish_app`, a version
/// record, ...).
pub fn parse_piece_set(what: &str, json: &str) -> Result<PieceSetSpec, String> {
    let s: Value = serde_json::from_str(json).map_err(|e| format!("{what}: the set is not JSON: {e}"))?;
    if !s.is_object() {
        return Err(format!("{what}: the set is not an object: a build carries ONE piece set"));
    }
    let name = s.get("name").and_then(Value::as_str).ok_or(format!("{what}: a set with no name"))?.to_string();
    let num = |f: &str| s.get(f).and_then(Value::as_u64).map(|n| n as usize).ok_or(format!("{what}: set {name} has no {f}"));
    let (k, m) = (num("k")?, num("m")?);
    let pieces = s
        .get("pieces")
        .and_then(Value::as_array)
        .ok_or(format!("{what}: set {name} has no pieces"))?
        .iter()
        .map(|p| {
            let address = p.get("address").and_then(Value::as_str).ok_or(format!("{what}: a piece of {name} has no address"))?.to_string();
            let sha256 = p
                .get("sha256")
                .and_then(Value::as_str)
                .and_then(core_types::hex::decode_array)
                .ok_or(format!("{what}: a piece of {name} has no 32-byte sha256"))?;
            Ok(PieceSpec { address, sha256 })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if k == 0 || pieces.len() != k + m {
        return Err(format!("{what}: piece set {name}: {} pieces for k {k} + m {m}", pieces.len()));
    }
    Ok(PieceSetSpec { name, k, m, pieces })
}

/// One SDK version as the platform tree publishes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SdkVersion {
    pub rev: String,
    /// A display name (`0.4`, `2026-10`), if the publisher gave one: never what selects it.
    pub name: Option<String>,
    /// The tree format tag the version reads and writes, if the record says.
    pub format_tag: Option<String>,
    /// Its piece set: the SDK and the runtime modules, one set.
    pub set: PieceSetSpec,
}

/// VERSION `rev`, from the platform app's published definition (`rows`, read whole): its `f/sdk/<rev>` record.
/// Refused by name when the platform publishes no such version, when its body names another rev, or when its bytes
/// are not a piece set.
pub fn sdk_version(rows: &[DefRecord], rev: &str) -> Result<SdkVersion, DbError> {
    let key = sdk_key(rev)?;
    let Some(r) = rows.iter().find(|r| r.key == key) else {
        return Err(DbError::Refused(format!("the platform tree publishes no SDK {rev} (no `{key}` in `{PLATFORM_APP}`'s published definition)")));
    };
    let said = r.body.get("rev").and_then(Value::as_str).unwrap_or("");
    if said != rev {
        return Err(DbError::Refused(format!("the record `{key}` names rev `{said}`: a version record names its own rev")));
    }
    let bytes = r.bytes.as_deref().ok_or_else(|| DbError::Refused(format!("the record `{key}` holds no piece set (no bytes)")))?;
    let json = std::str::from_utf8(bytes).map_err(|_| DbError::Refused(format!("the record `{key}`'s piece set is not text")))?;
    let set = parse_piece_set(&format!("SDK {rev}"), json).map_err(DbError::Refused)?;
    let text = |f: &str| r.body.get(f).and_then(Value::as_str).map(str::to_string);
    Ok(SdkVersion { rev: rev.to_string(), name: text("name"), format_tag: text("format_tag"), set })
}

/// `meta.sdk`, when a definition names one: `{ tree: <64-hex Register id>, rev: <40-hex SDK rev> }`, and nothing
/// else. A meta without `sdk` is valid (the app runs on its site's bootstrap SDK).
pub fn check_meta_sdk(meta: &Value) -> Result<(), DbError> {
    let Some(sdk) = meta.get("sdk") else { return Ok(()) };
    let bad = |why: &str| Err(DbError::Refused(format!("meta.sdk is {sdk}: {why} -- it must be {{ \"tree\": <the platform tree's 64-hex Register id>, \"rev\": <a 40-hex SDK rev> }}")));
    let Some(o) = sdk.as_object() else { return bad("not an object") };
    if o.keys().any(|k| k != "tree" && k != "rev") {
        return bad("it has a field other than tree and rev");
    }
    match o.get("tree").and_then(Value::as_str) {
        Some(t) if t.len() == 64 && core_types::hex::decode_array::<32>(t).is_some() => {}
        _ => return bad("its tree is not a 64-hex Register id"),
    }
    match o.get("rev").and_then(Value::as_str) {
        Some(r) if rev_ok(r) => Ok(()),
        _ => bad("its rev is not a 40-hex SDK rev"),
    }
}
