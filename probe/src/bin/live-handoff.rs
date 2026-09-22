//! M1's L4: a 100-row handoff of long text, on a live node, against the handoff's OWN budget — sent by the SDK.
//!
//! The architect's probe (`craftworks-docs/docs/reviews/2026-09-21-L4-live-handoff-probe.rs`) sent 102 writes
//! back-to-back as raw requests and found the node's fair queue (100 waiting + 1 in service, one key for every
//! delegate request) refused the 102nd with a host error and NO verdict. This run makes the same writes through
//! the SDK's own `CachedStore` — `put` per row, synchronously, as the handoff does — so what reaches the wire is
//! what the outbox decides to send. Its window (sdk#176, `WRITES_IN_FLIGHT`) is the thing under test.
//!
//! Each row is 5 KiB of distinct text, so each is a block of its own (> 1 KiB). A `Tick` and the store's own
//! `tick` once a second, as the page does. One PRIVATE node, isolated network mode on `L4_PORT` and `L4_PORT+1`,
//! every node directory inside `L4_TMP` (required: there is no default, so a run cannot land in a shared temp),
//! killed by its handle. Exit status is the verdict. `L4_ROWS` overrides 100.
//!
//! The verdict is on PROGRESS, not total time (builder#94): red if any 30 s passes with no newly published write,
//! or if any write is rolled back. The total is printed against the handoff's 30 s so the two can be compared: at
//! ~3.4 commits/s a 300-row publish takes ~90 s, which is the node's pace, not a failure. It listens up to 180 s.
//!
//! usage: L4_PORT=<port> L4_TMP=<dir> live-handoff <engine_delegate.wasm> <block.wasm> <register.wasm>

use anyhow::{bail, Context, Result};
use craftworks_sdk::cached_store::WRITES_IN_FLIGHT;
use craftworks_sdk::{CachedStore, Store as _};
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use protocol::{Bound, Reply, Request, TestKey, WriteState};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);
const HANDOFF_BUDGET: Duration = Duration::from_millis(30_000); // handoff.js `budgetMs`
const LISTEN: Duration = Duration::from_secs(180);
const NO_PROGRESS: Duration = Duration::from_millis(30_000); // builder#94: fail on no newly confirmed record

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws).await.context("connecting to the node")?;
    Ok(WebApi::start(stream))
}
async fn send_bytes(client: &mut WebApi, key: &DelegateKey, payload: Vec<u8>) -> Result<()> {
    timeout(STEP, client.send(ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: key.clone(), params: vec![].into(),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(payload))] })))
        .await.map_err(|_| anyhow::anyhow!("the node stopped accepting"))??;
    Ok(())
}
async fn send(client: &mut WebApi, key: &DelegateKey, r: &Request) -> Result<()> {
    send_bytes(client, key, protocol::encode_session_request(protocol::CURRENT, probe_session(), r).expect("encodes")).await
}
/// Everything the store has decided to send, sent. Returns how many frames.
async fn pump(client: &mut WebApi, key: &DelegateKey, store: &mut CachedStore) -> Result<usize> {
    let out = store.take_outbound();
    let n = out.len();
    for f in out { send_bytes(client, key, f).await?; }
    Ok(n)
}
fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("L4_PORT").ok().and_then(|p| p.parse().ok()).context("L4_PORT=<port> is required; there is no default")?;
    let tmp = std::env::var("L4_TMP").context("L4_TMP=<dir> is required; the node's data, config and log dirs go under it")?;
    let rows: u64 = std::env::var("L4_ROWS").ok().and_then(|p| p.parse().ok()).unwrap_or(100);
    let mut a = std::env::args().skip(1);
    let dw = a.next().context("usage: live-handoff <engine_delegate.wasm> <block.wasm> <register.wasm>")?;
    let (bw, rw) = (a.next().unwrap_or_default(), a.next().unwrap_or_default());
    let wasm = std::fs::read(&dw).context("reading the engine delegate")?;
    probe::check(&wasm).map_err(|e| anyhow::anyhow!("the engine delegate is refused: {e}"))?;
    let (block, register) = (std::fs::read(&bw).context("block.wasm")?, std::fs::read(&rw).context("register.wasm")?);
    println!("delegate under test: {} B, blake3 {}; rows {rows} x 5 KiB; window {WRITES_IN_FLIGHT}", wasm.len(), &blake3::hash(&wasm).to_hex()[..16]);

    let dir = std::path::Path::new(&tmp).join(format!("live-handoff-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 1 })?;
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

    // THE PAGE: the SDK's store, on the wall clock, speaking v4 under its own session.
    let mut store = CachedStore::new(Box::new(now_ms));
    let session = store.client.session().context("no session minted")?;
    store.client.send(&Request::Identity);
    pump(&mut client, &dkey, &mut store).await?;
    if let Ok(Ok(HostResponse::DelegateResponse { values, .. })) = timeout(Duration::from_secs(3), client.recv()).await {
        for v in values { if let OutboundDelegateMsg::ApplicationMessage(m) = v { store.on_inbound(&m.payload.to_vec()); } }
    }

    // THE HANDOFF'S WRITES: define, the rows, the marker — each a synchronous `put`, as handoff.js makes them.
    let text = |i: u64| -> Vec<u8> { let mut v = format!("{{\"title\":\"note {i}\",\"body\":\"").into_bytes(); let mut s = i.wrapping_mul(0x9E3779B97F4A7C15) | 1;
        while v.len() < 5 * 1024 { s ^= s << 13; s ^= s >> 7; s ^= s << 17; v.push(b'a' + (s % 26) as u8); } v.extend(b"\"}"); v };
    let first_id = store.next_write_id();
    let t0 = Instant::now();
    let mut sent_frames = 0usize;
    store.put(b"s/notes", br#"{"type":"Note","fields":[{"name":"title","kind":"text"},{"name":"body","kind":"text"}]}"#).expect("the store took the write");
    sent_frames += pump(&mut client, &dkey, &mut store).await?;
    for i in 0..rows {
        store.put(format!("d/notes/{:032x}", 0x0190_0000_0000u64 + i).as_bytes(), &text(i)).expect("the store took the write");
        sent_frames += pump(&mut client, &dkey, &mut store).await?;
    }
    store.put(b"d/craftworks.published/notes", b"{}").expect("the store took the write");
    sent_frames += pump(&mut client, &dkey, &mut store).await?;
    // A write the COPY refused (its own bound, `max_pending` = 256) was never sent: refused by name, reported
    // separately. Everything else must publish.
    let refused_by_copy: Vec<(u64, String)> = store.refused.drain(..).map(|(id, why)| (id, format!("{why:?}"))).collect();
    let ids: Vec<u64> = (first_id..store.next_write_id()).filter(|id| !refused_by_copy.iter().any(|(r, _)| r == id)).collect();
    let refused_rows = refused_by_copy.iter().filter(|(id, _)| *id > first_id && *id <= first_id + rows).count() as u64;
    let total = ids.len();
    let sent_ms = t0.elapsed().as_millis();
    let held_after_puts = store.held_count();

    let mut published: BTreeMap<u64, u128> = BTreeMap::new();
    let (mut busy, mut stalled, mut other_terminal, mut host_errors, mut stranded_calls, mut calls) = (0u32, 0u32, Vec::new(), 0u32, 0u32, 0u32);
    let mut error_log: Vec<(u128, String)> = Vec::new();
    let mut last_tick = Instant::now();
    let mut rolled_back: Vec<(u64, String)> = Vec::new();
    let mut last_progress = Instant::now();
    while t0.elapsed() < LISTEN && published.len() < total && last_progress.elapsed() < NO_PROGRESS {
        match timeout(Duration::from_millis(100), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => for v in values { if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                let bytes = m.payload.to_vec();
                match protocol::decode_reply(&bytes) {
                    Ok(Reply::SessionWriteState { session: s, write_id, state }) if s == session => match state {
                        WriteState::Published => { if !published.contains_key(&write_id) { last_progress = Instant::now(); } published.entry(write_id).or_insert(t0.elapsed().as_millis()); }
                        WriteState::Busy => busy += 1,
                        WriteState::Stalled => stalled += 1,
                        WriteState::Accepted | WriteState::ParityComplete => {}
                        other => other_terminal.push((write_id, format!("{other:?}"))),
                    },
                    Ok(Reply::Call { stranded, .. }) => { calls += 1; if stranded != 0 { stranded_calls += 1; } }
                    _ => {}
                }
                // The store hears every reply, as the page's does: a verdict opens the window.
                store.on_inbound(&bytes);
            } },
            Ok(Ok(_)) => {}
            Ok(Err(e)) => { host_errors += 1; error_log.push((t0.elapsed().as_millis(), format!("{e:?}").chars().take(220).collect::<String>())); }   // an error is DATA
            Err(_) => {}
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            store.send_tick(now_ms());
            rolled_back.extend(store.tick().rolled_back.into_iter().map(|(id, why)| (id, format!("{why:?}"))));
        }
        sent_frames += pump(&mut client, &dkey, &mut store).await?;
    }
    let all_ms = (published.len() == total).then(|| published.values().copied().max().unwrap_or(0));
    let longest_gap = { let mut v: Vec<u128> = published.values().copied().collect(); v.sort(); v.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0) };
    let at = |q: usize| { let mut v: Vec<u128> = published.values().copied().collect(); v.sort(); v.get((v.len() * q / 100).min(v.len().saturating_sub(1))).copied() };
    println!("refused BY THE COPY before sending: {} {:?}", refused_by_copy.len(), refused_by_copy.iter().map(|(_, w)| w.as_str()).collect::<std::collections::BTreeSet<_>>());
    println!("made {total} writes back-to-back in {sent_ms} ms; {held_after_puts} held by the window when the last was made");
    println!("published {} of {total}; first at {:?} ms, half at {:?} ms, ALL at {:?} ms   (handoff budget {} ms)", published.len(), at(0), at(50), all_ms, HANDOFF_BUDGET.as_millis());
    let missing: Vec<u64> = ids.iter().copied().filter(|id| !published.contains_key(id)).collect();
    println!("longest wait between two newly published writes {longest_gap} ms (no-progress bound {} ms); rolled back {} {:?}", NO_PROGRESS.as_millis(), rolled_back.len(), &rolled_back[..rolled_back.len().min(5)]);
    println!("within the handoff's total 30 s: {}", match all_ms { Some(ms) if ms <= HANDOFF_BUDGET.as_millis() => "yes", Some(_) => "NO (informational: the verdict is on progress)", None => "not all published" });
    println!("NEVER published: write ids {missing:?} ({first_id} = define, then rows in order, {} = marker)", first_id + total as u64 - 1);
    for (ms, e) in &error_log { println!("host error at {ms} ms: {e}"); }
    println!("window high-water {} (bound {WRITES_IN_FLIGHT}); frames sent {sent_frames}; still held {}; still pending in the copy {}",
        store.in_flight_high_water, store.held_count(), store.copy.pending_writes());
    println!("Busy answers {busy}; Stalled {stalled}; other terminal {other_terminal:?}; host errors {host_errors}; call reports {calls}, of which stranded {stranded_calls}");

    // READ BACK over a SECOND connection: every row, by page.
    let mut second = connect(&node.ws()).await?;
    send(&mut second, &dkey, &Request::Identity).await?; let _ = timeout(Duration::from_secs(3), second.recv()).await;
    let (mut got, mut after, mut req) = (0usize, None::<Vec<u8>>, 900u64);
    'pages: for _ in 0..50 {
        send(&mut second, &dkey, &Request::Range { req_id: req, lo: Bound::Included(b"d/notes/".to_vec()), hi: Bound::Excluded(b"d/notes0".to_vec()), reverse: false, after: after.clone(), max_entries: 256 }).await?;
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if let Ok(Ok(HostResponse::DelegateResponse { values, .. })) = timeout(Duration::from_millis(500), second.recv()).await {
                for v in values { if let OutboundDelegateMsg::ApplicationMessage(m) = v { if let Ok(Reply::Page { req_id, entries, cursor, .. }) = protocol::decode_reply(&m.payload.to_vec()) { if req_id == req {
                    got += entries.iter().filter(|(_, val)| val.len() >= 5 * 1024).count();
                    match cursor { Some(_) => { after = entries.last().map(|(k, _)| k.clone()); req += 1; continue 'pages; } None => break 'pages }
                } } } }
            }
        }
        break;
    }
    println!("read back over a second connection: {got} of {rows} rows with their full 5 KiB");
    drop(node);

    let mut red = Vec::new();
    if all_ms.is_none() { red.push(format!("only {} of {total} writes published (listened {} ms; stops at {} s or {} s with no progress)", published.len(), t0.elapsed().as_millis(), LISTEN.as_secs(), NO_PROGRESS.as_secs())); }
    if !rolled_back.is_empty() { red.push(format!("{} write(s) rolled back", rolled_back.len())); }
    if store.in_flight_high_water > WRITES_IN_FLIGHT { red.push(format!("{} writes outstanding at once, over the window of {WRITES_IN_FLIGHT}", store.in_flight_high_water)); }
    if host_errors != 0 { red.push(format!("{host_errors} host error(s): the node refused a request")); }
    if stalled != 0 { red.push(format!("{stalled} Stalled report(s)")); }
    if !other_terminal.is_empty() { red.push(format!("terminal states that are not Published: {other_terminal:?}")); }
    if stranded_calls != 0 { red.push(format!("{stranded_calls} call(s) stranded effects")); }
    if got as u64 != rows - refused_rows { red.push(format!("read back {got} of the {} rows the copy accepted", rows - refused_rows)); }
    if refused_by_copy.iter().any(|(_, w)| !w.starts_with("TooManyPending")) { red.push(format!("refused by the copy for another reason: {refused_by_copy:?}")); }
    if red.is_empty() { println!("VERDICT: GREEN"); Ok(()) } else { for r in &red { println!("RED: {r}"); } bail!("{} check(s) red", red.len()) }
}

/// This probe's session: sessioned, as every app is. It passed `CURRENT` to
/// the session-less encoder, which clamped it to v3 on the LEGACY session —
/// the one path no app takes (craftworks-sdk#194).
fn probe_session() -> u64 {
    protocol::mint_session(0x9E37_79B9_7F4A_7C15 ^ std::process::id() as u64)
}
