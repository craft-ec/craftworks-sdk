// sdk#234's ACCEPTANCE, live: the notes page on the SDK alone, against a
// PRIVATE local 0.2.136 node (explicit dirs, --disable-auto-update, loopback,
// named ports that refuse the owner's 7509/7609), in headless Chrome.
//
//   node examples/notes/acceptance.mjs        (build.sh first; BUDGET_MS to extend)
//
// On the page path (the engine in the page, the signer on the node): 50 notes put; a reload
// brings all 50 back; a second tab sees the first tab's notes. Kills what it
// started by recorded PID. Exits non-zero on any failed check.
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync } from "node:fs";
import { createServer, connect } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../..", import.meta.url));
const CHROME = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const OWNERS = [7509, 7609];
const BUDGET_MS = Number(process.env.BUDGET_MS ?? 600_000);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const kids = [];
let failed = 0;
const check = (ok, what, ev) => { if (!ok) failed += 1; console.log(`${ok ? "PASS" : "FAIL"}  ${what}${ev === undefined ? "" : `  — ${JSON.stringify(ev)}`}`); };
const finish = async code => {
  for (const k of kids.reverse()) { try { process.kill(k.pid, "SIGTERM"); } catch (_) {} }
  await sleep(2000);
  for (const k of kids) { try { process.kill(k.pid, 0); process.kill(k.pid, "SIGKILL"); } catch (_) {} }
  process.exit(code);
};
setTimeout(() => { console.log(`FAIL  the run exceeded its ${BUDGET_MS / 1000} s budget`); finish(1); }, BUDGET_MS).unref();
process.on("exit", () => { for (const k of kids) { try { process.kill(k.pid, "SIGKILL"); } catch (_) {} } });

const free = port => new Promise(ok => { const s = createServer().once("error", () => ok(false)).listen(port, "127.0.0.1", () => s.close(() => ok(true))); });
const answers = port => new Promise(ok => { const s = connect({ port, host: "127.0.0.1" }); const d = v => { s.destroy(); ok(v); }; s.once("connect", () => d(true)); s.once("error", () => d(false)); });
const announced = (child, re, ms) => new Promise((ok, bad) => {
  const t = setTimeout(() => bad(new Error(`not announced in ${ms} ms`)), ms);
  const on = d => { const m = re.exec(String(d)); if (m) { clearTimeout(t); ok(Number(m[1])); } };
  child.stdout?.on("data", on); child.stderr?.on("data", on);
});

async function node(ws, net) {
  for (const p of [ws, net]) if (OWNERS.includes(p)) throw new Error(`${p} is the owner's; refusing`);
  if (!(await free(ws)) || !(await free(net))) throw new Error(`port ${ws}/${net} is busy; refusing`);
  const dir = mkdtempSync(join(tmpdir(), "notes-node-"));
  for (const d of ["data", "config", "log"]) mkdirSync(join(dir, d));
  const child = spawn("freenet", ["network", "--is-gateway", "--skip-load-from-network",
    "--network-address", "127.0.0.1", "--network-port", String(net),
    "--public-network-address", "127.0.0.1", "--public-network-port", String(net),
    "--ws-api-address", "127.0.0.1", "--ws-api-port", String(ws),
    "--data-dir", join(dir, "data"), "--config-dir", join(dir, "config"), "--log-dir", join(dir, "log"),
    "--disable-auto-update"], { stdio: "ignore" });
  kids.push(child);
  for (let i = 0; i < 180 && !(await answers(ws)); i += 1) await sleep(250);
  console.log(`node pid ${child.pid} ws ${ws} (${dir})`);
  return child;
}

async function tab(debug) {
  const t = await (await fetch(`http://127.0.0.1:${debug}/json/new?about:blank`, { method: "PUT" })).json();
  const ws = new WebSocket(t.webSocketDebuggerUrl);
  await new Promise((ok, bad) => { ws.onopen = ok; ws.onerror = bad; });
  let seq = 0; const waiting = new Map();
  ws.onmessage = e => { const m = JSON.parse(e.data); if (waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); } };
  const evaluate = expr => new Promise(ok => { const id = ++seq; waiting.set(id, ok); ws.send(JSON.stringify({ id, method: "Runtime.evaluate", params: { expression: `(async () => { ${expr} })()`, awaitPromise: true, returnByValue: true } })); })
    .then(r => r.result?.result?.value);
  const until = async (expr, ms, every = 500) => { const end = Date.now() + ms; let v; do { v = await evaluate(expr).catch(() => null); if (v) return v; await sleep(every); } while (Date.now() < end); return v; };
  return { evaluate, until };
}

try {
  const server = spawn("python3", ["-u", "-m", "http.server", "0", "--bind", "127.0.0.1"], { cwd: ROOT, stdio: ["ignore", "pipe", "pipe"] });
  kids.push(server);
  const page = await announced(server, /port (\d+)/, 10_000);
  const chrome = spawn(CHROME, ["--headless=new", "--disable-gpu", "--remote-debugging-port=0", `--user-data-dir=${mkdtempSync(join(tmpdir(), "notes-chrome-"))}`, "about:blank"], { stdio: ["ignore", "pipe", "pipe"] });
  kids.push(chrome);
  const debug = await announced(chrome, /DevTools listening on ws:\/\/127\.0\.0\.1:(\d+)\//, 30_000);

  for (const [path, ws, net, hash] of [["page path", 17551, 37551, ""]]) {
    console.log(`\n── ${path} ──`);
    await node(ws, net);
    const url = `http://127.0.0.1:${page}/examples/notes/#node=${ws}${hash}`;
    const a = await tab(debug);
    await a.evaluate(`location.href = ${JSON.stringify(url)}; return 1;`);
    const ready = await a.until(`return !!window.__notes;`, 120_000);
    check(!!ready, `${path}: the page opened a node-backed db`, await a.evaluate(`return document.getElementById("status")?.textContent;`));
    if (!ready) continue;
    const t0 = Date.now();
    await a.evaluate(`for (let i = 0; i < 50; i += 1) await window.__notes.db.put("notes", { text: "note " + i }); return 1;`);
    const saved = await a.until(`return window.__notes.saving() === 0 && window.__notes.notes.getSnapshot().length === 50;`, 300_000, 1000);
    check(!!saved, `${path}: 50 notes put and published ("saving" back to 0) in ${Date.now() - t0} ms`, await a.evaluate(`return { saving: window.__notes.saving(), shown: window.__notes.notes.getSnapshot().length };`));
    await a.evaluate(`location.reload(); return 1;`);
    const back = await a.until(`return window.__notes?.notes?.getSnapshot().length === 50 ? 50 : null;`, 120_000, 1000);
    check(back === 50, `${path}: after a reload, all 50 are back`, await a.evaluate(`return window.__notes?.notes?.getSnapshot().length ?? document.getElementById("status")?.textContent;`));
    const b = await tab(debug);
    await b.evaluate(`location.href = ${JSON.stringify(url)}; return 1;`);
    const seen = await b.until(`return window.__notes?.notes?.getSnapshot().length === 50 ? 50 : null;`, 120_000, 1000);
    check(seen === 50, `${path}: a second tab sees the first tab's 50 notes`, await b.evaluate(`return window.__notes?.notes?.getSnapshot().length ?? document.getElementById("status")?.textContent;`));
    // update + delete, read back in the second tab after a reload.
    await a.evaluate(`const s = window.__notes.notes.getSnapshot(); await window.__notes.db.update("notes", s[0].id, { text: "edited" }); await window.__notes.db.delete("notes", s[1].id); return 1;`);
    await a.until(`return window.__notes.saving() === 0;`, 120_000, 1000);
    await b.evaluate(`location.reload(); return 1;`);
    const after = await b.until(`const s = window.__notes?.notes?.getSnapshot() ?? []; return s.length === 49 && s.some(r => r.fields.text === "edited") ? s.length : null;`, 120_000, 1000);
    check(after === 49, `${path}: an update and a delete reach the second tab`, await b.evaluate(`return (window.__notes?.notes?.getSnapshot() ?? []).length;`));

    // TWO APPS, ONE PERSON (the forest ruling): two other apps on this node
    // stand on the SAME register — the person's one tree — and each has its
    // own `notes`; the second cannot write the first's, and can read it.
    const appUrl = id => `${url}&app=${id}`;
    const x = await tab(debug), y = await tab(debug);
    await x.evaluate(`location.href = ${JSON.stringify(appUrl("app-x"))}; return 1;`);
    await y.evaluate(`location.href = ${JSON.stringify(appUrl("app-y"))}; return 1;`);
    const bothUp = (await x.until(`return !!window.__notes;`, 120_000)) && (await y.until(`return !!window.__notes;`, 120_000));
    if (!bothUp) { check(false, `${path}: two apps opened`, await x.evaluate(`return document.getElementById("status")?.textContent;`)); continue; }
    await x.evaluate(`await window.__notes.db.put("notes", { text: "x1" }); await window.__notes.db.put("notes", { text: "x2" }); return 1;`);
    await y.evaluate(`await window.__notes.db.put("notes", { text: "y1" }); return 1;`);
    await x.until(`return window.__notes.saving() === 0;`, 120_000, 500);
    await y.until(`return window.__notes.saving() === 0;`, 120_000, 500);
    const heads = [await x.evaluate(`return window.__notes.head();`), await y.evaluate(`return window.__notes.head();`), await a.evaluate(`return window.__notes.head();`)];
    check(heads[0] && heads[0] === heads[1] && heads[1] === heads[2], `${path}: two apps and the notes-example app stand on ONE register (the person's tree)`, heads.map(h => h?.slice(0, 12)));
    const own = [await x.until(`const n = window.__notes.notes.getSnapshot().length; return n === 2 ? n : null;`, 60_000, 500), await y.until(`const n = window.__notes.notes.getSnapshot().length; return n === 1 ? n : null;`, 60_000, 500)];
    check(own[0] === 2 && own[1] === 1, `${path}: each app sees only its OWN notes (app-x 2, app-y 1; notes-example's 49 in neither)`, own);
    const cross = await y.evaluate(`try { await window.__notes.db.other("app-x").put("notes", { text: "intruder" }); return "WRITTEN"; } catch (e) { return e.code + ": " + e.message; }`);
    check(/^REFUSED: read-only: `app-x` is another app's data/.test(cross), `${path}: app-y's write into app-x's space is refused by name`, cross);
    const read = await y.evaluate(`try { return (await window.__notes.db.other("app-x").scan("notes")).map(r => r.fields.text).sort(); } catch (e) { return e.code + ": " + e.message; }`);
    check(JSON.stringify(read) === JSON.stringify(["x1", "x2"]), `${path}: app-y READS app-x's notes (public by default), and only them`, read);
  }
} catch (e) {
  failed += 1;
  console.log(`FAIL  the run stopped: ${e.message}`);
}
console.log(failed ? `\n${failed} check(s) FAILED` : "\nall checks passed");
await finish(failed ? 1 : 0);
