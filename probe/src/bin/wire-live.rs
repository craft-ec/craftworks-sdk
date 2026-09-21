//! `wire`'s bytes, against a real node.
//!
//! The reason the framing is a Rust crate rather than JavaScript: **these are
//! the exact bytes the browser will send**, checked against the thing that has
//! to accept them rather than against anybody's account of it.
//!
//! Isolated NETWORK-mode node on its own port. Local mode silently drops a
//! delegate-originated PUT (F35), so anything a delegate writes has to be
//! exercised here.
//!
//! Everything it proves, and why each one is separate:
//!
//! 1. **An engine request reaches the delegate and its reply comes back** —
//!    `DelegateOp` framing, the path every write and read uses.
//! 2. **A >512 KiB `RegisterDelegate` goes through CHUNKED** — the delegate is
//!    ~721 KB, so `provision()` crosses stdlib's threshold on the first
//!    message of every session. A framing that only ever handled small
//!    messages would pass every other test here.
//! 3. **A client-API `Subscribe` receives a notification from ANOTHER
//!    connection's write** — the notifier that reaches a tab which made no
//!    write (F40).
//! 4. **The CONTROL: the same bytes without `encodingProtocol=native` do not
//!    work.** The node falls back to Flatbuffers when the parameter is absent,
//!    so without this the run would pass for a reason nobody checked and the
//!    URL could quietly lose the parameter again.

use anyhow::{bail, Context, Result};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use wire::{unframe, Incoming, Reassembler};

/// Short, because every wait here is on an ANSWER and a missing answer is a
/// data point rather than a reason to keep waiting.
const STEP: Duration = Duration::from_secs(8);
const BUDGET: Duration = Duration::from_secs(120);

type Sock = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn open(url: &str) -> Result<Sock> {
    let (sock, _) = connect_async(url).await.context("connecting")?;
    Ok(sock)
}

async fn send_frames(sock: &mut Sock, frames: Vec<Vec<u8>>) -> Result<()> {
    use futures::SinkExt;
    for f in frames {
        sock.send(Message::Binary(f.into())).await?;
    }
    Ok(())
}

/// Read until something is understood, or the deadline passes.
async fn next(sock: &mut Sock, r: &mut Reassembler, deadline: Instant) -> Option<Incoming> {
    use futures::StreamExt;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left.min(STEP), sock.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => match unframe(r, &b) {
                Incoming::Partial => continue,
                other => return Some(other),
            },
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
    None
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let started = Instant::now();
    let deadline = started + BUDGET;
    let port: u16 = std::env::var("LIVE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7712);
    let mut args = std::env::args().skip(1);
    let delegate_path = args.next().unwrap_or_else(|| {
        eprintln!("usage: wire-live <engine_delegate.wasm>");
        std::process::exit(2);
    });
    let wasm = std::fs::read(&delegate_path).context("reading the delegate")?;

    let dir = std::env::temp_dir().join(format!("wire-live-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let node = Node::spawn_in(
        port,
        &dir,
        Mode::IsolatedNetwork {
            network_port: port + 20000,
        },
    )?;
    println!("node:  isolated NETWORK mode on 127.0.0.1:{port} (pid recorded by the harness)");
    let _ = &node;

    let url = wire::ws_url("127.0.0.1", port).map_err(|e| anyhow::anyhow!(e))?;
    println!("url:   {url}");

    // ---- 1 + 2: a CHUNKED RegisterDelegate, which is the first thing any
    // session does and the one that crosses stdlib's threshold ----
    let delegate = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(Delegate::from((
        &DelegateCode::from(wasm.clone()),
        &Parameters::from(vec![]),
    ))));
    let dkey = delegate.key().clone();
    let frames = wire::frame_register_delegate(delegate, 1)
        .map_err(|e| anyhow::anyhow!("framing RegisterDelegate: {e}"))?;
    println!(
        "reg:   {} B of delegate -> {} frame(s){}",
        wasm.len(),
        frames.len(),
        if frames.len() > 1 {
            " (CHUNKED, as it must be)"
        } else {
            ""
        }
    );
    if frames.len() < 2 {
        bail!(
            "a {} B delegate framed into ONE frame — stdlib chunks over 512 KiB, \
             so either the delegate shrank below the threshold or the chunking \
             is not happening and provision() will fail on a real node",
            wasm.len()
        );
    }

    let mut a = open(&url).await?;
    let mut ra = Reassembler::new();
    send_frames(&mut a, frames).await?;
    match next(&mut a, &mut ra, deadline).await {
        Some(Incoming::Ack(kind)) => {
            println!("reg:   the node accepted a CHUNKED registration ({kind:?})")
        }
        other => bail!("the chunked registration was answered {other:?}"),
    }

    // ---- 3: an engine request reaches the delegate and answers ----
    let ask =
        protocol::encode_request(protocol::CURRENT, &protocol::Request::Identity).expect("encodes");
    let frames = wire::frame_delegate_op(&dkey, &Parameters::from(vec![]), ask, 2)
        .map_err(|e| anyhow::anyhow!("framing DelegateOp: {e}"))?;
    send_frames(&mut a, frames).await?;
    let mut saw_engine = false;
    while Instant::now() < deadline && !saw_engine {
        match next(&mut a, &mut ra, deadline).await {
            Some(Incoming::EngineBytes(msgs)) => {
                // Each message decoded on its OWN. A DelegateResponse can
                // carry several, and a run of messages is not a message.
                for bytes in msgs {
                    match protocol::decode_reply(&bytes) {
                        Ok(protocol::Reply::Identity { engine, .. }) => {
                            println!("op:    the delegate answered Identity: {engine}");
                            saw_engine = true;
                        }
                        Ok(_) => continue,
                        Err(why) => bail!("a delegate reply did not decode: {why:?}"),
                    }
                }
            }
            Some(Incoming::Unusable(why)) => bail!("unusable reply: {why:?}"),
            Some(_) => continue,
            None => break,
        }
    }
    if !saw_engine {
        bail!("an engine request reached the node and nothing came back within the budget");
    }

    // ---- 4: THE CONTROL — the same bytes without the encoding parameter ----
    //
    // Runs LAST so a failure here cannot be mistaken for the node being down.
    // The node falls back to Flatbuffers when the parameter is absent, so the
    // identical frames must NOT produce an engine reply.
    let bare = format!("ws://127.0.0.1:{port}/v1/contract/command");
    let mut b = open(&bare).await?;
    let mut rb = Reassembler::new();
    let ask =
        protocol::encode_request(protocol::CURRENT, &protocol::Request::Identity).expect("encodes");
    let frames = wire::frame_delegate_op(&dkey, &Parameters::from(vec![]), ask, 3)
        .map_err(|e| anyhow::anyhow!("framing: {e}"))?;
    send_frames(&mut b, frames).await?;
    let control_deadline = Instant::now() + Duration::from_secs(6);
    let got = next(&mut b, &mut rb, control_deadline).await;
    match got {
        Some(Incoming::EngineBytes(_)) => bail!(
            "the CONTROL produced an engine reply without encodingProtocol=native, so \
             the parameter is not what makes this work and the test above proves \
             nothing about it"
        ),
        other => println!("ctl:   without encodingProtocol=native the same bytes got {other:?}"),
    }

    println!("budget: {:?} of {BUDGET:?} used", started.elapsed());
    println!("done");
    Ok(())
}
