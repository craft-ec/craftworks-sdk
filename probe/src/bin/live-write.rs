//! Acceptance 3: one write, end to end, on a live node.
//!
//! Registers the engine delegate, provisions it, sends ONE write, and watches
//! it reach `Published` — then checks the things a client actually cares
//! about: the head Register really holds the new `(seq, root)`, the value is
//! readable over a SECOND independent connection, and it is still readable
//! after the client closes and reopens.
//!
//! It spawns its OWN node, in ISOLATED NETWORK mode. Local mode cannot
//! exercise this at all: a delegate-originated PUT is silently dropped there
//! (measured), so a local run would show a write that is accepted and never
//! published, and would look like an engine bug.
//!
//! The signing key is generated HERE, per run, marked test in its type, and
//! removed at the end. It is never read from disk and is never a real key.

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use engine::State;
use engine_delegate::wire::{Reply, Request, TestKey};
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Every wait is bounded. A live test with an unbounded wait is one that
/// hangs instead of failing.
const STEP: Duration = Duration::from_secs(10);
/// The whole run. Stated, so "it passed" always comes with how long it had.
const BUDGET: Duration = Duration::from_secs(120);

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws)
        .await
        .context("connecting to the node")?;
    Ok(WebApi::start(stream))
}

async fn send(client: &mut WebApi, key: &DelegateKey, r: &Request) -> Result<()> {
    let payload = bincode::serialize(r)?;
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

/// Collect the delegate's replies until `want` is seen or the deadline passes.
async fn until(client: &mut WebApi, want: State, deadline: Instant) -> (bool, Vec<State>) {
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        let owned = m.payload.to_vec();
                        match bincode::deserialize::<Reply>(&owned) {
                            Ok(Reply::Write { state, .. }) => {
                                seen.push(state);
                                if state == want {
                                    return (true, seen);
                                }
                            }
                            Ok(Reply::Call {
                                saw,
                                ops,
                                stranded,
                                dropped,
                                awaiting,
                                read_back,
                                effects,
                                head_put,
                                head_update,
                                note,
                                ..
                            }) => println!(
                                "  call: saw {saw:?} effects {effects} ops {ops} awaiting {awaiting} read_back {read_back} stranded {stranded} dropped {dropped}{}{}",
                                if head_put + head_update == 0 {
                                    String::new()
                                } else {
                                    format!("  head: PUT {head_put} B / UPDATE {head_update} B")
                                },
                                if note.is_empty() { String::new() } else { format!("  | {note}") }
                            ),
                            _ => {}
                        }
                    }
                }
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    (false, seen)
}

/// Read a contract's state through this connection.
async fn get_state(client: &mut WebApi, id: ContractInstanceId) -> Option<Vec<u8>> {
    timeout(
        STEP,
        client.send(ClientRequest::ContractOp(
            freenet_stdlib::client_api::ContractRequest::Get {
                key: id,
                return_contract_code: false,
                subscribe: false,
                blocking_subscribe: false,
            },
        )),
    )
    .await
    .ok()?
    .ok()?;
    let deadline = Instant::now() + STEP;
    while Instant::now() < deadline {
        match timeout(STEP, client.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(
                freenet_stdlib::client_api::ContractResponse::GetResponse { state, .. },
            ))) => return Some(state.as_ref().to_vec()),
            Ok(Ok(_)) => continue,
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
        .unwrap_or(7716);
    let mut args = std::env::args().skip(1);
    let delegate_wasm = args.next().unwrap_or_else(|| {
        eprintln!("usage: live-write <engine_delegate.wasm> <block.wasm> <register.wasm>");
        std::process::exit(2);
    });
    let block_wasm = args.next().unwrap_or_default();
    let register_wasm = args.next().unwrap_or_default();

    let wasm = std::fs::read(&delegate_wasm).context("reading the engine delegate")?;
    // The same gate the build runs. A delegate whose imports the node does
    // not define fails to INSTANTIATE, and the symptom is a delegate that is
    // simply never called.
    probe::check(&wasm).map_err(|e| anyhow::anyhow!("the engine delegate is refused: {e}"))?;
    let block_code = std::fs::read(&block_wasm).context("reading the block contract")?;
    let register_code = std::fs::read(&register_wasm).context("reading the register contract")?;

    let dir = std::env::temp_dir().join(format!("live-write-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    println!(
        "node: isolated NETWORK mode on 127.0.0.1:{port} (a delegate PUT is dropped in local mode)"
    );
    let node = Node::spawn_in(
        port,
        &dir,
        Mode::IsolatedNetwork {
            network_port: port + 20000,
        },
    )?;
    let mut client = connect(&node.ws()).await?;

    let delegate = DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(Delegate::from((
        &DelegateCode::from(wasm.clone()),
        &Parameters::from(vec![]),
    ))));
    let dkey = delegate.key().clone();
    timeout(
        STEP,
        client.send(ClientRequest::DelegateOp(
            DelegateRequest::RegisterDelegate {
                delegate,
                cipher: [0u8; 32],
                nonce: [0u8; 24],
            },
        )),
    )
    .await
    .map_err(|_| anyhow::anyhow!("registering the delegate timed out"))??;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    println!("delegate: registered, key {dkey}");

    // A TEST key, generated for THIS run. Never read from disk, never real,
    // and removed with the node's tree when the run ends.
    let mut seed = [0u8; 32];
    getrandom(&mut seed);
    let sk = SigningKey::from_bytes(&seed);
    let vk = sk.verifying_key();
    let mut register_params = Vec::from(*b"RG01");
    register_params.push(0u8);
    register_params.extend_from_slice(&vk.to_bytes());
    register_params.extend_from_slice(b"head");
    println!("key: TEST signing key generated for this run only (never from disk)");

    let head = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(register_code.clone())),
        Parameters::from(register_params.clone()),
    )));
    let head_id = *head.key().id();
    println!("head:  Register {head_id}");

    send(
        &mut client,
        &dkey,
        &Request::Install {
            block_code,
            register_code,
            register_params,
            signing_key: TestKey(sk.to_bytes().to_vec()),
        },
    )
    .await?;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    send(&mut client, &dkey, &Request::Start { epochs: vec![1] }).await?;
    let _ = timeout(Duration::from_secs(3), client.recv()).await;

    // ---- ONE write ----
    let key = b"live/one".to_vec();
    let value = vec![0x5Au8; 200];
    let t = Instant::now();
    send(
        &mut client,
        &dkey,
        &Request::Write {
            client: 1,
            write_id: 1,
            ops: vec![(key.clone(), engine::Op::Put(value.clone()))],
        },
    )
    .await?;
    let (published, states) = until(&mut client, State::Published, deadline).await;
    let took = t.elapsed();
    println!("write: states {states:?}");
    if !published {
        bail!("the write did not reach Published within the budget: {states:?}");
    }
    println!("write: accepted -> published in {took:?}  (one run, one node, one machine)");

    // ---- a SECOND write, so the head bump can be compared ----
    //
    // The first bump has to CREATE the Register, and creating a contract
    // carries its code. Every bump after that updates a contract the node
    // already has. One write can only ever show the expensive route, and a
    // measurement of the cheap one is the point.
    let key2 = b"live/two".to_vec();
    let t2 = Instant::now();
    send(
        &mut client,
        &dkey,
        &Request::Write {
            client: 1,
            write_id: 2,
            ops: vec![(key2.clone(), engine::Op::Put(vec![0x5Bu8; 200]))],
        },
    )
    .await?;
    let (published2, states2) = until(&mut client, State::Published, deadline).await;
    if !published2 {
        bail!("the second write did not publish: {states2:?}");
    }
    println!(
        "write2: accepted -> published in {:?} (the head was UPDATED, not re-created)",
        t2.elapsed()
    );

    // ---- the head really moved ----
    let head_state = get_state(&mut client, head_id).await;
    match head_state
        .as_deref()
        .and_then(engine_delegate::register::head_of)
    {
        Some((seq, root)) => println!("head:  Register holds seq {seq}, root {}", hex8(&root)),
        None => bail!("the head Register does not hold a readable record after Published"),
    }

    // ---- the value, over a SECOND independent connection ----
    let mut second = connect(&node.ws()).await?;
    send(&mut second, &dkey, &Request::Start { epochs: vec![1] }).await?;
    let _ = timeout(Duration::from_secs(3), second.recv()).await;
    send(
        &mut second,
        &dkey,
        &Request::Get {
            client: 2,
            req_id: 1,
            key: key.clone(),
        },
    )
    .await?;
    let got = read_value(&mut second, deadline).await;
    match &got {
        Some(v) if *v == value => println!("read:  the value is readable over a SECOND connection"),
        Some(v) => bail!(
            "the second connection read {} B, not the {} B written",
            v.len(),
            value.len()
        ),
        None => bail!("the value was not readable over a second connection"),
    }

    // ---- the page-closed promise: Flush, close, reopen, read back ----
    send(&mut client, &dkey, &Request::Flush).await?;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    drop(client);
    drop(second);
    println!("client: Flush sent, both connections closed");

    let mut reopened = connect(&node.ws()).await?;
    send(&mut reopened, &dkey, &Request::Start { epochs: vec![1] }).await?;
    let _ = timeout(Duration::from_secs(3), reopened.recv()).await;
    send(
        &mut reopened,
        &dkey,
        &Request::Get {
            client: 3,
            req_id: 1,
            key,
        },
    )
    .await?;
    match read_value(&mut reopened, deadline).await {
        Some(v) if v == value => println!("read:  still readable after closing and reopening"),
        other => bail!("after reopening, the value read back as {other:?}"),
    }

    println!("budget: {:?} of {BUDGET:?} used", started.elapsed());
    println!("done");
    Ok(())
}

async fn read_value(client: &mut WebApi, deadline: Instant) -> Option<Vec<u8>> {
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        let owned = m.payload.to_vec();
                        match bincode::deserialize::<Reply>(&owned) {
                            Ok(Reply::Read { result, .. }) => {
                                if let engine::read::ReadResult::Value(v) = result {
                                    return v;
                                }
                                println!("  read: answered {result:?}");
                                return None;
                            }
                            Ok(Reply::Call {
                                saw, effects, ops, stranded, dropped, ..
                            }) => println!(
                                "  call: saw {saw:?} effects {effects} ops {ops} stranded {stranded} dropped {dropped}"
                            ),
                            _ => {}
                        }
                    }
                }
            }
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
    None
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

/// A test key needs randomness, not a crate.
fn getrandom(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5eed)
        ^ (std::process::id() as u64) << 32;
    for b in buf.iter_mut() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *b = (s & 0xff) as u8;
    }
}
