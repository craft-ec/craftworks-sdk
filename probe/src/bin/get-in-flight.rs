//! IS A CLIENT'S GET IN PROGRESS OFF THE NODE'S ONE CLIENT QUEUE? (sdk#447, the architect's ruling (1))
//!
//! The page counts the ops on the wire ahead of a first send as places in the node's one queue for this client (F61,
//! sdk#390). sdk#447 leaves a SILENT GET (the node is still fetching it) out of that count, on the claim that the node
//! took it off its client queue once it dispatched it. V20 cannot show that: its client was the node's HTTP web path,
//! one transient client per request. This measures it on ONE websocket, as the page has one.
//!
//! TWO private nodes on loopback (never 7509/7609): A an isolated gateway with a throwaway transport key, B joined to
//! A only. B holds a block (PUT and answered first). Then A is PAUSED (SIGSTOP, by the handle it was spawned with): a
//! SILENT PEER, so a GET B cannot answer locally goes out to A and gets no answer (F64: up to its attempts × 60 s).
//! On B's one socket: that GET FIRST, then a GET of the block B holds, then a PUT. Every answer is timed and named by
//! the key it carries. The held GET answered while the first is unanswered = a GET in progress does not hold this
//! client's queue. A is resumed and both nodes stopped on every exit.
//!
//! Every line is JSON; the verdict line is last.
//!
//! `--silent-peers N` (default 1): N-1 more private nodes join A, and b's own connections are listed before any is
//! paused; ALL N are paused. The node's retry fallback asks one silent peer at most twice (F64), so N distinct silent
//! peers are what reach its full attempt bound.
//!
//! usage: get-in-flight <block.wasm> <dir> [--base PORT] [--watch SECS] [--silent-peers N]
use anyhow::{bail, Context, Result};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::*;
use probe::node::{Node, TempTree};
use probe::signer::{connect, container, contract_op, raw_block};
use std::time::{Duration, Instant};
use tokio::time::timeout;

fn arg(a: &[String], name: &str) -> Option<String> {
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

fn random_body() -> Vec<u8> {
    let mut body = vec![0u8; 700];
    getrandom::getrandom(&mut body).expect("random");
    body
}

fn hex(b: &[u8]) -> String {
    core_types::hex::encode(b)
}

/// SIGSTOP / SIGCONT to OUR OWN node, by the pid its spawn returned.
fn signal(pid: u32, sig: &str) -> Result<()> {
    let ok = std::process::Command::new("kill").args([sig, &pid.to_string()]).status()?.success();
    if !ok {
        bail!("kill {sig} {pid} failed");
    }
    Ok(())
}

/// Resumes the paused node on every exit, so its drop can kill and reap it.
struct Resume(u32);
impl Drop for Resume {
    fn drop(&mut self) {
        let _ = signal(self.0, "-CONT");
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let usage = "usage: get-in-flight <block.wasm> <dir> [--base PORT] [--watch SECS] [--silent-peers N]";
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (Some(bw), Some(dir)) = (a.first(), a.get(1)) else { bail!("{usage}") };
    let base: u16 = arg(&a, "--base").map(|v| v.parse()).transpose()?.unwrap_or(47_500);
    let watch = Duration::from_secs(arg(&a, "--watch").map(|v| v.parse()).transpose()?.unwrap_or(90));
    let silent_peers: u16 = arg(&a, "--silent-peers").map(|v| v.parse()).transpose()?.unwrap_or(1).max(1);
    // EVERY port this run derives, refused up front -- before anything starts -- if one is someone else's (the
    // architect on #444: a base a few below 7509 derives 7509).
    let mut ports = vec![base, base + 1, base + 10, base + 11];
    for i in 1..silent_peers {
        ports.extend([base + 20 + 10 * i, base + 21 + 10 * i]);
    }
    for port in &ports {
        probe::node::allowed_port(&format!("ws://127.0.0.1:{port}"))?;
    }
    let bcode = std::fs::read(bw).with_context(|| format!("reading {bw}"))?;
    let root = std::path::PathBuf::from(dir);
    let _tree = TempTree(root.clone());

    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).expect("random");
    let public = curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(secret).to_bytes();
    std::fs::create_dir_all(root.join("a"))?;
    std::fs::write(root.join("a/transport.key"), hex(&secret))?;
    let a_extra = [
        "--is-gateway".to_string(),
        "--transport-keypair".into(),
        root.join("a/transport.key").to_string_lossy().into_owned(),
        "--public-network-address".into(),
        "127.0.0.1".into(),
        "--public-network-port".into(),
        (base + 1).to_string(),
    ];
    let mut node_a = Node::spawn_private_network(base, base + 1, &root.join("a"), &a_extra)?;
    let b_extra = ["--gateway".to_string(), format!("127.0.0.1:{},{}", base + 1, hex(&public))];
    // The other silent peers: private nodes joined to A, as b is.
    let mut others = Vec::new();
    for i in 1..silent_peers {
        let (ws, net) = (base + 20 + 10 * i, base + 21 + 10 * i);
        others.push(Node::spawn_private_network(ws, net, &root.join(format!("c{i}")), &b_extra)?);
    }
    let node_b = Node::spawn_private_network(base + 10, base + 11, &root.join("b"), &b_extra)?;
    println!("{}", serde_json::json!({ "nodes": { "a": node_a.ws(), "b": node_b.ws(), "others": others.iter().map(|n| n.ws()).collect::<Vec<_>>(), "all_joined_to": "a only" } }));
    let mut c = connect(&node_b.ws()).await?;
    // b must be CONNECTED to every silent peer before they go silent, or the run measures fewer than it says.
    let wanted = usize::from(silent_peers);
    let joined_by = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut peers: Vec<String> = Vec::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        c.send(ClientRequest::NodeQueries(freenet_stdlib::client_api::NodeQuery::ConnectedPeers)).await?;
        if let Ok(Ok(HostResponse::QueryResponse(freenet_stdlib::client_api::QueryResponse::ConnectedPeers { peers: p }))) = timeout(Duration::from_secs(5), c.recv()).await {
            peers = p.iter().map(|(_, addr)| addr.to_string()).collect();
        }
        if peers.len() >= wanted || tokio::time::Instant::now() > joined_by {
            break;
        }
    }
    println!("{}", serde_json::json!({ "b_connected_to": peers }));
    if peers.len() < wanted {
        bail!("SETUP: b is connected to {} peers, not the {wanted} this run pauses", peers.len());
    }

    // SETUP: a block B HOLDS (PUT through B, answered, while A still answers).
    let (held_id, held_state) = raw_block(random_body());
    let held = container(&bcode, &held_id);
    let r = contract_op(&mut c, ContractRequest::Put { contract: held.clone(), state: WrappedState::new(held_state), related_contracts: Default::default(), subscribe: false, blocking_subscribe: false }).await?;
    println!("{}", serde_json::json!({ "setup_put_on_b": r }));

    // A goes SILENT.
    let mut _resume = Vec::new();
    for n in std::iter::once(&mut node_a).chain(others.iter_mut()) {
        let pid = n.pid().context("a peer is not running")?;
        signal(pid, "-STOP")?;
        _resume.push(Resume(pid));
    }
    println!("{}", serde_json::json!({ "paused": _resume.len(), "as": "SIGSTOP: silent peers" }));

    let (never_id, _) = raw_block(random_body());
    let never = container(&bcode, &never_id).key();
    let (put_id, put_state) = raw_block(random_body());
    let put = container(&bcode, &put_id);
    let names = [(*never.id(), "silent-get"), (*held.key().id(), "held-get"), (*put.key().id(), "later-put")];
    let name = |id: &ContractInstanceId| names.iter().find(|(k, _)| k == id).map_or("?", |(_, n)| *n);
    let get = |id: ContractInstanceId| ClientRequest::ContractOp(ContractRequest::Get { key: id, return_contract_code: false, subscribe: false, blocking_subscribe: false });

    let t0 = Instant::now();
    let mut answered: Vec<(&str, u128, String)> = Vec::new();
    c.send(get(*never.id())).await?;
    println!("{}", serde_json::json!({ "sent": "silent-get", "ms": t0.elapsed().as_millis() }));
    // Anything the node says in the next 3 s is heard (and timed) before the later ops go out.
    let mut plan: Vec<(u128, &str)> = vec![(3_000, "held-get"), (3_000, "later-put")];
    let end = tokio::time::Instant::now() + watch;
    while tokio::time::Instant::now() < end && answered.len() < 3 {
        while let Some((at, what)) = plan.first().copied() {
            if t0.elapsed().as_millis() < at {
                break;
            }
            plan.remove(0);
            match what {
                "held-get" => c.send(get(*held.key().id())).await?,
                _ => c.send(ClientRequest::ContractOp(ContractRequest::Put { contract: put.clone(), state: WrappedState::new(put_state.clone()), related_contracts: Default::default(), subscribe: false, blocking_subscribe: false })).await?,
            }
            println!("{}", serde_json::json!({ "sent": what, "ms": t0.elapsed().as_millis() }));
        }
        let got = match timeout(Duration::from_millis(100), c.recv()).await {
            Err(_) => continue,
            Ok(r) => r,
        };
        let ms = t0.elapsed().as_millis();
        let (who, what) = match got {
            Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key, state, .. })) => (name(key.id()), format!("state {} bytes", state.size())),
            Ok(HostResponse::ContractResponse(ContractResponse::NotFound { instance_id })) => (name(&instance_id), "not found".into()),
            Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key })) => (name(key.id()), "put".into()),
            Ok(other) => ("?", format!("{other:?}").chars().take(160).collect()),
            Err(e) => ("?", format!("error: {e}").chars().take(200).collect()),
        };
        println!("{}", serde_json::json!({ "answer": who, "what": what, "ms": ms }));
        answered.push((who, ms, what));
    }
    let at = |n: &str| answered.iter().find(|(w, _, _)| *w == n).map(|(_, ms, _)| *ms);
    let silent = at("silent-get");
    let before = |l: Option<u128>| match (l, silent) {
        (Some(l), Some(s)) => l < s,
        (Some(_), None) => true,
        _ => false,
    };
    println!("{}", serde_json::json!({ "verdict": {
        "silent_get_ms": silent, "held_get_ms": at("held-get"), "later_put_ms": at("later-put"), "watched_s": watch.as_secs(), "silent_peers": silent_peers,
        "held_get_answered_while_the_silent_get_was_unanswered": before(at("held-get")),
        "later_put_answered_while_the_silent_get_was_unanswered": before(at("later-put")),
    }}));
    // A PAUSED NODE NEVER OUTLIVES THE PROBE: resumed, then killed and reaped by its own handle -- and checked gone.
    let paused: Vec<u32> = _resume.iter().map(|r| r.0).collect();
    drop(_resume);
    drop(others);
    drop(node_a);
    drop(node_b);
    let alive: Vec<u32> = paused.iter().copied().filter(|pid| std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())).collect();
    println!("{}", serde_json::json!({ "paused_peers_gone": alive.is_empty(), "still_alive": alive }));
    if !alive.is_empty() {
        bail!("a paused peer outlived the probe: {alive:?}");
    }
    Ok(())
}
