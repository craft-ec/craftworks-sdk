//! M1's L2 (two tabs, one delegate, overlapping writes — with the re-send after `Busy` it lacked) and L5 (two
//! protocol VERSIONS on one engine), on a live node, after sessions (sdk#146/#165).
//!
//! Two connections = two tabs. Tab 1 is a v4 SESSION; tab 2 is a second v4 session in phase A and an OLD (v2,
//! session-less) page in phase B. BOTH number their writes from 1 — that is the point.
//!  A. `OVERLAP_BIG=n` (default 8): tab 1 sends `w1` = its key + n distinct 30 KB values (n = 100 is > 64 blocks
//!     under real contract ids); tab 2 sends ITS `w1` at once. Each tab ticks once a second. A write answered `Busy`
//!     is re-sent, as an outbox does. PASS: each tab's `w1` is `Published`; every state a tab hears names ITS OWN
//!     session (a foreign or unnamed one is COUNTED and fails); over a THIRD connection every key reads back.
//!  B. each tab sends a write of 130 blocks — over `max_commit_blocks` (128). PASS: the v4 tab is told
//!     `TooLarge`, the v2 tab `Failed` (the downgrade), each under its own addressing, while the OTHER tab keeps ticking.
//! ONE private node via `probe::node` (explicit port; log, data and config dirs all set, under `L2_TMP`; temp tree;
//! killed by handle). Every frame goes through the SESSIONED encoder, explicitly (`encode_session_request`, a v2
//! page included — it writes the session-less form below v4), never the session-less one (craftworks-sdk#194).
//! Host errors are COUNTED and listening continues. Exit status is the verdict.
//!
//! A LIVE PROBE, NOT A GATE TEST. L2 STRICT — waiting for the FINAL state (`ParityComplete`) on BOTH tabs — is
//! craftworks-sdk#184's reproduction: a verdict produced in the other tab's call is delivered to that tab and
//! dropped as foreign, so at `OVERLAP_BIG=1` it is RED some of the time on main until the ledger lands (WRITE-PATH
//! revision 3). Run it by hand, on a private node, and read the verdict.
//!
//! usage: L2_PORT=<port> L2_TMP=<dir> [OVERLAP_BIG=n] live-two-tabs <engine_delegate.wasm> <block.wasm> <register.wasm>

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use protocol::{Bound, Reply, Request, TestKey, WriteState};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);

struct Tab { name: &'static str, client: WebApi, version: u16, session: u64, heard: Vec<(u128, String)>, own: Vec<WriteState>, foreign: u32, unnamed: u32, host_errors: u32 }

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws).await.context("connecting to the node")?;
    Ok(WebApi::start(stream))
}
impl Tab {
    async fn send(&mut self, key: &DelegateKey, r: &Request) -> Result<()> {
        let payload = protocol::encode_session_request(self.version, self.session, r).expect("encodes");
        timeout(STEP, self.client.send(ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
            key: key.clone(), params: vec![].into(),
            inbound: vec![InboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(payload))] })))
            .await.map_err(|_| anyhow::anyhow!("the node stopped accepting"))??;
        Ok(())
    }
    /// Listen for `window`; returns the states heard for THIS tab's write `id`.
    async fn hear(&mut self, t0: Instant, window: Duration, id: u64) {
        let end = Instant::now() + window;
        while Instant::now() < end {
            match timeout(Duration::from_millis(100), self.client.recv()).await {
                Ok(Ok(HostResponse::DelegateResponse { values, .. })) => for v in values { if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                    match protocol::decode_reply(&m.payload.to_vec()) {
                        Ok(Reply::SessionWriteState { session, write_id, state }) => {
                            if session == self.session { if write_id == id { self.own.push(state); } self.heard.push((t0.elapsed().as_millis(), format!("w{write_id}: {state:?}"))); }
                            else { self.foreign += 1; self.heard.push((t0.elapsed().as_millis(), format!("FOREIGN session {session:#x} w{write_id}: {state:?}"))); }
                        }
                        Ok(Reply::WriteState { write_id, state }) => {
                            if self.version < protocol::SESSION_SINCE { if write_id == id { self.own.push(state); } self.heard.push((t0.elapsed().as_millis(), format!("w{write_id}: {state:?} (unnamed, as an old page is told)"))); }
                            else { self.unnamed += 1; self.heard.push((t0.elapsed().as_millis(), format!("UNNAMED w{write_id}: {state:?}"))); }
                        }
                        _ => {}
                    } } },
                Ok(Ok(_)) => {}
                Ok(Err(_)) => { self.host_errors += 1; }
                Err(_) => {}
            }
        }
    }
}
fn now_tick() -> u64 { protocol::tick_of(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)) }
fn big(i: u64, size: usize) -> Vec<u8> { let mut v = vec![0u8; size]; let mut s = i.wrapping_mul(0x9E3779B97F4A7C15) | 1; for b in v.iter_mut() { s ^= s << 13; s ^= s >> 7; s ^= s << 17; *b = (s & 0xff) as u8; } v }

/// Both tabs' writes, together; ticks from both; `Busy` re-sent. Until both are terminal or `limit`.
async fn overlap(t1: &mut Tab, t2: &mut Tab, key: &DelegateKey, w1: Request, w2: Request, id: u64, limit: Duration) -> Result<(u32, u32)> {
    let t0 = Instant::now();
    t1.own.clear(); t2.own.clear();
    t1.send(key, &w1).await?; t2.send(key, &w2).await?;
    let (mut resent1, mut resent2) = (0u32, 0u32);
    let terminal = |s: &[WriteState]| s.iter().any(|x| matches!(x, WriteState::Published | WriteState::Failed | WriteState::Lost | WriteState::TooLarge { .. }));
    let mut last_tick = Instant::now();
    while t0.elapsed() < limit && !(terminal(&t1.own) && terminal(&t2.own)) {
        t1.hear(t0, Duration::from_millis(150), id).await; t2.hear(t0, Duration::from_millis(150), id).await;
        if t1.own.last() == Some(&WriteState::Busy) { t1.own.pop(); resent1 += 1; t1.send(key, &w1).await?; }
        if t2.own.last() == Some(&WriteState::Busy) { t2.own.pop(); resent2 += 1; t2.send(key, &w2).await?; }
        if last_tick.elapsed() >= Duration::from_secs(1) { last_tick = Instant::now();
            t1.send(key, &Request::Tick { now: now_tick() }).await?; t2.send(key, &Request::Tick { now: now_tick() }).await?; }
    }
    // "Published alone is not redundancy" (M1 A0): keep ticking until BOTH have heard ParityComplete, bounded.
    let parity = |s: &[WriteState]| s.contains(&WriteState::ParityComplete) || !s.contains(&WriteState::Published);
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until && !(parity(&t1.own) && parity(&t2.own)) {
        t1.hear(t0, Duration::from_millis(400), id).await; t2.hear(t0, Duration::from_millis(400), id).await;
        if last_tick.elapsed() >= Duration::from_secs(1) { last_tick = Instant::now();
            t1.send(key, &Request::Tick { now: now_tick() }).await?; t2.send(key, &Request::Tick { now: now_tick() }).await?; }
    }
    t1.hear(t0, Duration::from_millis(800), id).await; t2.hear(t0, Duration::from_millis(800), id).await;
    Ok((resent1, resent2))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("L2_PORT").ok().and_then(|p| p.parse().ok()).context("L2_PORT=<port> is required; there is no default")?;
    let n: u64 = std::env::var("OVERLAP_BIG").ok().and_then(|p| p.parse().ok()).unwrap_or(8);
    let mut a = std::env::args().skip(1);
    let dw = a.next().context("usage: live-two-tabs <engine_delegate.wasm> <block.wasm> <register.wasm>")?;
    let (bw, rw) = (a.next().unwrap_or_default(), a.next().unwrap_or_default());
    let wasm = std::fs::read(&dw).context("reading the engine delegate")?;
    probe::check(&wasm).map_err(|e| anyhow::anyhow!("the engine delegate is refused: {e}"))?;
    let (block, register) = (std::fs::read(&bw).context("block.wasm")?, std::fs::read(&rw).context("register.wasm")?);
    println!("delegate under test: {} B, blake3 {}; OVERLAP_BIG = {n}", wasm.len(), &blake3::hash(&wasm).to_hex()[..16]);

    let tmp = std::env::var("L2_TMP").context("L2_TMP=<dir> is required; the node's data, config and log dirs go under it")?;
    let dir = std::path::Path::new(&tmp).join(format!("live-two-tabs-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 1 })?;
    let mut seed = [0u8; 48]; { use std::io::Read; std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?; }
    let s1 = protocol::mint_session(u64::from_le_bytes(seed[32..40].try_into().unwrap()));
    let s2 = protocol::mint_session(u64::from_le_bytes(seed[40..48].try_into().unwrap()));
    let mut t1 = Tab { name: "tab 1 (v4)", client: connect(&node.ws()).await?, version: protocol::CURRENT, session: s1, heard: vec![], own: vec![], foreign: 0, unnamed: 0, host_errors: 0 };
    let mut t2 = Tab { name: "tab 2 (v4)", client: connect(&node.ws()).await?, version: protocol::CURRENT, session: s2, heard: vec![], own: vec![], foreign: 0, unnamed: 0, host_errors: 0 };

    let (delegate, dkey) = wire::delegate_from_code(&wasm);
    timeout(STEP, t1.client.send(ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { delegate, cipher: [0u8; 32], nonce: [0u8; 24] })))
        .await.map_err(|_| anyhow::anyhow!("registering timed out"))??;
    let _ = timeout(Duration::from_secs(2), t1.client.recv()).await;
    let sk = SigningKey::from_bytes(&seed[..32].try_into().unwrap());
    let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
    t1.send(&dkey, &Request::Install { block_code: block, register_code: register, register_params: params, signing_key: TestKey(sk.to_bytes().to_vec()) }).await?;
    let _ = timeout(Duration::from_secs(2), t1.client.recv()).await;
    t1.send(&dkey, &Request::Identity).await?; t2.send(&dkey, &Request::Identity).await?;
    let t00 = Instant::now(); t1.hear(t00, Duration::from_secs(2), 0).await; t2.hear(t00, Duration::from_secs(2), 0).await;

    // ---- A: L2 ----
    let mut ops1 = vec![protocol::Op::Put(b"k/tab1".to_vec(), b"from tab 1".to_vec())];
    for i in 0..n { ops1.push(protocol::Op::Put(format!("k/fill/{i:04}").into_bytes(), big(i, 30 * 1024))); }
    ops1.sort_by(|x, y| match (x, y) { (protocol::Op::Put(a, _), protocol::Op::Put(b, _)) => a.cmp(b), _ => std::cmp::Ordering::Equal });
    let w_tab1 = Request::Write { write_id: 1, ops: ops1 };
    let w_tab2 = Request::Write { write_id: 1, ops: vec![protocol::Op::Put(b"k/tab2".to_vec(), b"from tab 2".to_vec())] };
    let (r1, r2) = overlap(&mut t1, &mut t2, &dkey, w_tab1, w_tab2, 1, Duration::from_secs(90)).await?;
    println!("\nA. two sessions, both `w1`, together (tab 1's carries {n} x 30 KB):");
    for t in [&t1, &t2] { println!("  {} session {:#x}: its w1 was told {:?}; FOREIGN states heard {}; UNNAMED {}; host errors {}", t.name, t.session, t.own, t.foreign, t.unnamed, t.host_errors); }
    println!("  re-sent after Busy: tab 1 x{r1}, tab 2 x{r2}");
    let (a_own1, a_own2) = (t1.own.clone(), t2.own.clone());

    // read back over a THIRD connection
    let mut t3 = Tab { name: "tab 3", client: connect(&node.ws()).await?, version: protocol::CURRENT, session: protocol::mint_session(s1 ^ s2 ^ 0x5555), heard: vec![], own: vec![], foreign: 0, unnamed: 0, host_errors: 0 };
    t3.send(&dkey, &Request::Identity).await?; let _ = timeout(Duration::from_secs(2), t3.client.recv()).await;
    let (mut keys, mut after, mut req) = (Vec::<String>::new(), None::<Vec<u8>>, 700u64);
    'pages: for _ in 0..40 {
        t3.send(&dkey, &Request::Range { req_id: req, lo: Bound::Included(b"k/".to_vec()), hi: Bound::Excluded(b"k0".to_vec()), reverse: false, after: after.clone(), max_entries: 64 }).await?;
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if let Ok(Ok(HostResponse::DelegateResponse { values, .. })) = timeout(Duration::from_millis(500), t3.client.recv()).await {
                for v in values { if let OutboundDelegateMsg::ApplicationMessage(m) = v { if let Ok(Reply::Page { req_id, entries, cursor, .. }) = protocol::decode_reply(&m.payload.to_vec()) { if req_id == req {
                    keys.extend(entries.iter().map(|(k, _)| String::from_utf8_lossy(k).into_owned()));
                    match cursor { Some(_) => { after = entries.last().map(|(k, _)| k.clone()); req += 1; continue 'pages; } None => break 'pages }
                } } } }
            }
        }
        break;
    }
    let fills = keys.iter().filter(|k| k.starts_with("k/fill/")).count() as u64;
    println!("  read back over a third connection: k/tab1 {}, k/tab2 {}, fill {fills} of {n}", keys.iter().any(|k| k == "k/tab1"), keys.iter().any(|k| k == "k/tab2"));
    let (a_foreign, a_unnamed) = (t1.foreign + t2.foreign, t1.unnamed + t2.unnamed);

    // ---- B: L5 — tab 2 becomes an OLD page: v2, no session ----
    t2.version = 2; t2.name = "tab 2 (v2)";
    let too_many = |tag: &str| Request::Write { write_id: 2, ops: (0..130u64).map(|i| protocol::Op::Put(format!("z/{tag}/{i:04}").into_bytes(), big(1000 + i, 2048))).collect() };
    let _ = overlap(&mut t1, &mut t2, &dkey, too_many("v4"), too_many("v2"), 2, Duration::from_secs(60)).await?;
    println!("\nB. a write of 130 blocks (over max_commit_blocks = 128) from each, together:");
    println!("  tab 1 (v4, session) was told {:?}", t1.own);
    println!("  tab 2 (v2, no session) was told {:?}", t2.own);
    drop(node);

    let mut red = Vec::new();
    let published = |s: &[WriteState]| s.contains(&WriteState::Published);
    if !published(&a_own1) { red.push(format!("A: tab 1's w1 never Published: {a_own1:?}")); }
    if !published(&a_own2) { red.push(format!("A: tab 2's w1 never Published: {a_own2:?}")); }
    for (who, own) in [("tab 1", &a_own1), ("tab 2", &a_own2)] { if published(own) && !own.contains(&WriteState::ParityComplete) { red.push(format!("A: {who}'s w1 was Published and never told ParityComplete within 60 s of ticks: {own:?}")); } }
    if a_foreign != 0 { red.push(format!("A: {a_foreign} state(s) naming ANOTHER session reached a tab")); }
    if a_unnamed != 0 { red.push(format!("A: {a_unnamed} UNNAMED state(s) reached a v4 tab")); }
    if fills != n || !keys.iter().any(|k| k == "k/tab1") || !keys.iter().any(|k| k == "k/tab2") { red.push("A: not every key reads back".into()); }
    if !t1.own.iter().any(|s| matches!(s, WriteState::TooLarge { .. })) { red.push(format!("B: the v4 tab was told {:?}, not TooLarge", t1.own)); }
    if t2.own.last() != Some(&WriteState::Failed) { red.push(format!("B: the v2 tab was told {:?}, not Failed", t2.own)); }
    if t1.host_errors + t2.host_errors != 0 { red.push(format!("host errors: tab 1 {}, tab 2 {}", t1.host_errors, t2.host_errors)); }
    if std::env::var("SHOW_HEARD").is_ok_and(|v| v == "1") { for t in [&t1, &t2] { println!("-- {} heard --", t.name); for (ms, l) in &t.heard { println!("  {ms:>6} ms  {l}"); } } }
    if red.is_empty() { println!("\nVERDICT: GREEN"); Ok(()) } else { for r in &red { println!("RED: {r}"); } bail!("{} check(s) red", red.len()) }
}
