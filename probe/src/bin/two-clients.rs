//! F61's SETTLING TEST: does the node answer contract ops in ONE sequence per NODE, or one per CLIENT?
//!
//! F61 measured one client's ops, across its connections, answered in one sequence (~37 ms a contract op). If
//! that sequence is per NODE, every tab, the keeper and a second app on one node share it; if per client, each
//! has its own. Two SEPARATE clients (two websockets, two WebApi sessions) on ONE private node, each PUTting
//! fresh `webapp` contracts (content-addressed: every PUT a new contract, never a dedupe), in four arms, each
//! repeated R times so the spread between repeats is printed beside every figure:
//!   solo   -- client 1 alone sends N PUTs at once; then client 2 alone
//!   single -- client 2 alone sends ONE PUT (the baseline for `behind`)
//!   behind -- client 1 sends N PUTs; 5 ms later client 2 sends ONE: when is client 2's answered?
//!   both   -- both send N at once: each one's per-op answer times and completion
//! Per-node FIFO predicts `behind` ~ client 1's whole burst and `both` ~ 2N ops of time for the later-finishing
//! client; per-client predicts `behind` ~ `single`. The machine's load is printed at every arm.
//!   two-clients <webapp.wasm> <ws_port> <dir> [N] [R]

use anyhow::{bail, Context, Result};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Each ANSWER's deadline: far above any measured op, so a missing answer is recorded, never waited on for ever.
const ANSWER: Duration = Duration::from_secs(60);

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws).await.context("connecting to the node")?;
    Ok(WebApi::start(stream))
}

fn load() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("unreadable: {e}"))
}

/// A fresh webapp contract: `tag` makes its state, and so its key, unique.
fn contract(code: &[u8], tag: &str) -> (ContractKey, ContractContainer, WrappedState) {
    let state = wire::webapp::app_container(&[("f", tag.as_bytes())]).expect("a container");
    let (_, c, s) = wire::puts::contract(code, &wire::webapp::params(&state), &state);
    (c.key(), c, s)
}

/// One client's burst: send every PUT at once (one after another on its socket), then take answers, each timed
/// from ITS send. Returns (send offset ms from `t0`, answer ms after its send) per op, in send order; `None` for an
/// op not answered within `ANSWER`.
async fn burst(api: &mut WebApi, code: &[u8], tag: &str, n: usize, t0: Instant) -> Result<Vec<(f64, Option<f64>)>> {
    let mut sent: HashMap<ContractKey, (usize, Instant)> = HashMap::new();
    let mut out = vec![(0.0, None); n];
    for i in 0..n {
        let (key, c, s) = contract(code, &format!("{tag}-{i}"));
        let at = Instant::now();
        api.send(ClientRequest::ContractOp(ContractRequest::Put {
            contract: c,
            state: s,
            related_contracts: RelatedContracts::default(),
            subscribe: false,
            blocking_subscribe: false,
        }))
        .await?;
        out[i].0 = at.duration_since(t0).as_secs_f64() * 1e3;
        sent.insert(key, (i, at));
    }
    let mut left = n;
    while left > 0 {
        match timeout(ANSWER, api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                if let Some((i, at)) = sent.remove(&key) {
                    out[i].1 = Some(at.elapsed().as_secs_f64() * 1e3);
                    left -= 1;
                }
            }
            Ok(Ok(other)) => eprintln!("  (an answer that is not a PUT's: {other:?})"),
            Ok(Err(e)) => bail!("the node refused: {e}"),
            Err(_) => break,
        }
    }
    Ok(out)
}

fn fmt(ops: &[(f64, Option<f64>)]) -> String {
    let got: Vec<f64> = ops.iter().filter_map(|o| o.1).collect();
    let missing = ops.len() - got.len();
    let done = ops.iter().filter_map(|o| o.1.map(|a| o.0 + a)).fold(0.0_f64, f64::max);
    let answers: Vec<String> = ops.iter().map(|o| o.1.map_or("-".into(), |a| format!("{a:.0}"))).collect();
    format!("answers ms [{}]; last answered {done:.0} ms after the arm began{}", answers.join(" "), if missing > 0 { format!("; {missing} NOT within {} s", ANSWER.as_secs()) } else { String::new() })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let code = std::fs::read(a.next().context("usage: two-clients <webapp.wasm> <ws_port> <dir> [N] [R]")?)?;
    let port: u16 = a.next().context("ws port")?.parse()?;
    let dir = std::path::PathBuf::from(a.next().context("dir")?);
    let n: usize = a.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    let r: usize = a.next().map(|s| s.parse()).transpose()?.unwrap_or(3);
    let _tree = TempTree(dir.clone());
    let node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 20000 })?;
    println!("node: isolated NETWORK mode, freenet on 127.0.0.1:{port}; N={n} PUTs a burst, R={r} repeats; load at start {}", load());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut c1 = connect(&node.ws()).await?;
    let mut c2 = connect(&node.ws()).await?;
    // Warm-up: one PUT each, so the first arm does not pay the node's first-contract costs.
    burst(&mut c1, &code, "warm1", 1, Instant::now()).await?;
    burst(&mut c2, &code, "warm2", 1, Instant::now()).await?;

    for rep in 0..r {
        println!("\n== repeat {} (load {}) ==", rep + 1, load());
        let t = Instant::now();
        println!("solo   client 1 alone, {n} PUTs: {}", fmt(&burst(&mut c1, &code, &format!("s1-{rep}"), n, t).await?));
        let t = Instant::now();
        println!("solo   client 2 alone, {n} PUTs: {}", fmt(&burst(&mut c2, &code, &format!("s2-{rep}"), n, t).await?));
        let t = Instant::now();
        println!("single client 2 alone, 1 PUT: {}", fmt(&burst(&mut c2, &code, &format!("one-{rep}"), 1, t).await?));
        let (b1, b2) = (format!("b1-{rep}"), format!("b2-{rep}"));
        let t = Instant::now();
        let (one, two) = tokio::join!(burst(&mut c1, &code, &b1, n, t), async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            burst(&mut c2, &code, &b2, 1, t).await
        });
        let (one, two) = (one?, two?);
        println!("behind client 1's {n} PUTs: {}", fmt(&one));
        println!("behind client 2's 1 PUT, sent {:.0} ms in: {}", two[0].0, fmt(&two));
        let (x1, x2) = (format!("x1-{rep}"), format!("x2-{rep}"));
        let t = Instant::now();
        let (one, two) = tokio::join!(burst(&mut c1, &code, &x1, n, t), burst(&mut c2, &code, &x2, n, t));
        println!("both   client 1, {n} PUTs: {}", fmt(&one?));
        println!("both   client 2, {n} PUTs: {}", fmt(&two?));
    }
    println!("\nload at end {}", load());
    drop(node);
    Ok(())
}
