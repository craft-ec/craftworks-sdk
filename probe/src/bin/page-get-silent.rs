//! sdk#447's ACCEPTANCE, LIVE: the PAGE sends exactly ONE node GET per key while the node is silent about it, for
//! longer than the node's own GET bound B (`page::rto::NODE_GET_BOUND_MS`).
//!
//! A SIGSTOPped peer makes silence deterministic (the real network cannot force it). Private loopback nodes, never
//! 7509/7609: gateway a, `--silent-peers N`-1 more joined to a, and b joined to a; b is connected to every one of them
//! before ALL are paused. The real `page::Page` then runs on ONE websocket to b: it is told a head whose root is a
//! block nobody holds, and a client read makes its engine fetch that root. Every GET frame the page sends is counted
//! by key and timed, every answer b gives is fed back to the page, and nothing else the page sends goes out.
//!
//! The verdict is the property, measured from both ends:
//! - the page: never a GET of a key while its previous GET of that key is UNANSWERED (the node still fetching),
//!   and never a re-send on the RTO -- a second GET only after an answer, or once B + an RTO has passed;
//! - node b's own log: its client GETs of that key, the transactions it spawned.
//!
//! b's log dir and console out/err are copied out to `<keep>` before the temp tree goes.
//!
//! usage: page-get-silent <block.wasm> <dir> <keep> [--base PORT] [--watch SECS] [--silent-peers N]
use anyhow::{bail, Context, Result};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, NodeQuery, QueryResponse, WebApi};
use freenet_stdlib::prelude::*;
use page::{Answer, Ms, Op, Page, PutPath};
use probe::node::{Node, TempTree};
use probe::signer::{connect, container};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::time::timeout;

fn arg(a: &[String], name: &str) -> Option<String> {
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

fn hex(b: &[u8]) -> String {
    core_types::hex::encode(b)
}

fn signal(pid: u32, sig: &str) -> Result<()> {
    if !std::process::Command::new("kill").args([sig, &pid.to_string()]).status()?.success() {
        bail!("kill {sig} {pid} failed");
    }
    Ok(())
}

struct Resume(u32);
impl Drop for Resume {
    fn drop(&mut self) {
        let _ = signal(self.0, "-CONT");
    }
}

/// Copies b's logs out before the temp tree is removed (declared after it, so dropped first).
struct Keep(std::path::PathBuf, std::path::PathBuf);
impl Drop for Keep {
    fn drop(&mut self) {
        let _ = std::process::Command::new("cp").arg("-R").arg(&self.0).arg(&self.1).status();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let usage = "usage: page-get-silent <block.wasm> <dir> <keep> [--base PORT] [--watch SECS] [--silent-peers N]";
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (Some(bw), Some(dir), Some(keep)) = (a.first(), a.get(1), a.get(2)) else { bail!("{usage}") };
    let base: u16 = arg(&a, "--base").map(|v| v.parse()).transpose()?.unwrap_or(47_600);
    let bound = page::rto::NODE_GET_BOUND_MS as u64;
    let watch = Duration::from_secs(arg(&a, "--watch").map(|v| v.parse()).transpose()?.unwrap_or(bound / 1000 + 90));
    let silent_peers: u16 = arg(&a, "--silent-peers").map(|v| v.parse()).transpose()?.unwrap_or(1).max(1);
    let bcode = std::fs::read(bw).with_context(|| format!("reading {bw}"))?;
    let root = std::path::PathBuf::from(dir);
    let _tree = TempTree(root.clone());
    let _keep = Keep(root.join("b"), std::path::PathBuf::from(keep));

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
    let node_a = Node::spawn_private_network(base, base + 1, &root.join("a"), &a_extra)?;
    let joined = ["--gateway".to_string(), format!("127.0.0.1:{},{}", base + 1, hex(&public))];
    let mut others = Vec::new();
    for i in 1..silent_peers {
        others.push(Node::spawn_private_network(base + 20 + 10 * i, base + 21 + 10 * i, &root.join(format!("c{i}")), &joined)?);
    }
    let node_b = Node::spawn_private_network(base + 10, base + 11, &root.join("b"), &joined)?;
    let mut c: WebApi = connect(&node_b.ws()).await?;
    let wanted = usize::from(silent_peers);
    let by = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut peers: Vec<String> = Vec::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        c.send(ClientRequest::NodeQueries(NodeQuery::ConnectedPeers)).await?;
        if let Ok(Ok(HostResponse::QueryResponse(QueryResponse::ConnectedPeers { peers: p }))) = timeout(Duration::from_secs(5), c.recv()).await {
            peers = p.iter().map(|(_, addr)| addr.to_string()).collect();
        }
        if peers.len() >= wanted || tokio::time::Instant::now() > by {
            break;
        }
    }
    println!("{}", serde_json::json!({ "b": node_b.ws(), "b_connected_to": peers, "B_ms": bound }));
    if peers.len() < wanted {
        bail!("SETUP: b is connected to {} peers, not the {wanted} this run pauses", peers.len());
    }
    let mut _resume = Vec::new();
    for n in std::iter::once(&node_a).chain(others.iter()) {
        let pid = n.pid().context("a peer is not running")?;
        signal(pid, "-STOP")?;
        _resume.push(Resume(pid));
    }
    println!("{}", serde_json::json!({ "paused": _resume.len() }));

    // THE PAGE: a head whose root nobody holds, and a read that needs it.
    let t0 = Instant::now();
    let now = || Ms(1_000 + t0.elapsed().as_millis() as u64);
    let mut p = Page::unstarted(engine::Params::default(), PutPath::Page, now());
    let mut missing = [0u8; 32];
    getrandom::getrandom(&mut missing).expect("random");
    p.event(engine::Event::HeadRead { epoch: engine::Epoch(0), seq: 1, root: missing });
    p.event(engine::Event::Get { client: engine::ClientId(1), req_id: engine::read::ReqId(1), key: b"k".to_vec() });

    let mut by_contract: std::collections::HashMap<ContractInstanceId, [u8; 32]> = std::collections::HashMap::new();
    // Per key: every send (ms) and every answer (ms, what).
    let mut sends: BTreeMap<[u8; 32], Vec<u64>> = BTreeMap::new();
    let mut answers: BTreeMap<[u8; 32], Vec<(u64, String)>> = BTreeMap::new();
    let mut other_ops = 0usize;
    // The property, checked at each send: the key's previous GET was answered, or B + an RTO passed since it.
    let mut violations: Vec<String> = Vec::new();
    let end = tokio::time::Instant::now() + watch;
    while tokio::time::Instant::now() < end {
        p.tick(now());
        for op in p.take_ops() {
            let Op::Get { id } = op else {
                other_ops += 1;
                continue;
            };
            let at = now().0;
            let last_send = sends.get(&id).and_then(|s| s.last().copied());
            let answered_since = last_send.is_some_and(|l| answers.get(&id).is_some_and(|a| a.iter().any(|(t, _)| *t >= l)));
            if let Some(l) = last_send {
                if !answered_since && at < l + bound {
                    violations.push(format!("{} sent again at {at} ms, {} ms after an UNANSWERED GET (inside B)", hex(&id[..4]), at - l));
                }
            }
            sends.entry(id).or_default().push(at);
            let key = *container(&bcode, &id).key().id();
            by_contract.insert(key, id);
            c.send(ClientRequest::ContractOp(ContractRequest::Get { key, return_contract_code: false, subscribe: false, blocking_subscribe: false })).await?;
            println!("{}", serde_json::json!({ "sent": hex(&id[..4]), "ms": at }));
        }
        let got = match timeout(Duration::from_millis(50), c.recv()).await {
            Err(_) => continue,
            Ok(r) => r,
        };
        let at = now();
        let (id, answer, what) = match got {
            Ok(HostResponse::ContractResponse(ContractResponse::NotFound { instance_id })) => match by_contract.get(&instance_id) {
                Some(id) => (*id, Answer::GetMissed(*id), "not found".to_string()),
                None => continue,
            },
            Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { key, state, .. })) => match by_contract.get(key.id()) {
                Some(id) => (*id, Answer::Got { id: *id, bytes: state.as_ref().to_vec() }, format!("state {} bytes", state.size())),
                None => continue,
            },
            Ok(other) => {
                println!("{}", serde_json::json!({ "other": format!("{other:?}").chars().take(160).collect::<String>(), "ms": at.0 }));
                continue;
            }
            Err(e) => {
                println!("{}", serde_json::json!({ "error": format!("{e}").chars().take(200).collect::<String>(), "ms": at.0 }));
                continue;
            }
        };
        answers.entry(id).or_default().push((at.0, what.clone()));
        println!("{}", serde_json::json!({ "answer": hex(&id[..4]), "what": what, "ms": at.0 }));
        p.answer(answer, at);
    }
    let per_key: BTreeMap<String, serde_json::Value> = sends
        .iter()
        .map(|(id, s)| (hex(&id[..4]), serde_json::json!({ "sent_ms": s, "answers": answers.get(id).cloned().unwrap_or_default() })))
        .collect();
    println!("{}", serde_json::json!({ "verdict": {
        "watched_s": watch.as_secs(), "B_ms": bound, "silent_peers": silent_peers, "per_key": per_key,
        "other_ops_not_sent": other_ops, "violations": violations, "one_node_get_per_key_while_silent": violations.is_empty() && !sends.is_empty(),
        "b_logs_kept_at": keep,
    }}));
    drop(c);
    Ok(())
}
