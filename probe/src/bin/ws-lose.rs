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
use probe::lose::{parse_lose, Lose, LoseHooks, Target};
use probe::proxy::serve;
use std::sync::{Arc, Mutex};

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
