//! sdk#146: two tabs, one delegate, both at write id 1 -- who is told what?
//!
//! Every tab and every page load is `ClientId(1)` starting at `WriteId(1)`, and
//! a `Reply::WriteState` names only the write id. The architect showed on the
//! ENGINE that tab 1's `w1: Published` would clear tab 2's queued `w1` as
//! published -- lost and acknowledged -- IF the node delivers that reply to
//! tab 2's connection. That `Published` comes from a `process()` call the NODE
//! triggers (its answer to the delegate's PUT), not from either tab. Whether
//! it reaches tab 2 is a fact about the node, and this measures it.
//!
//! Its OWN node, isolated network mode (a delegate's PUT is dropped in local
//! mode), on a port given explicitly -- there is no default, and the
//! launcher refuses 7509/7609 and any port something already holds. The node
//! is killed by the handle it was spawned with.
//!
//! usage: LIVE_PORT=<port> live-overlap <engine_delegate.wasm> <block.wasm> <register.wasm>

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use protocol::{Reply, Request, TestKey};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);
/// How long both connections are listened to after the two writes.
const LISTEN: Duration = Duration::from_secs(30);

/// Listen to one connection until `want` appears in what it hears, or `limit`.
async fn until_heard(
    client: &mut WebApi,
    t0: Instant,
    want: &str,
    limit: Duration,
) -> Vec<(u128, String)> {
    let mut all = Vec::new();
    let end = Instant::now() + limit;
    while Instant::now() < end {
        let got = listen(client, t0, Duration::from_millis(200)).await;
        let hit = got.iter().any(|(_, w)| w.contains(want));
        all.extend(got);
        if hit {
            break;
        }
    }
    all
}

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws)
        .await
        .context("connecting to the node")?;
    Ok(WebApi::start(stream))
}

async fn send(client: &mut WebApi, key: &DelegateKey, r: &Request) -> Result<()> {
    // PROTO=n speaks an older version, to run this against an older delegate.
    let v: u16 = std::env::var("PROTO")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(protocol::CURRENT);
    let payload = protocol::encode_request(v, r).expect("encodes");
    timeout(
        STEP,
        client.send(ClientRequest::DelegateOp(
            DelegateRequest::ApplicationMessages {
                key: key.clone(),
                params: vec![].into(),
                inbound: vec![InboundDelegateMsg::ApplicationMessage(
                    ApplicationMessage::new(payload),
                )],
            },
        )),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the node stopped accepting"))??;
    Ok(())
}

/// Everything one connection hears in `window`: (ms since t0, what).
async fn listen(client: &mut WebApi, t0: Instant, window: Duration) -> Vec<(u128, String)> {
    listen_ticking(client, t0, window, None).await
}

/// As `listen`, and if `tick` names the delegate, send `Request::Tick` once a
/// second the way a page does -- the engine has no clock, and what a commit
/// waits on after `Accepted` (read-backs, retries) is driven by ticks.
async fn listen_ticking(
    client: &mut WebApi,
    t0: Instant,
    window: Duration,
    tick: Option<&DelegateKey>,
) -> Vec<(u128, String)> {
    let mut log = Vec::new();
    let end = Instant::now() + window;
    let mut next_tick = Instant::now();
    while Instant::now() < end {
        if let Some(k) = tick {
            if Instant::now() >= next_tick {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if send(
                    client,
                    k,
                    &Request::Tick {
                        now: protocol::tick_of(now),
                    },
                )
                .await
                .is_err()
                {
                    log.push((t0.elapsed().as_millis(), "tick could not be sent".into()));
                }
                next_tick = Instant::now() + Duration::from_millis(protocol::TICK_MS);
            }
        }
        let left = end.saturating_duration_since(Instant::now()).min(
            next_tick
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(10)),
        );
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        let owned = m.payload.to_vec();
                        let what = match protocol::decode_reply(&owned) {
                            Ok(Reply::WriteState { write_id, state }) => {
                                format!("WriteState w{write_id}: {state:?}")
                            }
                            Ok(Reply::Value { value, .. }) => format!(
                                "Value {:?}",
                                value.map(|v| String::from_utf8_lossy(&v).into_owned())
                            ),
                            Ok(Reply::Call {
                                saw,
                                ops,
                                awaiting,
                                read_back,
                                stranded,
                                dropped,
                                effects,
                                note,
                                ..
                            }) => {
                                if std::env::var("SHOW_CALLS").is_ok_and(|v| v == "1") {
                                    let why = if note.starts_with("stranded:") {
                                        format!("  [{}]", note.split(" | ").next().unwrap_or(""))
                                    } else {
                                        String::new()
                                    };
                                    format!("call: saw {saw:?} effects {effects} ops {ops} awaiting {awaiting} read_back {read_back} stranded {stranded} dropped {dropped}{why}")
                                } else {
                                    continue;
                                }
                            }
                            Ok(other) => format!("{:?}", other).chars().take(60).collect(),
                            Err(e) => format!("undecodable reply: {e:?}"),
                        };
                        log.push((t0.elapsed().as_millis(), what));
                    }
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => {
                log.push((t0.elapsed().as_millis(), format!("connection error: {e}")));
                break;
            }
            Err(_) => continue,
        }
    }
    log
}

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("LIVE_PORT")
        .context("LIVE_PORT is required: there is no default port")?
        .parse()
        .context("LIVE_PORT must be a port number")?;
    let mut args = std::env::args().skip(1);
    let (Some(dw), Some(bw), Some(rw)) = (args.next(), args.next(), args.next()) else {
        bail!("usage: LIVE_PORT=<port> live-overlap <engine_delegate.wasm> <block.wasm> <register.wasm>");
    };
    let wasm = std::fs::read(&dw).context("reading the engine delegate")?;
    probe::check(&wasm).map_err(|e| anyhow::anyhow!("the engine delegate is refused: {e}"))?;
    let block_code = std::fs::read(&bw).context("reading the block contract")?;
    let register_code = std::fs::read(&rw).context("reading the register contract")?;

    let dir = std::env::temp_dir().join(format!("live-overlap-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let node = Node::spawn_in(
        port,
        &dir,
        Mode::IsolatedNetwork {
            network_port: port + 20000,
        },
    )?;
    println!(
        "node: isolated NETWORK mode, ws 127.0.0.1:{port}, own temp dir {}",
        dir.display()
    );

    let mut tab1 = connect(&node.ws()).await?;
    let (delegate, dkey) = wire::delegate_from_code(&wasm);
    timeout(
        STEP,
        tab1.send(ClientRequest::DelegateOp(
            DelegateRequest::RegisterDelegate {
                delegate,
                cipher: [0u8; 32],
                nonce: [0u8; 24],
            },
        )),
    )
    .await
    .map_err(|_| anyhow::anyhow!("registering the delegate timed out"))??;
    let _ = timeout(Duration::from_secs(2), tab1.recv()).await;
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed))?;
    let sk = SigningKey::from_bytes(&seed);
    let register_params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
    send(
        &mut tab1,
        &dkey,
        &Request::Install {
            block_code,
            register_code,
            register_params,
            signing_key: TestKey(sk.to_bytes().to_vec()),
        },
    )
    .await?;
    let _ = timeout(Duration::from_secs(2), tab1.recv()).await;
    send(&mut tab1, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), tab1.recv()).await;

    let mut tab2 = connect(&node.ws()).await?;
    send(&mut tab2, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), tab2.recv()).await;
    println!("setup: delegate {dkey} provisioned; two connections open, both said Identity");

    // THE OVERLAP: both tabs send write id 1, tab 2 immediately after tab 1,
    // so tab 1's commit is in flight when tab 2's arrives.
    let t0 = Instant::now();
    // OVERLAP_BIG=n makes tab 1's write n distinct 30 KB values besides its
    // key, so its commit takes long enough for tab 2's to arrive during it.
    let big: usize = std::env::var("OVERLAP_BIG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut ops1 = vec![protocol::Op::Put(
        b"k/tab1".to_vec(),
        b"from tab 1".to_vec(),
    )];
    for i in 0..big {
        let mut v = vec![0u8; 30 * 1024];
        v[..8].copy_from_slice(&(i as u64).to_le_bytes());
        ops1.push(protocol::Op::Put(format!("k/fill/{i:04}").into_bytes(), v));
    }
    ops1.sort_by(|a, b| match (a, b) {
        (protocol::Op::Put(x, _), protocol::Op::Put(y, _)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    send(
        &mut tab1,
        &dkey,
        &Request::Write {
            write_id: 1,
            ops: ops1,
        },
    )
    .await?;
    // AFTER_ACCEPT=1: send tab 2's write only once tab 1 has been told its w1
    // is Accepted and before it is Published -- the window the hazard needs.
    let after_accept = std::env::var("AFTER_ACCEPT").is_ok_and(|v| v == "1");
    let mut early1 = Vec::new();
    if after_accept {
        early1 = until_heard(&mut tab1, t0, "w1: Accepted", Duration::from_secs(60)).await;
        let published_already = early1.iter().any(|(_, w)| w.contains("w1: Published"));
        println!(
            "tab 1 heard Accepted at {:?} ms; already Published: {published_already}",
            early1
                .iter()
                .find(|(_, w)| w.contains("Accepted"))
                .map(|x| x.0)
        );
    }
    send(
        &mut tab2,
        &dkey,
        &Request::Write {
            write_id: 1,
            ops: vec![protocol::Op::Put(
                b"k/tab2".to_vec(),
                b"from tab 2".to_vec(),
            )],
        },
    )
    .await?;
    println!(
        "sent: tab1 w1 at 0 ms, tab2 w1 at {} ms",
        t0.elapsed().as_millis()
    );
    let listen_for = std::env::var("LISTEN_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(LISTEN);

    // TICK_ON = none | tab1 | tab2 | both: which connections send time.
    let tick_on = std::env::var("TICK_ON").unwrap_or_else(|_| "none".into());
    println!("ticks from: {tick_on}");
    let t1 = matches!(tick_on.as_str(), "tab1" | "both").then_some(&dkey);
    let t2 = matches!(tick_on.as_str(), "tab2" | "both").then_some(&dkey);
    let (mut log1, log2) = tokio::join!(
        listen_ticking(&mut tab1, t0, listen_for, t1),
        listen_ticking(&mut tab2, t0, listen_for, t2)
    );
    early1.append(&mut log1);
    let log1 = early1;
    println!("\n-- tab 1's connection --");
    for (ms, w) in &log1 {
        println!("  {ms:>6} ms  {w}");
    }
    println!("-- tab 2's connection --");
    for (ms, w) in &log2 {
        println!("  {ms:>6} ms  {w}");
    }

    // What is STORED, read over a third connection.
    let mut reader = connect(&node.ws()).await?;
    let mut stored = Vec::new();
    for (i, k) in [&b"k/tab1"[..], b"k/tab2"].iter().enumerate() {
        send(
            &mut reader,
            &dkey,
            &Request::Get {
                req_id: 10 + i as u64,
                key: k.to_vec(),
            },
        )
        .await?;
        let heard = listen(&mut reader, t0, Duration::from_secs(5)).await;
        stored.push((
            String::from_utf8_lossy(k).into_owned(),
            heard
                .into_iter()
                .map(|(_, w)| w)
                .find(|w| w.starts_with("Value"))
                .unwrap_or_else(|| "no answer".into()),
        ));
    }
    println!("-- stored (read over a third connection) --");
    for (k, v) in &stored {
        println!("  {k}: {v}");
    }

    let tab2_told_published = log2.iter().any(|(_, w)| w.contains("w1: Published"));
    let tab2_busy = log2.iter().any(|(_, w)| w.contains("w1: Busy"));
    println!(
        "\nVERDICT: tab 2 was told w1 Busy: {tab2_busy}; tab 2 was told w1 Published: {tab2_told_published}{}",
        if tab2_busy && tab2_told_published { "  <- tab 1's Published reached tab 2's connection while tab 2's own w1 was refused" } else { "" }
    );
    // A STRANDED EFFECT IS LOST at the end of its call (sdk#150): any call
    // reporting one fails the probe, whatever else happened.
    let strands: Vec<&String> = log1
        .iter()
        .chain(log2.iter())
        .map(|(_, w)| w)
        .filter(|w| w.contains("stranded ") && !w.contains("stranded 0"))
        .collect();
    drop(node);
    if !strands.is_empty() {
        bail!(
            "{} call(s) stranded effects (run with SHOW_CALLS=1 to see them): {:?}",
            strands.len(),
            strands
        );
    }
    Ok(())
}
