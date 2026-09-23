//! sdk#209, LIVE: the signer on a private node.
//!
//!  1. provision; the page PUTs the root blocks the heads will name (client PUT, Block code riding);
//!  2. THE RACE over two client connections: the same prev, two different nexts, sent back to back. Exactly ONE is
//!     `Signed`; the other is `AlreadySigned` with the winner's bytes -- told BEFORE any UPDATE leaves the page;
//!  3. the page PUTs the Register with the winner's record; signs on from it (the record alone is truth);
//!  4. IS A PAGE-UPDATEd REGISTER VISIBLE TO THE SIGNER'S SYNC READ? The page signs a head AHEAD (seq 5) with the
//!     test key itself and UPDATEs the Register; then asks the signer to sign on from seq 5. `Signed` = visible;
//!     `NotNext{current: seq 2}` = not visible (the signer only knows its own record);
//!  5. IS THE RECORD DURABLE BEFORE THE REPLY? A `Signed` arrives, and the node is SIGKILLed at once and restarted on
//!     the same data; the SAME prev with a DIFFERENT next is asked again. `AlreadySigned(first)` = durable;
//!     `Signed` = the record was lost with the process (two signatures at one seq: the fork the signer exists to stop);
//!  6. PUT-WITH-CODE: the page hands the signer two block states (no code); the signer names each block's contract
//!     (`Putting`) and PUTs it with the Block code it holds. Each named contract must match the one the page derives,
//!     and READ-LOCAL (`Held`) must say both present and a never-put block absent. (a) on the isolated node a client
//!     GET serves each; (b) on a PEERED node -- B joined to a private gateway A -- a client GET is refused (F55),
//!     which is why the page confirms through `Held`. `Put` answers reaching the page are counted, not assumed.
//!
//! Exit status is the verdict. usage: SIGNER_PORT=<port> live-signer <signer.wasm> <block.wasm> <register.wasm>

use anyhow::{bail, Context, Result};
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, DelegateRequest, HostResponse, WebApi,
};
use freenet_stdlib::prelude::*;
use probe::node::{Mode, Node, TempTree};
use signer::{Answer, Head, Next, Request};
use std::time::Duration;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(20);

async fn connect(ws: &str) -> Result<WebApi> {
    let (stream, _) = tokio_tungstenite::connect_async(ws)
        .await
        .context("connecting")?;
    Ok(WebApi::start(stream))
}

async fn register_delegate(c: &mut WebApi, wasm: &[u8]) -> Result<DelegateKey> {
    let (delegate, key) = wire::delegate_from_code(wasm);
    timeout(
        STEP,
        c.send(ClientRequest::DelegateOp(
            DelegateRequest::RegisterDelegate {
                delegate,
                cipher: [0u8; 32],
                nonce: [0u8; 24],
            },
        )),
    )
    .await
    .map_err(|_| anyhow::anyhow!("registering timed out"))??;
    let _ = timeout(Duration::from_secs(3), c.recv()).await;
    Ok(key)
}

/// Every signer request of this run gets its own id (SG02), from 1: `0` is UNATTRIBUTED.
static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Send `r` under a fresh id, and return the id its answer will carry.
async fn send_signer(c: &mut WebApi, key: &DelegateKey, r: &Request) -> Result<u32> {
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    timeout(
        STEP,
        c.send(ClientRequest::DelegateOp(
            DelegateRequest::ApplicationMessages {
                key: key.clone(),
                params: vec![].into(),
                inbound: vec![InboundDelegateMsg::ApplicationMessage(
                    ApplicationMessage::new(signer::encode_request(id, r)),
                )],
            },
        )),
    )
    .await
    .map_err(|_| anyhow::anyhow!("send blocked"))??;
    Ok(id)
}

/// The answer to request `id`, attributed by its id alone. Answers under any other id -- a `Put` (UNATTRIBUTED), or
/// another request's -- are counted into `others`, never taken for this one.
async fn answer_to(c: &mut WebApi, id: u32, others: &mut Vec<(u32, Answer)>) -> Result<Answer> {
    let end = tokio::time::Instant::now() + STEP;
    while tokio::time::Instant::now() < end {
        match timeout(Duration::from_millis(500), c.recv()).await {
            Ok(Ok(HostResponse::DelegateResponse { values, .. })) => {
                for v in values {
                    if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                        match signer::decode_answer(&m.payload) {
                            Some((got, a)) if got == id => return Ok(a),
                            Some(other) => others.push(other),
                            None => {}
                        }
                    }
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => bail!("node error: {e}"),
            Err(_) => {}
        }
    }
    bail!("no answer to signer request {id} within {STEP:?}")
}

/// Ask, and take the answer to THIS request; an answer to any other is a finding, not this one's.
async fn ask(c: &mut WebApi, key: &DelegateKey, r: &Request) -> Result<Answer> {
    let id = send_signer(c, key, r).await?;
    let mut others = Vec::new();
    let a = answer_to(c, id, &mut others).await?;
    if others.iter().any(|(i, _)| *i != signer::UNATTRIBUTED) {
        bail!("answers to requests not in flight arrived before request {id}'s: {others:?}");
    }
    Ok(a)
}

fn container(code: &[u8], params: &[u8]) -> ContractContainer {
    ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(code.to_vec())),
        Parameters::from(params.to_vec()),
    )))
}

use core_types::hex::encode as hex;

/// A client GET's outcome, kept whole: a NotFound is a finding here (F55), not an error to bail on.
#[derive(Debug)]
#[allow(dead_code)] // the fields are read through `Debug`, in the red lines and the control's report
enum Got {
    Served(Vec<u8>),
    /// `ContractResponse::NotFound`: an ANSWER, not a node error -- what F55 looks like to a client.
    NotFound,
    NodeError(String),
    /// No answer to the GET within STEP; what else arrived meanwhile, by name.
    Silent(Vec<String>),
}

async fn get(c: &mut WebApi, id: ContractInstanceId) -> Result<Got> {
    timeout(
        STEP,
        c.send(ClientRequest::ContractOp(ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        })),
    )
    .await
    .map_err(|_| anyhow::anyhow!("send blocked"))??;
    let mut seen = Vec::new();
    let end = tokio::time::Instant::now() + STEP;
    while tokio::time::Instant::now() < end {
        match timeout(Duration::from_millis(500), c.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => return Ok(Got::Served(state.as_ref().to_vec())),
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::NotFound { .. }))) => {
                return Ok(Got::NotFound)
            }
            Ok(Ok(other)) => seen.push(other.to_string()),
            Ok(Err(e)) => return Ok(Got::NodeError(e.to_string())),
            Err(_) => {}
        }
    }
    Ok(Got::Silent(seen))
}

/// Ask `Held` about `contracts` once; the `Put` answers that arrive first are counted, not mistaken for it.
async fn held_once(
    c: &mut WebApi,
    key: &DelegateKey,
    contracts: &[[u8; 32]],
    puts: &mut usize,
) -> Result<Vec<bool>> {
    let id = send_signer(
        c,
        key,
        &Request::Held {
            contracts: contracts.to_vec(),
        },
    )
    .await?;
    let mut others = Vec::new();
    let a = answer_to(c, id, &mut others).await?;
    for (i, o) in others {
        match o {
            Answer::Put { .. } if i == signer::UNATTRIBUTED => *puts += 1,
            other => bail!("request {id} (Held) heard an answer to request {i}: {other:?}"),
        }
    }
    match a {
        Answer::Held { present } => Ok(present),
        other => bail!("Held answered {other:?}"),
    }
}

/// Step 6: PutBlocks two blocks; the signer must name them as the page does; `Held` must say present for both and
/// absent for a never-put block (polled until the PUTs land, bounded by STEP); then a client GET of each, which the
/// isolated node serves and a PEERED node answers NotFound (F55) -- the expectation is `peered`'s, and a mismatch is red.
async fn put_with_code(
    c: &mut WebApi,
    key: &DelegateKey,
    bcode: &[u8],
    label: &str,
    first: u8,
    peered: bool,
    red: &mut Vec<String>,
) -> Result<()> {
    let fresh: Vec<([u8; 32], Vec<u8>)> = (first..first + 2).map(block).collect();
    let (never, _) = block(first + 2);
    let putting = ask(
        c,
        key,
        &Request::PutBlocks {
            states: fresh.iter().map(|(_, st)| st.clone()).collect(),
        },
    )
    .await?;
    let expect: Vec<[u8; 32]> = fresh
        .iter()
        .map(|(id, _)| contract_keys::block::contract_for(bcode, id))
        .collect();
    match &putting {
        Answer::Putting { contracts } if *contracts == expect => {
            println!("{label}: the signer named both block contracts as the page derives them")
        }
        other => red.push(format!(
            "{label}: expected Putting{{{expect:?}}}, told {other:?}"
        )),
    }
    let mut ask_about = expect.clone();
    ask_about.push(contract_keys::block::contract_for(bcode, &never));
    let mut puts = 0usize;
    let t = tokio::time::Instant::now();
    let mut present = held_once(c, key, &ask_about, &mut puts).await?;
    while present != [true, true, false] && t.elapsed() < STEP {
        tokio::time::sleep(Duration::from_millis(250)).await;
        present = held_once(c, key, &ask_about, &mut puts).await?;
    }
    println!(
        "{label}: READ-LOCAL Held [block {}, block {}, never-put {}] = {present:?} after {} ms",
        first,
        first + 1,
        first + 2,
        t.elapsed().as_millis()
    );
    if present != [true, true, false] {
        red.push(format!(
            "{label}: Held said {present:?}, not [true, true, false]"
        ));
    }
    puts += drain(c, Duration::from_secs(3))
        .await
        .iter()
        .filter(|a| matches!(a, Answer::Put { .. }))
        .count();
    println!("{label}: {puts} Put answer(s) reached the asking connection");
    for (i, (id, st)) in fresh.iter().enumerate() {
        let n = first as usize + i;
        let got = get(c, container(bcode, id).key().id().to_owned()).await?;
        let served = matches!(&got, Got::Served(b) if b == st);
        match (&got, peered) {
            (Got::Served(_), false) if served => println!("{label}: block {n}: a client GET serves it"),
            (Got::NotFound, true) => {
                println!("{label}: block {n}: a client GET is answered NotFound (F55)")
            }
            (g, true) if served => red.push(format!(
                "{label}: block {n}: a client GET SERVED it on the peered node ({g:?}): F55 did not reproduce -- is B peered? the case this step exists for is not covered"
            )),
            (g, _) => red.push(format!("{label}: block {n}: a client GET gave {g:?}")),
        }
    }
    match get(c, container(bcode, &never).key().id().to_owned()).await? {
        Got::Served(b) => red.push(format!(
            "{label}: CONTROL: a never-put block was served ({} B): the GET check cannot fail",
            b.len()
        )),
        g => println!("{label}: CONTROL: a never-put block is not served ({g:?})"),
    }
    Ok(())
}

/// Every signer answer that arrives on `c` within `window`.
async fn drain(c: &mut WebApi, window: Duration) -> Vec<Answer> {
    let mut got = Vec::new();
    let end = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < end {
        if let Ok(Ok(HostResponse::DelegateResponse { values, .. })) =
            timeout(Duration::from_millis(250), c.recv()).await
        {
            for v in values {
                if let OutboundDelegateMsg::ApplicationMessage(m) = v {
                    if let Some((_, a)) = signer::decode_answer(&m.payload) {
                        got.push(a);
                    }
                }
            }
        }
    }
    got
}

async fn contract_op(c: &mut WebApi, r: ContractRequest<'static>) -> Result<String> {
    timeout(STEP, c.send(ClientRequest::ContractOp(r)))
        .await
        .map_err(|_| anyhow::anyhow!("send blocked"))??;
    let end = tokio::time::Instant::now() + STEP;
    while tokio::time::Instant::now() < end {
        match timeout(Duration::from_millis(500), c.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { .. }))) => {
                return Ok("put".into())
            }
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateResponse { .. }))) => {
                return Ok("update".into())
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => bail!("node error: {e}"),
            Err(_) => {}
        }
    }
    bail!("no answer to a contract op within {STEP:?}")
}

/// A raw block the page PUTs, and its id (= the root a head names).
fn block(n: u8) -> ([u8; 32], Vec<u8>) {
    let body = vec![n; 700];
    let id = freenet_prolly::block_id(freenet_prolly::kind::RAW, &body);
    let mut st = vec![freenet_prolly::kind::RAW];
    st.extend_from_slice(&body);
    (id, st)
}

fn signed_bytes(a: &Answer) -> Option<Vec<u8>> {
    match a {
        Answer::Signed(b) | Answer::AlreadySigned(b) => Some(b.clone()),
        _ => None,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let v = std::process::Command::new("freenet")
        .arg("--version")
        .output()
        .context("freenet --version")?;
    println!(
        "freenet: {}",
        String::from_utf8_lossy(&v.stdout)
            .lines()
            .next()
            .unwrap_or_default()
    );
    let port: u16 = std::env::var("SIGNER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .context("SIGNER_PORT=<port> is required; there is no default")?;
    let mut a = std::env::args().skip(1);
    let usage = "usage: live-signer <signer.wasm> <block.wasm> <register.wasm>";
    let wasm = std::fs::read(a.next().context(usage)?)?;
    probe::check(&wasm)
        .map_err(|e| anyhow::anyhow!("the signer is refused by the import gate: {e}"))?;
    let bcode = std::fs::read(a.next().context(usage)?)?;
    let rcode = std::fs::read(a.next().context(usage)?)?;

    let dir = std::env::temp_dir().join(format!("live-signer-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    let mut node = Node::spawn_in(
        port,
        &dir,
        Mode::IsolatedNetwork {
            network_port: port + 1,
        },
    )?;
    let mut c1 = connect(&node.ws()).await?;
    let key = register_delegate(&mut c1, &wasm).await?;

    let mut seed = [0u8; 32];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    }
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
    let p = ask(
        &mut c1,
        &key,
        &Request::Provision {
            signing_key: sk.to_bytes().to_vec(),
            register_code: rcode.clone(),
            register_params: params.clone(),
            block_code: bcode.clone(),
        },
    )
    .await?;
    println!("provision: {p:?}");
    if p != Answer::Provisioned {
        bail!("not provisioned: {p:?}");
    }

    let roots: Vec<[u8; 32]> = (1..=8u8).map(|n| block(n).0).collect();
    for n in 1..=8u8 {
        let (id, st) = block(n);
        contract_op(
            &mut c1,
            ContractRequest::Put {
                contract: container(&bcode, &id),
                state: WrappedState::new(st),
                related_contracts: RelatedContracts::default(),
                subscribe: false,
                blocking_subscribe: false,
            },
        )
        .await
        .with_context(|| format!("putting root block {n}"))?;
    }
    println!("page: put 8 root blocks (client PUT)");
    let r = |n: usize| roots[n - 1];
    let genesis = Head {
        seq: 0,
        root: [0u8; 32],
    };
    let mut red: Vec<String> = Vec::new();

    // 2. THE RACE.
    let mut c2 = connect(&node.ws()).await?;
    let ra = Request::Sign {
        prev: genesis,
        next: Next {
            seq: 1,
            root: r(1),
            ledger: vec![],
        },
    };
    let rb = Request::Sign {
        prev: genesis,
        next: Next {
            seq: 1,
            root: r(2),
            ledger: vec![],
        },
    };
    let ia = send_signer(&mut c1, &key, &ra).await?;
    let ib = send_signer(&mut c2, &key, &rb).await?;
    let (mut oa, mut ob) = (Vec::new(), Vec::new());
    let (x, y) = tokio::join!(
        answer_to(&mut c1, ia, &mut oa),
        answer_to(&mut c2, ib, &mut ob)
    );
    let (x, y) = (x?, y?);
    let kinds = |a: &Answer| match a {
        Answer::Signed(_) => "Signed",
        Answer::AlreadySigned(_) => "AlreadySigned",
        _ => "other",
    };
    println!(
        "race: connection 1 told {}, connection 2 told {}",
        kinds(&x),
        kinds(&y)
    );
    let winner = match (&x, &y) {
        (Answer::Signed(w), Answer::AlreadySigned(l))
        | (Answer::AlreadySigned(l), Answer::Signed(w))
            if w == l =>
        {
            w.clone()
        }
        _ => {
            red.push(format!(
                "RACE: not one Signed + one AlreadySigned(same bytes): {x:?} / {y:?}"
            ));
            signed_bytes(&x).unwrap_or_default()
        }
    };
    let (wseq, wroot) = contract_keys::register::head_of(&winner)
        .context("the winner's record does not parse")?;
    println!(
        "race: the winner is seq {wseq}, root #{}",
        if wroot == r(1) { 1 } else { 2 }
    );

    // 3. The page PUTs the Register with the winner; signs on from it.
    contract_op(
        &mut c1,
        ContractRequest::Put {
            contract: container(&rcode, &params),
            state: WrappedState::new(winner.clone()),
            related_contracts: RelatedContracts::default(),
            subscribe: false,
            blocking_subscribe: false,
        },
    )
    .await
    .context("putting the Register")?;
    let s2 = ask(
        &mut c1,
        &key,
        &Request::Sign {
            prev: Head {
                seq: 1,
                root: wroot,
            },
            next: Next {
                seq: 2,
                root: r(3),
                ledger: vec![],
            },
        },
    )
    .await?;
    println!("sign on from the landed head: {}", kinds(&s2));
    if !matches!(s2, Answer::Signed(_)) {
        red.push(format!("sign on from seq 1: {s2:?}"));
    }

    // 4. VISIBILITY of a page-UPDATEd Register to the sync read.
    let ahead = contract_keys::register::head_state(&params, &sk.to_bytes(), 5, &r(5))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let rkey = container(&rcode, &params).key();
    contract_op(
        &mut c1,
        ContractRequest::Update {
            key: rkey,
            data: UpdateData::State(State::from(ahead)),
        },
    )
    .await
    .context("the page's UPDATE of the Register")?;
    let s5 = ask(
        &mut c1,
        &key,
        &Request::Sign {
            prev: Head { seq: 5, root: r(5) },
            next: Next {
                seq: 6,
                root: r(6),
                ledger: vec![],
            },
        },
    )
    .await?;
    let visible = matches!(s5, Answer::Signed(_));
    println!("VISIBILITY: after the page UPDATEd the Register to seq 5, signing on from seq 5 was told {}{}", kinds(&s5),
        match &s5 { Answer::NotNext { current } => format!(" (current seq {})", current.seq), Answer::Refused(w) => format!(" ({w:?})"), _ => String::new() });
    println!(
        "VISIBILITY: a page-UPDATEd Register {} to the signer's sync read",
        if visible {
            "IS visible"
        } else {
            "is NOT visible"
        }
    );

    // 5. DURABILITY: sign, SIGKILL at once, restart, re-ask the same prev with a different next.
    let (prev_d, s_first) = if visible {
        (
            Head { seq: 6, root: r(6) },
            ask(
                &mut c1,
                &key,
                &Request::Sign {
                    prev: Head { seq: 6, root: r(6) },
                    next: Next {
                        seq: 7,
                        root: r(7),
                        ledger: vec![],
                    },
                },
            )
            .await?,
        )
    } else {
        (
            Head { seq: 2, root: r(3) },
            ask(
                &mut c1,
                &key,
                &Request::Sign {
                    prev: Head { seq: 2, root: r(3) },
                    next: Next {
                        seq: 3,
                        root: r(7),
                        ledger: vec![],
                    },
                },
            )
            .await?,
        )
    };
    let first = signed_bytes(&s_first).context(format!(
        "the durability probe's first sign was not signed: {s_first:?}"
    ))?;
    drop(c1);
    drop(c2);
    node.restart()?; // stop() is SIGKILL + reap, the instant after the reply arrived
    let mut c3 = connect(&node.ws()).await?;
    let key2 = register_delegate(&mut c3, &wasm).await?;
    if key2 != key {
        red.push("the delegate key changed across the restart".into());
    }
    let again = ask(
        &mut c3,
        &key,
        &Request::Sign {
            prev: prev_d,
            next: Next {
                seq: prev_d.seq + 1,
                root: r(8),
                ledger: vec![],
            },
        },
    )
    .await?;
    let durable = matches!(&again, Answer::AlreadySigned(b) if *b == first);
    println!(
        "DURABILITY: after SIGKILL + restart, the same prev with a different next was told {}",
        kinds(&again)
    );
    println!(
        "DURABILITY: the record {} before the reply reached the page",
        if durable {
            "WAS durable"
        } else {
            "was NOT durable"
        }
    );
    if !durable {
        red.push(format!(
            "RECORD NOT DURABLE: after a restart the same prev was answered {again:?}"
        ));
    }

    // 6a. PUT-WITH-CODE on the ISOLATED node (no peer): a client GET serves the local copy.
    put_with_code(&mut c3, &key, &bcode, "6a isolated", 9, false, &mut red).await?;

    drop(c3);
    drop(node);

    // 6b. PUT-WITH-CODE on a PEERED node: B joined to a private gateway A, both on loopback, both made here. The case
    // the design must survive: a client GET of a delegate-put block is NotFound on a node with a peer (F55), so the
    // page confirms through READ-LOCAL (`Held`).
    let pdir = dir.join("peered");
    let mut secret = [0u8; 32];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut secret)?;
    }
    let public = curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(secret).to_bytes();
    std::fs::create_dir_all(pdir.join("a"))?;
    std::fs::write(pdir.join("a/transport.key"), hex(&secret))?;
    let (aws, anet, bws, bnet) = (port + 20, port + 21, port + 30, port + 31);
    let node_a = Node::spawn_private_network(
        aws,
        anet,
        &pdir.join("a"),
        &[
            "--is-gateway".into(),
            "--transport-keypair".into(),
            pdir.join("a/transport.key").to_string_lossy().into_owned(),
            "--public-network-address".into(),
            "127.0.0.1".into(),
            "--public-network-port".into(),
            anet.to_string(),
        ],
    )?;
    let node_b = Node::spawn_private_network(
        bws,
        bnet,
        &pdir.join("b"),
        &[
            "--gateway".into(),
            format!("127.0.0.1:{anet},{}", hex(&public)),
        ],
    )?;
    println!(
        "6b: A = private gateway ws {aws} net {anet}; B = joined to A only, ws {bws} net {bnet}"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut cb = connect(&node_b.ws()).await?;
    let keyb = register_delegate(&mut cb, &wasm).await?;
    let pb = ask(
        &mut cb,
        &keyb,
        &Request::Provision {
            signing_key: sk.to_bytes().to_vec(),
            register_code: rcode.clone(),
            register_params: params.clone(),
            block_code: bcode.clone(),
        },
    )
    .await?;
    if pb != Answer::Provisioned {
        bail!("6b: B's signer not provisioned: {pb:?}");
    }
    put_with_code(&mut cb, &keyb, &bcode, "6b peered", 12, true, &mut red).await?;
    drop(cb);
    drop(node_b);
    drop(node_a);

    if red.is_empty() {
        println!("VERDICT: GREEN");
        Ok(())
    } else {
        for r in &red {
            println!("RED: {r}");
        }
        bail!("{} red", red.len())
    }
}
