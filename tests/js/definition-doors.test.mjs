// THE DEFINITION DOORS THROUGH THE REAL SDK (app-as-data P2, ARCHITECTURE §19): the in-tab `Db` from pkg/node, as an
// app holds it (`wrap`). The reserved names are spelled here only to plant them: an ordinary write of each is REFUSED;
// the doors edit the draft, publish it in one write, and read both definitions back; the live marker is created once.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { wrap } from "../../js/wrap.js";

const sdk = wrap(createRequire(import.meta.url)("../../pkg/node/craftworks_sdk.js"));
let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack}\n`); }
};

await t("**an ordinary write of the reserved prefix is refused**, by name, and an ordinary domain is not", async () => {
  const db = new sdk.Db();
  for (const d of ["craftworks.draft", "craftworks.app", "craftworks.published", "craftworks"]) {
    await assert.rejects(() => db.define(d, { type: "T", fields: [] }), /reserved `craftworks\.` prefix/, `define of ${d}`);
  }
  await db.define("rows", { type: "Row", fields: [{ name: "n", kind: "int" }] });
  assert.ok((await db.put("rows", { n: 1 })).id, "THE CONTROL: an ordinary domain writes");
});

await t("**the doors**: draft edited, published in one write, both definitions read back; the marker created once", async () => {
  const db = new sdk.Db();
  await db.draftPut("meta", { name: "Notes" });
  await db.draftPut("c/header", { kind: "Text" });
  await db.draftPut("d/rows", { type: "Row" });
  assert.equal(await db.draftDelete("d/rows"), true);
  const byKey = rs => Object.fromEntries(rs.map(r => [r.key, r.body]));
  assert.deepEqual(byKey(await db.definition("draft")), { meta: { name: "Notes" }, "c/header": { kind: "Text" } });
  assert.deepEqual(await db.definition("app"), [], "published before a publish");
  assert.equal(await db.publishDefinition(), 2);
  assert.deepEqual(byKey(await db.definition("app")), { meta: { name: "Notes" }, "c/header": { kind: "Text" } });
  assert.equal(await db.publishDefinition(), 0, "an unchanged draft published something");
  await assert.rejects(() => db.draftPut("x/y", {}), /not a definition key/);
  assert.equal(await db.isPublished("rows"), false);
  assert.equal((await db.markPublished("rows")).outcome, "created");
  assert.equal((await db.markPublished("rows")).outcome, "exists");
  assert.equal(await db.isPublished("rows"), true);
});

// §19 P3: a definition is READ by app (the builder's project list), and only read: a write door takes no app, so an
// app argument handed to one does not reach another app's draft.
await t("**another app's definition is read, never written**: the in-tab Db reads by app id; a write door ignores one", async () => {
  const db = new sdk.Db();
  await db.draftPut("meta", { name: "Mine" });
  assert.deepEqual(await db.definition("draft", "someone-else"), [], "another app's (empty) draft read this tab's records");
  await db.draftPut("meta", { name: "Planted" }, "someone-else");
  assert.deepEqual(await db.definition("draft", "someone-else"), [], "a write door wrote into another app's draft");
  assert.deepEqual((await db.definition("draft")).map(r => r.body.name), ["Planted"], "THE CONTROL: the write landed in this tab's own draft");
  await assert.rejects(() => db.definition("draft", "Not An App"), /app id/);
  assert.deepEqual(await db.definitionApps(), [], "the tab's own (unnamed) draft was listed as an app of a tree");
});

if (failures) { process.stdout.write(`\n${failures} failing\n`); process.exit(1); }
process.stdout.write("\nall passing\n");
