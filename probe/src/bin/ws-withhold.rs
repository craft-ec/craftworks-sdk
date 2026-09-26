//! WITHHOLD PARITY (the live harness's BACKED_UP control, stock-take proposal 4): a proxy between a page and its node
//! that passes EVERYTHING through, except the node's ANSWER to a PUT of a PARITY block (a Block-contract state whose
//! kind byte is `freenet_prolly::kind::PARITY`). The page re-sends such a PUT until answered (rule 7); here it never
//! is. The data blocks are answered, so the page's commit reaches Published at k of every group -- and never
//! BACKED_UP. A harness assertion that "every write is BACKED_UP" must fail through this proxy; one that stays green
//! is not looking.
//!
//! Requests are read through the probes' one decoder (probe::frames). A connection that is not a WebSocket upgrade (the
//! node's HTTP: web containers, pieces) is piped through unchanged. Every withheld answer is counted on stderr.
//!
//! usage: ws-withhold <listen-port> <node-ws-port>          (both on 127.0.0.1; 7509/7609 refused)
use anyhow::{bail, Context, Result};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use futures::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

static WITHHELD: AtomicU64 = AtomicU64::new(0);
static PARITY_PUTS: AtomicU64 = AtomicU64::new(0);

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

async fn websocket(client: TcpStream, node_port: u16, conn: u64) -> Result<()> {
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
    // The contracts of parity PUTs this connection sent: their answers are dropped.
    let parity: Arc<Mutex<HashSet<ContractInstanceId>>> = Arc::default();
    let seen = parity.clone();
    let up = async move {
        let mut reqs = probe::frames::Requests::default();
        let socket = format!("conn-{conn}");
        while let Some(m) = c_rx.next().await {
            let m = m?;
            if let Message::Binary(b) = &m {
                if let Ok(probe::frames::Frame::Whole(ClientRequest::ContractOp(ContractRequest::Put { contract, state, .. }))) = reqs.push(&socket, b) {
                    if state.as_ref().first() == Some(&freenet_prolly::kind::PARITY) {
                        seen.lock().unwrap().insert(*contract.key().id());
                        PARITY_PUTS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            n_tx.send(m).await?;
        }
        anyhow::Ok(())
    };
    let down = async move {
        while let Some(m) = n_rx.next().await {
            let m = m?;
            if let Message::Binary(b) = &m {
                if let Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) =
                    bincode::deserialize::<Result<HostResponse, freenet_stdlib::client_api::ClientError>>(b)
                {
                    if parity.lock().unwrap().contains(key.id()) {
                        let n = WITHHELD.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("{}", serde_json::json!({ "withheld": key.id().to_string(), "withheld_total": n, "parity_puts_seen": PARITY_PUTS.load(Ordering::Relaxed) }));
                        continue;
                    }
                }
            }
            c_tx.send(m).await?;
        }
        anyhow::Ok(())
    };
    let _ = tokio::join!(up, down);
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [listen, node] = a.as_slice() else { bail!("usage: ws-withhold <listen-port> <node-ws-port>") };
    let (listen, node): (u16, u16) = (listen.parse()?, node.parse()?);
    for p in [listen, node] {
        if [7509, 7609].contains(&p) {
            bail!("{p} is somebody else's node: refused");
        }
    }
    let l = TcpListener::bind(("127.0.0.1", listen)).await.with_context(|| format!("binding {listen}"))?;
    eprintln!("{}", serde_json::json!({ "listening": listen, "node": node, "withholding": "answers to PARITY-block PUTs" }));
    let conns = AtomicU64::new(0);
    loop {
        let (s, _) = l.accept().await?;
        let conn = conns.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let r = if is_upgrade(&s).await { websocket(s, node, conn).await } else { pipe(s, node).await };
            if let Err(e) = r {
                eprintln!("{}", serde_json::json!({ "connection": conn, "ended": e.to_string() }));
            }
        });
    }
}
