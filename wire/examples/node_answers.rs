//! What a node would SAY about a contract PUT, and what a page's frames ARE —
//! in the stdlib's own encoding, for the JavaScript test that drives the real
//! `Session` (`tests/js/contract-put.test.mjs`). Bytes a test built by hand
//! would agree with the test's own idea of the format; these are the node's.
//!
//! `node_answers <code> <params> <state> <cause> [frame…]`, every argument
//! but `cause` in hex. Prints one JSON object:
//! `{"key", "ack", "refusal", "got", "frames": [{"op", …}]}` — the PUT's ack
//! and refusal and a GET answer, hex-encoded as the node sends them, and each
//! frame decoded.

use freenet_stdlib::client_api::{ClientError, ClientRequest, ContractError, ContractRequest, ContractResponse, ErrorKind, HostResponse, RequestError};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    assert!(a.len() >= 4, "usage: node_answers <code> <params> <state> <cause> [frame…]");
    let (key, c, _) = wire::puts::contract(&unhex(&a[0]), &unhex(&a[1]), &unhex(&a[2]));
    let ack = bincode::serialize(&Ok::<HostResponse, ClientError>(HostResponse::ContractResponse(
        ContractResponse::PutResponse { key: c.key() },
    )))
    .expect("encodes");
    let refusal: ClientError = ErrorKind::RequestError(RequestError::ContractError(ContractError::Put {
        key: c.key(),
        cause: a[3].clone().into(),
    }))
    .into();
    let refusal = bincode::serialize(&Err::<HostResponse, ClientError>(refusal)).expect("encodes");
    // A GET answer for this contract: what a page that never asked for it
    // must leave to whoever did.
    let got = bincode::serialize(&Ok::<HostResponse, ClientError>(HostResponse::ContractResponse(
        ContractResponse::GetResponse { key: c.key(), contract: None, state: freenet_stdlib::prelude::WrappedState::new(unhex(&a[2])) },
    )))
    .expect("encodes");
    let frames: Vec<String> = a[4..]
        .iter()
        .map(|f| match bincode::deserialize::<ClientRequest>(&unhex(f)) {
            Ok(ClientRequest::ContractOp(ContractRequest::Put { contract, state, .. })) => format!(
                r#"{{"op":"put","key":"{}","state":"{}"}}"#,
                contract.key().id().encode(),
                hex(state.as_ref())
            ),
            Ok(ClientRequest::ContractOp(ContractRequest::Get { key, subscribe, .. })) => {
                format!(r#"{{"op":"get","key":"{}","subscribe":{subscribe}}}"#, key.encode())
            }
            Ok(ClientRequest::ContractOp(ContractRequest::Update { key, .. })) => {
                format!(r#"{{"op":"update","key":"{}"}}"#, key.id().encode())
            }
            Ok(ClientRequest::DelegateOp(_)) => r#"{"op":"delegate"}"#.to_string(),
            Ok(other) => format!(r#"{{"op":"{}"}}"#, format!("{other:?}").split(['(', ' ', '{']).next().unwrap_or("?")),
            Err(_) => r#"{"op":"undecodable"}"#.to_string(),
        })
        .collect();
    println!(
        r#"{{"key":"{key}","ack":"{}","refusal":"{}","got":"{}","frames":[{}]}}"#,
        hex(&ack),
        hex(&refusal),
        hex(&got),
        frames.join(",")
    );
}
