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
use anyhow::{bail, Result};
use freenet_stdlib::client_api::{ClientError, ContractResponse, HostResponse};
use probe::lose::{chosen_line, not_found_line, parse_lose, Lose, Target, Verdict};
use probe::proxy::{serve, Hooks};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

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
                eprintln!("{}", chosen_line("root", c));
            }
        }
        return m;
    };
    let had = lose.chosen().is_some();
    let verdict = lose.block(cid, state.as_ref());
    if !had {
        if let Some(c) = lose.chosen() {
            eprintln!("{}", chosen_line("data", c));
        }
    }
    match verdict {
        Verdict::Relay => m,
        Verdict::NotFound(id) => {
            eprintln!("{}", not_found_line(&id, lose.not_found));
            let nf: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(ContractResponse::NotFound { instance_id: *key.id() }));
            Message::Binary(bincode::serialize(&nf).expect("a NotFound encodes").into())
        }
    }
}

struct LoseHooks(Mutex<Lose>);

impl Hooks for LoseHooks {
    fn down(&self, _conn: u64, m: Message) -> Option<Message> {
        Some(answer(&self.0, m))
    }
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
    let said = serde_json::json!({ "group": group, "lose": n });
    serve(listen.parse()?, node.parse()?, said, Arc::new(LoseHooks(Mutex::new(Lose::new(target, n))))).await
}
