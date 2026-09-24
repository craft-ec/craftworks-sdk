//! LIVE: writes through the PAGE PATH (ruling B) on a private node.
//!
//! The browser's shape exactly: one websocket to the node, `page-io`'s frames
//! sent byte for byte, every binary message fed back to it. The engine runs
//! here (in `page::Server`), the SIGNER delegate signs, blocks are page-PUT
//! with their code, the head Register is created by the first head's PUT.
//! Nothing of the engine delegate is involved.
//!
//! Publishes `ROWS` rows (default 100, then 300: main's switch-over gate) in
//! writes of `PER_WRITE`, each waited on until `Published`, then reads the
//! whole range back through the same page and checks every row.
//!
//! usage: PAGE_PORT=<port> PAGE_TMP=<dir> live-page-writes <signer.wasm> <block.wasm> <register.wasm>
//! The node's data, config and log dirs are all under PAGE_TMP (explicit, no
//! default); the node runs with --disable-auto-update (probe::node).

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use page::server::{Server, SignerFacts};
use page::{Ms, Page, PutPath};
use page_io::{Artefacts, PageIo};
use probe::node::{Mode, Node, TempTree};
use protocol::{Reply, Request};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const PER_WRITE_DEFAULT: usize = 10;

/// OPLOG=1 (scratch, the op count for the node's one queue, F61): every frame out and every message in, one line
/// each, `OP <ms> out|in <what>`, the ms on the page clock. Frames are decoded by stdlib's own request types and the
/// signer's own `decode_request`; inbound by page-io's decoder (`wire::unframe`).
fn oplog() -> bool {
    std::env::var("OPLOG").is_ok_and(|v| v == "1")
}

static CODES: std::sync::OnceLock<(freenet_stdlib::prelude::CodeHash, freenet_stdlib::prelude::CodeHash)> = std::sync::OnceLock::new();
/// The Block and Register code hashes, set once per process (every run uses the same two codes).
fn codes() -> &'static (freenet_stdlib::prelude::CodeHash, freenet_stdlib::prelude::CodeHash) {
    CODES.get().expect("codes set")
}

fn describe_out(f: &[u8], block_code: &freenet_stdlib::prelude::CodeHash, register_code: &freenet_stdlib::prelude::CodeHash) -> String {
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest, DelegateRequest};
    use freenet_stdlib::prelude::*;
    match bincode::deserialize::<ClientRequest>(f) {
        Ok(ClientRequest::ContractOp(ContractRequest::Put { contract, subscribe, .. })) => {
            let h = contract.key().code_hash().clone();
            let kind = if &h == block_code { "block" } else if &h == register_code { "REGISTER" } else { "other" };
            format!("PUT {kind}{}", if subscribe { " +subscribe" } else { "" })
        }
        Ok(ClientRequest::ContractOp(ContractRequest::Update { .. })) => "UPDATE (register)".into(),
        Ok(ClientRequest::ContractOp(ContractRequest::Get { subscribe, .. })) => format!("GET{}", if subscribe { " +subscribe" } else { "" }),
        Ok(ClientRequest::ContractOp(ContractRequest::Subscribe { .. })) => "SUBSCRIBE".into(),
        Ok(ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. })) => {
            let kinds: Vec<String> = inbound
                .iter()
                .map(|m| match m {
                    InboundDelegateMsg::ApplicationMessage(am) => match signer::decode_request(am.payload.as_ref()) {
                        Some((id, r)) => format!("signer#{id} {}", format!("{r:?}").split([' ', '{', '(']).next().unwrap_or("?")),
                        None => "undecodable signer message".into(),
                    },
                    other => format!("{other:?}").split([' ', '{', '(']).next().unwrap_or("?").to_string(),
                })
                .collect();
            format!("DELEGATE {}", kinds.join(", "))
        }
        Ok(ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { .. })) => "DELEGATE register".into(),
        Ok(other) => format!("{other:?}").split([' ', '{', '(']).next().unwrap_or("?").to_string(),
        Err(_) => "chunk/undecodable (StreamChunk)".into(),
    }
}

fn describe_in(i: &wire::Incoming) -> String {
    match i {
        wire::Incoming::EngineBytes(m) => {
            let kinds: Vec<String> = m.iter().map(|b| wire::signer::read_answer(b).map_or("?".into(), |(id, a)| format!("signer#{id} {}", format!("{a:?}").split([' ', '{', '(']).next().unwrap_or("?")))).collect();
            format!("signer answer {}", kinds.join(", "))
        }
        other => format!("{other:?}").chars().take(60).collect(),
    }
}
const SESSION: u64 = 11;

fn now_ms(t0: Instant) -> u64 {
    1_000 + t0.elapsed().as_millis() as u64
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let v = std::process::Command::new("freenet").arg("--version").output().context("freenet --version")?;
    println!("freenet: {}", String::from_utf8_lossy(&v.stdout).lines().next().unwrap_or_default());
    let port: u16 = std::env::var("PAGE_PORT").ok().and_then(|p| p.parse().ok()).context("PAGE_PORT=<port> is required; there is no default")?;
    if port == 7509 || port == 7609 {
        bail!("port {port} is the owner's node");
    }
    let tmp = std::env::var("PAGE_TMP").context("PAGE_TMP=<dir> is required: the node's three dirs go under it")?;
    let usage = "usage: live-page-writes <signer.wasm> <block.wasm> <register.wasm>";
    let mut a = std::env::args().skip(1);
    let signer_wasm = std::fs::read(a.next().context(usage)?)?;
    probe::check(&signer_wasm).map_err(|e| anyhow::anyhow!("the signer is refused by the import gate: {e}"))?;
    let block_code = std::fs::read(a.next().context(usage)?)?;
    let register_code = std::fs::read(a.next().context(usage)?)?;
    let rows: Vec<usize> = std::env::var("ROWS").ok().map(|r| r.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_else(|| vec![100, 300]);

    let mut verdict = Ok(());
    for (run, n) in rows.iter().enumerate() {
        // A FRESH node per run: one signer per node holds one key, and a
        // second key is refused (`KeyAlreadyProvisioned`), as it must be.
        let dir = std::path::PathBuf::from(&tmp).join(format!("live-page-writes-{}-{run}", std::process::id()));
        let _tree = TempTree(dir.clone());
        let node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 1 })?;
        println!("node {run}: ws {}, data/config/log under {}", node.ws(), dir.display());
        match one_run(&node.ws(), &signer_wasm, &block_code, &register_code, *n, run as u64).await {
            Ok(line) => println!("{line}"),
            Err(e) => {
                println!("RED: {n} rows: {e:#}");
                verdict = Err(e);
            }
        }
        drop(node);
    }
    println!("VERDICT: {}", if verdict.is_ok() { "GREEN" } else { "RED" });
    verdict
}

/// One fresh key (so one fresh Register), `n` rows published and read back.
async fn one_run(ws: &str, signer_wasm: &[u8], block_code: &[u8], register_code: &[u8], n: usize, run: u64) -> Result<String> {
    let (mut sock, _) = tokio_tungstenite::connect_async(ws).await.context("connecting")?;
    let t0 = Instant::now();
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    seed[0] ^= run as u8;
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let _ = CODES.set((
        freenet_stdlib::prelude::CodeHash::from_code(block_code),
        freenet_stdlib::prelude::CodeHash::from_code(register_code),
    ));
    let (container, signer) = wire::delegate_from_code(signer_wasm);
    let mut io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts {
            block_code: block_code.to_vec(),
            register_code: register_code.to_vec(),
            register_params: wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME),
            signer,
        },
    );
    io.provision(container, sk.to_bytes().to_vec());

    // Drive until `done` says so, or the budget runs out. Every frame out as
    // it is, every message in as it came; the page's timers on its own clock.
    async fn drive(
        sock: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
        io: &mut PageIo,
        t0: Instant,
        budget: Duration,
        drop_heads: &mut usize,
        mut done: impl FnMut(&[Reply], &PageIo) -> bool,
    ) -> Result<Vec<Reply>> {
        let start = Instant::now();
        let mut replies = Vec::new();
        // The probe's OWN reassembly, to see a whole message before deciding
        // to lose it: a GET answer may arrive as several chunks (0.2.136
        // streams them), and page-io must get all of a message or none.
        let mut seen = wire::Reassembler::default();
        let mut held: Vec<Vec<u8>> = Vec::new();
        loop {
            for f in io.take_frames() {
                if oplog() {
                    println!("OP {} out {}", now_ms(t0), describe_out(&f, &codes().0, &codes().1));
                }
                sock.send(Message::Binary(f.into())).await.context("send")?;
            }
            for r in io.take_replies() {
                replies.push(protocol::decode_reply(&r).map_err(|d| anyhow::anyhow!("a reply that does not decode: {d:?}"))?);
            }
            if done(&replies, io) {
                return Ok(replies);
            }
            if start.elapsed() > budget {
                bail!("not within {budget:?}; unusable: {:?}", io.unusable());
            }
            let wait = io.next_due().map_or(50, |d| d.0.saturating_sub(now_ms(t0)).clamp(1, 50));
            match tokio::time::timeout(Duration::from_millis(wait), sock.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => {
                    // The LOST-HEAD-READ case (sdk#175): a whole answer to the
                    // head Register's GET is discarded, as a lost read's.
                    held.push(b.to_vec());
                    let inc = wire::unframe(&mut seen, &b);
                    if oplog() && !matches!(inc, wire::Incoming::Partial) {
                        println!("OP {} in {}", now_ms(t0), describe_in(&inc));
                    }
                    match inc {
                        wire::Incoming::Partial => continue,
                        wire::Incoming::Got { id, .. } | wire::Incoming::GetFailed { id, .. } if *drop_heads > 0 && id == io.register_id() => {
                            *drop_heads -= 1;
                            held.clear();
                        }
                        _ => {
                            for f in held.drain(..) {
                                io.inbound(&f, Ms(now_ms(t0)));
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => bail!("the socket: {e}"),
                Ok(None) => bail!("the node closed the socket"),
                Err(_) => io.tick(Ms(now_ms(t0))),
            }
        }
    }

    drive(&mut sock, &mut io, t0, Duration::from_secs(60), &mut 0, |_, io| io.provisioned()).await.context("provisioning the signer")?;
    let provisioned_ms = t0.elapsed().as_millis();
    io.client(&protocol::encode_session_request(4, SESSION, &Request::Identity).expect("encodes"));
    drive(&mut sock, &mut io, t0, Duration::from_secs(60), &mut 0, |r, _| r.iter().any(|x| matches!(x, Reply::Identity { .. }))).await.context("identity")?;

    let mut write_ms = Vec::new();
    let per_write: usize = std::env::var("PER_WRITE").ok().and_then(|v| v.parse().ok()).unwrap_or(PER_WRITE_DEFAULT);
    // PREFILL=<rows>: one write of that many rows first (a realistic tree), then `per_write`-row writes.
    let prefill: usize = std::env::var("PREFILL").ok().and_then(|v| v.parse().ok()).unwrap_or(0).min(n);
    let all: Vec<usize> = (0..n).collect();
    let mut chunks: Vec<&[usize]> = Vec::new();
    if prefill > 0 {
        chunks.push(&all[..prefill]);
    }
    chunks.extend(all[prefill..].chunks(per_write));
    for (w, chunk) in chunks.into_iter().enumerate() {
        let id = w as u64 + 1;
        let ops = chunk.iter().map(|i| protocol::Op::Put(format!("r/{i:05}").into_bytes(), format!("value {i} of run {run}").into_bytes())).collect();
        let t = Instant::now();
        if oplog() {
            println!("OP {} WRITE {id} sent ({} row(s))", now_ms(t0), chunk.len());
        }
        // A write that does not read what it changes is sent FORCED
        // (sdk#283): a reads-less `Request::Write` is refused `Unread`.
        io.client(&protocol::encode_session_request(4, SESSION, &Request::forced_write(id, ops)).expect("encodes"));
        let got = drive(&mut sock, &mut io, t0, Duration::from_secs(120), &mut 0, |r, _| probe::verdict::write_outcome(id, r).is_some())
            .await
            .with_context(|| format!("write {id}"))?;
        // Published is what counts (ParityComplete follows it); anything
        // terminal without it is the failure, said with its reason.
        if let Some(Err(why)) = probe::verdict::write_outcome(id, &got) {
            bail!("{why}");
        }
        write_ms.push(t.elapsed().as_millis());
        if oplog() {
            println!("OP {} WRITE {id} PUBLISHED", now_ms(t0));
            // What follows Published (parity, read-backs, re-reads) before the next write: 2 s of the page's own traffic.
            let _ = drive(&mut sock, &mut io, t0, Duration::from_secs(2), &mut 0, |_, _| false).await;
            println!("OP {} WRITE {id} quiet window ends", now_ms(t0));
        }
    }

    // READ BACK: the whole range, through the same page.
    let req = Request::Range { req_id: 900, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 256 };
    let mut rows_read = 0usize;
    let mut after = None;
    loop {
        let r = match after.take() {
            None => req.clone(),
            Some(a) => Request::Range { req_id: 901 + rows_read as u64, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: Some(a), max_entries: 256 },
        };
        let rid = match &r {
            Request::Range { req_id, .. } => *req_id,
            _ => unreachable!(),
        };
        io.client(&protocol::encode_session_request(4, SESSION, &r).expect("encodes"));
        let got = drive(&mut sock, &mut io, t0, Duration::from_secs(60), &mut 0, |rs, _| rs.iter().any(|x| matches!(x, Reply::Page { req_id, .. } if *req_id == rid))).await.context("read back")?;
        let Some(Reply::Page { entries, cursor, .. }) = got.into_iter().find(|x| matches!(x, Reply::Page { req_id, .. } if *req_id == rid)) else { bail!("no page") };
        for (i, (k, v)) in entries.iter().enumerate() {
            let want = format!("r/{:05}", rows_read + i);
            if k != want.as_bytes() || !v.starts_with(format!("value {} of run {run}", rows_read + i).as_bytes()) {
                bail!("row {} read back as {:?}", rows_read + i, String::from_utf8_lossy(k));
            }
        }
        rows_read += entries.len();
        match cursor {
            Some(c) if !entries.is_empty() => after = Some(c),
            _ => break,
        }
    }
    if rows_read != n {
        bail!("read back {rows_read} of {n} rows");
    }
    // REOPEN, WITH THE HEAD READ LOST (sdk#175): a second page on the same
    // node and key — its signer provisioned already — whose first THREE head
    // reads are lost. It must ask again and read every row, never an empty tree.
    let (mut sock2, _) = tokio_tungstenite::connect_async(ws).await.context("connecting again")?;
    let (container2, signer2) = wire::delegate_from_code(signer_wasm);
    let mut io2 = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page), SignerFacts::default()),
        Artefacts {
            block_code: block_code.to_vec(),
            register_code: register_code.to_vec(),
            register_params: wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME),
            signer: signer2,
        },
    );
    // Registered again (the same delegate), provisioned again with the SAME
    // key (accepted: a key is never replaced, only re-stated).
    io2.provision(container2, sk.to_bytes().to_vec());
    drive(&mut sock2, &mut io2, t0, Duration::from_secs(60), &mut 0, |_, io| io.provisioned()).await.context("reopen: provisioning")?;
    let mut lose = 3usize;
    io2.client(&protocol::encode_session_request(4, SESSION + 1, &Request::Identity).expect("encodes"));
    drive(&mut sock2, &mut io2, t0, Duration::from_secs(60), &mut lose, |r, _| r.iter().any(|x| matches!(x, Reply::Identity { .. }))).await.context("reopen: identity")?;
    let rr = Request::Range { req_id: 7000, lo: protocol::Bound::Unbounded, hi: protocol::Bound::Unbounded, reverse: false, after: None, max_entries: 256 };
    io2.client(&protocol::encode_session_request(4, SESSION + 1, &rr).expect("encodes"));
    let got = drive(&mut sock2, &mut io2, t0, Duration::from_secs(90), &mut lose, |rs, _| rs.iter().any(|x| matches!(x, Reply::Page { req_id: 7000, .. }))).await.context("reopen: read")?;
    let first = got.iter().find_map(|x| if let Reply::Page { req_id: 7000, entries, .. } = x { Some(entries.len()) } else { None }).unwrap_or(0);
    if lose != 0 {
        bail!("reopen: the lost head reads were never asked again ({lose} left)");
    }
    if first != n.min(256) {
        bail!("reopen: the first page read {first} rows after 3 lost head reads (want {}): an empty tree?", n.min(256));
    }
    let (rto, srtt, window) = io.server.page.clock();
    let mut sorted = write_ms.clone();
    sorted.sort();
    Ok(format!(
        "GREEN: {n} rows in {} writes: every write Published and every row read back; a reopened page with 3 lost head reads read {first} rows. provisioned in {provisioned_ms} ms; per write median {} ms, max {} ms; total {} ms; page clock at the end: RTO {rto} ms, SRTT {:.1?} ms, GET window {window}; unusable {:?}",
        write_ms.len(),
        sorted[sorted.len() / 2],
        sorted.last().copied().unwrap_or(0),
        t0.elapsed().as_millis(),
        srtt,
        io.unusable()
    ))
}
