// A RELOAD IS THE SAME PERSON (the switch-over blocker, sdk#234), on the REAL
// wasm Session in page mode: it ASKS the signer which Register it signs for
// before anything is minted — and mints only when told "none". The frames are
// decoded, and the signer's answers encoded, by the stdlib and signer-proto
// themselves (`wire/examples/node_answers.rs`).
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
const enc = s => new TextEncoder().encode(s);
const hex = b => Buffer.from(b).toString("hex");
function stdlib(frames = [], signerAnswer = null) {
  const r = spawnSync("cargo", ["run", "-q", "-p", "wire", "--example", "node_answers", "--", "00", "00", "00", "x", ...frames.map(hex)],
    { cwd: root, encoding: "utf8", maxBuffer: 1 << 26, env: { ...process.env, ...(signerAnswer ? { SIGNER_ANSWER: signerAnswer } : {}) } });
  assert.equal(r.status, 0, r.stderr);
  return JSON.parse(r.stdout);
}
/** The signer requests a session sends now, by name, with their ids. */
const signerRequests = s => { const out = s.outbound(); s.sent(out.length); return stdlib(out).frames.flatMap(f => f.signer ?? []); };
/** Register params as `wire::register_params` lays them out: RG01, a version byte, the key, the name. */
const params = keyByte => new Uint8Array([...enc("RG01"), 0, ...new Array(32).fill(keyByte), ...enc("head")]);
const page = () => { const s = new Session(7999); s.set_page_mode(true, enc("signer code")); s.provision(enc("delegate"), enc("block code"), enc("register code")); return s; };
const answer = (s, id, p) => { s.on_inbound(new Uint8Array(Buffer.from(stdlib([], `register:${id}:${p ? hex(p) : "none"}`).signer_answer, "hex"))); };

await t("**a page ASKS the signer which Register first — nothing is minted or provisioned yet**", async () => {
  const reqs = signerRequests(page());
  assert.deepEqual(reqs.map(r => r.req), ["Register"], `a page opened with ${JSON.stringify(reqs)}`);
});

await t("**the signer names a Register: the page opens IT, provisions nothing, and two pages told the same stand on the same head**", async () => {
  const heads = [];
  for (const _ of ["the reload", "a second tab"]) {
    const s = page();
    const [q] = signerRequests(s);
    answer(s, q.id, params(7));
    assert.deepEqual(signerRequests(s).filter(r => r.req === "Provision"), [], "a key was provisioned over the signer's own");
    assert.equal(s.provisioned(), true, "the named Register was not opened");
    heads.push(s.head_id());
  }
  assert.ok(heads[0].length === 64 && heads[0] === heads[1], `the reload and the second tab stand on ${heads}`);
  const other = page();
  answer(other, signerRequests(other)[0].id, params(8));
  assert.notEqual(other.head_id(), heads[0], "THE CONTROL: another Register gave the same head");
});

await t("**the signer holds NO key: only then is one minted and provisioned**", async () => {
  const s = page();
  const [q] = signerRequests(s);
  answer(s, q.id, null);
  assert.deepEqual(signerRequests(s).map(r => r.req), ["Provision"], "a first page did not provision a new key");
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
