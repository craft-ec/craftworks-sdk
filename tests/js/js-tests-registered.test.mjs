// EVERY JS TEST FILE RUNS: tools/js-tests-registered.mjs over a script and a file list, each refusal with its control.
import assert from "node:assert/strict";
import { registered } from "../../tools/js-tests-registered.mjs";

let failures = 0;
const t = (name, fn) => {
  try { fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const chain = (...fs) => fs.map(f => `node tests/js/${f}`).join(" && ");

t("**a test file the chain never runs is refused by name**; control: the same file listed passes", () => {
  const files = ["a.test.mjs", "keep-tab.test.mjs", "vectors.json"];
  assert.deepEqual(registered(chain("a.test.mjs"), files).unrun, ["tests/js/keep-tab.test.mjs"]);
  assert.deepEqual(registered(chain("a.test.mjs", "keep-tab.test.mjs"), files).unrun, [], "THE CONTROL");
});

t("**a chain entry with no file is refused**, and one listed twice; a .cjs and the last entry count too", () => {
  const r = registered(chain("ids.test.cjs", "gone.test.mjs", "ids.test.cjs"), ["ids.test.cjs"]);
  assert.deepEqual(r.missing, ["tests/js/gone.test.mjs"]);
  assert.deepEqual(r.twice, ["tests/js/ids.test.cjs"]);
  assert.deepEqual(registered(chain("ids.test.cjs"), ["ids.test.cjs"]), { unrun: [], missing: [], twice: [], count: 1 }, "THE CONTROL");
});

t("non-test files in tests/js (fixtures, vectors) are not asked for", () => {
  assert.deepEqual(registered(chain("a.test.mjs"), ["a.test.mjs", "pieces-vectors.json", "helper.mjs"]).unrun, []);
});

if (failures) { process.stdout.write(`js-tests-registered: ${failures} FAILED\n`); process.exit(1); }
