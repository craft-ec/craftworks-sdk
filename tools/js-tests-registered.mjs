// EVERY JS TEST FILE RUNS (the JS twin of sdk#475). `npm test` is a hand-written chain in package.json, so a new
// tests/js/*.test.* file that is not added to it never runs, and every gate stays green: keep-tab.test.mjs sat outside
// the chain until engineer1 found it wiring #474. Refuses, by name: a test file the chain never runs, a chain entry
// with no file, and an entry listed twice.
//
// usage: node tools/js-tests-registered.mjs            (from the SDK root; exit 1 on any refusal)
import { readdirSync, readFileSync } from "node:fs";

const TEST_FILE = /\.test\.(mjs|cjs|js)$/;

/** The refusals for a `test` script and the files in tests/js: `{ unrun, missing, twice }`. */
export function registered(testScript, files) {
  const listed = [...testScript.matchAll(/node (tests\/js\/\S+?)(?=\s|&&|$)/g)].map(m => m[1]);
  const tests = files.filter(f => TEST_FILE.test(f)).map(f => `tests/js/${f}`);
  return {
    unrun: tests.filter(f => !listed.includes(f)),
    missing: listed.filter(f => !tests.includes(f)),
    twice: [...new Set(listed.filter((f, i) => listed.indexOf(f) !== i))],
    count: tests.length,
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const script = JSON.parse(readFileSync("package.json", "utf8")).scripts.test;
  const r = registered(script, readdirSync("tests/js"));
  for (const f of r.unrun) process.stdout.write(`${f}: a JS test file npm test never runs (add it to package.json's test chain)\n`);
  for (const f of r.missing) process.stdout.write(`${f}: in package.json's test chain, but there is no such file\n`);
  for (const f of r.twice) process.stdout.write(`${f}: listed twice in package.json's test chain\n`);
  const bad = r.unrun.length + r.missing.length + r.twice.length;
  if (!bad) process.stdout.write(`js tests registered: ${r.count} of ${r.count}\n`);
  process.exit(bad ? 1 : 0);
}
