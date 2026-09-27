//! THE ONE PROXY, DRIVEN (engineer1 on sdk#525): both probes' hooks through `probe::proxy::serve` over real loopback
//! sockets, against a fake node -- so a change to the shared bridge cannot silently alter either probe's evidence.
//! * ws-withhold: the node's answer to a PARITY PUT never reaches the page; a DATA PUT's does.
//! * ws-lose: once the head names the root's group, the node's answer to a GET of the root reaches the page as the
//!   node's own NotFound for that contract.
//!
//! And both evidence lines, pinned byte for byte.
use freenet_prolly::kind;
use freenet_stdlib::client_api::{ClientError, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use futures::{SinkExt, StreamExt};
use probe::lose::{Lose, LoseHooks, Target};
use probe::proxy::{serve, Hooks};
use probe::withhold::{withheld_line, Withhold};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// A free loopback port (never one of the owner's: `serve` refuses those anyway).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A fake node: for each binary frame a client sends, it sends back the next of `replies` (in order), as a node
/// answers each request.
async fn fake_node(replies: Vec<Vec<u8>>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(s).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        let mut replies = replies.into_iter();
        while let Some(Ok(m)) = rx.next().await {
            if matches!(m, Message::Binary(_)) {
                if let Some(r) = replies.next() {
                    tx.send(Message::Binary(r.into())).await.unwrap();
                }
            }
        }
    });
    port
}

fn ok(r: HostResponse) -> Vec<u8> {
    bincode::serialize(&Ok::<HostResponse, ClientError>(r)).unwrap()
}

/// A contract of `code` under `params`, and its key.
fn contract(code: &[u8], params: &[u8]) -> (ContractContainer, ContractKey) {
    let c = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(Arc::new(ContractCode::from(code.to_vec())), Parameters::from(params.to_vec()))));
    let key = c.key();
    (c, key)
}

/// The proxy with `hooks` in front of a fake node answering `replies`; the page's WebSocket to it.
async fn page_through(hooks: Arc<dyn Hooks>, replies: Vec<Vec<u8>>) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let node = fake_node(replies).await;
    let listen = free_port();
    tokio::spawn(serve(listen, node, serde_json::json!({}), hooks));
    for _ in 0..100 {
        if let Ok((ws, _)) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{listen}/v1/contract/command?encodingProtocol=native")).await {
            return ws;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("THE SETUP: the proxy never listened");
}

async fn next_answer(ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> Option<HostResponse> {
    match tokio::time::timeout(Duration::from_millis(700), ws.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => bincode::deserialize::<Result<HostResponse, ClientError>>(&b).ok()?.ok(),
        _ => None,
    }
}

#[tokio::test]
async fn ws_withhold_drops_only_the_parity_puts_answer() {
    let (parity, pkey) = contract(b"block code", b"parity params");
    let (data, dkey) = contract(b"block code", b"data params");
    let hooks = Arc::new(Withhold::default());
    let replies = vec![
        ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key: pkey })),
        ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key: dkey })),
    ];
    let mut ws = page_through(hooks.clone(), replies).await;
    for f in wire::frame_put(parity, WrappedState::new([vec![kind::PARITY], b"p".to_vec()].concat()), 1).unwrap() {
        ws.send(Message::Binary(f.into())).await.unwrap();
    }
    assert!(next_answer(&mut ws).await.is_none(), "the parity PUT's answer reached the page");
    for f in wire::frame_put(data, WrappedState::new([vec![kind::RAW], b"d".to_vec()].concat()), 2).unwrap() {
        ws.send(Message::Binary(f.into())).await.unwrap();
    }
    match next_answer(&mut ws).await {
        Some(HostResponse::ContractResponse(ContractResponse::PutResponse { key })) => assert_eq!(key, dkey),
        other => panic!("the data PUT's answer did not reach the page: {other:?}"),
    }
    assert_eq!(hooks.withheld.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(hooks.parity_puts.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn ws_lose_answers_the_lost_blocks_get_with_the_nodes_not_found() {
    // The head: a real signed Register state naming a root and its parity (the root's group of one).
    let body = b"the root node".to_vec();
    let root = freenet_prolly::block_id(kind::TREE_NODE, &body);
    let par: Vec<[u8; 32]> = (0..freenet_prolly::parity::PARITY as u8).map(|i| [40 + i; 32]).collect();
    let ledger = signer_proto::head::Ledger { parity: Some(par.concat()), ..Default::default() };
    let value = signer_proto::head::value(&root, &ledger);
    let sk = [7u8; 32];
    let vk = ed25519_dalek::SigningKey::from_bytes(&sk).verifying_key().to_bytes();
    let head_state = contract_keys::register::head_state(&wire::register_params(&vk, wire::HEAD_NAME), &sk, 3, &value).unwrap();
    let (_, head_key) = contract(b"register code", b"register params");
    // The root block's contract: the block code under the root's id, so its answer is recognised as a block.
    let (_, root_key) = contract(b"block code", &root);
    let hooks = Arc::new(LoseHooks(Mutex::new(Lose::new(Target::Root, freenet_prolly::parity::PARITY))));
    let replies = vec![
        ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key: head_key, contract: None, state: WrappedState::new(head_state) })),
        ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key: root_key, contract: None, state: WrappedState::new([vec![kind::TREE_NODE], body].concat()) })),
    ];
    let mut ws = page_through(hooks, replies).await;
    for id in [*head_key.id(), *root_key.id()] {
        for f in wire::frame_get(id, false, 1).unwrap() {
            ws.send(Message::Binary(f.into())).await.unwrap();
        }
        let a = next_answer(&mut ws).await.expect("an answer reached the page");
        if id == *root_key.id() {
            match a {
                HostResponse::ContractResponse(ContractResponse::NotFound { instance_id }) => assert_eq!(instance_id, id),
                other => panic!("the lost root's GET was answered {other:?}, not the node's NotFound"),
            }
        } else {
            assert!(matches!(a, HostResponse::ContractResponse(ContractResponse::GetResponse { .. })), "the head's answer was changed");
        }
    }
}

/// Both probes' evidence lines, byte for byte (the harness reads them).
#[test]
fn the_evidence_lines_keep_their_bytes() {
    let id = ContractInstanceId::new([1; 32]);
    assert_eq!(withheld_line(&id, 2, 5).to_string(), format!(r#"{{"parity_puts_seen":5,"withheld":"{id}","withheld_total":2}}"#));
    assert_eq!(probe::lose::not_found_line(&[3; 32], 7).to_string(), r#"{"not_found":"0303030303030303","not_found_total":7}"#);
}
