//! WHAT HAPPENS TO A BURST OF WRITES, at the delegate boundary.
//!
//! `live-write` sends ONE write and it publishes in about 180 ms. A page
//! sending twenty in ten milliseconds gets ten published and twelve that are
//! never answered at all — submitted, awaiting a verdict that does not come,
//! and rolled back a minute later with their rows vanishing from the screen.
//!
//! This counts both directions at the delegate boundary: `Request::Write`
//! frames sent, and every reply that comes back, per write id. The shell's
//! own `Reply::Call` carries `stranded` — *effects still queued at the end of
//! the call, and therefore LOST; must be zero* — which would name the fault
//! outright.
//!
//! NO BROWSER and no DOM. The page was already eliminated: the identical
//! numbers appear from a burst of direct `db.put` calls with the DOM entirely
//! out of the picture.
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
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse, WebApi};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use protocol::WriteState as State;
use protocol::{Reply, Request, TestKey};
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
    let payload = protocol::encode_request(protocol::CURRENT, r);
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
                        match protocol::decode_reply(&owned) {
                            Ok(Reply::WriteState { state, .. }) => {
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
    let _deadline = started + BUDGET;
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

    let (delegate, dkey) = wire::delegate_from_code(&wasm);
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
    // From `wire`, not laid out here: the contract INSTANCE is derived from
    // these bytes, so a second copy of the layout is a silent fork of an id.
    let register_params = wire::register_params(&vk.to_bytes(), wire::HEAD_NAME);
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
    send(&mut client, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), client.recv()).await;

    // ---- THE BURST ----
    let n: u64 = std::env::var("BURST")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    println!("burst: {n} writes, sent without waiting for any verdict");

    let t = Instant::now();
    for i in 1..=n {
        send(
            &mut client,
            &dkey,
            &Request::Write {
                write_id: i,
                ops: vec![protocol::Op::Put(
                    format!("burst/{i:04}").into_bytes(),
                    vec![0x5Au8; 200],
                )],
            },
        )
        .await?;
    }
    println!("burst: all {n} frames sent in {:?}", t.elapsed());

    // ---- WHAT CAME BACK ----
    //
    // Counted per write id, so "answered" is a fact about each write rather
    // than a total that a few noisy ones could satisfy.
    let mut verdicts: std::collections::BTreeMap<u64, Vec<State>> = Default::default();
    let mut calls = 0u32;
    let mut stranded_total = 0u32;
    let mut dropped_total = 0u32;
    let mut ops_total = 0u32;
    let watch = Instant::now() + Duration::from_secs(45);
    while Instant::now() < watch {
        let left = watch.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        let owned = m.payload.to_vec();
                        match protocol::decode_reply(&owned) {
                            Ok(Reply::WriteState { write_id, state }) => {
                                verdicts.entry(write_id).or_default().push(state);
                            }
                            Ok(Reply::Call {
                                ops,
                                stranded,
                                dropped,
                                ..
                            }) => {
                                calls += 1;
                                ops_total += ops;
                                stranded_total += stranded;
                                dropped_total += dropped;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
        // Everything answered terminally? Then stop early rather than
        // spending the whole window.
        let done = (1..=n).all(|i| {
            verdicts.get(&i).is_some_and(|s| {
                s.iter().any(|x| {
                    matches!(
                        x,
                        State::Published
                            | State::ParityComplete
                            | State::Failed
                            | State::Lost
                            | State::Busy
                    )
                })
            })
        });
        if done {
            break;
        }
    }

    // ---- THE COUNTERS MUST HAVE MOVED ----
    //
    // A counter that was never incremented reads exactly like a true zero,
    // and that has pointed an investigation the wrong way three times now. So
    // before any zero here is believed, the ones that MUST be non-zero are
    // checked.
    if calls == 0 {
        bail!("no Reply::Call arrived at all — this probe counted nothing, so every zero below is meaningless");
    }
    if ops_total == 0 {
        bail!("Reply::Call reported {calls} call(s) and ZERO ops across all of them; the counter is not moving, so `stranded 0` cannot be trusted either");
    }
    println!("calls: {calls}, ops {ops_total} (both non-zero, so the zeros below mean something)");

    let mut answered = 0u64;
    let mut unanswered = Vec::new();
    for i in 1..=n {
        match verdicts.get(&i) {
            Some(states) => {
                answered += 1;
                if i <= 3 || states.len() > 1 {
                    println!("  write {i}: {states:?}");
                }
            }
            None => unanswered.push(i),
        }
    }

    println!("\nverdicts: {answered} of {n} writes were answered at all");
    println!("stranded: {stranded_total}   dropped: {dropped_total}");
    if !unanswered.is_empty() {
        println!(
            "UNANSWERED: {} write(s) crossed the socket and never got a WriteState: {:?}",
            unanswered.len(),
            unanswered
        );
    }
    let published = (1..=n)
        .filter(|i| {
            verdicts.get(i).is_some_and(|s| {
                s.iter()
                    .any(|x| matches!(x, State::Published | State::ParityComplete))
            })
        })
        .count();
    let busy = (1..=n)
        .filter(|i| verdicts.get(i).is_some_and(|s| s.contains(&State::Busy)))
        .count();
    println!(
        "published: {published}   busy: {busy}   never answered: {}",
        unanswered.len()
    );
    println!("budget: {:?} of {:?} used", started.elapsed(), BUDGET);
    Ok(())
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
