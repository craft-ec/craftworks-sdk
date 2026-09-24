// A SITE (builder#117) through the REAL packaged `Session` (pkg/node), driven the way a page drives it: what
// goes to the node is decoded by the stdlib's own encoding (`wire/examples/node_answers.rs`), never by bytes
// this file built. The site's whole path -- read, sign, PUT, read back -- is `page/tests/site.rs` and
// `page-io/tests/wire_node.rs`; this pins the ENTRY an app uses: the link, the first frame, the status and the
// refusals.
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
const enc = s => new TextEncoder().encode(s);
const SITE_CODE = enc("a site contract's code");
const WEB = enc("an app's web part");

/** The frames `s` would send now, decoded by the stdlib -- and sent. */
const flush = s => {
  const out = s.outbound();
  s.sent(out.length);
  const r = spawnSync("cargo", ["run", "-q", "-p", "wire", "--example", "node_answers", "--",
    hex(enc("c")), hex(enc("p")), hex(enc("s")), "x", ...out.map(hex)], { cwd: root, encoding: "utf8", maxBuffer: 1 << 26 });
  assert.equal(r.status, 0, `node_answers failed: ${r.stderr}`);
  return JSON.parse(r.stdout).frames;
};

/** Everything `node_answers` says for `frames`, with the signer's answer `signerAnswer` encoded. */
function stdlib(frames = [], signerAnswer = null) {
  const r = spawnSync("cargo", ["run", "-q", "-p", "wire", "--example", "node_answers", "--", "00", "00", "00", "x", ...frames.map(hex)],
    { cwd: root, encoding: "utf8", maxBuffer: 1 << 26, env: { ...process.env, SIGNER_CODE: hex(enc("signer code")), ...(signerAnswer ? { SIGNER_ANSWER: signerAnswer } : {}) } });
  assert.equal(r.status, 0, r.stderr);
  return JSON.parse(r.stdout);
}
/** Register params as `wire::register_params` lays them out: RG01, mode 0, the key, the name. */
const params = keyByte => new Uint8Array([...enc("RG01"), 0, ...new Array(32).fill(keyByte), ...enc("head")]);
/** A page whose registration was answered, its Register query not yet. */
const asking = () => {
  const s = new Session(7999);
  s.provision(enc("signer code"), enc("block code"), enc("register code"));
  flush(s);
  s.on_inbound(new Uint8Array(Buffer.from(stdlib().registered, "hex")));
  return s;
};
/** A page whose signer NAMED its Register (`keyByte`'s): the authority a site is relabelled from. */
const provisioned = (keyByte = 7) => {
  const s = asking();
  const out = s.outbound();
  s.sent(out.length);
  const [q] = stdlib(out).frames.flatMap(f => f.signer ?? []);
  s.on_inbound(new Uint8Array(Buffer.from(stdlib([], `register:${q.id}:${hex(params(keyByte))}`).signer_answer, "hex")));
  flush(s);
  return s;
};
const status = (s, app) => JSON.parse(s.site_status(app));

await t("no page, no site: publishing before there is a path to the node is refused by name", async () => {
  const s = new Session(7999);
  assert.throws(() => s.publish_site("notes", SITE_CODE, WEB), /provision first/);
  assert.deepEqual(status(s, "notes"), { state: "none", version: 0, said: "" });
});

await t("before the signer names the Register there is no authority yet: refused as NOT YET, not as a bad app id", async () => {
  const s = asking();
  assert.throws(() => s.publish_site("notes", SITE_CODE, WEB), /not yet: the node's signer has not said/);
  assert.throws(() => s.site_link("notes", SITE_CODE), /not yet: the node's signer has not said/);
});

await t("**the link is the site contract's, the same for every publish, and the first frame READS it with a subscription**", async () => {
  const s = provisioned();
  const link = s.site_link("notes", SITE_CODE);
  assert.equal(s.publish_site("notes", SITE_CODE, WEB), link, "publish_site returned another link than site_link");
  assert.deepEqual(status(s, "notes"), { state: "publishing", version: 0, said: "" });
  const frames = flush(s);
  assert.deepEqual(frames.filter(f => f.op === "get"), [{ op: "get", key: link, subscribe: true }], `the site was not read first: ${JSON.stringify(frames)}`);
  assert.equal(frames.filter(f => f.op === "put").length, 0, "a site was PUT before it was read and signed");
  // Another web part: the same address. Another app: another address.
  assert.equal(s.publish_site("notes", SITE_CODE, enc("another bundle")), link, "a new bundle moved the link");
  assert.notEqual(s.site_link("tasks", SITE_CODE), link, "THE CONTROL: another app got the same link");
  assert.notEqual(provisioned(8).site_link("notes", SITE_CODE), link, "THE CONTROL: another person's site got the same link");
});

await t("a label that is no app id publishes nothing, by name", async () => {
  const s = provisioned();
  assert.throws(() => s.publish_site("Not An App!", SITE_CODE, WEB), /not an app id/);
  assert.equal(flush(s).length, 0, "a bad app id sent a frame");
  assert.deepEqual(status(s, "Not An App!"), { state: "none", version: 0, said: "" });
});

await t("a person's cancel ends it as `cancelled`", async () => {
  const s = provisioned();
  s.publish_site("notes", SITE_CODE, WEB);
  flush(s);
  s.cancel_site("notes");
  assert.deepEqual(status(s, "notes"), { state: "cancelled", version: 0, said: "" });
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
