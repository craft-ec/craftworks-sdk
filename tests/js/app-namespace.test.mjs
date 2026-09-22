// THE APP NAMESPACE (the forest ruling), on the REAL wasm Session: a person's
// one tree is divided by app; every name an app uses is RELATIVE to it, so it
// has no name for another app's data and cannot write it. Reading another
// app's data (public by default) is the explicit `@app/name` form `other()`
// uses. The mapping itself is tested natively (tests/app_namespace.rs); an
// app's own reads and writes need a node, so the two-apps-on-one-register run
// is live: examples/notes/acceptance.mjs.
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const { Session } = createRequire(import.meta.url)("../../pkg/node/craftworks_sdk.js");
let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack ?? JSON.stringify(e)}\n`); }
};
const schema = JSON.stringify({ type: "Note", fields: [{ name: "text", kind: "text", required: true }] });
const refused = (fn, re) => {
  let err; try { fn(); } catch (e) { err = e; }
  assert.ok(err, "not refused");
  assert.equal(err.code, "REFUSED", JSON.stringify(err));
  assert.match(err.message, re);
};
const app = name => { const s = new Session(7999); s.set_app(name); return s; };

await t("**a session with NO app writes nothing — refused by name**", async () => {
  const s = new Session(7999);
  refused(() => s.define("notes", schema), /no app/);
  refused(() => s.put("notes", JSON.stringify({ text: "x" })), /no app/);
});

await t("**another app's data is never WRITTEN: `@app/name` is refused before the store**", async () => {
  const a = app("alpha");
  for (const write of [() => a.put("@beta/notes", "{}"), () => a.define("@beta/notes", schema), () => a.delete("@beta/notes", "00".repeat(24)), () => a.update("@beta/notes", "00".repeat(24), "{}")]) {
    refused(write, /another app's: an app writes only its own/);
  }
});

await t("an app id is set once, and must be one", async () => {
  const s = app("alpha");
  refused(() => s.set_app("beta"), /cannot become `beta`/);
  s.set_app("alpha");
  for (const bad of ["", "has.dot", "UPPER", "a".repeat(33), "@x"]) refused(() => new Session(7999).set_app(bad), /must be 1–32/);
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
