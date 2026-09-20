// The engine-backed database wrapper: promises, stable codes, one re-ask.
//
// No wasm and no socket. The wrapper's whole job is to move JSON and resolve
// promises, so a fake session that throws what Rust throws exercises all of
// it — and does so on any machine, which is what makes it a gate.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { engineDb, DbError } from "../../js/engine-db.js";

let failures = 0;
const test = (name, fn) => {
  try { fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const atest = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};

/// A session that throws the codes Rust throws, `plan` answers in order.
const fake = plan => {
  let calls = 0;
  return {
    calls: () => calls,
    scan() {
      const step = plan[Math.min(calls, plan.length - 1)];
      calls += 1;
      if (step.throw) throw step.throw;
      return JSON.stringify(step.rows);
    },
    put() {
      const step = plan[Math.min(calls, plan.length - 1)];
      calls += 1;
      if (step.throw) throw step.throw;
      return JSON.stringify(step.rows);
    },
  };
};

const notLoaded = { code: "NOT_LOADED", message: "not loaded yet", transient: true };
const tooLarge = { code: "TOO_LARGE", message: "over the limit", transient: false };

await atest("a NOT_LOADED read reloads and asks again, once", async () => {
  const s = fake([{ throw: notLoaded }, { rows: [{ id: "a" }] }]);
  const db = engineDb(s);
  const rows = await db.scan("tasks");
  assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.calls(), 2, "it did not re-ask exactly once");
});

await atest("a read that is STILL not loaded throws rather than spinning", async () => {
  const s = fake([{ throw: notLoaded }]);
  const db = engineDb(s);
  await assert.rejects(() => db.scan("tasks"), e => {
    assert.ok(e instanceof DbError, "not a DbError");
    assert.equal(e.code, "NOT_LOADED");
    return true;
  });
  assert.equal(s.calls(), 2, "it asked more than twice; a missing range must not spin");
});

await atest("THE CONTROL: a read that succeeds first time is not retried", async () => {
  const s = fake([{ rows: [] }]);
  const db = engineDb(s);
  const rows = await db.scan("tasks");
  assert.deepEqual(rows, []);
  assert.equal(s.calls(), 1, "a successful read was asked twice");
});

await atest("an error that a reload cannot fix is NOT retried", async () => {
  const s = fake([{ throw: tooLarge }]);
  const db = engineDb(s);
  await assert.rejects(() => db.scan("tasks"), e => {
    assert.equal(e.code, "TOO_LARGE");
    assert.equal(e.transient, false);
    return true;
  });
  assert.equal(s.calls(), 1, "a TOO_LARGE was retried; no reload can make a record smaller");
});

await atest("a WRITE is never retried, whatever it says", async () => {
  const s = fake([{ throw: notLoaded }, { rows: { id: "a" } }]);
  const db = engineDb(s);
  await assert.rejects(() => db.put("tasks", {}), e => e.code === "NOT_LOADED");
  assert.equal(s.calls(), 1, "a write was re-sent; a retried write can be applied twice");
});

test("nothing in the wrapper branches on a message", () => {
  const src = readFileSync(new URL("../../js/engine-db.js", import.meta.url), "utf8");
  // A message is prose. Branching on it changes behaviour when someone
  // rewords it, and changes it silently.
  const bad = src.split("\n").filter(l =>
    /\.message\s*(===|==|!==|!=)/.test(l) ||
    /\.message\.(includes|startsWith|match|indexOf)/.test(l));
  assert.deepEqual(bad, [], `these lines branch on a message:\n${bad.join("\n")}`);
  // And it really does branch on the code, so the check above is not vacuous.
  assert.ok(/\.code\s*!==\s*"NOT_LOADED"/.test(src), "the wrapper does not branch on a code at all");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
