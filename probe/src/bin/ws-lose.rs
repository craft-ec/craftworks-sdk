//! LOSE DATA AT THE READER (builder#176, Phase 4's finish-line demo): a proxy between a READER's page and ITS OWN node
//! that passes everything through, except that the GET answers for the blocks of ONE group become the node's own
//! `NotFound` (`probe::lose` decides which). To that reader the blocks are LOST on the network: a published record
//! must still read, from parity (rule 11), with up to `m` of a group lost -- and must NOT with `m + 1` (the control).
//! Nothing is withheld from any publish and no node's store is touched: the loss is what this reader's node says, so
//! a realnet line that reports it says "V's node answers NotFound for N blocks (ws-lose)", never "a node lost data".
//!
//! A connection that is not a WebSocket upgrade (the node's HTTP: the app's web containers and pieces) is piped through
//! unchanged, and the node is dialled only when a client connects (a harness guard fails on a client connected before
//! the reader's page opens). Every choice and every NotFound is a JSON line on stderr: the evidence that the loss
//! happened.
//!
//! usage: ws-lose <listen-port> <node-ws-port> --group data|root --lose m|m+1
//!        (both ports on 127.0.0.1; `probe::node::RESERVED` refused)
use anyhow::{bail, Context, Result};
use freenet_stdlib::client_api::{ClientError, ContractResponse, HostResponse};
use futures::{SinkExt, StreamExt};
use probe::lose::{parse_lose, Lose, Target, Verdict};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// Is this the first packet of a WebSocket upgrade? (Peeked, not consumed.)
async fn is_upgrade(s: &TcpStream) -> bool {
    let mut buf = [0u8; 2048];
    match s.peek(&mut buf).await {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase().contains("upgrade: websocket"),
        Err(_) => false,
    }
}

async fn pipe(client: TcpStream, node_port: u16) -> Result<()> {
    let node = TcpStream::connect(("127.0.0.1", node_port)).await.context("the node")?;
    let (mut cr, mut cw) = client.into_split();
    let (mut nr, mut nw) = node.into_split();
    let a = tokio::io::copy(&mut cr, &mut nw);
    let b = tokio::io::copy(&mut nr, &mut cw);
    let _ = tokio::join!(a, b);
    Ok(())
}

/// The answer this proxy sends in place of `m`: the node's own `NotFound` for a lost block, or `m` itself.
fn answer(lose: &Mutex<Lose>, m: Message) -> Message {
    let Message::Binary(b) = &m else { return m };
    let Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key, state, .. }))) =
        bincode::deserialize::<Result<HostResponse, ClientError>>(b)
    else {
        return m;
    };
    let mut lose = lose.lock().unwrap();
    // A BLOCK is a state whose (kind, body) hashes to the id its key names under the key's own contract code.
    let block = wire::block::block_of_state(state.as_ref())
        .filter(|(cid, _)| {
            let code: Option<[u8; 32]> = key.code_hash().as_ref().try_into().ok();
            code.is_some_and(|code| contract_keys::instance(&code, cid).as_slice() == key.id().as_bytes())
        });
    let Some((cid, _)) = block else {
        let had = lose.chosen().is_some();
        lose.head(state.as_ref());
        if !had {
            if let Some(c) = lose.chosen() {
                eprintln!("{}", serde_json::json!({ "chosen": "root", "k": c.k, "lost": c.lost.iter().map(hex).collect::<Vec<_>>() }));
            }
        }
        return m;
    };
    let had = lose.chosen().is_some();
    let verdict = lose.block(cid, state.as_ref());
    if !had {
        if let Some(c) = lose.chosen() {
            eprintln!("{}", serde_json::json!({ "chosen": "data", "k": c.k, "slots": c.slots.len(), "lost": c.lost.iter().map(hex).collect::<Vec<_>>() }));
        }
    }
    match verdict {
        Verdict::Relay => m,
        Verdict::NotFound(id) => {
            eprintln!("{}", serde_json::json!({ "not_found": hex(&id), "not_found_total": lose.not_found }));
            let nf: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(ContractResponse::NotFound { instance_id: *key.id() }));
            Message::Binary(bincode::serialize(&nf).expect("a NotFound encodes").into())
        }
    }
}

fn hex(id: &[u8; 32]) -> String {
    id.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

// The handshake callback's `Result<Response, ErrorResponse>` is tungstenite's signature, not ours to shrink.
#[allow(clippy::result_large_err)]
async fn websocket(client: TcpStream, node_port: u16, lose: Arc<Mutex<Lose>>) -> Result<()> {
    let mut path = String::new();
    let client = tokio_tungstenite::accept_hdr_async(client, |req: &Request, resp: Response| {
        path = req.uri().to_string();
        Ok(resp)
    })
    .await
    .context("accepting the page's WebSocket")?;
    let (node, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{node_port}{path}")).await.context("the node's WebSocket")?;
    let (mut c_tx, mut c_rx) = client.split();
    let (mut n_tx, mut n_rx) = node.split();
    let up = async move {
        while let Some(m) = c_rx.next().await {
            n_tx.send(m?).await?;
        }
        anyhow::Ok(())
    };
    let down = async move {
        while let Some(m) = n_rx.next().await {
            c_tx.send(answer(&lose, m?)).await?;
        }
        anyhow::Ok(())
    };
    let _ = tokio::join!(up, down);
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    const USAGE: &str = "usage: ws-lose <listen-port> <node-ws-port> --group data|root --lose m|m+1";
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [listen, node, g, group, l, lose] = a.as_slice() else { bail!(USAGE) };
    if g != "--group" || l != "--lose" {
        bail!(USAGE);
    }
    let (target, n) = (Target::parse(group)?, parse_lose(lose)?);
    // Both ports go through the probes' one guard against the owner's nodes.
    let listen = probe::node::allowed_port(&format!("ws://127.0.0.1:{listen}"))?;
    let node = probe::node::allowed_port(&format!("ws://127.0.0.1:{node}"))?;
    let l = TcpListener::bind(("127.0.0.1", listen)).await.with_context(|| format!("binding {listen}"))?;
    eprintln!("{}", serde_json::json!({ "listening": listen, "node": node, "group": group, "lose": n }));
    let state = Arc::new(Mutex::new(Lose::new(target, n)));
    let conns = AtomicU64::new(0);
    loop {
        let (s, _) = l.accept().await?;
        let conn = conns.fetch_add(1, Ordering::Relaxed);
        let state = state.clone();
        tokio::spawn(async move {
            let r = if is_upgrade(&s).await { websocket(s, node, state).await } else { pipe(s, node).await };
            if let Err(e) = r {
                eprintln!("{}", serde_json::json!({ "connection": conn, "ended": e.to_string() }));
            }
        });
    }
}
