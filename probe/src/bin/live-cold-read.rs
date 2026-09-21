//! M1's L3, and three findings that wait on it: a COLD read on a real node.
//!
//!  1. sdk#150 PR 5 — a cold read WIDER than one return carries (`max_gets` = 4; the engine asks for 8).
//!  2. sdk#166 — WHICH connection hears a reply produced in a call the NODE made (a `GetContractResponse`).
//!  3. sdk#166 — two tabs, DIFFERENT ranges, the SAME `req_id`, both cold and in flight together.
//!
//! A single isolated node can never run this: everything it put is "held here" (F14), so nothing is cold.
//! So: TWO private nodes on loopback. A is an isolated gateway with a THROWAWAY transport key made here; B joins
//! A and nothing else (`--skip-load-from-network --gateway 127.0.0.1:<A>,<pubkey>`). A writes and publishes;
//! B's delegate is installed with the SAME test signing key, so it reads the head A wrote and finds a tree it
//! holds none of. Every port is given explicitly and 7509/7609 are refused; each node lives in a temp tree that
//! is removed; each is killed by the handle it was spawned with; every wait is bounded and the run has a budget.
//!
//! usage: L3_PORT=<base port> live-cold-read <engine_delegate.wasm> <block.wasm> <register.wasm>
//!        (uses base, base+1 for A's ws/net and base+10, base+11 for B's)

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{TempTree, RESERVED};
use protocol::{Bound, Reply, Request, TestKey};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);
const BUDGET: Duration = Duration::from_secs(420);
const COLD_READ: Duration = Duration::from_secs(60);

struct Live { child: Option<Child>, ws: u16 }
impl Live {
    fn url(&self) -> String { format!("ws://127.0.0.1:{}/v1/contract/command?encodingProtocol=native", self.ws) }
}
impl Drop for Live { fn drop(&mut self) { if let Some(mut c) = self.child.take() { let _ = c.kill(); let _ = c.wait(); } } }

fn free(port: u16) -> Result<()> {
    if RESERVED.contains(&port) { bail!("port {port} belongs to someone else's node; refusing"); }
    if TcpStream::connect(("127.0.0.1", port)).is_ok() { bail!("something is already listening on {port}; refusing to share it"); }
    Ok(())
}

fn spawn(dir: &Path, ws: u16, net: u16, extra: &[String]) -> Result<Live> {
    free(ws)?; free(net)?;
    for sub in ["data", "config", "log"] { std::fs::create_dir_all(dir.join(sub))?; }
    let mut args: Vec<String> = vec!["network".into(), "--skip-load-from-network".into(),
        "--network-address".into(), "127.0.0.1".into(), "--network-port".into(), net.to_string(),
        "--ws-api-address".into(), "127.0.0.1".into(), "--ws-api-port".into(), ws.to_string(),
        "--data-dir".into(), dir.join("data").to_string_lossy().into_owned(),
        "--config-dir".into(), dir.join("config").to_string_lossy().into_owned(),
        "--log-dir".into(), dir.join("log").to_string_lossy().into_owned()];
    args.extend(extra.iter().cloned());
    let child = Command::new("freenet").args(&args)
        .stdout(Stdio::from(std::fs::File::create(dir.join("log/console.out"))?))
        .stderr(Stdio::from(std::fs::File::create(dir.join("log/console.err"))?))
        .spawn().context("spawning freenet")?;
    let mut live = Live { child: Some(child), ws };
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if let Some(c) = live.child.as_mut() { if let Ok(Some(st)) = c.try_wait() { bail!("a node exited before it was ready ({st})"); } }
        if TcpStream::connect(("127.0.0.1", ws)).is_ok() { std::thread::sleep(Duration::from_millis(400)); return Ok(live); }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("a node did not accept connections within 45 s")
}

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws).await.context("connecting")?;
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
/// What one connection hears for `window`, as (ms, line). Replies only; `Call` reports are compacted.
async fn hear(client: &mut WebApi, t0: Instant, window: Duration) -> Vec<(u128, String)> {
    let mut out = Vec::new();
    let end = Instant::now() + window;
    while Instant::now() < end {
        let left = end.saturating_duration_since(Instant::now());
        match timeout(left.min(Duration::from_millis(250)), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => for v in values {
                if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                    let line = match protocol::decode_reply(&m.payload.to_vec()) {
                        Ok(Reply::Page { req_id, entries, cursor, .. }) => format!("PAGE req {req_id}: {} rows, first {:?}, more: {}", entries.len(),
                            entries.first().map(|(k, _)| String::from_utf8_lossy(k).into_owned()), cursor.is_some()),
                        Ok(Reply::Unavailable { req_id, .. }) => format!("UNAVAILABLE req {req_id}"),
                        Ok(Reply::WriteState { write_id, state } | Reply::SessionWriteState { write_id, state, .. }) => format!("w{write_id}: {state:?}"),
                        Ok(Reply::Call { saw, ops, stranded, effects, dropped, note, .. }) => format!("call saw {saw:?}: effects {effects} ops {ops} dropped {dropped}{} STRANDED {stranded}", if note.is_empty() { String::new() } else { format!(" note {note:?}") }),
                        Ok(Reply::Identity { head_seq, .. }) => format!("identity: head seq {head_seq}"),
                        Ok(_) => continue, Err(e) => format!("undecodable reply: {e:?}") };
                    out.push((t0.elapsed().as_millis(), line));
                } },
            Ok(Ok(_)) => continue,
            // RECORDED, not a reason to stop listening. Breaking here hid the node's own words and returned to a
            // caller that ticked again at once: one refused request became 15 a second (run 5, B's log:
            // "the delegate is parked and its pending queue is full", 1530 times in 50 s).
            Ok(Err(e)) => {
                out.push((t0.elapsed().as_millis(), format!("NODE ERROR: {}", e.to_string().chars().take(160).collect::<String>())));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_) => continue,
        }
    }
    out
}
async fn register_and_install(client: &mut WebApi, wasm: &[u8], block: &[u8], register: &[u8], params: &[u8], sk: &SigningKey) -> Result<DelegateKey> {
    let (delegate, dkey) = wire::delegate_from_code(wasm);
    timeout(STEP, client.send(ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { delegate, cipher: [0u8; 32], nonce: [0u8; 24] })))
        .await.map_err(|_| anyhow::anyhow!("registering timed out"))??;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    send(client, &dkey, &Request::Install { block_code: block.to_vec(), register_code: register.to_vec(), register_params: params.to_vec(),
        signing_key: TestKey(sk.to_bytes().to_vec()) }).await?;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    Ok(dkey)
}
fn range(req_id: u64, prefix: &str, max_entries: u32) -> Request {
    let mut hi = prefix.as_bytes().to_vec(); *hi.last_mut().unwrap() += 1;
    Request::Range { req_id, lo: Bound::Included(prefix.as_bytes().to_vec()), hi: Bound::Excluded(hi), reverse: false, after: None, max_entries }
}
/// Prints the log; returns (effects stranded, node ops) summed over its call reports. `ops` counts NODE operations
/// only (entry.rs `node_ops`), so for a read it is the GETs issued -- the number M1 compares with the native twin.
fn show(title: &str, log: &[(u128, String)]) -> (u32, u32) {
    println!("  -- {title} --");
    let calls: Vec<&(u128, String)> = log.iter().filter(|(_, l)| l.starts_with("call")).collect();
    let stranded: u32 = calls.iter().filter_map(|(_, l)| l.rsplit("STRANDED ").next()?.parse::<u32>().ok()).sum();
    let ops: u32 = calls.iter().filter_map(|(_, l)| l.split(" ops ").nth(1)?.split(' ').next()?.parse::<u32>().ok()).sum();
    let show_calls = std::env::var("SHOW_CALLS").is_ok_and(|v| v == "1");
    for (ms, l) in log.iter().filter(|(_, l)| show_calls || !l.starts_with("call")) { println!("    {ms:>6} ms  {l}"); }
    println!("    ({} call reports heard here; node ops {ops}; effects stranded across them: {stranded})", calls.len());
    (stranded, ops)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let started = Instant::now();
    let base: u16 = std::env::var("L3_PORT").ok().and_then(|p| p.parse().ok()).context("L3_PORT=<base port> is required; there is no default")?;
    let mut a = std::env::args().skip(1);
    let (dw, bw, rw) = (a.next().context("usage: live-cold-read <engine_delegate.wasm> <block.wasm> <register.wasm>")?, a.next().unwrap_or_default(), a.next().unwrap_or_default());
    let wasm = std::fs::read(&dw).context("reading the engine delegate")?;
    probe::check(&wasm).map_err(|e| anyhow::anyhow!("the engine delegate is refused: {e}"))?;
    let (block, register) = (std::fs::read(&bw).context("block.wasm")?, std::fs::read(&rw).context("register.wasm")?);

    let root = std::env::temp_dir().join(format!("live-cold-read-{}", std::process::id()));
    let _tree = TempTree(root.clone());
    // A's transport key: THROWAWAY, made here, hex on disk as the node expects, gone with the temp tree.
    let mut secret = [0u8; 32]; seed(&mut secret);
    let public = curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(secret).to_bytes();
    std::fs::create_dir_all(root.join("a"))?;
    std::fs::write(root.join("a/transport.key"), hex(&secret))?;
    let node_a = spawn(&root.join("a"), base, base + 1, &["--is-gateway".into(), "--transport-keypair".into(), root.join("a/transport.key").to_string_lossy().into_owned(),
        "--public-network-address".into(), "127.0.0.1".into(), "--public-network-port".into(), (base + 1).to_string()])?;
    let node_b = spawn(&root.join("b"), base + 10, base + 11, &["--gateway".into(), format!("127.0.0.1:{},{}", base + 1, hex(&public))])?;
    println!("nodes: A = isolated gateway ws {} net {}; B = joined to A ONLY, ws {} net {}  (both private, loopback, temp dirs)", base, base + 1, base + 10, base + 11);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut sk_seed = [0u8; 32]; seed(&mut sk_seed); sk_seed[0] ^= 0x5A;
    let sk = SigningKey::from_bytes(&sk_seed);
    let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
    println!("key: ONE TEST signing key for this run, installed on BOTH delegates so B reads the head A wrote");

    // ---- A writes: w/ (300 rows, the wide read), a/ b/ c/ (60 rows each) ----
    let mut ca = connect(&node_a.url()).await?;
    let dkey = register_and_install(&mut ca, &wasm, &block, &register, &params, &sk).await?;
    send(&mut ca, &dkey, &Request::Identity).await?; let _ = hear(&mut ca, started, Duration::from_secs(2)).await;
    let mut rows: Vec<(String, u32)> = (0..300).map(|i| ("w".to_string(), i)).collect();
    for p in ["a", "b", "c"] { rows.extend((0..60).map(|i| (p.to_string(), i))); }
    let mut published = 0;
    for (n, chunk) in rows.chunks(40).enumerate() {
        let mut ops: Vec<protocol::Op> = chunk.iter().map(|(p, i)| { let mut v = vec![p.as_bytes()[0]; 1000]; v[..4].copy_from_slice(&i.to_le_bytes());
            protocol::Op::Put(format!("{p}/{i:04}").into_bytes(), v) }).collect();
        ops.sort_by(|x, y| match (x, y) { (protocol::Op::Put(k1, _), protocol::Op::Put(k2, _)) => k1.cmp(k2), _ => std::cmp::Ordering::Equal });
        let id = 100 + n as u64;
        // re-sent on Busy, as an outbox does
        let t = Instant::now(); let mut ok = false;
        'one: while t.elapsed() < Duration::from_secs(60) {
            send(&mut ca, &dkey, &Request::Write { write_id: id, ops: ops.clone() }).await?;
            let end = Instant::now() + Duration::from_secs(30);
            while Instant::now() < end {
                let got = hear(&mut ca, started, Duration::from_millis(500)).await;
                if got.iter().any(|(_, l)| *l == format!("w{id}: Published")) { ok = true; break 'one; }
                if got.iter().any(|(_, l)| *l == format!("w{id}: Busy")) { tokio::time::sleep(Duration::from_millis(500)).await; continue 'one; }
                send(&mut ca, &dkey, &Request::Tick { now: protocol::tick_of(now_ms()) }).await?;
            }
        }
        if !ok { bail!("SETUP FAILED on A: write {id} ({} rows) never published — nothing below would mean anything", chunk.len()); }
        published += chunk.len();
    }
    println!("setup: A published {published} rows in {} writes ({:.1} s)", rows.chunks(40).count(), started.elapsed().as_secs_f32());

    // ---- B: two tabs, the SAME delegate code, the SAME test key ----
    let mut t1 = connect(&node_b.url()).await?; let mut t2 = connect(&node_b.url()).await?;
    let dkey_b = register_and_install(&mut t1, &wasm, &block, &register, &params, &sk).await?;
    send(&mut t1, &dkey_b, &Request::Identity).await?;
    let id1 = hear(&mut t1, started, Duration::from_secs(8)).await;
    send(&mut t2, &dkey_b, &Request::Identity).await?;
    let id2 = hear(&mut t2, started, Duration::from_secs(4)).await;
    let head = id1.iter().chain(id2.iter()).filter(|(_, l)| l.starts_with("identity")).map(|(_, l)| l.clone()).next_back();
    println!("B: {:?}   <- PRECONDITION: a head seq > 0 means B found the tree A wrote; 0 means every read below is of an EMPTY tree", head);

    // ---- TEST 2 first (it needs the coldest tree): which connection hears a node-driven reply? ----
    println!("\nTEST 2  tab 1 asks Range a/ (60 rows, cold) as req 1; tab 2 asks NOTHING. Who hears the Page?");
    let t0 = Instant::now();
    send(&mut t1, &dkey_b, &range(1, "a/", 256)).await?;
    let (h1, h2) = tokio::join!(hear(&mut t1, t0, COLD_READ / 2), hear(&mut t2, t0, COLD_READ / 2));
    let (s2a, _) = show("tab 1 (the asker)", &h1); let (s2b, _) = show("tab 2 (silent)", &h2);
    let rows_of = |h: &[(u128, String)], req: u64| -> usize { h.iter().filter_map(|(_, l)| l.strip_prefix(&format!("PAGE req {req}: "))?.split(' ').next()?.parse::<usize>().ok()).sum() };
    let t2_rows = rows_of(&h1, 1);

    // ---- TEST 3: two tabs, different ranges, the same req_id, both cold ----
    println!("\nTEST 3  tab 1 asks Range b/ as req 7, tab 2 asks Range c/ as req 7, together. Each should hear 60 rows of ITS OWN prefix.");
    let t0 = Instant::now();
    send(&mut t1, &dkey_b, &range(7, "b/", 256)).await?;
    send(&mut t2, &dkey_b, &range(7, "c/", 256)).await?;
    let (h1, h2) = tokio::join!(hear(&mut t1, t0, COLD_READ / 2), hear(&mut t2, t0, COLD_READ / 2));
    // Judged: each tab hears 60 rows of ITS OWN prefix. It is also the cell that decides whether sdk#166 is live.
    let (s3a, _) = show("tab 1 (asked b/)", &h1); let (s3b, _) = show("tab 2 (asked c/)", &h2);
    let first_of = |h: &[(u128, String)]| -> Option<String> { h.iter().find_map(|(_, l)| l.strip_prefix("PAGE req 7: ").map(|x| x.to_string())) };
    let t3 = (first_of(&h1), first_of(&h2));

    // ---- TEST 1: the wide cold read ----
    println!("\nTEST 1  tab 1 asks Range w/ (300 rows, cold, wider than one return's 4 GETs), paging 256 at a time.");
    let mut t1 = connect(&node_b.url()).await?;   // a FRESH connection: nothing from the tests above is in its way
    send(&mut t1, &dkey_b, &Request::Identity).await?; let idw = hear(&mut t1, started, Duration::from_secs(3)).await;
    println!("    fresh connection: {:?}", idw.iter().filter(|(_, l)| l.starts_with("identity")).map(|(_, l)| l.clone()).next_back());
    let t0 = Instant::now(); let mut got = 0usize; let mut after: Option<Vec<u8>> = None; let mut all = Vec::new(); let mut req = 20u64;
    let mut last_tick = Instant::now() - Duration::from_secs(1);
    loop {
        let mut r = range(req, "w/", 256); if let Request::Range { after: a, .. } = &mut r { *a = after.clone(); }
        send(&mut t1, &dkey_b, &r).await?;
        let mut page: Option<(usize, bool)> = None; let end = Instant::now() + COLD_READ;
        while Instant::now() < end && page.is_none() {
            let h = hear(&mut t1, t0, Duration::from_millis(1000)).await;
            for (_, l) in &h { if l.starts_with(&format!("PAGE req {req}:")) { let n: usize = l.split(": ").nth(1).and_then(|x| x.split(' ').next()).and_then(|x| x.parse().ok()).unwrap_or(0); page = Some((n, l.ends_with("more: true"))); }
                               if l.starts_with(&format!("UNAVAILABLE req {req}")) { page = Some((0, false)); } }
            all.extend(h);
            // One tick a SECOND, by the wall clock, whatever the node answered.
            if last_tick.elapsed() >= Duration::from_secs(1) {
                send(&mut t1, &dkey_b, &Request::Tick { now: protocol::tick_of(now_ms()) }).await?;
                last_tick = Instant::now();
            }
        }
        match page { Some((n, more)) => { got += n; if !more || n == 0 { break; } after = Some(format!("w/{:04}", got - 1).into_bytes()); req += 1; }
                     None => { println!("    no answer to req {req} within {COLD_READ:?}"); break; } }
        if started.elapsed() > BUDGET { println!("    BUDGET reached"); break; }
    }
    let (s1, gets1) = show("tab 1", &all);
    println!("    => {got} of 300 rows in {:.1} s; live GETs (node ops over its calls) {gets1}", t0.elapsed().as_secs_f32());

    println!("\ndone in {:.1} s (budget {} s). Nodes are killed by their handles; temp tree removed.", started.elapsed().as_secs_f32(), BUDGET.as_secs());
    drop(node_b); drop(node_a);
    if let Ok(keep) = std::env::var("KEEP_LOGS") {
        for n in ["a", "b"] { let _ = std::fs::create_dir_all(format!("{keep}/{n}"));
            if let Ok(rd) = std::fs::read_dir(root.join(n).join("log")) { for f in rd.flatten() { let _ = std::fs::copy(f.path(), format!("{keep}/{n}/{}", f.file_name().to_string_lossy())); } } }
        println!("node logs copied to {keep}");
    }
    // THE VERDICT, now that there is a green to protect (sdk#150's read limit).
    let mut red: Vec<String> = Vec::new();
    // NOT ANSWERED is judged apart from ANSWERED WRONG, so a node that stops serving the delegate and an engine that
    // returns the wrong rows cannot be confused.
    let refused = all.iter().filter(|(_, l)| l.starts_with("NODE ERROR")).count();
    let pages = all.iter().filter(|(_, l)| l.starts_with("PAGE req")).count();
    if pages == 0 { red.push(format!("TEST 1 NOT ANSWERED: no page in the window; the node refused {refused} request(s): {:?}",
        all.iter().find(|(_, l)| l.starts_with("NODE ERROR")).map(|(_, l)| l.clone()))); }
    else if got != 300 { red.push(format!("TEST 1 ANSWERED WRONG: read {got} of 300 rows")); }
    if s1 != 0 { red.push(format!("TEST 1 stranded {s1} effect(s)")); }
    if t2_rows != 60 { red.push(format!("TEST 2 the asker heard {t2_rows} of 60 rows")); }
    if s2a + s2b != 0 { red.push(format!("TEST 2 stranded {} effect(s)", s2a + s2b)); }
    let own = |p: &Option<String>, prefix: &str| p.as_deref().is_some_and(|l| l.starts_with("60 rows") && l.contains(&format!("\"{prefix}0000\"")));
    if !own(&t3.0, "b/") || !own(&t3.1, "c/") { red.push(format!("TEST 3 a tab did not hear 60 rows of its own prefix: tab 1 {:?}, tab 2 {:?}", t3.0, t3.1)); }
    if s3a + s3b != 0 { red.push(format!("TEST 3 stranded {} effect(s)", s3a + s3b)); }
    if red.is_empty() { println!("VERDICT: GREEN -- cold reads come back whole, nothing stranded; compare live GETs {gets1} with the native twin"); Ok(()) }
    else { for r in &red { println!("RED: {r}"); } bail!("{} check(s) red", red.len()) }
}

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
/// Throwaway randomness for TEST keys only: the OS's, read from /dev/urandom.
fn seed(buf: &mut [u8]) { use std::io::Read; std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf)).expect("/dev/urandom"); }

/// This probe's session: sessioned, as every app is. It passed `CURRENT` to
/// the session-less encoder, which clamped it to v3 on the LEGACY session —
/// the one path no app takes (craftworks-sdk#194).
fn probe_session() -> u64 {
    protocol::mint_session(0x9E37_79B9_7F4A_7C15 ^ std::process::id() as u64)
}
