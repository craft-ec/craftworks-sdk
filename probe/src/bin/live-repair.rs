//! LIVE: an assets-dashboard pass REPAIRS what the node never received (sdk#479 P4; KEEPER §5).
//!
//! On a PRIVATE node (explicit data/config/log dirs under PAGE_TMP, --disable-auto-update; never 7509/7609), per ARM:
//! 1. a fresh key publishes 30 rows of distinct 2,000-byte values (one leaf group: k = 30 members + m = 8 parity);
//!    the probe WITHHOLDS 7 of the group's blocks, so the node never receives them: ONE value member (the first wave
//!    is the data and ONE parity per group, so a group signs with one member stalled -- k of k + 1), and SIX of the
//!    group's follow-up parity (sent once the head lands). The group keeps 31 of its 38: still k, so it is repairable,
//!    and the repair both REBUILDS a member and RECOMPUTES parity;
//! 2. that page is closed (a page closed before BACKED_UP: the keeper's case, COMMIT-LIFE §P);
//! 3. a second page on the same key and node reopens, and runs ONE full pass with the arm's policy;
//! 4. a second pass, `off` (watch only), measures what the node holds now.
//! ARM `always`: the first pass repairs the 7 (`repaired` = 7) and the second finds the group whole (margin m).
//! ARM `off` (the control): nothing repaired, and the second pass still finds the 7 absent (margin m - 7).
//!
//! Why 7 = m - 1 and not k - 1: a group is rebuilt from ANY k of its k + m, so at most m of it can be lost and still be
//! repaired (k - 1 would leave 9 of 38 -- below k, damaged, never repairable). And why only ONE member: withholding
//! more members than the first wave's one parity covers leaves the commit below k, so it is never signed (measured:
//! 7 members withheld -> the write never publishes).
//!
//! usage: PAGE_PORT=<port> PAGE_TMP=<dir> live-repair <signer.wasm> <block.wasm> <register.wasm>
use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use page::audit::{Report, Repair};
use page::server::{Server, SignerFacts};
use page::{Ms, Page, PutPath};
use page_io::{Artefacts, PageIo};
use probe::node::{Mode, Node, TempTree};
use protocol::{Reply, Request};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const SESSION: u64 = 11;
const ROWS: usize = 30;
/// Withheld of the value group: ONE member and SIX follow-up parity -- m - 1 = 7 in all, so the group keeps k.
const MEMBERS: usize = 1;
const PARITY: usize = 6;
const WITHHELD: usize = MEMBERS + PARITY;

fn now_ms(t0: Instant) -> u64 {
    1_000 + t0.elapsed().as_millis() as u64
}

type Sock = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Frames out, with the WITHHOLDING rule: a whole request (reassembled if chunked) that PUTs a block the rule names
/// is never sent -- all its frames dropped -- and counted.
struct Out {
    requests: probe::frames::Requests,
    /// Frames of a chunked request not yet whole.
    pending: Vec<Vec<u8>>,
    /// Blocks withheld: every PUT of one is dropped.
    withheld: BTreeSet<[u8; 32]>,
    /// Still to withhold: value members (any time), and the value group's parity (only once the head has LANDED --
    /// the follow-up parity; the first wave's one parity is what lets the group sign).
    members_left: usize,
    parity_left: usize,
    landed: bool,
    dropped: usize,
}

impl Out {
    async fn send(&mut self, sock: &mut Sock, frame: Vec<u8>) -> Result<()> {
        use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
        self.pending.push(frame.clone());
        let req = match self.requests.push("page", &frame).map_err(|e| anyhow::anyhow!(e))? {
            probe::frames::Frame::Partial => return Ok(()),
            probe::frames::Frame::Whole(r) => r,
        };
        let frames = std::mem::take(&mut self.pending);
        if let ClientRequest::ContractOp(ContractRequest::Put { state, .. }) = &req {
            if let Some((id, body)) = wire::block::block_of_state(state.as_ref()) {
                let kind = state.as_ref().first().copied();
                // A value member (2,000 bytes), or the VALUE group's parity (a symbol of one: longer than 2,000 bytes;
                // the root's group-of-one parity is a node's size, shorter).
                let member = kind == Some(freenet_prolly::kind::RAW) && body.len() == 2_000;
                let group_parity = kind == Some(freenet_prolly::kind::PARITY) && body.len() > 2_000;
                if !self.withheld.contains(&id) {
                    if member && self.members_left > 0 {
                        self.members_left -= 1;
                        self.withheld.insert(id);
                    } else if group_parity && self.landed && self.parity_left > 0 {
                        self.parity_left -= 1;
                        self.withheld.insert(id);
                    }
                }
                if self.withheld.contains(&id) {
                    self.dropped += 1;
                    return Ok(());
                }
            }
        }
        for f in frames {
            sock.send(Message::Binary(f.into())).await.context("send")?;
        }
        Ok(())
    }
}

/// Drive `io` over `sock` until `done`, or `budget` passes.
async fn drive(sock: &mut Sock, io: &mut PageIo, out: &mut Out, t0: Instant, budget: Duration, mut done: impl FnMut(&[Reply], &mut PageIo) -> bool) -> Result<Vec<Reply>> {
    let start = Instant::now();
    let mut replies = Vec::new();
    loop {
        out.landed = io.server.page.published().0 >= 1;
        for f in io.take_frames() {
            out.send(sock, f).await?;
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
                io.inbound(&b, Ms(now_ms(t0)));
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => bail!("the socket: {e}"),
            Ok(None) => bail!("the node closed the socket"),
            Err(_) => io.tick(Ms(now_ms(t0))),
        }
    }
}

/// A pass over the reopened page's tree: started with `policy`, driven to its report.
async fn pass(sock: &mut Sock, io: &mut PageIo, out: &mut Out, t0: Instant, policy: Repair) -> Result<Report> {
    io.audit(policy);
    let mut report = None;
    drive(sock, io, out, t0, Duration::from_secs(180), |_, io| {
        report = io.server.page.take_audit();
        report.is_some()
    })
    .await
    .with_context(|| format!("the {policy:?} pass"))?;
    report.context("no report")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let v = std::process::Command::new("freenet").arg("--version").output().context("freenet --version")?;
    println!("freenet: {}", String::from_utf8_lossy(&v.stdout).lines().next().unwrap_or_default());
    let port: u16 = std::env::var("PAGE_PORT").ok().and_then(|p| p.parse().ok()).context("PAGE_PORT=<port> is required; there is no default")?;
    probe::node::allowed_ports(&[port, port + 1])?;
    let tmp = std::env::var("PAGE_TMP").context("PAGE_TMP=<dir> is required: the node's three dirs go under it")?;
    let usage = "usage: live-repair <signer.wasm> <block.wasm> <register.wasm>";
    let mut a = std::env::args().skip(1);
    let signer_wasm = std::fs::read(a.next().context(usage)?)?;
    probe::check(&signer_wasm).map_err(|e| anyhow::anyhow!("the signer is refused by the import gate: {e}"))?;
    let block_code = std::fs::read(a.next().context(usage)?)?;
    let register_code = std::fs::read(a.next().context(usage)?)?;

    let mut verdict = Ok(());
    for (run, policy) in [Repair::Always, Repair::Off].into_iter().enumerate() {
        // A FRESH node per arm: one signer per node holds one key.
        let dir = std::path::PathBuf::from(&tmp).join(format!("live-repair-{}-{run}", std::process::id()));
        let _tree = TempTree(dir.clone());
        let mut node = Node::spawn_in(port, &dir, Mode::IsolatedNetwork { network_port: port + 1 })?;
        println!("arm {policy:?}: node ws {}, pid {:?}, dirs under {}", node.ws(), node.pid(), dir.display());
        match arm(&node.ws(), &signer_wasm, &block_code, &register_code, policy, run as u8).await {
            Ok(line) => println!("{line}"),
            Err(e) => {
                println!("RED: arm {policy:?}: {e:#}");
                verdict = Err(e);
            }
        }
        drop(node);
    }
    println!("VERDICT: {}", if verdict.is_ok() { "GREEN" } else { "RED" });
    verdict
}

async fn arm(ws: &str, signer_wasm: &[u8], block_code: &[u8], register_code: &[u8], policy: Repair, run: u8) -> Result<String> {
    let t0 = Instant::now();
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    seed[0] ^= run;
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);

    // 1. PUBLISH, withholding WITHHELD value blocks.
    let (mut sock, _) = tokio_tungstenite::connect_async(ws).await.context("connecting")?;
    let mut io = probe::page::for_key(&sk, signer_wasm, block_code.to_vec(), register_code.to_vec());
    let mut out = Out { requests: Default::default(), pending: Vec::new(), withheld: BTreeSet::new(), members_left: MEMBERS, parity_left: PARITY, landed: false, dropped: 0 };
    drive(&mut sock, &mut io, &mut out, t0, Duration::from_secs(60), |_, io| io.provisioned()).await.context("provisioning")?;
    io.client(&protocol::encode_session_request(4, SESSION, &Request::Identity).expect("encodes"));
    drive(&mut sock, &mut io, &mut out, t0, Duration::from_secs(60), |r, _| r.iter().any(|x| matches!(x, Reply::Identity { .. }))).await.context("identity")?;
    let ops = (0..ROWS)
        .map(|i| {
            // Full-width bytes (a deterministic generator per row): parity is stored TRIMMED, so a mostly-zero value
            // would make its group's parity a few bytes long, indistinguishable by size from anything else.
            let mut x = (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(run);
            let v: Vec<u8> = (0..2_000)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8 | 1
                })
                .collect();
            protocol::Op::Put(format!("r/{i:05}").into_bytes(), v)
        })
        .collect();
    io.client(&protocol::encode_session_request(4, SESSION, &Request::forced_write(1, ops)).expect("encodes"));
    let got = drive(&mut sock, &mut io, &mut out, t0, Duration::from_secs(180), |r, _| probe::verdict::write_outcome(1, r).is_some()).await.context("the write")?;
    if let Some(Err(why)) = probe::verdict::write_outcome(1, &got) {
        let states: Vec<String> = got.iter().map(|r| format!("{r:?}").chars().take(120).collect()).collect();
        bail!("the write: {why} after {} ms; {} PUT request(s) withheld; replies {states:?}; unusable {:?}", t0.elapsed().as_millis(), out.dropped, io.unusable());
    }
    // The follow-up parity goes out once the head lands: drive until all SIX are withheld.
    let wait = Instant::now();
    while out.withheld.len() < WITHHELD && wait.elapsed() < Duration::from_secs(120) {
        // One second at a time (the budget ends each drive): the loop, not `done`, reads `out`.
        let _ = drive(&mut sock, &mut io, &mut out, t0, Duration::from_secs(1), |_, _| false).await;
    }
    let root = io.server.page.published().1;
    let withheld: Vec<[u8; 32]> = out.withheld.iter().copied().collect();
    if withheld.len() != WITHHELD {
        bail!("THE SETUP: {} blocks withheld (want {WITHHELD}: {MEMBERS} member, {PARITY} parity)", withheld.len());
    }
    let publish_line = format!("published seq {} root {}; withheld {} block(s): {MEMBERS} member, {PARITY} parity ({} PUT request(s) dropped)", io.server.page.published().0, engine::short_id(&root), withheld.len(), out.dropped);
    // 2. The writer page is CLOSED (it would re-PUT them for ever: rule 7).
    drop(io);
    drop(sock);

    // 3. REOPEN on the same key and node; a fresh page, its signer provisioned already.
    let (mut sock2, _) = tokio_tungstenite::connect_async(ws).await.context("connecting again")?;
    let (container2, signer2) = wire::delegate_from_code(signer_wasm);
    let mut io2 = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page, Ms(now_ms(t0))), SignerFacts::default()),
        Artefacts { block_code: block_code.to_vec(), register_code: register_code.to_vec(), register_params: wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME), signer: signer2 },
    );
    io2.provision(container2, sk.to_bytes().to_vec());
    let mut out2 = Out { requests: Default::default(), pending: Vec::new(), withheld: BTreeSet::new(), members_left: 0, parity_left: 0, landed: true, dropped: 0 };
    drive(&mut sock2, &mut io2, &mut out2, t0, Duration::from_secs(60), |_, io| io.provisioned()).await.context("reopen: provisioning")?;
    io2.client(&protocol::encode_session_request(4, SESSION + 1, &Request::Identity).expect("encodes"));
    drive(&mut sock2, &mut io2, &mut out2, t0, Duration::from_secs(90), |r, io| r.iter().any(|x| matches!(x, Reply::Identity { .. })) && io.server.page.published().1 == root)
        .await
        .context("reopen: identity and the published head")?;

    // 4. The arm's pass, then a watch-only pass that measures.
    let first = pass(&mut sock2, &mut io2, &mut out2, t0, policy).await?;
    let second = pass(&mut sock2, &mut io2, &mut out2, t0, Repair::Off).await?;
    let fmt = |r: &Report| {
        format!(
            "groups {} (whole {}, degraded {}, damaged {}), margins {:?}, repaired {}, bytes {}, pending {}, rejected {}",
            r.groups,
            r.whole,
            r.degraded,
            r.damaged.len(),
            r.margins,
            r.repaired,
            r.bytes,
            r.pending,
            r.rejected.len()
        )
    };
    let margins: BTreeMap<i64, usize> = second.margins.clone();
    let (want_repaired, want_whole) = match policy {
        Repair::Always => (WITHHELD, true),
        Repair::Off | Repair::Below(_) => (0, false),
    };
    let line = format!("arm {policy:?}: {publish_line}; pass 1 ({policy:?}): {}; pass 2 (Off): {}; unusable {:?}", fmt(&first), fmt(&second), io2.unusable());
    if first.repaired != want_repaired {
        bail!("{line}\n  -> pass 1 repaired {} (want {want_repaired})", first.repaired);
    }
    let group_whole = second.degraded == 0 && second.damaged.is_empty();
    if group_whole != want_whole {
        bail!("{line}\n  -> after pass 1, the node's group is {} (want {})", if group_whole { "whole" } else { "short" }, if want_whole { "whole" } else { "short" });
    }
    if !want_whole && !margins.contains_key(&(8 - WITHHELD as i64)) {
        bail!("{line}\n  -> the control's group is not short by exactly {WITHHELD}");
    }
    Ok(format!("GREEN: {line}"))
}
