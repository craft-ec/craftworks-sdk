//! What a LIVE page probe (a `live-*` binary on a private node) needs every time: its clock, its arguments and the
//! checks on them, a page reopened on a key, the replies it decodes, and one wait on the socket. One home, so a probe
//! adds only what it measures.

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use page::server::{Server, SignerFacts};
use page::{Ms, Page, PutPath};
use page_io::{Artefacts, PageIo};
use protocol::Reply;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// A probe's socket to its node.
pub type Sock = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The page's clock: an epoch-like origin, never 0 (sdk#397: a zero-origin clock hid an RTO defect).
pub fn now_ms(t0: Instant) -> u64 {
    1_000 + t0.elapsed().as_millis() as u64
}

/// A live probe's inputs: its node's port (and `port + 1`, the network port), the dir its node's three dirs go under,
/// and the three contracts it runs.
pub struct Args {
    pub port: u16,
    pub tmp: String,
    pub signer_wasm: Vec<u8>,
    pub block_code: Vec<u8>,
    pub register_code: Vec<u8>,
}

/// Read a live probe's arguments (`PAGE_PORT`, `PAGE_TMP`, then `<signer.wasm> <block.wasm> <register.wasm>`), with
/// no defaults: the port refused if it is the owner's (or `port + 1` is), the signer refused by the import gate.
/// Prints the freenet version the probe runs against.
pub fn args(name: &str) -> Result<Args> {
    let v = std::process::Command::new("freenet").arg("--version").output().context("freenet --version")?;
    println!("freenet: {}", String::from_utf8_lossy(&v.stdout).lines().next().unwrap_or_default());
    let port: u16 = std::env::var("PAGE_PORT").ok().and_then(|p| p.parse().ok()).context("PAGE_PORT=<port> is required; there is no default")?;
    crate::node::allowed_ports(&[port, port + 1])?;
    let tmp = std::env::var("PAGE_TMP").context("PAGE_TMP=<dir> is required: the node's three dirs go under it")?;
    let usage = format!("usage: {name} <signer.wasm> <block.wasm> <register.wasm>");
    let mut a = std::env::args().skip(1);
    let signer_wasm = std::fs::read(a.next().context(usage.clone())?)?;
    crate::check(&signer_wasm).map_err(|e| anyhow::anyhow!("the signer is refused by the import gate: {e}"))?;
    let block_code = std::fs::read(a.next().context(usage.clone())?)?;
    let register_code = std::fs::read(a.next().context(usage)?)?;
    Ok(Args { port, tmp, signer_wasm, block_code, register_code })
}

/// A page REOPENED on `sk`'s key (its signer provisioned already on this node), unstarted at `t0`'s clock, and the
/// signer container its provisioning sends.
pub fn reopened(sk: &ed25519_dalek::SigningKey, signer_wasm: &[u8], block_code: &[u8], register_code: &[u8], t0: Instant) -> (freenet_stdlib::prelude::DelegateContainer, PageIo) {
    let (container, signer) = wire::delegate_from_code(signer_wasm);
    let io = PageIo::new(
        Server::new(Page::unstarted(engine::Params::default(), PutPath::Page, Ms(now_ms(t0))), SignerFacts::default()),
        Artefacts {
            block_code: block_code.to_vec(),
            register_code: register_code.to_vec(),
            register_params: wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME),
            signer,
        },
    );
    (container, io)
}

/// The replies `io` has for the app, decoded (one that does not decode is an error, never skipped).
pub fn replies(io: &mut PageIo) -> Result<Vec<Reply>> {
    io.take_replies().iter().map(|r| protocol::decode_reply(r).map_err(|d| anyhow::anyhow!("a reply that does not decode: {d:?}"))).collect()
}

/// ONE wait on the socket, no longer than `io`'s next due time (capped at 50 ms): a binary frame's bytes, or `None`
/// -- `io` ticked on a timeout, or a frame that is not binary. A socket error or close is an error.
pub async fn next_frame(sock: &mut Sock, io: &mut PageIo, t0: Instant) -> Result<Option<Vec<u8>>> {
    let wait = io.next_due().map_or(50, |d| d.0.saturating_sub(now_ms(t0)).clamp(1, 50));
    match tokio::time::timeout(Duration::from_millis(wait), sock.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => Ok(Some(b.to_vec())),
        Ok(Some(Ok(_))) => Ok(None),
        Ok(Some(Err(e))) => bail!("the socket: {e}"),
        Ok(None) => bail!("the node closed the socket"),
        Err(_) => {
            io.tick(Ms(now_ms(t0)));
            Ok(None)
        }
    }
}
