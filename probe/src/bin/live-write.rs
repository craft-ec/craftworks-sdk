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

    // ---- ONE write ----
    let key = b"live/one".to_vec();
    let value = vec![0x5Au8; 200];
    let t = Instant::now();
    send(
        &mut client,
        &dkey,
        &Request::Write {
            write_id: 1,
            ops: vec![protocol::Op::Put(key.clone(), value.clone())],
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
            write_id: 2,
            ops: vec![protocol::Op::Put(key2.clone(), vec![0x5Bu8; 200])],
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
    send(&mut second, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), second.recv()).await;
    send(
        &mut second,
        &dkey,
        &Request::Get {
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

    // ---- a LIST over the new connection, as a table component does ----
    //
    // Two writes are in the tree by now, so a range must return both, in
    // order, over a connection that did not make them.
    send(
        &mut second,
        &dkey,
        &Request::Range {
            req_id: 2,
            lo: protocol::Bound::Unbounded,
            hi: protocol::Bound::Unbounded,
            reverse: false,
            after: None,
            max_entries: 100_000,
        },
    )
    .await?;
    match read_page(&mut second, deadline).await {
        Some((rows, used)) => {
            let keys: Vec<String> = rows
                .iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .collect();
            if keys.len() < 2 {
                bail!("a list over the new connection returned {keys:?}, not both writes");
            }
            println!("  list:  {keys:?} over the SECOND connection (page size used {used})");
            if used >= 100_000 {
                bail!("the engine reported {used} as the page size it used, so nothing clamped");
            }
        }
        None => bail!("a range over the new connection returned no page at all"),
    }

    // ---- SLICE 6: a subscription, and where a push actually lands ----
    //
    // The second connection asks to be told when `live/` changes, then the
    // FIRST writes. What is being established is not only "a notification
    // exists" but WHICH CONNECTION it reaches — a delegate's output goes back
    // to whoever invoked it, and whether a push can reach a connection that
    // did not is a property of the platform, not of this engine. It is
    // measured here rather than assumed, and reported either way.
    send(
        &mut second,
        &dkey,
        &Request::SubscribeRange {
            sub_id: 7,
            lo: protocol::Bound::Included(b"live/".to_vec()),
            hi: protocol::Bound::Excluded(b"live0".to_vec()),
        },
    )
    .await?;
    match read_subscribed(&mut second, deadline).await {
        Some(protocol::Accepted::Yes) => {
            println!("sub:   the engine took a subscription over the SECOND connection")
        }
        Some(other) => bail!("the engine refused the subscription: {other:?}"),
        None => {
            bail!("the subscribe was never answered, so a client cannot tell subscribed from lost")
        }
    }

    // The first connection writes into the subscribed range.
    let third_key = b"live/three".to_vec();
    send(
        &mut client,
        &dkey,
        &Request::Write {
            write_id: 3,
            ops: vec![protocol::Op::Put(third_key.clone(), vec![0x33u8; 64])],
        },
    )
    .await?;
    let on_writer = drain_changed(&mut client, Instant::now() + STEP).await;

    // And the second connection, which asked for it, turns over WITHOUT
    // asking for the data — a Tick is the cheapest message that is not a
    // request for anything.
    send(&mut second, &dkey, &Request::Tick { now: 1 }).await?;
    let on_watcher = drain_changed(&mut second, Instant::now() + STEP).await;

    println!(
        "sub:   Changed seen on the WRITER's connection: {on_writer:?}; on the WATCHER's: {on_watcher:?}"
    );
    if on_writer.is_empty() && on_watcher.is_empty() {
        bail!(
            "a commit landed inside a subscribed range and NO connection saw a \
             Changed at all — the notification path is not wired"
        );
    }
    // Whichever connection it landed on, the fact under test is that the
    // engine produced exactly one per commit and named the range's own
    // subscription.
    let all: Vec<u64> = on_writer.iter().chain(on_watcher.iter()).copied().collect();
    if all.iter().any(|s| *s != 7) {
        bail!("a Changed named a subscription nobody took: {all:?}");
    }
    if all.len() != 1 {
        bail!("one commit produced {} notifications: {all:?}", all.len());
    }
    if on_watcher.is_empty() {
        println!(
            "  NOTE: the push reached the WRITER's connection, not the watcher's. A \
             delegate's output returns to whoever invoked it, so a notification is \
             delivered on whatever exchange is in flight — which is why a live \
             binding's correctness rests on its backstop and not on being told."
        );
    }

    // The BACKSTOP, which is what makes a binding correct either way: the
    // watcher re-reads the head and finds the newer root.
    send(&mut second, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), second.recv()).await;
    send(
        &mut second,
        &dkey,
        &Request::Get {
            req_id: 9,
            key: third_key.clone(),
        },
    )
    .await?;
    match read_value(&mut second, deadline).await {
        Some(v) if v.len() == 64 => {
            println!("back:  the watcher read the new row on its own re-read (the backstop)")
        }
        other => bail!("the backstop did not find the write: {other:?}"),
    }

    // ---- SLICE 6b: the CLIENT subscription to the head contract ----
    //
    // #45 established that an engine-originated push returns to whoever
    // invoked the delegate and never reaches a watcher (F40). The platform's
    // own push path is this one: a CLIENT subscription to a contract receives
    // UpdateNotifications. The head Register IS the tree — its value is
    // (seq, root) — so "the head moved" is "the tree moved".
    //
    // The watcher's tick is DISABLED for this section: it sends nothing, asks
    // for nothing, and must still be told.
    //
    // THE CONTROL FIRST, and it costs nothing because the state it needs
    // already exists: write 3 landed a moment ago while the watcher held NO
    // client subscription, and nothing was pushed to it. Confirm that
    // explicitly rather than inferring it from the `Changed` section — an
    // assertion that the notifier works is worth nothing beside a connection
    // that would have been told anyway.
    match wait_head_update(&mut second, Instant::now() + Duration::from_secs(3)).await {
        None => println!(
            "head:  CONTROL — with no client subscription the watcher was pushed \
             NOTHING for a commit that had already landed"
        ),
        Some(n) => bail!(
            "the watcher was pushed {n} update(s) WITHOUT holding a client \
             subscription, so the notifier below is not what is doing the work"
        ),
    }

    timeout(
        STEP,
        second.send(ClientRequest::ContractOp(
            freenet_stdlib::client_api::ContractRequest::Subscribe {
                key: head_id,
                summary: None,
            },
        )),
    )
    .await
    .map_err(|_| anyhow::anyhow!("subscribing to the head contract timed out"))??;
    match wait_subscribed(&mut second, Instant::now() + Duration::from_secs(5)).await {
        true => println!("head:  the WATCHER holds a client subscription to the head contract"),
        false => bail!(
            "the node did not confirm a client subscription to the head \
             contract, so anything below would be measuring the backstop"
        ),
    }

    // A writes. The watcher sends NOTHING.
    let fourth_key = b"live/four".to_vec();
    send(
        &mut client,
        &dkey,
        &Request::Write {
            write_id: 4,
            ops: vec![protocol::Op::Put(fourth_key.clone(), vec![0x44u8; 64])],
        },
    )
    .await?;

    let pushed = wait_head_update(&mut second, Instant::now() + Duration::from_secs(20)).await;
    match pushed {
        Some(n) => println!(
            "head:  the WATCHER was pushed {n} head update(s) WITHOUT ticking — \
             this is the notifier a second tab runs on"
        ),
        None => bail!(
            "the watcher holds a client subscription to the head contract and \
             a commit moved it, and nothing was pushed within 20s. The live \
             notifier does not work; a binding would be on its tick alone."
        ),
    }

    // ---- the page-closed promise: Flush, close, reopen, read back ----
    send(&mut client, &dkey, &Request::Flush).await?;
    let _ = timeout(Duration::from_secs(2), client.recv()).await;
    drop(client);
    drop(second);
    println!("client: Flush sent, both connections closed");

    let mut reopened = connect(&node.ws()).await?;
    send(&mut reopened, &dkey, &Request::Identity).await?;
    let _ = timeout(Duration::from_secs(3), reopened.recv()).await;
    send(&mut reopened, &dkey, &Request::Get { req_id: 1, key }).await?;
    match read_value(&mut reopened, deadline).await {
        Some(v) if v == value => println!("read:  still readable after closing and reopening"),
        other => bail!("after reopening, the value read back as {other:?}"),
    }

    println!("budget: {:?} of {BUDGET:?} used", started.elapsed());
    println!("done");
    Ok(())
}

/// Did the node confirm a CLIENT subscription?
async fn wait_subscribed(client: &mut WebApi, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(
                freenet_stdlib::client_api::ContractResponse::SubscribeResponse { .. },
            ))) => return true,
            Ok(Ok(_)) => continue,
            _ => return false,
        }
    }
    false
}

/// Wait for the node to PUSH an update for a subscribed contract.
///
/// Nothing is sent on this connection while waiting. That is the whole point:
/// a push that only arrives because the watcher asked for something is not a
/// push, it is a reply, and #45 showed the difference matters.
async fn wait_head_update(client: &mut WebApi, deadline: Instant) -> Option<usize> {
    let mut n = 0;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(Duration::from_secs(2)), client.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(
                freenet_stdlib::client_api::ContractResponse::UpdateNotification { key, .. },
            ))) => {
                n += 1;
                println!("  pushed: UpdateNotification for {key}");
                return Some(n);
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => {
                println!("  pushed: connection error {e}");
                return None;
            }
            Err(_) => continue,
        }
    }
    None
}

/// Whether a subscribe was taken.
async fn read_subscribed(client: &mut WebApi, deadline: Instant) -> Option<protocol::Accepted> {
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        if let Ok(Reply::Subscribed { accepted, .. }) =
                            protocol::decode_reply(&m.payload.to_vec())
                        {
                            return Some(accepted);
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

/// Every `Changed` this connection has been handed, until the deadline.
///
/// Drains rather than waits for one: a push arrives on whatever exchange is
/// in flight, so "did anything arrive here" is the question, and a function
/// that returned on the first one could not answer "how many".
async fn drain_changed(client: &mut WebApi, deadline: Instant) -> Vec<u64> {
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(Duration::from_millis(400)), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        if let Ok(Reply::Changed { sub_id, why, .. }) =
                            protocol::decode_reply(&m.payload.to_vec())
                        {
                            println!("  changed: sub {sub_id} why {why:?}");
                            out.push(sub_id);
                        }
                    }
                }
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    out
}

/// One page of a range, and the page size the engine actually used.
async fn read_page(
    client: &mut WebApi,
    deadline: Instant,
) -> Option<(Vec<(Vec<u8>, Vec<u8>)>, u32)> {
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        let owned = m.payload.to_vec();
                        match protocol::decode_reply(&owned) {
                            Ok(Reply::Page {
                                entries,
                                max_entries,
                                ..
                            }) => return Some((entries, max_entries)),
                            Ok(Reply::Unavailable { .. }) => return None,
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

async fn read_value(client: &mut WebApi, deadline: Instant) -> Option<Vec<u8>> {
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left.min(STEP), client.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        let owned = m.payload.to_vec();
                        match protocol::decode_reply(&owned) {
                            Ok(Reply::Value { value, .. }) => return value,
                            Ok(Reply::Unavailable { blocked_on, .. }) => {
                                println!("  read: Unavailable({})", hex8(&blocked_on));
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
