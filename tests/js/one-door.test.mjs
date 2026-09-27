// ONE DOOR FOR A SITE (ARCHITECTURE §19, app-as-data P5; the architect): the SDK's doors that can reach a NON-TREE
// PUT are DERIVED from its sources (tools/doors.mjs), never listed by hand -- and pinned here. A site is written only
// through the definition's one publish (`publishDefinition`, wasm `publish_definition_site`); a content-addressed
// piece is PUT only by the load pieces' repair (`repairPieces`, wasm `put_contract`). Two planted controls per the
// architect: a NEW door that reaches a sink, and a sink call planted inside an EXISTING read door, are each caught.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { deriveJs, deriveWasm, doorsOf } from "../../tools/doors.mjs";

const root = fileURLToPath(new URL("../..", import.meta.url));
let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const session = readFileSync(`${root}/web/src/session.rs`, "utf8");
const lib = readFileSync(`${root}/web/src/lib.rs`, "utf8");

await t("**the site-write doors are exactly publishDefinition; the piece-PUT doors exactly the load pieces' repair** (derived)", () => {
  const d = doorsOf(root);
  assert.deepEqual(d.siteWrite, { wasm: ["publish_definition_site"], js: ["engine-db.js:publishDefinition"] });
  assert.deepEqual(d.piecePut, { wasm: ["put_contract"], js: ["pieces.js:repairPieces"] });
});

await t("THE CONTROL: a NEW exported door that reaches a site sink is caught", () => {
  const planted = session.replace(/\n {4}pub fn site_link\(/, "\n    pub fn republish(&mut self) { if let Some(p) = self.page_mut() { let _ = p.publish_site(\"a\", &[], vec![], page::Ms(0)); } }\n    pub fn site_link(");
  assert.notEqual(planted, session, "THE SETUP: nothing was planted");
  assert.deepEqual(deriveWasm({ session: planted, lib }).siteWrite, ["publish_definition_site", "republish"]);
});

await t("THE CONTROL: a sink call planted inside an EXISTING read door (site_link) is caught -- through a private helper too", () => {
  const direct = session.replace(/(\n {4}pub fn site_link\([^{]*\{)/, "$1\n        if let Some(p) = self.page_mut() { let _ = p.put_contract(todo!(), todo!(), page::Ms(0)); }");
  assert.notEqual(direct, session, "THE SETUP: nothing was planted");
  assert.deepEqual(deriveWasm({ session: direct, lib }).piecePut, ["put_contract", "site_link"]);
  const viaHelper = session.replace(/(\n {4}pub fn site_link\([^{]*\{)/, "$1\n        self.create_site(\"a\", \"\", vec![], js_sys::Array::new(), vec![], todo!()).ok();");
  assert.deepEqual(deriveWasm({ session: viaHelper, lib }).siteWrite, ["publish_definition_site", "site_link"]);
});

await t("THE CONTROL: a JS door that calls a site-write door is named", () => {
  const js = { "x.js": "  sneaky: () => session.publish_definition_site(a, b, c, d, e),\n" };
  assert.deepEqual(deriveJs(js, ["publish_definition_site"]), ["x.js:sneaky"]);
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
