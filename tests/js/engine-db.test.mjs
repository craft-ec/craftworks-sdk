// The engine-backed database wrapper: promises, stable codes, and a read that
// actually waits for the data.
//
// No wasm and no socket. The fake session behaves the way the real one does —
// a NOT_LOADED carries a TICKET, the data arrives on a LATER TASK, and the
// ticket is only reported ended once it has. That last part is the whole
// point: the first version of this wrapper awaited a resolved promise and
// asked again in a microtask, before any message could have arrived, and its
// test passed because the fake scripted the second call to succeed.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { engineDb, DbError } from "../../js/engine-db.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack?.split("\n")[0] ?? e}\n`); }
};

/**
 * A session whose data ARRIVES LATER.
 *
 * `afterMs` is when the load completes. Until then the read throws
 * NOT_LOADED with a ticket, exactly as the Rust side does, and `take_loads`
 * reports nothing. A wrapper that re-asks without waiting sees the second
 * NOT_LOADED and fails — which is the behaviour the old test could not see.
 */
const lateSession = ({ afterMs = 20, rows = [{ id: "a" }], fail = false } = {}) => {
  let loaded = false;
  let asks = 0;
  let requests = 0;
  let ended = [];
  let nextTicket = 1;
  const byRange = new Map();
  const s = {
    asks: () => asks,
    requests: () => requests,
    scan(domain = "tasks") {
      asks += 1;
      if (loaded) return JSON.stringify(rows);
      // ONE request per range, as the session does.
      let ticket = byRange.get(domain);
      if (ticket === undefined) {
        ticket = nextTicket++;
        byRange.set(domain, ticket);
        requests += 1;
        setTimeout(() => {
          loaded = !fail;
          ended.push({ id: ticket, ok: !fail, code: fail ? "UNAVAILABLE" : "LOADED" });
          s.pump();               // the page, woken by a message arriving
        }, afterMs);
      }
      const e = new Error("not loaded");
      e.code = "NOT_LOADED";
      e.transient = true;
      e.wait = ticket;
      throw e;
    },
    put() { return s.scan(); },
    take_loads() { const out = ended; ended = []; return JSON.stringify(out); },
    pump: () => {},
  };
  return s;
};

await t("a read waits for the data and then answers — TWO asks, one request", async () => {
  const s = lateSession({ afterMs: 20 });
  const db = engineDb(s);
  s.pump = db.drain;
  const rows = await db.scan("tasks");
  assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.asks(), 2, "the read did not ask exactly twice");
  assert.equal(s.requests(), 1, "the read issued more than one request for one range");
});

await t("CONCURRENT reads of one unloaded range issue ONE request", async () => {
  const s = lateSession({ afterMs: 20 });
  const db = engineDb(s);
  s.pump = db.drain;
  const all = await Promise.all([db.scan("tasks"), db.scan("tasks"), db.scan("tasks")]);
  for (const rows of all) assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.requests(), 1,
    "three components reading one range asked the node three times; that multiplies every first paint");
});

await t("THE CONTROL: a loaded read asks ONCE and requests nothing", async () => {
  const s = lateSession({ afterMs: 0 });
  const db = engineDb(s);
  s.pump = db.drain;
  await db.scan("tasks");                     // warm it
  const asksBefore = s.asks(), reqBefore = s.requests();
  const rows = await db.scan("tasks");
  assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.asks() - asksBefore, 1, "a loaded read asked twice");
  assert.equal(s.requests() - reqBefore, 0,
    "a loaded read requested a range it already had; without this the test above proves nothing");
});

await t("a load that ENDS without delivering rejects — it is not 'not yet'", async () => {
  const s = lateSession({ afterMs: 10, fail: true });
  const db = engineDb(s);
  s.pump = db.drain;
  await assert.rejects(() => db.scan("tasks"), e => {
    assert.ok(e instanceof DbError);
    assert.equal(e.code, "UNAVAILABLE");
    return true;
  });
});

await t("a NOT_LOADED with no ticket rejects rather than hanging", async () => {
  const s = {
    scan() { const e = new Error("x"); e.code = "NOT_LOADED"; throw e; },
    take_loads: () => "[]",
  };
  const db = engineDb(s);
  await assert.rejects(() => db.scan("tasks"), e => e.code === "NOT_LOADED");
});

await t("an error a load cannot fix is NOT waited on", async () => {
  let asks = 0;
  const s = {
    scan() { asks += 1; const e = new Error("too big"); e.code = "TOO_LARGE"; e.transient = false; throw e; },
    take_loads: () => "[]",
  };
  const db = engineDb(s);
  await assert.rejects(() => db.scan("tasks"), e => e.code === "TOO_LARGE");
  assert.equal(asks, 1, "a TOO_LARGE was retried; no load makes a record smaller");
});

await t("a WRITE is never retried, whatever it says", async () => {
  const s = lateSession({ afterMs: 5 });
  const db = engineDb(s);
  s.pump = db.drain;
  await assert.rejects(() => db.put("tasks", {}), e => e.code === "NOT_LOADED");
  assert.equal(s.asks(), 1, "a write was re-sent; a retried write can be applied twice");
});

await t("nothing in the wrapper branches on a message, or waits on a timer", () => {
  const src = readFileSync(new URL("../../js/engine-db.js", import.meta.url), "utf8");
  const code = src.split("\n").filter(l => !l.trim().startsWith("//"));
  const onMessage = code.filter(l =>
    /\.message\s*(===|==|!==|!=)/.test(l) || /\.message\.(includes|startsWith|match|indexOf)/.test(l));
  assert.deepEqual(onMessage, [], `these branch on a message:\n${onMessage.join("\n")}`);
  // A timer here would either spin or answer late, and neither is a fact
  // about the data. The wake-up is an event.
  const timers = code.filter(l => /setTimeout|setInterval|Promise\.resolve\(\)/.test(l));
  assert.deepEqual(timers, [], `these wait on a clock instead of on the answer:\n${timers.join("\n")}`);
  assert.ok(/\.code\s*!==\s*"NOT_LOADED"/.test(src), "the wrapper does not branch on a code at all");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
