//! M1's L6: a write that changes NOTHING, on a live node (sdk#160, fixed by sdk#164).
//!
//! `Db::define` writes the schema bytes every time an app opens, so the second open of any app sends a write
//! whose tree is the published tree: no blocks, so no confirmation could ever arrive to bump the head, and before
//! sdk#164 the write was `Accepted`, never `Published`, and EVERY later write was `Busy`.
//!
//! One PRIVATE node, isolated network mode (a delegate's PUT is dropped in local mode), port given explicitly.
//!   1. write 1: the schema row                       -> must publish (the CONTROL: the node and delegate work)
//!   2. write 2: the SAME bytes again                 -> must be `Published` AT ONCE, with ZERO ops sent to the node
//!   3. write 3: a real, different write              -> must publish (THE ENGINE IS OPEN AFTERWARDS)
//!
//! Exit status is the verdict: 0 green, 1 red. Run it against the delegate built BEFORE sdk#164 to see it red.
//!
//! usage: L6_PORT=<port> live-noop-write <engine_delegate.wasm> <block.wasm> <register.wasm>

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use protocol::{Reply, Request, TestKey, WriteState};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);
const BUDGET: Duration = Duration::from_secs(150);
/// How long a write may take to publish here. F38 measured 130 ms for a head update on a node like this one; 20 s
/// is ~150x that, and far under `max_accept_age` (64 s), so a miss is a failure and not a slow pass.
const PUBLISH: Duration = Duration::from_secs(20);

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws).await.context("connecting to the node")?;
    Ok(WebApi::start(stream))
}
async fn send(client: &mut WebApi, key: &DelegateKey, r: &Request) -> Result<()> {
    let payload = protocol::encode_session_request(protocol::CURRENT, probe_session(), r).expect("encodes");
    timeout(STEP, client.send(ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: key.clone(), params: vec![].into(),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(payload))] })))
        .await.map_err(|_| anyhow::anyhow!("the node stopped accepting"))??;
    Ok(())
}
struct Heard { states: Vec<WriteState>, calls: u32, ops: u32, node_calls: u32, stranded: u32, first_published_ms: Option<u128> }
/// Send one write, tick once a second, listen until `Published` (+1 s for stragglers) or the deadline.
async fn write(client: &mut WebApi, key: &DelegateKey, id: u64, ops: Vec<protocol::Op>) -> Result<Heard> {
    let t0 = Instant::now();
    send(client, key, &Request::Write { write_id: id, ops }).await?;
    let mut h = Heard { states: vec![], calls: 0, ops: 0, node_calls: 0, stranded: 0, first_published_ms: None };
    let mut last_tick = Instant::now();
    let mut stop_at: Option<Instant> = None;
    while t0.elapsed() < PUBLISH && stop_at.is_none_or(|s| Instant::now() < s) {
        if let Ok(Ok(HostResponse::DelegateResponse { values, .. })) = timeout(Duration::from_millis(250), client.recv()).await {
            for v in values { if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                match protocol::decode_reply(&m.payload.to_vec()) {
                    Ok(Reply::WriteState { write_id, state } | Reply::SessionWriteState { write_id, state, .. }) if write_id == id => {
                        if state == WriteState::Published && h.first_published_ms.is_none() { h.first_published_ms = Some(t0.elapsed().as_millis()); stop_at = Some(Instant::now() + Duration::from_millis(2500)); }
                        if state == WriteState::Busy { stop_at = Some(Instant::now()); }
                        h.states.push(state);
                    }
                    Ok(Reply::Call { ops, stranded, saw, effects, note, .. }) => {
                        if std::env::var("SHOW_CALLS").is_ok_and(|v| v == "1") { println!("      w{id} +{:>5} ms  call saw {saw:?}: effects {effects} ops {ops} stranded {stranded} {note}", t0.elapsed().as_millis()); }
                        if !matches!(saw, protocol::Saw::Client) { h.node_calls += 1; }
                        if !matches!(saw, protocol::Saw::Client) || h.calls == 0 { h.ops += ops; } h.calls += 1; h.stranded += stranded; }
                    _ => {}
                } } }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) { last_tick = Instant::now();
            send(client, key, &Request::Tick { now: protocol::tick_of(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)) }).await?; }
    }
    Ok(h)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let started = Instant::now();
    let port: u16 = std::env::var("L6_PORT").ok().and_then(|p| p.parse().ok()).context("L6_PORT=<port> is required; there is no default")?;
    let mut a = std::env::args().skip(1);
    let dw = a.next().context("usage: live-noop-write <engine_delegate.wasm> <block.wasm> <register.wasm>")?;
    let (bw, rw) = (a.next().unwrap_or_default(), a.next().unwrap_or_default());
    let wasm = std::fs::read(&dw).context("reading the engine delegate")?;
    probe::check(&wasm).map_err(|e| anyhow::anyhow!("the engine delegate is refused: {e}"))?;
    let (block, register) = (std::fs::read(&bw).context("block.wasm")?, std::fs::read(&rw).context("register.wasm")?);
    println!("delegate under test: {dw} ({} B, blake3 {})", wasm.len(), &blake3::hash(&wasm).to_hex()[..16]);

    let dir = std::env::temp_dir().join(format!("live-noop-write-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 1 })?;
    println!("node: private, isolated NETWORK mode, ws {port} net {}", port + 1);
    let mut client = connect(&node.ws()).await?;
    let (delegate, dkey) = wire::delegate_from_code(&wasm);
    timeout(STEP, client.send(ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { delegate, cipher: [0u8; 32], nonce: [0u8; 24] })))
        .await.map_err(|_| anyhow::anyhow!("registering timed out"))??;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    let mut seed = [0u8; 32]; { use std::io::Read; std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?; }
    let sk = SigningKey::from_bytes(&seed);
    let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
    send(&mut client, &dkey, &Request::Install { block_code: block, register_code: register, register_params: params, signing_key: TestKey(sk.to_bytes().to_vec()) }).await?;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    send(&mut client, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), client.recv()).await;

    // what `Db::define("tasks", …)` writes: one row under the schema key, the schema as JSON
    let schema = || vec![protocol::Op::Put(b"s/tasks".to_vec(), br#"{"type":"Task","fields":[{"name":"title","kind":"text","required":true}]}"#.to_vec())];
    let line = |n: &str, h: &Heard| println!("{n}: told {:?}; published at {:?} ms; {} call reports of which NODE-DRIVEN {}; `Call.ops` {}; stranded {}", h.states, h.first_published_ms, h.calls, h.node_calls, h.ops, h.stranded);

    let w1 = write(&mut client, &dkey, 1, schema()).await?;                     line("write 1  define            ", &w1);
    let w2 = write(&mut client, &dkey, 2, schema()).await?;                     line("write 2  define AGAIN      ", &w2);
    let w3 = write(&mut client, &dkey, 3, vec![protocol::Op::Put(b"d/tasks/0001".to_vec(), b"a real record".to_vec())]).await?;
                                                                                line("write 3  a real write      ", &w3);
    drop(node);
    let mut red: Vec<String> = Vec::new();
    let published = |h: &Heard| h.states.contains(&WriteState::Published);
    if !published(&w1) { red.push("CONTROL FAILED: write 1 never published — the node or delegate is not working, nothing else means anything".into()); }
    if !published(&w2) { red.push(format!("write 2 (identical) was told {:?} and never Published", w2.states)); }
    // ZERO NODE OPS, shown structurally: every op sent to the node comes back as a call the NODE makes
    // (PutResponse / GetResponse / UpdateResponse). Before sdk#150's read-limit PR `Reply::Call.ops` could not be
    // used for this -- it counted the call's client replies too -- so both are checked.
    // And `Call.ops` itself, now that it counts node operations only (entry.rs `node_ops`): 0 for the no-op, and
    // not 0 for the real writes -- the control that makes the zero mean something.
    if published(&w2) && w2.ops != 0 { red.push(format!("write 2 changed nothing and its calls report {} node op(s)", w2.ops)); }
    if (published(&w1) && w1.ops == 0) || (published(&w3) && w3.ops == 0) { red.push("CONTROL: a real write's calls report 0 node ops, so `ops == 0` proves nothing".into()); }
    if published(&w2) && w2.node_calls != 0 { red.push(format!("write 2 changed nothing and the node still made {} call(s) on its behalf", w2.node_calls)); }
    if (published(&w1) && w1.node_calls == 0) || (published(&w3) && w3.node_calls == 0) { red.push("CONTROL: a real write produced no node-driven call, so `node_calls == 0` proves nothing".into()); }
    if let Some(ms) = w2.first_published_ms { if ms > 1000 { red.push(format!("write 2 took {ms} ms to publish: not 'at once'")); } }
    if !published(&w3) { red.push(format!("THE ENGINE IS NOT OPEN AFTERWARDS: write 3 was told {:?}", w3.states)); }
    if w1.stranded + w2.stranded + w3.stranded != 0 { red.push("a call stranded effects".into()); }
    println!("done in {:.1} s (budget {} s)", started.elapsed().as_secs_f32(), BUDGET.as_secs());
    if red.is_empty() { println!("VERDICT: GREEN — a no-op write publishes at once with zero ops, and the engine stays open"); Ok(()) }
    else { for r in &red { println!("RED: {r}"); } bail!("{} check(s) red", red.len()) }
}

/// This probe's session: sessioned, as every app is. It passed `CURRENT` to
/// the session-less encoder, which clamped it to v3 on the LEGACY session —
/// the one path no app takes (craftworks-sdk#194).
fn probe_session() -> u64 {
    protocol::mint_session(0x9E37_79B9_7F4A_7C15 ^ std::process::id() as u64)
}
