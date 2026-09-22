// THE FILES THE JAVASCRIPT FETCHES ARE THE FILES THE BUILD SHIPS.
//
// `SHIPPED_ARTEFACTS` (js/session.js) names each artefact by a URL beside
// session.js; `build.sh` ships the files and writes their hashes to
// artefacts.json. Nothing tied the two together: renaming an entry to a file
// the build no longer ships (`./engine_delegate.wasm`, after the switch-over)
// left every other test green, and a page would have fetched a 404 and
// handed an empty array to provisioning (main's mutant M1 on sdk#260).
//
// So, from the BUILT pkg/web (the session.js a page loads, and the manifest
// beside it): for EVERY entry of SHIPPED_ARTEFACTS, the file it names exists
// in pkg/web, it is the manifest's file for that name, and its sha256 is the
// manifest's hash. Not a silent skip when the build is missing: without a
// build there is nothing to check, and that is a failure.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack}\n`); }
};

const web = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "pkg", "web");
const sha = path => createHash("sha256").update(readFileSync(path)).digest("hex");

/**
 * Every way `shipped` (name → URL) disagrees with the build in `dir` and its
 * `manifest`: a file that is not shipped, a file that is not the manifest's
 * for that name, a hash that is not the manifest's. Empty means they agree.
 */
function disagreements(shipped, manifest, dir) {
  const out = [];
  for (const [name, url] of Object.entries(shipped)) {
    const file = basename(new URL(url).pathname);
    const entry = manifest[name];
    if (!entry) { out.push(`${name}: the manifest has no entry`); continue; }
    if (file !== entry.file) out.push(`${name}: fetches ${file}, the manifest ships ${entry.file}`);
    const path = join(dir, file);
    if (!existsSync(path)) { out.push(`${name}: ${file} is not in the build`); continue; }
    if (sha(path) !== entry.sha256) out.push(`${name}: ${file} does not hash to the manifest's ${entry.sha256}`);
  }
  return out;
}

const manifest = JSON.parse(readFileSync(join(web, "artefacts.json"), "utf8"));
const { SHIPPED_ARTEFACTS } = await import(join(web, "session.js"));

await t("**every artefact the JavaScript fetches is a file the build ships, with the manifest's hash**", async () => {
  const names = Object.keys(SHIPPED_ARTEFACTS).sort();
  assert.deepEqual(names, ["block", "register", "signer"], `SHIPPED_ARTEFACTS names ${names.join(", ")}`);
  assert.deepEqual(disagreements(SHIPPED_ARTEFACTS, manifest, web), []);
});

await t("THE CONTROL: a name the build no longer ships is refused, naming it", async () => {
  const wrong = { ...SHIPPED_ARTEFACTS, signer: new URL("./engine_delegate.wasm", SHIPPED_ARTEFACTS.signer).href };
  const found = disagreements(wrong, manifest, web);
  assert.ok(found.some(d => /signer: fetches engine_delegate\.wasm/.test(d)), `not refused: ${found}`);
  assert.ok(found.some(d => /engine_delegate\.wasm is not in the build/.test(d)), `the missing file was not named: ${found}`);
});

await t("THE CONTROL: a hash that is not the file's is refused", async () => {
  const lied = { ...manifest, block: { ...manifest.block, sha256: "0".repeat(64) } };
  const found = disagreements(SHIPPED_ARTEFACTS, lied, web);
  assert.ok(found.some(d => /block: block\.wasm does not hash/.test(d)), `not refused: ${found}`);
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
