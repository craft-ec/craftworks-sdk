// A PUT OF A CONTRACT THE APP NAMES (builder#104), through the REAL `Session`.
//
// Not a fake: this constructs the wasm `Session` (pkg/node) and drives it the
// way a page does — `outbound`/`sent` for what goes to the node, `on_inbound`
// for what comes back, `reconnected` for a new socket. What the node says is
// the stdlib's own encoding (`wire/examples/node_answers.rs`), and the frames
// the Session sends are decoded by it too: bytes this file built by hand would
// only agree with its own idea of the format.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const { Session } = createRequire(import.meta.url)("../../pkg/node/craftworks_sdk.js");

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack}\n`); }
};

const hex = b => Buffer.from(b).toString("hex");
const bytes = h => new Uint8Array(Buffer.from(h, "hex"));
const CODE = new TextEncoder().encode("an app's contract code");
const PARAMS = new TextEncoder().encode("its params");
const STATE = new TextEncoder().encode("a web container");
const OTHER = new TextEncoder().encode("somebody else's code");

/** The node's ack and refusal for (code, params, state), and `frames` decoded. */
function node(code, params, state, cause = "invalid put", frames = []) {
  const r = spawnSync("cargo", ["run", "-q", "-p", "wire", "--example", "node_answers", "--",
    hex(code), hex(params), hex(state), cause, ...frames.map(hex)], { cwd: root, encoding: "utf8", maxBuffer: 1 << 26 });
  assert.equal(r.status, 0, `node_answers failed: ${r.stderr}`);
  const o = JSON.parse(r.stdout);
  return { key: o.key, ack: bytes(o.ack), refusal: bytes(o.refusal), frames: o.frames };
}

const status = (s, k) => JSON.parse(s.put_status(k));
/** What the page would send now, decoded — and sent. */
const flush = (s, ours) => {
  const out = s.outbound();
  s.sent(out.length);
  return node(CODE, PARAMS, STATE, "x", out).frames.filter(f => ours === undefined || f.op === "put");
};

/** A session provisioned on the page path (the only one): the node is reached
 * through page-io, and a PUT before this has no path at all. */
const provisioned = () => {
  const s = new Session(7999);
  s.provision(new TextEncoder().encode("signer code"), new TextEncoder().encode("block code"), new TextEncoder().encode("register code"));
  flush(s);
  return s;
};

const us = node(CODE, PARAMS, STATE);
const them = node(OTHER, PARAMS, STATE);

await t("the PUT goes out as ONE frame: a Put of the app's contract with its state, under the key put_contract returned", async () => {
  const s = provisioned();
  assert.deepEqual(status(s, us.key), { state: "none", said: "" });
  const key = s.put_contract(CODE, PARAMS, STATE);
  assert.equal(key, us.key, "put_contract returned a key the node does not name the contract by");
  assert.deepEqual(status(s, key), { state: "pending", said: "" });
  assert.deepEqual(flush(s, "puts"), [{ op: "put", key, state: hex(STATE) }], "the PUT frames sent are not exactly the PUT");
  assert.equal(s.outbound().length, 0, "the PUT was not given up once sent");
});

await t("**the node's ack settles it: none → pending → put**", async () => {
  const s = provisioned();
  const key = s.put_contract(CODE, PARAMS, STATE);
  flush(s);
  s.on_inbound(them.ack);
  assert.deepEqual(status(s, key), { state: "pending", said: "" }, "an ack for somebody else's contract settled ours");
  s.on_inbound(us.ack);
  assert.deepEqual(status(s, key), { state: "put", said: "" });
});

await t("**a refusal naming the contract: → refused, in the node's words, and not blamed on provisioning**", async () => {
  const s = provisioned();
  const key = s.put_contract(CODE, PARAMS, STATE);
  flush(s);
  s.on_inbound(node(OTHER, PARAMS, STATE, "not ours").refusal);
  assert.deepEqual(status(s, key), { state: "pending", said: "" }, "a refusal of somebody else's contract settled ours");
  s.on_inbound(node(CODE, PARAMS, STATE, "the state does not hash to its params").refusal);
  assert.deepEqual(status(s, key), { state: "refused", said: "the state does not hash to its params" });
  assert.equal(s.refused(), "", "the app's refused PUT was reported as provisioning's");
  s.on_inbound(us.ack);
  assert.equal(status(s, key).state, "refused", "a late answer flipped a settled PUT");
});

await t("**a dropped socket: the PUT stays pending and the PAGE sends it again at its deadline; the new answer settles it; a settled one stays**", async () => {
  const s = provisioned();
  const done = s.put_contract(OTHER, PARAMS, STATE);
  const key = s.put_contract(CODE, PARAMS, STATE);
  flush(s);
  s.on_inbound(them.ack);
  s.reconnected();
  assert.deepEqual(status(s, key), { state: "pending", said: "" }, "a drop ended a PUT the page still owns");
  assert.equal(status(s, done).state, "put", "a drop reopened a PUT already answered");
  // Nothing answers: the page's clock sends it again (the RTO, from 1 s).
  let again = [];
  const end = Date.now() + 15_000;
  while (Date.now() < end && again.length === 0) {
    await new Promise(r => setTimeout(r, 200));
    s.tick();
    again = flush(s, "puts").filter(f => f.op === "put" && f.key === key);
  }
  assert.equal(again.length, 1, "the unanswered PUT was never sent again");
  s.on_inbound(us.ack);
  assert.deepEqual(status(s, key), { state: "put", said: "" });
});

await t("**the PUT goes through page-io, and its ack and refusal come back to put_status**", async () => {
  const s = new Session(7999);
  assert.throws(() => s.put_contract(CODE, PARAMS, STATE), /provision first/, "a PUT before there is a path to the node");
  s.provision(new TextEncoder().encode("signer code"), new TextEncoder().encode("block code"), new TextEncoder().encode("register code"));
  flush(s);
  const key = s.put_contract(CODE, PARAMS, STATE);
  assert.deepEqual(flush(s, "puts"), [{ op: "put", key, state: hex(STATE) }], "the PUT did not go out");
  s.on_inbound(us.ack);
  assert.deepEqual(status(s, key), { state: "put", said: "" }, "page-io's handed-back ack never reached put_status");
  const other = s.put_contract(OTHER, PARAMS, STATE);
  flush(s);
  s.on_inbound(node(OTHER, PARAMS, STATE, "too big").refusal);
  assert.deepEqual(status(s, other), { state: "refused", said: "too big" });
});

await t("an answer for a contract this session never put is counted unusable, not dropped", async () => {
  const s = new Session(7999);
  s.provision(new TextEncoder().encode("signer code"), new TextEncoder().encode("block code"), new TextEncoder().encode("register code"));
  flush(s);
  const before = s.unusable();
  s.on_inbound(them.ack);
  assert.notEqual(s.unusable(), before);
  assert.match(s.unusable(), /never put/);
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
