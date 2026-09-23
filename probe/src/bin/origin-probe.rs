//! LIVE: WHAT ORIGIN DOES THE SIGNER SEE (sdk#318, the architect's C1)?
//!
//! A web app the node serves runs sandboxed inside the node's SHELL page; the node injects a WebSocket shim into the
//! app that routes `new WebSocket(..)` over postMessage to the shell, which opens the real socket with the app's
//! token. If that holds, the signer sees `WebApp(<app contract id>)` for a page the node served, and `None` for a
//! client it did not. This measures it with the signer's own gate as the instrument:
//!
//! - a private LOCAL node; the signer provisioned natively (tokenless), which hands back the owner capability;
//! - two app web containers, identical but for a tag, whose page asks the signer "which Register?" over a plain
//!   `new WebSocket` and puts the answer's kind in its title; the capability APPROVES app A only;
//! - headless Chrome opens each app's shell page; the title the shell shows is read back over CDP (`/json/list`).
//!
//! Gated "which Register?" answers `Register` only for an approved `WebApp(id)` (or the capability), else
//! `Refused(NotApproved)`. So: A → `Register` means the bridge carries A's identity; B → `Refused` is the control
//! (an unapproved app); the native tokenless ask → `Refused` is the second control.
//!
//! usage: PROBE_PORT=<port> PROBE_TMP=<dir> origin-probe <signer.wasm> <webapp.wasm>

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use probe::node::{Mode, Node, TempTree};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use wire::signer::{SignerAnswer, SignerRequest};


type Sock = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Send `frames`, then read messages until a signer answer (or `ack`) arrives, within 30 s.
async fn exchange(sock: &mut Sock, frames: Vec<Vec<u8>>, want_signer: bool) -> Result<Option<SignerAnswer>> {
    for f in frames {
        sock.send(Message::Binary(f.into())).await?;
    }
    let mut re = wire::Reassembler::default();
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        let Ok(Some(Ok(Message::Binary(b)))) = tokio::time::timeout(Duration::from_secs(1), sock.next()).await else { continue };
        match wire::unframe(&mut re, &b) {
            wire::Incoming::EngineBytes(msgs) => {
                for m in msgs {
                    if let Some((_, a)) = wire::signer::read_answer(&m) {
                        return Ok(Some(a));
                    }
                }
            }
            wire::Incoming::Ack(_) if !want_signer => return Ok(None),
            wire::Incoming::Partial => {}
            other => {
                if !want_signer {
                    bail!("expected an ack, got {other:?}");
                }
            }
        }
    }
    bail!("no answer within 30 s")
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// The app page: over a plain `new WebSocket`, register the signer (frames `reg`) then ask "which Register?"
/// (frames `ask`); the title becomes `origin-probe <tag> <answer kind>`.
fn page(tag: &str, reg: &[Vec<u8>], ask: &[Vec<u8>]) -> String {
    let arr = |fs: &[Vec<u8>]| fs.iter().map(|f| format!("\"{}\"", b64(f))).collect::<Vec<_>>().join(",");
    format!(
        r#"<!doctype html><html><head><title>origin-probe {tag} waiting</title></head><body><script>
const dec = s => Uint8Array.from(atob(s), c => c.charCodeAt(0));
const REG = [{reg}].map(dec), ASK = [{ask}].map(dec);
const KINDS = ["Signed","NotNext","AlreadySigned","Provisioned","Putting","Put","Held","Refused","Register","Capability","Approved","Owns"];
const ws = new WebSocket(`ws://${{location.host}}/v1/contract/command?encodingProtocol=native`);
ws.binaryType = "arraybuffer";
let asked = false;
ws.onopen = () => {{ for (const f of REG) ws.send(f); }};
ws.onmessage = e => {{
  const b = new Uint8Array(e.data);
  if (!asked) {{ asked = true; for (const f of ASK) ws.send(f); return; }}
  for (let i = 0; i + 12 <= b.length; i++) {{
    if (b[i] === 83 && b[i+1] === 71 && b[i+2] === 48 && b[i+3] === 50) {{
      const tagv = b[i+8] | (b[i+9] << 8) | (b[i+10] << 16) | (b[i+11] << 24);
      document.title = "origin-probe {tag} " + (KINDS[tagv] ?? ("kind " + tagv));
      return;
    }}
  }}
}};
ws.onerror = () => {{ document.title = "origin-probe {tag} socket-error"; }};
</script></body></html>"#,
        reg = arr(reg),
        ask = arr(ask)
    )
}

/// Opens each URL in headless Chrome and prints every tab's title once none says "waiting" (or after 90 s).
const READ_JS: &str = r#"
import { spawn } from "node:child_process";
const [dataDir, ...urls] = process.argv.slice(2);
const chrome = spawn("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", ["--headless=new", "--disable-gpu", "--remote-debugging-port=0", `--user-data-dir=${dataDir}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
const port = await new Promise((ok, bad) => { const t = setTimeout(() => bad(new Error("no port")), 30000); chrome.stderr.on("data", d => { const m = /127\.0\.0\.1:(\d+)\//.exec(String(d)); if (m) { clearTimeout(t); ok(m[1]); } }); });
for (const u of urls) await fetch(`http://127.0.0.1:${port}/json/new?${u}`, { method: "PUT" });
const end = Date.now() + 90000;
let list = [];
while (Date.now() < end) {
  list = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).filter(t => t.type === "page");
  const ours = list.filter(t => t.title.startsWith("origin-probe"));
  if (ours.length >= urls.length && ours.every(t => !t.title.endsWith("waiting"))) break;
  await new Promise(r => setTimeout(r, 500));
}
for (const t of list) console.log(`${t.title}  <${t.url}>`);
chrome.kill("SIGKILL");
process.exit(0);
"#;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("PROBE_PORT").ok().and_then(|p| p.parse().ok()).context("PROBE_PORT is required")?;
    if port == 7509 || port == 7609 {
        bail!("port {port} is the owner's node");
    }
    let tmp = std::path::PathBuf::from(std::env::var("PROBE_TMP").context("PROBE_TMP is required")?);
    let mut a = std::env::args().skip(1);
    let signer_code = std::fs::read(a.next().context("signer.wasm")?)?;
    let webapp_code = std::fs::read(a.next().context("webapp.wasm")?)?;

    let dir = tmp.join(format!("origin-probe-{}", std::process::id()));
    let _tree = TempTree(dir.clone());
    std::fs::create_dir_all(dir.join("webapp-cache"))?;
    // ITS OWN web-app cache (craftworks CLAUDE.md): the child inherits this.
    std::env::set_var("FREENET_WEBAPP_CACHE_DIR", dir.join("webapp-cache"));
    let node = Node::spawn_in(port, &dir, Mode::Local)?;
    println!("node: ws {} under {}", node.ws(), dir.display());
    let (mut sock, _) = tokio_tungstenite::connect_async(node.ws()).await.context("connecting")?;

    // The signer, provisioned natively: tokenless, so the capability comes back to THIS client.
    let (container, signer) = wire::delegate_from_code(&signer_code);
    exchange(&mut sock, wire::frame_register_delegate(container.clone(), 1).map_err(anyhow::Error::msg)?, false).await.context("registering the signer")?;
    let sk = [0x42u8; 32];
    let cap = match exchange(&mut sock, wire::signer::frame_provision(&signer, 2, sk.to_vec(), b"register code, unused here".to_vec(), wire::register_params(&[1u8; 32], wire::HEAD_NAME), b"block code, unused here".to_vec(), 2).map_err(anyhow::Error::msg)?, true).await? {
        Some(SignerAnswer::Capability(c)) => c,
        other => bail!("provisioning answered {other:?} (is this signer.wasm the #318 build?)"),
    };
    // CONTROL 2: this same tokenless client, WITHOUT the capability, is refused "which Register?".
    let native = exchange(&mut sock, wire::signer::frame_register_query(&signer, 3, 3).map_err(anyhow::Error::msg)?, true).await?;
    println!("native tokenless ask, no capability: {native:?}");

    // Two apps, A approved and B not, each a page asking over a plain WebSocket.
    let reg = wire::frame_register_delegate(container, 11).map_err(anyhow::Error::msg)?;
    let ask = wire::signer::frame_register_query(&signer, 12, 12).map_err(anyhow::Error::msg)?;
    let mut apps = Vec::new();
    for (i, tag) in ["A", "B"].iter().enumerate() {
        let html = page(tag, &reg, &ask);
        let state = wire::webapp::app_container(&[("index.html", html.as_bytes())]).map_err(anyhow::Error::msg)?;
        let params = wire::webapp::params(&state);
        let (address, contract, wrapped) = wire::puts::contract(&webapp_code, &params, &state);
        exchange(&mut sock, wire::frame_put(contract.clone(), wrapped, 20 + i as u32).map_err(anyhow::Error::msg)?, false).await.with_context(|| format!("putting app {tag}"))?;
        let mut id = [0u8; 32];
        id.copy_from_slice(&contract.key().id().as_bytes()[..32]);
        println!("app {tag}: {address}");
        apps.push((tag.to_string(), address, id));
    }
    let approve = SignerRequest::WithCap { cap, inner: Box::new(SignerRequest::Approve { app: apps[0].2 }) };
    let framed = wire::frame_delegate_op(&signer, &freenet_stdlib::prelude::Parameters::from(vec![]), signer::encode_request(4, &approve), 4).map_err(anyhow::Error::msg)?;
    println!("approve A: {:?}", exchange(&mut sock, framed, true).await?);

    // Chrome opens each app's SHELL page, as a person would; a small node script drives it (CDP over fetch) and
    // prints each tab's title once the page has set it.
    let script = dir.join("read.mjs");
    std::fs::write(&script, READ_JS)?;
    let urls: Vec<String> = apps.iter().map(|(_, addr, _)| format!("http://127.0.0.1:{port}/v1/contract/web/{addr}/")).collect();
    let out = std::process::Command::new("node").arg(&script).arg(dir.join("chrome")).args(&urls).output().context("node")?;
    let text = String::from_utf8_lossy(&out.stdout);
    for (tag, _, _) in &apps {
        let line = text.lines().find(|l| l.contains(&format!("origin-probe {tag} "))).map(str::trim).unwrap_or("(no title)");
        println!("app {tag} ({}): {line}", if tag == "A" { "APPROVED" } else { "not approved" });
    }
    if !out.status.success() {
        println!("  reader: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
    }
    drop(node);
    Ok(())
}
