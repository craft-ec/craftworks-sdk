//! `classify-frames`, the realnet harness's one decoder of what a page SENT its
//! node: every kind of frame the SDK's own `wire` encoders make, run through
//! the binary, gets the verdict the A-side check's ruling gives it. The frames
//! are the SDK's bytes (never hand-written), so a change to the format or the
//! encoders is a failure here, not a silently passing harness.
use base64::Engine as _;
use freenet_stdlib::prelude::*;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

const BLOCK_CODE: &[u8] = b"the block contract's code, as the harness reads it from block.wasm";

/// Every frame of `framed`, as the harness captures them on one socket.
fn lines(window: &str, framed: Vec<Vec<u8>>) -> Vec<String> {
    framed
        .into_iter()
        .map(|f| serde_json::json!({ "t": 1, "window": window, "socket": "ws-1", "data": base64::engine::general_purpose::STANDARD.encode(f) }).to_string())
        .collect()
}

fn classify(input: &[String]) -> Vec<serde_json::Value> {
    let dir = std::env::temp_dir().join(format!("classify-frames-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let block = dir.join("block.wasm");
    std::fs::write(&block, BLOCK_CODE).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_classify-frames"))
        .args(["--block-code", block.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child.stdin.take().unwrap().write_all((input.join("\n") + "\n").as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "classify-frames failed");
    String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn container(code: &[u8], params: &[u8]) -> ContractContainer {
    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        Arc::new(ContractCode::from(code.to_vec())),
        Parameters::from(params.to_vec()),
    )))
}

fn verdict_of(rows: &[serde_json::Value], window: &str) -> Vec<(String, String)> {
    rows.iter()
        .filter(|r| r["window"] == window)
        .map(|r| {
            let op = r["op"].as_str().unwrap().to_string();
            let what = match r["signer"].as_str() {
                Some(s) => format!("{op}:{s}"),
                None => match r["code"].as_str() {
                    Some(c) => format!("{op}:{c}"),
                    None => op,
                },
            };
            (what, r["verdict"].as_str().unwrap().to_string())
        })
        .collect()
}

/// **Every kind, its verdict.** Reads and signer QUERIES pass; a Block-code PUT (small, or chunked) is REPORTED; a
/// PUT of any other contract, an UPDATE, a delegate REGISTRATION (always chunked: reassembly is exercised) and a
/// signer Sign / Provision / PutBlocks FAIL.
#[test]
fn every_frame_the_sdk_sends_gets_the_rulings_verdict() {
    let (_, signer) = wire::delegate_from_code(b"the signer delegate's code");
    let cid = [7u8; 32];
    let block = wire::block::block_contract(BLOCK_CODE, &cid);
    let register = container(b"the register contract's code", b"its params");
    let big = vec![9u8; 2 * 1024 * 1024];
    let mut input = Vec::new();
    input.extend(lines("get", wire::frame_get(*block.key().id(), false, 1).unwrap()));
    input.extend(lines("subscribe-get", wire::frame_get(*register.key().id(), true, 2).unwrap()));
    input.extend(lines("register-query", wire::signer::frame_register_query(&signer, 3, 3).unwrap()));
    input.extend(lines("held", wire::signer::frame_held(&signer, 4, vec![[1u8; 32]], 4).unwrap()));
    input.extend(lines("block-put", wire::frame_put(block.clone(), WrappedState::new(vec![1, 2, 3]), 5).unwrap()));
    let chunked = wire::frame_put(block.clone(), WrappedState::new(big), 6).unwrap();
    assert!(chunked.len() > 1, "THE SETUP: the big PUT was not chunked");
    input.extend(lines("block-put-chunked", chunked));
    input.extend(lines("register-put", wire::frame_put(register.clone(), WrappedState::new(vec![4]), 7).unwrap()));
    input.extend(lines("update", wire::frame_update(register.key(), vec![5], 8).unwrap()));
    let (delegate, _) = wire::delegate_from_code(b"any delegate");
    input.extend(lines("register-delegate", wire::frame_register_delegate(delegate, 9).unwrap()));
    let head = signer::Head { seq: 1, root: [2u8; 32] };
    let next = signer::Next { seq: 2, root: [3u8; 32], ledger: Vec::new() };
    input.extend(lines("sign", wire::signer::frame_sign(&signer, 10, head, next, 10).unwrap()));
    input.push(serde_json::json!({ "t": 1, "window": "garbage", "socket": "ws-1", "data": base64::engine::general_purpose::STANDARD.encode([0xffu8; 12]) }).to_string());

    let rows = classify(&input);
    let v = |w: &str| verdict_of(&rows, w);
    let one = |what: &str, verdict: &str| vec![(what.to_string(), verdict.to_string())];
    assert_eq!(v("get"), one("get", "pass"));
    assert_eq!(v("subscribe-get"), one("get", "pass"));
    assert_eq!(v("register-query"), one("signer:register-query", "pass"));
    assert_eq!(v("held"), one("signer:held", "pass"));
    assert_eq!(v("block-put"), one("put:block", "report"));
    assert_eq!(v("block-put-chunked"), one("put:block", "report"), "a chunked PUT is ONE request once reassembled");
    assert_eq!(v("register-put"), one("put:other", "fail"));
    assert_eq!(v("update"), one("update", "fail"));
    assert_eq!(v("register-delegate"), one("register-delegate", "fail"));
    assert_eq!(v("sign"), one("signer:sign", "fail"));
    assert_eq!(v("garbage"), one("undecodable", "fail"), "a frame that does not decode is FAILED, never skipped");
}
