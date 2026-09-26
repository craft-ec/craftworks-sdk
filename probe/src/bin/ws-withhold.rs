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
//! usage: ws-withhold <listen-port> <node-ws-port>          (both on 127.0.0.1; `probe::node::RESERVED` refused)
use anyhow::{bail, Result};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use probe::proxy::{serve, Hooks};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

static WITHHELD: AtomicU64 = AtomicU64::new(0);
static PARITY_PUTS: AtomicU64 = AtomicU64::new(0);

/// Per page connection: its requests' reassembly, and the contracts of the parity PUTs it sent (their answers dropped).
#[derive(Default)]
struct Withhold {
    conns: Mutex<HashMap<u64, (probe::frames::Requests, HashSet<ContractInstanceId>)>>,
}

impl Hooks for Withhold {
    fn up(&self, conn: u64, m: &Message) {
        let Message::Binary(b) = m else { return };
        let mut conns = self.conns.lock().unwrap();
        let (reqs, parity) = conns.entry(conn).or_default();
        if let Ok(probe::frames::Frame::Whole(ClientRequest::ContractOp(ContractRequest::Put { contract, state, .. }))) = reqs.push(&format!("conn-{conn}"), b) {
            if state.as_ref().first() == Some(&freenet_prolly::kind::PARITY) {
                parity.insert(*contract.key().id());
                PARITY_PUTS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn down(&self, conn: u64, m: Message) -> Option<Message> {
        if let Message::Binary(b) = &m {
            if let Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) =
                bincode::deserialize::<Result<HostResponse, freenet_stdlib::client_api::ClientError>>(b)
            {
                if self.conns.lock().unwrap().get(&conn).is_some_and(|(_, p)| p.contains(key.id())) {
                    let n = WITHHELD.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!("{}", serde_json::json!({ "withheld": key.id().to_string(), "withheld_total": n, "parity_puts_seen": PARITY_PUTS.load(Ordering::Relaxed) }));
                    return None;
                }
            }
        }
        Some(m)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [listen, node] = a.as_slice() else { bail!("usage: ws-withhold <listen-port> <node-ws-port>") };
    let (listen, node) = (listen.parse()?, node.parse()?);
    serve(listen, node, serde_json::json!({ "withholding": "answers to PARITY-block PUTs" }), Arc::new(Withhold::default())).await
}
