// PAGE-MODE OPENS, COUNTED: N fresh private 0.2.136 nodes, one fresh tab each,
// a page-mode session opened through the real openSession, and how long until
// the signer is provisioned (or that it never was within the budget, with what
// page-io called unusable). The evidence a re-ask on the RTO is judged by:
// before it, 4 of 9 opens hung.
//
//   node tools/page-mode-opens.mjs [N=24]      (build.sh first)
//
// Private nodes (explicit dirs in $TMPDIR, --disable-auto-update, loopback,
// ports refused if they are the owner's 7509/7609); everything started is
// killed by its recorded PID.
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const CHROME = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const N = Number(process.argv[2] ?? 24), BUDGET_S = 15;
const kids = []; const bye = () => { for (const k of kids) try { process.kill(k.pid, "SIGKILL"); } catch (_) {} };
process.on("exit", bye);
const ann = (c, re) => new Promise(ok => { const on = d => { const m = re.exec(String(d)); if (m) ok(Number(m[1])); }; c.stdout.on("data", on); c.stderr.on("data", on); });
const probe = join(ROOT, "tools", ".page-mode-open.tmp.html");
writeFileSync(probe, `<!doctype html><meta charset="utf-8"><body><script type="module">
import { load } from "../pkg/web/index.js";
import { SHIPPED_ARTEFACTS } from "../pkg/web/session.js";
const port = Number(new URLSearchParams(location.hash.slice(1)).get("node"));
const t0 = performance.now();
const sdk = await load();
const h = await sdk.internals.openSession(sdk.internals.Session, { port, pageMode: true, artefacts: SHIPPED_ARTEFACTS });
for (;;) {
  if (h.provisioned()) { window.__r = { ok: true, ms: Math.round(performance.now() - t0) }; break; }
  if (performance.now() - t0 > ${BUDGET_S * 1000}) { window.__r = { ok: false, unusable: h.unusable() }; break; }
  await new Promise(r => setTimeout(r, 50));
}
</script>`);
try {
  const srv = spawn("python3", ["-u", "-m", "http.server", "0", "--bind", "127.0.0.1"], { cwd: ROOT, stdio: ["ignore", "pipe", "pipe"] }); kids.push(srv);
  const page = await ann(srv, /port (\d+)/);
  const ch = spawn(CHROME, ["--headless=new", "--disable-gpu", "--remote-debugging-port=0", `--user-data-dir=${mkdtempSync(join(tmpdir(), "pmo-chrome-"))}`, "about:blank"], { stdio: ["ignore", "pipe", "pipe"] }); kids.push(ch);
  const debug = await ann(ch, /DevTools listening on ws:\/\/127\.0\.0\.1:(\d+)\//);
  const results = [];
  for (let i = 0; i < N; i += 1) {
    const WS = 17700 + 2 * i, NET = 37700 + 2 * i;
    if ([7509, 7609].includes(WS) || [7509, 7609].includes(NET)) throw new Error("refusing an owner's port");
    const dir = mkdtempSync(join(tmpdir(), "pmo-node-")); for (const d of ["data", "config", "log"]) mkdirSync(join(dir, d));
    const n = spawn("freenet", ["network", "--is-gateway", "--skip-load-from-network", "--network-address", "127.0.0.1", "--network-port", String(NET), "--public-network-address", "127.0.0.1", "--public-network-port", String(NET), "--ws-api-address", "127.0.0.1", "--ws-api-port", String(WS), "--data-dir", join(dir, "data"), "--config-dir", join(dir, "config"), "--log-dir", join(dir, "log"), "--disable-auto-update"], { stdio: "ignore", env: { ...process.env, FREENET_WEBAPP_CACHE_DIR: join(dir, "webapp_cache") } }); kids.push(n);
    await new Promise(r => setTimeout(r, 5000));
    const t = await (await fetch(`http://127.0.0.1:${debug}/json/new?about:blank`, { method: "PUT" })).json();
    const ws = new WebSocket(t.webSocketDebuggerUrl); await new Promise(r => (ws.onopen = r));
    let id = 0; const w = new Map(); ws.onmessage = e => { const m = JSON.parse(e.data); w.get(m.id)?.(m); };
    const ev = x => new Promise(ok => { const k = ++id; w.set(k, ok); ws.send(JSON.stringify({ id: k, method: "Runtime.evaluate", params: { expression: x, returnByValue: true, awaitPromise: true } })); }).then(r => r.result?.result?.value);
    await ev(`location.href = "http://127.0.0.1:${page}/tools/.page-mode-open.tmp.html#node=${WS}"; 1`);
    let r = null;
    for (let s = 0; s < BUDGET_S + 10 && !r; s += 1) { await new Promise(res => setTimeout(res, 1000)); r = await ev("window.__r ?? null"); }
    r ??= { ok: false, unusable: ["the page never reported"] };
    results.push(r);
    console.log(`open ${i + 1}/${N}: ${r.ok ? `provisioned in ${r.ms} ms` : `NOT provisioned in ${BUDGET_S} s; unusable ${JSON.stringify(r.unusable).slice(0, 200)}`}`);
    ws.close(); try { process.kill(n.pid, "SIGKILL"); } catch (_) {}
  }
  const ok = results.filter(r => r.ok), ms = ok.map(r => r.ms).sort((a, b) => a - b);
  console.log(`\n${ok.length} of ${N} opened; provisioned in ms: min ${ms[0]}, median ${ms[Math.floor(ms.length / 2)]}, max ${ms[ms.length - 1]}`);
  process.exitCode = ok.length === N ? 0 : 1;
} finally { rmSync(probe, { force: true }); bye(); }
