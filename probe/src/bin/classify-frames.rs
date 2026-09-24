//! THE ONE DECODER of what a page SENT its node (the realnet harness's A-side
//! check, builder#— ruling): raw client→node frames in, one classification
//! per request out. No other place decodes these; the harness captures bytes
//! (CDP, every target) and never reads a tag.
//!
//! - A frame is deserialised with freenet-stdlib's OWN request types (the
//!   format's owner): `ClientRequest` → `ContractRequest` / `DelegateRequest`.
//!   A request the SDK chunked (`StreamChunk`) is reassembled with stdlib's
//!   own `ReassemblyBuffer`, per socket, and then classified.
//! - A delegate message's payload goes to `signer_proto::decode_request`
//!   (re-exported by `signer`): SG02 and the signer's request kind.
//! - A PUT is classified by its contract CODE: the Block contract's code hash
//!   (`--block-code <block.wasm>`) or any other.
//!
//! What a VIEW-ONLY step may send, and what FAILS it (the ruling):
//! GET/SUBSCRIBE, and signer QUERIES (Register, Held), pass; a Block-code PUT
//! is REPORTED (a repair; content-addressed, so a view may send one); a PUT of
//! ANY other contract, an UPDATE, a delegate REGISTRATION and a signer
//! Sign / Provision / PutBlocks FAIL.
//!
//! In:  JSONL lines `{"t":…, "window":…, "socket":…, "data":"<base64>"}`.
//! Out: JSONL lines `{"t", "window", "socket", "op", "contract"?, "code"?,
//!       "signer"?, "verdict": "pass"|"report"|"fail", "why"?}`; a frame that
//!       does not decode is `op: "undecodable"`, verdict `fail` (never skipped).
//!
//! usage: classify-frames --block-code <block.wasm> < frames.jsonl > classified.jsonl
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use freenet_stdlib::client_api::streaming::ReassemblyBuffer;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, DelegateRequest};
use freenet_stdlib::prelude::*;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

#[derive(serde::Deserialize)]
struct Frame {
    t: serde_json::Value,
    #[serde(default)]
    window: serde_json::Value,
    socket: String,
    data: String,
}

fn hex(b: &[u8]) -> String {
    core_types::hex::encode(b)
}

/// One request's classification (without the frame's own fields).
fn classify(req: &ClientRequest<'_>, block_code: &CodeHash) -> Vec<serde_json::Value> {
    use serde_json::json;
    match req {
        ClientRequest::ContractOp(ContractRequest::Put { contract, .. }) => {
            let key = contract.key();
            let block = key.code_hash() == block_code;
            vec![json!({
                "op": "put",
                "contract": key.to_string(),
                "code": if block { "block" } else { "other" },
                "verdict": if block { "report" } else { "fail" },
                "why": if block { "a Block-contract PUT (repair; content-addressed)" } else { "a PUT of a non-Block contract" },
            })]
        }
        ClientRequest::ContractOp(ContractRequest::Update { key, .. }) => {
            vec![json!({ "op": "update", "contract": key.to_string(), "verdict": "fail", "why": "an UPDATE" })]
        }
        ClientRequest::ContractOp(ContractRequest::Get { key, .. }) => {
            vec![json!({ "op": "get", "contract": key.to_string(), "verdict": "pass" })]
        }
        ClientRequest::ContractOp(ContractRequest::Subscribe { key, .. }) => {
            vec![json!({ "op": "subscribe", "contract": key.to_string(), "verdict": "pass" })]
        }
        ClientRequest::ContractOp(other) => {
            vec![json!({ "op": "contract-other", "detail": format!("{other:?}").chars().take(80).collect::<String>(), "verdict": "fail", "why": "a contract request this check does not know" })]
        }
        ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { .. }) => {
            vec![json!({ "op": "register-delegate", "verdict": "fail", "why": "a delegate REGISTRATION" })]
        }
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) => inbound
            .iter()
            .map(|m| match m {
                InboundDelegateMsg::ApplicationMessage(am) => match signer::decode_request(&am.payload) {
                    Some((_, r)) => {
                        let (name, fail) = match r {
                            signer::Request::Sign { .. } => ("sign", true),
                            signer::Request::Provision { .. } => ("provision", true),
                            signer::Request::PutBlocks { .. } => ("put-blocks", true),
                            signer::Request::Held { .. } => ("held", false),
                            signer::Request::Register => ("register-query", false),
                        };
                        json!({
                            "op": "signer", "signer": name,
                            "verdict": if fail { "fail" } else { "pass" },
                            "why": if fail { format!("a signer {name}") } else { String::new() },
                        })
                    }
                    None => json!({ "op": "delegate-message", "verdict": "fail", "why": "a delegate message that is not the signer's (SG02)" }),
                },
                other => json!({ "op": "delegate-inbound", "detail": format!("{other:?}").chars().take(60).collect::<String>(), "verdict": "fail", "why": "a delegate message this check does not know" }),
            })
            .collect(),
        ClientRequest::DelegateOp(other) => {
            vec![json!({ "op": "delegate-other", "detail": format!("{other:?}").chars().take(80).collect::<String>(), "verdict": "fail", "why": "a delegate request this check does not know" })]
        }
        ClientRequest::Disconnect { .. } | ClientRequest::Close => vec![json!({ "op": "close", "verdict": "pass" })],
        other => vec![json!({ "op": "client-other", "detail": format!("{other:?}").chars().take(80).collect::<String>(), "verdict": "fail", "why": "a client request this check does not know" })],
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let block_path = match args.as_slice() {
        [flag, p] if flag == "--block-code" => p,
        _ => bail!("usage: classify-frames --block-code <block.wasm> < frames.jsonl"),
    };
    let block_code = *ContractCode::from(std::fs::read(block_path).with_context(|| format!("reading {block_path}"))?).hash();
    let mut streams: BTreeMap<String, ReassemblyBuffer> = BTreeMap::new();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let (mut frames, mut requests) = (0usize, 0usize);
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        frames += 1;
        let f: Frame = serde_json::from_str(&line).with_context(|| format!("frame line {frames}"))?;
        let emit = |out: &mut std::io::StdoutLock, rows: Vec<serde_json::Value>| -> Result<()> {
            for mut r in rows {
                let o = r.as_object_mut().expect("an object");
                o.insert("t".into(), f.t.clone());
                o.insert("window".into(), f.window.clone());
                o.insert("socket".into(), f.socket.clone().into());
                writeln!(out, "{r}")?;
            }
            Ok(())
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&f.data) {
            Ok(b) => b,
            Err(e) => {
                emit(&mut out, vec![serde_json::json!({ "op": "undecodable", "verdict": "fail", "why": format!("not base64: {e}") })])?;
                continue;
            }
        };
        let req: ClientRequest<'_> = match bincode::deserialize(&bytes) {
            Ok(r) => r,
            Err(e) => {
                emit(&mut out, vec![serde_json::json!({ "op": "undecodable", "bytes": bytes.len(), "head": hex(&bytes[..bytes.len().min(16)]), "verdict": "fail", "why": format!("not a ClientRequest: {e}") })])?;
                continue;
            }
        };
        // A CHUNK of a request the SDK split (stdlib's own rule): reassembled
        // per socket; classified once whole.
        if let ClientRequest::StreamChunk { stream_id, index, total, data } = &req {
            let buf = streams.entry(f.socket.clone()).or_default();
            match buf.receive_chunk(*stream_id, *index, *total, data.clone()) {
                Ok(None) => continue,
                Ok(Some(whole)) => match bincode::deserialize::<ClientRequest<'_>>(&whole) {
                    Ok(inner) => {
                        requests += 1;
                        emit(&mut out, classify(&inner, &block_code))?;
                    }
                    Err(e) => emit(&mut out, vec![serde_json::json!({ "op": "undecodable", "verdict": "fail", "why": format!("a reassembled stream is not a ClientRequest: {e}") })])?,
                },
                Err(e) => emit(&mut out, vec![serde_json::json!({ "op": "undecodable", "verdict": "fail", "why": format!("a stream chunk did not reassemble: {e:?}") })])?,
            }
            continue;
        }
        requests += 1;
        emit(&mut out, classify(&req, &block_code))?;
    }
    eprintln!("classify-frames: {frames} frame(s), {requests} whole request(s)");
    Ok(())
}
